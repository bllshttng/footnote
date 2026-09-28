//! The `Ｆ[no]` brand mark, carried over from footnote.sh: the full-width
//! `Ｆ` (U+FF26, two cells) with the `[no]` stamp directly after it, no gap.
//! The stamp is reverse video: an off-white label under a dark theme, an ink
//! label under a light one. One row only (user ruling, 2026-09-25): a
//! terminal cannot superscript and a two-row layout read odd.

use crate::theme::Role;

/// The mark as one string: the full-width `Ｆ` then `[no]`. The tab bar's
/// width math reads this through [`glyph widths`](crate::client glyph_cols),
/// so the `Ｆ` claims its two columns.
pub const TEXT: &str = "\u{FF26}[no]";

/// One row of `(text, role)`: the `Ｆ` in the bold title role, `[no]` in the
/// reverse-video stamp role.
pub fn one_row() -> Vec<(&'static str, Role)> {
    vec![("\u{FF26}", Role::Title), ("[no]", Role::Stamp)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn one_row_is_the_fullwidth_f_then_the_bracketed_no() {
        let row = one_row();
        assert_eq!(row[0], ("\u{FF26}", Role::Title));
        assert_eq!(row[1], ("[no]", Role::Stamp));
        let joined: String = row.iter().map(|(s, _)| *s).collect();
        assert_eq!(joined, TEXT);
    }

    #[test]
    fn the_stamp_role_is_inverse_under_every_theme() {
        for name in crate::theme::THEME_NAMES {
            let t = Theme::from_name(name).0;
            let (fg, _bg, flags) = crate::theme::cell_style(Role::Stamp, &t);
            assert_eq!(fg, t.stamp, "{name}");
            assert_eq!(
                flags & crate::proto::cell_flags::INVERSE,
                crate::proto::cell_flags::INVERSE,
                "{name}"
            );
        }
    }
}
