//! The wire-version contract: what every PROTO_VERSION bump changed and how
//! far back a server still admits clients. Moved verbatim out of proto.rs
//! (file budget shrink); re-exported there, so `fno::proto::PROTO_VERSION`
//! and friends keep their paths. Intra-doc links name proto.rs items.
#![allow(rustdoc::broken_intra_doc_links)]
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
/// v11 (work-queue dispatch, x-6f77): a new `DispatchNext` client verb (prefix+g
/// "grab work") AND `Layout` gains `backlog: Vec<BacklogCard>` (the sideline
/// work-queue lane) - both wire-shape changes, so the shared counter bumps once.
///
/// v12 (in-scrollback search, x-e780): `ClientMsg::{SearchOpen, SearchStep,
/// SearchClear}` (prefix+/ free-text find over a pane's server-side vt history)
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
/// v17: `Command::RenameTab { tab, name }` - explicit tab rename (prefix+,,
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
///
/// v29 (x-c376): `Command::PeekAgent { name, seq }` asks the server for a
/// sideline row's recent transcript (shelled `fno agents peek <name>`, read-only)
/// and `ServerMsg::PeekBody { seq, name, lines }` returns it to the requesting
/// client only; the client drops any body whose `seq` is not the current request.
///
/// v30 (x-7561): `Command::ReapAgents` bulk-reaps every exited fno-agent
/// registry row (uppercase `X`, server-shelled to `fno-agents reap`), and
/// `Command::{StopExternal, RemoveExternal}` route an external claude-daemon
/// row's lifecycle through `claude stop|rm <attach_id>` keyed by stable attach
/// id, gated by a durable generation-checked compare-and-set in `squads.json`
/// (`external_lifecycle` collection).
///
/// v31 (x-9f75): `PanePlacement.here` - open-here repoints the sender's focused
/// (attach-viewer) pane at the target session instead of minting a tab or
/// split. `#[serde(default)]` keeps a v30 placement (no `here`) parseable as
/// `here: false` (today's semantics).
///
/// v32 (x-cd67): `AgentRow.subline` - the server-composed dim line-2 subline
/// (`<branch> · <cwd-tail>`) for the sideline. `#[serde(default)]` keeps a v31
/// reader parsing it as `None` (the pre-feature one-line row).
///
/// v33 (x-c914): the account-scoped dispatch verbs carry the client's
/// session-local active account. `ClientMsg::DispatchNext { account }` and
/// `Command::DispatchNode { node, account }` append `--account <id>` to the
/// server's `fno agents dispatch one` shell so a mux-initiated spawn bills the chosen
/// claude account; `None` = today's default (no flag). `AgentRow { account }`
/// carries the birth/roster account for the sideline glyph.
///
/// v34 (x-9c5f): peek-overlay follow-ups. `Command::MailAgent { name, text }`
/// and `Command::RespawnAgent { name }` are the two new off-loop server verbs;
/// `AgentRow { updated_at, pr }` carry the peek header's `changed Ns ago` stamp
/// and `PR #N` label.
///
/// v36 (x-1d91, the Backlog section): `BacklogCard { project, lane, head }` carry
/// the sideline's `project · lane` attribution subline and the explicit on-deck
/// head-of-queue marker; `Layout::backlog_lanes` carries UNCAPPED per-lane counts, feeding both
/// the section's exact `+N more` and the mini-kanban's lane headers;
/// `Layout::backlog_stale` marks the section as last-known rather than current; `Command::BacklogVerb { node, verb }`
/// ([`BacklogVerb`]) shells the existing `fno backlog rank --top` / `defer`
/// porcelain server-side - the mux never writes `graph.json` itself. All new reads
/// are `#[serde(default)]`, keeping a v35 reader wire-tolerant.
///
/// v37 (x-b186): `AgentRow { tail }` carries the row's most recent assistant
/// line for the extended sideline table's message column. Additive and
/// `#[serde(default)]`, so a v36 reader parses it as `None` (no tail cell).
///
/// v38: `Command::ToggleDiffPane` (the git diff side pane). A new verb, not an
/// additive field, so it needs the bump: a v37 server cannot deserialize it,
/// and without a version difference the handshake would accept an upgraded
/// client and then drop it on the first decode error instead of telling the
/// operator to restart the server.
///
/// v39 (x-d807): `Command::ResizeSeam` (dragging a pane divider). Same case as
/// v38 - a new verb rather than an additive field, so a v38 server cannot
/// deserialize it. Mux servers deliberately outlive client upgrades, so without
/// the bump an upgraded client would attach cleanly to a v38 server and then
/// have the connection dropped on its first divider drag, instead of being told
/// at handshake to restart the server.
///
/// v40 (x-aa95): `Command::MovePane` (drag-to-relocate and keyboard move-pane).
/// Same case again - a new verb, not an additive field, so a v39 server cannot
/// deserialize it and an unbumped upgraded client would lose its connection on
/// the first relocation rather than at handshake.
///
/// v41 also carries the mesh crown fields (`AgentRow.crown_level`/`crown_scope`,
/// additive + `#[serde(default)]`, documented inline on the fields); the two
/// changes share the one version bump.
///
/// v41 (layout-api): the mux layout script API - `PaneInfo.fno_id`,
/// `PanePlacement.{tab, at}` + `TabSel`, the `PaneSplit`/`Tab*`/`LayoutGet`/
/// `PaneWhere`/`PaneBreak`/`TabJoin` control verbs (+ `TabList`/`LayoutTree`/
/// `PaneLocation`/`TabSpawned` replies). The added FIELDS are all
/// `#[serde(default)]`-tolerant, but the new VERBS are not - a v40 server cannot
/// deserialize a `PaneSplit`, so one bump covers the whole node's wire delta
/// (Locked Decision 7).
///
/// v43 (x-d6a8, US9 drag faces): `Command::BreakPane`/`JoinTab` - the interactive
/// drag counterparts of the `PaneBreak`/`TabJoin` control verbs, dispatching into
/// the same `CoreMsg`s. New verbs, not additive fields, so a v42 server cannot
/// deserialize a `BreakPane` and an unbumped client would lose its connection on
/// the first pane-break drag rather than at handshake. Rides on top of the x-c4d4
/// layout-template v42 bump (independent additive wire deltas, one version each).
///
/// v45 (x-a2d0, clickable links): `ServerMsg::OpenLink { url }` - the server
/// resolves the URL under a click (OSC 8 or linkified text, see [`crate::link`])
/// and the CLIENT opens it, because the client is the process sitting at the
/// human's desk. A new variant, not an additive field, so a v44 client cannot
/// decode it; the handshake is what stops the skew.
///
/// v46 (x-3e17, pane focus): `ControlVerb::PaneFocus` + `ServerMsg::PaneFocused`
/// - the CLI door onto the focus trunk the TUI already owns. New variants, not
/// additive fields, so a v45 server cannot deserialize a `PaneFocus`; the
/// handshake is what stops the skew.
///
/// v50 (x-132c): `AgentRow.{spawned_by_session, harness_session_id}` - the
/// lineage pair the sideline joins into a parent/child forest. Additive and
/// `#[serde(default)]`, so an unbumped client would merely keep rendering
/// flat; the bump names the skew so the handshake restarts an old server
/// instead. (Numbered one past the x-5f7f resume-gesture v49 it rebases
/// onto.)
///
/// v52 (x-588a): pane reads and sends carry the pane's captured identity and
/// the registry identity used to address it. Additive fields remain defaulted,
/// but the send identity is a safety contract, so the handshake must reject an
/// older peer rather than let it type into an unverified pane.
///
/// v51 (x-1499, tab dictionary): `ControlVerb::TabWhere` +
/// `ServerMsg::TabLocation`/`TabPaneOccupant` - the reverse location lookup
/// (what lives at the tab the operator is looking at). `TabSel::Index`
/// becomes the 1-based ordinal the UI shows, where it was a 0-based vector
/// index, so a captured `--tab <n>` selector changes meaning across the
/// bump; the handshake is what tells an old client to restart. New variants
/// are not additive-tolerant either.
/// v55 (guarded tab close): `ControlVerb::TabClose` and the target-specific
/// `ServerMsg::TabClosed` receipt.
///
/// v56 (hover affordance): `ClientMsg::LinkHover` + `ServerMsg::LinkHover` -
/// the sequenced, initiator-only hover lookup for clickable URLs. New
/// variants, so an unbumped peer cannot decode the pair; the handshake is
/// what stops the skew.
///
/// v57 (x-d401, unmeasured liveness): `AgentNoPaneReason::LivenessUnmeasured`
/// - a NEW enum variant, so a v56 peer cannot decode a row carrying it, the
/// same reason `BackendNotLive` bumped 53 -> 54. `AgentRow.pane_activity`
/// rides the same bump (additive-tolerant on its own, but it is the shape
/// change the variant belongs to).
///
/// v58 (x-07c2, dedicated thread pane): `ControlVerb::ThreadPane` - a NEW
/// enum variant, so a v57 peer cannot decode it and closes the connection
/// instead of running the reach; the handshake is what stops the skew.
/// `AgentRow.reach` rides the same bump (additive-tolerant on its own via
/// `#[serde(default)]`, but it is the shape change the verb belongs to).
///
/// v59 (classified lineage): `PaneInfo` gains `harness_session_id`,
/// `predecessor_session_ids`, and `forked_from_session_id` - each
/// additive-tolerant via `#[serde(default)]`, but the shape change belongs
/// to one bump, and the handshake, not serde tolerance, is the skew guard.
///
/// v60 (workspace restore): `ControlVerb::WorkspaceRestore` +
/// `ServerMsg::WorkspaceRestored` ([`RestoreRow`]s). New variants: a v59 peer
/// cannot decode them, so the handshake stops the skew.
///
/// v61 (DND presence): `AgentRow.dnd`, additive; the pane-send refusal rides
/// the same generation.
/// v62 (row detach): `Command::DetachPane { pane }` removes a live worker
/// pane from the visible tree without touching frozen `ClientMsg::Detach`.
///
/// v64 (portals): `PanePlacement.portal`, `PanePlacement.portal_new` and
/// `AgentRow.portal` make the one thread pane an addressable set.
/// `thread_pane` stays as a compatibility alias meaning portal 0.
///
/// v65 (tab organization): `ControlVerb::TabReorder { squad, tab, to }`, the
/// CLI door onto the reorder trunk, and `PaneInfo.shell_idle`, the measured
/// "idle now" reading the used-shell prune sweep needs. A new verb is not
/// additive-tolerant; the field rides the same generation.
/// v66 (sideline rename): `Command::RenameAgent` - a new verb, this generation.
/// The same generation also adds `ControlVerb::ThreadPane`'s
/// `#[serde(default)]` `placement` - the tab/split/at/target a FRESH portal
/// open honors, now that the geometry refusal lives inside `reach_portal`
/// where the slot lookup knows occupancy. Additive, so the compatibility
/// floor does not move; a repoint keeps owning its geometry and says so.
/// v68 (x-5baf): `LayoutSlot.cwd`, `#[serde(default)]`; floor stays 58.
/// v69 (x-a600): `Command::RedrawPane`, `#[serde(default)]`; floor stays 58.
pub const PROTO_VERSION: u32 = 69;

/// The oldest wire version this build can speak. Bumps that only add verbs or
/// `#[serde(default)]` fields move `PROTO_VERSION`; a change to an existing
/// shape must move this floor too.
pub const MIN_COMPAT_PROTO: u32 = 58;

/// The first wire generation whose SERVER admits a compatibility-floor range
/// of clients instead of equality-gating attach. Every older generation
/// (`58`/`59`, shipped 2026-08-29) refuses any `client_proto !=` its own, so
/// the client-side sidecar verdict (`SessionRow::wire_stale`) reads a sidecar
/// below this generation as unattachable, never merely "older".
///
/// Ceiling, recorded because the `.ver` sidecar stamps only `PROTO_VERSION`:
/// a future server whose floor EXCEEDS a given client's generation will still
/// read as compatible to that client until the sidecar carries the floor
/// alongside the version. Equality-with-the-version must not come back as the
/// test: it misflags every one-generation-old floor-admitting server.
pub const FLOOR_SINCE_PROTO: u32 = 60;
