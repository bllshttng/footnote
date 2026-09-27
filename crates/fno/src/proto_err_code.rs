//! Error-code namespace (move out of proto.rs for the shrink-only ratchet).

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
/// (v41, layout-api) `PaneWhere` could not read the agents registry (missing
/// / unreadable / mid-write partial JSON). DISTINCT from NOT_FOUND: the id
/// might exist; the lookup itself failed. Never an empty-success (Locked 4).
pub const REGISTRY_UNAVAILABLE: u32 = 7;
/// (v41, layout-api) `PaneWhere`: the `fno_id` is absent from the registry.
pub const NOT_FOUND: u32 = 8;
/// (v41, layout-api) `PaneWhere`: the `fno_id` is in the registry but hosts
/// no live pane (a paneless bg/headless session). DISTINCT from NOT_FOUND so
/// a script can branch (Locked 4).
pub const NOT_PANE_HOSTED: u32 = 9;
/// (v42) `LayoutApply`: a fixed-arity template got the wrong slot
/// count (e.g. `grid-2x2` with 3 slots). Pre-mutation, atomic.
pub const TEMPLATE_ARITY: u32 = 10;
/// (v42) `LayoutApply`: the template's slots cannot tile the tab's
/// viewport above `MIN_ROWS x MIN_COLS`. The refusal names the overflowing
/// slots; the tab is left completely unchanged (atomic).
pub const TEMPLATE_UNFITTABLE: u32 = 11;
/// (v42) `LayoutApply`: an unknown template name. Pre-mutation, atomic.
pub const TEMPLATE_UNKNOWN: u32 = 12;
/// `PaneFocus`: the pane exists but no non-passive client is attached, so
/// there is no viewer to move. DISTINCT from [`DEAD_PANE`] on purpose: "your
/// pane is gone" and "nobody is watching" are different problems, and
/// collapsing them leaves the operator unable to tell which one they have.
pub const NO_CLIENT: u32 = 13;
/// (v51) The addressed identity disagrees with the pane's captured
/// identity or its unique registry occupant; no bytes were typed.
pub const TARGET_IDENTITY_MISMATCH: u32 = 14;
/// (v60) `WorkspaceRestore` arrived before the session's first real
/// attach, so the persisted squads were never read into memory and an empty
/// member list would read as "nothing to restore". The refusal names the
/// attach precondition; the store is untouched.
pub const RESTORE_NOT_RUN: u32 = 15;
/// (v61) `PaneSend` targeted a pane whose registry row is DND.
/// The bytes did not land; use mail send to queue durable until release.
pub const TARGET_DND: u32 = 16;
/// (v75) `RetireSession`'s durable half failed: the store write
/// did not land, so the retirement is NOT durable and the caller retries
/// the whole verb.
pub const STORE_WRITE_FAILED: u32 = 17;
