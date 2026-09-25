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

#[cfg(test)]
mod tests {
    use super::*;

    // AC6-HP: the builder carries the label chain plus the new fields.
    #[test]
    fn builder_carries_label_node_branch_ctx() {
        let m = pane_meta(
            3,
            None,
            Some("x-0e67"),
            "/home/u/proj",
            Some("claude"),
            Some("main"),
            Some("49%"),
        );
        assert_eq!(m.id, 3);
        assert_eq!(m.label, "claude");
        assert_eq!(m.node.as_deref(), Some("x-0e67"));
        assert_eq!(m.branch.as_deref(), Some("main"));
        assert_eq!(m.ctx.as_deref(), Some("49%"));
    }

    #[test]
    fn builder_drops_missing_facts() {
        let m = pane_meta(4, None, None, "/home/u/proj", None, None, None);
        assert_eq!(m.label, "proj");
        assert_eq!((m.node, m.branch, m.ctx), (None, None, None));
    }
}
