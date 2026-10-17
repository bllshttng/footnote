//! `pane_label` and the pure `PaneMeta` builder, moved out of server.rs under
//! the file-budget ratchet. server.rs re-imports the label fn so the
//! existing callers and tests resolve unchanged.

use super::sanitize_tab_name;
use crate::agents_view::RegistryAgent;
use crate::proto::{PaneMeta, TabMeta};
use crate::tree::{self, Tab};
use std::collections::HashMap;

/// A pane's display label for the session navigator (v22). Unlike
/// `tab_label` (which prefers a dir name so a tab reads as its worktree), a
/// pane's discriminator WITHIN a tab is what it is running, so `cmd` leads when
/// the pane carries no registered name. Chain: registered name
/// (`FNO_AGENT_SELF`) -> `cmd` -> `node` -> cwd basename -> `shell`. Sanitized
/// like a wire name (these land in chrome cells). Never an ordinal - a plain
/// pane is `shell`, not a number the operator cannot map back.
pub(crate) fn pane_label(
    name: Option<&str>,
    node: Option<&str>,
    cwd: &str,
    cmd: Option<&str>,
) -> String {
    for c in [name, cmd, node].into_iter().flatten() {
        let clean = sanitize_tab_name(c);
        if !clean.is_empty() {
            return clean;
        }
    }
    let base = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let clean = sanitize_tab_name(base);
    if clean.is_empty() {
        "shell".to_string()
    } else {
        clean
    }
}

/// The pure `PaneMeta` builder: the label chain plus the frame's bottom-edge
/// facts. A missing fact takes `None` and the client drops the field.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_meta(
    id: u64,
    name: Option<&str>,
    node: Option<&str>,
    cwd: &str,
    cmd: Option<&str>,
    branch: Option<&str>,
    ctx: Option<&str>,
) -> PaneMeta {
    PaneMeta {
        id,
        label: pane_label(name, node, cwd, cmd),
        node: node.map(str::to_string),
        branch: branch.map(str::to_string),
        ctx: ctx.map(str::to_string),
    }
}

/// A tab's display label, from spawn-time facts only - no I/O, no
/// subprocess on the layout path (squad.rs's origin-freeze discipline).
/// Chain: explicit rename > registered name (`FNO_AGENT_SELF`) >
/// `FNO_NODE` provenance > spawn-cwd basename when it differs from the squad's
/// > command basename > the bare 1-based index (so a plain shell tab renders
/// unchanged). `pane` is the focused pane's `(name, node, cwd, cmd)`; `None`
/// (a reaped pane racing tree cleanup) falls through to the index - the
/// derivation never panics on a missing pane.
#[allow(clippy::type_complexity)]
pub(crate) fn tab_label(
    rename: Option<&str>,
    pane: Option<(Option<&str>, Option<&str>, &str, Option<&str>)>,
    squad_cwd: &str,
    i: usize,
) -> String {
    if let Some(name) = rename {
        return name.to_string();
    }
    if let Some((name, node, cwd, cmd)) = pane {
        // Every derived candidate is sanitized like a rename (codex peer
        // review): FNO_NODE values, dir names, and argv all admit control
        // bytes, and these strings land in chrome cells. A candidate that
        // sanitizes to empty (e.g. whitespace-only) falls through to the
        // next source instead of rendering a blank label.
        if let Some(name) = name {
            let clean = sanitize_tab_name(name);
            if !clean.is_empty() {
                return clean;
            }
        }
        if let Some(node) = node {
            let clean = sanitize_tab_name(node);
            if !clean.is_empty() {
                return clean;
            }
        }
        fn base(p: &str) -> &str {
            p.trim_end_matches('/').rsplit('/').next().unwrap_or("")
        }
        let cwd_base = base(cwd);
        if !cwd_base.is_empty() && cwd_base != base(squad_cwd) {
            let clean = sanitize_tab_name(cwd_base);
            if !clean.is_empty() {
                return clean;
            }
        }
        if let Some(cmd) = cmd {
            let clean = sanitize_tab_name(cmd);
            if !clean.is_empty() {
                return clean;
            }
        }
    }
    (i + 1).to_string()
}

