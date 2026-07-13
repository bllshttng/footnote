//! Wire protocol + socket lifecycle for the fno mux.
//!
//! Length-prefixed (u32 big-endian) JSON messages over a Unix socket at
//! `~/.fno/mux/<session>.sock`. The socket dir is 0700 - it accepts keystrokes
//! into your shell, so it is a security boundary. There is no lockfile: the
//! socket bind IS the lock, liveness is a connect-probe, and a stale socket is
//! unlinked at bind time.
//!
//! Channel discipline (epic Locked Decision): client->server input/control is
//! reliable and never dropped; only server->client render frames are
//! droppable, and a droppable frame is always SELF-CONTAINED (a `Frame`
//! carries the full grid + cursor, never a delta over a possibly-dropped
//! predecessor).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::tree::{Dir, Rect, TabId};

/// Bumped on any wire-incompatible change. The server outlives `cargo install`
/// upgrades, so both sides exchange this at Attach and refuse loudly on skew.
/// There is no automated backstop tying this to the message shapes: bump it in
/// the SAME commit as any `ClientMsg`/`ServerMsg`/`Frame` shape change.
///
/// v2 (Phase 2 layout): `Attach` gains `cwd`; `Frame`s are pane-tagged;
/// `Command`/`Layout`/`ModeSync` added; the never-sent `ServerMsg::Cursor`
/// variant is removed (the cursor rides INSIDE `Frame`).
///
/// v3 (Phase 3 multi-client/sessions): `TabMeta` gains a stable `id`;
/// `Command::SelectTab` selects by that id (u64), not by index; `Layout`
/// gains `area` (the clamped content-area its rects were computed for);
/// pre-Attach `ClientMsg::{Query, KillServer}` + `ServerMsg::Info` added
/// (wire-shape FROZEN - see the variants).
///
/// v4 (Phase 4a script API): the one-shot control-verb family -
/// `ClientMsg::Control { proto, build, verb }` carrying [`ControlVerb`], and
/// the replies `ServerMsg::{PaneList, PaneText, PaneSpawned, Ok, WaitDone,
/// Err}`. Control connections handshake exactly like `Attach` (versioned, NOT
/// pre-Attach-frozen); the frozen `Query`/`KillServer` pair is untouched.
///
/// v5 (Phase 4a agent edge, G2+G3 shapes in one bump): `Layout` gains
/// `agents` (sideline [`AgentRow`]s with the fact-badge lattice);
/// `ControlVerb::PaneRun` gains `claim` (the per-pane writer-claim opt-in set
/// at agent spawn); `ControlVerb::{PaneClaim, PaneRelease}` added (the relay
/// acquires around an injection burst).
///
/// v6 (Phase 4b block model): `PaneRead` gains `block` ([`BlockSel`]) and its
/// `lines` reaches into history; `PaneWait` gains `command_done`; `WaitOutcome`
/// gains `CommandDone`; `PaneText` gains `block` ([`BlockMeta`]); `err_code`
/// gains `BLOCK_UNAVAILABLE`. OSC 133 command blocks (see [`crate::vt`]).
///
/// v7 (Phase 5 G1 scroll/select/copy): `ClientMsg::Mouse { pane, event }`
/// ([`MouseEvent`]) - the client forwards pane-rect mouse events; the server
/// routes by the pane's mouse mode (SGR-encode to the PTY, else mux-side
/// scroll/focus/selection). `ServerMsg::Copy { text }` ships extracted
/// selection text to the client's clipboard chain. `cell_flags::SELECTED`
/// marks selected cells in a `Frame` so every co-viewer sees the highlight;
/// `Frame` gains `scroll_offset` so the client renders the `[+N]` indicator.
///
/// v8 (Phase 6 block navigation): `ClientMsg::BlockJump`/`BlockSelect { pane,
/// dir }` walk the OSC 133 block store server-side (jump the shared scroll,
/// or move the block-scoped selection); `ClientMsg::BlockRerun { pane }`
/// re-sends the selected block's command line, guarded idle. `Command::
/// CopySelection` copies the current selection over the keyboard (the block
/// select -> copy composition). `BlockDir` names the walk direction.
///
/// v9 (blocked-prompt answer queue, x-c929): `AgentRow` gains `answerable`
/// ([`AnswerablePrompt`]) - the daemon's extracted numbered menu riding the
/// existing blocked badge; `ClientMsg::PaneAnswer { pane, fingerprint,
/// region_lines, keystroke }` injects a picked option after the server
/// re-verifies `fingerprint` against its live grid; `err_code` gains
/// `STALE`/`BUSY`.
///
/// v10 (status-row provenance): `Layout` gains `focus_node` - the focused
/// pane's `FNO_NODE` provenance (x-84a8), parsed server-side from the pane-run
/// argv, so the client status row shows `⚑ <node>` config-free. `None` for an
/// ad-hoc pane.
///
/// v11 (work-queue dispatch, x-6f77): a new `DispatchNext` client verb (leader+g
/// "grab work") AND `Layout` gains `backlog: Vec<BacklogCard>` (the sideline
/// work-queue lane) - both wire-shape changes, so the shared counter bumps once.
///
/// v12 (in-scrollback search, x-e780): `ClientMsg::{SearchOpen, SearchStep,
/// SearchClear}` (leader+/ free-text find over a pane's server-side vt history)
/// and the initiator-only reply `ServerMsg::SearchResult { pane_id, total,
/// current }`. `SearchStep` reuses [`BlockDir`] (`Prev` = older match, `Next` =
/// newer). Only match counts + coordinates cross the wire; the 10k-line history
/// never leaves the server. The match jump + highlight reach co-viewers via the
/// shared-scroll `Frame` + `cell_flags::SELECTED` broadcast (v7), so no new
/// frame plumbing.
///
/// v13: `Command::FocusPane(pane_id)` for the sideline click-to-focus path.
/// v14: `Command::AttachAgent(id)` + `AgentRow.attach_id` for the sideline
/// click-to-attach path (a watch-only claude bg row -> `claude attach <id>`).
/// v15: `MouseKind::Move` (1003 any-motion hover reports) drives focus-follows-
/// mouse; `Command::DispatchNode(id)` starts a targeted interactive session from
/// a clicked work-queue card (the confirm path).
/// v16: `Command::NewSquad { name, origin }` - explicit named-workspace
/// creation (the `+` sideline footer, x-9e5e).
/// v17: `Command::RenameTab { tab, name }` - explicit tab rename (leader+,,
/// x-c150); a blank name clears the rename back to the derived label.
/// v18: `BacklogCard.{pane_id, attach_id, where_hint}` - publish-time routes
/// so an in-flight work-queue card focuses/attaches/locates its live session
/// (x-54fa) instead of dead-ending.
/// v19: `Command::{RenameSquad, RemoveSquad, MoveSquad, MoveTab}` - squad
/// management verbs (x-96e8): rename/clear a workspace label, close a whole
/// workspace, reorder the sideline, re-home a tab into another workspace.
/// Parallel-branch hazard: two in-flight mux branches both take "next", so the
/// one that merges second must re-bump (v17/v18 were re-numbered once already).
/// v20: `AgentRow.external` - a sideline row surfaced or liveness-upgraded from
/// claude's daemon roster rather than the fno registry (x-0a2e); renders dim.
/// `#[serde(default)]` keeps an older reader wire-tolerant. (Re-bumped from 19:
/// x-96e8 merged first and took 19 - the second-to-merge re-bump rule.) v21
/// adds `PaneSend { guarded }` for the server-side atomic guarded block-pipe.
/// v22: `TabMeta.panes: Vec<PaneMeta>` - every leaf pane of a tab, labelled, so
/// the session navigator (x-653d) can goto a pane in any tab/squad.
/// v23: `AgentRow.seen` - server-side per-pane seen bit (x-4328), set when the
/// operator focuses a `Done` pane, cleared when it leaves `Done`; distinguishes
/// a looked-at finished agent (`Idle`) from one still surfaced (`DoneUnseen`).
/// v24 (x-0090, agents-first sideline): `AgentRow.tab` - the `TabId` hosting a
/// pane-hosted row, so the sideline renders a tab-ordinal suffix (client derives
/// the ordinal from the Layout); `AgentRow.cwd_base` - the cwd basename of an
/// orphan watch-only row, for the ` (basename)` suffix under `~ elsewhere`. Both
/// `#[serde(default)]`, keeping a v23 reader wire-tolerant. Parallel-branch
/// hazard: if another mux branch takes v24 first, the second-to-merge re-bumps
/// (same rule as the v17/v18/v20 churn).
/// v25 (x-8f11, persisted squads + bulk recruit): `Command::RecruitAgents {
/// squad, ids }` recruits N watch-only agents into a named workspace
/// (create-if-absent); `Command::DismissMember { squad, attach_id }` deletes a
/// tombstoned member from a persisted workspace. `AgentRow.tombstone` marks a
/// synthesized dead-member row (dimmed, dismissable) under its squad; both new
/// `AgentRow` reads are `#[serde(default)]`, keeping a v24 reader wire-tolerant.
/// (v24 was taken by x-0090's `AgentRow.tab`/`cwd_base`, so this re-bumps per
/// the second-to-merge rule the v17/v18/v20 churn established.)
///
/// v26 (x-76ea): `Command::StopAgent { name }` / `Command::RemoveAgent { name }`
/// give the sideline a per-row lifecycle verb (`x` on a live row stops it, on an
/// exited row removes it), server-shelled to `fno-agents stop|rm <name>`. Both
/// validate the name against the current agents catalog server-side and refuse
/// an `external: true` roster row (owned by the claude daemon, not the fno
/// registry) with a notice.
///
/// v27 (x-0333): `Command::ReorderTab { squad, tab, delta }` moves a tab within
/// its client-captured squad while preserving the active tab by stable id.
/// v28 (x-3e38): pane-run and watch-only attach carry an explicit squad target
/// plus optional directional split placement.
pub const PROTO_VERSION: u32 = 28;

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

