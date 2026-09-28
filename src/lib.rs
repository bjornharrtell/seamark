//! Building blocks for JSON:API servers backed by SeaORM.
//!
//! Seamark provides explicit resource mappings, query planning, standard
//! SeaORM adapters, ordinary mutations, and Atomic Operations integration.
//! It does not claim complete JSON:API or Atomic Operations conformance.

#![forbid(unsafe_code)]

pub mod atomic;
pub mod atomic_http;
pub mod authorization;
pub mod document;
pub mod http;
mod json;
pub mod limits;
mod media;
pub mod projection;
pub mod query;
pub mod registry;
pub mod seaorm;
pub mod seaorm_mutation;
