//! The backlog domain: the typed node model and (in later groups) the
//! relational store, the owned-table modules, and parity. Wave 3 ships the
//! model; the wave 4 tables live beside it in their owning modules.

pub mod model;

pub use model::{state_type, ModelError, Node, Priority, RelationType, StateType, Status};
