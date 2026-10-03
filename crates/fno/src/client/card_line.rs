//! Shared sideline card fields and hit ranges.

use std::ops::Range;

use unicode_width::UnicodeWidthStr;

use crate::proto::{AgentRow, BacklogCard};

/// Keep node and PR adjacent at the panel's right edge, separated by ` · `.
pub(super) fn identity_spans(a: &AgentRow, text_w: usize) -> IdentitySpans {
    let node = a.node.as_deref().filter(|s| !s.is_empty());
    let node_w = node.map_or(0, |s| s.width());
    let pr_w = a.pr.map(|n| format!("#{n}").width()).filter(|w| *w <= 6);
    let pr = pr_w.filter(|w| *w <= text_w).map(|w| text_w - w..text_w);
    let gap = usize::from(node_w > 0 && pr.is_some()) * 3;
    let node_end = pr
        .as_ref()
        .map_or(text_w, |span| span.start.saturating_sub(gap));
    let node_span = (node_w > 0 && node_end >= node_w).then(|| node_end - node_w..node_end);
    let separator = node_span
        .as_ref()
        .zip(pr.as_ref())
        .map(|(node, pr)| node.end..pr.start);
    IdentitySpans {
        node: node_span,
        separator,
        pr,
    }
}

pub(super) fn node_span(a: &AgentRow, text_w: usize) -> Option<Range<usize>> {
    identity_spans(a, text_w).node
}

pub(super) fn pr_span(a: &AgentRow, text_w: usize) -> Option<Range<usize>> {
    identity_spans(a, text_w).pr
}

pub(super) struct IdentitySpans {
    pub node: Option<Range<usize>>,
    pub separator: Option<Range<usize>>,
    pub pr: Option<Range<usize>>,
}

pub(super) fn metrics(a: &AgentRow, status: &str) -> String {
    let context = a.context_used_pct.map_or_else(
        || "????  ?".into(),
        |pct| super::row_meter::ctx_meter_text(pct, 4),
    );
    let count = a
        .compaction_count
        .map_or_else(|| "?c".into(), |n| format!("{n}c"));
    let cost = super::row_meter::cost_cell(a.session_cost_cents);
    format!("{context} · {count} · {cost} · {status}")
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
/// now shows in their own columns. `t-<node>-opus` keeps only `t`, so it
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
