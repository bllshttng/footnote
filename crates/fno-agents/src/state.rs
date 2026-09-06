//! Shared state files (Wave 3): `registry.json` (schema v4) and per-agent
//! `state.json` (schema v1), plus the flock-protected, atomic read/modify/write
//! helpers the daemon and worker share.
//!
//! Coupling-discipline invariants honored here:
//!
//! - **One writer per file via advisory lock.** Mutations take `LOCK_EX`; the
//!   daemon-down read path takes `LOCK_SH`. std's `File::lock`/`lock_shared`
//!   (stable since Rust 1.89) wrap `flock(2)`, the same advisory-lock family
//!   Python's `fcntl.flock` uses, so a Python `fno` process and the Rust daemon
//!   serialize against each other (US6.12, the load-bearing cross-language
//!   coupling proven by `tests/flock_interop.rs`).
//! - **Atomic publish via tempfile + rename.** A reader never observes a torn
//!   write; it sees either the old file or the fully-written new one. Optional
//!   fields are preserved across updates by round-tripping through the typed
//!   struct (no field-dropping reserialization).
//! - **`state.status` is canonical; `registry.status` is a projection** (LD10).
//!   This module stores both; conflict resolution (state wins) is the daemon's.

use crate::identity::session_handle_tier;
use crate::AgentStatus;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Current registry schema version.
///
/// v4 (ab-a171ceb2) is a forward-compat bump for `host_mode`: v4 is
/// structurally identical to v3 (host_mode is additive-optional and read
/// version-independently via absent==exec coercion), but stamping v4 forces a
/// pre-host_mode reader - which accepts only {1,2,3} and has no host_mode code
/// - to REJECT the store rather than silently treat an interactive row as exec
/// and orphan a live TUI during reconcile. Readers stay backward-compatible:
/// the accepted-version set still spans 1..=4 (see ACCEPTED_SCHEMA_VERSIONS in
/// client_verbs.rs and the Python load_registry range check).
///
/// v5 (inside-out E3.1, X2/X3) is the same kind of forward-compat bump for the
/// additive `inside_leg` field: structurally identical to v4 (an absent
/// `inside_leg` reads as `None`), but stamping v5 forces a pre-inside-leg reader
/// to REJECT rather than silently DROP a stored inside-leg report on write-back
/// (Rust serde has no `deny_unknown_fields`, so an old daemon would otherwise
/// round-trip the field out of existence). Accepted set widens to 1..=5.
///
/// v6 (mux agent edge, 4a-G2) is the same kind of forward-compat bump for the
/// additive `mux` ref: structurally identical to v5 (an absent `mux` reads as
/// `None`), but stamping v6 forces a pre-mux reader to REJECT rather than
/// silently drop the ref on write-back - losing it would orphan a live
/// mux-hosted agent (badges, inject, and list all dispatch on the ref during
/// the dual-run window). Accepted set widens to 1..=6.
///
/// v7 (screen-manifest fallback authority) is the same bump for the additive
/// `screen_state` verdict: absent reads as `None`, but a pre-v7 writer would
/// silently drop a stored verdict on write-back and blind the manifest rung
/// of the badge lattice. Accepted set widens to 1..=7.
// v8 (x-ec59) is the canonical-identity bump for `harness` / `harness_session_id`
// (mirrors Python's SCHEMA_VERSION): a pre-v8 reader rejects the store rather than
// silently dropping the canonical fields on a read-modify-write.
//
// v9 (x-1b1e) removes `claude_short_id`: the claude jobId (a pure prefix of the
// session UUID) now lives in `short_id`, unifying the transport-key field across
// providers. A legacy row's `claude_short_id` backfills into `short_id` on load
// (see `backfill_short_id`); a pre-v9 reader must reject a v9 store rather than
// drop the jobId on a read-modify-write. Accepted set widens to 1..=9.
//
// v10 (x-880e) removes the on-disk `provider` field and the legacy per-provider
// session-id trio (`codex_session_id`, `gemini_session_id`, `claude_session_uuid`):
// `harness` is the sole identity axis and `harness_session_id` the sole session id.
// A legacy row's `provider` backfills `legacy_provider` -> `harness`, and each
// per-provider key backfills `harness_session_id`, at load (accept-on-read); those
// keys are `skip_serializing` so they never round-trip. A pre-v10 reader must reject
// a v10 store rather than mis-read a harness-only row. Accepted set widens to 1..=10.
//
// v11 (US9) adds the crown fields (`crown_level`/`crown_scope`/`crown_grantor`),
// mirrored here as additive-optional passthrough so the daemon preserves a
// spawn-stamped crown across a read-modify-write (a Python-only field would be
// dropped when the daemon re-serializes the row). Python's asdict emits them on
// every written row, so a pre-v11 reader must reject a v11 store rather than
// TypeError on the unknown keys. Accepted set widens to 1..=11.
//
// v12 (x-ae2d) adds `route_settings_path` - the route-settings file a routed
// worker was launched with - mirrored here for the same reason as the crown
// fields: a Python-only field is dropped when the daemon re-serializes the row,
// which would leave the relaunch guard reading None on every row the daemon has
// touched. Python's asdict emits the key on every written row, so a pre-v12
// reader must reject a v12 store. Accepted set widens to 1..=12.
//
// v13 (x-0358) adds `fno_id` - the target-minted run id of an adopted /target
// orphan, so a revived session is durably linked to the node it was working
// (recoverable even if the worktree moves). Same X3 passthrough rationale: a
// Python-only field would be dropped on the daemon's read-modify-write.
// Accepted set widens to 1..=13.
//
// v14 (x-e21e) adds `delivery_policy` - a recipient's mail delivery policy
// ("bus-only": never prompt-line inject, always the durable bus). A
// DELIVERY-POLICY fact, never a liveness verdict (mail_inject.rs documents the
// not-live misnomer that misled readers twice). Same X3 passthrough rationale:
// a Python-only field would be dropped on the daemon's read-modify-write.
// Accepted set widens to 1..=14.
//
// v15 restores `provider` with its literal model-provider meaning, separate
// from the harness identity. Rows through v14 still treat an on-disk provider
// as the removed harness alias and migrate it at the read choke point.
//
// v16 (x-944f) adds `origin` and `spawn_trigger` - two fields Python's
// AgentEntry has written for releases and Rust never modelled, so every Rust
// write re-serialized the row from the typed struct and dropped them. Measured
// 2026-08-20: 0 of 37 live rows carried either key. The bump is what turns a
// pre-v16 binary's SILENT erasure into a loud refusal, which is the same
// reason v11-v14 bumped for their own mirrors.
//
// v17 (x-d401) adds `model_basis`, the requested-vs-verified qualifier on
// `model`. Same rationale as v16: a pre-v17 binary re-serializes the row from
// its typed struct and drops the key, and a pre-v17 Python reader would see an
// unknown key AT its own schema and TypeError. Accepted set widens to 1..=17.
//
// v18 adds predecessor/fork lineage fields. v19 (x-de10) adds
// `sandbox_posture` - the sandbox posture a codex thread was launched with,
// applied by `thread/resume` across a daemon restart. Same additive-optional
// shape as v11-v18: skip-when-None keeps old rows slim, and the bump turns a
// pre-v19 reader's silent erasure into a loud refusal.
//
// v20 adds the account axis (`launch_account`) and the one optional
// `related_session_id`. An older writer must refuse rather than erase those
// fields during a read-modify-write. Accepted set widens to 1..=20.
//
// v21 (x-98ab) adds `node` - the backlog node a row works, stamped by the
// Python spawn seams from resolved provenance so a reap decision never parses
// the node out of a name. Additive-optional passthrough: without this mirror a
// daemon read-modify-write drops the Python stamp. Accepted set widens to 1..=21.
//
// v22 (x-ac6b) adds `keeper_child_pid` - the process a lane-B keeper hosts,
// the daemon's restart-sweep assertion. The bump is not for the reader (an
// absent key reads as None) but for the WRITER: a pre-v22 daemon accepts the
// unknown key through serde and its next read-modify-write silently drops it,
// after which the respawn-detection comparison has no recorded pid and
// backfills whichever child currently answers. The bump turns that silent
// erasure into a loud refusal, the same reason v16 bumped for origin.
// Accepted set widens to 1..=22.
//
// v23 (x-3837) adds `substrate` - the lane a row was spawned on ("pane",
// "thread", "headless"), stamped once at birth by the writer that resolved the
// lane so a later restore reads the lane instead of guessing it off a mux ref
// or a pid. `None` on rows whose writer cannot know (adopt, manifest
// synthesis); ABSENCE MEANS UNKNOWN, never "pane". Same writer-refusal
// rationale as v22: the stamp is written once and read much later, so an
// erasure on read-modify-write is unrecoverable rather than self-healing.
// Accepted set widens to 1..=23.
//
// v24 (x-2019) adds the requested axis - `requested_model` /
// `requested_provider` / `requested_effort`, the spawn REQUEST verbatim as
// typed beside the observed axes, so a silent substitution is a one-line diff.
// Same writer-protection rationale as v22/v23: a pre-v24 writer accepts the
// unknown keys and erases them on its next read-modify-write. Measured live
// 2026-09-01: a writer without the fields erased the stamps at an EQUAL
// version number, so this takes the next free number instead of reusing 23.
// Accepted set widens to 1..=24.
//
// v25 adds the explicit model-route identity - `route_provider_id` /
// `model_name` / `account_record_id`, captured at spawn, identifiers only.
// The provider-outage supervisor joins outage evidence on these; a row
// without them is a blind spot for that collector, never a default. Mirrors
// Python's AgentEntry; same additive-optional writer-protection rationale as
// v22-v24: a pre-v25 writer accepts the unknown keys and erases them on its
// next read-modify-write. Accepted set widens to 1..=25.
//
// v26 adds the served facts - `liveness` / `liveness_measured_at` (written
// only by the reconcile sweep, always paired) and `harness_title` (the
// sweep's last-seen title baseline). All three are additive-optional; the
// bump is what makes a pre-v26 reader degrade (drop the keys, refuse the
// write) instead of TypeError on the unknown AgentEntry kwargs at an equal
// version number. Accepted set widens to 1..=26.
//
// v27 (x-04ce) adds `launch_account_source` - WHO chose the row's
// `launch_account`: "caller" (a flag on this spawn's argv) or "config"
// (accounts.quota.pick_on_launch picked it). None on every other row:
// launch_account "default" already says nobody chose, a revive inherits the
// source row's stamp, and mints that cannot attribute stay silent. The
// provenance vocabulary is shared with the spawn receipt (`account_source`)
// and defined once in Python's `spawn_flag_owners`. Before it, a config
// injection read as a caller decision - the exact misread this column ends.
// Same writer-protection rationale as v22-v25: a pre-v27 writer accepts the
// unknown keys and erases them on its next read-modify-write. Accepted set
// widens to 1..=27.
// Rendered by build.rs from src/registry_schema.toml (the version's single
// owner); see that file for the bump protocol.
include!(concat!(env!("OUT_DIR"), "/registry_schema.rs"));
/// Current per-agent state schema version (design: schema v1).
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Errors from state-file access.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("state json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "registry schema_version {found} unsupported; this fno understands 1..={max}. \
         Upgrade or downgrade fno to match."
    )]
    UnsupportedSchemaVersion { found: u32, max: u32 },
    #[error("registry invariant violation: {0}")]
    InvariantViolation(String),
    /// The mirror of Python's `_refuse_source_ahead_schema_bump`. Typed rather
    /// than folded into `InvariantViolation` so `state_error_code` can hand the
    /// daemon client a distinguishable code: this is the caller's own binary
    /// being ahead of the deployment, not a corrupt file.
    #[error(
        "refusing to raise the shared registry at {path} from schema_version={found} \
         to schema_version={current}: this fno is running from source at {source_root}, \
         not from the deployed install, so the bump exists only on this branch and \
         every deployed reader on the machine would degrade until it merges. Either \
         deploy this schema (fno doctor update), or point this checkout at its own \
         registry (FNO_AGENTS_HOME, or config.paths.agents_registry_path for the \
         Python side)."
    )]
    SourceAheadSchemaBump {
        path: String,
        found: u32,
        current: u32,
        source_root: String,
    },
    /// The blocking-pool task reading the registry was cancelled by daemon
    /// shutdown before it ran, not by a failure in the read itself.
    #[error("cancelled during shutdown: {0}")]
    Cancelled(String),
}

/// The daemon-owned agent registry (`~/.fno/agents/registry.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Registry {
    pub schema_version: u32,
    /// Rows. Python's `registry.write_registry` (cli/.../agents/registry.py)
    /// stores these under the canonical top-level `"agents"` key and reads ONLY
    /// that key (no `entries` fallback). Serialize under `agents` so a Rust write
    /// verb (`rm`/`stop`/reconcile) that rewrites a Python-authored registry
    /// leaves it readable by Python rather than stranding the surviving rows
    /// under an `entries` key Python ignores (Codex P1, PR #364). `alias =
    /// "entries"` keeps reading older daemon-written registries. Combined with
    /// ab-e5a57efa this makes the typed read path parse Python registries.
    #[serde(default, rename = "agents", alias = "entries")]
    pub entries: Vec<RegistryEntry>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            schema_version: REGISTRY_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Every session id one row answers to at the full-id tier: its own harness
/// session id, the one optional related id (a fork's uuid addresses its row
/// too - both stay valid forever), and predecessor ids (a succeeded session
/// follows the row that answers as its successor).
fn entry_session_ids(e: &RegistryEntry) -> impl Iterator<Item = &str> {
    [
        e.harness_session_id.as_deref(),
        e.related_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .chain(e.predecessor_session_ids.iter().map(String::as_str))
}

/// True when `harness`/`session_id` is the row's PRIMARY KEY: same harness,
/// full-id tier on any of the row's session ids.
fn session_key_matches(e: &RegistryEntry, harness: &str, session_id: &str) -> bool {
    e.harness_name() == harness
        && entry_session_ids(e).any(|sid| session_handle_tier(session_id, sid) == Some(0))
}

/// True when `token` is one of the row's LABELS: its own name or a prior
/// label in `aliases`. Identity is not consulted here.
fn label_matches(e: &RegistryEntry, token: &str) -> bool {
    e.name == token || e.aliases.iter().any(|a| a == token)
}

impl Registry {
    /// Resolve by the primary key. The pair, never the id
    /// alone: id shapes differ per harness (claude uuid4, codex uuid7 whose
    /// head-8 collides inside one minute, opencode case-sensitive `ses_`).
    pub fn find_by_session(&self, harness: &str, session_id: &str) -> Option<&RegistryEntry> {
        self.entries
            .iter()
            .find(|e| session_key_matches(e, harness, session_id))
    }

    pub fn find_by_session_mut(
        &mut self,
        harness: &str,
        session_id: &str,
    ) -> Option<&mut RegistryEntry> {
        let idx = self
            .entries
            .iter()
            .position(|e| session_key_matches(e, harness, session_id))?;
        self.entries.get_mut(idx)
    }

    /// How many rows answer to `token` as a LABEL (own name or a prior
    /// label). Two or more means the label has no honest row: callers that
    /// cannot resolve by identity must refuse, never take the first match.
    pub fn label_matches_count(&self, token: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| label_matches(e, token))
            .count()
    }

    /// Find by token: LABEL first (own name, then prior labels), then
    /// identity (the full-id session tier). Name is demoted to a
    /// mutable alias; session id is the key this falls back for. An
    /// AMBIGUOUS label (two rows answer it) resolves to nothing: picking the
    /// first match would let one of two honest rows receive the other's
    /// write, and labels are unique at birth, so this only ever fires on a
    /// corrupted store (labels are unique at birth).
    pub fn find(&self, token: &str) -> Option<&RegistryEntry> {
        let labels = self.label_matches_count(token);
        if labels > 1 {
            return None;
        }
        self.entries
            .iter()
            .find(|e| label_matches(e, token))
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| session_handle_tier_any(e, token))
            })
    }

    pub fn find_mut(&mut self, token: &str) -> Option<&mut RegistryEntry> {
        if self.label_matches_count(token) > 1 {
            return None;
        }
        let idx = self
            .entries
            .iter()
            .position(|e| label_matches(e, token))
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|e| session_handle_tier_any(e, token))
            })?;
        self.entries.get_mut(idx)
    }

    /// Find an entry by agent name or its canonical full harness session id.
    /// The one optional related id resolves at the same tier: a fork's full
    /// uuid addresses its row too (both ids stay valid forever). A
    /// predecessor id resolves at the full tier only (x-dfe7): delivery
    /// naming a succeeded session follows the row that now answers as its
    /// successor.
    pub fn find_name_or_full_session_id(&self, token: &str) -> Option<&RegistryEntry> {
        self.entries
            .iter()
            .find(|entry| entry.name == token || session_handle_tier_any(entry, token))
    }
}

/// True when `token` resolves the row at the full-id session tier.
fn session_handle_tier_any(e: &RegistryEntry, token: &str) -> bool {
    entry_session_ids(e).any(|sid| session_handle_tier(token, sid) == Some(0))
}

/// The `(harness, full session id)` write key for a row resolved outside a
/// locked write. Capture BEFORE the update; re-find with
/// [`Registry::find_by_session_mut`] inside the closure so a same-name
/// replacement cannot receive the first row's write. `None` id = a legacy
/// row; its writer falls back to the demoted label lookup.
pub fn registry_write_key(e: &RegistryEntry) -> (String, Option<String>) {
    (e.harness_name().to_string(), e.harness_session_id.clone())
}

/// Re-find under the write lock by the identity [`registry_write_key`]
/// captured, falling back to the demoted label lookup only for a legacy row
/// with no session id.
pub fn find_keyed_mut<'r>(
    reg: &'r mut Registry,
    key: &(String, Option<String>),
    name: &str,
) -> Option<&'r mut RegistryEntry> {
    let keyed = key.1.as_deref().and_then(|sid| {
        reg.entries
            .iter()
            .position(|e| session_key_matches(e, &key.0, sid))
    });
    let idx = match keyed {
        Some(i) => Some(i),
        None => {
            if reg.label_matches_count(name) > 1 {
                None
            } else {
                reg.entries
                    .iter()
                    .position(|e| label_matches(e, name))
                    .or_else(|| {
                        reg.entries
                            .iter()
                            .position(|e| session_handle_tier_any(e, name))
                    })
            }
        }
    };
    idx.and_then(move |i| reg.entries.get_mut(i))
}

/// Inside-leg agent state (inside-out multiplexer E3, "contract v2"). The inside
/// leg is a hook that reports a claude pane's lifecycle state WITHOUT spawning or
/// sending keystrokes; the daemon stores its latest report on the registry row.
/// Serializes lowercase (`working` / `blocked` / `done`) as the inside-leg
/// wire shape. PTY liveness (`ConnState::Exited`) always overrides
/// this badge -- a dead pane is never resurrected by a stale inside-leg state
/// (umbrella Locked Decision D4).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InsideLegState {
    Working,
    Blocked,
    Done,
}

/// The stored form of one inside-leg report (contract v2: X2). The wire payload
/// the daemon receives is `{session_id, seq, state, reason?, ttl_ms?}`; the
/// daemon adds `received_at` and stores the rest here on the [`RegistryEntry`].
/// `seq` is per-`session_id` monotonic so a reordered/duplicate report can be
/// dropped (`seq <= last_seq`); `ttl_ms` bounds how long the badge stays live
/// before it ages to unknown. NOTE (E3.1 scope): this struct is the storage
/// CONTRACT only -- the seq-drop, TTL-aging, and 3-tier authority BEHAVIOUR that
/// consume these fields land in E3.2/E3.3. Mirrored in Python's `AgentEntry`
/// (`inside_leg: Optional[dict]`, a lossless passthrough) so a row round-trips
/// across the mixed-language registry (X3 / ab-b946b59c).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InsideLegReport {
    pub state: InsideLegState,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub received_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}

/// The stored form of one screen-manifest verdict (the fallback rung of the
/// badge lattice: pane-exit > hook > screen-manifest > liveness). Written only
/// by the daemon's scrape sweep, and ONLY for rows with no `inside_leg`
/// authority (per-capability arbitration: a hook-bearing agent is never
/// scraped). `state` is the manifest vocabulary (`working`/`idle`/`blocked` -
/// note `idle`, not the hook's `done`); `rule` is the matched
/// [`crate::manifest::ManifestRule`] id, kept for the `detect explain`
/// surface; `seq` is per-row monotonic so verdict history orders; `at` is the
/// registry's `YYYY-MM-DDThh:mm:ssZ` stamp and `ttl_ms` bounds reader trust
/// exactly like `inside_leg.received_at`/`ttl_ms` (the sweep refreshes `at`
/// before it lapses, so a live daemon keeps a steady verdict fresh; a dead
/// daemon's last verdict ages out instead of pinning a stale badge). Mirrored
/// in Python's `AgentEntry` as `screen_state: Optional[dict]` (X3 passthrough).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScreenStateReport {
    pub state: String,
    pub rule: String,
    pub seq: u64,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// (x-c929) The answerable-prompt payload when this `blocked` verdict came
    /// from a rule with an `[answer]` grammar and the region yielded a clean
    /// numbered menu; `None` for every other state or a focus-only blocked
    /// prompt. Rides the badge to the sideline (JSON passthrough); the mux
    /// server re-verifies its fingerprint before injecting a picked answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answerable: Option<crate::manifest::AnswerablePrompt>,
}

impl ScreenStateReport {
    /// True while this verdict is trustworthy at `now_secs` - the same aging
    /// discipline as [`InsideLegReport::is_live_at`]: no `ttl_ms` never
    /// self-ages; a TTL'd verdict expires once `at + ttl_ms` passes; an
    /// unparseable `at` fails CLOSED (expired, liveness-only).
    pub fn is_live_at(&self, now_secs: u64) -> bool {
        let Some(ttl_ms) = self.ttl_ms else {
            return true;
        };
        match rfc3339_like_to_secs(&self.at) {
            Some(recv) => now_secs.saturating_sub(recv).saturating_mul(1000) <= ttl_ms,
            None => false,
        }
    }
}

impl InsideLegReport {
    /// True when this report is still authoritative at `now_secs` (epoch
    /// seconds), the TTL half of the 3-tier authority lattice (inside-out E3.3,
    /// AC-X2-2). A report with no `ttl_ms` never ages out on its own -- it is
    /// cleared only by the ordered exit teardown, a `done`, or a newer report.
    /// A report WITH a ttl expires once `received_at + ttl_ms` has passed, so a
    /// `working` whose inside-leg process died (PTY still alive, exit-override
    /// never fires) cannot pin a permanent stale badge. A `received_at` that
    /// does not parse fails CLOSED (treated as expired -> the scraper takes
    /// over), never as live: a corrupt stamp must not be the thing that pins a
    /// forever-`working`.
    pub fn is_live_at(&self, now_secs: u64) -> bool {
        let Some(ttl_ms) = self.ttl_ms else {
            return true;
        };
        match rfc3339_like_to_secs(&self.received_at) {
            Some(recv) => now_secs.saturating_sub(recv).saturating_mul(1000) <= ttl_ms,
            None => false,
        }
    }

    /// True when `received_at` is within `window_secs` of `now_secs` -- a plain
    /// recency test (distinct from `is_live_at`, which never ages a report that
    /// carries no `ttl_ms`). Used as the "provably live" signal that stops an
    /// ask/mail routing miss from false-orphaning a live worker (x-c393). An
    /// unparseable stamp fails CLOSED (not recent), so a corrupt row can never
    /// shield a dead session from orphaning.
    pub fn received_within(&self, now_secs: u64, window_secs: u64) -> bool {
        match rfc3339_like_to_secs(&self.received_at) {
            // A future stamp (recv > now) is corrupt/clock-skewed, not recent:
            // require recv <= now so it cannot suppress orphaning (fail closed).
            Some(recv) => recv <= now_secs && now_secs - recv <= window_secs,
            None => false,
        }
    }
}