/// Refuse frames larger than this. A full 500x500 styled grid serializes to a
/// few MB of JSON; 32MB is far above any real frame, low enough that a
/// corrupt length prefix cannot OOM the reader.
pub const MAX_MSG_BYTES: u32 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("message of {0} bytes exceeds the {MAX_MSG_BYTES}-byte cap")]
    TooLarge(u32),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("peer closed the connection")]
    Closed,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Client -> server. Everything here rides the reliable channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMsg {
    /// First message on a fresh connection. `proto`/`build` drive the version
    /// handshake; `rows`/`cols` are the client's CONTENT-AREA viewport (its
    /// terminal minus client-local chrome: sideline panel, tab bar). `cwd` is
    /// the directory the client was launched from - the server resolves it to
    /// a canonical repo root to select or create the squad (squad.rs).
    Attach {
        proto: u32,
        build: String,
        rows: u16,
        cols: u16,
        cwd: String,
    },
    /// Raw keystroke bytes for the focused pane's PTY. Never dropped.
    Input(Vec<u8>),
    /// The client's CONTENT-AREA viewport changed.
    Resize { rows: u16, cols: u16 },
    /// Clean detach: the client is leaving; the server keeps the PTYs.
    Detach,
    /// A layout/tab/squad command from the client's leader-key layer
    /// (keys.rs). Reliable; a refused command comes back as a one-line
    /// notice, never a dropped connection.
    Command(Command),
    /// Sent INSTEAD of `Attach` as the first message on a fresh connection:
    /// ask who this server is (`fno mux ls`). The server answers with one
    /// [`ServerMsg::Info`] and closes; no client is registered.
    ///
    /// Wire shape FROZEN forever: pre-Attach messages bypass the version
    /// handshake (Invariants, Phase 3 plan), so every past and future build
    /// must parse this identically. Changing it means a NEW variant.
    Query,
    /// Sent INSTEAD of `Attach` as the first message on a fresh connection:
    /// shut the session down (`fno mux kill-server`). The server Byes every
    /// client, kills every pane child, and exits 0.
    ///
    /// Wire shape FROZEN forever: pre-Attach, bypasses the version handshake
    /// (Invariants, Phase 3 plan). Changing it means a NEW variant.
    KillServer,
    /// The v4 script-API one-shot control connection (`fno mux pane ...`):
    /// `proto`/`build` drive the SAME version handshake as `Attach` (control
    /// verbs are versioned, unlike the frozen `Query`/`KillServer` pair -
    /// AC4-FR), then `verb` runs and the server answers with exactly one reply
    /// and closes. No `Attach`, no frame stream, no registered client.
    Control {
        proto: u32,
        build: String,
        verb: ControlVerb,
    },
    /// (v7) A mouse event inside a pane's content rect, forwarded for
    /// server-side routing (brief Locked 2). `pane` names the rect the client
    /// hit-tested; `event` is in 0-based pane-local coordinates (the client
    /// maps outer-terminal coords and never sends a chrome click here).
    /// Shift-modified events are the native-selection escape hatch and are
    /// never captured, so they never reach this variant (AC3-EDGE).
    Mouse { pane: u64, event: MouseEvent },
    /// (v8) Walk the pane's OSC 133 command blocks, moving the shared per-pane
    /// scroll so `dir`'s adjacent block anchors at the viewport top. A pane with
    /// no blocks replies with a `Notice` and no scroll change.
    BlockJump { pane: u64, dir: BlockDir },
    /// (v8) Move the block-scoped selection to `dir`'s adjacent block (the whole
    /// command + output span), so the existing copy chain (leader+y) yanks it.
    BlockSelect { pane: u64, dir: BlockDir },
    /// (v8) Re-send the selected (else newest) block's command line to the pane
    /// PTY. Refused unless the pane is known-idle - a rerun injected into a busy
    /// agent corrupts its composer (false-ready is the forbidden direction).
    BlockRerun { pane: u64 },
    /// (v11, x-6f77) "Grab work" (leader+g): dispatch the next ready backlog
    /// node into a new pane in this session. Server-wide (no pane field): the
    /// server shells the Python porcelain off the core loop, and the outcome
    /// (no ready work / lanes full / failure) returns as a one-line `Notice`.
    /// A read-only observer client is refused at the core (mutating_sender).
    DispatchNext,
    /// (v9, x-c929) Answer a blocked prompt from the queue without focusing it.
    /// `keystroke` is the exact bytes the daemon pinned for the chosen option
    /// (never client-fabricated); `fingerprint`/`region_lines` name the region
    /// snapshot the operator read. The server re-reads its live bottom-N grid,
    /// re-hashes, and injects `keystroke` ONLY on a fingerprint match (else a
    /// "prompt changed" notice - fail closed to focus). A pane under a foreign
    /// writer-claim bounces with a "driven by relay" notice. The freshness
    /// re-check is what makes a picked answer safe across the scrape lag.
    PaneAnswer {
        pane: u64,
        fingerprint: [u8; 32],
        region_lines: u16,
        keystroke: Vec<u8>,
    },
    /// (v12, x-e780) Open/refresh a free-text search over the pane's server-side
    /// vt history (leader+/): scan case-insensitively, jump the shared scroll to
    /// the initial match, highlight it via the v7 `SELECTED` broadcast, and store
    /// the match list as a per-pane snapshot. An empty `query` clears (never a
    /// scan that matches every row). The reply is one [`ServerMsg::SearchResult`]
    /// to the initiator; co-viewers get the jump + highlight via the broadcast
    /// `Frame`. History text never crosses the wire.
    SearchOpen { pane: u64, query: String },
    /// (v12, x-e780) Walk the active search's match snapshot: `Prev` toward older,
    /// `Next` toward newer (reusing [`BlockDir`], whose doc semantics match n/N).
    /// Re-jumps + re-highlights and replies a fresh `SearchResult`. A pane with no
    /// active search no-ops with a `Notice`, never a panic.
    SearchStep { pane: u64, dir: BlockDir },
    /// (v12, x-e780) Clear the active search: drop the highlight (selection) and
    /// the per-pane search state, then broadcast a `Frame`. Idempotent: clearing
    /// with nothing active still clears + broadcasts (the client sends this on
    /// every search exit, and a no-match search_open has already dropped the
    /// state server-side).
    SearchClear { pane: u64 },
}

/// A block-navigation walk direction (v8). `Prev` moves toward older blocks,
/// `Next` toward newer / the live tail.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BlockDir {
    Prev,
    Next,
}

/// One mouse event forwarded from a client (v7, brief US1/US2/US3). Coordinates
/// are 0-based and pane-rect-local; the server either SGR-encodes this onto the
/// pane's PTY (the app has mouse reporting on) or interprets it mux-side.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MouseEvent {
    pub row: u16,
    pub col: u16,
    pub kind: MouseKind,
}

