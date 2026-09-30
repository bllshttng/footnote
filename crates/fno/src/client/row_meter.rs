//! Worker readings formatted without probing or changing their measurement time.

use super::*;

pub(super) fn ctx_cell(pct: Option<u8>) -> String {
    let Some(pct) = pct else { return "-".into() };
    let pct = pct.min(100);
    let filled = (u16::from(pct) * 3).div_ceil(100) as usize;
    format!("{pct}%{}{}", "▪".repeat(filled), "▫".repeat(3 - filled))
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
