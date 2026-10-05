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

/// One metrics-line field's render state: a served value, a loading skeleton
/// (the fold has not landed yet), or hidden - this harness or role cannot
/// report it, so it never prints a `?`.
pub(super) enum MetricCell {
    Value(String),
    Loading,
    Hidden,
}

/// The four metrics-line fields in paint order: activity, context,
/// compactions, tokens. Cost moved to line 2 (the operator's 2026-10-04
/// mockup; ruled d-027912c6), where card_detail_text paints it served-only.
/// Hidden outranks served: codex transcripts carry no context window or
/// compaction boundaries, so those two fields hide there, while its tool
/// calls land like any other transcript. Only claude and codex transcripts
/// resolve at all (`SessionTranscripts::find`), so any other harness shows
/// a dash for the activity cell - never a fabricated flat line - and a
/// bare pane or an exited row's unserved fields hide instead of pulsing.
pub(super) fn metric_cells(a: &AgentRow) -> [MetricCell; 4] {
    let reportable = !a.exited && matches!(a.harness.as_deref(), Some("claude" | "codex"));
    let codex = a.harness.as_deref() == Some("codex");
    let field = |value: Option<String>, hidden: bool| {
        if hidden {
            MetricCell::Hidden
        } else {
            match value {
                Some(v) => MetricCell::Value(v),
                None if !reportable => MetricCell::Hidden,
                None => MetricCell::Loading,
            }
        }
    };
    let activity = match a.harness.as_deref() {
        Some("claude" | "codex") => match activity_cell(a) {
            Some(cell) => MetricCell::Value(cell.text),
            None => field(None::<String>, false),
        },
        // A transcriptless harness (opencode, pi): a dash, never a fake zero.
        Some(_) => MetricCell::Value("-".into()),
        // A bare pane has nothing that could ever land: hidden.
        None => MetricCell::Hidden,
    };
    [
        activity,
        field(a.context_used_pct.map(|p| format!("{p}%")), codex),
        field(a.compaction_count.map(|n| format!("{n}c")), codex),
        field(a.session_tokens.map(super::row_meter::token_cell), false),
    ]
}

/// The line-3 activity cell: the served intervals from the server's ring,
/// drawn as up to 8 ramp cells scaled to the card's own max, one height per
/// refresh interval. Fewer than two intervals reads as the narrow 5-cell
/// fill bar of the latest level; none reads as waiting (the skeleton). The
/// ramp glyph set carries all eight block heights so one row still shows
/// shape; the painter colors each cell by its failed share.
const ACTIVITY_CELLS: usize = 8;
const FILL_BAR_CELLS: usize = 5;
const ACTIVITY_RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub(super) struct ActivityCell {
    /// The glyphs, no percent suffix: context is a plain number beside it.
    pub text: String,
    /// One color kind per glyph (0 ok, 1 warn, 2 error), same length.
    pub kinds: Vec<u8>,
}

/// The failed-share color stops: zero failures paints ok, any share under
/// half paints warn, half or more paints error.
fn color_kind(calls: u8, failed: u8) -> u8 {
    if calls == 0 || failed == 0 {
        0
    } else if failed * 2 < calls {
        1
    } else {
        2
    }
}

pub(super) fn activity_cell(a: &AgentRow) -> Option<ActivityCell> {
    let pairs = a.activity.as_ref()?;
    if pairs.is_empty() {
        return None; // no interval closed yet: the skeleton waits
    }
    if pairs.len() < 2 {
        // The narrow fill bar of the latest level: busy or idle at the
        // coarsest resolution. One interval has no share to grade, so the
        // bar paints in the color that pair earns and the rest stays ok.
        let (calls, failed) = pairs[0];
        let eighths = if calls == 0 { 0 } else { FILL_BAR_CELLS * 8 };
        let bar = super::row_meter::bar_of(eighths, FILL_BAR_CELLS);
        let kind = color_kind(calls, failed);
        let kinds = bar
            .chars()
            .map(|c| if c == ' ' { 0 } else { kind })
            .collect();
        return Some(ActivityCell { text: bar, kinds });
    }
    let max = pairs.iter().map(|p| p.0).max().unwrap_or(0);
    let pad = ACTIVITY_CELLS.saturating_sub(pairs.len());
    let mut text: String = std::iter::repeat('▁').take(pad).collect();
    let mut kinds: Vec<u8> = std::iter::repeat(0).take(pad).collect();
    for (calls, failed) in pairs {
        text.push(ramp_height(*calls, max));
        kinds.push(color_kind(*calls, *failed));
    }
    Some(ActivityCell { text, kinds })
}