/// The mouse gesture. Wheel carries its own intent (there is no separate scroll
/// verb - brief Locked 12); press/drag/release drive focus and selection when
/// the mux interprets, and SGR button codes when it forwards.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseKind {
    Press(MouseButton),
    Release(MouseButton),
    /// Motion with a button held (mouse-drag; drives selection mux-side).
    Drag(MouseButton),
    /// (v15) Motion with NO button held (1003 any-motion tracking): hover.
    /// Client-local - drives focus-follows-mouse and sideline highlight; never
    /// forwarded to a pane PTY.
    Move,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PaneTarget {
    #[default]
    CurrentRoute,
    SquadName(String),
    SquadId(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PanePlacement {
    #[serde(default)]
    pub target: PaneTarget,
    #[serde(default)]
    pub split: Option<Dir>,
}

/// The script-API verbs (`fno mux pane ls|read|run|send|wait|kill`), each a
/// one-shot request answered by exactly one [`ServerMsg`] reply. Versioned as
/// part of `Control` (v4); a new verb or a shape change bumps `PROTO_VERSION`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ControlVerb {
    /// Every pane across every squad -> [`ServerMsg::PaneList`].
    PaneLs,
    /// One pane's text -> [`ServerMsg::PaneText`]. Without `block`, `lines`
    /// selects the last N logical rows and (v6) reaches into scrollback history
    /// (full visible grid when `None`; AC5-UI keeps the no-flag behavior). With
    /// `block` set, returns that OSC 133 command block's output span instead
    /// (v6); `lines` is ignored in block mode.
    PaneRead {
        pane: u64,
        lines: Option<u16>,
        #[serde(default)]
        block: Option<BlockSel>,
    },
    /// Spawn `argv` as a new pane in the squad `cwd` resolves to (created if
    /// absent) -> [`ServerMsg::PaneSpawned`]. The agents-spawn-agents
    /// primitive. `cols`/`rows` default to the VT defaults when `None`.
    /// `claim: true` (v5) marks the pane writer-claim ELIGIBLE (an agent pane:
    /// the relay may hold its input around an injection burst); general panes
    /// pass `false` and never consult a claim (brief Locked 5).
    PaneRun {
        cwd: String,
        argv: Vec<String>,
        cols: Option<u16>,
        rows: Option<u16>,
        #[serde(default)]
        claim: bool,
        #[serde(default)]
        placement: PanePlacement,
    },
    /// Write raw bytes to a pane's PTY (no focus change) -> [`ServerMsg::Ok`].
    /// `guarded` (v21) makes the send atomic against the target going busy:
    /// the server evaluates the same idle authority as the block-rerun guard
    /// (idle badge, then the writer-claim interlock) under the core-loop pane
    /// lock immediately before the write, bouncing `busy: relay` with
    /// [`err_code::TARGET_NOT_IDLE`] instead of injecting into an in-flight
    /// write. Default `false` keeps raw `PaneSend` (the claim holder's own
    /// channel, `fno mux pane send`) unguarded.
    PaneSend {
        pane: u64,
        bytes: Vec<u8>,
        #[serde(default)]
        guarded: bool,
    },
    /// Block until the pane's output settles (`quiet_ms` with no new output),
    /// matches `pattern` (regex over the visible grid), the child exits, or
    /// `timeout_ms` elapses -> [`ServerMsg::WaitDone`]. The deadline is
    /// ALWAYS bounded (no infinite wait); the outcome distinguishes which.
    PaneWait {
        pane: u64,
        quiet_ms: Option<u64>,
        pattern: Option<String>,
        timeout_ms: u64,
        /// (v6) Also resolve on the pane's next OSC 133 `D` (command done),
        /// yielding [`WaitOutcome::CommandDone`] with the command's exit code.
        /// Always bounded by `timeout_ms`; a markerless pane (no shell-init)
        /// simply times out (the CLI flags the degradation in `--json`), never
        /// an infinite wait (AC3-FR).
        #[serde(default)]
        command_done: bool,
    },
    /// Close a pane by id (the `ClosePane` cascade) -> [`ServerMsg::Ok`].
    PaneKill { pane: u64 },
    /// Acquire the writer claim on a claim-eligible pane (v5) ->
    /// [`ServerMsg::Ok`] / [`ServerMsg::Err`]. While held, human `Input` to
    /// the pane bounces with BEL + a `busy: relay` notice; `PaneSend` (the
    /// holder's own channel) still lands. `holder_pid` anchors the off-loop
    /// PID-liveness release (a dead holder frees the pane without a server
    /// restart - AC3-FR). Refused on a general (non-eligible) pane and when
    /// another live holder has it.
    PaneClaim { pane: u64, holder_pid: u32 },
    /// Release the writer claim (v5) -> [`ServerMsg::Ok`]. Idempotent: no
    /// claim held is still Ok (the burst may have raced a pane exit, which
    /// releases unconditionally).
    PaneRelease { pane: u64 },
}

/// One sideline agent row inside [`ServerMsg::Layout`] (v5, brief US2). The
/// server's off-loop registry reader joins registry rows to panes via the
/// `mux` ref and derives the 3-tier fact-badge lattice: `exited` (pane-exit
/// fact, beats everything) > `badge` (in-TTL inside-leg report) > liveness
/// (both `None`/`false` - a plain row). `squad` is the squad the row renders
/// under; `None` is the catch-all for rows whose cwd matches no squad.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRow {
    pub squad: Option<u64>,
    pub name: String,
    /// The mux pane hosting this agent in THIS session; `None` = a watch-only
    /// row (bg/headless/daemon-worker agents surfaced from the registry).
    pub pane_id: Option<u64>,
    /// In-TTL inside-leg badge; `None` = liveness-only (never a scraped guess).
    pub badge: Option<AgentBadge>,
    /// The report's human reason, when badged.
    pub reason: Option<String>,
    /// Pane-exit / registry-exited fact: renders dim + exit marker regardless
    /// of any live-TTL badge (fact beats report, structurally).
    pub exited: bool,
    /// (v9, x-c929) The answerable-prompt payload when this row is `blocked` on
    /// a numbered menu a manifest `[rule.answer]` grammar could enumerate;
    /// `None` for any other state or a focus-only blocked prompt. A structural
    /// twin of the daemon's `AnswerablePrompt` (the crates share no types - the
    /// registry JSON is the contract), carried onto the badge for the client's
    /// answer overlay. `#[serde(default)]` keeps a v8 reader wire-tolerant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answerable: Option<AnswerablePrompt>,
    /// (v14) The `claude attach <id>` target for a watch-only row: the claude
    /// bg jobId. Present only when `pane_id` is `None` (a paneless bg/headless
    /// claude row) and the registry recorded a jobId; lets a sideline click
    /// attach the detached session into a fresh mux pane instead of dead-ending
    /// on a notice. `None` for a pane-hosted row (it focuses its pane) or a
    /// non-attachable row. `#[serde(default)]` keeps a v13 reader wire-tolerant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_id: Option<String>,
    /// (v20, x-0a2e) True when this row's provenance is claude's daemon roster
    /// (a synthesized foreign session or a roster-liveness-upgraded registry
    /// row) rather than the fno registry: rendered dim, read-only toward the
    /// claude daemon roster. NOT an attachability signal (that is
    /// `attach_id.is_some()`); an external row whose pane died is still
    /// `external: true` but exited. `#[serde(default)]` keeps a v19 reader
    /// wire-tolerant (defaults false).
    #[serde(default)]
    pub external: bool,
    /// (v23, x-4328) True once the operator has focused this pane while it was
    /// `Done` (server-side `Core.seen`, keyed by `pane_id`; a watch-only row
    /// with no `pane_id` is always false - it cannot be focused). Consumed at
    /// the client's `pane_state` fold to distinguish a seen-Done (`Idle`) from
    /// an unseen-Done (`DoneUnseen`, surfaced). `#[serde(default)]` keeps a
    /// v22 reader wire-tolerant (defaults false, degrading to the
    /// pre-feature `done == unseen`).
    #[serde(default)]
    pub seen: bool,
    /// (v24, x-0090) The tab hosting this row's pane, for the agents-first
    /// sideline: the client derives the display ordinal from the `Layout` it
    /// already holds (a `TabId`, not a precomputed ordinal, so a closed tab
    /// recomputes correctly - Discretion 2). `Some` only on a pane-hosted row
    /// (`pane_id: Some`); a watch-only row has no tab. `#[serde(default)]`
    /// keeps a v23 reader wire-tolerant (`None` -> no ordinal suffix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<TabId>,
    /// (v24, x-0090) The cwd basename of an ORPHAN watch-only row (matched no
    /// squad), rendered as a ` (basename)` suffix under the `~ elsewhere`
    /// header so two same-named workers in different repos are distinguishable.
    /// The client can't derive it (an orphan matches no squad, so its cwd is
    /// not among the `Layout`'s squads), hence the wire carries it. `None` for
    /// any pane-hosted or squad-matched row. `#[serde(default)]` keeps a v23
    /// reader wire-tolerant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_base: Option<String>,
    /// (v25, x-8f11) True for a SYNTHESIZED dead-member row: a tombstoned
    /// persisted member with no live pane, rendered dimmed under its squad and
    /// dismissable (`DismissMember`). Distinguishes it from a squad-header row
    /// (`x` removes the squad) so the same `x` key dismisses a tombstone but
    /// removes a squad, disambiguated by row type. `#[serde(default)]` keeps a
    /// v24 reader wire-tolerant (defaults false).
    #[serde(default)]
    pub tombstone: bool,
}

/// (v11, x-6f77) One work-queue card for the sideline backlog lane, derived
/// read-only from `~/.fno/graph.json` (backlog_view). The mux needs four fields
/// per node, not the whole graph model; the FILE is the contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BacklogCard {
    pub id: String,
    pub slug: String,
    /// `p0`..`p3` (the raw string; the sideline shows it verbatim).
    pub priority: String,
    pub state: CardState,
    /// (v18) The pane in THIS session working the node (`FNO_NODE` provenance
    /// equality), when in flight. A click focuses it instead of dead-ending.
    /// Route priority is pane > attach > `where_hint`; at most one is acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// (v18) The `claude attach <id>` jobId of the paneless bg session working
    /// the node - the same target the agents-row click would use (v14 gate
    /// unchanged). Present only when no pane in this session has the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_id: Option<String>,
    /// (v18) One-line locator for an in-flight card with no route here (e.g.
    /// `in flight - worked by <holder>`), shown as the click notice instead of
    /// a bare "already dispatching". `None` when a route exists or nothing is
    /// known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub where_hint: Option<String>,
}

/// The queue state a card renders as. Classified from `_status` alone
/// (backlog_view::classify): a claimed node with a stale `blocked_by` is
/// in-flight, not blocked.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CardState {
    Ready,
    Blocked,
    InFlight,
}

/// One selectable option of an [`AnswerablePrompt`] (v9). Structural twin of the
/// daemon's `manifest::AnswerOption`; the registry/wire JSON is the contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerOption {
    /// The captured menu index the operator presses, e.g. "1".
    pub idx: String,
    /// The display label (untruncated; the client truncates for width).
    pub label: String,
    /// The exact PTY bytes to inject, pinned by the daemon's manifest `send`
    /// mapping - the client relays these opaquely and never fabricates bytes.
    pub keystroke: Vec<u8>,
}

/// A blocked prompt the operator can answer from the queue without focusing the
/// pane (v9, x-c929). Extracted by the daemon, carried on the badge; the client
/// renders `prompt` + `options` in the answer overlay and, on a pick, sends
/// [`ClientMsg::PaneAnswer`] with `fingerprint`/`region_lines`/the option's
/// `keystroke`, which the server re-verifies against its live grid before
/// injecting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerablePrompt {
    /// The lines above the first option, display-only.
    pub prompt: String,
    pub options: Vec<AnswerOption>,
    /// blake3 of the region text the daemon read; the server's freshness key.
    pub fingerprint: [u8; 32],
    /// The N of `bottom_non_empty_lines(N)`, so the server re-reads the same
    /// region window to re-hash.
    pub region_lines: usize,
}

