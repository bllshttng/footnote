//! The `fno` front door as a library, so integration tests (and the
//! client-under-portable-pty e2e harness) can link the same code the binary
//! runs. The binary (`main.rs`) is a thin role-select over these modules.

// The repo bans em-dashes, so doc comments use a leading "- " as a prose dash;
// clippy misreads that as an unindented markdown list item. The convention is
// deliberate, so silence the false positive crate-wide rather than reflow it
// away at every site.
#![allow(clippy::doc_lazy_continuation)]

pub mod agents_view;
pub mod backlog_view;
pub mod bootstrap;
pub mod client;
pub mod clipboard;
pub mod connections_view;
pub mod digest_overlay;
pub mod keys;
pub mod mouse;
pub mod mux_cli;
pub mod needs_overlay;
pub mod popup;
pub mod proto;
pub mod pty;
pub mod server;
pub mod squad;
pub mod squad_store;
pub mod templates;
pub mod tree;
pub mod version;
pub mod view_store;
pub mod vt;
pub mod web;
