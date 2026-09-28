use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, Request, StatusCode};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, IntoActiveModel, QueryFilter, Schema, Set,
};
use seamark::document::{RelationshipData, ResourceIdentifier};
use seamark::http::{
    self, AdapterError, AdapterResource, MutationAdapterError, MutationCommand, MutationOutcome,
    RelationshipMutation, RequestAuthorizer, ResourceAdapter,
};
use seamark::registry::{
    AttributeMapping, AttributePermission, RelationshipMapping, RelationshipPermission,
    ResourceDefinition, ResourcePermission, ResourceRegistry,
};
use seamark::seaorm::SeaOrmColumnValueCodec;
use seamark::seaorm_mutation::{
    SeaOrmBaseMutationAdapter, SeaOrmBaseMutationExecutor, SeaOrmJoinTableMutationHandler,
    SeaOrmResourceMutationHandler,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const JSONAPI: &str = "application/vnd.api+json";

mod port {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_http_mutation_ports")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub port_id: i32,
        pub title: String,
        pub description: Option<String>,
        pub owner_id: Option<i32>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod person {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_http_mutation_people")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub person_id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_http_mutation_tags")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub tag_id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod port_tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_http_mutation_port_tags")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub port_id: i32,
        #[sea_orm(primary_key, auto_increment = false)]
        pub tag_id: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

fn registry() -> ResourceRegistry {
    ResourceRegistry::new([
        enabled_resource("ports", "port_id")
            .mapped_attribute(enabled_attribute("name", "title"))
            .mapped_attribute(enabled_attribute("description", "description"))
            .mapped_relationship(enabled_relationship("owner", "owner_id", "people", true))
            .mapped_relationship(
                enabled_relationship("tags", "tag_links", "tags", false)
                    .to_many_join_table("port_id", "tag_id"),
            ),
        ResourceDefinition::new("people", "person_id")
            .mapped_attribute(enabled_attribute("name", "name")),
        ResourceDefinition::new("tags", "tag_id")
            .mapped_attribute(enabled_attribute("name", "name")),
    ])
    .unwrap()
}

fn enabled_resource(type_name: &str, identifier: &str) -> ResourceDefinition {
    ResourceDefinition::new(type_name, identifier)
        .allow(ResourcePermission::Create)
        .allow(ResourcePermission::Update)
        .allow(ResourcePermission::Delete)
}

fn enabled_attribute(public_name: &str, model_field: &str) -> AttributeMapping {
    AttributeMapping::new(public_name, model_field)
        .allow(AttributePermission::Create)
        .allow(AttributePermission::Update)
}

fn enabled_relationship(
    public_name: &str,
    model_field: &str,
    target_type: &str,
    to_one: bool,
) -> RelationshipMapping {
    let mapping = RelationshipMapping::new(public_name, model_field, target_type)
        .allow(RelationshipPermission::LinkageRead)
        .allow(RelationshipPermission::BaseReplace)
        .allow(RelationshipPermission::BaseAdd)
        .allow(RelationshipPermission::BaseRemove)
        .allow(RelationshipPermission::ResourceCreate)
        .allow(RelationshipPermission::ResourceUpdate);
    if to_one {
        mapping.to_one_foreign_key(true)
    } else {
        mapping.to_many()
    }
}

struct AllowAll;

#[async_trait]
impl RequestAuthorizer for AllowAll {
    async fn authorize(
        &self,
        _resource_type: &str,
        _resource_id: Option<&str>,
        _headers: &HeaderMap,
    ) -> bool {
        true
    }

    async fn authorize_mutation(
        &self,
        _action: seamark::http::MutationAction,
        _resource: &ResourceDefinition,
        _resource_id: Option<&str>,
        _command: &MutationCommand,
        _headers: &HeaderMap,
    ) -> bool {
        true
    }
}

struct NoReads;

#[async_trait]
impl ResourceAdapter for NoReads {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
    ) -> Result<Vec<AdapterResource>, AdapterError> {
        Err(AdapterError)
    }

    async fn resource(
        &self,
        _resource: &ResourceDefinition,
        _id: &str,
    ) -> Result<Option<AdapterResource>, AdapterError> {
        Err(AdapterError)
    }
}

struct PortMutationExecutor;

#[async_trait]
impl SeaOrmBaseMutationExecutor for PortMutationExecutor {
    fn supports(&self, resource: &ResourceDefinition, command: &MutationCommand) -> bool {
        resource.type_name() == "ports"
            && matches!(
                command,
                MutationCommand::Create { .. }
                    | MutationCommand::Update { .. }
                    | MutationCommand::Delete { .. }
                    | MutationCommand::ReadRelationship { .. }
                    | MutationCommand::ModifyRelationship { .. }
            )
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        _resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        match command {
            MutationCommand::Create { changeset } => {
                let title = changeset
                    .attributes
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("created")
                    .to_owned();
                let description = changeset
                    .attributes
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let model = port::ActiveModel {
                    title: Set(title),
                    description: Set(description),
                    ..Default::default()
                }
                .insert(transaction)
                .await
                .map_err(|_| MutationAdapterError::Failed)?;
                if let Some(owner) = changeset.relationships.get("owner_id") {
                    set_owner(transaction, model.port_id, owner).await?;
                }
                if let Some(tags) = changeset.relationships.get("tag_links") {
                    let RelationshipData::Many(identifiers) = tags else {
                        return Err(MutationAdapterError::Unsupported);
                    };
                    replace_tags(transaction, model.port_id, identifiers).await?;
                }
                Ok(MutationOutcome::Resource(
                    port_resource(transaction, model).await?,
                ))
            }
            MutationCommand::Update { id, changeset } => {
                let port_id = parse_id(id)?;
                let model = port::Entity::find_by_id(port_id)
                    .one(transaction)
                    .await
                    .map_err(|_| MutationAdapterError::Failed)?
                    .ok_or(MutationAdapterError::NotFound)?;
                let mut active = model.into_active_model();
                if let Some(title) = changeset.attributes.get("title") {
                    active.title = Set(title
                        .as_str()
                        .ok_or(MutationAdapterError::Failed)?
                        .to_owned());
                }
                if let Some(description) = changeset.attributes.get("description") {
                    active.description = Set(description.as_str().map(str::to_owned));
                }
                let mut model = active
                    .update(transaction)
                    .await
                    .map_err(|_| MutationAdapterError::Failed)?;
                if let Some(owner) = changeset.relationships.get("owner_id") {
                    set_owner(transaction, port_id, owner).await?;
                    model.owner_id = match owner {
                        RelationshipData::Null => None,
                        RelationshipData::One(identifier) => Some(parse_identifier(identifier)?),
                        RelationshipData::Many(_) => {
                            return Err(MutationAdapterError::Unsupported);
                        }
                    };
                }
                if let Some(tags) = changeset.relationships.get("tag_links") {
                    let RelationshipData::Many(identifiers) = tags else {
                        return Err(MutationAdapterError::Unsupported);
                    };
                    replace_tags(transaction, port_id, identifiers).await?;
                }
                Ok(MutationOutcome::Resource(
                    port_resource(transaction, model).await?,
                ))
            }
            MutationCommand::Delete { id } => {
                let result = port::Entity::delete_by_id(parse_id(id)?)
                    .exec(transaction)
                    .await
                    .map_err(|_| MutationAdapterError::Failed)?;
                if result.rows_affected == 0 {
                    return Err(MutationAdapterError::NotFound);
                }
                Ok(MutationOutcome::Deleted)
            }
            MutationCommand::ReadRelationship { id, relationship } => {
                let port_id = parse_id(id)?;
                require_port(transaction, port_id).await?;
                read_relationship(transaction, port_id, relationship)
                    .await
                    .map(MutationOutcome::Relationship)
            }
            MutationCommand::ModifyRelationship {
                id,
                relationship,
                mutation,
            } => {
                let port_id = parse_id(id)?;
                require_port(transaction, port_id).await?;
                match (relationship.model_field(), mutation) {
                    ("owner_id", RelationshipMutation::Replace(data)) => {
                        set_owner(transaction, port_id, data).await?;
                    }
                    ("tag_links", RelationshipMutation::Replace(data)) => {
                        let RelationshipData::Many(identifiers) = data else {
                            return Err(MutationAdapterError::Unsupported);
                        };
                        replace_tags(transaction, port_id, identifiers).await?;
                    }
                    ("tag_links", RelationshipMutation::Add(identifiers)) => {
                        add_tags(transaction, port_id, identifiers).await?;
                    }
                    ("tag_links", RelationshipMutation::Remove(identifiers)) => {
                        remove_tags(transaction, port_id, identifiers).await?;
                    }
                    _ => return Err(MutationAdapterError::Unsupported),
                }
                read_relationship(transaction, port_id, relationship)
                    .await
                    .map(MutationOutcome::Relationship)
            }
        }
    }
}

struct EmptyIdPortMutationExecutor;

#[async_trait]
impl SeaOrmBaseMutationExecutor for EmptyIdPortMutationExecutor {
    fn supports(&self, resource: &ResourceDefinition, command: &MutationCommand) -> bool {
        resource.type_name() == "ports"
            && matches!(
                command,
                MutationCommand::Create { .. }
                    | MutationCommand::Update { .. }
                    | MutationCommand::Delete { .. }
                    | MutationCommand::ReadRelationship { .. }
                    | MutationCommand::ModifyRelationship { .. }
            )
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        _resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        let MutationCommand::Create { changeset } = command else {
            return Err(MutationAdapterError::Unsupported);
        };
        let title = changeset
            .attributes
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("unaddressable")
            .to_owned();
        port::ActiveModel {
            title: Set(title.clone()),
            description: Set(None),
            owner_id: Set(None),
            ..Default::default()
        }
        .insert(transaction)
        .await
        .map_err(|_| MutationAdapterError::Failed)?;
        Ok(MutationOutcome::Resource(AdapterResource {
            id: String::new(),
            attributes: BTreeMap::from([("title".to_owned(), json!(title))]),
            ..AdapterResource::default()
        }))
    }
}

async fn require_port(
    transaction: &DatabaseTransaction,
    port_id: i32,
) -> Result<(), MutationAdapterError> {
    port::Entity::find_by_id(port_id)
        .one(transaction)
        .await
        .map_err(|_| MutationAdapterError::Failed)?
        .ok_or(MutationAdapterError::NotFound)
        .map(|_| ())
}

async fn set_owner(
    transaction: &DatabaseTransaction,
    port_id: i32,
    data: &RelationshipData,
) -> Result<(), MutationAdapterError> {
    let owner_id = match data {
        RelationshipData::Null => None,
        RelationshipData::One(identifier) => {
            let owner_id = parse_identifier(identifier)?;
            if person::Entity::find_by_id(owner_id)
                .one(transaction)
                .await
                .map_err(|_| MutationAdapterError::Failed)?
                .is_none()
            {
                return Err(MutationAdapterError::RelatedResourceNotFound);
            }
            Some(owner_id)
        }
        RelationshipData::Many(_) => return Err(MutationAdapterError::Unsupported),
    };
    let model = port::Entity::find_by_id(port_id)
        .one(transaction)
        .await
        .map_err(|_| MutationAdapterError::Failed)?
        .ok_or(MutationAdapterError::NotFound)?;
    let mut active = model.into_active_model();
    active.owner_id = Set(owner_id);
    active
        .update(transaction)
        .await
        .map_err(|_| MutationAdapterError::Failed)?;
    Ok(())
}

async fn replace_tags(
    transaction: &DatabaseTransaction,
    port_id: i32,
    identifiers: &[ResourceIdentifier],
) -> Result<(), MutationAdapterError> {
    port_tag::Entity::delete_many()
        .filter(port_tag::Column::PortId.eq(port_id))
        .exec(transaction)
        .await
        .map_err(|_| MutationAdapterError::Failed)?;
    add_tags(transaction, port_id, identifiers).await
}

async fn add_tags(
    transaction: &DatabaseTransaction,
    port_id: i32,
    identifiers: &[ResourceIdentifier],
) -> Result<(), MutationAdapterError> {
    for identifier in identifiers {
        let tag_id = parse_identifier(identifier)?;
        if tag::Entity::find_by_id(tag_id)
            .one(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?
            .is_none()
        {
            return Err(MutationAdapterError::RelatedResourceNotFound);
        }
        let already_linked = port_tag::Entity::find()
            .filter(port_tag::Column::PortId.eq(port_id))
            .filter(port_tag::Column::TagId.eq(tag_id))
            .one(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?
            .is_some();
        if !already_linked {
            port_tag::ActiveModel {
                port_id: Set(port_id),
                tag_id: Set(tag_id),
            }
            .insert(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?;
        }
    }
    Ok(())
}

async fn remove_tags(
    transaction: &DatabaseTransaction,
    port_id: i32,
    identifiers: &[ResourceIdentifier],
) -> Result<(), MutationAdapterError> {
    for identifier in identifiers {
        let tag_id = parse_identifier(identifier)?;
        if tag::Entity::find_by_id(tag_id)
            .one(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?
            .is_none()
        {
            return Err(MutationAdapterError::RelatedResourceNotFound);
        }
        port_tag::Entity::delete_many()
            .filter(port_tag::Column::PortId.eq(port_id))
            .filter(port_tag::Column::TagId.eq(tag_id))
            .exec(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?;
    }
    Ok(())
}

async fn read_relationship(
    transaction: &DatabaseTransaction,
    port_id: i32,
    relationship: &RelationshipMapping,
) -> Result<RelationshipData, MutationAdapterError> {
    match relationship.model_field() {
        "owner_id" => {
            let model = port::Entity::find_by_id(port_id)
                .one(transaction)
                .await
                .map_err(|_| MutationAdapterError::Failed)?
                .ok_or(MutationAdapterError::NotFound)?;
            Ok(match model.owner_id {
                Some(id) => RelationshipData::One(identifier("people", id)),
                None => RelationshipData::Null,
            })
        }
        "tag_links" => {
            let links = port_tag::Entity::find()
                .filter(port_tag::Column::PortId.eq(port_id))
                .all(transaction)
                .await
                .map_err(|_| MutationAdapterError::Failed)?
                .into_iter()
                .map(|link| identifier("tags", link.tag_id))
                .collect();
            Ok(RelationshipData::Many(links))
        }
        _ => Err(MutationAdapterError::Unsupported),
    }
}

async fn port_resource(
    transaction: &DatabaseTransaction,
    model: port::Model,
) -> Result<AdapterResource, MutationAdapterError> {
    let relationships = BTreeMap::from([
        (
            "owner_id".to_owned(),
            seamark::document::Relationship {
                data: Some(match model.owner_id {
                    Some(id) => RelationshipData::One(identifier("people", id)),
                    None => RelationshipData::Null,
                }),
                ..Default::default()
            },
        ),
        (
            "tag_links".to_owned(),
            seamark::document::Relationship {
                data: Some(
                    read_relationship(
                        transaction,
                        model.port_id,
                        &ResourceDefinition::new("ports", "port_id")
                            .to_many_relationship("tags", "tag_links", "tags")
                            .relationships()[0],
                    )
                    .await?,
                ),
                ..Default::default()
            },
        ),
    ]);
    Ok(AdapterResource {
        id: model.port_id.to_string(),
        attributes: BTreeMap::from([
            ("title".to_owned(), json!(model.title)),
            ("description".to_owned(), json!(model.description)),
        ]),
        relationships,
    })
}

fn parse_id(id: &str) -> Result<i32, MutationAdapterError> {
    id.parse().map_err(|_| MutationAdapterError::NotFound)
}

fn parse_identifier(identifier: &ResourceIdentifier) -> Result<i32, MutationAdapterError> {
    if identifier.id.is_none() || identifier.lid.is_some() {
        return Err(MutationAdapterError::RelatedResourceNotFound);
    }
    identifier
        .id
        .as_deref()
        .ok_or(MutationAdapterError::RelatedResourceNotFound)
        .and_then(parse_id)
}

fn identifier(type_name: &str, id: i32) -> ResourceIdentifier {
    ResourceIdentifier {
        type_name: type_name.to_owned(),
        id: Some(id.to_string()),
        ..Default::default()
    }
}

async fn create_tables(database: &DatabaseConnection) {
    for statement in [
        "DROP TABLE IF EXISTS seamark_http_mutation_port_tags",
        "DROP TABLE IF EXISTS seamark_http_mutation_ports",
        "DROP TABLE IF EXISTS seamark_http_mutation_tags",
        "DROP TABLE IF EXISTS seamark_http_mutation_people",
    ] {
        database.execute_unprepared(statement).await.unwrap();
    }
    let backend = database.get_database_backend();
    let schema = Schema::new(backend);
    for statement in [
        schema.create_table_from_entity(port::Entity),
        schema.create_table_from_entity(person::Entity),
        schema.create_table_from_entity(tag::Entity),
        schema.create_table_from_entity(port_tag::Entity),
    ] {
        database.execute(&statement).await.unwrap();
    }
}

fn mutation_request(method: &str, uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(ACCEPT, JSONAPI)
        .header(CONTENT_TYPE, JSONAPI)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn relationship_request(method: &str, uri: &str, data: Value) -> Request<Body> {
    mutation_request(method, uri, &json!({"data": data}))
}

async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

/// Exercises base resource and relationship routes against one isolated database.
pub async fn run_case(database: &DatabaseConnection) {
    create_tables(database).await;
    person::ActiveModel {
        person_id: Set(7),
        name: Set("Owner".to_owned()),
    }
    .insert(database)
    .await
    .unwrap();
    for (id, name) in [(4, "Four"), (5, "Five")] {
        tag::ActiveModel {
            tag_id: Set(id),
            name: Set(name.to_owned()),
        }
        .insert(database)
        .await
        .unwrap();
    }

    let registry = Arc::new(registry());
    let executor: Arc<dyn SeaOrmBaseMutationExecutor> = Arc::new(PortMutationExecutor);
    let adapter = Arc::new(SeaOrmBaseMutationAdapter::new(
        database.clone(),
        vec![executor],
    ));
    let router = http::ApiBuilder::new(registry.clone(), Arc::new(AllowAll))
        .reads(Arc::new(NoReads))
        .mutations(adapter)
        .try_build()
        .unwrap();

    let response = router
        .clone()
        .oneshot(mutation_request(
            "POST",
            "/ports",
            &json!({
                "data": {
                    "type": "ports",
                    "attributes": {"name": "Initial"},
                    "relationships": {"owner": {"data": {"type": "people", "id": "7"}}}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = response_json(response).await;
    let port_id = created["data"]["id"]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert_eq!(created["data"]["attributes"]["name"], "Initial");

    let invalid_executor: Arc<dyn SeaOrmBaseMutationExecutor> =
        Arc::new(EmptyIdPortMutationExecutor);
    let invalid_adapter = Arc::new(SeaOrmBaseMutationAdapter::new(
        database.clone(),
        vec![invalid_executor],
    ));
    let invalid_router = http::ApiBuilder::new(registry.clone(), Arc::new(AllowAll))
        .reads(Arc::new(NoReads))
        .mutations(invalid_adapter)
        .try_build()
        .unwrap();
    let response = invalid_router
        .oneshot(mutation_request(
            "POST",
            "/ports",
            &json!({
                "data": {
                    "type": "ports",
                    "attributes": {"name": "Unaddressable"}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.headers()[CONTENT_TYPE], JSONAPI);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = response_json(response).await;
    assert_eq!(error["errors"][0]["code"], "mutation_failed");
    assert_eq!(error["errors"][0]["status"], "500");
    assert_eq!(
        port::Entity::find().all(database).await.unwrap().len(),
        1,
        "an unaddressable empty-ID create result must roll back its inserted row"
    );

    let response = router
        .clone()
        .oneshot(mutation_request(
            "POST",
            "/ports",
            &json!({
                "data": {
                    "type": "ports",
                    "attributes": {"name": "Missing Owner"},
                    "relationships": {
                        "owner": {"data": {"type": "people", "id": "99"}}
                    }
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[CONTENT_TYPE], JSONAPI);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = response_json(response).await;
    assert_eq!(error["errors"][0]["code"], "related_resource_not_found");
    assert_eq!(error["errors"][0]["status"], "404");
    assert_eq!(
        port::Entity::find().all(database).await.unwrap().len(),
        1,
        "a failed create referencing a missing related resource must roll back"
    );

    let response = router
        .clone()
        .oneshot(mutation_request(
            "PATCH",
            "/ports/999",
            &json!({
                "data": {
                    "type": "ports",
                    "id": "999",
                    "attributes": {"name": "Missing"}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[CONTENT_TYPE], JSONAPI);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = response_json(response).await;
    assert_eq!(error["errors"][0]["code"], "resource_not_found");
    assert_eq!(error["errors"][0]["status"], "404");

    let response = router
        .clone()
        .oneshot(mutation_request("DELETE", "/ports/999", &json!({})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[CONTENT_TYPE], JSONAPI);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = response_json(response).await;
    assert_eq!(error["errors"][0]["code"], "resource_not_found");
    assert_eq!(error["errors"][0]["status"], "404");
    let persisted = port::Entity::find_by_id(port_id)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.title, "Initial");

    let response = router
        .clone()
        .oneshot(mutation_request(
            "PATCH",
            &format!("/ports/{port_id}"),
            &json!({
                "data": {
                    "type": "ports",
                    "id": port_id.to_string(),
                    "attributes": {"description": null}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let updated = response_json(response).await;
    assert_eq!(updated["data"]["attributes"]["name"], "Initial");
    assert!(updated["data"]["attributes"]["description"].is_null());
    let persisted = port::Entity::find_by_id(port_id)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.title, "Initial");
    assert_eq!(persisted.description, None);
    assert_eq!(persisted.owner_id, Some(7));

    let response = router
        .clone()
        .oneshot(relationship_request(
            "PATCH",
            &format!("/ports/{port_id}/relationships/owner"),
            json!({"type": "people", "id": "7"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await["data"],
        json!({"type": "people", "id": "7"})
    );
    assert_eq!(
        port::Entity::find_by_id(port_id)
            .one(database)
            .await
            .unwrap()
            .unwrap()
            .owner_id,
        Some(7)
    );

    let response = router
        .clone()
        .oneshot(relationship_request(
            "POST",
            &format!("/ports/{port_id}/relationships/tags"),
            json!([
                {"type": "tags", "id": "4"},
                {"type": "tags", "id": "4"}
            ]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await["data"],
        json!([{"type":"tags","id":"4"}])
    );
    assert_eq!(
        port_tag::Entity::find()
            .filter(port_tag::Column::PortId.eq(port_id))
            .all(database)
            .await
            .unwrap()
            .len(),
        1
    );

    let response = router
        .clone()
        .oneshot(relationship_request(
            "POST",
            &format!("/ports/{port_id}/relationships/tags"),
            json!([{"type": "tags", "id": "99"}]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        port_tag::Entity::find()
            .filter(port_tag::Column::PortId.eq(port_id))
            .all(database)
            .await
            .unwrap()
            .len(),
        1
    );

    for method in ["POST", "DELETE"] {
        let response = router
            .clone()
            .oneshot(relationship_request(
                method,
                &format!("/ports/{port_id}/relationships/tags"),
                json!([]),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_json(response).await["data"],
            json!([{"type":"tags","id":"4"}])
        );
    }

    let response = router
        .clone()
        .oneshot(relationship_request(
            "POST",
            &format!("/ports/{port_id}/relationships/tags"),
            json!([{"type": "tags", "id": "4"}]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await["data"],
        json!([{"type":"tags","id":"4"}])
    );
    assert_eq!(
        port_tag::Entity::find()
            .filter(port_tag::Column::PortId.eq(port_id))
            .all(database)
            .await
            .unwrap()
            .len(),
        1
    );

    let response = router
        .clone()
        .oneshot(relationship_request(
            "DELETE",
            &format!("/ports/{port_id}/relationships/tags"),
            json!([{"type": "tags", "id": "5"}]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await["data"],
        json!([{"type":"tags","id":"4"}])
    );

    let response = router
        .clone()
        .oneshot(relationship_request(
            "DELETE",
            &format!("/ports/{port_id}/relationships/tags"),
            json!([{"type": "tags", "id": "99"}]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = router
        .clone()
        .oneshot(relationship_request(
            "PATCH",
            &format!("/ports/{port_id}/relationships/tags"),
            json!([{"type": "tags", "id": "5"}]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await["data"],
        json!([{"type":"tags","id":"5"}])
    );
    let persisted_tags = port_tag::Entity::find()
        .filter(port_tag::Column::PortId.eq(port_id))
        .all(database)
        .await
        .unwrap();
    assert_eq!(persisted_tags.len(), 1);
    assert_eq!(persisted_tags[0].tag_id, 5);

    let response = router
        .clone()
        .oneshot(relationship_request(
            "PATCH",
            &format!("/ports/{port_id}/relationships/tags"),
            json!([]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["data"], json!([]));

    let response = router
        .clone()
        .oneshot(mutation_request(
            "PATCH",
            &format!("/ports/{port_id}"),
            &json!({
                "data": {
                    "type": "ports",
                    "id": port_id.to_string(),
                    "attributes": {"name": "Should roll back"},
                    "relationships": {"owner": {"data": {"type": "people", "id": "99"}}}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let persisted = port::Entity::find_by_id(port_id)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.title, "Initial");
    assert_eq!(persisted.owner_id, Some(7));

    let response = router
        .clone()
        .oneshot(mutation_request(
            "PATCH",
            &format!("/ports/{port_id}"),
            &json!({
                "data": {
                    "type": "ports",
                    "id": port_id.to_string(),
                    "relationships": {"owner": {"data": null}}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await["data"]["relationships"]["owner"]["data"],
        Value::Null
    );
    let persisted = port::Entity::find_by_id(port_id)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.owner_id, None);

    let response = router
        .clone()
        .oneshot(read_request("/ports/999/relationships/tags"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = router
        .clone()
        .oneshot(mutation_request(
            "DELETE",
            &format!("/ports/{port_id}"),
            &json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        port::Entity::find_by_id(port_id)
            .one(database)
            .await
            .unwrap()
            .is_none()
    );

    let standard_handler: Arc<dyn SeaOrmBaseMutationExecutor> = Arc::new(
        SeaOrmResourceMutationHandler::<port::Entity, _>::new(
            &registry,
            "ports",
            SeaOrmColumnValueCodec::<port::Entity>::default(),
        )
        .unwrap(),
    );
    let standard_tags_handler: Arc<dyn SeaOrmBaseMutationExecutor> = Arc::new(
        SeaOrmJoinTableMutationHandler::<port_tag::Entity, _>::new(
            &registry,
            "ports",
            "tags",
            SeaOrmColumnValueCodec::<port_tag::Entity>::default(),
        )
        .unwrap(),
    );
    let standard_adapter = Arc::new(SeaOrmBaseMutationAdapter::new(
        database.clone(),
        vec![standard_handler, standard_tags_handler],
    ));
    let standard_router = http::ApiBuilder::new(registry, Arc::new(AllowAll))
        .mutations(standard_adapter)
        .try_build()
        .unwrap();
    let response = standard_router
        .oneshot(mutation_request(
            "POST",
            "/ports",
            &json!({
                "data": {
                    "type": "ports",
                    "attributes": {"name": "Standard executor"},
                    "relationships": {
                        "owner": {"data": {"type": "people", "id": "7"}},
                        "tags": {"data": [
                            {"type": "tags", "id": "4"},
                            {"type": "tags", "id": "5"}
                        ]}
                    }
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = response_json(response).await;
    assert_eq!(created["data"]["attributes"]["name"], "Standard executor");
    assert_eq!(
        created["data"]["relationships"]["owner"]["data"],
        json!({"type": "people", "id": "7"})
    );
    assert_eq!(
        created["data"]["relationships"]["tags"]["data"],
        json!([
            {"type": "tags", "id": "4"},
            {"type": "tags", "id": "5"}
        ])
    );
}

fn read_request(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(ACCEPT, JSONAPI)
        .body(Body::empty())
        .unwrap()
}
