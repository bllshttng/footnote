//! The one spawn door.
//!
//! Task 1.1 lands the contract and the owned-caller proof. The door's
//! transaction (`spawn_transaction`), the backend dispatch table, and
//! the producer rewiring build on these modules; the public
//! `spawn()` entry comes with the transaction.

pub use crate::spawn_context::{
    resolve_owned_identity_from, resolve_self_identity, session_identity_key, OwnedDisposition,
    OwnedDisposition as _OwnedDispositionAlias, OwnedIdentity,
};
pub use crate::spawn_contract::{
    validate, InvocationRef, NonSessionSource, SessionRef, SpawnError, SpawnHow, SpawnOrigin,
    SpawnOwner, SpawnProvenance, SpawnRequest, SpawnWork, ValidatedSpawn,
};
