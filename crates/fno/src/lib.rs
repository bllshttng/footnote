//! The `fno` front door as a library, so integration tests (and the
//! client-under-portable-pty e2e harness) can link the same code the binary
//! runs. The binary (`main.rs`) is a thin role-select over these modules.

// The repo bans em-dashes, so doc comments use a leading "- " as a prose dash;
// clippy misreads that as an unindented markdown list item. The convention is
// deliberate, so silence the false positive crate-wide rather than reflow it
// away at every site.
#![allow(clippy::doc_lazy_continuation)]

pub mod agents_alias;
pub mod agents_history;
pub mod agents_view;
pub mod argv_witness;
pub mod attention_api;
pub mod attest;
pub mod backlog_list;
pub mod backlog_model;
pub mod backlog_snapshot;
pub mod backlog_view;
pub mod backlog_write;
pub(crate) mod band_remap;
pub mod board_reader;
pub mod bootstrap;
pub mod build_drift;
pub mod chrome;
pub mod cli_args;
pub mod client;
pub mod clipboard;
pub mod config_defaults;
pub mod connections_view;
pub mod context_used;
pub mod digest_overlay;
pub mod dispatch_launch;
pub mod doctor_update;
#[cfg(test)]
#[path = "doctor_update_tests.rs"]
mod doctor_update_tests;
pub mod effect_map;
pub mod event_cli;
mod event_signals;
pub mod event_store;
pub mod feed_overlay;
pub(crate) mod first_check;
pub mod footnote_transcript;
pub mod frame_ansi;
pub mod frame_html;
pub mod git_paths;
pub mod keys;
pub mod lane_colors_panel;
pub(crate) mod lattice;
pub mod law_cli;
pub mod link;
pub mod lint_cli;
pub mod live_store_fence;
pub mod messages_model;
pub mod model_catalog;
pub mod mouse;
pub mod mux_cli;
pub mod mux_rows;
pub mod needs_overlay;
pub mod notify;
#[path = "mux_cli/notify_verb.rs"]
pub mod notify_verb;
pub mod observe;
pub mod open_router;
pub mod org_model;
pub mod org_overlay;
pub mod org_root;
pub mod org_titles;
pub mod pane_argv;
pub mod pane_border;
pub mod pane_cwd;
mod pane_send_audit;
pub mod paths_route;
pub mod popup;
pub mod process_admission;
pub mod product_boundary;
pub mod proto;
pub mod proto_pane;
pub mod proto_slot;
pub mod provider_key;
pub mod pty;
pub mod ratatui_blit;
pub mod redact;
pub mod registry_label;
pub mod registry_read;
pub mod remote_target;
pub mod restore_gate;
pub mod restore_liveness;
pub mod review_invocation;
pub mod role_migration;
pub mod search_query;
pub mod served_liveness;
pub mod server;
pub mod server_stats;
pub mod session_origin;
pub mod setup_autowire;
#[cfg(test)]
#[path = "setup_autowire_tests.rs"]
mod setup_autowire_tests;
pub mod setup_detect;
pub mod setup_profile;
pub mod setup_run;
pub mod setup_services;
pub mod share;
pub mod sideline_color;
pub mod spawn_floor;
pub mod spawn_journal;
pub mod splash;
pub mod sprites;
pub mod squad;
pub mod squad_cascade;
pub mod squad_store;
pub mod state_layout;
pub mod store_client;
pub mod store_conn;
pub mod team_census;
pub mod templates;
#[cfg(test)]
mod test_keeper_cleanup;
#[cfg(test)]
pub(crate) mod test_owner;
pub mod theme;
pub mod theme_import;
pub(crate) mod thread_viewer;
pub mod transcript_tail;
pub mod transcript_transfer;
pub mod tree;
pub mod uninstall;
pub(crate) mod update_prebuilt;
pub mod version;
pub mod view_store;
pub mod vt;
pub mod web;
pub mod wizard;
pub mod wizard_term;
pub mod wordmark;
pub mod yard_overlay;

pub mod doctor_cost;
pub mod otel_read;
