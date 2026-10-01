//! What a sideline card's two lines say: the slug, the `harness/model`
//! lead of line 2, and the context-bar + node cell of line 1. The paint and
//! the click resolver both read [`meter_node`], so a node tap lands where
//! the node is drawn.

use std::ops::Range;

use unicode_width::UnicodeWidthStr;

use super::row_meter::{ctx_bar, CTX_BAR_CELLS, CTX_BAR_W};
use crate::proto::{AgentRow, BacklogCard};

/// Line 1's context cell: the bar, then the node. A narrow cell drops the
/// bar first, then the node; neither is ever ellipsized.
pub(super) struct MeterNode {
    pub text: String,
    /// The bar's fill columns, relative to the cell start.
    pub fill: Option<Range<usize>>,
    /// The node's columns, relative to the cell start: the tap target.
    pub node: Option<Range<usize>>,
}

pub(super) fn meter_node(a: &AgentRow, width: usize) -> MeterNode {
    let bar = ctx_bar(a.context_used_pct);
    let node = a.node.as_deref().unwrap_or("");
    let node_w = node.width();
    // `[`, the digits, `%|`, then the fill cells up to the closing `]`.
    let fill = a.context_used_pct.map(|pct| {
        let start = pct.to_string().len() + 3;
        start..start + CTX_BAR_CELLS
    });
    if node_w > 0 && width > CTX_BAR_W + node_w {
        let at = CTX_BAR_W + 1;
        return MeterNode {
            text: format!("{bar} {node}"),
            fill,
            node: Some(at..at + node_w),
        };
    }
    if node_w > 0 && width >= node_w {
        return MeterNode {
            text: node.to_string(),
            fill: None,
            node: Some(0..node_w),
        };
    }
    if node_w == 0 && width >= CTX_BAR_W {
        return MeterNode {
            text: bar,
            fill,
            node: None,
        };
    }
    MeterNode {
        text: String::new(),
        fill: None,
        node: None,
    }
}

/// The card's short model: no `[1m]` window tag, and a `claude-` id names
/// its family (`claude-opus-5-5` -> `opus`).
fn short_model(model: &str) -> &str {
    let model = model.split('[').next().unwrap_or(model);
    match model.strip_prefix("claude-") {
        Some(rest) => rest.split('-').next().unwrap_or(rest),
        None => model,
    }
}

/// Line 2's lead: `claude/opus`, `codex/gpt-6.1-sol`, or whichever half is
/// known.
pub(super) fn harness_model(a: &AgentRow) -> Option<String> {
    let model = a
        .model
        .as_deref()
        .map(short_model)
        .filter(|m| !m.is_empty());
    match (a.harness.as_deref(), model) {
        (Some(h), Some(m)) => Some(format!("{h}/{m}")),
        (Some(h), None) => Some(h.to_string()),
        (None, Some(m)) => Some(m.to_string()),
        (None, None) => None,
    }
}

/// The card's name: the worker name without the node and model the card
/// now shows in their own columns. `t-x-5316-opus` keeps only `t`, so it
/// reads `t-<node slug>` when the board knows the node; with no slug, it
/// keeps the node rather than show a bare prefix.
pub(super) fn slug(a: &AgentRow, backlog: &[BacklogCard]) -> String {
    let model_tokens: Vec<&str> = a
        .model
        .as_deref()
        .map(|m| m.split('[').next().unwrap_or(m).split('-').collect())
        .unwrap_or_default();
    let strip_model = |name: &str| -> String {
        match name.rsplit_once('-') {
            Some((head, last))
                if !head.is_empty()
                    && model_tokens.iter().any(|t| t.eq_ignore_ascii_case(last)) =>
            {
                head.to_string()
            }
            _ => name.to_string(),
        }
    };
    let Some(node) = a.node.as_deref().filter(|n| !n.is_empty()) else {
        return strip_model(&a.name);
    };
    let bare = node.replace('-', "");
    let parts: Vec<&str> = a.name.split('-').collect();
    let node_parts: Vec<&str> = node.split('-').collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        if parts[i..].starts_with(&node_parts) {
            i += node_parts.len();
        } else if parts[i] == bare {
            i += 1;
        } else {
            kept.push(parts[i]);
            i += 1;
        }
    }
    if kept.len() == parts.len() {
        return strip_model(&a.name);
    }
    let rest = strip_model(&kept.join("-"));
    if rest.contains('-') {
        return rest;
    }
    // Only a prefix (or nothing) is left: the node was the name's substance.
    match backlog
        .iter()
        .find(|c| c.id == node)
        .map(|c| c.slug.as_str())
    {
        Some(s) if !s.is_empty() && rest.is_empty() => s.to_string(),
        Some(s) if !s.is_empty() => format!("{rest}-{s}"),
        _ => strip_model(&a.name),
    }
}