/// The inside-leg badge vocabulary (contract v2), as rendered in the sideline.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentBadge {
    Working,
    Blocked,
    Done,
}

/// Layout mutations the client can request. Interpreted (leader-key table)
/// client-side; executed on the server's core loop, which owns the tree.
/// `SelectTab` names a stable [`TabMeta::id`] from the last `Layout`'s
/// catalog; `SelectSquad` names a squad id from the same catalog - the
/// server rejects stale values fail-closed (BEL + notice), so a client racing
/// a layout change can never corrupt state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Command {
    SplitH,
    SplitV,
    ClosePane,
    FocusDir(Dir),
    ResizeDir(Dir),
    NewTab,
    SelectTab(TabId),
    NextTab,
    PrevTab,
    CloseTab,
    SelectSquad(u64),
    /// (v8) Copy the focused pane's current selection over the keyboard (the
    /// block-select -> leader+y composition; the mouse path copies on release).
    /// A no-op `Notice` when nothing is selected.
    CopySelection,
    /// (v13) Focus a pane by id, wherever it lives: the server scans every
    /// squad/tab for the leaf, switches the sender's view to that squad+tab, and
    /// sets the tab focus to it. Names a `pane_id` from the last `Layout`'s
    /// agent rows (the sideline click path); a stale id is refused fail-closed
    /// with a notice, like the other catalog-named commands.
    FocusPane(u64),
    /// (v14) Attach a watch-only claude bg session into a fresh mux pane by its
    /// jobId (`AgentRow.attach_id`): the server spawns `claude attach <id>` as a
    /// new tab in the sender's squad and switches the sender to it. The id is
    /// validated (8 hex digits) before it reaches the argv - a malformed id is
    /// refused fail-closed with a notice, and the argv is never a shell string,
    /// so the value can only ever be `claude attach`'s positional arg.
    AttachAgent {
        id: String,
        #[serde(default)]
        placement: PanePlacement,
    },
    /// (v15) Start a targeted interactive session on a clicked work-queue card's
    /// node (id or slug), behind the client's one-keypress confirm. Reuses the
    /// `DispatchNext` porcelain (`fno dispatch one`) pinned to `--node`, so the
    /// lane cap, the same-node claim race (a node claimed between click and Enter
    /// bounces `already-dispatching`), and the "read-only observer refused"
    /// guarantee all hold exactly as leader+g. Value over `DispatchNext`: the
    /// operator picks WHICH card, not just "next".
    DispatchNode(String),
    /// (v16) Create a NAMED squad (a workspace) explicitly, bypassing the
    /// attach cwd-resolution path entirely (Unit 2). The server rejects a
    /// blank/whitespace-only `name` with a notice (nothing created); otherwise
    /// it seeds a squad named `name` with `origins = origin.into_iter().collect()`
    /// and one shell tab rooted at `origin` (or the creating client's squad
    /// cwd), then switches the sender's view onto it. The `+` sideline button
    /// sends this after the name-input overlay.
    NewSquad {
        name: String,
        origin: Option<String>,
    },
    /// (v17) Rename a tab (x-c150). `tab` names a stable [`TabMeta::id`] from
    /// the last `Layout`'s catalog (a stale id is refused fail-closed with a
    /// notice, like `SelectTab`). The server sanitizes `name` (strip control
    /// chars, trim, cap [`MAX_TAB_NAME`]) before storing; a blank/whitespace
    /// name CLEARS the explicit rename and reverts to the derived label -
    /// deliberately unlike `NewSquad`'s blank-refusal, because "reset to
    /// auto" is a meaningful rename target.
    RenameTab {
        tab: TabId,
        name: String,
    },
    /// (v19, x-96e8) Rename a squad/workspace. `squad` names a stable squad id
    /// from the last `Layout` catalog (a stale id is refused fail-closed with a
    /// notice, like `SelectSquad`). The server sanitizes `name` (strip control
    /// chars, trim, cap [`MAX_SQUAD_NAME`]); a blank name CLEARS the explicit
    /// name back to the derived `origins` label (the `RenameTab` precedent) -
    /// EXCEPT for an origin-less squad (a `NewSquad` workspace), whose derived
    /// label would be empty: there a blank is refused with `name required`.
    RenameSquad {
        squad: u64,
        name: String,
    },
    /// (v19, x-96e8) Close a whole workspace: reap every pane across all its
    /// tabs, remove the squad, re-anchor views. Removing the LAST squad ends
    /// the session (`Bye`), identical to closing its tabs one at a time
    /// (Locked Decision 8). Destructiveness is gated CLIENT-side by a confirm,
    /// not refused server-side (every live squad always has an occupied tab, so
    /// "refuse while occupied" would be a dead verb). A stale/unknown id is
    /// refused with a notice.
    RemoveSquad(u64),
    /// (v19, x-96e8) Reorder the sideline: move the squad `delta` positions in
    /// `Session::squads` (the sideline order). The target index is clamped to
    /// the list bounds; an already-at-edge move is a silent no-op (holding a
    /// reorder key at the top must not bell). An unknown id is refused with a
    /// notice. Pure presentation reorder: no ids, views, or active_tab change.
    MoveSquad {
        squad: u64,
        delta: i32,
    },
    /// (v19, x-96e8) Re-home a whole tab (and every pane in it) into another
    /// squad. `tab` names a `TabMeta::id`, `squad` a destination squad id, both
    /// from the last `Layout` catalog. An unknown tab, unknown dst, or dst ==
    /// src is refused fail-closed with a notice. Panes are NOT reaped (they
    /// move with the tab); if the source squad's last tab moves out, the now
    /// empty squad is removed (its panes already gone) and views re-anchor.
    MoveTab {
        tab: TabId,
        squad: u64,
    },
    /// (v27, x-0333) Reorder a tab within its current squad. The target index is
    /// clamped to the sibling bounds; an already-at-edge move is a silent no-op.
    /// The server remaps `active_tab` so the same stable tab remains active. A
    /// stale/unknown tab id is refused with a notice.
    ReorderTab {
        squad: u64,
        tab: TabId,
        delta: i32,
    },
    /// (v25, x-8f11) Recruit N watch-only agents into a NAMED workspace
    /// (create-if-absent, no origin). `squad` is a workspace name; `ids` are
    /// `claude attach` jobIds from the sideline marks. The server is the
    /// authoritative gate: a blank name or empty `ids` is refused fail-closed;
    /// each id is validated through the exact `AttachAgent` gates (8-hex shape +
    /// catalog membership); an id already paned or already a member is a dedup
    /// no-op; per-id outcomes fold into one partial-success notice. Members are
    /// written through to `~/.fno/squads.json`.
    RecruitAgents {
        squad: String,
        ids: Vec<String>,
    },
    /// (v25, x-8f11) Delete a TOMBSTONED member (a dead worker's dimmed row)
    /// from a persisted workspace. `squad` names a stable squad id; `attach_id`
    /// the member. Only a tombstone is dismissable (a live member leaves by
    /// closing its pane); an unknown workspace/member is refused with a notice.
    /// Write-through delete.
    DismissMember {
        squad: u64,
        attach_id: String,
    },
    /// (v26, x-76ea) Stop a live agent row from the sideline: the server shells
    /// `fno-agents stop <name>` (idempotent - already-exited is a clean no-op).
    /// `name` is validated against the current agents catalog server-side; a
    /// stale name is refused fail-closed with a notice, like `FocusPane`. An
    /// `external: true` roster row is refused (the claude daemon owns it, not the
    /// fno registry). The row's exited flag flips on the next registry poll.
    StopAgent {
        name: String,
    },
    /// (v26, x-76ea) Remove an EXITED agent row: the server shells `fno-agents
    /// rm <name>`. Refused with a notice when the named row is still live
    /// (stop-then-rm ordering, mirrored by the CLI's own live-row refusal) or
    /// `external`. Same catalog validation as `StopAgent`; the row vanishes on
    /// the next registry poll.
    RemoveAgent {
        name: String,
    },
}

impl Command {
    pub fn attach_agent(id: impl Into<String>) -> Self {
        Self::AttachAgent {
            id: id.into(),
            placement: PanePlacement::default(),
        }
    }
}

