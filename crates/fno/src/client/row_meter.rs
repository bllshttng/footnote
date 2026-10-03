//! Worker readings formatted without probing or changing their measurement time.

use super::*;

/// Eighth-block fill levels, lightest to full: U+258F through U+2588 give
/// eight steps per cell, so a one-line meter reads smooth instead of the
/// crude `[48%|####    ]` block bar it replaced.
const EIGHTHS: [char; 8] = [
    '\u{258F}', '\u{258E}', '\u{258D}', '\u{258C}', '\u{258B}', '\u{258A}', '\u{2589}', '\u{2588}',
];

/// The smooth meter: `cells` glyph cells, then the percent after one space.
/// Partial fill shows the partial glyph in its own cell; the rest stays
/// blank. No brackets, no pipe, no ellipsis.
pub(super) fn ctx_meter_text(pct: u8, cells: usize) -> String {
    let eighths = (usize::from(pct.min(100)) * cells * 8).div_ceil(100);
    let mut bar = String::new();
    for i in 0..cells {
        let level = eighths.saturating_sub(i * 8).min(8);
        if level == 0 {
            bar.push(' ');
        } else {
            bar.push(EIGHTHS[level - 1]);
        }
    }
    format!("{bar} {pct}%")
}

/// The list row's context meter: three cells, percent after the bar.
/// Unmeasured reads `-`, never `0%`.
pub(super) fn ctx_cell(pct: Option<u8>) -> String {
    let Some(pct) = pct else { return "-".into() };
    ctx_meter_text(pct, 3)
}

/// The card's context meter, padded to [`CTX_BAR_W`] so the cost and node
/// after it line up card to card. The bar itself is [`CTX_BAR_CELLS`] wide.
pub(super) const CTX_BAR_W: usize = 13;
pub(super) const CTX_BAR_CELLS: usize = 8;

pub(super) fn ctx_bar(pct: Option<u8>) -> String {
    let Some(pct) = pct else {
        return format!("{:<CTX_BAR_W$}", "-");
    };
    format!("{:<CTX_BAR_W$}", ctx_meter_text(pct, CTX_BAR_CELLS))
}

/// A priced dollar amount or an unknown marker; never a token substitute.
pub(super) fn cost_cell(cents: Option<u64>) -> String {
    cents.map_or_else(
        || "?".into(),
        |c| {
            if c >= 100_000 {
                format!("${:.1}k", c as f64 / 100_000.0)
            } else {
                format!("${}.{:02}", c / 100, c % 100)
            }
        },
    )
}

/// The name column's width at a panel text width: the fixed cells and a
/// 22-column name leave `extra`, and the name takes 22 + extra/4, clamped
/// to `[22, 40]`. The description takes the most extra width, then the
/// title (operator ruling, d-36438ea4).
pub(super) fn name_w(text_w: u16, fixed_cells: u16, gaps: u16) -> u16 {
    let extra =
        usize::from(text_w).saturating_sub(usize::from(fixed_cells) + 22 + usize::from(gaps));
    (22 + extra / 4).clamp(22, 40) as u16
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

#[cfg(test)]
mod tests {
    use super::cost_cell;
    use super::name_w;
    #[test]
    fn cost_is_dollars_or_unknown() {
        assert_eq!(cost_cell(Some(42)), "$0.42");
        assert_eq!(cost_cell(Some(4497)), "$44.97");
        assert_eq!(cost_cell(Some(120_000)), "$1.2k");
        assert_eq!(cost_cell(Some(0)), "$0.00");
        assert_eq!(cost_cell(None), "?");
    }

    // The unit test the plan names: 50, 80, 120, 200 and 320 columns, both
    // layouts' fixed-cell sums.
    #[test]
    fn name_width_takes_a_quarter_of_the_surplus_within_clamps() {
        // Card: status 5 + slots 6 + 6 = 17 fixed, 4 gaps. List: 28 fixed,
        // 6 gaps.
        for text_w in [50u16, 80, 120, 200, 320] {
            let card = name_w(text_w, 17, 4);
            let list = name_w(text_w, 28, 6);
            assert!((22..=40).contains(&card), "{text_w}: {card}");
            assert!((22..=40).contains(&list), "{text_w}: {list}");
            assert!(
                card >= list,
                "the narrower fixed sum keeps more: {card} {list}"
            );
        }
        assert_eq!(name_w(50, 17, 4), 23);
        assert_eq!(name_w(320, 17, 4), 40);
    }
}
