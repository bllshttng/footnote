//! Typed payloads for the sideline new-agent launcher (v83).
//!
//! Every field is a structured value the server forwards into canonical
//! `fno agents spawn` argv elements. No free-form shell text exists anywhere
//! in this exchange: the message rides stdin (`--prompt-file -`), never an
//! interpolated argv element, and the harness/cwd/advanced values are
//! single argv elements by construction.

use serde::{Deserialize, Serialize};

/// One launch request from the popup. `request_id` is client-minted and
/// monotonic; `revision` is the draft revision the snapshot was taken at, so
/// a stale response can never overwrite a newer draft (AC1-EDGE).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLaunchRequest {
    pub request_id: u64,
    pub revision: u64,
    /// The exact cwd the launch runs in; displayed verbatim in the popup.
    pub cwd: String,
    /// Harness name from the launcher catalog (`claude`, `codex`, ...).
    pub harness: String,
    /// Canonical spawn substrate (`pane` or `thread`; `headless` is not
    /// offered by the popup).
    pub substrate: String,
    /// Optional explicit pins. `None` = the harness default; the popup shows
    /// "harness default" rather than inventing a resolved value.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Optional mux tab selector for pane placement (`--tab`).
    #[serde(default)]
    pub placement: Option<String>,
    /// The seed text. Empty = an intentionally interactive launch; the door
    /// owns whether the harness/substrate combination accepts one, and its
    /// refusal (never a fabricated seed) is what the operator sees.
    #[serde(default)]
    pub message: String,
}

/// One progress update for a launch attempt, correlated by `request_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLaunchUpdate {
    pub request_id: u64,
    pub state: LaunchState,
}

/// The lifecycle the popup renders. Accepted != born: `Starting` is the
/// acknowledgment, only `Launched` carries a verified receipt, and
/// `Unknown` explicitly refuses to claim whether a worker was born.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchState {
    /// The request passed pre-birth validation and the spawn subprocess is
    /// running. Not a birth.
    Starting,
    /// A definitive no-birth refusal (the door refused before any effect).
    /// Deliberate retry is safe.
    Refused { reason: String },
    /// The spawn receipt was decoded. `pane` is present only for a
    /// pane-hosted launch; a bg thread hands off through the roster row.
    /// `seed_delivered`: `Some(true)` proven, `Some(false)` unproven or
    /// intentionally unattempted, `None` = the receipt carries no seed fact
    /// (a bg thread).
    Launched {
        name: String,
        pane: Option<u64>,
        seed_delivered: Option<bool>,
    },
    /// The outcome is unknowable (timeout, lost reply, malformed receipt).
    /// Whether a worker was born is unresolved: retry stays blocked until
    /// the operator reconciles the roster/journal evidence.
    Unknown { reason: String },
}
