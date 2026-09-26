//! The `f[no]` brand mark, carried over from footnote.sh: a serif-feeling `f`
//! with `[no]` beside it in dimmed amber. One row only (user ruling,
//! 2026-09-25): a terminal cannot superscript and a two-row layout read odd.

use crate::theme::Role;

/// One row of `(text, role)`: the `f` in the bold title role, `[no]` in the
/// dim amber wordmark role.
pub fn one_row() -> Vec<(&'static str, Role)> {
    vec![("f", Role::Title), ("[no]", Role::Wordmark)]
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
