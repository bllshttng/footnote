//! The `fno` front door as a library, so integration tests (and the
//! client-under-portable-pty e2e harness) can link the same code the binary
//! runs. The binary (`main.rs`) is a thin role-select over these modules.

pub mod agents_view;
pub mod backlog_view;
pub mod bootstrap;
pub mod client;
pub mod clipboard;
pub mod digest_overlay;
pub mod keys;
pub mod mouse;
pub mod mux_cli;
pub mod proto;
pub mod pty;
pub mod server;
pub mod squad;
pub mod squad_store;
pub mod tree;
pub mod version;
pub mod vt;
pub mod web;