/// Server -> client.
///
/// Channel discipline (Locked Decision 4): `Layout`/`ModeSync`/`Bye` ride the
/// per-client RELIABLE channel (awaited, never dropped - a dropped Layout is
/// a protocol bug, not a degraded mode); only pane-tagged self-contained
/// `Frame`s are droppable (per-(client, pane) newest-wins). v1's `Cursor`
/// variant is gone: it was never sent (the cursor rides inside `Frame`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMsg {
    /// A self-contained render frame (full grid + cursor) for ONE pane.
    /// Droppable: the server keeps only the newest unsent frame per
    /// (client, pane), so a flooded pane coalesces without starving its
    /// siblings. `pane_id` lives on the variant, not in [`Frame`]: the VT
    /// grid (`vt::Pane`) does not know its mux pane id - the server's pane
    /// registry tags the frame at send time.
    Frame { pane_id: u64, frame: Frame },
    /// The squad/tab catalog + computed rects for the receiving client's
    /// viewed tab, relative to the CONTENT AREA. The server sends rects,
    /// never the tree; the client never runs the layout algorithm. Reliable.
    /// `area` is the clamped (rows, cols) the rects were computed for
    /// (view-scoped smallest-client clamp); a client larger than `area`
    /// letterboxes client-side without inferring the bound from the rects.
    Layout {
        squads: Vec<SquadMeta>,
        active_squad: u64,
        panes: Vec<(u64, Rect)>,
        focus: u64,
        area: (u16, u16),
        /// Sideline agent rows (v5): registry-derived, fact-badged. Empty for
        /// a session with no known agents.
        #[serde(default)]
        agents: Vec<AgentRow>,
        /// (v10) The focused pane's `FNO_NODE` provenance, parsed server-side
        /// from the pane-run argv (x-84a8). `None` for an ad-hoc pane; carried
        /// on `Layout` (not `Frame`) because it changes only on focus/structure
        /// changes, mirroring how `SquadMeta::canonical_cwd` reaches the client.
        #[serde(default)]
        focus_node: Option<String>,
        /// (v11, x-6f77) Board-ordered work-queue cards for the sideline backlog
        /// lane (backlog_view). Empty when the graph is unreadable or has no
        /// ready/blocked/in-flight work; `#[serde(default)]` keeps a v10 reader
        /// wire-tolerant.
        #[serde(default)]
        backlog: Vec<BacklogCard>,
    },
    /// Escape bytes syncing the client terminal to the newly focused pane's
    /// negotiated modes (bracketed paste, mouse reporting, DECCKM, ...).
    /// Applied verbatim to the client TTY. Reliable, and ordered BEFORE the
    /// `Layout`/frames that assume those modes.
    ModeSync { bytes: Vec<u8> },
    /// A one-line human-facing notice (refused command, failed split, ...)
    /// the client renders as transient feedback + BEL. Reliable.
    Notice { text: String },
    /// The server is refusing or ending this connection; `reason` is
    /// human-facing (version skew, shutdown, session ended, ...).
    Bye { reason: String },
    /// The answer to a pre-Attach [`ClientMsg::Query`] (`fno mux ls`).
    ///
    /// Wire shape FROZEN forever: pre-Attach traffic bypasses the version
    /// handshake (Invariants, Phase 3 plan). Changing it means a NEW variant.
    Info {
        session: String,
        clients: u32,
        squads: u32,
        panes: u32,
    },
    // -- v4 control-verb replies (one per Control connection, then close) --
    /// Answer to [`ControlVerb::PaneLs`].
    PaneList { panes: Vec<PaneInfo> },
    /// Answer to [`ControlVerb::PaneRead`]: the pane's text (matches
    /// [`crate::vt::frame_text`]). `block` (v6) carries the command-block
    /// metadata when the request selected a block; `None` for a plain grid/
    /// history read. `#[serde(default)]` so a plain read stays wire-stable.
    PaneText {
        pane_id: u64,
        text: String,
        #[serde(default)]
        block: Option<BlockMeta>,
    },
    /// Answer to [`ControlVerb::PaneRun`]: the fresh pane's id, machine-read
    /// by the CLI so scripts compose.
    PaneSpawned { pane_id: u64 },
    /// A verb that carries no payload succeeded (`PaneSend`, `PaneKill`).
    Ok,
    /// Answer to [`ControlVerb::PaneWait`].
    WaitDone { outcome: WaitOutcome },
    /// A control verb failed (dead pane, spawn failure, version skew, ...).
    /// `code` is one of [`err_code`]; `msg` is one human line.
    Err { code: u32, msg: String },
    /// (v7) Extracted selection text destined for the client's clipboard chain
    /// (brief Locked 5). Copy extraction happens server-side (history lives
    /// there); the client execs its local clipboard tool, else emits OSC 52,
    /// else reports the failure visibly. Reliable - a dropped copy is silent
    /// data loss, never acceptable.
    Copy { text: String },
    /// (v12, x-e780) The initiator-only result of a `SearchOpen`/`SearchStep`:
    /// `total` matches in the snapshot and the `current` 1-based position after
    /// the jump. `total == 0` means no matches (the client shows "no matches" +
    /// BEL and the viewport did not move). Co-viewers never receive this - they
    /// see only the shared jump + highlight via the broadcast `Frame`, not the
    /// `[i/n]` counter chrome. Reliable.
    SearchResult {
        pane_id: u64,
        total: u32,
        current: u32,
    },
}

/// One pane's metadata in a [`ServerMsg::PaneList`]. `cwd` is the squad's
/// canonical root; `child_pid` is `None` only if the OS never reported one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneInfo {
    pub pane_id: u64,
    pub squad_id: u64,
    pub tab_id: u64,
    pub cwd: String,
    pub child_pid: Option<u32>,
    pub title: Option<String>,
}

/// Why a [`ControlVerb::PaneWait`] returned. The CLI maps each to a distinct
/// exit code (AC4-EDGE: timeout is tellable apart from a match and a settle).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WaitOutcome {
    /// No new output for `quiet_ms`.
    Quiet,
    /// The `pattern` matched the visible grid.
    Matched,
    /// `timeout_ms` elapsed with neither condition met.
    Timeout,
    /// The pane's child exited (or the pane was closed) while waiting.
    PaneExited,
    /// (v6) An OSC 133 `D` marker fired (the running command finished);
    /// `exit` is its reported exit code when the shell emitted one.
    CommandDone { exit: Option<i32> },
}

/// Which OSC 133 command block a [`ControlVerb::PaneRead`] selects (v6). Defined
/// here (not in [`crate::vt`]) so the wire and the VT store share one type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BlockSel {
    /// The most recent block: the open one if a command is still running, else
    /// the last completed block, else the implicit whole-output block on a
    /// pane that never emitted markers.
    Last,
    /// A specific monotonic per-pane block sequence.
    Seq(u64),
}

/// Metadata for a command block in a [`ServerMsg::PaneText`] reply (v6). The
/// degradation flags are VISIBLE, never silent: `implicit` = markerless pane's
/// whole-output fallback; `complete=false` = still streaming (no `D` yet);
/// `truncated` = the span's top scrolled out of the window.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockMeta {
    /// `None` for the implicit whole-output block of a markerless pane.
    pub seq: Option<u64>,
    pub exit: Option<i32>,
    pub complete: bool,
    pub truncated: bool,
    pub implicit: bool,
}

/// `ServerMsg::Err` codes. One namespace so the CLI's exit-code mapping and
/// the server's error construction never drift.
pub mod err_code {
    /// A pane id that no live pane owns (read/send/wait/kill).
    pub const DEAD_PANE: u32 = 1;
    /// A control connection whose `proto` disagrees with the server (AC4-FR).
    pub const VERSION_SKEW: u32 = 2;
    /// `PaneRun` could not spawn the child (no PTY, argv not executable).
    pub const SPAWN_FAILED: u32 = 3;
    /// A malformed request the server could parse but not act on.
    pub const BAD_REQUEST: u32 = 4;
    /// (v6) A block read that cannot be answered: an evicted or nonexistent
    /// block, or a specific `seq` requested on a markerless pane.
    pub const BLOCK_UNAVAILABLE: u32 = 5;
    /// (v21) A guarded `PaneSend` refused: the target pane is not provably idle
    /// (busy/blocked agent) or a live relay holds its writer claim. The bytes
    /// did not land; the caller retries or overrides with `--force`.
    pub const TARGET_NOT_IDLE: u32 = 6;
}

/// One pane inside a [`TabMeta`] (v22, x-653d): the leaf id the session
/// navigator's goto targets plus a derived, display-only `label` (the running
/// command / node / cwd basename, else `shell`). The client never focuses a
/// pane by label - it sends `FocusPane(id)`; the label is filter/display text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneMeta {
    pub id: u64,
    pub label: String,
}

/// One tab's catalog entry inside [`ServerMsg::Layout`]. `id` is the stable
/// session-scoped tab identity (monotonic u64, never reused - Locked
/// Decision 6 extended to tabs); `Command::SelectTab` names it, so a
/// selection can never race a catalog change onto the wrong tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabMeta {
    pub id: u64,
    pub name: String,
    /// (v22, x-653d) Every leaf pane of this tab, so the session navigator can
    /// list and goto panes across tabs/squads (the sideline only ever tiled the
    /// active view's panes). `#[serde(default)]` keeps a v21 reader wire-tolerant
    /// (empty -> the navigator simply lists no plain panes for the tab).
    #[serde(default)]
    pub panes: Vec<PaneMeta>,
}

/// One squad's catalog entry inside [`ServerMsg::Layout`]. Identity is the
/// server-scoped `id` (monotonic, never reused); `canonical_cwd` is the
/// resolved repo root the squad is keyed by; `name` is display-only (the
/// root's basename, disambiguated by a parent segment when needed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SquadMeta {
    pub id: u64,
    pub name: String,
    pub canonical_cwd: String,
    pub tabs: Vec<TabMeta>,
    pub active_tab: usize,
    /// (v19, x-96e8) Total live pane count across ALL the squad's tabs - the
    /// blast radius the `RemoveSquad` confirm names before it reaps them.
    /// Display-only; a slightly stale count only skews the prompt, and the
    /// server reaps whatever is actually live at commit.
    pub panes: usize,
}

/// A complete rendered screen: `rows * cols` cells in row-major order plus the
/// cursor. Self-contained by construction - drawing a `Frame` never requires
/// any earlier message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Frame {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Cell>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    /// (v7) Lines this pane is scrolled above the live bottom; 0 = live. The
    /// client renders a minimal `[+N]` indicator when non-zero (US1, AC1-UI);
    /// broadcast in the frame so every co-viewer shows the same indicator.
    #[serde(default)]
    pub scroll_offset: u16,
}

