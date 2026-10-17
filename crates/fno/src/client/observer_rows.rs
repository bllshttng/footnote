//! Observers on the sideline: the lines that tell the driver who watches
//! which pane. They ride the org block's bottom slot (`org_block.rs`), so the
//! agent rows never move, and they vanish with the last observer.

use super::super::{LayoutView, View};
use crate::proto::{BacklogCard, ObserverRow};

/// Observer lines shown before the rest fold into one `+N more` line.
const MAX_LINES: usize = 3;

/// Cut `line` to `width` characters, ending in `…` when it had to shrink, so a
/// long observer name reads as clipped instead of as a different name.
fn clip(line: String, width: usize) -> String {
    if line.chars().count() <= width {
        return line;
    }
    let mut out: String = line.chars().take(width.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// One line per observer, `◉ teacher · ssh · lead-5317`, then a `+N more`
/// line when the list is longer than [`MAX_LINES`]. `pane_name` names the
/// watched pane; an unnamed pane reads `pane 3`.
fn lines(observers: &[ObserverRow], pane_name: impl Fn(u64) -> Option<String>) -> Vec<String> {
    let mut out: Vec<String> = observers
        .iter()
        .take(MAX_LINES)
        .map(|o| {
            let pane = pane_name(o.pane_id).unwrap_or_else(|| format!("pane {}", o.pane_id));
            format!("\u{25c9} {} \u{b7} {} \u{b7} {pane}", o.name, o.via.label())
        })
        .collect();
    if observers.len() > MAX_LINES {
        out.push(format!(
            "\u{25c9} +{} more watching",
            observers.len() - MAX_LINES
        ));
    }
    out
}

impl View {
    /// Take one `Layout` message: the rows, the board cards, the server's wire
    /// version and the observers, in the order every consumer expects.
    pub(in crate::client) fn apply_layout(
        &mut self,
        layout: LayoutView,
        backlog: Vec<BacklogCard>,
        proto: Option<u32>,
        observers: Vec<ObserverRow>,
    ) {
        self.set_layout(layout);
        self.backlog = backlog;
        self.server_proto = proto;
        self.observers = observers;
    }

    /// How many lines [`View::observer_lines`] returns, without building them:
    /// the block-height math runs on every hit test.
    pub(in crate::client) fn observer_line_count(&self) -> usize {
        let n = self.observers.len();
        n.min(MAX_LINES) + usize::from(n > MAX_LINES)
    }

    /// The sideline lines for the current observers, each fitted to `width`;
    /// empty when nobody watches.
    pub(in crate::client) fn observer_lines(&self, width: usize) -> Vec<String> {
        lines(&self.observers, |pane| {
            self.layout
                .agents
                .iter()
                .find(|a| a.pane_id == Some(pane))
                .map(|a| a.name.clone())
        })
        .into_iter()
        .map(|l| clip(l, width))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ObserverVia;

    fn row(pane_id: u64, name: &str, via: ObserverVia) -> ObserverRow {
        ObserverRow {
            pane_id,
            name: name.into(),
            via,
        }
    }

    #[test]
    fn observers_list_with_route_and_pane_and_fold_past_three() {
        assert!(lines(&[], |_| None).is_empty(), "nobody watches: no lines");

        let two = [
            row(3, "teacher", ObserverVia::Ssh),
            row(9, "amy", ObserverVia::Share),
        ];
        let got = lines(&two, |p| (p == 3).then(|| "lead-5317".to_string()));
        assert_eq!(
            got,
            [
                "\u{25c9} teacher \u{b7} ssh \u{b7} lead-5317",
                "\u{25c9} amy \u{b7} browser \u{b7} pane 9"
            ]
        );

        let five: Vec<ObserverRow> = (0..5)
            .map(|i| row(1, &format!("v{i}"), ObserverVia::Share))
            .collect();
        let got = lines(&five, |_| None);
        assert_eq!(got.len(), 4);
        assert_eq!(got[3], "\u{25c9} +2 more watching");
    }

    #[test]
    fn a_long_observer_line_ends_in_an_ellipsis_at_the_width() {
        let line = "\u{25c9} teacher \u{b7} ssh \u{b7} lead-5317".to_string();
        assert_eq!(clip(line.clone(), 40), line);
        let cut = clip(line, 12);
        assert_eq!(cut.chars().count(), 12);
        assert!(cut.ends_with('\u{2026}'), "{cut}");
    }
}
