//! The wire text-size ceilings and the build stamp. Moved out of proto.rs
//! (file budget shrink) and re-exported there, so `fno::proto::MAX_*` keeps
//! its path. The version consts stay in proto.rs itself: the version-bump
//! parity gate greps the physical file for them.
#![allow(rustdoc::broken_intra_doc_links)]

/// (v34, x-9c5f) The peek-overlay free-text mail ceiling: the server refuses
/// (never truncates) a [`Command::MailAgent`] whose sanitized text exceeds this,
/// because a silently cut instruction to a worker is worse than a visible
/// refusal (Locked Decision 7).
pub const MAX_MAIL_TEXT: usize = 400;

/// The stored tab-name ceiling (x-c150), shared by the server-side sanitize
/// (the authoritative cap for any wire client) and the rename overlay's input
/// cap (the TUI affordance, so the operator sees exactly what will be stored).
pub const MAX_TAB_NAME: usize = 32;

/// The stored squad-name ceiling (x-96e8), the same 32-char cap as
/// [`MAX_TAB_NAME`] applied to `RenameSquad` on both the server sanitize and
/// the client input. A sibling const (not a shared rename) so the two rename
/// paths stay independently readable.
pub const MAX_SQUAD_NAME: usize = 32;

/// The crate version, carried in the handshake purely for the error message.
pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
