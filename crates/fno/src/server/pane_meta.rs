//! `pane_label` and the pure `PaneMeta` builder, moved out of server.rs under
//! the file-budget ratchet. server.rs re-imports the label fn so the
//! existing callers and tests resolve unchanged.

use super::sanitize_tab_name;
use crate::proto::PaneMeta;

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
/// back to the spawn-captured name.
pub(crate) fn pane_registry_name(
    agents: &[crate::agents_view::RegistryAgent],
    session_name: &str,
    pid: u64,
) -> Option<String> {
    let mux_row = |a: &&crate::agents_view::RegistryAgent| matches!(&a.mux, Some((s, p)) if s == session_name && *p == pid);
    // A recycled pane id can leave an exited row on the same (session, pane);
    // the live row is the one still hosting the pane.
    agents
        .iter()
        .find(|a| mux_row(a) && !a.exited)
        .or_else(|| agents.iter().find(|a| mux_row(a)))
        .map(|a| a.name.clone())
}

/// The context reading for one pane, joined through the registry row that
/// hosts it: the row whose `mux` names this session and pane, keyed by the
/// same transcript identity the tail pass reads. `None` reads as "no
/// reading" and the frame drops the field.
pub(crate) fn pane_ctx(
    agents: &[crate::agents_view::RegistryAgent],
    session_name: &str,
    ctx_by_session: &std::collections::HashMap<String, String>,
    pid: u64,
) -> Option<String> {
    agents
        .iter()
        .find(|a| matches!(&a.mux, Some((s, p)) if s == session_name && *p == pid))
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
    fn registry_name_beats_the_spawn_captured_self() {
        let agents = vec![
            agent("kestrel-heir", Some(("mux0", 7)), true),
            agent("bob", Some(("mux0", 7)), false),
        ];
        let got = pane_registry_name(&agents, "mux0", 7);
        assert_eq!(got.as_deref(), Some("bob"));
    }

    #[test]
    fn pane_registry_name_is_none_for_an_unhosted_pane() {
        let agents = vec![agent("other", Some(("mux0", 9)), false)];
        assert_eq!(pane_registry_name(&agents, "mux0", 7), None);
    }

    fn agent(name: &str, mux: Option<(&str, u64)>, exited: bool) -> RegistryAgent {
        RegistryAgent {
            name: name.into(),
            mux: mux.map(|(s, p)| (s.to_string(), p)),
            exited,
            ..Default::default()
        }
    }
}