impl Frame {
    /// The load-bearing invariant: exactly `rows * cols` cells. Serde cannot
    /// enforce it (a short `cells` deserializes cleanly), so the compositor
    /// checks this at the trust boundary before doing any slice math - a
    /// mismatched frame is treated like a malformed message, never drawn.
    pub fn geometry_ok(&self) -> bool {
        self.cells.len() == self.rows as usize * self.cols as usize
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        }
    }
}

/// Style bits carried per cell (`Cell::flags`).
pub mod cell_flags {
    pub const BOLD: u8 = 1 << 0;
    pub const ITALIC: u8 = 1 << 1;
    pub const UNDERLINE: u8 = 1 << 2;
    pub const INVERSE: u8 = 1 << 3;
    pub const DIM: u8 = 1 << 4;
    /// The second cell of a wide (CJK/emoji) glyph. Compositors skip it so
    /// the glyph's right half is never overdrawn.
    pub const WIDE_SPACER: u8 = 1 << 5;
    /// (v7) Part of the active selection. The server renders the flag into the
    /// broadcast frame so every co-viewer sees the same highlight (brief
    /// Locked 4); the compositor draws it as an inverse/tinted cell.
    pub const SELECTED: u8 = 1 << 6;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The version-handshake decision, factored pure so it is unit-testable. On a
/// mismatch the message names BOTH versions and how to recover, because the
/// operator seeing it is mid-upgrade and the server is the stale side.
pub fn check_attach_version(client_proto: u32, client_build: &str) -> Result<(), String> {
    if client_proto == PROTO_VERSION {
        return Ok(());
    }
    Err(format!(
        "protocol version mismatch: client {client_build} speaks v{client_proto}, \
         server {BUILD_VERSION} speaks v{PROTO_VERSION}. The running server predates \
         your fno upgrade - stop it (it keeps running across upgrades by design) \
         and re-run fno to start a fresh one."
    ))
}

// ---------------------------------------------------------------------------
// Codec: u32-BE length prefix + JSON body
// ---------------------------------------------------------------------------

/// Encode one message with its length prefix.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len()).map_err(|_| ProtoError::TooLarge(u32::MAX))?;
    if len > MAX_MSG_BYTES {
        return Err(ProtoError::TooLarge(len));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

/// Decode a length-checked body. `Err(Malformed)` on any parse failure - the
/// caller must close the connection loudly, never act on a half-frame.
fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtoError> {
    Ok(serde_json::from_slice(body)?)
}

fn check_len(len: u32) -> Result<usize, ProtoError> {
    if len > MAX_MSG_BYTES {
        return Err(ProtoError::TooLarge(len));
    }
    Ok(len as usize)
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), ProtoError>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    let buf = encode(msg)?;
    w.write_all(&buf).await?;
    Ok(())
}

pub async fn read_msg<R, T>(r: &mut R) -> Result<T, ProtoError>
where
    R: tokio::io::AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ProtoError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = check_len(u32::from_be_bytes(len_buf))?;
    let mut body = vec![0u8; len];
    match r.read_exact(&mut body).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ProtoError::Closed),
        Err(e) => return Err(e.into()),
    }
    decode_body(&body)
}

/// Sync twin of [`write_msg`] for plain `std` streams (tests, simple tools).
pub fn write_msg_sync<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), ProtoError> {
    let buf = encode(msg)?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Sync twin of [`read_msg`].
pub fn read_msg_sync<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, ProtoError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ProtoError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = check_len(u32::from_be_bytes(len_buf))?;
    let mut body = vec![0u8; len];
    match r.read_exact(&mut body) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ProtoError::Closed),
        Err(e) => return Err(e.into()),
    }
    decode_body(&body)
}

// ---------------------------------------------------------------------------
// Socket lifecycle
// ---------------------------------------------------------------------------

/// The mux socket directory: `$FNO_MUX_DIR` when set (tests point this at a
/// tempdir), else `~/.fno/mux`.
pub fn mux_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FNO_MUX_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".fno").join("mux")
}

/// Create `dir` (and parents) born 0700, then force 0700 on a pre-existing
/// one. `DirBuilder::mode` makes fresh directories private atomically -
/// `create_dir_all` + `set_permissions` leaves a window where the dir exists
/// with umask-loosened permissions (gemini security-medium). The follow-up
/// `set_permissions` only tightens a dir that already existed.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Create the mux dir if needed and force 0700 either way: the sockets in it
/// accept keystrokes into your shell, so group/world access is never OK.
pub fn ensure_mux_dir() -> std::io::Result<PathBuf> {
    let dir = mux_dir();
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Socket path for a session name. Path separators are rejected rather than
/// sanitized so a session name can never escape the 0700 mux dir.
pub fn socket_path(session: &str) -> Result<PathBuf, String> {
    if session.is_empty() || session.contains('/') || session.contains('\0') {
        return Err(format!("invalid session name: {session:?}"));
    }
    Ok(mux_dir().join(format!("{session}.sock")))
}

/// The wire-version sidecar for a session socket (`<name>.sock` -> `<name>.ver`),
/// written by the server at startup with its [`PROTO_VERSION`] (x-1a85). It lets
/// `fno mux ls` tell a stale-wire server (predating the installed binary, so a
/// new client's handshake would be rejected) from a healthy same-version one -
/// a distinction the FROZEN, version-agnostic `Query`/`Info` probe cannot make.
/// A `.ver` file is skipped by [`session_names`]'s `.sock` filter, so it never
/// reads as a session.
pub fn version_sidecar_path(socket: &Path) -> PathBuf {
    socket.with_extension("ver")
}

pub const DEFAULT_SESSION: &str = "main";

/// Outcome of [`bind_or_probe`].
pub enum BindOutcome {
    /// We own the socket: this process should run the server.
    Bound(UnixListener),
    /// A live server already owns it: attach instead.
    AlreadyRunning,
}

/// Bind the session socket, treating the bind itself as the lock.
///
/// - Fresh path: bind wins atomically.
/// - `AddrInUse`: connect-probe. A successful connect means a live server
///   (`AlreadyRunning`). Refused/failed connects (retried briefly, so a server
///   between its bind and listen syscalls is not misread as dead) mean a stale
///   socket from a dead server: unlink it and bind again.
///
/// ponytail: unlink-then-rebind has a tiny two-racers-over-a-stale-socket
/// window (both probe dead, both unlink+bind; the second unlink can orphan the
/// first winner's socket). The plan locks "no lockfile - bind is the lock";
/// the cold-start race (AC4-EDGE, no stale socket) is fully atomic, and the
/// stale+simultaneous case needs a dead server AND a photo-finish start. If it
/// ever bites, the upgrade is an O_EXCL sidecar lock around the unlink.
pub fn bind_or_probe(path: &Path) -> std::io::Result<BindOutcome> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(BindOutcome::Bound(l)),
        Err(e) if socket_in_use(&e) => {
            if probe_alive(path) {
                return Ok(BindOutcome::AlreadyRunning);
            }
            // Stale socket from a dead server: take the name over.
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            match UnixListener::bind(path) {
                Ok(l) => Ok(BindOutcome::Bound(l)),
                Err(e) if socket_in_use(&e) => {
                    // Someone else won the rebind race; they are the server.
                    Ok(BindOutcome::AlreadyRunning)
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Bind failed because the path is taken. Linux reports `EADDRINUSE`
/// (`AddrInUse`); macOS reports `EEXIST` (`AlreadyExists`) when the socket
/// file already exists. Both mean the same thing here.
fn socket_in_use(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists
    )
}

/// Connect bound for liveness probes at bind time. Generous next to a socket
/// round-trip; a wedged predecessor times out in ~1s and reads as alive on the
/// first attempt (the refused-connect retry loop below only re-tries a dead
/// socket), so server startup never hangs forever on it.
const PROBE_ALIVE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Connect to an AF_UNIX SOCK_STREAM path with a bounded timeout. std's
/// `UnixStream::connect` has no connect-timeout knob, so a live-but-wedged
/// listener (dead accept loop, full backlog) blocks it indefinitely - the
/// `fno restart --mux` hang. Nonblocking connect + `poll` for writability,
/// then blocking mode restored so the caller's read/write timeouts apply.
///
/// The `ErrorKind::TimedOut` this returns means "wedged server" and callers
/// match on it to keep a wedged server out of the "stale/dead" bucket. That
/// signal is only sound for the CONNECT: a post-connect read/write timeout
/// (a server that accepted then stopped answering) is a different state and
/// must NOT be kind-matched against `TimedOut` here - callers treat those
/// with a bare `Err(_)` arm instead.
pub fn connect_unix_timeout(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket path contains NUL")
    })?;
    // SAFETY: a standard libc socket/connect/poll sequence. The fd is closed on
    // every error path and wrapped into a UnixStream (which owns + closes it)
    // on success.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Close-on-exec: std's UnixStream::connect sets it, so replacing that
        // call must too - otherwise a raw fd leaks into every child the mux
        // spawns (PTYs, the digest shell-out). fcntl(F_SETFD) is the portable
        // path (macOS has no SOCK_CLOEXEC socket-type flag); the tiny
        // create->set window is acceptable here (no fork+exec races on it).
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = c_path.as_bytes();
        if bytes.len() >= std::mem::size_of_val(&addr.sun_path) {
            libc::close(fd);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path too long",
            ));
        }
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr(),
            bytes.len(),
        );
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let addr_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let rc = libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len);
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            // AF_UNIX with a FULL accept backlog: Linux reports EAGAIN from a
            // nonblocking connect (macOS reports EINPROGRESS and the poll
            // below times out). Same wedged-server meaning - normalize to
            // TimedOut so every caller sees one signal on both platforms.
            if err.raw_os_error() == Some(libc::EAGAIN) {
                libc::close(fd);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connect timed out (accept backlog full)",
                ));
            }
            // A connect interrupted by a signal (EINTR) is NOT a failure: POSIX
            // says the connection continues asynchronously, exactly like
            // EINPROGRESS, so poll for its completion rather than reporting the
            // signal as a dead server (which the bind path would unlink).
            if err.raw_os_error() != Some(libc::EINPROGRESS)
                && err.raw_os_error() != Some(libc::EINTR)
            {
                libc::close(fd);
                return Err(err);
            }
            // Poll against a deadline, retrying on EINTR with the remaining
            // budget so a signal storm can neither abort the connect early nor
            // let it outlast `timeout`.
            let deadline = std::time::Instant::now() + timeout;
            let writable = loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let pr = libc::poll(&mut pfd, 1, ms);
                if pr < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::EINTR) {
                        continue; // interrupted: re-poll with recomputed remaining
                    }
                    libc::close(fd);
                    return Err(e);
                }
                break pr;
            };
            if writable == 0 {
                libc::close(fd);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connect timed out",
                ));
            }
            let mut soerr: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            if libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut soerr as *mut _ as *mut libc::c_void,
                &mut len,
            ) < 0
            {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            if soerr != 0 {
                libc::close(fd);
                return Err(std::io::Error::from_raw_os_error(soerr));
            }
        }
        // Restore blocking mode for the subsequent read/write timeouts.
        if libc::fcntl(fd, libc::F_SETFL, flags) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(UnixStream::from_raw_fd(fd))
    }
}

