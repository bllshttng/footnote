//! Worker readings formatted without probing or changing their measurement time.

use super::*;

pub(super) fn ctx_cell(pct: Option<u8>) -> String {
    let Some(pct) = pct else { return "-".into() };
    let filled = (u16::from(pct.min(100)) * 3).div_ceil(100) as usize;
    format!("{pct}%{}{}", "▪".repeat(filled), "▫".repeat(3 - filled))
}

/// The card's context bar, `[28%|###     ]`, padded to [`CTX_BAR_W`] so the
/// node after it lines up card to card. Unmeasured reads `-`, never `0%`.
pub(super) const CTX_BAR_W: usize = 15;
pub(super) const CTX_BAR_CELLS: usize = 8;
/// At or past this share the bar paints red: claude compacts near the top
/// of the window, so this is the warning before it happens.
pub(super) const CTX_NEAR_COMPACT_PCT: u8 = 80;

pub(super) fn ctx_bar(pct: Option<u8>) -> String {
    let bar = match pct {
        None => "-".to_string(),
        Some(pct) => {
            let filled = (usize::from(pct.min(100)) * CTX_BAR_CELLS).div_ceil(100);
            format!(
                "[{pct}%|{}{}]",
                "#".repeat(filled),
                " ".repeat(CTX_BAR_CELLS - filled)
            )
        }
    };
    format!("{bar:<CTX_BAR_W$}")
}

pub(super) fn up_cell(started_at: Option<u64>, now: u64) -> String {
    let Some(started_at) = started_at else {
        return "-".into();
    };
    let elapsed = now.saturating_sub(started_at);
    if elapsed >= 86400 {
        format!("{}d", elapsed / 86400)
    } else if elapsed >= 3600 {
        format!("{}h", elapsed / 3600)
    } else if elapsed >= 60 {
        format!("{}m", elapsed / 60)
    } else {
        format!("{elapsed}s")
    }
}

impl View {
    pub(super) fn table_rows_with_depths(&self) -> (Vec<DisplayRow<'_>>, Vec<usize>) {
        let (rows, depths) = self.tree_rows_with_depths();
        let (mut rows, mut depths) = self.sort_agent_runs(rows, depths);
        let has_agent = rows.iter().any(|row| matches!(row, DisplayRow::Agent(_)));
        rows.insert(0, DisplayRow::TableHead);
        depths.insert(0, 0);
        if !has_agent {
            rows.insert(1, DisplayRow::TableEmpty);
            depths.insert(1, 0);
        }
        self.card_rows(rows, depths)
    }
}