/// True when a badge report ENTERS `target` from a different prior state (x-dd84).
/// This is the whole episode gate for the OS-notification wire: firing only on
/// the edge INTO `blocked`/`done` means a repeat report at `target` (prev already
/// `target`) does not re-fire, and a return to `working` then back to `blocked`
/// fires once more - "once per blocked episode" with no per-row bookkeeping. A
/// missing prior report (`None`) counts as entering.
pub fn enters(prev: Option<InsideLegState>, new: InsideLegState, target: InsideLegState) -> bool {
    new == target && prev != Some(target)
}

/// Parse the fixed `YYYY-MM-DDThh:mm:ssZ` UTC stamp the registry writes
/// (`now_rfc3339_like`) back to epoch seconds. Inverse of the daemon's `civil`
/// (epoch -> civil) helper, using Howard Hinnant's days-from-civil. Returns
/// `None` for any shape that is not exactly that form (wrong length, non-digit
/// fields, missing separators) so a malformed or legacy stamp fails the TTL
/// gate closed rather than pinning a stale badge. Fractional seconds / offsets
/// are intentionally unsupported: the only producer is `now_rfc3339_like`,
/// which never emits them.
pub fn rfc3339_like_to_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    // "2026-06-27T00:00:00Z" == 20 bytes, separators at fixed offsets.
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    // Parse the digits straight from the validated byte slice -- no UTF-8
    // boundary check or temporary allocation, and an explicit non-digit reject
    // (gemini review).
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let mut val = 0i64;
        for &ch in b.get(lo..hi)? {
            if !ch.is_ascii_digit() {
                return None;
            }
            val = val * 10 + i64::from(ch - b'0');
        }
        Some(val)
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // days_from_civil (Hinnant): days since 1970-01-01 for a proleptic Gregorian
    // y/m/d. Mirrors the daemon's `civil` constants in reverse.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

/// Where a mux-hosted agent's PTY lives (4a-G2, brief Locked 4/7): the mux
/// session name + the pane id `fno mux pane run` printed. A row carries
/// exactly ONE live ref - `mux` XOR a worker-socket identity (non-empty
/// `short_id`) XOR a `claude --bg` thread (`claude_short_id`) - enforced at
/// write time by [`validate_single_live_ref`]; every consumer (list, badges,
/// inject) dispatches on the ref during the G2-G4 dual-run window. Mirrored in
/// Python's `AgentEntry` as `mux: Optional[dict]` (X3 rule).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MuxRef {
    pub session: String,
    pub pane_id: u64,
}

/// One registry row (design schema v6). Optional fields default to `None` and
/// are preserved across `update_registry` because the whole row round-trips
/// through this typed struct -- but ONLY for fields this struct models. A
/// Python `AgentEntry` field with no counterpart here is DROPPED on the next
/// Rust write, silently, because there is no serde catch-all. `origin` and
/// `spawn_trigger` sat outside the struct that way until x-944f and read
/// 0-of-37 populated on the live fleet as a result. Adding a Python-only field
/// means mirroring it here in the same commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RegistryEntry {
    pub name: String,
    /// Prior labels that must keep resolving, Python's `AgentEntry.aliases`
    /// (`registry.py:343`). `rename_agent` appends the old label here. Without
    /// this mirror a Rust registry write would DROP the field off every
    /// Python-authored row (no serde catch-all on this struct), silencing
    /// every historical address in one write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Daemon-set PTY field. Python's `AgentEntry` now mirrors it as
    /// `short_id: str = ""` (ab-b946b59c) so a real PTY row in a mixed registry
    /// is Python-readable and round-trips losslessly; `skip_serializing_if`
    /// still drops it when empty so a *Rust*-authored exec/ask row stays slim and
    /// a round-tripped Python row omits it (default-to-empty on read, ab-e5a57efa;
    /// Codex P1, PR #364). A real daemon PTY agent always has a non-empty
    /// short_id, so it still serializes for those rows; conversely a one-shot
    /// `ask` row always has an empty short_id (no worker-socket identity). That
    /// exclusivity is what [`RegistryEntry::is_one_shot_ask`] keys on -- a
    /// non-empty short_id on an ask row, or an empty one on a PTY row, is a
    /// producer bug. (Python mirrors with a `str` default, not `Option`, because
    /// a `"short_id": null` would fail this `String` field's deserialize.)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub short_id: String,
    /// v10-v14 backfill-only harness alias. The read choke point moves a
    /// pre-v15 row's `provider` into this field before harness backfill.
    #[serde(default, skip_serializing)]
    pub legacy_provider: String,
    /// Model-provider axis (v15), distinct from the harness identity. It is
    /// stamped from route resolution and never inferred from `harness`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Requested or verified model id for the lane. Optional so older rows
    /// remain lossless when a Python writer adds the axis fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// (x-d401) The basis for `model`: "requested" (stamped at spawn from the
    /// flag or route the caller named) or "verified" (read back from a
    /// verified pane status). A bare model is two facts in one field - the
    /// x-aa8e shape - so the pair travels together; a row with a model and
    /// no basis predates this field and reads as unmarked, never as verified.
    /// Additive-optional like the field it qualifies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_basis: Option<String>,
    /// Reasoning-effort arm used by the lane, when one was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// SERVED harness liveness, one of `alive|dead|unmeasured`,
    /// written ONLY by the reconcile sweep and always paired with
    /// `liveness_measured_at`. It never replaces the stored `status` (the
    /// pane/ask arms still settle that); it exists so no reader has to
    /// believe a status field that is an init-time snapshot - a probe answer
    /// older than two sweep budgets reads as stale, and the field's age is
    /// on the wire beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness_measured_at: Option<String>,
    /// The LAST title the harness reported for this session (claude's
    /// Ctrl+R agent-name record; codex/opencode's index title), kept ONLY as
    /// the diff baseline the sweep's `agent_renamed` emit compares against.
    /// The row's `name` is never written from it: the label is fno's, the
    /// title is the harness's, and every reader is served the probe's fresh
    /// reading with this stored value as fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_title: Option<String>,
    pub cwd: String,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` as
    /// `project_root: str = ""` (ab-b946b59c; see `short_id`): default on read,
    /// skip-when-empty on write.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_root: String,
    /// On disk this is Rust-set only (Python's `session_id` is a computed
    /// `@property`, excluded from its serialized rows): skip when absent so
    /// Python can read a Rust-written row (Codex P1). When a Rust PTY row DOES
    /// record one, Python's load_registry drops the key before constructing the
    /// entry and recomputes the same projection from the *_session_id fields
    /// (ab-b946b59c).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The FULL claude session UUID -- the stream-json `--resume` target,
    /// distinct from the 8-hex jobId in `short_id`. v10 (x-880e): a load-derived
    /// in-memory alias only. `skip_serializing` keeps it off disk (harness_session_id
    /// is the sole persisted session id); `backfill_harness_aliases` populates it
    /// from `harness_session_id` on load, so the ~30 daemon read sites need no churn.
    /// A post-load mutation of this field is synced back into `harness_session_id`
    /// at the write choke point (AC6-FR). [stream-json host lane node]
    #[serde(default, skip_serializing)]
    pub claude_session_uuid: Option<String>,
    /// Canonical harness identity (x-ec59), mirroring Python's `AgentEntry`:
    /// `harness` is the harness name (identity only -- `provider` stays
    /// load-bearing for dispatch) and `harness_session_id` is the worker's own
    /// session id in its harness's store. Both additive-optional, back-filled
    /// from the legacy per-provider fields at load via
    /// [`RegistryEntry::backfill_harness_aliases`] so a Rust reader of a legacy
    /// row and a Python reader of a Rust-minted canonical row both resolve.
    /// Skip-when-`None` keeps a Rust-authored row slim; Python's `asdict` always
    /// emits the key, so a Python row round-trips fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    /// Historical current-session ids retained by a classified succession.
    /// Delivery follows `harness_session_id`; this list is audit provenance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predecessor_session_ids: Vec<String>,
    /// Immediate predecessor for a live branch row. Branch rows keep their own
    /// full session id and stable fno id rather than sharing a mutable row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_session_id: Option<String>,
    /// The ACCOUNT axis this worker was launched under (x-d285, v20). Three
    /// values, never two: `Some("default")` (the spawn positively pinned no
    /// account), `Some(<account-id>)` (explicit or headroom-picked), `None`
    /// (legacy row or a mint that cannot know - never readable as default,
    /// because a silent default is how the wrong bill gets paid). Mirrors
    /// Python's `AgentEntry.launch_account`; the re-entry resolver reads it to
    /// rebuild `CLAUDE_CONFIG_DIR` or refuse. Same X3 passthrough as
    /// `route_settings_path`: without this mirror a daemon read-modify-write
    /// drops a Python-stamped account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_account: Option<String>,
    /// WHO chose `launch_account` (x-04ce, v26): `"caller"` (a flag on this
    /// spawn's argv) or `"config"` (`accounts.quota.pick_on_launch` picked
    /// it). `None` when launch_account is `"default"` (nobody chose - the
    /// value already says so), when a revive inherited the account, and on
    /// rows whose mint cannot attribute the choice. Mirrors Python's
    /// `AgentEntry.launch_account_source`; same X3 passthrough so a daemon
    /// write-back preserves the stamp. The vocabulary is shared with the
    /// spawn receipt's `account_source` and defined once in Python's
    /// `spawn_flag_owners`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_account_source: Option<String>,
    /// The SECOND valid session id an additive fork/background minted on this
    /// row (x-d285, v20). Both ids stay valid forever and resolve to the same
    /// row and launch binding; neither replaces the other, and at most ONE
    /// optional id exists (no list, edge, or lineage graph). Mirrors Python's
    /// `AgentEntry.related_session_id`; same X3 passthrough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_session_id: Option<String>,
    /// The backlog node this row WORKS (x-98ab, v21), mirroring Python's
    /// `AgentEntry.node`: stamped once at birth by the Python spawn seams from
    /// the spawn's resolved provenance and by the client-side ask lanes from
    /// their inherited `FNO_NODE`, so a reap decision reads the node instead of
    /// parsing it out of a name. `None` on rows whose writer cannot know:
    /// adopt, and daemon-hosted mints (the daemon's ambient `FNO_NODE` names
    /// whatever session started the daemon, never the child's node). ABSENCE
    /// MEANS UNKNOWN, never "ad-hoc" - the same discipline as `origin`. Same
    /// X3 passthrough duty as every Python-stamped field: a row the daemon
    /// re-serializes must keep the stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// v23 (x-2019): the spawn REQUEST, verbatim as the flags spelled it (any
    /// `[1m]` suffix included), stamped once at birth beside the observed
    /// axes. `model`/`model_basis` flip to a verified observation; these never
    /// do, so requested-vs-observed stays a one-line diff. Absence means
    /// unknown, the `node`/`origin` discipline - never a default. Mirrors
    /// Python's `AgentEntry.requested_*`; same X3 passthrough duty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<String>,
    /// Explicit model-route identity captured by the spawn path (v25),
    /// mirroring Python's `AgentEntry.route_provider_id`/`model_name`/
    /// `account_record_id`. These fields contain stable identifiers only;
    /// credentials and route settings remain in their protected stores. Same
    /// X3 passthrough duty as every Python-stamped field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_record_id: Option<String>,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` (ab-b946b59c):
    /// skip when absent (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messaging_socket_path: Option<String>,
    // v10 (x-880e): load-derived in-memory aliases only; skip_serializing keeps
    // them off disk (harness_session_id is the sole persisted session id) and
    // backfill_harness_aliases populates them on load, so daemon read sites need
    // no churn. A post-load mutation syncs back at the write choke point (AC6-FR).
    #[serde(default, skip_serializing)]
    pub codex_session_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub gemini_session_id: Option<String>,
    #[serde(default)]
    pub mcp_channel_id: Option<String>,
    /// Hosting mode: absent/`None` == `"exec"` (one-shot, the default for every
    /// pre-existing row), `Some("interactive")` == a long-lived drivable TUI
    /// (`fno agents host`/`promote`). Skip-when-`None` so a *Rust*-authored exec
    /// row omits the key; Python's missing-key coercion then maps the absence
    /// back to `"exec"`. (Python itself always emits the key via `asdict` -- as
    /// `"exec"` or `"interactive"` -- and Rust reads the concrete value fine, so
    /// both directions agree.) Consumers must read it via
    /// [`RegistryEntry::host_mode_or_default`], never the raw `Option`, so the
    /// absent==exec rule lives in one place. [interactive-drive node]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_mode: Option<String>,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` (ab-b946b59c):
    /// skip when absent (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc_session_id: Option<String>,
    pub status: AgentStatus,
    #[serde(default)]
    pub last_message_at: Option<String>,
    pub created_at: String,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` as
    /// `pid: Optional[int]` (ab-b946b59c): skip when absent so a round-tripped
    /// Python row stays slim and Python-readable (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The worker process's start time, captured alongside `pid` at spawn, used
    /// to detect PID reuse: a liveness/reap/signal decision treats `pid` as "our
    /// worker" only if the live process's start time still matches this
    /// (ab-d19e6458). Per-host, per-boot value (Linux: `/proc/<pid>/stat` field
    /// 22 in clock ticks; macOS: `kinfo_proc` start `timeval` in microseconds) —
    /// only ever compared for equality against a fresh read of the SAME pid, so
    /// the unit/epoch difference across platforms is irrelevant. Daemon-set PTY
    /// field, mirrored in Python's `AgentEntry` (ab-b946b59c); skip when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start_time: Option<u64>,
    /// The KEEPER's child pid for a lane-B thread row (x-ac6b): the process
    /// the keeper hosts, learned from the spawn Identify reply and re-asserted
    /// unchanged by the registry-side keeper sweep on every daemon start. A
    /// changed pid means something respawned and is wearing the row's name -
    /// the exact failure that field exists to catch. Distinct from `pid`,
    /// which for these rows is the KEEPER's own pid. Mirrors Python's
    /// `AgentEntry.keeper_child_pid`; skip-when-absent so a pre-field row
    /// reads as unknown, never as a mismatch. Gated by the v22 schema bump:
    /// an older writer must refuse the store rather than silently erase the
    /// assertion input on read-modify-write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keeper_child_pid: Option<u32>,
    /// The substrate this row was spawned on: "pane", "thread" or "headless".
    /// `None` on a row whose writer cannot know (adopt, manifest synthesis) -
    /// ABSENCE MEANS UNKNOWN, never "pane", because a silent default would tell
    /// restore to resurrect a session that exited on purpose. Mirrors Python's
    /// `AgentEntry.substrate`; gated by the v23 schema bump so an older writer
    /// refuses the store rather than erasing a stamp on read-modify-write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substrate: Option<String>,
    #[serde(default)]
    pub log_path: Option<String>,
    /// Timestamp of the most recent reconcile probe (finding #1 High): the
    /// reconcile sweep orders entries by ASC `last_reconciled_at` so a
    /// budget-exhausted sweep stays fair across a large registry. Daemon-set,
    /// mirrored in Python's `AgentEntry` (ab-b946b59c); skip when absent (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<String>,
    /// Latest inside-leg report for this row's claude pane (inside-out E3,
    /// contract v2). `None` for every non-inside-leg row (the default for every
    /// pre-existing row, and for any provider/lane that does not run a hook).
    /// Skip-when-`None` so a row without a report stays slim and a stale reader
    /// rejects via the v5 schema bump rather than silently dropping it. Mirrored
    /// in Python's `AgentEntry` as `inside_leg: Optional[dict]` (X3 / ab-b946b59c).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inside_leg: Option<InsideLegReport>,
    /// When reconcile's Exited transition proved this row's backing process
    /// gone (ISO 8601 UTC), cleared again if current evidence contradicts it.
    /// Retirement no longer reads it (x-c672: the reverse join plus transcript
    /// quiet decide); the liveness ladder's heartbeat rung does, treating a
    /// heartbeat advancing past it as life. Deliberately NOT `last_reconciled_at`
    /// (reconcile re-stamps that on every probe, so it can't anchor a stable
    /// clock). Daemon-set, mirrored in Python's `AgentEntry` as `exited_at`;
    /// skip when absent so a pre-GC row round-trips losslessly
    /// (additive-optional, no schema bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<String>,
    /// The mux hosting ref for a pane-substrate agent (4a-G2): `Some` means
    /// this row's PTY is a pane in `mux.session`, and pane-exit facts /
    /// live-inject / sideline badges all key on it. `None` for every daemon
    /// worker, bg-thread, and headless row. One live ref per row (mux XOR
    /// worker XOR bg) - see [`MuxRef`] and [`validate_single_live_ref`].
    /// Skip-when-`None` so a pre-mux row stays slim; a stale reader rejects
    /// via the v6 schema bump rather than silently dropping the ref. Mirrored
    /// in Python's `AgentEntry` as `mux: Optional[dict]` (X3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mux: Option<MuxRef>,
    /// Latest screen-manifest verdict for this row's mux pane (v7, the
    /// fallback rung under the hook). Daemon-scrape-set, and mutually
    /// exclusive with a live `inside_leg` authority BY THE WRITER (the sweep
    /// skips hook-bearing rows; the inside-leg store clears this field on the
    /// capability flip) - readers still treat inside_leg as unconditionally
    /// senior, defense in depth. Skip-when-`None` so an unscraped row stays
    /// slim and a stale reader rejects via the v7 bump rather than silently
    /// dropping a verdict. Mirrored in Python's `AgentEntry` as
    /// `screen_state: Optional[dict]` (X3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_state: Option<ScreenStateReport>,
    /// Crown fields (US9, v11): who holds an orchestrator crown and at what
    /// altitude. The Python spawn path is the sole writer (grantor-stamped,
    /// never self-declared); the daemon only custodies them so a spawn-stamped
    /// crown round-trips losslessly across a read-modify-write - the same X3
    /// passthrough treatment as `inside_leg`/`screen_state`. Skip-when-`None`
    /// keeps a Rust-authored uncrowned row slim; Python's `asdict` always emits
    /// the keys, so a crowned Python row round-trips fine. Crown liveness ==
    /// this row's liveness (no separate lifecycle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crown_level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crown_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crown_grantor: Option<String>,
    /// Route-settings path (x-ae2d, v12): the `route-settings/<sha16>.json`
    /// this worker was launched with, or `None` when it was never routed. The
    /// Python spawn seams are the sole writers and the Python relaunch paths
    /// the sole readers; the daemon only custodies it so a stamped path
    /// survives a read-modify-write - without this mirror the daemon's next GC
    /// or screen-state write would drop it and the relaunch guard would read
    /// `None` on every row it had touched. Same X3 passthrough treatment as
    /// `crown_*`. A path, never route contents: that file is 0600 and carries a
    /// live `ANTHROPIC_AUTH_TOKEN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_settings_path: Option<String>,
    /// fno do target run id (x-0358, v13): the `fno_id` of the /target session an
    /// adopted orphan was working, so the revived session is linked to its node.
    /// Set by the adopt verb from the matched `.fno/target-state.md`; `None` for
    /// every row that did not come from a target manifest. Identity-adjacent
    /// linkage, or the stable pane identity for a harness without a session id;
    /// never read for liveness or ownership. Same X3 passthrough as
    /// `route_settings_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fno_id: Option<String>,
    /// Mail delivery policy (x-e21e, v14): `Some("bus-only")` means mail to
    /// this recipient never prompt-line injects and always takes the durable
    /// bus; `None` is the default injectable policy every worker keeps. A
    /// DELIVERY-POLICY fact, never a liveness verdict - the same distinction
    /// that renamed `NOT_INJECTABLE` off "not-live" (`mail_inject.rs`): a
    /// bus-only session may be alive and mid-turn, it just belongs on the bus.
    /// Stamped by the session itself via `fno agents register
    /// --delivery-policy bus-only`; the Python-side send path is the reader and
    /// the gate. Same X3 passthrough as `fno_id`: without this mirror the
    /// daemon's read-modify-write would drop a Python-stamped policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_policy: Option<String>,
    /// Sandbox posture the worker was LAUNCHED with (x-de10, v19):
    /// `Some("danger-full-access")` for a `--yolo` codex thread,
    /// `Some("workspace-write")` for the bounded default. Recorded at spawn by
    /// the codex thread lane and applied by `thread/resume`, so a daemon
    /// restart can no longer silently demote a danger-full-access worker to
    /// workspace-write (or escalate, if a posture is ever added that the
    /// resume lane defaults away from). `None` on rows that predate the field
    /// or whose harness records no posture; the resume lane then applies the
    /// safe default. Same X3 passthrough duty as `origin`: a field the struct
    /// does not know is dropped on write-back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_posture: Option<String>,
    /// Registration origin (x-944f, v16), mirroring Python's `AgentEntry`:
    /// `Some("operator")` for a session a human started by hand (`fno agents
    /// register`, `/fno-me`), `Some("spawn")` for a footnote-created worker,
    /// `Some("adopted")` for one the harness-store healer found already
    /// running, `None` for a row nothing ever stamped. The reap lane reads it
    /// as a PROTECTOR: an operator row is never reaped. That makes the erasure
    /// this mirror closes a safety defect, not a cosmetic one -- before it,
    /// every Rust write dropped the stamp and every reap decision was made
    /// without the one field that answers "is a human sitting in this session".
    /// `None` and `"spawn"` are NOT the same fact; only `"operator"` protects,
    /// and only `"spawn"` retires.
    ///
    /// The mirror is load-bearing for the same X3 passthrough reason as
    /// `delivery_policy`: Python stamps this at row birth, the daemon touches
    /// the same rows, and a field this struct does not know is dropped on
    /// write-back. Without it the watchdog's retire lane reads every long-lived
    /// worker as unknown and stops reclaiming slots, and the mail escalation
    /// loses every operator row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// What caused this spawn (x-42c5), mirroring Python's `AgentEntry`. Same
    /// X3 passthrough as `origin` and erased by the same defect: it shipped
    /// Python-only and read 0-of-37 on the live fleet. Distinct from `origin`,
    /// which answers only human-or-not; this carries the cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_trigger: Option<String>,
    /// The spawn-time parent edge (x-132c), mirroring Python's `AgentEntry`
    /// (declared x-30f6, read 0-of-30 on the live fleet because no writer
    /// stamped it). Ambient-captured from the SPAWNING session's environment
    /// at every mint site; never required of a caller. Same X3 passthrough as
    /// `origin`: a field this struct does not know is dropped on daemon
    /// write-back, which is exactly how the fleet lost the Python stamps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by_harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by_cwd: Option<String>,
    /// v9 backfill-only (x-1b1e): the removed `claude_short_id`. Deserialized
    /// (under its old key) so a legacy row's jobId survives the read, but NEVER
    /// serialized -- [`RegistryEntry::backfill_short_id`] moves it into
    /// `short_id` at load and clears it, so it never round-trips. This is the
    /// Rust mirror of Python's `load_registry` popping `claude_short_id` from the
    /// raw row. Not part of identity; no consumer reads it directly.
    #[serde(default, rename = "claude_short_id", skip_serializing)]
    pub legacy_claude_short_id: Option<String>,
}

/// The one-live-ref invariant (brief Locked 7), checked at write time by both
/// [`update_registry`] (Rust) and Python's `write_registry`: a row that carries
/// the `mux` ref must not ALSO carry a transport identity (non-empty `short_id`:
/// a worker-socket key or, since v9, a `claude --bg` jobId) - a double-ref row
/// would make consumers dispatch the same agent down two substrates. Scoped to
/// mux rows only: pre-existing worker/bg field combinations are not this
/// invariant's business. (Backfill runs before this check, so a legacy bg
/// row's jobId is already in `short_id`.)
pub fn validate_single_live_ref(entry: &RegistryEntry) -> Result<(), String> {
    if entry.mux.is_none() {
        return Ok(());
    }
    if !entry.short_id.is_empty() {
        return Err(format!(
            "registry row {:?} carries a mux ref alongside a worker/bg ref; a row holds exactly one live ref (mux XOR worker XOR bg)",
            entry.name,
        ));
    }
    Ok(())
}