/// True if something accepts connections at `path`. Retries a few times so a
/// server that has bound but not yet reached `listen` is not declared dead.
/// A connect TIMEOUT counts as alive: only a refused connect proves the
/// server is dead, and unlinking a wedged-but-live server's socket would
/// orphan it (still running, unreachable by name, invisible to ls).
fn probe_alive(path: &Path) -> bool {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        match connect_unix_timeout(path, PROBE_ALIVE_CONNECT_TIMEOUT) {
            Ok(_) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return true,
            Err(_) => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame() -> Frame {
        let mut cells = vec![Cell::default(); 2 * 3];
        cells[0] = Cell {
            c: 'h',
            fg: Color::Indexed(2),
            bg: Color::Rgb(10, 20, 30),
            flags: cell_flags::BOLD | cell_flags::UNDERLINE,
        };
        Frame {
            rows: 2,
            cols: 3,
            cells,
            cursor_row: 1,
            cursor_col: 2,
            cursor_visible: true,
            scroll_offset: 4,
        }
    }

    #[test]
    fn proto_frame_roundtrips_through_codec() {
        let msg = ServerMsg::Frame {
            pane_id: 7,
            frame: test_frame(),
        };
        let bytes = encode(&msg).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let decoded: ServerMsg = read_msg_sync(&mut cursor).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn proto_client_msgs_roundtrip() {
        for msg in [
            ClientMsg::Attach {
                proto: PROTO_VERSION,
                build: BUILD_VERSION.into(),
                rows: 40,
                cols: 120,
                cwd: "/home/user/code/footnote".into(),
            },
            ClientMsg::Input(b"echo hello\r".to_vec()),
            ClientMsg::Resize { rows: 50, cols: 90 },
            ClientMsg::Detach,
            ClientMsg::Command(Command::SplitH),
            ClientMsg::Command(Command::FocusDir(Dir::Left)),
            ClientMsg::Command(Command::ResizeDir(Dir::Down)),
            ClientMsg::Command(Command::SelectTab(3)),
            ClientMsg::Command(Command::SelectSquad(42)),
            ClientMsg::Command(Command::FocusPane(3)),
            ClientMsg::Command(Command::attach_agent("c19cd2c3")),
            ClientMsg::Command(Command::DispatchNode("x-a496".into())),
            ClientMsg::Query,
            ClientMsg::KillServer,
            ClientMsg::Mouse {
                pane: 3,
                event: MouseEvent {
                    row: 4,
                    col: 9,
                    kind: MouseKind::Press(MouseButton::Left),
                },
            },
            ClientMsg::Mouse {
                pane: 3,
                event: MouseEvent {
                    row: 0,
                    col: 0,
                    kind: MouseKind::WheelUp,
                },
            },
            ClientMsg::Mouse {
                pane: 5,
                event: MouseEvent {
                    row: 1,
                    col: 6,
                    kind: MouseKind::Move,
                },
            },
            ClientMsg::Mouse {
                pane: 7,
                event: MouseEvent {
                    row: 2,
                    col: 2,
                    kind: MouseKind::Drag(MouseButton::Left),
                },
            },
            ClientMsg::BlockJump {
                pane: 3,
                dir: BlockDir::Prev,
            },
            ClientMsg::BlockSelect {
                pane: 3,
                dir: BlockDir::Next,
            },
            ClientMsg::BlockRerun { pane: 5 },
            ClientMsg::Command(Command::CopySelection),
            ClientMsg::SearchOpen {
                pane: 3,
                query: "deadlock".into(),
            },
            ClientMsg::SearchStep {
                pane: 3,
                dir: BlockDir::Prev,
            },
            ClientMsg::SearchStep {
                pane: 3,
                dir: BlockDir::Next,
            },
            ClientMsg::SearchClear { pane: 3 },
        ] {
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ClientMsg = read_msg_sync(&mut cursor).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn agent_row_from_v13_json_defaults_attach_id_none() {
        // A pre-v14 (v13) AgentRow omits `attach_id` entirely (skip-when-None).
        // A v14 reader must decode it as `None`, never fail - the wire
        // back-compat that lets the version skew window hold.
        let v13 = r#"{"squad":null,"name":"bg","pane_id":null,
                      "badge":null,"reason":null,"exited":false}"#;
        let row: AgentRow = serde_json::from_str(v13).unwrap();
        assert_eq!(row.attach_id, None);
        assert_eq!(row.answerable, None);
        assert_eq!(row.name, "bg");
    }

    #[test]
    fn agent_row_from_pre_external_json_defaults_external_false() {
        // x-0a2e AC3-FR: a pre-external (<=v19) AgentRow omits `external`
        // entirely. A v20 reader must decode it as `false`, never fail - the
        // skew window that lets an older client talk to a v20 server during
        // the handshake, and vice versa.
        let older = r#"{"squad":null,"name":"bg","pane_id":null,
                      "badge":null,"reason":null,"exited":false}"#;
        let row: AgentRow = serde_json::from_str(older).unwrap();
        assert!(!row.external, "missing external key => false");
    }

    #[test]
    fn agent_row_from_pre_seen_json_defaults_seen_false() {
        // AC1-ERR (x-4328): a pre-v23 AgentRow omits `seen` entirely. A v23
        // reader must decode it as `false` - the client then degrades to the
        // pre-feature `done == unseen`, never a panic.
        let older = r#"{"squad":null,"name":"bg","pane_id":null,
                      "badge":null,"reason":null,"exited":false}"#;
        let row: AgentRow = serde_json::from_str(older).unwrap();
        assert!(!row.seen, "missing seen key => false");
    }

    #[test]
    fn backlog_card_from_pre_v18_json_defaults_route_fields_none() {
        // A pre-v18 (v11..v17) BacklogCard omits the route fields entirely
        // (skip-when-None). A v18 reader must decode it as all-None, never
        // fail - same skew contract as the v14 `AgentRow.attach_id` bump.
        let pre_v18 = r#"{"id":"x-54fa","slug":"card-attach","priority":"p2",
                      "state":"InFlight"}"#;
        let card: BacklogCard = serde_json::from_str(pre_v18).unwrap();
        assert_eq!(card.pane_id, None);
        assert_eq!(card.attach_id, None);
        assert_eq!(card.where_hint, None);
        // And the reverse: a v18 writer with all-None routes emits no route
        // keys, so a pre-v18 reader (strict about nothing, but keep the
        // wire minimal) sees exactly the pre-v18 shape.
        let out = serde_json::to_string(&card).unwrap();
        assert!(!out.contains("pane_id") && !out.contains("where_hint"));
    }

    #[test]
    fn proto_v3_server_msgs_roundtrip() {
        // Every new/changed v3 server message survives the codec (mirrors the
        // Phase 1/2 roundtrip discipline): Layout carries `area`, TabMeta a
        // stable `id`, and the pre-Attach `Info` answer parses back exactly.
        for msg in [
            ServerMsg::Layout {
                squads: vec![SquadMeta {
                    id: 1,
                    name: "footnote".into(),
                    canonical_cwd: "/code/footnote/footnote".into(),
                    tabs: vec![
                        TabMeta {
                            id: 7,
                            name: "1".into(),
                            panes: vec![PaneMeta {
                                id: 4,
                                label: "claude".into(),
                            }],
                        },
                        TabMeta {
                            id: 12,
                            name: "2".into(),
                            panes: vec![],
                        },
                    ],
                    active_tab: 1,
                    panes: 2,
                }],
                active_squad: 1,
                panes: vec![
                    (
                        4,
                        Rect {
                            x: 0,
                            y: 0,
                            rows: 24,
                            cols: 40,
                        },
                    ),
                    (
                        9,
                        Rect {
                            x: 41,
                            y: 0,
                            rows: 24,
                            cols: 39,
                        },
                    ),
                ],
                focus: 9,
                area: (24, 80),
                agents: vec![
                    AgentRow {
                        squad: Some(1),
                        name: "peer".into(),
                        pane_id: Some(4),
                        badge: Some(AgentBadge::Blocked),
                        reason: Some("permission prompt".into()),
                        exited: false,
                        answerable: Some(AnswerablePrompt {
                            prompt: "Do you want to proceed?".into(),
                            options: vec![
                                AnswerOption {
                                    idx: "1".into(),
                                    label: "Yes".into(),
                                    keystroke: b"1".to_vec(),
                                },
                                AnswerOption {
                                    idx: "2".into(),
                                    label: "No".into(),
                                    keystroke: b"2".to_vec(),
                                },
                            ],
                            fingerprint: [7u8; 32],
                            region_lines: 8,
                        }),
                        attach_id: None,
                        external: false,
                        seen: false,
                        cwd_base: None,
                        tombstone: false,
                        tab: None,
                    },
                    AgentRow {
                        squad: None,
                        name: "bg-watch".into(),
                        pane_id: None,
                        badge: None,
                        reason: None,
                        exited: true,
                        answerable: None,
                        attach_id: None,
                        external: false,
                        seen: false,
                        cwd_base: None,
                        tombstone: false,
                        tab: None,
                    },
                ],
                focus_node: Some("x-66e8".into()),
                backlog: vec![
                    BacklogCard {
                        id: "x-6f77".into(),
                        slug: "work-queue-sideline".into(),
                        priority: "p1".into(),
                        state: CardState::InFlight,
                        pane_id: Some(7),
                        attach_id: None,
                        where_hint: None,
                    },
                    BacklogCard {
                        id: "ab-53c0".into(),
                        slug: "sync-wiki".into(),
                        priority: "p2".into(),
                        state: CardState::Ready,
                        pane_id: None,
                        attach_id: None,
                        where_hint: None,
                    },
                ],
            },
            ServerMsg::ModeSync {
                bytes: b"\x1b[?2004h\x1b[?1000l".to_vec(),
            },
            ServerMsg::Notice {
                text: "split refused: pane too small".into(),
            },
            ServerMsg::Bye {
                reason: "session ended".into(),
            },
            ServerMsg::Info {
                session: "work".into(),
                clients: 2,
                squads: 1,
                panes: 3,
            },
        ] {
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ServerMsg = read_msg_sync(&mut cursor).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn proto_v4_control_verbs_roundtrip() {
        // Every control verb survives the codec inside the versioned Control
        // envelope (mirrors the v3 discipline). PROTO_VERSION rides along so a
        // skew is detectable server-side.
        for verb in [
            ControlVerb::PaneLs,
            ControlVerb::PaneRead {
                pane: 3,
                lines: Some(40),
                block: None,
            },
            ControlVerb::PaneRead {
                pane: 3,
                lines: None,
                block: Some(BlockSel::Last),
            },
            ControlVerb::PaneRead {
                pane: 3,
                lines: None,
                block: Some(BlockSel::Seq(7)),
            },
            ControlVerb::PaneRun {
                cwd: "/code/footnote".into(),
                argv: vec!["claude".into(), "--print".into()],
                cols: Some(120),
                rows: None,
                claim: true,
                placement: PanePlacement::default(),
            },
            ControlVerb::PaneClaim {
                pane: 5,
                holder_pid: 4242,
            },
            ControlVerb::PaneRelease { pane: 5 },
            ControlVerb::PaneSend {
                pane: 5,
                bytes: b"hello\r".to_vec(),
                guarded: true,
            },
            ControlVerb::PaneWait {
                pane: 5,
                quiet_ms: Some(200),
                pattern: Some("done".into()),
                timeout_ms: 5000,
                command_done: true,
            },
            ControlVerb::PaneKill { pane: 5 },
        ] {
            let msg = ClientMsg::Control {
                proto: PROTO_VERSION,
                build: BUILD_VERSION.into(),
                verb,
            };
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ClientMsg = read_msg_sync(&mut cursor).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn proto_v28_placement_roundtrips_for_pane_run_and_attach() {
        let placement = PanePlacement {
            target: PaneTarget::SquadId(42),
            split: Some(Dir::Up),
        };
        for msg in [
            ClientMsg::Control {
                proto: PROTO_VERSION,
                build: BUILD_VERSION.into(),
                verb: ControlVerb::PaneRun {
                    cwd: "/code/footnote".into(),
                    argv: vec!["claude".into()],
                    cols: None,
                    rows: None,
                    claim: false,
                    placement: placement.clone(),
                },
            },
            ClientMsg::Command(Command::AttachAgent {
                id: "c19cd2c3".into(),
                placement,
            }),
        ] {
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ClientMsg = read_msg_sync(&mut cursor).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn proto_v4_control_replies_roundtrip() {
        for msg in [
            ServerMsg::PaneList {
                panes: vec![PaneInfo {
                    pane_id: 4,
                    squad_id: 1,
                    tab_id: 7,
                    cwd: "/code/footnote".into(),
                    child_pid: Some(4242),
                    title: None,
                }],
            },
            ServerMsg::PaneText {
                pane_id: 4,
                text: "marker-42\n$ ".into(),
                block: None,
            },
            ServerMsg::PaneText {
                pane_id: 4,
                text: "$ false".into(),
                block: Some(BlockMeta {
                    seq: Some(2),
                    exit: Some(1),
                    complete: true,
                    truncated: false,
                    implicit: false,
                }),
            },
            ServerMsg::PaneSpawned { pane_id: 9 },
            ServerMsg::Ok,
            ServerMsg::WaitDone {
                outcome: WaitOutcome::Quiet,
            },
            ServerMsg::WaitDone {
                outcome: WaitOutcome::Timeout,
            },
            ServerMsg::WaitDone {
                outcome: WaitOutcome::CommandDone { exit: Some(0) },
            },
            ServerMsg::Err {
                code: err_code::DEAD_PANE,
                msg: "no such pane: 99".into(),
            },
            ServerMsg::Copy {
                text: "selected lines\nincluding history".into(),
            },
            ServerMsg::SearchResult {
                pane_id: 4,
                total: 12,
                current: 3,
            },
            ServerMsg::SearchResult {
                pane_id: 4,
                total: 0,
                current: 0,
            },
        ] {
            let bytes = encode(&msg).unwrap();
            let mut cursor = std::io::Cursor::new(bytes);
            let decoded: ServerMsg = read_msg_sync(&mut cursor).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn proto_reader_rejects_oversized_length_prefix() {
        let mut bytes = (MAX_MSG_BYTES + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(b"junk");
        let mut cursor = std::io::Cursor::new(bytes);
        let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
        assert!(matches!(res, Err(ProtoError::TooLarge(_))), "{res:?}");
    }

    #[test]
    fn proto_reader_surfaces_malformed_body_as_error() {
        // Valid length prefix, garbage body: must error, never yield a value.
        let body = b"not json at all";
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        let mut cursor = std::io::Cursor::new(bytes);
        let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
        assert!(matches!(res, Err(ProtoError::Malformed(_))), "{res:?}");
    }

    #[test]
    fn proto_clean_eof_reads_as_closed() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
        assert!(matches!(res, Err(ProtoError::Closed)), "{res:?}");
    }

    #[test]
    fn proto_mid_body_eof_reads_as_closed() {
        let body = br#"{"Ok":null}"#;
        let mut bytes = ((body.len() + 1) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        let mut cursor = std::io::Cursor::new(bytes);
        let res: Result<ServerMsg, _> = read_msg_sync(&mut cursor);
        assert!(matches!(res, Err(ProtoError::Closed)), "{res:?}");
    }

    #[test]
    fn proto_version_match_is_accepted() {
        assert!(check_attach_version(PROTO_VERSION, BUILD_VERSION).is_ok());
    }

    #[test]
    fn proto_version_mismatch_names_both_versions() {
        let err = check_attach_version(PROTO_VERSION + 1, "9.9.9").unwrap_err();
        assert!(err.contains("9.9.9"), "{err}");
        assert!(
            err.contains(&format!("v{}", PROTO_VERSION + 1)),
            "client proto version missing: {err}"
        );
        assert!(
            err.contains(&format!("v{PROTO_VERSION}")),
            "server proto version missing: {err}"
        );
        assert!(err.contains(BUILD_VERSION), "server build missing: {err}");
    }

    #[test]
    fn proto_frame_geometry_check_catches_cell_count_mismatch() {
        let mut f = test_frame();
        assert!(f.geometry_ok());
        f.cells.pop();
        assert!(!f.geometry_ok(), "short cells vec must fail the check");
        f.cells.clear();
        assert!(!f.geometry_ok());
    }

    #[test]
    fn proto_pre_attach_wire_shapes_are_frozen() {
        // Query/KillServer/Info bypass the version handshake, so their JSON
        // encodings are FROZEN forever (Invariants). This pins the exact
        // bytes: if this test breaks, you changed a frozen shape - add a new
        // variant instead.
        assert_eq!(
            serde_json::to_string(&ClientMsg::Query).unwrap(),
            r#""Query""#
        );
        assert_eq!(
            serde_json::to_string(&ClientMsg::KillServer).unwrap(),
            r#""KillServer""#
        );
        assert_eq!(
            serde_json::to_string(&ServerMsg::Info {
                session: "s".into(),
                clients: 1,
                squads: 2,
                panes: 3,
            })
            .unwrap(),
            r#"{"Info":{"session":"s","clients":1,"squads":2,"panes":3}}"#
        );
    }

    #[test]
    fn proto_session_name_cannot_escape_mux_dir() {
        assert!(socket_path("../evil").is_err());
        assert!(socket_path("").is_err());
        assert!(socket_path("ok-name_1").is_ok());
    }
}