/// The height map: zero reads the flat baseline; everything else scales to
/// the card's own max over the 8 cells.
fn ramp_height(calls: u8, max: u8) -> char {
    if calls == 0 || max == 0 {
        ACTIVITY_RAMP[0]
    } else {
        ACTIVITY_RAMP[(usize::from(calls) * ACTIVITY_CELLS / usize::from(max)).clamp(1, 7)]
    }
}

/// Past this age a Loading field gives up the pulse and holds a static dash.
const LOADING_DASH_AFTER_S: u64 = 10;

/// Whether any field still waits on the fold: the breathe timer's arm signal.
/// A row whose Loading fields are all past [`LOADING_DASH_AFTER_S`] holds a
/// static dash and arms no more frames.
pub(super) fn has_loading(a: &AgentRow, now: u64) -> bool {
    metric_cells(a)
        .iter()
        .any(|c| matches!(c, MetricCell::Loading))
        && !loading_gave_up(a, now)
}

/// Past 10s a row's Loading fields hold a static dash: the pulse gave up. A
/// row with no `started_at` reads as fully aged - reportable rows set it at
/// spawn, so this covers a wire gap rather than a real case.
pub(super) fn loading_gave_up(a: &AgentRow, now: u64) -> bool {
    now.saturating_sub(a.started_at.unwrap_or(0)) > LOADING_DASH_AFTER_S
}

/// Skeleton widths mirror each field's served width (ramp, percent, count,
/// tokens) so a landing fold does not reflow the line.
const LOADING_W: [usize; 4] = [8, 4, 3, 8];

pub(super) fn metrics(a: &AgentRow, now: u64, message: Option<&str>, width: usize) -> String {
    let phase = crate::lattice::spin_epoch().map(|t0| t0.elapsed().as_millis() as u64);
    let gave_up = loading_gave_up(a, now);
    let fields: Vec<String> = metric_cells(a)
        .iter()
        .enumerate()
        .filter_map(|(i, c)| match c {
            MetricCell::Value(v) => Some(v.clone()),
            MetricCell::Loading if !gave_up => {
                Some(super::row_meter::skeleton_cell(LOADING_W[i], phase))
            }
            // The fold never landed: a static dash at the field's own width
            // keeps the line from reflowing while it stops the pulse.
            MetricCell::Loading => Some(format!("{:<w$}", "-", w = LOADING_W[i])),
            MetricCell::Hidden => None,
        })
        .collect();
    let prefix = fields.join(" · ");
    let Some(message) = message.filter(|s| !s.is_empty()) else {
        return crate::chrome::clip(&prefix, width);
    };
    if prefix.is_empty() {
        // Every field hid: the message stands alone, no leading separator.
        return crate::chrome::clip(message, width);
    }
    let separator = " · ";
    let room = width.saturating_sub(crate::chrome::str_cols(&prefix));
    let separator_w = crate::chrome::str_cols(separator);
    if room <= separator_w {
        return crate::chrome::clip(&prefix, width);
    }
    format!(
        "{prefix}{separator}{}",
        crate::chrome::fit_ellipsis(message, room - separator_w)
    )
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

/// Line 2's model, shortened to the name users recognize.
pub(super) fn model_label(a: &AgentRow) -> Option<String> {
    a.model
        .as_deref()
        .map(short_model)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// The board's model badge includes the harness; card line 2 keeps just the
/// model label so the lead name or role remains visible.
pub(super) fn harness_model(a: &AgentRow) -> Option<String> {
    match (a.harness.as_deref(), model_label(a)) {
        (Some(h), Some(m)) => Some(format!("{h}/{m}")),
        (Some(h), None) => Some(h.to_string()),
        (None, model) => model,
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
