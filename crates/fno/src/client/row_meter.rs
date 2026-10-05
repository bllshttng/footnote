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

/// The loading skeleton's breathe: one shade step up and back down, every
/// unserved cell on the shared spin clock so the column pulses together.
/// `None` (reduced motion, or the clock never started) holds the lightest.
const BREATHE: [char; 4] = ['░', '▒', '▓', '▒'];

pub(super) fn skeleton_cell(width: usize, elapsed_ms: Option<u64>) -> String {
    let frame = elapsed_ms.map_or(0, |ms| {
        (ms / crate::lattice::SPIN_FRAME_MS) as usize % BREATHE.len()
    });
    BREATHE[frame].to_string().repeat(width)
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

/// A priced dollar amount. The `~` marks LIST price: cents price the base
/// catalog rates, so a tiered model's doubled top band and a subscription's
/// actual bill both read lower.
pub(super) fn cost_cell(cents: u64) -> String {
    if cents >= 100_000 {
        format!("~${:.1}k", cents as f64 / 100_000.0)
    } else if cents % 100 == 0 {
        format!("~${}", cents / 100)
    } else {
        format!("~${}.{:02}", cents / 100, cents % 100)
    }
}

/// A compact token count: `1.4B`, `367M`, `12.3k`, never a grouped digit run.
pub(super) fn token_cell(tokens: u64) -> String {
    let (div, suffix) = if tokens >= 1_000_000_000 {
        (1_000_000_000.0, "B")
    } else if tokens >= 1_000_000 {
        (1_000_000.0, "M")
    } else if tokens >= 1_000 {
        (1_000.0, "k")
    } else {
        return format!("{tokens} tok");
    };
    let v = tokens as f64 / div;
    if (v * 10.0).round() % 10.0 == 0.0 {
        format!("{v:.0}{suffix} tok")
    } else {
        format!("{v:.1}{suffix} tok")
    }
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
    use super::{cost_cell, name_w, skeleton_cell, token_cell};
    #[test]
    fn cost_is_dollars_and_tokens_compact() {
        assert_eq!(cost_cell(42), "~$0.42");
        assert_eq!(cost_cell(4497), "~$44.97");
        assert_eq!(cost_cell(13_400), "~$134");
        assert_eq!(cost_cell(120_000), "~$1.2k");
        assert_eq!(cost_cell(0), "~$0");
        assert_eq!(token_cell(12_345), "12.3k tok");
        assert_eq!(token_cell(999), "999 tok");
        assert_eq!(token_cell(367_000_000), "367M tok");
        assert_eq!(token_cell(1_416_159_173), "1.4B tok");
        assert_eq!(token_cell(1_000_000_000), "1B tok");
        assert_eq!(skeleton_cell(3, None), "░░░");
        assert_eq!(skeleton_cell(2, Some(0)), "░░");
        assert_eq!(skeleton_cell(2, Some(250)), "▒▒");
        assert_eq!(skeleton_cell(2, Some(1000)), "░░");
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
