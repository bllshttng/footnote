use super::*;

/// Client -> server. Everything here rides the reliable channel. `Eq` is NOT
/// derived: a `Control` verb may carry an [`AnchoredLayoutSpec`] (`f32` weights).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Clean detach: the client is leaving; the server keeps the PTYs.
    Detach,
    /// A layout/tab/squad command from the client's prefix-key layer
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
    Mouse {
        pane: u64,
        event: MouseEvent,
    },
    /// (v56, hover affordance) Ask which cells of `pane` belong to the link
    /// under pane-local `(row, col)`, for the client's hover underline.
    /// Read-only and initiator-only: the reply is one
    /// [`ServerMsg::LinkHover`] to THIS client - co-viewers see nothing, pane
    /// state and the frame stream are untouched. `seq` is a client-local
    /// monotonic counter echoed on the reply so a result for a target the
    /// pointer has already left is dropped client-side (the same A->B->A
    /// guard `PeekAgent` uses).
    LinkHover {
        pane: u64,
        row: u16,
        col: u16,
        seq: u64,
    },
    /// (v8) Walk the pane's OSC 133 command blocks, moving the shared per-pane
    /// scroll so `dir`'s adjacent block anchors at the viewport top. A pane with
    /// no blocks replies with a `Notice` and no scroll change.
    BlockJump {
        pane: u64,
        dir: BlockDir,
    },
    /// (v8) Move the block-scoped selection to `dir`'s adjacent block (the whole
    /// command + output span), so the existing copy chain (prefix+y) yanks it.
    BlockSelect {
        pane: u64,
        dir: BlockDir,
    },
    /// (v8) Re-send the selected (else newest) block's command line to the pane
    /// PTY. Refused unless the pane is known-idle - a rerun injected into a busy
    /// agent corrupts its composer (false-ready is the forbidden direction).
    BlockRerun {
        pane: u64,
    },
    /// (v11, x-dddd) "Grab work" (prefix+g): dispatch the next ready backlog
    /// node into a new pane in this session. Server-wide (no pane field): the
    /// server shells the Python porcelain off the core loop, and the outcome
    /// (no ready work / lanes full / failure) returns as a one-line `Notice`.
    /// A read-only observer client is refused at the core (mutating_sender).
    /// (v31) `account` is the client's session-local active account,
    /// appended as `--account <id>` to the spawn; `None` = the default account.
    DispatchNext {
        #[serde(default)]
        account: Option<String>,
    },
    /// (v9) Answer a blocked prompt from the queue without focusing it.
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
    /// (v12) Open/refresh a free-text search over the pane's server-side
    /// vt history (prefix+/): scan case-insensitively, jump the shared scroll to
    /// the initial match, highlight it via the v7 `SELECTED` broadcast, and store
    /// the match list as a per-pane snapshot. An empty `query` clears (never a
    /// scan that matches every row). The reply is one [`ServerMsg::SearchResult`]
    /// to the initiator; co-viewers get the jump + highlight via the broadcast
    /// `Frame`. History text never crosses the wire.
    SearchOpen {
        pane: u64,
        query: String,
    },
    /// (v12) Walk the active search's match snapshot: `Prev` toward older,
    /// `Next` toward newer (reusing [`BlockDir`], whose doc semantics match n/N).
    /// Re-jumps + re-highlights and replies a fresh `SearchResult`. A pane with no
    /// active search no-ops with a `Notice`, never a panic.
    SearchStep {
        pane: u64,
        dir: BlockDir,
    },
    /// (v12) Clear the active search: drop the highlight (selection) and
    /// the per-pane search state, then broadcast a `Frame`. Idempotent: clearing
    /// with nothing active still clears + broadcasts (the client sends this on
    /// every search exit, and a no-match search_open has already dropped the
    /// state server-side).
    SearchClear {
        pane: u64,
    },
    /// (v83, ) The sideline new-agent popup submits one typed launch
    /// request. The server validates pre-birth, shells canonical
    /// `fno agents spawn` OFF the core loop, and answers this client with
    /// [`ServerMsg::AgentLaunch`] progress updates correlated by
    /// `request_id`. Structured values only - the message rides stdin at
    /// the spawn door, never an argv element.
    AgentLaunch(crate::proto::agent_launch::AgentLaunchRequest),
    PaneInput(pane_input::PaneInputRequest),
}

