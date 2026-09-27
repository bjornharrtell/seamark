//! Building blocks for JSON:API servers backed by SeaORM.
//!
//! Seamark is being implemented in milestones. The first milestone provides
//! JSON:API document types and structural validation; it does not yet provide
//! HTTP routes, persistence, or complete specification conformance.

#![forbid(unsafe_code)]

pub mod document;