/// The resolvable-handle invariant (x-7bcd): at creation, every registry row
/// carries at least one handle an outside observer can resolve without asking
/// the worker anything. Any one of three legs satisfies it: (1) `pid` +
/// `pid_start_time`, when the writer owns the process; (2) a non-empty
/// `log_path`, when the writer has created the file it records (file
/// existence is enforced at the mint site, not here -- a stat per row under
/// the write lock is a stall this check does not need); (3) `harness` +
/// `harness_session_id`, when the writer owns neither a pid nor a log file.
/// Scoped to NEW rows only by the caller (a pre-existing violating row must
/// never wedge the registry -- AC3-FR); this function itself does no I/O and
/// makes no new-vs-existing distinction.
pub fn validate_resolvable_handle(entry: &RegistryEntry) -> Result<(), String> {
    let leg1 = entry.pid.is_some() && entry.pid_start_time.is_some();
    let leg2 = entry.log_path.as_deref().is_some_and(|p| !p.is_empty());
    let leg3 = entry.harness.as_deref().is_some_and(|h| !h.is_empty())
        && entry
            .harness_session_id
            .as_deref()
            .is_some_and(|s| !s.is_empty());
    if leg1 || leg2 || leg3 {
        return Ok(());
    }
    Err(format!(
        "registry row '{}' carries no resolvable handle: needs one of (pid + pid_start_time), \
         log_path, or (harness + harness_session_id)",
        entry.name,
    ))
}

/// `host_mode` value for a one-shot exec session (the default when absent).
pub const HOST_MODE_EXEC: &str = "exec";
/// `host_mode` value for a long-lived drivable interactive session.
pub const HOST_MODE_INTERACTIVE: &str = "interactive";

/// The env key the Python spawn seam sets when an account overlay was applied
/// (x-d285). An `--account` bg spawn execs into this binary with the overlay
/// in `os.environ`; the account ID itself has no argv carrier, so the seam
/// publishes it here for the row mint to stamp. Absent on route-bearing and
/// pane spawns, which never leave Python.
pub const LAUNCH_ACCOUNT_ENV_KEY: &str = "FNO_LAUNCH_ACCOUNT";

/// The launch-account value a Rust mint seam stamps (x-d285). Three-valued,
/// mirroring Python: an explicit `FNO_LAUNCH_ACCOUNT` wins; else an ambient
/// `CLAUDE_CONFIG_DIR` means a config namespace is in play this mint cannot
/// attribute, so the row records unknown (`None`) rather than "default";
/// neither set proves the true default slot, so `Some("default")`.
pub fn launch_account_from_env() -> Option<String> {
    if let Ok(id) = std::env::var(LAUNCH_ACCOUNT_ENV_KEY) {
        if !id.is_empty() {
            return Some(id);
        }
    }
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    Some("default".to_string())
}

/// The carrier for launch-account PROVENANCE across the exec seam. The
/// Python seam that injects a headroom-picked `--account`
/// (`_pick_account_at_seam`) sets it to `"config"` in the same breath, so
/// the Rust mint can tell an injected pick from a flag the operator typed.
/// An id-adjacent adjective, never a credential.
pub const LAUNCH_ACCOUNT_SOURCE_ENV_KEY: &str = "FNO_LAUNCH_ACCOUNT_SOURCE";

/// WHO chose the row's launch account (x-04ce). A source rides a concrete
/// account: only when `launch_account_from_env()` named a specific id (not
/// `"default"`, not unknown) does a source exist, and the carrier must then
/// speak the vocabulary (`"caller"` / `"config"`); anything else reads as the
/// flag (`"caller"`). A source over `"default"` or an unknown id is the
/// contradiction this pairing forbids - nobody config-picked the fallback.
pub fn launch_account_source_from_env() -> Option<String> {
    let id = launch_account_from_env()?;
    if id == "default" {
        return None;
    }
    match std::env::var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY) {
        Ok(src) if src == "caller" || src == "config" => Some(src),
        _ => Some("caller".to_string()),
    }
}

/// The launch-account pair a mint seam stamps (x-04ce): the row's
/// `launch_account` fact plus WHO chose it. One read so a seam cannot take
/// the two env reads at different moments.
pub fn launch_provenance_from_env() -> (Option<String>, Option<String>) {
    (launch_account_from_env(), launch_account_source_from_env())
}
/// `host_mode` value for an ADOPTED `claude --bg` session footnote holds live via
/// a daemon `control.sock` attach (G1 held-attach substrate, x-26df). Distinct
/// from `interactive` (a footnote-SPAWNED PTY worker): an `attached` row's process
/// is Claude's, not footnote's, and it is driven over the held attach, not a
/// worker socket. G2 teaches grid to consume it; the standard worker reconcile
/// must not treat it as a managed PTY worker.
pub const HOST_MODE_ATTACHED: &str = "attached";

/// Claude spawn `mode` (D2, inside-out-multiplexer E1). Disambiguates the two
/// claude PTY lanes WITHIN an interactive `host_mode`: `stream_json` is the
/// Agent-SDK adoption lane (`claude -p --resume`, billed against the SDK pool);
/// `interactive` is the subscription-billed `ClaudeProvider` PTY lane (the
/// keystone). Absent reads as `stream_json` so every existing promote call site
/// keeps its current behavior; grid/relay request `interactive` explicitly. The
/// daemon routes on this field, never on a guess.
pub const CLAUDE_MODE_STREAM_JSON: &str = "stream_json";
/// See [`CLAUDE_MODE_STREAM_JSON`]: the interactive subscription-billed lane.
pub const CLAUDE_MODE_INTERACTIVE: &str = "interactive";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTransition {
    Succession,
    Branch,
    Deferred,
}

/// Classify a new full session id from one liveness truth result.
pub fn classify_session_transition(
    predecessor_session_id: &str,
    successor_session_id: &str,
    predecessor_reachable: Option<bool>,
) -> SessionTransition {
    if predecessor_session_id.is_empty()
        || successor_session_id.is_empty()
        || predecessor_session_id == successor_session_id
    {
        return SessionTransition::Deferred;
    }
    match predecessor_reachable {
        Some(false) => SessionTransition::Succession,
        Some(true) => SessionTransition::Branch,
        None => SessionTransition::Deferred,
    }
}

impl RegistryEntry {
    /// Move a dead predecessor row to its successor while retaining history.
    pub fn apply_succession(
        &mut self,
        predecessor_session_id: &str,
        successor_session_id: &str,
    ) -> bool {
        if self.harness_session_id.as_deref() != Some(predecessor_session_id)
            || successor_session_id.is_empty()
            || predecessor_session_id == successor_session_id
        {
            return false;
        }
        if !self
            .predecessor_session_ids
            .iter()
            .any(|id| id == predecessor_session_id)
        {
            self.predecessor_session_ids
                .push(predecessor_session_id.to_string());
        }
        self.harness_session_id = Some(successor_session_id.to_string());
        true
    }

    /// Clone this row as an independently addressable live branch.
    pub fn fork_for_session(
        &self,
        name: &str,
        successor_session_id: &str,
        predecessor_session_id: &str,
        fno_id: &str,
    ) -> Self {
        let mut branch = self.clone();
        branch.name = name.to_string();
        branch.fno_id = Some(fno_id.to_string());
        branch.harness_session_id = Some(successor_session_id.to_string());
        branch.predecessor_session_ids.clear();
        branch.forked_from_session_id = Some(predecessor_session_id.to_string());
        branch.crown_level = None;
        branch.crown_scope = None;
        branch.crown_grantor = None;
        branch.short_id.clear();
        branch.session_id = None;
        branch.claude_session_uuid = None;
        branch.codex_session_id = None;
        branch.gemini_session_id = None;
        branch.messaging_socket_path = None;
        branch.mcp_channel_id = None;
        branch.cc_session_id = None;
        branch.pid = None;
        branch.pid_start_time = None;
        branch.log_path = None;
        branch.last_message_at = None;
        branch.last_reconciled_at = None;
        branch.inside_leg = None;
        branch.screen_state = None;
        branch.exited_at = None;
        branch.mux = None;
        branch
    }

    fn migrate_provider_semantics(&mut self, schema_version: u32) {
        if schema_version < 15 {
            if let Some(provider) = self.provider.take() {
                self.legacy_provider = provider;
            }
        }
    }

    /// Two-way sync of `harness`/`harness_session_id` with the legacy
    /// per-provider identity fields (x-ec59), the Rust mirror of Python's
    /// `harness_identity.sync_harness_aliases` + the registry harness back-fill.
    /// Applied at load so a Rust reader of a legacy row and a Python reader of a
    /// Rust-minted canonical row both resolve. `harness` adopts `provider` when
    /// absent (provider is always set; harness is identity-only, never gates the
    /// read). Then canonical wins: a set `harness_session_id` syncs the matching
    /// legacy key (a conflicting legacy value is overwritten, never leaked);
    /// otherwise the first present legacy value back-fills `harness_session_id`.
    /// The claude legacy key is `claude_session_uuid` (the registry's identity),
    /// NOT the manifest's `claude_session_id`.
    pub fn backfill_harness_aliases(&mut self) {
        if self.harness.is_none() && !self.legacy_provider.is_empty() {
            self.harness = Some(self.legacy_provider.clone());
        }
        match self.harness_session_id.clone() {
            Some(hsid) if !hsid.is_empty() => match self.harness.as_deref() {
                Some("claude") => self.claude_session_uuid = Some(hsid),
                Some("codex") => self.codex_session_id = Some(hsid),
                Some("gemini") => self.gemini_session_id = Some(hsid),
                _ => {}
            },
            _ => {
                // Adopt from THIS harness's own legacy key when known, so a stale
                // legacy id of a DIFFERENT harness can't cross-contaminate; only a
                // genuinely unknown harness scans all keys (a pre-migration row
                // whose harness has not been resolved).
                let legacy = match self.harness.as_deref() {
                    Some("claude") => self.claude_session_uuid.clone(),
                    Some("codex") => self.codex_session_id.clone(),
                    Some("gemini") => self.gemini_session_id.clone(),
                    _ => self
                        .claude_session_uuid
                        .clone()
                        .or_else(|| self.codex_session_id.clone())
                        .or_else(|| self.gemini_session_id.clone()),
                };
                if let Some(value) = legacy {
                    if !value.is_empty() && value != "null" {
                        self.harness_session_id = Some(value);
                    }
                }
            }
        }
    }

    /// v9 transport-key backfill (x-1b1e), the Rust mirror of Python's
    /// `load_registry` popping the removed `claude_short_id` into `short_id`.
    /// Applied at load, before [`validate_single_live_ref`]: a legacy row's
    /// jobId (deserialized into `legacy_claude_short_id`) moves into an empty
    /// `short_id` and the transient is cleared so it never round-trips. A
    /// conflicting pair (both set, different values -- the drift this removal
    /// kills) KEEPS `short_id` and returns the legacy value so the caller can
    /// warn once; it never silently prefers the legacy value.
    pub fn backfill_short_id(&mut self) -> Option<String> {
        let legacy = self.legacy_claude_short_id.take()?;
        if legacy.is_empty() {
            return None;
        }
        if self.short_id.is_empty() {
            self.short_id = legacy;
            None
        } else if self.short_id != legacy {
            Some(legacy) // conflict: keep short_id, surface for a warn
        } else {
            None
        }
    }

    /// The provider transport key (v9, x-1b1e), or `None` when this row has
    /// none: the non-empty `short_id`. For claude it is the jobId (`claude
    /// attach/logs <jobId>`); for a daemon PTY row the worker-socket key. The
    /// single accessor consumers use to reach a session's wire handle, so no
    /// verb re-implements the empty-string guard. [x-1b1e transport extraction]
    pub fn transport_short(&self) -> Option<&str> {
        (!self.short_id.is_empty()).then_some(self.short_id.as_str())
    }

    /// The row's harness name as a required-string view (x-880e). The single
    /// accessor every RegistryEntry consumer uses instead of the raw identity
    /// field, so the provider->harness migration touches one place. `harness` is
    /// set on load by [`RegistryEntry::backfill_harness_aliases`]; during the
    /// migration window a not-yet-backfilled fresh row falls back to the legacy
    /// `provider`. Collapses to `harness`-only once `provider` is removed.
    pub fn harness_name(&self) -> &str {
        match self.harness.as_deref() {
            Some(h) if !h.is_empty() => h,
            // A not-yet-backfilled fresh row falls back to the load-only
            // legacy_provider (empty for a v10 row); backfill sets harness on load.
            _ => &self.legacy_provider,
        }
    }

    /// The hosting mode with the absent==exec rule applied in one place.
    /// `None` on disk (and the legacy rows that predate the field) read as
    /// [`HOST_MODE_EXEC`]; an explicit value passes through. Reconcile/liveness
    /// and the spawn path must use this, never the raw `Option`, so a missing
    /// key can never be mistaken for a non-exec mode. [interactive-drive node]
    pub fn host_mode_or_default(&self) -> &str {
        self.host_mode.as_deref().unwrap_or(HOST_MODE_EXEC)
    }

    /// True when this row is a long-lived interactive host (vs a one-shot exec
    /// session). The reconcile branch keys off this: an exec worker that exited
    /// is normal; an interactive worker is expected to stay live until `/quit`.
    pub fn is_interactive(&self) -> bool {
        self.host_mode_or_default() == HOST_MODE_INTERACTIVE
    }

    /// True when this row is a one-shot `ask` agent the daemon does NOT manage as
    /// a worker process: empty `short_id` (no worker-socket identity) AND no
    /// recorded `pid`. Such an agent has no process whose liveness could make it
    /// `live` -- its terminal status is `exited`, and its post-run value is
    /// *resumability* (a recorded provider session id), surfaced separately from
    /// status via the `session_id` projection. Only PTY agents (`spawn`/`host`/
    /// `promote`) carry a non-empty short_id + pid and can be `live`; this is the
    /// invariant documented on the `short_id` field ("a real daemon PTY agent
    /// always has a non-empty short_id"). Reconcile uses this to settle a
    /// finished ask to `exited` by process-liveness alone, never consulting
    /// session-file reachability for status. [plan ab-70faa65b, Locked Decision #1]
    pub fn is_one_shot_ask(&self) -> bool {
        // v9 (x-1b1e) moved the claude jobId from `claude_short_id` into
        // `short_id`, so a claude shellout (`ask`/`--bg`) row now carries a
        // non-empty short_id and the empty-short_id proxy no longer catches it.
        // Mirror recover()'s provider+host_mode guard: a non-interactive claude
        // row has no daemon PTY, so its surviving session file is a resumability
        // artifact, not "running" -- without this it would fall through to the
        // reachability probe and be kept falsely `live` forever.
        let is_claude_shellout = self.harness_name() == "claude" && !self.is_interactive();
        // A mux-hosted row (4a-G2) also has an empty short_id and may lack a
        // pid (the pane-child lookup is best-effort), but it is a LIVE hosted
        // agent, never a finished ask - without this exclusion the reconcile
        // sweep would flip it to Exited unprobed (codex P1, PR #142).
        (self.short_id.is_empty() || is_claude_shellout) && self.pid.is_none() && self.mux.is_none()
    }
}

/// Per-agent runtime state (`<short_id>/state.json`, schema v1). `state.status`
/// is canonical (LD10).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentState {
    pub schema_version: u32,
    pub short_id: String,
    pub status: AgentStatus,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub last_message_at: Option<String>,
    #[serde(default)]
    pub last_reply: Option<String>,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default)]
    pub last_restart_at: Option<String>,
    /// `None` for shellout (claude) agents; `Some` for PTY-managed agents.
    #[serde(default)]
    pub pty: Option<PtyState>,
}

impl AgentState {
    /// Construct a fresh PTY-managed agent state.
    pub fn new_pty(short_id: impl Into<String>) -> Self {
        AgentState {
            schema_version: STATE_SCHEMA_VERSION,
            short_id: short_id.into(),
            status: AgentStatus::Spawning,
            ready: false,
            last_message_at: None,
            last_reply: None,
            restart_count: 0,
            last_restart_at: None,
            pty: Some(PtyState::default()),
        }
    }
}

/// An open interactive drive window. Bundling the drive facts behind a single
/// `Option<DriveWindow>` makes the inconsistent `{drive_active: false,
/// drive_session_id: Some(..)}` state impossible: either there is a window
/// (`Some`) carrying all its fields, or there is none (`None`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DriveWindow {
    pub session_id: Option<String>,
    pub mode: Option<String>,
    /// Monotonic-clock baseline of the last drive heartbeat (count-during-sleep
    /// ns; see [`crate::MonotonicTimestamp`]).
    pub last_heartbeat_at_monotonic_ns: Option<u64>,
}

/// PTY sub-state. The on-disk shape stays flat (`active`, `drive_active`,
/// `drive_session_id`, `drive_mode`, `last_heartbeat_at_monotonic_ns`) via a
/// hand-written serde impl below, so cross-language schema parity (Wave 7) is a
/// direct field map; in memory the drive cluster is one `Option<DriveWindow>`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PtyState {
    pub active: bool,
    /// `Some` while an interactive drive window is open; `None` otherwise.
    pub drive: Option<DriveWindow>,
}

impl PtyState {
    /// Recovery step 4/5 ordering primitive (finding #12 Critical): atomically
    /// READ the active drive window (returning its session id + mode + last
    /// heartbeat) AND clear it. Callers MUST use the returned value to emit
    /// `drive_crashed` — the read happens here, before the clear, so the event
    /// reflects what the window was. Returns `None` if no drive was active.
    ///
    /// With the drive cluster behind one `Option`, read-then-clear is just
    /// `Option::take`: there is no window between the read and the clear for a
    /// second observer to see a half-cleared state.
    pub fn take_active_drive(&mut self) -> Option<DriveWindow> {
        self.drive.take()
    }
}

/// Flat on-disk projection of [`PtyState`], mediating between the typed
/// `Option<DriveWindow>` and the design's flat `state.json` schema. `drive_active`
/// is the discriminant; the option fields default to `None`/absent.
#[derive(Serialize, Deserialize)]
struct PtyStateWire {
    active: bool,
    #[serde(default)]
    drive_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_heartbeat_at_monotonic_ns: Option<u64>,
}

impl Serialize for PtyState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let wire = match &self.drive {
            Some(d) => PtyStateWire {
                active: self.active,
                drive_active: true,
                drive_session_id: d.session_id.clone(),
                drive_mode: d.mode.clone(),
                last_heartbeat_at_monotonic_ns: d.last_heartbeat_at_monotonic_ns,
            },
            None => PtyStateWire {
                active: self.active,
                drive_active: false,
                drive_session_id: None,
                drive_mode: None,
                last_heartbeat_at_monotonic_ns: None,
            },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PtyState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = PtyStateWire::deserialize(deserializer)?;
        // `drive_active` is canonical for window presence. A legacy/partial file
        // with the flag clear collapses any stray option fields to `None`, which
        // is exactly the inconsistent state the refactor makes unrepresentable.
        let drive = if wire.drive_active {
            Some(DriveWindow {
                session_id: wire.drive_session_id,
                mode: wire.drive_mode,
                last_heartbeat_at_monotonic_ns: wire.last_heartbeat_at_monotonic_ns,
            })
        } else {
            None
        };
        Ok(PtyState {
            active: wire.active,
            drive,
        })
    }
}

// ---------------------------------------------------------------------------
// Locked, atomic file access.
// ---------------------------------------------------------------------------

/// Load the registry under a shared lock. A missing file yields an empty
/// registry (0 agents is a valid steady state, not an error). The shared lock
/// is the daemon-down read path (`fno agents list` when the socket is down)
/// AND recovery step 1.
pub fn load_registry(path: &Path) -> Result<Registry, StateError> {
    let (registry, _raw_rows) = load_registry_with_counts(path)?;
    Ok(registry)
}

/// [`load_registry`] plus the raw on-disk row count the typed decode must be
/// reconciled against. The daemon's startup assertion (x-4c87 AC5) reads both:
/// a registry whose rows the typed reader dropped (today only a future-schema
/// partial read) must refuse to serve, never publish the dropped subset as the
/// complete roster.
pub fn load_registry_with_counts(path: &Path) -> Result<(Registry, usize), StateError> {
    // Lock the SAME sidecar `update_registry` locks (shared mode here), not the
    // data file. This is the canonical cross-language lock target: a Python
    // `fno` writer taking `flock` on `<registry>.lock` and the Rust daemon's
    // exclusive write-lock then live in one domain, so reader/writer and
    // cross-language writers actually mutually exclude (US6.12). Locking the
    // data file directly would (a) not exclude against the sidecar-based
    // writer and (b) reintroduce the rename-invalidates-fd footgun.
    // Acquire the lock FIRST, then decide existence: a `!path.exists()` check
    // before the lock could race a concurrent writer creating registry.json and
    // return a stale empty registry (Codex P2). The open-after-lock below is the
    // authoritative existence check.
    let lock = acquire_shared(&lock_path(path))?;
    let result = match OpenOptions::new().read(true).open(path) {
        Ok(file) => read_registry_tolerant(path, &file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = lock.unlock();
            return Ok((Registry::default(), 0));
        }
        Err(e) => {
            let _ = lock.unlock();
            return Err(e.into());
        }
    };
    let _ = lock.unlock();
    result
}

