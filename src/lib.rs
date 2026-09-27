//! Building blocks for JSON:API servers backed by SeaORM.
//!
//! Seamark is being implemented in milestones. Its current protocol and
//! registry foundations do not yet provide persistence or complete JSON:API
//! specification conformance.

#![forbid(unsafe_code)]

pub mod document;
pub mod http;
pub mod registry;
