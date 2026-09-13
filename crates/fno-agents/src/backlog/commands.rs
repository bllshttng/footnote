//! The canonical backlog action catalog (x-920a wave 4).
//!
//! One table owns the vocabulary: eleven canonical groups, every legacy name
//! mapped to exactly one `(group, action)` pair, `annotate` named as the
//! explicit exception it remains until x-26bd replaces it. The catalog is
//! the single source both the compatibility help and any future dispatcher
//! must read - no second handwritten mapping in help or completion.
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One canonical group: its purpose line and the action vocabulary, where
/// each action names the legacy first-level command it dispatches to (the
/// group's action argument is the new spelling; the legacy name is kept for
/// the compatibility window).
#[derive(Debug)]
pub struct Group {
    pub name: &'static str,
    pub purpose: &'static str,
    /// `(action, legacy_command)` pairs: `fno backlog <group> <action>`
    /// dispatches to the legacy command `<legacy_command>`.
    pub actions: &'static [(&'static str, &'static str)],
}

impl Group {
    /// The action for a legacy command, if the table knows it.
    pub fn action_for_legacy(command: &str) -> Option<(&'static str, &'static str)> {
        GROUPS.iter().find_map(|g| {
            g.actions
                .iter()
                .find(|(_, legacy)| *legacy == command)
                .map(|(action, _)| (g.name, *action))
        })
    }
}

/// The canonical table, in plan order.
pub const GROUPS: &[Group] = &[
    Group {
        name: "add",
        purpose: "Create or import a node",
        actions: &[
            ("node", "add"),
            ("idea", "idea"),
            ("scoped", "new"),
            ("plan", "intake"),
        ],
    },
    Group {
        name: "get",
        purpose: "Read one node and its provenance",
        actions: &[
            ("node", "get"),
            ("status", "status"),
            ("provenance", "provenance"),
            ("project-root", "project-root"),
            ("version", "version"),
        ],
    },
    Group {
        name: "find",
        purpose: "Search node text",
        actions: &[("text", "find")],
    },
    Group {
        name: "list",
        purpose: "Read saved node sets",
        actions: &[
            ("next", "next"),
            ("ready", "ready"),
            ("queued", "queued"),
            ("worked", "worked"),
            ("lanes", "lanes"),
            ("undispatched", "undispatched"),
            ("stuck-epics", "stuck-epics"),
        ],
    },
    Group {
        name: "update",
        purpose: "Change node fields",
        actions: &[
            ("node", "update"),
            ("rank", "rank"),
            ("priority", "reprioritize"),
        ],
    },
    Group {
        name: "note",
        purpose: "Record progress or a blocking finding",
        actions: &[("state", "note")],
    },
    Group {
        name: "move",
        purpose: "Change lifecycle or queue state",
        actions: &[
            ("done", "done"),
            ("reopen", "reopen"),
            ("defer", "defer"),
            ("undefer", "undefer"),
            ("queue", "queue"),
            ("unqueue", "unqueue"),
            ("requeue", "requeue"),
            ("unclaim", "unclaim"),
            ("retract", "retract"),
        ],
    },
    Group {
        name: "link",
        purpose: "Change node relationships",
        actions: &[
            ("contain", "contain"),
            ("decompose", "decompose"),
            ("relatedness", "relatedness"),
            ("collisions", "collisions"),
            ("supersede", "supersede"),
            ("unsupersede", "unsupersede"),
            ("carveout", "carveout"),
            ("epic", "epic"),
        ],
    },
    Group {
        name: "board",
        purpose: "Show work and demand",
        actions: &[
            ("summary", "board"),
            ("open", "view"),
            ("roadmap", "roadmap"),
            ("album", "album"),
            ("bases", "bases"),
            ("demand", "demand"),
            ("cost", "cost"),
        ],
    },
    Group {
        name: "triage",
        purpose: "Analyze and order work",
        actions: &[
            ("run", "triage"),
            ("groom", "groom"),
            ("discover", "discover"),
            ("pick", "pick"),
            ("encounter", "encounter"),
        ],
    },
    Group {
        name: "admin",
        purpose: "Operate storage and automation",
        actions: &[
            ("advance", "advance"),
            ("archive", "archive"),
            ("archive-dedupe-ids", "archive-dedupe-ids"),
            ("backfill-deferred-kind", "backfill-deferred-kind"),
            ("batch", "batch"),
            ("capture", "capture"),
            ("decide", "decide"),
            ("decide-reindex", "decide-reindex"),
            ("decide-retract", "decide-retract"),
            ("decisions", "decisions"),
            ("dispatch-lanes", "dispatch-lanes"),
            ("join", "join"),
            ("lane-fill", "lane-fill"),
            ("maintain", "maintain"),
            ("migrate-difficulty", "migrate-difficulty"),
            ("migrate-priorities", "migrate-priorities"),
            ("migrate-updated-at", "migrate-updated-at"),
            ("reconcile", "reconcile"),
            ("reconcile-findings", "reconcile-findings"),
            ("remove", "remove"),
            ("render-views", "render-views"),
            ("retro", "retro"),
            ("session", "session"),
            ("task", "task"),
            ("unarchive", "unarchive"),
        ],
    },
];

/// Resolve `group + action` to the legacy command it dispatches to.
pub fn resolve(group: &str, action: &str) -> Option<&'static str> {
    GROUPS
        .iter()
        .find(|g| g.name == group)?
        .actions
        .iter()
        .find(|(a, _)| *a == action)
        .map(|(_, legacy)| *legacy)
}

/// Resolve a bare legacy command to `(group, action)`.
pub fn from_legacy(command: &str) -> Option<(&'static str, &'static str)> {
    GROUPS.iter().find_map(|g| {
        g.actions
            .iter()
            .find(|(_, legacy)| *legacy == command)
            .map(|(action, _)| (g.name, *action))
    })
}

/// The catalog as JSON: groups, actions, legacy names, counts.
pub fn catalog_json() -> Value {
    let groups: Vec<Value> = GROUPS
        .iter()
        .map(|g| {
            json!({
                "group": g.name,
                "purpose": g.purpose,
                "actions": g.actions.iter().map(|(a, l)| json!({
                    "action": a,
                    "legacy": l,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "groups": groups,
        "group_count": GROUPS.len(),
        "mapped_names": GROUPS.iter().map(|g| g.actions.len()).sum::<usize>(),
        "exception": "annotate remains callable with its existing nested actions until x-26bd replaces it",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_legacy_name_maps_exactly_once() {
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for g in GROUPS {
            for (_, legacy) in g.actions {
                assert_eq!(
                    seen.insert(legacy, g.name),
                    None,
                    "{legacy} maps into both {seen:?} and {g:?}"
                );
            }
        }
    }

    #[test]
    fn version_and_retract_are_included() {
        assert_eq!(
            Group::action_for_legacy("version"),
            Some(("get", "version"))
        );
        assert_eq!(
            Group::action_for_legacy("retract"),
            Some(("move", "retract"))
        );
    }

    #[test]
    fn resolution_round_trips_both_directions() {
        assert_eq!(resolve("note", "state"), Some("note"));
        assert_eq!(from_legacy("note"), Some(("note", "state")));
        assert_eq!(resolve("add", "scoped"), Some("new"));
        assert_eq!(from_legacy("new"), Some(("add", "scoped")));
        assert_eq!(resolve("nope", "x"), None);
        assert_eq!(from_legacy("nope"), None);
    }
}