/// Server -> client.
///
/// Channel discipline (Locked Decision 4): `Layout`/`ModeSync`/`Bye` ride the
/// per-client RELIABLE channel (awaited, never dropped - a dropped Layout is
/// a protocol bug, not a degraded mode); only pane-tagged self-contained
/// `Frame`s are droppable (per-(client, pane) newest-wins). v1's `Cursor`
/// variant is gone: it was never sent (the cursor rides inside `Frame`).
///
/// Not `Eq` (was, pre-v41): `LayoutTree` embeds `tree::Node`, whose branch
/// ratios are `f32` (no `Eq`, NaN). `PartialEq` is retained for tests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServerMsg {
    /// A self-contained render frame (full grid + cursor) for ONE pane.
    /// Droppable: the server keeps only the newest unsent frame per
    /// (client, pane), so a flooded pane coalesces without starving its
    /// siblings. `pane_id` lives on the variant, not in [`Frame`]: the VT
    /// grid (`vt::Pane`) does not know its mux pane id - the server's pane
    /// registry tags the frame at send time.
    Frame {
        pane_id: u64,
        frame: Frame,
    },
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
        /// from the pane-run argv. `None` for an ad-hoc pane; carried
        /// on `Layout` (not `Frame`) because it changes only on focus/structure
        /// changes, mirroring how `SquadMeta::canonical_cwd` reaches the client.
        #[serde(default)]
        focus_node: Option<String>,
        /// (v11, x-dddd) Board-ordered work-queue cards for the sideline backlog
        /// lane (backlog_view). Empty when the graph is unreadable or has no
        /// ready/blocked/in-flight work; `#[serde(default)]` keeps a v10 reader
        /// wire-tolerant.
        #[serde(default)]
        backlog: Vec<BacklogCard>,
        /// (v36) The UNCAPPED per-lane queue-card counts (lane name ->
        /// count, lane-sorted). Feeds both the section's exact `+N more` under
        /// the capped `backlog` list above and the mini-kanban's lane headers, so
        /// the two can never disagree about how much work exists.
        #[serde(default)]
        backlog_lanes: Vec<(String, usize)>,
        /// (v36) The graph read has been failing, so `backlog` is the
        /// last-known set rather than current fact. The section keeps rendering
        /// it (a blank lane would be worse) but says it is stale.
        #[serde(default)]
        backlog_stale: bool,
        /// Positive dead-member candidates from the server's shared sweep
        /// classifier, used by the sideline menu label.
        #[serde(default)]
        sweep_dead_count: usize,
        /// (v97) The server's wire version, announced on every layout. `None`
        /// reads on an older server's layout, which the client treats as
        /// "cannot parse post-announcement commands" - the safe reading when
        /// the announcer is absent.
        #[serde(default)]
        proto: Option<u32>,
    },
    /// Escape bytes syncing the client terminal to the newly focused pane's
    /// negotiated modes (bracketed paste, mouse reporting, DECCKM, ...).
    /// Applied verbatim to the client TTY. Reliable, and ordered BEFORE the
    /// `Layout`/frames that assume those modes.
    ModeSync {
        bytes: Vec<u8>,
    },
    /// A one-line human-facing notice (refused command, failed split, ...)
    /// the client renders as transient feedback + BEL. Reliable.
    Notice {
        text: String,
    },
    /// The server is refusing or ending this connection; `reason` is
    /// human-facing (version skew, shutdown, session ended, ...).
    Bye {
        reason: String,
    },
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
    PaneList {
        panes: Vec<PaneInfo>,
    },
    /// (v78) Answer to [`ControlVerb::ServerStats`]: the in-memory
    /// human_touch emission-failure count with its measurement window
    /// (instance start + measured time); never an all-time fact.
    ServerStats {
        touch_emit_failures: u64,
        started_at: String,
        measured_at: String,
    },
    /// Answer to [`ControlVerb::PaneRead`]: the pane's text (matches
    /// [`crate::vt::frame_text`]). `block` (v6) carries the command-block
    /// metadata when the request selected a block; `None` for a plain grid/
    /// history read. `#[serde(default)]` so a plain read stays wire-stable.
    PaneText {
        pane_id: u64,
        text: String,
        #[serde(default)]
        block: Option<BlockMeta>,
        /// (v51) Identity captured from the pane's spawn argv.
        #[serde(default)]
        pane_name: Option<String>,
        /// (v51) The registry identity joined to this pane ref, not a
        /// fact owned by the pane itself.
        #[serde(default)]
        registry_fno_id: Option<String>,
    },
    /// Answer to [`ControlVerb::PaneRun`]: the fresh pane's id, machine-read
    /// by the CLI so scripts compose. `placement` (v44) carries the
    /// server-authored exact-placement receipt on `--at current` spawns; `None`
    /// on legacy focused-relative spawns (omitted on the wire via skip-if-none,
    /// so a v43 reader is unaffected).
    PaneSpawned {
        pane_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placement: Option<ResolvedPlacement>,
    },
    /// A verb that carries no payload succeeded (`PaneSend`, `PaneKill`).
    Ok,
    /// (v60) Answer to [`ControlVerb::WorkspaceRestore`]: one row per
    /// member, including every refusal with its reason.
    WorkspaceRestored {
        rows: Vec<RestoreRow>,
    },
    /// (v71) Answer to [`ControlVerb::SquadReload`]: counts now held.
    SquadReloaded {
        squads: usize,
        members: usize,
        emptied: usize,
    },
    /// (v75) Answer to [`ControlVerb::RetireSession`]: how many members the
    /// store tombstoned and how many attached panes closed. Both are zero on
    /// a repeat call: retirement is idempotent, never an error.
    /// The closed panes are NAMED, and any tab the closes emptied
    /// and removed is named too; both lists ride default-skipped so an older
    /// reader is unaffected.
    SessionRetired {
        retired: usize,
        panes_closed: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        closed_panes: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tabs_removed: Vec<String>,
        /// Panes the 60s operator_typing guard skipped: the close
        /// was refused, not lost. Default-skipped so an older reader is
        /// unaffected.
        #[serde(default)]
        skipped_typing: usize,
    },
    /// Answer to [`ControlVerb::PaneWait`].
    WaitDone {
        outcome: WaitOutcome,
    },
    /// A control verb failed (dead pane, spawn failure, version skew, ...).
    /// `code` is one of [`err_code`]; `msg` is one human line.
    Err {
        code: u32,
        msg: String,
    },
    /// (v7) Extracted selection text destined for the client's clipboard chain
    /// (brief Locked 5). Copy extraction happens server-side (history lives
    /// there); the client execs its local clipboard tool, else emits OSC 52,
    /// else reports the failure visibly. Reliable - a dropped copy is silent
    /// data loss, never acceptable.
    Copy {
        text: String,
    },
    /// (v45) A URL the user clicked, for the client to hand to the
    /// platform opener. Resolved server-side because the grid (and its OSC 8
    /// hyperlink state and scrollback) lives there; opened client-side because
    /// the client is the process at the human's desk, exactly like [`Self::Copy`]
    /// and its clipboard. Already filtered through `link::is_openable` by the
    /// server; the client checks again before exec rather than trusting the
    /// wire. Reliable - a dropped click is a dead-feeling button.
    OpenLink {
        url: String,
    },
    /// (v56, hover affordance) The initiator-only answer to a
    /// [`ClientMsg::LinkHover`]: the visible pane-local cells to underline for
    /// the requester's current pointer target. `cells` empty means no link (or
    /// a pane/cell the requester cannot view); the URL never rides this reply,
    /// so the affordance leaks no pane text. Reliable.
    LinkHover {
        pane_id: u64,
        seq: u64,
        cells: Vec<(u16, u16)>,
    },
    /// (v12) The initiator-only result of a `SearchOpen`/`SearchStep`:
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
    /// (v29) The transcript body for a [`Command::PeekAgent`], sent only
    /// to the requesting client. `seq` echoes the request's counter so the client
    /// drops a stale reply; `name` is the peeked row (for a defensive header
    /// cross-check); `lines` is the shelled `fno agents peek` output (already
    /// split into display lines), with any error/timeout text carried in-band as
    /// lines too - the overlay renders whatever comes back and never closes on a
    /// fetch error. Reliable (one small message per keystroke).
    PeekBody {
        seq: u64,
        name: String,
        lines: Vec<String>,
    },
    /// (v83, ) Progress for one sideline launch attempt, sent only to
    /// the requesting client. `Starting` may be followed by at most one
    /// terminal state per request; a duplicate submission is answered with
    /// the SAME attempt, never a second spawn.
    AgentLaunch(crate::proto::agent_launch::AgentLaunchUpdate),
    // -- v41 (layout-api) control-verb replies --
    /// Answer to [`ControlVerb::TabLs`].
    TabList {
        tabs: Vec<TabInfo>,
    },
    /// Answer to [`ControlVerb::LayoutGet`]: the nested tree + per-pane geometry
    /// for the requested scope (Locked Decision 5).
    LayoutTree {
        squads: Vec<SquadLayout>,
    },
    /// Answer to [`ControlVerb::AgentRowsGet`]: the row-set receipt.
    AgentRowsReceipt {
        rows: Vec<AgentRowReceipt>,
    },
    /// Answer to [`ControlVerb::PaneWhere`]: where an `fno_id` lives right now.
    /// The multi-tab / multi-pane shape is mirroring-ready (one id can host
    /// several panes across tabs). Never emitted empty-but-successful.
    PaneLocation {
        fno_id: String,
        squad_id: u64,
        squad_name: Option<String>,
        /// The tabs hosting this id's panes, each with its optional name.
        tabs: Vec<(TabId, Option<String>)>,
        /// (v51) Each hosting tab's current 1-based ordinal, parallel
        /// to `tabs`, so the human receipt prints label and id together.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_ordinals: Option<Vec<usize>>,
        /// The pane ids hosting this id.
        panes: Vec<u64>,
    },
    /// Answer to [`ControlVerb::PaneBreak`]: the id of the freshly created
    /// tab, plus (v51) its label pair for the human receipt.
    TabSpawned {
        tab_id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_ordinal: Option<usize>,
    },
    /// Answer to [`ControlVerb::PaneFocus`]: where the pane was found, and how
    /// many viewers actually ended up looking at it.
    ///
    /// `clients_moved` is load-bearing, not decoration. Accepting the command
    /// proves a command was accepted, never that anything moved on screen - a
    /// bare `Ok` here would be the same class of lie as reading `queued
    /// (durable)` as delivery. It counts clients whose view IS the resolved
    /// (squad, tab) after the dispatch, not clients the loop iterated over.
    PaneFocused {
        pane: u64,
        squad_id: u64,
        squad_name: Option<String>,
        tab_id: TabId,
        /// (v51) Label pair for the human receipt; see
        /// [`ResolvedPlacement::tab_name`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_ordinal: Option<usize>,
        clients_moved: usize,
    },
    /// (v42) Answer to [`ControlVerb::LayoutApply`]: one [`SlotResult`]
    /// per requested slot, in slot order, so a script captures the created pane
    /// ids from the receipt (never predicts them). A top-level `Err` (arity /
    /// fit / unknown template) means nothing was mutated; a `LayoutApplied` with
    /// a `SpawnFailed` slot is a reported partial success.
    LayoutApplied {
        results: Vec<SlotResult>,
    },
    /// (v44) Answer to [`ControlVerb::LayoutGraft`]: the committed
    /// anchor/squad/tab and one outcome per named slot. Graft is all-or-nothing,
    /// so every slot is filled on this receipt; a refusal is a top-level `Err`.
    LayoutGrafted {
        anchor: u64,
        squad: u64,
        tab: TabId,
        /// (v51) Label pair for the human receipt; see
        /// [`ResolvedPlacement::tab_name`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_ordinal: Option<usize>,
        results: Vec<GraftSlotResult>,
    },
    /// (v51) Answer to [`ControlVerb::TabWhere`]: what lives at a tab
    /// location right now. `panes` is every pane in the tab in tree order,
    /// each with its joined worker identity; an occupant with `fno_id: None`
    /// is an EXPLICIT empty pane, never a failed lookup. A tab always holds
    /// at least one pane, so this is never emitted empty.
    TabLocation {
        squad_id: u64,
        squad_name: Option<String>,
        tab_id: TabId,
        name: Option<String>,
        /// The tab's current 1-based ordinal (the UI's `·N`).
        ordinal: usize,
        /// The tab's focused pane - where `mux view` moves the operator.
        focus: u64,
        panes: Vec<TabPaneOccupant>,
    },
    /// Answer to [`ControlVerb::TabClose`]. The receipt names the exact stable
    /// tab and every pane reaped; `forced` records the explicit guard override.
    TabClosed {
        tab_id: TabId,
        pane_ids: Vec<u64>,
        forced: bool,
    },
    PaneInputResult(pane_input::PaneInputResult),
}