/// The raw on-disk row count the typed decode is reconciled against (x-4c87):
/// the canonical `agents` array, falling back to the legacy `entries` alias,
/// mirroring [`Registry`]'s serde rename/alias precedence. A missing or
/// non-array key counts as 0 (a valid empty registry, not a divergence).
fn registry_raw_row_count(probe: &serde_json::Value) -> usize {
    probe
        .get("agents")
        .or_else(|| probe.get("entries"))
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

/// The x-4c87 refusal text: name the registry path, both row counts, and the
/// comparison to run. Names no force/skip/ignore/bypass/no-verify remedy --
/// there is deliberately no override that defeats this check (x-d19e wording
/// rule, scoped to diagnostics this change introduces).
pub fn registry_row_divergence_msg(path: &Path, raw_rows: usize, decoded_rows: usize) -> String {
    format!(
        "{} contains raw_rows={} but Rust decoded_rows={}; inspect {} and compare \
         its agents array count with the Rust decoded count",
        path.display(),
        raw_rows,
        decoded_rows,
        path.display()
    )
}

/// Read a registry, tolerating ONLY a genuinely empty file (0 bytes / all
/// whitespace) as the empty registry. A present-but-unparseable file (malformed
/// JSON, schema mismatch, corruption) propagates an error instead of silently
/// defaulting: a default fed back through `update_registry`'s
/// read-modify-write would publish an empty registry and permanently wipe every
/// other agent (Gemini high, PR #364). `write_json_atomic` publishes via
/// tempfile + rename, so a reader never observes a torn write -- a parse failure
/// is therefore real corruption, not the transient partial read the prior
/// `unwrap_or_default()` was excusing.
///
/// Returns the typed registry plus the RAW array row count (x-4c87): a
/// positive raw count whose typed decode loses rows is an
/// `InvariantViolation` carrying both counts and the comparison to run, never a
/// successful empty roster. The one exception stays the forward-schema retry
/// below, where every dropped row is announced; the daemon's startup assertion
/// still refuses that partial read before serving.
fn read_registry_tolerant(path: &Path, mut file: &File) -> Result<(Registry, usize), StateError> {
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok((Registry::default(), 0));
    }
    let probe: serde_json::Value = serde_json::from_str(&buf)?;
    let raw_rows = registry_raw_row_count(&probe);
    let mut reg: Registry = match serde_json::from_str::<Registry>(&buf) {
        Ok(reg) => {
            // The same-schema success guard (x-4c87 AC3): serde cannot drop
            // elements from a `Vec` today, so this holds by construction -- but
            // the invariant is load-bearing enough (a changed reader mapping a
            // nonempty array to the default empty vec would publish a false
            // zero-agent roster) that it is asserted, not assumed.
            if reg.entries.len() != raw_rows {
                return Err(StateError::InvariantViolation(registry_row_divergence_msg(
                    path,
                    raw_rows,
                    reg.entries.len(),
                )));
            }
            reg
        }
        Err(typed_err) => {
            // A newer writer can widen a field's VALUES, not only add keys, and
            // `AgentStatus` has no catch-all variant -- so one row carrying a
            // status this binary has never heard of failed the WHOLE file at
            // serde and took the daemon's registry reads down with it. Tolerating
            // added keys alone left that door open.
            //
            // Retry per row, keeping the ones this binary can represent, but ONLY
            // when the store says it is newer than us. At or below our own schema
            // an unparseable row is a writer bug and stays fatal -- and since
            // x-4c87 it is fatal BY NAME when rows were on disk: the error
            // carries the raw and decoded counts so a lost roster can never read
            // as a valid empty one downstream.
            let on_disk = probe
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if on_disk <= REGISTRY_SCHEMA_VERSION as u64 {
                if raw_rows > 0 {
                    // Keep the count-bearing refusal (AC3) but carry the typed
                    // error too: a decode failure that lost no rows (a bad
                    // top-level type) still names its field instead of
                    // masquerading as a pure count divergence (PR 924 review).
                    let mut msg = registry_row_divergence_msg(path, raw_rows, 0);
                    msg.push_str(&format!("; typed error: {typed_err}"));
                    return Err(StateError::InvariantViolation(msg));
                }
                return Err(typed_err.into());
            }
            let rows = probe
                .get("agents")
                .or_else(|| probe.get("entries"))
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut entries = Vec::with_capacity(rows.len());
            let mut skipped: Vec<usize> = Vec::new();
            for (i, row) in rows.into_iter().enumerate() {
                match serde_json::from_value::<RegistryEntry>(row) {
                    Ok(entry) => entries.push(entry),
                    Err(_) => skipped.push(i),
                }
            }
            if !skipped.is_empty() {
                eprintln!(
                    "fno agents: registry: skipped row(s) {skipped:?} this fno cannot \
                     represent at schema_version={on_disk}. Those agents are invisible \
                     to this process until it is upgraded."
                );
            }
            // Forward-schema partial read: divergence is allowed HERE (every
            // dropped row is announced above), so this arm is exempt from the
            // count guard the Ok arm runs; the daemon's startup assertion is
            // what refuses to serve a partial roster as complete.
            Registry {
                // Saturate rather than `as u32`. A truncating cast can wrap an
                // absurd version DOWN to one at or below ours, and the write
                // guard keys on that number -- so the one store we must never
                // overwrite would be the one that looks safe to overwrite.
                schema_version: u32::try_from(on_disk).unwrap_or(u32::MAX),
                entries,
            }
        }
    };
    // Harness identity back-fill (x-ec59): canonical fields resolve from the
    // legacy per-provider fields on every load, so a legacy row read by Rust and
    // a canonical row written by Rust both round-trip. Applied here (the single
    // read choke point) covers both load_registry and update_registry's RMW read.
    for entry in &mut reg.entries {
        entry.migrate_provider_semantics(reg.schema_version);
        entry.backfill_harness_aliases();
        // v9 transport-key backfill (x-1b1e): move a legacy row's
        // `claude_short_id` into `short_id`. A conflicting pair keeps `short_id`
        // and warns once (never silently prefers the legacy value).
        if let Some(legacy) = entry.backfill_short_id() {
            eprintln!(
                "fno agents: warning: registry row {:?} carries short_id={:?} and legacy claude_short_id={:?}; keeping short_id",
                entry.name, entry.short_id, legacy
            );
        }
    }
    // Forward-compat guard on the TYPED daemon path (Codex P2, ab-a171ceb2):
    // the raw client path (client_verbs::load_registry_entries) already rejects
    // unsupported versions, but the daemon reads through here and previously
    // accepted any u32. Reject anything outside 1..=REGISTRY_SCHEMA_VERSION so a
    // pre-inside-leg daemon refuses a v5 store (instead of silently dropping the
    // inside-leg report) and the current daemon refuses a future v6 store.
    // READ FORWARD (see Python load_registry and client_verbs). The earlier
    // forward-compat guard refused a newer store so a stale reader could not
    // silently drop a field. The refusal turned out to be the worse failure:
    // registry.json is global to every agent here, so one process ahead of the
    // deployment took the whole fleet's registry reads down at once. Serde
    // ignores unknown fields on RegistryEntry, so a newer store reads as the
    // subset this binary understands.
    //
    // Dropping a field is now made safe by refusing to WRITE (update_registry
    // below) and by announcing every degraded read, rather than by refusing to
    // look. A version below 1 is damage, not a newer writer, and still fails.
    if reg.schema_version < 1 {
        return Err(StateError::UnsupportedSchemaVersion {
            found: reg.schema_version,
            max: REGISTRY_SCHEMA_VERSION,
        });
    }
    if reg.schema_version > REGISTRY_SCHEMA_VERSION {
        // Announce once per observed version, not once per read. The rule this
        // implements ("a degraded read must leave a trace") is right for a
        // one-shot CLI and wrong for the daemon, which reads this file on a
        // 5-second idle loop and on most request handlers: an operator sitting
        // in a mixed-version state for ten minutes would get 120 copies. A new
        // version still announces, so an upgrade or a further bump is never
        // swallowed by the latch.
        static LAST_ANNOUNCED: AtomicU32 = AtomicU32::new(0);
        if LAST_ANNOUNCED.swap(reg.schema_version, Ordering::Relaxed) != reg.schema_version {
            eprintln!(
                "fno agents: registry is schema_version={}, ahead of the \
                 schema_version={REGISTRY_SCHEMA_VERSION} this fno understands. \
                 Reading the fields it knows and ignoring the rest; writes are \
                 refused until this fno is upgraded. Rows may be incomplete.",
                reg.schema_version
            );
        }
    }
    Ok((reg, raw_rows))
}

/// The repo root when `exe` is a binary built inside a source checkout, else
/// `None`. Pure over the path so it is testable without a real install.
///
/// `home` stops the walk: a cargo-installed `~/.cargo/bin/fno-agents` reaches
/// `$HOME` in three steps, and plenty of people keep a dotfiles repo there, so
/// walking past it would read every deployed binary on such a machine as
/// source-run. The Python half needs no such stop, because a wheel module sits
/// six or more levels below `$HOME` and its bounded walk cannot reach it.
fn source_root_for_exe(exe: &Path, home: Option<&Path>) -> Option<PathBuf> {
    for parent in exe.ancestors().skip(1).take(6) {
        if home == Some(parent) {
            return None;
        }
        // A linked worktree's `.git` is a FILE, so this tests existence.
        if parent.join(".git").exists() {
            return Some(parent.to_path_buf());
        }
    }
    None
}

/// Refuse to RAISE the shared registry's schema from a source-built binary.
///
/// The Rust half of x-665d, and the exact mirror of Python's
/// `_refuse_source_ahead_schema_bump`. `registry.json` has writers in two
/// languages: a guard on one leaves the daemon, mux, and every client verb
/// still able to poison the file, which is the lesson x-d07d recorded when its
/// own read fix had to land on four readers rather than one.
///
/// Fires only when all three hold: the target IS the process-global registry
/// (`FNO_AGENTS_HOME`, else `$HOME/.fno/agents/registry.json`), this binary was
/// built inside a checkout and the target lies outside it, and the on-disk
/// version is strictly below this one. A missing file reads as
/// `Registry::default()`, whose version equals this one, so an absent registry
/// never fires.
///
/// Same known sharp edge as the Python half: this refuses ANY source-run raise
/// of the shared file, not only one that exceeds the deployment. Telling those
/// apart means reading the deployed binary's own constant, and the case
/// self-heals in seconds because every deployed process stamps this file on its
/// next write. The cost of guessing wrong the other way is a fleet-wide outage.
/// The whole decision, as a pure function of its inputs: `Some(root)` refuses
/// and names the checkout, `None` lets the write proceed.
///
/// Split out from the environment read below on purpose. The condition that
/// matters most here cannot be reproduced on every machine - it needs a
/// git-managed `$HOME`, which is a common dotfiles pattern and is not how this
/// developer's machine is set up. A test that can only observe the ambient
/// environment would be green here for the wrong reason, so the inputs are
/// arguments and the test constructs the case instead of hoping for it.
fn source_ahead_root(
    exe: &Path,
    home: Option<&Path>,
    resolved_target: &Path,
    shared: &Path,
    found: u32,
) -> Option<PathBuf> {
    if found >= REGISTRY_SCHEMA_VERSION {
        return None;
    }
    if resolved_target != shared {
        return None;
    }
    let root = source_root_for_exe(exe, home)?;
    if resolved_target.starts_with(&root) {
        return None;
    }
    Some(root)
}

fn refuse_source_ahead_schema_bump(path: &Path, found: u32) -> Result<(), StateError> {
    let shared = crate::paths::AgentsHome::from_env().registry_json();
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let shared = shared.canonicalize().unwrap_or(shared);
    let Ok(exe) = std::env::current_exe() else {
        return Ok(());
    };
    let exe = exe.canonicalize().unwrap_or(exe);
    // Canonicalize HOME the same way the exe is. Comparing a canonical path
    // against a raw `$HOME` never matches when the home path contains a symlink
    // or a trailing slash, and then the walk runs PAST home: a dotfiles repo at
    // `$HOME` would make a cargo-installed `~/.cargo/bin/fno-agents` read as
    // source-run, so a deployed binary would refuse every legitimate upgrade of
    // the shared registry. That is this guard inverted into the outage it exists
    // to prevent.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.canonicalize().unwrap_or(h));
    let Some(root) = source_ahead_root(&exe, home.as_deref(), &resolved, &shared, found) else {
        return Ok(());
    };
    Err(StateError::SourceAheadSchemaBump {
        path: path.display().to_string(),
        found,
        current: REGISTRY_SCHEMA_VERSION,
        source_root: root.display().to_string(),
    })
}

/// Read-modify-write the registry under an exclusive lock, publishing the
/// result atomically (tempfile + rename). The lock is held across the whole
/// read-modify-write so two daemons (or a daemon and a Python `fno`) never
/// interleave. The closure mutates the registry in place.
pub fn update_registry<F, T>(path: &Path, f: F) -> Result<T, StateError>
where
    F: FnOnce(&mut Registry) -> T,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Lock on a stable sidecar so the rename of the data file never invalidates
    // the lock fd (renaming the locked file out from under a held flock is the
    // classic footgun; locking the sidecar sidesteps it entirely).
    let lock = acquire_exclusive(&lock_path(path))?;
    let mut registry = read_existing_registry(path)?;
    // The half of read-forward that protects the file. The read above drops
    // fields this binary does not know, so writing those rows back would erase
    // them for every agent on the machine. Checked under the lock, against what
    // was actually read, so a writer that raced in between cannot slip past.
    if registry.schema_version > REGISTRY_SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchemaVersion {
            found: registry.schema_version,
            max: REGISTRY_SCHEMA_VERSION,
        });
    }
    // The other direction of the same comparison (x-665d). The check above stops
    // a stale writer erasing fields it cannot see; this one stops a SOURCE-run
    // writer creating those stale readers, by refusing the bump at line
    // `registry.schema_version = REGISTRY_SCHEMA_VERSION` below. Inside the lock
    // and before `write_json_atomic`, for the reason the comment above already
    // argues: a racing writer must not slip past.
    refuse_source_ahead_schema_bump(path, registry.schema_version)?;
    // The rows themselves, not just their signatures: a receipt for a removed
    // row must be built from the row the closure is about to drop, and the
    // closure leaves no other copy (x-a879). The vector is small.
    let before_entries = registry.entries.clone();
    let before = before_entries
        .iter()
        .map(|entry| (entry.name.clone(), identity_signature(entry)))
        .collect::<BTreeMap<_, _>>();
    let out = f(&mut registry);
    // Write-path harness sync (x-880e, AC6-FR): a closure that mutated a legacy
    // session-id field (the stream-json adopt path writes claude_session_uuid on a
    // uuid-less bg row) must land the value in harness_session_id before serde
    // drops the now-skip_serializing legacy key. backfill adopts legacy->canonical
    // when harness_session_id is unset -- and the only such mutation fires on rows
    // whose harness_session_id is None -- so no post-load mutation is lost.
    for entry in &mut registry.entries {
        entry.backfill_harness_aliases();
    }
    validate_changed_identities(&before, &registry.entries)
        .map_err(StateError::InvariantViolation)?;
    // One-live-ref invariant (4a-G2), enforced at the single Rust write choke
    // point so no closure can persist a double-ref row. The lock guard drops
    // on the early return, so a violation never wedges the registry.
    for entry in &registry.entries {
        if let Err(msg) = validate_single_live_ref(entry) {
            return Err(StateError::InvariantViolation(msg));
        }
    }
    // Resolvable-handle invariant (x-7bcd), scoped to new rows only via the
    // pre-write `before` snapshot already built above for identity checks --
    // a pre-existing violating row is never re-validated (AC3-FR).
    for entry in &registry.entries {
        if !before.contains_key(&entry.name) {
            if let Err(msg) = validate_resolvable_handle(entry) {
                return Err(StateError::InvariantViolation(msg));
            }
        }
    }
    // Upgrade-on-write (Codex P2, ab-a171ceb2): stamp the current schema version
    // so a Rust write of an older (e.g. v3) store bumps it to v4, matching
    // Python's write_registry (which always writes SCHEMA_VERSION). Without this,
    // adding host_mode to an existing v3 registry would leave schema_version:3 and
    // a pre-host_mode reader would still accept it - defeating the forward-compat
    // bump for every store that predates it (the common case).
    registry.schema_version = REGISTRY_SCHEMA_VERSION;
    write_json_atomic(path, &registry)?;
    // Removal accounting (x-a879) runs AFTER the write persisted: a removal
    // that failed to persist never happened, and announcing it would be a
    // false alarm. Within the accounting the receipt still precedes its own
    // event.
    account_for_removed_rows(path, &before_entries, &registry.entries);
    let _ = lock.unlock();
    Ok(out)
}

/// Rename a row's LABEL in one transaction, the Rust port of Python's
/// `rename_agent` (`cli/src/fno/agents/registry.py:2632`). Label-only: the
/// harness identity `(harness, harness_session_id, short_id)` is the lock, so a
/// rename never crosses into the worker's own harness - claude and codex keep
/// their native session names. The old label lands in `aliases` and keeps
/// resolving. `token` resolves through the same tiers the sibling verbs accept
/// (`find_name_or_full_session_id`: label, full session id + canonical handle,
/// related/predecessor ids) plus the transport short id and a prior label held
/// as an alias.
pub fn rename_agent(path: &Path, token: &str, new_name: &str) -> Result<(String, String), String> {
    if !is_valid_registry_label(new_name) {
        return Err(
            "registry name must be 1-64 letters, numbers, underscores, or hyphens".to_string(),
        );
    }
    // Resolve BEFORE the lock. The resolution reads the same file the
    // transaction re-reads under the lock, and the identity re-check inside the
    // closure is what makes a mid-flight change a typed refusal rather than a
    // rename of the wrong row (Python's "changed before rename"). The tiers
    // mirror Python's `resolve_agent_in` exactly: a FULL session id (any of
    // harness/related/predecessor, case-insensitive per the shared tier helper)
    // wins outright; otherwise name, alias, transport short id, canonical
    // handle (first-8) and legacy suffix (last-8) are unioned and the union
    // must be unique.
    use crate::identity::session_handle_tier;
    let snapshot = load_registry(path).map_err(|e| e.to_string())?;
    let session_tier = |e: &RegistryEntry| {
        [
            e.harness_session_id.as_deref(),
            e.related_session_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .chain(e.predecessor_session_ids.iter().map(String::as_str))
        .find_map(|session_id| session_handle_tier(token, session_id))
    };
    let label_tier = |e: &RegistryEntry| {
        e.name == token
            || (!e.short_id.is_empty() && e.short_id == token)
            || e.aliases.iter().any(|a| a == token)
            || session_tier(e).is_some()
    };
    let by_full: Vec<&RegistryEntry> = snapshot
        .entries
        .iter()
        .filter(|e| session_tier(e) == Some(0))
        .collect();
    let matches: Vec<&RegistryEntry> = if by_full.is_empty() {
        snapshot.entries.iter().filter(|e| label_tier(e)).collect()
    } else {
        by_full
    };
    let source = match matches.as_slice() {
        [one] => one,
        [] => return Err(format!("no such agent: {token}")),
        _ => return Err(format!("{token} is ambiguous - use its full session id")),
    };
    let identity = (
        source.harness.clone(),
        source.harness_session_id.clone(),
        source.short_id.clone(),
    );
    let old_name = source.name.clone();
    if old_name == new_name {
        return Ok((old_name, new_name.to_string()));
    }
    let resolved_name = old_name.clone();
    // The closure's Result IS the transaction verdict: update_registry hands it
    // back as the Ok payload, so an inner Err must propagate - dropping it would
    // report a refused rename as a success.
    match update_registry(path, |registry| {
        let idx = registry
            .entries
            .iter()
            .position(|e| {
                (
                    e.harness.clone(),
                    e.harness_session_id.clone(),
                    e.short_id.clone(),
                ) == identity
                    && e.name == resolved_name
            })
            .ok_or_else(|| {
                format!(
                    "agent {resolved_name:?} changed before rename; retry with its full session id"
                )
            })?;
        // "Names another worker" includes a label the worker still ANSWERS to:
        // a prior label held as another row's alias refuses too, or the renamed
        // label would resolve ambiguous (two rows) the moment anyone used it.
        if registry.entries.iter().enumerate().any(|(i, e)| {
            i != idx && (e.name == new_name || e.aliases.iter().any(|a| a == new_name))
        }) {
            return Err(format!(
                "registry label {new_name:?} already names another worker"
            ));
        }
        let target = &mut registry.entries[idx];
        if !target.aliases.iter().any(|a| a == &resolved_name) {
            target.aliases.push(resolved_name.clone());
        }
        target.name = new_name.to_string();
        Ok(())
    }) {
        Ok(inner) => inner?,
        Err(e) => return Err(e.to_string()),
    }
    Ok((old_name, new_name.to_string()))
}

/// The label grammar `rename_agent` enforces (1..=64 chars from
/// `[A-Za-z0-9_-]`). The ONE grammar predicate in this crate: the daemon's
/// `valid_agent_name` delegates here, so the spawn-time name rule and the
/// rename-time rule cannot drift. (fno's proto.rs carries its own copy for the
/// pre-subprocess notice; the crates do not link, only shell.)
pub fn is_valid_registry_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The `agent.rename` RPC handler, beside the transaction it serves (the
/// daemon module is shrink-only). Grammar is refused BEFORE any lock: a
/// hostile token must never reach a write. `rename_agent` owns resolution,
/// the identity lock, the duplicate refusal and the alias append; the harness
/// session is untouched by construction.
pub(crate) fn rename_response(
    registry_path: &Path,
    req: &crate::protocol::Request,
) -> crate::protocol::Response {
    use crate::protocol::{ErrorCode, Response};
    let token = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "rename needs a <name> (current label, short id, or full session id)",
            )
        }
    };
    let Some(new_name) = req.params.get("new_name").and_then(|v| v.as_str()) else {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "rename needs --name <new-label>",
        );
    };
    if !is_valid_registry_label(new_name) {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "registry name must be 1-64 letters, numbers, underscores, or hyphens",
        );
    }
    match rename_agent(registry_path, token, new_name) {
        Ok((old, new)) => Response::ok(
            req.id,
            serde_json::json!({"renamed": true, "old_name": old, "new_name": new}),
        ),
        Err(msg) => Response::err(req.id, ErrorCode::Internal, msg),
    }
}

/// Removal accounting at the write choke point (x-a879): every row the
/// closure dropped gets a recovery receipt staged first and a
/// `registry_row_removed` event naming the row, the remover and the reason,
/// whatever door dropped it. Both the home and the event stream derive from
/// the registry path - `registry.json` sits at `<agents home>/registry.json`,
/// and the event rides the SAME agent-lifecycle log the daemon writes
/// `agent_row_reaped` to - so no caller threads a home or emitter argument.
///
/// A row counts as removed only when NO surviving row shares any of its
/// identity tokens (session id, short id, name): a rename or a session-id
/// backfill mutates one token while the row itself stays.
fn account_for_removed_rows(path: &Path, before: &[RegistryEntry], after: &[RegistryEntry]) {
    let after_sids: std::collections::BTreeSet<&str> = after
        .iter()
        .filter_map(|e| e.harness_session_id.as_deref().filter(|s| !s.is_empty()))
        .collect();
    let after_short_ids: std::collections::BTreeSet<&str> = after
        .iter()
        .map(|e| e.short_id.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    // A surviving row's PRIOR labels count as its presence: rename_agent moves
    // the old name into `aliases`, so an id-less renamed row keeps matching
    // here through its alias. Without this the row stages a false
    // registry_row_removed for a row that is alive.
    let after_names: std::collections::BTreeSet<&str> = after
        .iter()
        .flat_map(|e| {
            e.aliases
                .iter()
                .map(String::as_str)
                .chain([e.name.as_str()])
        })
        .collect();
    let removed: Vec<&RegistryEntry> = before
        .iter()
        .filter(|e| {
            let sid_matched = e
                .harness_session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_some_and(|s| after_sids.contains(s));
            let short_matched =
                !e.short_id.is_empty() && after_short_ids.contains(e.short_id.as_str());
            !(sid_matched || short_matched || after_names.contains(e.name.as_str()))
        })
        .collect();
    if removed.is_empty() {
        return;
    }
    let Some(home_dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    let home = crate::paths::AgentsHome::at(home_dir);
    let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
    let remover = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_string());
    for entry in &removed {
        crate::receipt::stage_removal_accounting(&home, entry, &remover, &emitter);
    }
    // One grouped `registry_rows_lost` beside the per-row events (x-f0d2): the
    // per-row events carry receipts, this one names the writer, pid and verb,
    // so a save that drops rows can no longer vanish without a door being
    // named. Emitted after the per-row events, so a reader taking the first
    // line still sees a row receipt.
    let lost: Vec<serde_json::Value> = removed
        .iter()
        .map(|e| {
            serde_json::json!({
                "harness_session_id": e.harness_session_id.clone().unwrap_or_default(),
                "name": e.name.clone(),
            })
        })
        .collect();
    let _ = emitter.emit(
        "registry_rows_lost",
        &serde_json::json!({
            "writer": "rust",
            "pid": std::process::id(),
            "verb": invocation_verb(),
            "lost": lost,
        }),
    );
}

/// The command line that named this write, bounded: argv0's basename plus up
/// to five following arguments, 200 chars. The binary name alone cannot tell
/// `agents reap --apply` from `board --json`, and which door dropped rows is
/// exactly the question the grouped loss event exists to answer.
fn invocation_verb() -> String {
    let mut parts: Vec<String> = std::env::args_os()
        .take(6)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Some(first) = parts.first_mut() {
        if let Some(basename) = Path::new(&*first).file_name() {
            *first = basename.to_string_lossy().into_owned();
        }
    }
    let joined = parts.join(" ");
    joined.chars().take(200).collect()
}

type IdentitySignature = (String, String, String, String);

fn identity_signature(entry: &RegistryEntry) -> IdentitySignature {
    (
        entry.name.clone(),
        entry.short_id.clone(),
        entry.harness_name().to_string(),
        entry.harness_session_id.clone().unwrap_or_default(),
    )
}

fn validate_changed_identities(
    before: &BTreeMap<String, IdentitySignature>,
    entries: &[RegistryEntry],
) -> Result<(), String> {
    use crate::identity::{canonical_handle, legacy_suffix_handle, session_handle_tier};

    // Tokens come in two kinds, mirroring registry.py. Chosen tokens (the row
    // name, an explicit transport short id) may not shadow another row's name,
    // short id, or any session address tier. Minted tokens (the session id and
    // its canonical handle) shadow only the SAME session: a tier-0 full-id
    // match. A first-eight overlap between two DIFFERENT sessions is the codex
    // same-window shape, not a collision - resolution fails closed on the
    // shared short asking for the full id.
    let matches = |token: &str, other: &RegistryEntry, same_session_only: bool| {
        if !same_session_only
            && (token == other.name || (!other.short_id.is_empty() && token == other.short_id))
        {
            return true;
        }
        let Some(session_id) = other.harness_session_id.as_deref() else {
            return false;
        };
        match session_handle_tier(token, session_id) {
            Some(0) => true,
            Some(_) => !same_session_only,
            None => false,
        }
    };

    for (index, candidate) in entries.iter().enumerate() {
        if before.get(&candidate.name) == Some(&identity_signature(candidate)) {
            continue;
        }
        let mut chosen = BTreeSet::from([candidate.name.clone()]);
        if !candidate.short_id.is_empty() {
            chosen.insert(candidate.short_id.clone());
        }
        let session_id = candidate.harness_session_id.as_deref().unwrap_or("");
        let mut minted = BTreeSet::new();
        if !session_id.is_empty() {
            minted.insert(session_id.to_string());
            minted.insert(canonical_handle(session_id));
        }
        let legacy = (!session_id.is_empty()).then(|| legacy_suffix_handle(session_id));
        for (other_index, other) in entries.iter().enumerate() {
            if index == other_index {
                continue;
            }
            let collision = chosen
                .iter()
                .find(|token| matches(token, other, false))
                .cloned()
                .or_else(|| {
                    minted
                        .iter()
                        .find(|token| matches(token, other, true))
                        .cloned()
                })
                .or_else(|| {
                    legacy
                        .as_ref()
                        .filter(|token| matches(token, other, true))
                        .cloned()
                });
            if let Some(token) = collision {
                return Err(format!(
                    "registry identity {token:?} for new or changed row {:?} collides with row {:?}; use a different name or the full session id",
                    candidate.name, other.name
                ));
            }
        }
    }
    Ok(())
}

fn read_existing_registry(path: &Path) -> Result<Registry, StateError> {
    match OpenOptions::new().read(true).open(path) {
        Ok(file) => Ok(read_registry_tolerant(path, &file)?.0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e.into()),
    }
}

/// Load a per-agent `state.json`. `Ok(None)` when the file is absent (recovery
/// distinguishes "registry entry without state.json" from a present-but-partial
/// state).
pub fn load_state(path: &Path) -> Result<Option<AgentState>, StateError> {
    // Lock the SAME `.lock` sidecar `write_state_atomic` locks (shared mode),
    // not the data file: readers and writers must synchronize on one inode or
    // a read can race a concurrent write/rename (Codex P1). Acquire the lock
    // BEFORE deciding existence so a writer creating the file mid-call cannot
    // be missed.
    let lock = acquire_shared(&lock_path(path))?;
    let r = match OpenOptions::new().read(true).open(path) {
        Ok(file) => read_json::<AgentState>(&file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = lock.unlock();
            return Ok(None);
        }
        Err(e) => {
            let _ = lock.unlock();
            return Err(e.into());
        }
    };
    let _ = lock.unlock();
    match r {
        Ok(s) => Ok(Some(s)),
        // Present but empty/partial: treat as absent state so recovery marks
        // the agent inconsistent rather than crashing.
        Err(_) => Ok(None),
    }
}

/// Atomically write a per-agent `state.json` (tempfile + rename) under an
/// exclusive lock on its sidecar.
pub fn write_state_atomic(path: &Path, state: &AgentState) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = acquire_exclusive(&lock_path(path))?;
    write_json_atomic(path, state)?;
    let _ = lock.unlock();
    Ok(())
}