/// The registry row's CURRENT label for one pane, joined the same way
/// [`pane_ctx`] joins: the row whose `mux` names this session and pane. A
/// rename rewrites only this row (the pane's `FNO_AGENT_SELF` is env, frozen
/// at spawn), so the frame and tab chrome read it at layout time and fall
/// back to the spawn-captured name. `pane_pid` is the pane's current child
/// pid: a recycled pane id reuses the (session, pane) pair across server
/// incarnations, so a row whose recorded pid disagrees is a dead row wearing
/// the pair and must not name the pane. A row that records no pid (legacy)
/// cannot be falsified, and a caller that cannot know the child (`None`)
/// keeps the unguarded read.
pub(crate) fn pane_registry_name(
    agents: &[crate::agents_view::RegistryAgent],
    session_name: &str,
    pane_id: u64,
    pane_pid: Option<u32>,
) -> Option<String> {
    hosts_pane(agents, session_name, pane_id, pane_pid)
        // A recycled pane id can leave an exited row on the same
        // (session, pane); the live row is the one still hosting the pane.
        .find(|a| !a.exited)
        .or_else(|| hosts_pane(agents, session_name, pane_id, pane_pid).next())
        .map(|a| a.name.clone())
}

/// The rows eligible to answer for one pane: the (session, pane) pair, and
/// - when both the pane's child pid and the row's recorded pid are known -
/// an equality check between them. A recycled pane id reuses the pair
/// across server incarnations, so a pid mismatch is a dead row wearing the
/// pair. A row that records no pid (legacy) cannot be falsified, and a
/// caller that cannot know the child (`None`) keeps the unguarded read.
fn hosts_pane<'a>(
    agents: &'a [RegistryAgent],
    session_name: &'a str,
    pane_id: u64,
    pane_pid: Option<u32>,
) -> impl Iterator<Item = &'a RegistryAgent> + 'a {
    agents.iter().filter(move |a| {
        matches!(&a.mux, Some((s, p)) if s == session_name && *p == pane_id)
            && match (pane_pid, a.pid) {
                (Some(child), Some(row_pid)) => u64::from(child) == row_pid,
                _ => true,
            }
    })
}

/// One tab's `TabMeta` for the client `Layout` (moved out of server.rs under
/// the file-budget ratchet): the derived tab label from the focused pane
/// (registry name first, gated on the pane's child pid so a recycled pane id
/// cannot inherit a dead row's name), plus every leaf pane labelled from its
/// own entry so the navigator can goto a pane in any tab/squad.
pub(crate) fn tab_meta(
    t: &Tab,
    squad_cwd: &str,
    index: usize,
    agents: &[RegistryAgent],
    session_name: &str,
    panes: &HashMap<u64, super::PaneEntry>,
    ctx_by_session: &HashMap<String, String>,
    branch_by_cwd: &HashMap<String, String>,
) -> TabMeta {
    // The registry row hosting the focus pane carries the LIVE name (a
    // rename rewrites the row, never the pane's spawn-captured env), so it
    // leads the derived chain ahead of `FNO_AGENT_SELF`.
    let focus_entry = panes.get(&t.focus);
    let focus_reg = pane_registry_name(
        agents,
        session_name,
        t.focus,
        focus_entry.and_then(|e| e.pty.child_pid()),
    );
    TabMeta {
        id: t.id,
        // An explicit rename is the ONLY chosen name; a pane-derived or
        // ordinal label is not. The client renders a chosen name without a
        // forced ordinal.
        named: t.name.is_some(),
        name: tab_label(
            t.name.as_deref(),
            focus_entry.map(|e| {
                (
                    focus_reg.as_deref().or(e.name.as_deref()),
                    e.node.as_deref(),
                    e.cwd.as_str(),
                    e.cmd.as_deref(),
                )
            }),
            squad_cwd,
            index,
        ),
        panes: tree::leaves(&t.root)
            .iter()
            .map(|pid| {
                let e = panes.get(pid);
                let pane_pid = e.and_then(|e| e.pty.child_pid());
                let ctx = pane_ctx(agents, session_name, ctx_by_session, *pid, pane_pid);
                let reg = pane_registry_name(agents, session_name, *pid, pane_pid);
                let name = reg.as_deref().or_else(|| e.and_then(|e| e.name.as_deref()));
                pane_meta(
                    *pid,
                    name,
                    e.and_then(|e| e.node.as_deref()),
                    e.map(|e| e.cwd.as_str()).unwrap_or(""),
                    e.and_then(|e| e.cmd.as_deref()),
                    e.and_then(|e| branch_by_cwd.get(&e.cwd))
                        .map(String::as_str),
                    ctx.as_deref(),
                )
            })
            .collect(),
    }
}

