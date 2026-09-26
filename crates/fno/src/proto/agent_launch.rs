//! Typed payloads for the sideline new-agent launcher (v91).
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
    /// Canonical spawn substrate. EMPTY means the door's default (`thread`
    /// where the harness seats one, `headless` where it does not); `pane`
    /// names the pane lane explicitly (the only way `--tab`/`--split` reach
    /// a pane), `thread` names the thread lane explicitly. `headless` is
    /// never sent by the popup and is refused pre-wire.
    pub substrate: String,
    /// Optional explicit pins. `None` = the harness default; the popup shows
    /// "harness decides" rather than inventing a resolved value.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional model provider pin from a configured routing row.
    #[serde(default)]
    pub provider: Option<String>,
    /// True when the model came from a picked routing row: argv omits
    /// `--harness` so the door resolves that row's harness, route, account
    /// and effort itself. A typed model keeps the false default and the
    /// plain `--harness` override.
    #[serde(default)]
    pub model_names_harness: bool,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Optional mux tab selector for pane placement (`--tab`), or `new` for
    /// a thread opened in a new tab through its portal.
    #[serde(default)]
    pub placement: Option<String>,
    /// Portal index for a thread spawn placed through a portal
    /// (`--portal N`). Required by the door whenever a thread spawn carries
    /// a placement flag.
    #[serde(default)]
    pub portal: Option<u8>,
    /// Split direction for a thread spawn opened as a split beside the
    /// focused pane (`--split <dir>`: left/right/up/down).
    #[serde(default)]
    pub split: Option<String>,
    /// The backlog node the launch works, set only by a board prefill;
    /// rides as `--node`.
    #[serde(default)]
    pub node: Option<String>,
    /// The seed text. Empty = an intentionally interactive launch; the door
    /// owns whether the harness/substrate combination accepts one, and its
    /// refusal (never a fabricated seed) is what the operator sees.
    #[serde(default)]
    pub message: String,
    /// Additional argv tokens for `fno agents spawn`; never shell text.
    #[serde(default)]
    pub extra_flags: Vec<String>,
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