/// Read-modify-write a per-agent `state.json` while holding the exclusive
/// sidecar lock across the WHOLE operation, so concurrent writers cannot
/// interleave between the read and the write (the lost-update footgun a
/// `load_state` + `write_state_atomic` pair has).
///
/// Returns `Ok(false)` without calling `f` when the file is absent or partial:
/// drive window mutations must never fabricate a `state.json` on the worker's
/// behalf (recovery distinguishes "registry entry without state.json"). The
/// drive admit / cleanup paths route their window writes through here so a
/// stale-driver takeover cannot drop the authority window via a read that
/// predates the new driver's write.
pub fn update_state_atomic<F>(path: &Path, f: F) -> Result<bool, StateError>
where
    F: FnOnce(&mut AgentState),
{
    let lock = acquire_exclusive(&lock_path(path))?;
    let existing = match OpenOptions::new().read(true).open(path) {
        Ok(file) => read_json::<AgentState>(&file).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            let _ = lock.unlock();
            return Err(e.into());
        }
    };
    let result = match existing {
        Some(mut st) => {
            f(&mut st);
            write_json_atomic(path, &st)?;
            true
        }
        None => false,
    };
    let _ = lock.unlock();
    Ok(result)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// Open (creating if needed) the lock sidecar and take an exclusive advisory
/// lock, blocking until acquired. The returned `File` holds the lock until it
/// is unlocked or dropped.
fn acquire_exclusive(lock_file: &Path) -> Result<File, StateError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_file)?;
    file.lock()?;
    Ok(file)
}

/// Try to take the exclusive sidecar lock without interrupting startup
/// recovery. A writer holding this lock owns the temp file and recovery must
/// leave it alone for the next pass.
pub(crate) fn try_lock_path_exclusive(path: &Path) -> Result<Option<File>, StateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path(path))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Open (creating if needed) the lock sidecar and take a shared advisory lock,
/// blocking until acquired. Multiple readers share; an exclusive writer
/// excludes them. Same sidecar target as [`acquire_exclusive`].
fn acquire_shared(lock_file: &Path) -> Result<File, StateError> {
    if let Some(parent) = lock_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_file)?;
    file.lock_shared()?;
    Ok(file)
}