/// The context reading for one pane, joined through the registry row that
/// hosts it: the row whose `mux` names this session and pane, gated on the
/// pane's child pid the same way [`pane_registry_name`] gates the label
/// join. `None` reads as "no reading" and the frame drops the field.
pub(crate) fn pane_ctx(
    agents: &[crate::agents_view::RegistryAgent],
    session_name: &str,
    ctx_by_session: &std::collections::HashMap<String, String>,
    pane_id: u64,
    pane_pid: Option<u32>,
) -> Option<String> {
    hosts_pane(agents, session_name, pane_id, pane_pid)
        .next()
        .and_then(|a| {
            a.claude_session_uuid
                .clone()
                .or_else(|| a.harness_session_id.clone())
        })
        .and_then(|uuid| ctx_by_session.get(&uuid).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_view::RegistryAgent;

    // AC6-HP: the builder carries the label chain plus the new fields.
    #[test]
    fn builder_carries_label_node_branch_ctx() {
        let m = pane_meta(
            3,
            None,
            Some("node7"),
            "/home/u/proj",
            Some("claude"),
            Some("main"),
            Some("49%"),
        );
        assert_eq!(m.id, 3);
        assert_eq!(m.label, "claude");
        assert_eq!(m.node.as_deref(), Some("node7"));
        assert_eq!(m.branch.as_deref(), Some("main"));
        assert_eq!(m.ctx.as_deref(), Some("49%"));
    }

    #[test]
    fn builder_drops_missing_facts() {
        let m = pane_meta(4, None, None, "/home/u/proj", None, None, None);
        assert_eq!(m.label, "proj");
        assert_eq!((m.node, m.branch, m.ctx), (None, None, None));
    }

    // A rename rewrites the registry row; the pane's FNO_AGENT_SELF
    // env is frozen at spawn. The chrome reads the row (live row first);
    // the layout layer falls back to the spawn-captured name when this
    // returns None.
    #[test]
    fn registry_name_reads_the_hosting_row() {
        // A rename rewrites the registry row; the pane's FNO_AGENT_SELF
        // env is frozen at spawn. The chrome reads the row (live row
        // first); the layout layer falls back to the spawn-captured name
        // when this returns None. An unhosted pane reads None. A caller
        // that cannot know the child pid (`None`) keeps the unguarded
        // read.
        let agents = vec![
            agent("kestrel-successor", Some(("mux0", 7)), true, None),
            agent("bob", Some(("mux0", 7)), false, None),
        ];
        assert_eq!(
            pane_registry_name(&agents, "mux0", 7, None).as_deref(),
            Some("bob")
        );
        let other = vec![agent("other", Some(("mux0", 9)), false, None)];
        assert_eq!(pane_registry_name(&other, "mux0", 7, None), None);
    }

    // The recycle case: a server restart hands the same pane id to a new
    // pane, and the dead worker's row from the previous incarnation wears
    // the same (session, pane) pair. A row whose recorded pid disagrees
    // with the pane's current child is refused, so the label falls through
    // to the spawn-captured name instead of the dead worker's.
    #[test]
    fn recycled_pane_id_refuses_a_foreign_pid_row() {
        let stale = vec![agent("kestrel", Some(("mux0", 7)), true, Some(100))];
        assert_eq!(pane_registry_name(&stale, "mux0", 7, Some(200)), None);
        // Keeper re-adoption keeps the same child pid: the row still
        // names its pane across the restart.
        assert_eq!(
            pane_registry_name(&stale, "mux0", 7, Some(100)).as_deref(),
            Some("kestrel")
        );
        // A caller that cannot know the child pid keeps the unguarded
        // read, and a legacy row that records no pid cannot be falsified.
        assert_eq!(
            pane_registry_name(&stale, "mux0", 7, None).as_deref(),
            Some("kestrel")
        );
        assert_eq!(
            pane_registry_name(
                &[agent("legacy", Some(("mux0", 7)), true, None)],
                "mux0",
                7,
                Some(200)
            )
            .as_deref(),
            Some("legacy")
        );
        // The pid gate applies to a live row too: a row wearing the pair
        // with a foreign pid is not this pane's worker.
        let foreign_live = vec![agent("impostor", Some(("mux0", 7)), false, Some(999))];
        assert_eq!(
            pane_registry_name(&foreign_live, "mux0", 7, Some(200)),
            None
        );
    }

    fn agent(
        name: &str,
        mux: Option<(&str, u64)>,
        exited: bool,
        pid: Option<u64>,
    ) -> RegistryAgent {
        RegistryAgent {
            name: name.into(),
            mux: mux.map(|(s, p)| (s.to_string(), p)),
            exited,
            pid,
            ..Default::default()
        }
    }
}
