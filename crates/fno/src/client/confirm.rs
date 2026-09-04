//! A pending destructive/costly action and what a confirmed Enter
//! commits. Moved out of client.rs (file budget shrink): the confirm
//! vocabulary is one question, so it answers from its own module.
//! The gesture sites, the commit mapping and the row stamp all read
//! these types through the re-export in the parent.

use super::{SectionKey, TabId};

/// A pending destructive/costly action awaiting the operator's one-keypress
/// confirm. `label` is the entity name shown in the prompt; `action` is what
/// Enter commits (x-a496 dispatch, extended by x-96e8 with squad removal).
pub(crate) struct ConfirmAction {
    pub(crate) action: ConfirmKind,
    pub(crate) label: String,
}

/// What a confirmed [`ConfirmAction`] sends on Enter.
pub(crate) enum ConfirmKind {
    /// Start a targeted session on a work-queue card's node (x-a496).
    Dispatch { node: String },
    /// Close a whole workspace (x-96e8). `panes` is the blast radius named in
    /// the prompt; `last` warns that removing the session's only squad ends it.
    RemoveSquad {
        squad: u64,
        panes: usize,
        last: bool,
    },
    /// Stop a live agent row (x-76ea). The captured `name`, not the row index,
    /// commits - a row that raced out between confirm and Enter resolves to the
    /// server's stale-name refusal. (v67) The row's harness session id rides
    /// beside the label (law d-e952ed19): `sid` is captured with the name at
    /// gesture time so the server resolves identity-first; `None` for an old
    /// capture or a bare-identity row.
    StopAgent { name: String, sid: Option<String> },
    /// Remove an exited agent row (x-76ea). Same captured name + session id.
    RemoveAgent { name: String, sid: Option<String> },
    /// Bulk-reap every exited fno-agent registry row (x-7561, uppercase `X`).
    /// No payload - the server's reap verb owns the candidate set.
    ReapAgents,
    /// Stop a live external claude-daemon row by stable `attach_id` (x-7561).
    /// The captured attach id, not the row index, commits; `name` is cosmetic.
    StopExternal { attach_id: String, name: String },
    /// Remove a stopped external tombstone by `attach_id` (x-7561). Same
    /// captured-id commit; the server gates rm on a persisted `stopped` state.
    RemoveExternal { attach_id: String, name: String },
    /// Dismiss a member TOMBSTONE from its squad's member list (x-8f11). A
    /// tombstone is not a registry agent, so RemoveAgent cannot reach it.
    DismissMember { squad: u64, attach_id: String },
    /// Remove every exited row in one section (x-f300). The SECTION commits, not
    /// the row list: the set is re-folded on Enter, so rows that died or were
    /// reaped while the prompt sat open are handled honestly. `dead` is the count
    /// the prompt showed, kept only to name it.
    ClearDead {
        key: SectionKey,
        squad: Option<u64>,
        dead: usize,
    },
    /// Close one tab (the tab menu's destructive item). The captured stable
    /// [`TabId`], not a view index, commits: `CloseTab` closes the SENDER'S
    /// VIEWED tab server-side, so Enter first selects the captured tab then
    /// closes it. The id is re-resolved at Enter (the ClearDead re-fold
    /// precedent): a tab that raced out between arm and Enter is a notice,
    /// never a bare CloseTab closing whatever is viewed now.
    CloseTab { tab: TabId },
}
