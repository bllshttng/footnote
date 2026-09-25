//! The `f[no]` brand mark, carried over from footnote.sh: a serif-feeling `f`
//! with `[no]` raised beside it in dimmed amber. A terminal cannot superscript
//! and superscript o has no reliable glyph, so the raised look is a TWO-ROW
//! variant (`[no]` on the upper row, right of the `f`) offered where the
//! surface has a row to spare; everywhere else wears the one-row form.

use crate::theme::Role;

/// One row of `(text, role)`: the `f` in the bold title role, `[no]` in the
/// dim amber wordmark role.
pub fn one_row() -> Vec<(&'static str, Role)> {
    vec![("f", Role::Title), ("[no]", Role::Wordmark)]
}

/// The two-row variant as a per-char grid: row 0 is ` [no]` (one cell right
/// of the f's column), row 1 is `f`.
pub fn two_row() -> Vec<Vec<(char, Role)>> {
    let mut top: Vec<(char, Role)> = vec![(' ', Role::Title)];
    for ch in "[no]".chars() {
        top.push((ch, Role::Wordmark));
    }
    vec![top, vec![('f', Role::Title)]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn one_row_is_the_f_then_the_bracketed_no() {
        let row = one_row();
        assert_eq!(row[0], ("f", Role::Title));
        assert_eq!(row[1], ("[no]", Role::Wordmark));
    }

    #[test]
    fn two_row_raises_the_no_above_and_right_of_the_f() {
        let rows = two_row();
        let top: String = rows[0].iter().map(|(c, _)| c).collect();
        let bottom: String = rows[1].iter().map(|(c, _)| c).collect();
        assert_eq!(top, " [no]");
        assert_eq!(bottom, "f");
        assert_eq!(rows[0][1].1, Role::Wordmark);
    }

    #[test]
    fn the_wordmark_role_resolves_dim_amber_under_every_theme() {
        for name in ["terminal", "catppuccin", "tokyo-night", "gruvbox"] {
            let t = Theme::from_name(name).0;
            let (fg, _bg, flags) = crate::theme::cell_style(Role::Wordmark, &t);
            assert_eq!(fg, t.accent, "{name}");
            assert_eq!(
                flags & crate::proto::cell_flags::DIM,
                crate::proto::cell_flags::DIM,
                "{name}"
            );
        }
    }
}