fn read_json<T: for<'de> Deserialize<'de>>(mut file: &File) -> Result<T, StateError> {
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StateError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("state"),
        std::process::id()
    ));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        let bytes = serde_json::to_vec_pretty(value)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that mutate process-wide env vars hold this so parallel test
    /// threads never interleave mid-arm snapshots of the same keys.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn enters_fires_once_per_episode() {
        use InsideLegState::{Blocked, Done, Working};
        // Walk working -> blocked -> blocked -> blocked -> working -> blocked.
        // `enters(.., Blocked)` must be true ONLY on the two edges into blocked
        // (positions 2 and 6), not on the repeats within an episode.
        let seq = [Working, Blocked, Blocked, Blocked, Working, Blocked];
        let fired: Vec<bool> = seq
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let prev = if i == 0 { None } else { Some(seq[i - 1]) };
                enters(prev, s, Blocked)
            })
            .collect();
        assert_eq!(fired, [false, true, false, false, false, true]);
        // A first-ever report of blocked (prev None) counts as entering.
        assert!(enters(None, Blocked, Blocked));
        // Done is its own episode axis, independent of blocked.
        assert!(enters(Some(Working), Done, Done));
        assert!(!enters(Some(Done), Done, Done));
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-state-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_entry(name: &str) -> RegistryEntry {
        RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            name: name.into(),
            short_id: format!("{name}-id"),
            legacy_provider: String::new(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: Some("codex".into()),
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp/x".into(),
            project_root: "/tmp/x".into(),
            session_id: Some("uuid-1".into()),
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: Some("uuid-1".into()),
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-05-24T00:00:00Z".into(),
            pid: Some(1234),
            pid_start_time: None,
            keeper_child_pid: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            sandbox_posture: None,
            origin: None,
            spawn_trigger: None,
            legacy_claude_short_id: None,
            ..Default::default()
        }
    }

    #[test]
    fn state_substrate_roundtrips_and_absence_reads_none() {
        // v23: the lane stamp survives the FILE round-trip through the same
        // load/save pair the daemon uses, and a row without one reads None -
        // absence means unknown, never "pane".
        let dir = tmpdir("substrate");
        let path = dir.join("registry.json");
        let mut stamped = sample_entry("thread-worker");
        stamped.substrate = Some("thread".into());
        let mut old_row = sample_entry("old-row");
        // Distinct session identity: the write path backfills
        // harness_session_id from codex_session_id, and two rows sharing
        // "uuid-1" refuse the store on an identity collision.
        old_row.codex_session_id = Some("uuid-2".into());
        update_registry(&path, |r| {
            r.entries.push(stamped);
            r.entries.push(old_row);
        })
        .unwrap();
        let reg = load_registry(&path).unwrap();
        let thread_row = reg
            .entries
            .iter()
            .find(|e| e.name == "thread-worker")
            .unwrap();
        assert_eq!(thread_row.substrate.as_deref(), Some("thread"));
        let old_row = reg.entries.iter().find(|e| e.name == "old-row").unwrap();
        assert_eq!(old_row.substrate, None);

        // skip-when-None keeps an unstamped row's key off disk entirely.
        let raw = std::fs::read_to_string(&path).unwrap();
        let old_obj = raw.split("old-row").nth(1).unwrap();
        assert!(!old_obj.contains("substrate"));

        // Python-authored shape (the key present) parses into the same value.
        let python_row = r#"{"name":"m","provider":"claude","cwd":"/p","log_path":null,
            "created_at":"2026-07-02T00:00:00Z","status":"live",
            "substrate":"headless"}"#;
        let row: RegistryEntry = serde_json::from_str(python_row).unwrap();
        assert_eq!(row.substrate.as_deref(), Some("headless"));
    }

    #[test]
    fn state_mux_ref_roundtrips_and_python_dict_shape_parses() {
        // 4a-G2: the mux ref survives the typed round-trip, and the exact
        // JSON shape Python's AgentEntry writes ({"session": ..., "pane_id":
        // ...} under "mux") parses back into MuxRef (X3 mixed-language rule).
        let mut e = sample_entry("mux-agent");
        e.short_id = String::new(); // one live ref: mux only
        e.mux = Some(MuxRef {
            session: "work".into(),
            pane_id: 7,
        });
        let json = serde_json::to_string(&e).unwrap();
        let back: RegistryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mux.as_ref().unwrap().session, "work");
        assert_eq!(back.mux.as_ref().unwrap().pane_id, 7);

        // Python-authored shape (dict passthrough) parses identically.
        let python_row = r#"{"name":"m","provider":"claude","cwd":"/p","log_path":null,
            "claude_short_id":null,"codex_session_id":null,"gemini_session_id":null,
            "created_at":"2026-07-02T00:00:00Z","status":"live","last_message_at":null,
            "mcp_channel_id":null,"mux":{"session":"main","pane_id":3}}"#;
        let row: RegistryEntry = serde_json::from_str(python_row).unwrap();
        assert_eq!(row.mux.as_ref().unwrap().pane_id, 3);
        // A pre-mux row (absent key) reads as None.
        assert_eq!(sample_entry("plain").mux, None);
    }

    #[test]
    fn harness_backfill_legacy_row_gains_canonical() {
        // x-ec59 / AC1-EDGE: a pre-migration Python row (provider + the legacy
        // per-provider uuid, no harness) gains the canonical pair on load.
        let python_legacy = r#"{"name":"w","provider":"claude","cwd":"/p","log_path":null,
            "claude_short_id":"7c5dcf5d","claude_session_uuid":"UUID-1","codex_session_id":null,
            "gemini_session_id":null,"created_at":"2026-07-13T00:00:00Z","status":"live",
            "last_message_at":null,"mcp_channel_id":null}"#;
        let mut e: RegistryEntry = serde_json::from_str(python_legacy).unwrap();
        e.migrate_provider_semantics(14);
        e.backfill_harness_aliases();
        assert_eq!(e.harness.as_deref(), Some("claude"));
        assert_eq!(e.harness_session_id.as_deref(), Some("UUID-1"));
    }

    #[test]
    fn backfill_short_id_moves_legacy_into_empty_short() {
        // AC2-EDGE (Rust side): a legacy row's claude_short_id moves into short_id.
        let legacy = r#"{"name":"w","provider":"claude","cwd":"/p","log_path":null,
            "claude_short_id":"7c5dcf5d","created_at":"2026-07-13T00:00:00Z","status":"live"}"#;
        let mut e: RegistryEntry = serde_json::from_str(legacy).unwrap();
        assert_eq!(e.backfill_short_id(), None);
        assert_eq!(e.short_id, "7c5dcf5d");
        assert_eq!(e.legacy_claude_short_id, None); // consumed
    }

    #[test]
    fn backfill_short_id_conflict_keeps_short_and_reports_legacy() {
        // AC3-EDGE (Rust side): both set, different -> short_id wins, legacy surfaced.
        let conflict = r#"{"name":"w","provider":"claude","cwd":"/p","log_path":null,
            "short_id":"aaaaaaaa","claude_short_id":"bbbbbbbb",
            "created_at":"2026-07-13T00:00:00Z","status":"live"}"#;
        let mut e: RegistryEntry = serde_json::from_str(conflict).unwrap();
        assert_eq!(e.backfill_short_id().as_deref(), Some("bbbbbbbb"));
        assert_eq!(e.short_id, "aaaaaaaa"); // short_id wins
    }

    #[test]
    fn harness_backfill_canonical_only_row_syncs_legacy() {
        // A canonical-only row (post-migration mint): the legacy alias is synced
        // so an old reader still resolves the session.
        let mut e = sample_entry("w");
        e.legacy_provider = "claude".into();
        e.codex_session_id = None;
        e.session_id = None;
        e.claude_session_uuid = None;
        e.harness = Some("claude".into());
        e.harness_session_id = Some("CANON".into());
        e.backfill_harness_aliases();
        assert_eq!(e.claude_session_uuid.as_deref(), Some("CANON"));
    }

    #[test]
    fn harness_backfill_conflict_is_canonical_wins() {
        // AC2-EDGE (Rust side): a conflicting legacy value is overwritten.
        let mut e = sample_entry("w");
        e.legacy_provider = "claude".into();
        e.harness = Some("claude".into());
        e.harness_session_id = Some("CANON".into());
        e.claude_session_uuid = Some("STALE".into());
        e.backfill_harness_aliases();
        assert_eq!(e.harness_session_id.as_deref(), Some("CANON"));
        assert_eq!(e.claude_session_uuid.as_deref(), Some("CANON"));
    }

    #[test]
    fn harness_backfill_does_not_cross_contaminate() {
        // A claude row carrying a stale codex id must NOT adopt it: only the
        // row's own harness key is consulted when harness is known.
        let mut e = sample_entry("w");
        e.legacy_provider = "claude".into();
        e.harness = Some("claude".into());
        e.harness_session_id = None;
        e.claude_session_uuid = None;
        e.codex_session_id = Some("STALE-CODEX".into());
        e.session_id = None;
        e.backfill_harness_aliases();
        assert_eq!(e.harness_session_id, None);
    }

    #[test]
    fn harness_backfill_reads_python_canonical_row_via_registry() {
        // Cross-language: a Python-authored canonical codex row parses into
        // Registry and, after the load-time backfill (mirrors
        // read_registry_tolerant), resolves the legacy alias too.
        let python_json = r#"{"schema_version":7,"agents":[{"name":"w","provider":"codex",
            "cwd":"/p","log_path":null,"claude_short_id":null,"codex_session_id":null,
            "gemini_session_id":null,"created_at":"2026-07-13T00:00:00Z","status":"live",
            "last_message_at":null,"mcp_channel_id":null,"harness":"codex",
            "harness_session_id":"THREAD"}]}"#;
        let mut reg: Registry = serde_json::from_str(python_json).unwrap();
        for e in &mut reg.entries {
            e.backfill_harness_aliases();
        }
        assert_eq!(reg.entries[0].harness_session_id.as_deref(), Some("THREAD"));
        assert_eq!(reg.entries[0].codex_session_id.as_deref(), Some("THREAD"));
    }

    #[test]
    fn state_mux_row_skips_key_when_absent() {
        // Slim rows: no "mux" key serialized for non-mux rows, so a
        // round-tripped worker row stays byte-familiar to older tooling.
        let v = serde_json::to_value(sample_entry("w")).unwrap();
        assert!(v.get("mux").is_none());
    }

    #[test]
    fn state_mux_row_is_never_a_one_shot_ask() {
        // codex P1 (PR #142): empty short_id + no pid describes a mux row too;
        // reconcile must not settle a live hosted agent as a finished ask.
        let mut e = sample_entry("mux-live");
        e.short_id = String::new();
        e.pid = None;
        assert!(e.is_one_shot_ask(), "baseline: bare row reads as ask");
        e.mux = Some(MuxRef {
            session: "main".into(),
            pane_id: 4,
        });
        assert!(!e.is_one_shot_ask(), "a mux ref is a live hosting handle");
    }

    #[test]
    fn state_v9_claude_shellout_row_is_a_one_shot_ask() {
        // x-1b1e regression: v9 moved the claude jobId into short_id, so a
        // finished claude `ask`/`--bg` row now carries a NON-empty short_id.
        // The empty-short_id proxy no longer catches it; without the provider+
        // host_mode guard reconcile would fall through to the reachability probe
        // and keep the row falsely `live` off its surviving (resumability-only)
        // session file -- the exact defect recover() already had to fix.
        let mut ask = sample_entry("cc-ask");
        ask.legacy_provider = "claude".into();
        ask.harness = None;
        ask.backfill_harness_aliases();
        ask.short_id = "7c5dcf5d".into(); // v9: jobId lives here now
        ask.host_mode = None; // exec (shellout), not interactive
        ask.pid = None;
        ask.mux = None;
        assert!(
            ask.is_one_shot_ask(),
            "a v9 claude shellout row (non-empty short_id, exec, no pid) is a one-shot ask"
        );

        // An interactive claude stream worker DOES have a daemon PTY: probe it,
        // never settle it by liveness-alone.
        let mut worker = ask.clone();
        worker.host_mode = Some(HOST_MODE_INTERACTIVE.into());
        assert!(
            !worker.is_one_shot_ask(),
            "an interactive claude worker is PTY-managed, not a one-shot ask"
        );

        // An adopted row carries an external pid -> excluded by the pid guard.
        let mut adopted = ask.clone();
        adopted.host_mode = Some(HOST_MODE_ATTACHED.into());
        adopted.pid = Some(4242);
        assert!(
            !adopted.is_one_shot_ask(),
            "an adopted row (external pid) is not a one-shot ask"
        );
    }

    #[test]
    fn state_update_registry_enforces_one_live_ref() {
        // Write-time invariant (brief Locked 7): a mux ref alongside a worker
        // short_id (or a bg claude_short_id) is refused; the store is left
        // untouched and the lock released (a later clean write succeeds).
        let dir = tmpdir("one-ref");
        let path = dir.join("registry.json");
        let res = update_registry(&path, |r| {
            let mut e = sample_entry("double"); // sample has short_id set
            e.mux = Some(MuxRef {
                session: "main".into(),
                pane_id: 1,
            });
            r.entries.push(e);
        });
        assert!(
            matches!(res, Err(StateError::InvariantViolation(_))),
            "double-ref row must be refused: {res:?}"
        );
        assert!(
            load_registry(&path).unwrap().entries.is_empty(),
            "refused write must not persist"
        );
        // bg-thread ref (jobId in short_id, v9) + mux is refused the same way.
        let res = update_registry(&path, |r| {
            let mut e = sample_entry("bg-double");
            e.short_id = "abcd1234".into();
            e.mux = Some(MuxRef {
                session: "main".into(),
                pane_id: 2,
            });
            r.entries.push(e);
        });
        assert!(matches!(res, Err(StateError::InvariantViolation(_))));
        // A clean mux-only row persists (lock was released by the refusals).
        update_registry(&path, |r| {
            let mut e = sample_entry("clean");
            e.short_id = String::new();
            e.mux = Some(MuxRef {
                session: "main".into(),
                pane_id: 3,
            });
            r.entries.push(e);
        })
        .unwrap();
        let reg = load_registry(&path).unwrap();
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(reg.schema_version, REGISTRY_SCHEMA_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_update_registry_allows_first_eight_overlap_between_sessions() {
        let dir = tmpdir("identity-collision");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            let mut first = sample_entry("first");
            first.short_id = "transport1".into();
            first.harness = Some("codex".into());
            first.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
            registry.entries.push(first);
        })
        .unwrap();

        // A first-eight overlap between two DIFFERENT sessions is the codex
        // same-window shape (UUIDv7 ids share their top bits inside one
        // time window), not a write-blocking collision: resolution fails
        // closed on the shared short and the full id still resolves.
        update_registry(&path, |registry| {
            let mut second = sample_entry("second");
            second.short_id = "transport2".into();
            second.harness = Some("codex".into());
            // Same first-eight (canonical address) as `first`, different
            // full id -> allowed.
            second.harness_session_id = Some("aaaaaaaa-0000-0000-0000-222222222222".into());
            registry.entries.push(second);
        })
        .unwrap();

        assert_eq!(load_registry(&path).unwrap().entries.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_update_registry_refuses_second_row_claiming_same_full_session_id() {
        let dir = tmpdir("identity-duplicate-session");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            let mut first = sample_entry("first");
            first.harness = Some("codex".into());
            first.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
            registry.entries.push(first);
        })
        .unwrap();

        let result = update_registry(&path, |registry| {
            let mut second = sample_entry("second");
            second.harness = Some("codex".into());
            // SAME full session id under a different row is a tier-0
            // collision: one session, two rows.
            second.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
            registry.entries.push(second);
        });

        assert!(matches!(result, Err(StateError::InvariantViolation(_))));
        assert_eq!(load_registry(&path).unwrap().entries.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_transition_classifies_dead_live_and_unknown_predecessors() {
        assert_eq!(
            classify_session_transition("session-a", "session-b", Some(false)),
            SessionTransition::Succession
        );
        assert_eq!(
            classify_session_transition("session-a", "session-b", Some(true)),
            SessionTransition::Branch
        );
        assert_eq!(
            classify_session_transition("session-a", "session-b", None),
            SessionTransition::Deferred
        );
    }

    #[test]
    fn succession_preserves_thread_id_and_branch_keeps_two_rows() {
        let mut predecessor = sample_entry("worker");
        predecessor.fno_id = Some("thread-a".into());
        predecessor.harness_session_id = Some("session-a".into());
        predecessor.short_id = "transport-a".into();
        predecessor.session_id = Some("session-a".into());
        predecessor.codex_session_id = Some("session-a".into());
        predecessor.log_path = Some("/tmp/session-a.log".into());
        predecessor.crown_level = Some(2);
        predecessor.crown_scope = Some("scope-a".into());
        predecessor.crown_grantor = Some("human".into());
        predecessor.mux = Some(MuxRef {
            session: "main".into(),
            pane_id: 4,
        });

        assert!(predecessor.apply_succession("session-a", "session-b"));
        assert_eq!(predecessor.fno_id.as_deref(), Some("thread-a"));
        assert_eq!(predecessor.harness_session_id.as_deref(), Some("session-b"));
        assert_eq!(predecessor.predecessor_session_ids, vec!["session-a"]);
        assert!(!predecessor.apply_succession("session-a", "session-c"));

        let branch =
            predecessor.fork_for_session("worker-branch", "session-c", "session-b", "thread-c");
        assert_eq!(predecessor.harness_session_id.as_deref(), Some("session-b"));
        assert_eq!(branch.harness_session_id.as_deref(), Some("session-c"));
        assert_eq!(branch.forked_from_session_id.as_deref(), Some("session-b"));
        assert_eq!(branch.fno_id.as_deref(), Some("thread-c"));
        assert_ne!(predecessor.fno_id, branch.fno_id);
        assert!(branch.crown_level.is_none());
        assert!(branch.crown_scope.is_none());
        assert!(branch.crown_grantor.is_none());
        assert!(branch.short_id.is_empty());
        assert!(branch.session_id.is_none());
        assert!(branch.codex_session_id.is_none());
        assert!(branch.log_path.is_none());
        assert!(branch.mux.is_none());
        assert!(branch.pid.is_none());
    }

    #[test]
    fn session_lineage_predecessor_full_id_joins_the_successor_row() {
        // x-dfe7 AC6-HP (delivery join): mail/inject naming a succeeded
        // session's full uuid finds the row that now answers as B.
        let mut registry = Registry::default();
        let mut entry = sample_entry("worker");
        entry.harness_session_id = Some("session-b".into());
        entry.related_session_id = None;
        entry.predecessor_session_ids = vec!["session-a".into()];
        registry.entries.push(entry);

        assert!(
            registry.find_name_or_full_session_id("session-a").is_some(),
            "a predecessor full id follows the successor row"
        );
        assert!(
            registry.find_name_or_full_session_id("session-b").is_some(),
            "the current session still joins"
        );
    }

    // rename_agent: label mutation with identity lock + alias carry.

    #[test]
    fn rename_agent_renames_and_carries_the_old_label_as_alias() {
        let dir = tmpdir("rename-happy");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            let mut e = sample_entry("worker-a");
            e.harness = Some("claude".into());
            e.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
            registry.entries.push(e);
        })
        .unwrap();

        let (old, new) = rename_agent(&path, "worker-a", "worker-b").unwrap();
        assert_eq!(old, "worker-a");
        assert_eq!(new, "worker-b");

        let reg = load_registry(&path).unwrap();
        let row = reg
            .find("worker-b")
            .expect("renamed row under the new label");
        assert_eq!(row.aliases, vec!["worker-a".to_string()]);
        assert_eq!(
            row.harness_session_id.as_deref(),
            Some("aaaaaaaa-0000-0000-0000-111111111111")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_agent_resolves_by_short_id_and_full_session_id() {
        let dir = tmpdir("rename-tokens");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            let mut e = sample_entry("worker-a");
            e.short_id = "abcd1234".into();
            e.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
            registry.entries.push(e);
        })
        .unwrap();
        rename_agent(&path, "abcd1234", "worker-b").unwrap();
        assert!(load_registry(&path).unwrap().find("worker-b").is_some());

        rename_agent(&path, "aaaaaaaa-0000-0000-0000-111111111111", "worker-c").unwrap();
        let reg = load_registry(&path).unwrap();
        let row = reg.find("worker-c").unwrap();
        assert_eq!(
            row.aliases,
            vec!["worker-a".to_string(), "worker-b".to_string()]
        );

        // Code review: the sibling-verb tiers ride along - a canonical handle
        // (first 8 of the session id) and a related session id resolve too.
        let mut forked = sample_entry("worker-f");
        forked.harness_session_id = Some("bbbbbbbb-0000-0000-0000-222222222222".into());
        forked.related_session_id = Some("cccccccc-0000-0000-0000-333333333333".into());
        update_registry(&path, |registry| registry.entries.push(forked)).unwrap();
        rename_agent(&path, "bbbbbbbb", "worker-g").unwrap();
        assert!(load_registry(&path).unwrap().find("worker-g").is_some());
        rename_agent(&path, "cccccccc-0000-0000-0000-333333333333", "worker-h").unwrap();
        assert!(load_registry(&path).unwrap().find("worker-h").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_agent_refuses_duplicate_and_unknown_and_grammar() {
        let dir = tmpdir("rename-refusals");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            let mut a = sample_entry("worker-a");
            a.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
            registry.entries.push(a);
            registry.entries.push(sample_entry("worker-b"));
        })
        .unwrap();

        let dup = rename_agent(&path, "worker-a", "worker-b").unwrap_err();
        assert!(dup.contains("already names another worker"), "{dup}");
        // Renaming onto a label another row ANSWERS to (its alias) refuses the
        // same way: that label must not be made ambiguous at resolve time.
        rename_agent(&path, "worker-b", "worker-x").unwrap();
        let alias_dup = rename_agent(&path, "worker-b", "worker-a").unwrap_err();
        assert!(
            alias_dup.contains("already names another worker"),
            "{alias_dup}"
        );
        let unknown = rename_agent(&path, "no-such-row", "worker-c").unwrap_err();
        assert!(unknown.contains("no such agent"), "{unknown}");
        let grammar = rename_agent(&path, "worker-a", "bad label!").unwrap_err();
        assert!(grammar.contains("1-64 letters"), "{grammar}");
        // Nothing was written by any refused call (the b->x rename above DID
        // land: worker-x holds it, and "worker-b" answers only as that row's
        // old-label alias, never as a name of its own).
        let reg = load_registry(&path).unwrap();
        assert!(reg.find("worker-a").is_some() && reg.find("worker-x").is_some());
        assert_eq!(
            reg.find("worker-b").map(|e| e.name.as_str()),
            Some("worker-x")
        );
        assert!(reg.find("worker-c").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_agent_by_old_label_after_rename_still_lands_on_the_row() {
        // The "changed before rename" guard needs identity present but the
        // resolved name moved; the alias tier is its observable twin: an old
        // label resolves to the SAME identity, so the write must land.
        let dir = tmpdir("rename-alias-tier");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            let mut e = sample_entry("worker-a");
            e.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
            registry.entries.push(e);
        })
        .unwrap();
        rename_agent(&path, "worker-a", "worker-b").unwrap();
        rename_agent(&path, "worker-a", "worker-c").unwrap();
        let reg = load_registry(&path).unwrap();
        let row = reg
            .find("worker-c")
            .expect("alias token reached the same row");
        assert_eq!(
            row.aliases,
            vec!["worker-a".to_string(), "worker-b".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_agent_same_label_is_a_noop() {
        let dir = tmpdir("rename-noop");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            registry.entries.push(sample_entry("worker-a"));
        })
        .unwrap();
        let (old, new) = rename_agent(&path, "worker-a", "worker-a").unwrap();
        assert_eq!((old, new), ("worker-a".to_string(), "worker-a".to_string()));
        let reg = load_registry(&path).unwrap();
        let row = reg.find("worker-a").unwrap();
        assert!(row.aliases.is_empty(), "no self-alias on a no-op rename");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_agent_id_less_row_leaves_no_false_removal_accounting() {
        // A LEGACY id-less row (no session id, no short id; on disk from before
        // the resolvable-handle invariant) renamed matches NO removal signal
        // except its alias, and account_for_removed_rows must read the alias or
        // it fires registry_row_removed for a live row. The fixture is
        // hand-written JSON because the write choke point refuses to MINT such
        // a row.
        let dir = tmpdir("rename-id-less");
        let path = dir.join("registry.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "schema_version": REGISTRY_SCHEMA_VERSION,
                "agents": [{
                    "name": "worker-a",
                    "cwd": "/tmp/x",
                    "project_root": "/tmp/x",
                    "status": "live",
                    "created_at": "2026-05-24T00:00:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        assert!(load_registry(&path).unwrap().find("worker-a").is_some());
        // The choke point itself refuses: a renamed row reads as NEW to
        // validate_resolvable_handle (its old name left the before map), and an
        // id-less row has no handle - so this verb cannot mint the unresolvable
        // row the accounting fear began with. Fail-closed refusal, row intact.
        let refused = rename_agent(&path, "worker-a", "worker-b").unwrap_err();
        assert!(refused.contains("no resolvable handle"), "{refused}");
        assert!(load_registry(&path).unwrap().find("worker-a").is_some());
        assert!(load_registry(&path).unwrap().find("worker-b").is_none());
        // The ALIAS half of the accounting fix still matters for the path the
        // refusal does not reach: a row renamed by Python's retask (which
        // carries aliases) survives a later Rust write without a false
        // registry_row_removed. Covered by the alias membership in
        // account_for_removed_rows' after_names.
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_update_registry_allows_retired_suffix_collision() {
        let dir = tmpdir("legacy-suffix-compatible");
        let path = dir.join("registry.json");
        update_registry(&path, |registry| {
            let mut first = sample_entry("first");
            first.short_id = "transport1".into();
            first.harness = Some("codex".into());
            first.harness_session_id = Some("019fb417-0000-0000-0000-111122223333".into());
            registry.entries.push(first);
        })
        .unwrap();
        update_registry(&path, |registry| {
            let mut second = sample_entry("second");
            second.short_id = "transport2".into();
            second.harness = Some("codex".into());
            // Different first-eight (canonical) but same last-eight (retired
            // read-only tier) -> allowed: a retired-tier collision never refuses.
            second.harness_session_id = Some("019fb418-0000-0000-0000-111122223333".into());
            registry.entries.push(second);
        })
        .unwrap();

        assert_eq!(load_registry(&path).unwrap().entries.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_registry_loads_empty() {
        let dir = tmpdir("missing");
        let reg = load_registry(&dir.join("registry.json")).unwrap();
        assert_eq!(reg.schema_version, REGISTRY_SCHEMA_VERSION);
        assert!(reg.entries.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn python_written_registry_loads_via_typed_path() {
        // Regression for ab-e5a57efa: the typed daemon read path
        // (`load_registry`, used by list/stop/rm/reconcile/status) must parse a
        // registry authored by Python's `registry.write_registry`. That writer
        // uses the top-level `"agents"` key and `AgentEntry` rows that omit the
        // Rust-daemon-only `short_id`/`project_root` fields. Before the fix the
        // whole-file parse failed and `unwrap_or_default()` returned 0 agents.
        let dir = tmpdir("python-registry");
        let path = dir.join("registry.json");
        // Byte-for-byte the shape Python emits (no short_id, no project_root,
        // key is "agents").
        let python_json = r#"{
  "schema_version": 3,
  "agents": [
    {
      "name": "worker-claude",
      "provider": "claude",
      "cwd": "/Users/x/proj",
      "log_path": "/Users/x/.fno/agents/worker-claude.log",
      "claude_short_id": "abc123",
      "codex_session_id": null,
      "gemini_session_id": null,
      "created_at": "2026-05-26T00:00:00Z",
      "status": "live",
      "last_message_at": null,
      "mcp_channel_id": null
    }
  ]
}"#;
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, python_json).unwrap();

        let reg = load_registry(&path).unwrap();
        assert_eq!(reg.entries.len(), 1, "Python-written row must be read");
        let e = reg.find("worker-claude").unwrap();
        assert_eq!(e.harness_name(), "claude");
        assert_eq!(e.status, AgentStatus::Live);
        // v9: the legacy claude_short_id backfills into short_id on load.
        assert_eq!(e.short_id, "abc123");
        assert_eq!(e.legacy_claude_short_id, None); // consumed by the backfill
                                                    // The other Rust-only field defaults to empty for Python-authored rows.
        assert_eq!(e.project_root, "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn python_row_roundtrips_to_python_shape_under_agents_key() {
        // Codex P1 (PR #364): after the daemon rewrites a Python-authored
        // registry (e.g. `rm` removing one agent), the surviving rows must stay
        // readable by Python -- which reads ONLY the top-level `agents` key and
        // whose `AgentEntry(**row)` rejects unknown keys. So the serialized form
        // must (a) use `agents`, not `entries`, and (b) omit every Rust-only
        // field that a Python row lacks (short_id/project_root/session_id/
        // messaging_socket_path/cc_session_id/pid/last_reconciled_at).
        // v10 (x-880e): a Python-authored row is harness-shaped -- harness +
        // harness_session_id, no provider or per-provider session keys.
        let python_json = r#"{"schema_version":10,"agents":[
            {"name":"w","harness":"codex","cwd":"/p","log_path":"/l",
             "harness_session_id":"sid","created_at":"2026-05-26T00:00:00Z",
             "status":"live","last_message_at":null,"mcp_channel_id":null}]}"#;
        let reg: Registry = serde_json::from_str(python_json).unwrap();
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();

        assert!(out.get("agents").is_some(), "must serialize under `agents`");
        assert!(out.get("entries").is_none(), "must NOT serialize `entries`");
        let row = &out["agents"][0];
        for rust_only in [
            "short_id",
            "project_root",
            "session_id",
            "messaging_socket_path",
            "cc_session_id",
            "pid",
            "pid_start_time",
            "last_reconciled_at",
        ] {
            assert!(
                row.get(rust_only).is_none(),
                "Python-authored row must omit Rust-only field `{rust_only}`"
            );
        }
        // v10: the removed identity keys never re-serialize (skip_serializing).
        for removed in [
            "provider",
            "codex_session_id",
            "gemini_session_id",
            "claude_session_uuid",
        ] {
            assert!(
                row.get(removed).is_none(),
                "v10 row must omit removed key `{removed}`"
            );
        }
        // The canonical identity fields survive.
        assert_eq!(row["name"], "w");
        assert_eq!(row["harness"], "codex");
        assert_eq!(row["harness_session_id"], "sid");
    }

    #[test]
    fn v15_model_provider_roundtrips_separately_from_harness() {
        let dual_axis = concat!("open", "code");
        let python_json = format!(
            r#"{{"schema_version":15,"agents":[
            {{"name":"w","harness":"{dual_axis}","provider":"{dual_axis}","model":"glm-5.3","effort":"low","cwd":"/p",
             "log_path":"/l","created_at":"2026-08-19T00:00:00Z","status":"live"}}]}}"#
        );
        let reg: Registry = serde_json::from_str(&python_json).unwrap();
        assert_eq!(reg.entries[0].harness.as_deref(), Some(dual_axis));
        assert_eq!(reg.entries[0].provider.as_deref(), Some(dual_axis));
        assert_eq!(reg.entries[0].model.as_deref(), Some("glm-5.3"));
        assert_eq!(reg.entries[0].effort.as_deref(), Some("low"));

        let out = serde_json::to_value(&reg).unwrap();
        assert_eq!(out["agents"][0]["harness"], dual_axis);
        assert_eq!(out["agents"][0]["provider"], dual_axis);
        assert_eq!(out["agents"][0]["model"], "glm-5.3");
        assert_eq!(out["agents"][0]["effort"], "low");
    }

    #[test]
    fn host_mode_cross_language_round_trip_parity() {
        // interactive-drive node (ab-26b5fe82): the host_mode add must round-trip
        // both directions across the Rust<->Python registry boundary.

        // (a) Rust READS a Python-written row that OMITS host_mode -> exec.
        let no_key = r#"{"schema_version":3,"agents":[
            {"name":"legacy","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#;
        let reg: Registry = serde_json::from_str(no_key).unwrap();
        assert_eq!(reg.entries[0].host_mode, None);
        assert_eq!(reg.entries[0].host_mode_or_default(), HOST_MODE_EXEC);
        assert!(!reg.entries[0].is_interactive());

        // (b) Rust READS a row carrying host_mode="interactive" -> interactive.
        let interactive = r#"{"schema_version":3,"agents":[
            {"name":"bot2","provider":"codex","cwd":"/p","log_path":"/l",
             "codex_session_id":"019e7157","created_at":"2026-05-26T00:00:00Z",
             "status":"live","host_mode":"interactive"}]}"#;
        let reg: Registry = serde_json::from_str(interactive).unwrap();
        assert_eq!(reg.entries[0].host_mode_or_default(), HOST_MODE_INTERACTIVE);
        assert!(reg.entries[0].is_interactive());

        // (c) Rust WRITES an exec row (host_mode None) -> key OMITTED, so a
        // Python AgentEntry(**row) does not gain an unexpected key and Python's
        // missing-key coercion maps the absence back to "exec".
        let mut exec_entry = sample_entry("w");
        exec_entry.host_mode = None;
        let mut reg = Registry::default();
        reg.entries.push(exec_entry);
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert!(
            out["agents"][0].get("host_mode").is_none(),
            "exec row must omit host_mode (skip_serializing_if)"
        );

        // (d) Rust WRITES an interactive row -> host_mode present and readable.
        let mut int_entry = sample_entry("bot2");
        int_entry.host_mode = Some(HOST_MODE_INTERACTIVE.to_string());
        let mut reg = Registry::default();
        reg.entries.push(int_entry);
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(out["agents"][0]["host_mode"], "interactive");
    }

    #[test]
    fn route_settings_path_survives_a_daemon_read_modify_write() {
        // v12 (x-ae2d): Python stamps the route path at spawn and the Python
        // relaunch paths read it back. The daemon touches the same rows (GC,
        // screen scrape), so if it dropped this field on write-back the guard
        // would read None on every row it had passed through - a guard on one of
        // N paths, which is no guard. This asserts the passthrough holds.
        const PATH: &str = "/home/me/.fno/route-settings/abc123.json";

        // A row Python stamped: build it through the real serializer and inject
        // only the new key, so this stays a round-trip test rather than a
        // hand-written fixture that can drift from the struct.
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("plain"));
        let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

        // (a) An unrouted row OMITS the key, so Python's AgentEntry(**row) gains
        // no unexpected kwarg.
        assert!(wire["agents"][0].get("route_settings_path").is_none());

        // (b) A pre-v12 row (the same shape, key absent) reads as never-routed
        // rather than as corrupt.
        let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(reg.entries[0].route_settings_path, None);

        // (c) The daemon must re-emit a stamped path, not drop it on write-back.
        wire["agents"][0]["route_settings_path"] = serde_json::Value::from(PATH);
        let reg: Registry = serde_json::from_value(wire).unwrap();
        assert_eq!(reg.entries[0].route_settings_path.as_deref(), Some(PATH));
        let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(
            back["agents"][0]["route_settings_path"], PATH,
            "the daemon must re-emit a Python-stamped route path, not drop it"
        );
    }

    #[test]
    fn launch_account_and_related_id_survive_a_daemon_read_modify_write() {
        // v20 (x-d285): Python stamps both at the spawn seams and the re-entry
        // resolver reads them. Same passthrough claim as route_settings_path:
        // a daemon write-back that dropped either key would strip the account
        // axis off every row it touched.
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("plain"));
        let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

        // An unstamped row omits both keys, so Python's AgentEntry(**row) is
        // unaffected and the absence reads as unknown, never as "default".
        assert!(wire["agents"][0].get("launch_account").is_none());
        assert!(wire["agents"][0].get("related_session_id").is_none());

        wire["agents"][0]["launch_account"] = serde_json::Value::from("makers");
        wire["agents"][0]["related_session_id"] = serde_json::Value::from("sess-fork");
        let reg: Registry = serde_json::from_value(wire).unwrap();
        assert_eq!(reg.entries[0].launch_account.as_deref(), Some("makers"));
        assert_eq!(
            reg.entries[0].related_session_id.as_deref(),
            Some("sess-fork")
        );
        let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(back["agents"][0]["launch_account"], "makers");
        assert_eq!(back["agents"][0]["related_session_id"], "sess-fork");
    }

    #[test]
    fn related_session_id_resolves_the_row_at_full_tier() {
        // x-d285: a fork's full uuid addresses its row through the related
        // slot at the same tier as the primary - "both ids stay valid
        // forever" means addressable, not merely stored. The name and the
        // primary id keep resolving unchanged.
        let mut reg = Registry::default();
        let mut row = sample_entry("forked");
        row.harness_session_id = Some("01a027ad-fe00-7c12-a116-9ee37c6bdfec".into());
        row.related_session_id = Some("02b118cf-aa11-4d55-9bc2-7f33a1e99110".into());
        reg.entries.push(row);

        assert!(reg.find_name_or_full_session_id("forked").is_some());
        assert!(reg
            .find_name_or_full_session_id("01a027ad-fe00-7c12-a116-9ee37c6bdfec")
            .is_some());
        assert!(
            reg.find_name_or_full_session_id("02b118cf-aa11-4d55-9bc2-7f33a1e99110")
                .is_some(),
            "the related id resolves the row"
        );
        assert!(reg.find_name_or_full_session_id("nosuch").is_none());
    }

    #[test]
    fn launch_account_from_env_is_three_valued() {
        // x-d285: the Rust mint's account fact. An explicit seam id wins; an
        // ambient config dir the mint cannot attribute is unknown; neither
        // present proves the default slot. Environment mutation is process-
        // wide, so the test snapshots and restores both keys around its arms,
        // under the shared env-mutation lock (a sibling test mutating the same
        // keys on another thread would interleave mid-arm snapshots).
        let _guard = ENV_LOCK.lock().unwrap();
        fn restore(launch: Option<std::ffi::OsString>, dir: Option<std::ffi::OsString>) {
            match launch {
                Some(v) => std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, v),
                None => std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY),
            }
            match dir {
                Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
        }
        let saved_launch = std::env::var_os(LAUNCH_ACCOUNT_ENV_KEY);
        let saved_dir = std::env::var_os("CLAUDE_CONFIG_DIR");

        std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, "makers");
        std::env::set_var("CLAUDE_CONFIG_DIR", "/unrelated");
        let a = launch_account_from_env();

        std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY);
        let b = launch_account_from_env();

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let c = launch_account_from_env();

        restore(saved_launch, saved_dir);

        assert_eq!(a.as_deref(), Some("makers"), "seam id outranks ambient dir");
        assert_eq!(b, None, "an ambient config dir is unknown, never default");
        assert_eq!(
            c.as_deref(),
            Some("default"),
            "neither present proves default"
        );
    }

    #[test]
    fn launch_account_source_rides_a_concrete_account() {
        // x-04ce P2: a source over "default" or an unknown id is the
        // contradiction "config picked the fallback". The carrier counts only
        // when the value read names a real account, and only caller/config
        // speak the vocabulary. Environment mutation is process-wide, so the
        // test snapshots and restores both keys around its arms, under the
        // same env-mutation lock the three-valued test holds.
        let _guard = ENV_LOCK.lock().unwrap();
        fn restore(launch: Option<std::ffi::OsString>, src: Option<std::ffi::OsString>) {
            match launch {
                Some(v) => std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, v),
                None => std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY),
            }
            match src {
                Some(v) => std::env::set_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY, v),
                None => std::env::remove_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY),
            }
        }
        let saved_launch = std::env::var_os(LAUNCH_ACCOUNT_ENV_KEY);
        let saved_src = std::env::var_os(LAUNCH_ACCOUNT_SOURCE_ENV_KEY);

        std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY);
        std::env::set_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY, "config");
        let no_id = launch_account_source_from_env();

        std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, "default");
        let default_id = launch_account_source_from_env();

        std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, "makers");
        let picked = launch_account_source_from_env();

        std::env::remove_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY);
        let flag_only = launch_account_source_from_env();

        std::env::set_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY, "bogus");
        let off_vocabulary = launch_account_source_from_env();

        restore(saved_launch, saved_src);

        assert_eq!(no_id, None, "a carrier without an account proves nothing");
        assert_eq!(default_id, None, "nobody config-picked the fallback slot");
        assert_eq!(picked.as_deref(), Some("config"), "the pick names itself");
        assert_eq!(
            flag_only.as_deref(),
            Some("caller"),
            "an id without a carrier was on the argv"
        );
        assert_eq!(
            off_vocabulary.as_deref(),
            Some("caller"),
            "a word outside the vocabulary reads as the flag"
        );
    }

    #[test]
    fn origin_survives_a_daemon_read_modify_write() {
        // v16 (x-944f): Python stamps this at row birth, and BOTH watchdog
        // lanes read it - reap as a PROTECTOR (an operator session is never
        // reaped) and retire to answer "did footnote make this row?". The
        // daemon touches the same rows, so dropping it on write-back left the
        // protector unreachable and turned every marked worker back into
        // UNKNOWN, which never retires: the lane goes silently unsatisfiable
        // rather than visibly broken. Measured before this mirror existed:
        // 0 of 37 live rows carried an origin, operator rows the SessionStart
        // hook had stamped included. Same passthrough shape as
        // delivery_policy (v14).
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("leader"));
        let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

        // (a) An unmarked row OMITS the key, so a pre-marker row stays unknown
        // rather than gaining a value nothing wrote.
        assert!(wire["agents"][0].get("origin").is_none());

        // (b) A row written before the field existed reads as unknown.
        let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(reg.entries[0].origin, None);

        // (c) The daemon must re-emit every marker Python writes, not drop it.
        for marker in ["spawn", "operator", "adopted"] {
            wire["agents"][0]["origin"] = serde_json::Value::from(marker);
            let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(reg.entries[0].origin.as_deref(), Some(marker));
            let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
            assert_eq!(back["agents"][0]["origin"], marker);
        }
    }

    #[test]
    fn delivery_policy_survives_a_daemon_read_modify_write() {
        // v14 (x-e21e): Python stamps the policy at register time and the
        // Python send path gates on it. The daemon touches the same rows, so if
        // it dropped this field on write-back the gate would silently revert a
        // bus-only recipient to injectable - the defect again, one daemon tick
        // later. Same passthrough assertion shape as route_settings_path (v12).
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("leader"));
        let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

        // (a) An unmarked row OMITS the key (Python's AgentEntry(**row) gains
        // no unexpected kwarg; every worker keeps the default policy).
        assert!(wire["agents"][0].get("delivery_policy").is_none());

        // (b) A pre-v14 row (key absent) reads as no-policy, not corrupt.
        let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(reg.entries[0].delivery_policy, None);

        // (c) The daemon must re-emit a stamped policy, not drop it.
        wire["agents"][0]["delivery_policy"] = serde_json::Value::from("bus-only");
        let reg: Registry = serde_json::from_value(wire).unwrap();
        assert_eq!(reg.entries[0].delivery_policy.as_deref(), Some("bus-only"));
        let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(
            back["agents"][0]["delivery_policy"], "bus-only",
            "the daemon must re-emit a Python-stamped delivery policy, not drop it"
        );
    }

    #[test]
    fn spawn_trigger_survives_a_daemon_read_modify_write() {
        // v16 (x-944f): the same erasure hit `spawn_trigger`, which shipped
        // under x-42c5 as a Python-only field. One struct change fixes both,
        // so both are asserted.
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("triggered"));
        let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

        assert!(wire["agents"][0].get("spawn_trigger").is_none());

        wire["agents"][0]["spawn_trigger"] = serde_json::Value::from("think_spawn:work-start");
        let reg: Registry = serde_json::from_value(wire).unwrap();
        assert_eq!(
            reg.entries[0].spawn_trigger.as_deref(),
            Some("think_spawn:work-start")
        );
        let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(
            back["agents"][0]["spawn_trigger"], "think_spawn:work-start",
            "the daemon must re-emit a Python-stamped spawn trigger, not drop it"
        );
    }

    #[test]
    fn spawned_by_edge_survives_a_daemon_read_modify_write() {
        // x-132c: the parent edge shipped as a Python-only declaration and read
        // 0-of-30 on the live fleet. The struct mirror is what keeps a daemon
        // write-back from erasing it again.
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("child"));
        let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

        assert!(wire["agents"][0].get("spawned_by_session").is_none());

        wire["agents"][0]["spawned_by_session"] =
            serde_json::Value::from("7420e8f7-eeba-4309-8c37-9fc56674f112");
        wire["agents"][0]["spawned_by_harness"] = serde_json::Value::from("claude");
        wire["agents"][0]["spawned_by_cwd"] = serde_json::Value::from("/w");
        let reg: Registry = serde_json::from_value(wire).unwrap();
        assert_eq!(
            reg.entries[0].spawned_by_session.as_deref(),
            Some("7420e8f7-eeba-4309-8c37-9fc56674f112")
        );
        assert_eq!(reg.entries[0].spawned_by_harness.as_deref(), Some("claude"));
        assert_eq!(reg.entries[0].spawned_by_cwd.as_deref(), Some("/w"));
        let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(
            back["agents"][0]["spawned_by_session"], "7420e8f7-eeba-4309-8c37-9fc56674f112",
            "the daemon must re-emit a Python-stamped parent edge, not drop it"
        );
    }

    #[test]
    fn screen_state_cross_language_round_trip_parity() {
        // v7: the additive `screen_state` verdict must round-trip both
        // directions across the Rust<->Python registry boundary, exactly like
        // inside_leg (v5) and mux (v6) before it.

        // (a) Rust READS a row that OMITS screen_state -> None, no migration.
        let no_key = r#"{"schema_version":6,"agents":[
            {"name":"legacy","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#;
        let reg: Registry = serde_json::from_str(no_key).unwrap();
        assert_eq!(reg.entries[0].screen_state, None);

        // (b) Rust READS a full verdict -> Some, all fields land.
        let with_verdict = r#"{"schema_version":7,"agents":[
            {"name":"pane","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live",
             "screen_state":{"state":"idle","rule":"idle_prompt","seq":3,
                             "at":"2026-07-02T00:00:00Z","ttl_ms":30000}}]}"#;
        let reg: Registry = serde_json::from_str(with_verdict).unwrap();
        let v = reg.entries[0].screen_state.as_ref().unwrap();
        assert_eq!(v.state, "idle");
        assert_eq!(v.rule, "idle_prompt");
        assert_eq!(v.seq, 3);
        assert_eq!(v.at, "2026-07-02T00:00:00Z");
        assert_eq!(v.ttl_ms, Some(30000));

        // (c) Rust WRITES a row without a verdict -> key OMITTED, so a Python
        // AgentEntry(**row) gains no unexpected key.
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("w"));
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert!(
            out["agents"][0].get("screen_state").is_none(),
            "row without a verdict must omit screen_state (skip_serializing_if)"
        );

        // (d) Full round-trip preserves the verdict unchanged.
        let mut scraped = sample_entry("pane");
        scraped.screen_state = Some(ScreenStateReport {
            state: "blocked".into(),
            rule: "permission_prompt".into(),
            seq: 9,
            at: "2026-07-02T01:00:00Z".into(),
            ttl_ms: None,
            answerable: None,
        });
        let mut reg = Registry::default();
        reg.entries.push(scraped.clone());
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert!(
            out["agents"][0]["screen_state"].get("ttl_ms").is_none(),
            "absent ttl_ms omitted"
        );
        let reg2: Registry = serde_json::from_value(out).unwrap();
        assert_eq!(reg2.entries[0].screen_state, scraped.screen_state);
    }

    #[test]
    fn screen_state_report_ttl_ages_and_fails_closed() {
        let now = rfc3339_like_to_secs("2026-07-02T00:01:00Z").unwrap();
        let mk = |at: &str, ttl_ms: Option<u64>| ScreenStateReport {
            state: "working".into(),
            rule: "busy".into(),
            seq: 1,
            at: at.into(),
            ttl_ms,
            answerable: None,
        };
        // No TTL never self-ages; in-TTL live; lapsed expires; corrupt stamp
        // fails closed (a bad `at` must not pin a forever-working badge).
        assert!(mk("2026-07-02T00:00:00Z", None).is_live_at(now));
        assert!(mk("2026-07-02T00:00:30Z", Some(60_000)).is_live_at(now));
        assert!(!mk("2026-07-02T00:00:00Z", Some(5_000)).is_live_at(now));
        assert!(!mk("garbage", Some(60_000)).is_live_at(now));
    }

    #[test]
    fn inside_leg_cross_language_round_trip_parity() {
        // inside-out E3.1 (X2/X3): the additive `inside_leg` field must round-trip
        // both directions across the Rust<->Python registry boundary, like every
        // prior additive RegistryEntry field.

        // (a) Rust READS a Python-written row that OMITS inside_leg -> None.
        let no_key = r#"{"schema_version":5,"agents":[
            {"name":"legacy","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#;
        let reg: Registry = serde_json::from_str(no_key).unwrap();
        assert_eq!(reg.entries[0].inside_leg, None);

        // (b) Rust READS a full inside-leg report -> Some, lowercase state parses,
        // optional reason/ttl_ms present.
        let with_report = r#"{"schema_version":5,"agents":[
            {"name":"pane","provider":"claude","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live",
             "inside_leg":{"state":"working","seq":7,"reason":"running tests",
                           "received_at":"2026-06-27T00:00:00Z","ttl_ms":5000}}]}"#;
        let reg: Registry = serde_json::from_str(with_report).unwrap();
        let rep = reg.entries[0].inside_leg.as_ref().unwrap();
        assert_eq!(rep.state, InsideLegState::Working);
        assert_eq!(rep.seq, 7);
        assert_eq!(rep.reason.as_deref(), Some("running tests"));
        assert_eq!(rep.received_at, "2026-06-27T00:00:00Z");
        assert_eq!(rep.ttl_ms, Some(5000));

        // (c) Rust WRITES a row without a report -> key OMITTED (skip_serializing_if),
        // so a Python AgentEntry(**row) does not gain an unexpected key and a stale
        // reader never sees the field.
        let mut bare = sample_entry("w");
        bare.inside_leg = None;
        let mut reg = Registry::default();
        reg.entries.push(bare);
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert!(
            out["agents"][0].get("inside_leg").is_none(),
            "row without a report must omit inside_leg (skip_serializing_if)"
        );

        // (d) Rust WRITES a report -> present, state lowercase, absent reason/ttl
        // omitted (skip_serializing_if on the nested struct).
        let mut withrep = sample_entry("pane");
        withrep.inside_leg = Some(InsideLegReport {
            state: InsideLegState::Done,
            seq: 12,
            reason: None,
            received_at: "2026-06-27T01:00:00Z".into(),
            ttl_ms: None,
        });
        let mut reg = Registry::default();
        reg.entries.push(withrep);
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        let badge = &out["agents"][0]["inside_leg"];
        assert_eq!(badge["state"], "done");
        assert_eq!(badge["seq"], 12);
        assert!(badge.get("reason").is_none(), "absent reason omitted");
        assert!(badge.get("ttl_ms").is_none(), "absent ttl_ms omitted");

        // (e) Full round-trip preserves the report unchanged.
        let reg2: Registry = serde_json::from_value(out).unwrap();
        assert_eq!(
            reg2.entries[0].inside_leg,
            Some(InsideLegReport {
                state: InsideLegState::Done,
                seq: 12,
                reason: None,
                received_at: "2026-06-27T01:00:00Z".into(),
                ttl_ms: None,
            })
        );
    }

    #[test]
    fn rfc3339_like_to_secs_round_trips_known_stamps() {
        // The unix epoch and a couple of fixed dates; values cross-checked against
        // `date -u -d <stamp> +%s`. Proves the days-from-civil inverse matches the
        // daemon's civil() forward direction (the producer of received_at).
        assert_eq!(rfc3339_like_to_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_like_to_secs("2026-06-27T00:00:00Z"),
            Some(1_782_518_400)
        );
        assert_eq!(
            rfc3339_like_to_secs("2026-06-27T00:00:05Z"),
            Some(1_782_518_405)
        );
    }

    #[test]
    fn rfc3339_like_to_secs_rejects_malformed() {
        // Wrong length, bad separators, non-digit, out-of-range fields, and the
        // fractional/offset forms now_rfc3339_like never emits -- all None so the
        // TTL gate fails closed rather than trusting a garbage stamp.
        for bad in [
            "",
            "2026-06-27",
            "2026-06-27T00:00:00",    // no Z
            "2026/06/27T00:00:00Z",   // wrong separators
            "20260627T000000Z",       // compact form, wrong length
            "2026-13-27T00:00:00Z",   // month 13
            "2026-06-27T24:00:00Z",   // hour 24
            "2026-06-27T00:00:00.5Z", // fractional (21 bytes)
            "abcd-ef-ghTij:kl:mnZ",   // non-digit
        ] {
            assert_eq!(rfc3339_like_to_secs(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn inside_leg_is_live_at_ttl_gate() {
        let recv = "2026-06-27T00:00:00Z";
        let recv_secs = rfc3339_like_to_secs(recv).unwrap();
        let rep = |ttl| InsideLegReport {
            state: InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: recv.into(),
            ttl_ms: ttl,
        };

        // No ttl -> never ages out on its own (cleared by teardown/done/newer report).
        assert!(rep(None).is_live_at(recv_secs + 10_000));

        // ttl=5000ms: live at +4s, live exactly at +5s (<=), expired at +6s (AC-X2-2).
        assert!(rep(Some(5000)).is_live_at(recv_secs + 4));
        assert!(rep(Some(5000)).is_live_at(recv_secs + 5));
        assert!(!rep(Some(5000)).is_live_at(recv_secs + 6));

        // A clock that reads BEFORE received_at (skew) is still live (saturating_sub).
        assert!(rep(Some(5000)).is_live_at(recv_secs.saturating_sub(100)));

        // An unparseable received_at with a ttl fails CLOSED (expired), so a corrupt
        // stamp can never pin a permanent badge.
        let mut corrupt = rep(Some(5000));
        corrupt.received_at = "not-a-stamp".into();
        assert!(!corrupt.is_live_at(recv_secs));
    }

    #[test]
    fn rust_reads_python_row_with_explicit_empty_and_null_fields() {
        // ab-b946b59c: Python's `AgentEntry` now mirrors the Rust-only PTY
        // fields, so its `asdict` emits them for EVERY row -- short_id/
        // project_root as "" (their Rust type is `String`, so a null would fail
        // deserialize) and the Option fields as null. Rust must read that shape.
        let python_json = r#"{"schema_version":4,"agents":[
            {"name":"py-ask","provider":"codex","cwd":"/p","log_path":"/l",
             "short_id":"","project_root":"",
             "claude_short_id":null,"codex_session_id":"sid","gemini_session_id":null,
             "claude_session_uuid":null,"messaging_socket_path":null,"cc_session_id":null,
             "mcp_channel_id":null,"host_mode":"exec",
             "created_at":"2026-05-26T00:00:00Z","status":"exited","last_message_at":null,
             "pid":null,"pid_start_time":null,"last_reconciled_at":null}]}"#;
        let reg: Registry = serde_json::from_str(python_json).unwrap();
        let e = &reg.entries[0];
        assert_eq!(e.name, "py-ask");
        assert_eq!(e.short_id, ""); // "" deserializes into the String field
        assert_eq!(e.project_root, "");
        assert_eq!(e.pid, None); // null -> None for the Option fields
        assert_eq!(e.pid_start_time, None);
        assert_eq!(e.cc_session_id, None);
        assert_eq!(e.codex_session_id.as_deref(), Some("sid"));
        assert!(e.is_one_shot_ask(), "empty short_id + no pid => ask row");
    }

    #[test]
    fn pty_agent_still_serializes_its_short_id() {
        // The skip-when-empty must NOT drop a real daemon agent's short_id/pid.
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("worker-A")); // short_id "worker-A-id", pid Some
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        let row = &out["agents"][0];
        assert_eq!(row["short_id"], "worker-A-id");
        assert_eq!(row["pid"], 1234);
    }

    #[test]
    fn empty_registry_file_loads_default_but_corrupt_file_errors() {
        // Gemini high (PR #364): an empty/whitespace file is a valid empty
        // registry, but a present-but-unparseable file must error LOUDLY rather
        // than default -- otherwise update_registry's read-modify-write republishes
        // the empty default and wipes every other agent.
        let dir = tmpdir("corrupt-registry");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");

        // Empty file -> empty registry, no error.
        std::fs::write(&path, "   \n").unwrap();
        assert!(load_registry(&path).unwrap().entries.is_empty());

        // Corrupt (non-empty, unparseable) file -> error, not silent default.
        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(
            load_registry(&path).is_err(),
            "corrupt registry must surface an error"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_refuses_to_wipe_a_corrupt_registry() {
        // The data-loss path Gemini flagged: update_registry reads, mutates,
        // writes. If the read silently defaulted on a corrupt file, the write
        // would publish an (almost) empty registry. It must instead propagate the
        // parse error and leave the file byte-for-byte intact.
        let dir = tmpdir("no-wipe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        let corrupt = "{\"schema_version\": 3, \"agents\": [ BROKEN";
        std::fs::write(&path, corrupt).unwrap();

        let result = update_registry(&path, |r| r.entries.push(sample_entry("new-A")));
        assert!(result.is_err(), "update over corrupt registry must error");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            corrupt,
            "corrupt registry must be left untouched, not overwritten"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_upgrades_schema_version_on_write() {
        // Codex P2 (ab-a171ceb2): a Rust write of an existing older store must
        // bump schema_version to the current version, or the forward-compat bump
        // never takes effect for the common case (stores that predate it).
        let dir = tmpdir("upgrade-on-write");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        std::fs::write(
            &path,
            r#"{"schema_version":3,"agents":[{"name":"w","provider":"codex","cwd":"/p","log_path":"/l","created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#,
        )
        .unwrap();
        update_registry(&path, |r| r.entries.push(sample_entry("w2"))).unwrap();
        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk["schema_version"], REGISTRY_SCHEMA_VERSION,
            "Rust write must upgrade the on-disk schema_version"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A row-shaped raw registry body, the shape the x-a879 loss took from
    /// disk: real fields, no test-side struct construction.
    fn seeded_registry_body(rows: &[String]) -> String {
        format!(
            r#"{{"schema_version":{},"agents":[{}]}}"#,
            REGISTRY_SCHEMA_VERSION,
            rows.join(",")
        )
    }

    fn loss_shaped_row(name: &str, harness: &str, sid: Option<&str>) -> String {
        let sid_json = match sid {
            Some(s) => format!(r#""{s}""#),
            None => "null".to_string(),
        };
        format!(
            r#"{{"name":"{name}","short_id":"{name}-id","harness":"{harness}","harness_session_id":{sid_json},"cwd":"/tmp/{name}","created_at":"2026-09-01T10:00:00Z","status":"live"}}"#
        )
    }

    #[test]
    fn update_registry_accounts_for_a_removed_row() {
        // x-a879: a closure that drops a row leaves a receipt and an event,
        // whatever door the closure belongs to.
        let dir = tmpdir("removal-accounting");
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("registry.json");
        std::fs::write(
            &path,
            seeded_registry_body(&[
                loss_shaped_row("kept-a", "claude", Some("a-s")),
                loss_shaped_row("dropped", "claude", Some("dropped-s")),
                loss_shaped_row("kept-b", "codex", Some("b-s")),
            ]),
        )
        .unwrap();

        update_registry(&path, |r| {
            r.entries.retain(|e| e.name != "dropped");
        })
        .unwrap();

        // The write persisted the two survivors.
        let reg = load_registry(&path).unwrap();
        assert_eq!(reg.entries.len(), 2);

        // One per-row event on the agent-lifecycle log, naming the row, the
        // remover, and a staged receipt (the grouped registry_rows_lost line
        // lands after it; the tests for that event assert its own shape).
        let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
        let removals: Vec<serde_json::Value> = events
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "registry_row_removed")
            .collect();
        assert_eq!(removals.len(), 1, "exactly one removal event: {events}");
        let event: serde_json::Value = removals[0].clone();
        assert_eq!(event["type"], "registry_row_removed");
        assert_eq!(event["source"], "daemon");
        assert_eq!(event["data"]["name"], "dropped");
        assert_eq!(event["data"]["harness"], "claude");
        assert_eq!(event["data"]["harness_session_id"], "dropped-s");
        assert_eq!(event["data"]["receipt_staged"], true);
        assert_eq!(event["data"]["pid"], std::process::id());
        assert!(
            event["data"]["remover"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "the remover is named, not blank: {event}"
        );

        // The receipt sits beside the reap-path ones and says who took the row.
        let receipt_path = home.join("reap-receipts").join("claude-dropped-s.json");
        let receipt: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&receipt_path).unwrap()).unwrap();
        assert_eq!(receipt["row_name"], "dropped");
        assert_eq!(receipt["removed_by"], event["data"]["remover"]);
        assert!(receipt["resume"].as_str().is_some_and(|s| !s.is_empty()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_emits_registry_rows_lost_naming_the_writer() {
        // x-f0d2: beside the per-row receipts, one grouped event names the
        // writer that dropped rows: pid, the verb that ran, and the lost ids
        // with their names. The 09-03 rows vanished with no journal naming a
        // writer; this is that instrument.
        let dir = tmpdir("rows-lost");
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("registry.json");
        std::fs::write(
            &path,
            seeded_registry_body(&[
                loss_shaped_row("kept", "claude", Some("kept-s")),
                loss_shaped_row("dropped", "claude", Some("dropped-s")),
            ]),
        )
        .unwrap();

        update_registry(&path, |r| {
            r.entries.retain(|e| e.name != "dropped");
        })
        .unwrap();

        let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
        let lost: Vec<serde_json::Value> = events
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "registry_rows_lost")
            .collect();
        assert_eq!(lost.len(), 1, "exactly one grouped loss event: {events}");
        let data = &lost[0]["data"];
        assert_eq!(data["writer"], "rust");
        assert_eq!(data["pid"], std::process::id());
        assert!(
            data["verb"].as_str().is_some_and(|s| !s.is_empty()),
            "the verb names the door, not just the binary: {}",
            lost[0]
        );
        let ids: Vec<&str> = data["lost"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["harness_session_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["dropped-s"]);
        assert_eq!(data["lost"][0]["name"], "dropped");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_emits_nothing_when_nothing_is_removed() {
        let dir = tmpdir("removal-quiet");
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("registry.json");
        std::fs::write(
            &path,
            seeded_registry_body(&[loss_shaped_row("solo", "claude", Some("solo-s"))]),
        )
        .unwrap();

        update_registry(&path, |r| {
            r.entries.push(sample_entry("added"));
        })
        .unwrap();

        assert!(
            !home.join("events.jsonl").exists(),
            "a removal-free write must not open the events stream"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_announces_a_removal_it_cannot_build_a_receipt_for() {
        let dir = tmpdir("removal-no-receipt");
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("registry.json");
        std::fs::write(
            &path,
            seeded_registry_body(&[
                loss_shaped_row("kept", "claude", Some("kept-s")),
                // No session identity: no resume record is renderable.
                loss_shaped_row("identity-less", "claude", None),
            ]),
        )
        .unwrap();

        update_registry(&path, |r| {
            r.entries.retain(|e| e.name != "identity-less");
        })
        .unwrap();

        // The write itself succeeds and the event still announces the removal.
        let reg = load_registry(&path).unwrap();
        assert_eq!(reg.entries.len(), 1);
        let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(events.lines().next().unwrap()).unwrap();
        assert_eq!(event["data"]["name"], "identity-less");
        assert_eq!(event["data"]["receipt_staged"], false);
        assert!(
            event["data"]["reason"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "the announce carries the receipt-build failure: {event}"
        );
        assert!(
            !home.join("reap-receipts").exists(),
            "no receipt file for a row with nothing to resume"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_keeps_a_receipt_the_sweep_already_staged() {
        // The reap sweep writes its receipt BEFORE dropping the rows through
        // this same choke point. The accounting must not rewrite that file
        // with `removed_by`: a pure reap receipt keeps the x-b150 shape.
        let dir = tmpdir("removal-keep-staged");
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("registry.json");
        std::fs::write(
            &path,
            seeded_registry_body(&[loss_shaped_row("swept", "claude", Some("swept-s"))]),
        )
        .unwrap();
        let receipt_path = home.join("reap-receipts").join("claude-swept-s.json");
        std::fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
        std::fs::write(
            &receipt_path,
            r#"{"row_name":"swept","resume":"claude --resume swept-s"}"#,
        )
        .unwrap();

        update_registry(&path, |r| {
            r.entries.retain(|e| e.name != "swept");
        })
        .unwrap();

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&receipt_path).unwrap()).unwrap();
        assert!(
            on_disk.get("removed_by").is_none(),
            "the sweep's receipt was rewritten: {on_disk}"
        );
        let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(events.lines().next().unwrap()).unwrap();
        assert_eq!(event["data"]["receipt_staged"], true);
        assert_eq!(event["data"]["name"], "swept");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_does_not_announce_a_rename() {
        // A rename mutates the name while the session stays: not a removal.
        let dir = tmpdir("removal-rename");
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("registry.json");
        std::fs::write(
            &path,
            seeded_registry_body(&[loss_shaped_row("old-name", "claude", Some("rn-s"))]),
        )
        .unwrap();

        update_registry(&path, |r| {
            if let Some(e) = r.entries.iter_mut().find(|e| e.name == "old-name") {
                e.name = "new-name".into();
            }
        })
        .unwrap();

        assert!(
            !home.join("events.jsonl").exists(),
            "a rename must not read as a removal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_does_not_announce_a_session_id_backfill() {
        // The late-bind shape: a closure fills harness_session_id on an
        // existing row. The row stays; only its identity token set grows.
        let dir = tmpdir("removal-backfill");
        let home = dir.join("agents");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("registry.json");
        std::fs::write(
            &path,
            seeded_registry_body(&[loss_shaped_row("bound-later", "claude", None)]),
        )
        .unwrap();

        update_registry(&path, |r| {
            if let Some(e) = r.entries.iter_mut().find(|e| e.name == "bound-later") {
                e.harness_session_id = Some("late-bound-s".into());
            }
        })
        .unwrap();

        assert!(
            !home.join("events.jsonl").exists(),
            "a session-id backfill must not read as a removal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_registry_reads_a_newer_schema_forward() {
        // This assertion was inverted deliberately. The old contract (Codex P2,
        // ab-a171ceb2) refused a future version so a stale reader could not
        // silently drop a field. Refusing turned out to be the worse failure:
        // registry.json is shared by every agent on the machine, so one process
        // ahead of the deployment took the whole fleet's reads down at once.
        // Dropping a field is made safe by refusing to WRITE and by announcing
        // the degrade, not by refusing to look.
        let dir = tmpdir("version-guard");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        let future = REGISTRY_SCHEMA_VERSION + 1;
        std::fs::write(
            &path,
            format!(r#"{{"schema_version":{future},"agents":[]}}"#),
        )
        .unwrap();
        assert!(
            load_registry(&path).is_ok(),
            "a newer writer must not brick this reader"
        );
        std::fs::write(&path, r#"{"schema_version":1,"agents":[]}"#).unwrap();
        assert!(
            load_registry(&path).is_ok(),
            "v1 must still read (back-compat)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_newer_row_with_an_unknown_status_skips_that_row_only() {
        // Added KEYS were only half the problem. AgentStatus has no catch-all
        // variant, so one row carrying a status this binary never heard of used
        // to fail the WHOLE file at serde and take the daemon's reads with it.
        let dir = tmpdir("newer-status");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        std::fs::write(
            &path,
            // One past THIS binary, derived so a schema bump cannot quietly
            // turn the future-schema fixture into a current-schema one and
            // leave the test asserting a condition it no longer sets up.
            format!(
                r#"{{"schema_version":{},"agents":[
                {{"name":"future","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"hibernating","created_at":"2026-01-01T00:00:00Z"}},
                {{"name":"readable","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"live","created_at":"2026-01-01T00:00:00Z"}}
            ]}}"#,
                REGISTRY_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();

        let reg = load_registry(&path).expect("one unrepresentable row must not brick the read");

        let names: Vec<&str> = reg.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["readable"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_structured_status_from_a_newer_writer_skips_its_row_only() {
        // The three readers must agree about the same file. This row used to be
        // kept as "live" by the raw client path while the typed path skipped it,
        // so `fno agents list` showed a worker the mail path could not see.
        let dir = tmpdir("structured-status");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        std::fs::write(
            &path,
            // Derived, not literal: see the sibling fixture above.
            format!(
                r#"{{"schema_version":{},"agents":[
                {{"name":"future","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":{{"state":"live","since":1}},"created_at":"2026-01-01T00:00:00Z"}},
                {{"name":"readable","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"live","created_at":"2026-01-01T00:00:00Z"}}
            ]}}"#,
                REGISTRY_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();

        let reg = load_registry(&path).expect("a structured status must not brick the read");

        let names: Vec<&str> = reg.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["readable"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_same_unknown_status_stays_fatal_at_our_own_schema() {
        // At or below our schema an unknown value is a writer bug, not a gap.
        let dir = tmpdir("current-status");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        std::fs::write(
            &path,
            &format!(
                r#"{{"schema_version":{},"agents":[
                {{"name":"bad","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"hibernating","created_at":"2026-01-01T00:00:00Z"}}
            ]}}"#,
                REGISTRY_SCHEMA_VERSION
            ),
        )
        .unwrap();

        assert!(load_registry(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_refuses_to_write_over_a_newer_schema() {
        // The write block is what makes reading forward safe here.
        let dir = tmpdir("version-write-guard");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        // Derived from the constant so a bump cannot make "newer" mean "ours".
        let newer_version = REGISTRY_SCHEMA_VERSION + 1;
        let newer = format!(r#"{{"schema_version":{newer_version},"agents":[]}}"#);
        std::fs::write(&path, &newer).unwrap();

        match update_registry(&path, |reg| reg.entries.clear()) {
            Err(StateError::UnsupportedSchemaVersion { found, max }) => {
                assert_eq!(found, newer_version);
                assert_eq!(max, REGISTRY_SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            newer,
            "the refused write must leave the newer file byte-identical"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_then_load_roundtrips_and_preserves_optionals() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("registry.json");
        update_registry(&path, |r| r.entries.push(sample_entry("worker-A"))).unwrap();

        // A second update that only flips status must preserve codex_session_id.
        update_registry(&path, |r| {
            r.find_mut("worker-A").unwrap().status = AgentStatus::Idle;
        })
        .unwrap();

        let reg = load_registry(&path).unwrap();
        let e = reg.find("worker-A").unwrap();
        assert_eq!(e.status, AgentStatus::Idle);
        assert_eq!(e.codex_session_id.as_deref(), Some("uuid-1"));
        assert_eq!(e.pid, Some(1234));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A row with none of the three legs: no pid identity, no legacy provider
    /// (so `backfill_harness_aliases` cannot promote one), no log_path, no
    /// harness. Unlike `sample_entry`, which carries a `legacy_provider` that
    /// backfill resolves into a real `harness`/`harness_session_id` pair
    /// before the write-choke-point guard ever runs.
    fn handleless_entry(name: &str) -> RegistryEntry {
        let mut e = sample_entry(name);
        e.legacy_provider = String::new();
        e.codex_session_id = None;
        e.session_id = None;
        e.pid = None;
        e.pid_start_time = None;
        e.log_path = None;
        e.harness = None;
        e.harness_session_id = None;
        e
    }

    #[test]
    fn validate_resolvable_handle_refuses_all_three_legs_empty() {
        let e = handleless_entry("ghost");
        let err = validate_resolvable_handle(&e).expect_err("handle-less row must be refused");
        assert_eq!(
            err,
            "registry row 'ghost' carries no resolvable handle: needs one of \
             (pid + pid_start_time), log_path, or (harness + harness_session_id)"
        );
    }

    // ── x-665d: refuse a source-ahead bump of the SHARED registry ───────────

    #[test]
    fn source_root_for_exe_finds_the_checkout_above_a_target_build() {
        let root = tmpdir("src-root-target");
        let exe = root.join("target").join("debug").join("fno-agents");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        assert_eq!(source_root_for_exe(&exe, None), Some(root.clone()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn source_root_for_exe_counts_a_linked_worktrees_git_file() {
        // A linked worktree's `.git` is a FILE. `is_dir()` would miss every
        // worktree, which is the only place a source-ahead build ever runs.
        let root = tmpdir("src-root-worktree");
        let exe = root.join("target").join("debug").join("fno-agents");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(root.join(".git"), b"gitdir: /elsewhere\n").unwrap();

        assert_eq!(source_root_for_exe(&exe, None), Some(root.clone()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn source_root_for_exe_returns_none_for_a_deployed_binary() {
        let root = tmpdir("src-root-deployed");
        let exe = root.join("cargo").join("bin").join("fno-agents");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();

        assert_eq!(source_root_for_exe(&exe, None), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn source_root_for_exe_stops_at_home_so_a_dotfiles_repo_is_not_a_checkout() {
        // `~/.cargo/bin/fno-agents` reaches `$HOME` in three steps. Plenty of
        // people keep a dotfiles repo there; without the stop, every deployed
        // binary on such a machine would read as source-run and refuse writes
        // it must be allowed to make.
        let home = tmpdir("src-root-home");
        let exe = home.join(".cargo").join("bin").join("fno-agents");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(home.join(".git")).unwrap();

        assert_eq!(source_root_for_exe(&exe, Some(&home)), None);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn source_root_for_exe_stops_at_a_symlinked_home_too() {
        // The caller canonicalizes the exe, so it must canonicalize home as
        // well. A raw `$HOME` that resolves through a symlink never equals the
        // canonical ancestor, the walk runs past home, and a dotfiles repo
        // there turns a deployed binary into a source-run one.
        // Canonicalize the base first: on macOS `/var` is itself a symlink to
        // `/private/var`, so a raw temp path would add a second layer and
        // measure that instead of the one this test is about.
        let real = tmpdir("src-root-symlink-home").canonicalize().unwrap();
        let exe = real.join(".cargo").join("bin").join("fno-agents");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(real.join(".git")).unwrap();
        let link = real.with_extension("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The raw (symlinked) spelling does not match the canonical ancestor.
        assert_eq!(source_root_for_exe(&exe, Some(&link)), Some(real.clone()));
        // Canonicalized, as the caller now does, it stops the walk.
        let canonical = link.canonicalize().unwrap();
        assert_eq!(source_root_for_exe(&exe, Some(&canonical)), None);

        std::fs::remove_file(&link).ok();
        std::fs::remove_dir_all(&real).ok();
    }

    /// The git-managed-$HOME case, constructed rather than observed.
    ///
    /// This developer's machine has an unsymlinked HOME with no `.git` above
    /// `~/.fno`, so the inverted guard cannot fire here and a green suite would
    /// say nothing about it. A dotfiles repo at `$HOME` is one of the most
    /// common setups there is, and footnote ships to those people.
    ///
    /// Writing this test narrowed the claim it was written to defend, which is
    /// why it exists. At the DEFAULT registry location the home stop is not what
    /// saves a deployed binary: `~/.fno/agents/registry.json` sits inside `$HOME`,
    /// so the store-inside-the-root escape hatch already returns "proceed" with
    /// or without it. The stop is load-bearing only once the registry lives
    /// OUTSIDE home - `FNO_AGENTS_HOME` pointed at `/var/lib/...`, or a
    /// relocated `config.paths.agents_registry_path`. Both are asserted below,
    /// in both directions, so neither reads as passing by accident.
    #[test]
    fn a_deployed_binary_under_a_git_managed_home_still_writes() {
        let home = tmpdir("git-managed-home").canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let exe = home.join(".cargo").join("bin").join("fno-agents");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let older = REGISTRY_SCHEMA_VERSION - 1;

        // The default location, inside home. Deployed either way: the
        // store-inside-the-root check answers this one before the stop matters.
        let in_home = home.join(".fno").join("agents").join("registry.json");
        std::fs::create_dir_all(in_home.parent().unwrap()).unwrap();
        assert_eq!(
            source_ahead_root(&exe, Some(&home), &in_home, &in_home, older),
            None
        );
        assert_eq!(
            source_ahead_root(&exe, None, &in_home, &in_home, older),
            None
        );

        // A registry OUTSIDE home (FNO_AGENTS_HOME, or a relocated path). Here
        // the stop is the only thing keeping a deployed binary writable.
        let outside = tmpdir("git-managed-home-store")
            .canonicalize()
            .unwrap()
            .join("registry.json");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        assert_eq!(
            source_ahead_root(&exe, Some(&home), &outside, &outside, older),
            None,
            "a cargo-installed binary must write even when $HOME is a git repo"
        );
        assert_eq!(
            source_ahead_root(&exe, None, &outside, &outside, older),
            Some(home.clone()),
            "without the stop the same deployed binary refuses: this is the bug"
        );

        // SOURCE: a real checkout that is not home still refuses, so the stop
        // did not buy deployment safety by disarming the guard.
        let checkout = home.join("code").join("footnote");
        let src_exe = checkout.join("target").join("debug").join("fno-agents");
        std::fs::create_dir_all(src_exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(checkout.join(".git")).unwrap();
        assert_eq!(
            source_ahead_root(&src_exe, Some(&home), &outside, &outside, older),
            Some(checkout),
            "a source build outside home must still refuse"
        );

        std::fs::remove_dir_all(outside.parent().unwrap()).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_registry_refuses_a_source_ahead_bump_of_the_shared_registry() {
        // AC8-CON: `registry.json` has writers in two languages, so a guard on
        // one leaves the daemon, mux, and every client verb still able to
        // poison the file. This test binary genuinely runs from
        // `<checkout>/target/...`, so it IS the source-run case; pointing
        // FNO_AGENTS_HOME at a tmp dir outside the checkout makes that dir the
        // process-global registry the guard protects.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tmpdir("source-ahead-shared");
        let saved = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", &home);
        let path = crate::paths::AgentsHome::from_env().registry_json();
        let older = REGISTRY_SCHEMA_VERSION - 1;
        let body = format!(r#"{{"schema_version":{older},"agents":[]}}"#);
        std::fs::write(&path, &body).unwrap();

        let result = update_registry(&path, |reg| reg.entries.clear());

        match saved {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        match result {
            Err(StateError::SourceAheadSchemaBump { found, current, .. }) => {
                assert_eq!(found, older);
                assert_eq!(current, REGISTRY_SCHEMA_VERSION);
            }
            other => panic!("expected SourceAheadSchemaBump, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            body,
            "the refused write must leave the file byte-identical"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_registry_bumps_a_named_store_that_is_not_the_shared_one() {
        // AC3-HP, the escape hatch: it works by moving the target, never by
        // silencing the check. `update_registry_upgrades_schema_version_on_write`
        // covers the same ground for the ordinary older-store upgrade; this one
        // states the guard's own condition.
        let dir = tmpdir("named-store-bump");
        let path = dir.join("registry.json");
        let older = REGISTRY_SCHEMA_VERSION - 1;
        std::fs::write(
            &path,
            format!(r#"{{"schema_version":{older},"agents":[]}}"#),
        )
        .unwrap();

        update_registry(&path, |r| r.entries.push(sample_entry("w"))).unwrap();

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk["schema_version"], REGISTRY_SCHEMA_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_resolvable_handle_passes_for_each_leg_alone() {
        let mut leg1 = handleless_entry("pid-only");
        leg1.pid = Some(4242);
        leg1.pid_start_time = Some(9);
        assert!(validate_resolvable_handle(&leg1).is_ok());

        let mut leg2 = handleless_entry("log-only");
        leg2.log_path = Some("/tmp/some.log".into());
        assert!(validate_resolvable_handle(&leg2).is_ok());

        let mut leg3 = handleless_entry("harness-only");
        leg3.harness = Some("claude".into());
        leg3.harness_session_id = Some("sess-1".into());
        assert!(validate_resolvable_handle(&leg3).is_ok());

        // Half of leg1 (pid without pid_start_time) must not pass.
        let mut half_leg1 = handleless_entry("pid-no-start");
        half_leg1.pid = Some(4242);
        assert!(validate_resolvable_handle(&half_leg1).is_err());

        // Half of leg3 (harness without a session id) must not pass.
        let mut half_leg3 = handleless_entry("harness-no-session");
        half_leg3.harness = Some("claude".into());
        assert!(validate_resolvable_handle(&half_leg3).is_err());
    }

    #[test]
    fn update_registry_refuses_a_new_handleless_row() {
        let dir = tmpdir("refuse-handleless");
        let path = dir.join("registry.json");
        // Seed with one legitimate row so the "unchanged on refusal" assertion
        // has real content to check.
        update_registry(&path, |r| r.entries.push(sample_entry("seed"))).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let result = update_registry(&path, |r| r.entries.push(handleless_entry("ghost")));
        match result {
            Err(StateError::InvariantViolation(msg)) => {
                assert!(msg.contains("ghost"), "error must name the row: {msg}");
                assert!(msg.contains("resolvable handle"));
            }
            other => panic!("expected InvariantViolation, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a refused write must leave the registry unchanged"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_writes_a_new_row_carrying_any_single_leg() {
        let dir = tmpdir("accept-single-leg");
        let path = dir.join("registry.json");

        let mut leg1 = handleless_entry("owns-pid");
        leg1.pid = Some(111);
        leg1.pid_start_time = Some(1);
        update_registry(&path, |r| r.entries.push(leg1)).unwrap();

        let mut leg2 = handleless_entry("owns-log");
        leg2.log_path = Some("/tmp/owns-log.log".into());
        update_registry(&path, |r| r.entries.push(leg2)).unwrap();

        let mut leg3 = handleless_entry("owns-harness");
        leg3.harness = Some("codex".into());
        leg3.harness_session_id = Some("thread-1".into());
        update_registry(&path, |r| r.entries.push(leg3)).unwrap();

        let reg = load_registry(&path).unwrap();
        let names: std::collections::BTreeSet<&str> =
            reg.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["owns-pid", "owns-log", "owns-harness"])
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_never_revalidates_a_preexisting_violating_row() {
        // AC3-FR, the wedge test: a registry that already holds a row
        // violating the invariant (the measured shape -- a legacy row with
        // no pid, no log_path, no harness_session_id) must remain writable
        // forever after. The guard only ever looks at rows absent from the
        // pre-write snapshot.
        let dir = tmpdir("wedge");
        let path = dir.join("registry.json");
        // Written directly (not through update_registry) so no guard has ever
        // seen this row -- exactly how a pre-x-7bcd legacy row landed.
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{REGISTRY_SCHEMA_VERSION},"agents":[
                {{"name":"legacy-ghost","cwd":"/x","log_path":null,
                 "created_at":"2026-01-01T00:00:00Z","status":"live"}}
            ]}}"#
            ),
        )
        .unwrap();

        // An unrelated mutation (touching a DIFFERENT row) must succeed even
        // though "legacy-ghost" still carries no resolvable handle.
        let mut leg2 = handleless_entry("new-and-valid");
        leg2.log_path = Some("/tmp/new-and-valid.log".into());
        let result = update_registry(&path, |r| r.entries.push(leg2));
        assert!(
            result.is_ok(),
            "a pre-existing violation must never wedge an unrelated write: {result:?}"
        );

        let reg = load_registry(&path).unwrap();
        assert!(
            reg.find("legacy-ghost").is_some(),
            "the pre-existing violating row must survive untouched"
        );
        assert!(reg.find("new-and-valid").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_json_absent_is_none_present_roundtrips() {
        let dir = tmpdir("state");
        let path = dir.join("wkA/state.json");
        assert!(load_state(&path).unwrap().is_none());

        let st = AgentState::new_pty("wkA");
        write_state_atomic(&path, &st).unwrap();
        let back = load_state(&path).unwrap().unwrap();
        assert_eq!(back.short_id, "wkA");
        assert_eq!(back.status, AgentStatus::Spawning);
        assert!(back.pty.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_state_file_treated_as_absent() {
        // Recovery's "registry entry with partial state.json" path: a present
        // but empty file must read as None (-> inconsistent), never an error.
        let dir = tmpdir("empty-state");
        let path = dir.join("state.json");
        std::fs::write(&path, b"").unwrap();
        assert!(load_state(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn take_active_drive_reads_before_clear() {
        // The recovery ordering invariant in miniature: the returned value
        // carries the session id, and after the call the window is cleared.
        let mut pty = PtyState {
            active: true,
            drive: Some(DriveWindow {
                session_id: Some("drive-uuid".into()),
                mode: Some("interactive".into()),
                last_heartbeat_at_monotonic_ns: Some(42),
            }),
        };
        let taken = pty.take_active_drive().expect("a drive was active");
        assert_eq!(taken.session_id.as_deref(), Some("drive-uuid"));
        assert_eq!(taken.mode.as_deref(), Some("interactive"));
        // Cleared after read.
        assert!(pty.drive.is_none());
        // Idempotent: a second take finds nothing.
        assert!(pty.take_active_drive().is_none());
    }

    #[test]
    fn take_active_drive_none_when_no_drive() {
        let mut pty = PtyState::default();
        assert!(pty.take_active_drive().is_none());
    }

    #[test]
    fn pty_state_wire_shape_is_flat_and_stable() {
        // The Option<DriveWindow> in-memory shape must still serialize to the
        // flat state.json schema (Wave 7 cross-language parity).
        let no_drive = PtyState {
            active: true,
            drive: None,
        };
        assert_eq!(
            serde_json::to_value(&no_drive).unwrap(),
            serde_json::json!({"active": true, "drive_active": false})
        );

        let with_drive = PtyState {
            active: true,
            drive: Some(DriveWindow {
                session_id: Some("d-1".into()),
                mode: Some("interactive".into()),
                last_heartbeat_at_monotonic_ns: Some(99),
            }),
        };
        assert_eq!(
            serde_json::to_value(&with_drive).unwrap(),
            serde_json::json!({
                "active": true,
                "drive_active": true,
                "drive_session_id": "d-1",
                "drive_mode": "interactive",
                "last_heartbeat_at_monotonic_ns": 99
            })
        );
        // Roundtrips back to the same typed value.
        let back: PtyState =
            serde_json::from_value(serde_json::to_value(&with_drive).unwrap()).unwrap();
        assert_eq!(back, with_drive);
    }

    #[test]
    fn pty_state_collapses_inconsistent_legacy_shape() {
        // A legacy/partial file with drive_active:false but a stray session_id
        // deserializes to drive: None - the inconsistent state is normalized
        // away rather than carried.
        let legacy = serde_json::json!({
            "active": true,
            "drive_active": false,
            "drive_session_id": "stray",
        });
        let pty: PtyState = serde_json::from_value(legacy).unwrap();
        assert!(pty.drive.is_none());
    }

    // (x-4c87 raw-versus-decoded row count family) moved verbatim into its own module: this file is over the
    // shrink-only line, and test motion is the sanctioned shrink.
    #[path = "x4c87_row_counts.rs"]
    mod row_count_tests;
}

// ---------------------------------------------------------------------------
// v23 (x-2019): the substitution verdict. The Rust twin of
// fno.agents.row_contradiction.model_substitution - one comparison, two
// languages, so the daemon's list rows and Python's emitters cannot disagree
// about which row substituted.
// ---------------------------------------------------------------------------

/// The bracketed capacity suffix a model token may carry: a request spelled
/// `glm-5.3[1m]` served by a session answering as `glm-5.3` is the SAME model
/// to the operator's eye - the suffix names the context window requested, not
/// a different model - so the comparison strips exactly one trailing suffix
/// from each side before comparing.
fn model_family(token: &str) -> String {
    let trimmed = token.trim();
    match trimmed.rfind('[') {
        Some(idx) if trimmed.ends_with(']') && idx > 0 => trimmed[..idx].to_lowercase(),
        _ => trimmed.to_lowercase(),
    }
}

/// The three-word comparison: `substituted` names the silent replacement,
/// `match` is same-family after suffix normalization, `unknown` covers every
/// missing or unreadable side - an unanswered probe is not a verdict, and
/// neither is a row whose mint never saw a request.
pub fn model_substitution(
    requested: Option<&str>,
    observed: Option<&serde_json::Value>,
) -> &'static str {
    let observed_token = match observed {
        Some(v) if v.get("kind").and_then(serde_json::Value::as_str) == Some("observed") => {
            v.get("model").and_then(serde_json::Value::as_str)
        }
        Some(v) if v.is_string() => v.as_str(),
        _ => None,
    };
    let (Some(req), Some(obs)) = (requested, observed_token) else {
        return "unknown";
    };
    if req.trim().is_empty() || obs.trim().is_empty() {
        return "unknown";
    }
    if model_family(req) == model_family(obs) {
        "match"
    } else {
        "substituted"
    }
}

#[cfg(test)]
mod substitution_tests {
    use super::*;

    #[test]
    fn specimen_table_matches_the_node() {
        let obs = |m: &str| serde_json::json!({"kind": "observed", "model": m});
        // The operator's specimen trio, verbatim.
        assert_eq!(
            model_substitution(Some("glm-5.3[1m]"), Some(&obs("glm-5.3"))),
            "match"
        );
        assert_eq!(
            model_substitution(Some("glm-5.3-flash[1m]"), Some(&obs("glm-5.3-flash"))),
            "match"
        );
        assert_eq!(
            model_substitution(Some("glm-5.3[1m]"), Some(&obs("glm-5.3-flash"))),
            "substituted"
        );
        // Family change reads the same in either direction.
        assert_eq!(
            model_substitution(Some("glm-5.3-flash"), Some(&obs("glm-5.3"))),
            "substituted"
        );
        // Missing / unreadable sides are UNKNOWN, never a match.
        assert_eq!(model_substitution(None, Some(&obs("glm-5.3"))), "unknown");
        assert_eq!(
            model_substitution(
                Some("glm-5.3[1m]"),
                Some(&serde_json::json!({"kind": "no-transcript"}))
            ),
            "unknown"
        );
        assert_eq!(model_substitution(Some("glm-5.3[1m]"), None), "unknown");
        assert_eq!(
            model_substitution(Some(""), Some(&obs("glm-5.3"))),
            "unknown"
        );
        assert_eq!(
            model_substitution(
                Some("glm-5.3[1m]"),
                Some(&serde_json::json!({"kind": "observed", "model": null}))
            ),
            "unknown"
        );
        // A bare observed string (the direct-call shape) still compares.
        assert_eq!(
            model_substitution(Some("glm-5.3[1m]"), Some(&serde_json::json!("glm-5.3"))),
            "match"
        );
        assert_eq!(
            model_substitution(
                Some("glm-5.3[1m]"),
                Some(&serde_json::json!("glm-5.3-flash"))
            ),
            "substituted"
        );
    }
}

#[cfg(test)]
#[path = "state_lookup_tests.rs"]
mod lookup_tests;
