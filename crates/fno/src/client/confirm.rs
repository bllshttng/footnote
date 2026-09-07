//! A pending destructive/costly action and what a confirmed Enter
//! commits. Moved out of client.rs (file budget shrink): the confirm
//! vocabulary is one question, so it answers from its own module.
//! The gesture sites, the commit mapping and the row stamp all read
//! these types through the re-export in the parent.

use super::{SectionKey, TabId};
use crate::proto::Command;

/// The command that clears ONE dead row, by what kind of row it is. Three
/// stores hold dead rows and each has its own verb: a member TOMBSTONE lives in
/// the squad's member list (`RemoveAgent` resolves only against the agent
/// registry, so it would answer "no such agent" and leave the row on screen), an
/// EXTERNAL row routes by its stable attach_id (x-7561), and a registry row goes
/// by name. One mapping so the row menu and the bulk clear cannot disagree.
pub(crate) fn remove_dead(a: &super::AgentRow) -> Command {
    match (a.tombstone, a.squad, a.external, a.attach_id.clone()) {
        (true, Some(squad), _, Some(attach_id)) => Command::DismissMember { squad, attach_id },
        (_, _, true, Some(attach_id)) => Command::RemoveExternal {
            attach_id,
            name: a.name.clone(),
        },
        _ => Command::RemoveAgent {
            name: a.name.clone(),
            harness_session_id: a.harness_session_id.clone(),
            // A dead row's pane is gone or dangling; the pane leg has
            // nothing to answer, so resolution stays registry-only here.
            // The live-row gesture path carries pane_id itself.
            pane_id: None,
            measure: false,
        },
    }
}

/// How many rows one clear-dead may remove. Each row costs the server a
/// `fno agents rm` subprocess (`agent_action` spawns one per command, unbounded),
/// so an unbounded fan-out would let a long-lived section stampede the daemon.
/// ponytail: a flat cap, repeat to clear the rest; the upgrade is a section-scoped
/// bulk verb server-side, which the single-process `ReapAgents` already models.
pub(crate) const CLEAR_DEAD_MAX: usize = 25;

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
    /// beside the label: `sid` is captured with the name at
    /// gesture time so the server resolves identity-first; `None` for an old
    /// capture or a bare-identity row. (x-e763) The pane the row was drawn
    /// from rides too, so a bare-pane row acts through its pane even with no
    /// registry row.
    StopAgent {
        name: String,
        sid: Option<String>,
        pane_id: Option<u64>,
    },
    /// Remove an agent row in one gesture (x-76ea, x-e763). Same captured
    /// name + session id + pane id. (x-b5d1) `measure` arms the
    /// measure-and-remove prompt and wire flag for an Unmeasured row: rm's
    /// daemon-side gate does the measuring the stop leg cannot.
    RemoveAgent {
        name: String,
        sid: Option<String>,
        pane_id: Option<u64>,
        measure: bool,
    },
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
    /// the closes it. The id is re-resolved at Enter (the ClearDead re-fold
    /// precedent): a tab that raced out between arm and Enter is a notice,
    /// never a bare CloseTab closing whatever is viewed now.
    CloseTab { tab: TabId },
}

impl ConfirmKind {
    /// The one command a confirmed kind commits. `None` for the kinds whose
    /// commit is not a plain one-command send: `Dispatch` carries the view's
    /// active account, `ClearDead` re-folds the dead set at Enter, `CloseTab`
    /// re-resolves the tab at Enter. The commit path keeps those three arms.
    pub(crate) fn command(self) -> Option<Command> {
        match self {
            ConfirmKind::Dispatch { .. } => None,
            ConfirmKind::RemoveSquad { squad, .. } => Some(Command::RemoveSquad(squad)),
            ConfirmKind::StopAgent { name, sid, pane_id } => Some(Command::StopAgent {
                name,
                harness_session_id: sid,
                pane_id,
            }),
            ConfirmKind::RemoveAgent {
                name,
                sid,
                pane_id,
                measure,
            } => Some(Command::RemoveAgent {
                name,
                harness_session_id: sid,
                pane_id,
                measure,
            }),
            ConfirmKind::ReapAgents => Some(Command::ReapAgents),
            ConfirmKind::StopExternal { attach_id, name } => {
                Some(Command::StopExternal { attach_id, name })
            }
            ConfirmKind::RemoveExternal { attach_id, name } => {
                Some(Command::RemoveExternal { attach_id, name })
            }
            ConfirmKind::DismissMember { squad, attach_id } => {
                Some(Command::DismissMember { squad, attach_id })
            }
            ConfirmKind::ClearDead { .. } | ConfirmKind::CloseTab { .. } => None,
        }
    }
}
