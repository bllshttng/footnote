//! Mux chrome themes (x-f75e): a named palette the chrome reads. `terminal` is
//! the default and inherits the emulator's own colors, so every existing render
//! path stays byte-identical (Default + the INVERSE/BOLD/DIM flags do the work,
//! no color introduced). Named themes give the chrome (border, title, esc chip,
//! footer, the active tab, the selected row, the accent) explicit colors while
//! the body content stays an inverse block in the terminal's own colors.
//!
//! The body is not recolored per-theme on purpose: that is a terminal-emulator
//! job, not a multiplexer's, and keeping it inverse is what makes `terminal` a
//! true no-op and keeps modal content readable against every palette.

use crate::keys::KeymapWarning;
use crate::proto::{cell_flags, Color};

/// One mux chrome theme. `terminal` is the only theme with `inherit: true`: it
/// renders Default + flags so nothing clashes with the user's emulator and every
/// pre-theme render stays byte-identical.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// `true` only for `terminal`: render Default + INVERSE/BOLD/DIM, ignoring
    /// the palette fields (except `accent`, which stays `Indexed(3)` so the
    /// needs-attention glyph does not regress). The switch that makes
    /// byte-identity a single branch in [`cell_style`].
    pub inherit: bool,
    pub border: Color,
    pub title: Color,
    /// Absorbs the old hardcoded `LATTICE_ACCENT` (`Indexed(3)`): the one color
    /// reserved for the needs-attention state and the active tab dot. `Indexed(3)`
    /// under `terminal` because index 3 follows the emulator's own palette, so it
    /// is the one color that cannot clash.
    pub accent: Color,
    pub sel: Color,
    pub dim: Color,
    pub chip: Color,
}

/// How a framed cell is colored, resolved against a [`Theme`] by [`cell_style`].
/// Body roles cover modal content; the rest are chrome the frame adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Border,
    Title,
    /// The `esc close` affordance.
    Chip,
    Subtitle,
    /// `(is_active,)` - a section tab; the active one carries the accent.
    Tab(bool),
    Footer,
    /// A body content cell (the inverse block).
    Body,
    /// The selected row's cell - a cut-out under `terminal`, a `sel` highlight
    /// under a named theme.
    BodySel,
    /// A `PopupRow::Header` cell inside the body.
    BodyHead,
    ScrollTrack,
    ScrollThumb,
}

impl Theme {
    /// Resolve a theme by name. An unknown or empty name falls back to
    /// `terminal` and returns a notice through the same channel a refused keymap
    /// rebind uses: a config that is quietly ignored is indistinguishable from
    /// one that was never written (`client.rs` keymap notices make the same
    /// argument). Never silent.
    pub fn from_name(name: &str) -> (Theme, Option<KeymapWarning>) {
        let t = match name.trim() {
            "" | "terminal" => Some(theme_terminal()),
            "catppuccin" => Some(theme_catppuccin()),
            "tokyo-night" => Some(theme_tokyo_night()),
            "gruvbox" => Some(theme_gruvbox()),
            _ => None,
        };
        match t {
            Some(t) => (t, None),
            None => (
                theme_terminal(),
                Some(KeymapWarning(format!(
                    "unknown mux theme {name:?}, using terminal"
                ))),
            ),
        }
    }

    /// The default theme (`terminal`), the one every render assumes when no
    /// config names one.
    pub fn default_theme() -> Theme {
        theme_terminal()
    }
}

/// `(fg, bg, flags)` for a cell of `role` under `t`. The single place a role
/// becomes concrete style, so a new chrome element adds a variant here and is
/// colored consistently by both overlay families.
pub fn cell_style(role: Role, t: &Theme) -> (Color, Color, u8) {
    if t.inherit {
        // terminal: Default everywhere, the flags carry every distinction. This
        // branch is what keeps pre-theme renders byte-identical.
        return match role {
            Role::BodySel => (Color::Default, Color::Default, 0),
            Role::BodyHead | Role::Title | Role::Chip | Role::Tab(true) | Role::ScrollThumb => {
                (Color::Default, Color::Default, cell_flags::INVERSE | cell_flags::BOLD)
            }
            Role::Subtitle | Role::Tab(false) | Role::Footer | Role::ScrollTrack => {
                (Color::Default, Color::Default, cell_flags::INVERSE | cell_flags::DIM)
            }
            // Body, Border: plain inverse.
            _ => (Color::Default, Color::Default, cell_flags::INVERSE),
        };
    }
    // Named theme: chrome takes explicit colors on the default bg; the body
    // stays the inverse block so content reads in the emulator's own colors.
    match role {
        Role::Body => (Color::Default, Color::Default, cell_flags::INVERSE),
        Role::BodySel => (Color::Default, t.sel, 0),
        Role::BodyHead => (Color::Default, Color::Default, cell_flags::INVERSE | cell_flags::BOLD),
        Role::Border => (t.border, Color::Default, 0),
        Role::Title => (t.title, Color::Default, cell_flags::BOLD),
        Role::Chip => (t.chip, Color::Default, cell_flags::BOLD),
        Role::Subtitle => (t.dim, Color::Default, 0),
        Role::Tab(true) => (t.accent, Color::Default, cell_flags::BOLD),
        Role::Tab(false) => (t.dim, Color::Default, 0),
        Role::Footer => (t.dim, Color::Default, 0),
        Role::ScrollTrack => (t.dim, Color::Default, cell_flags::DIM),
        Role::ScrollThumb => (t.border, Color::Default, cell_flags::BOLD),
    }
}

fn theme_terminal() -> Theme {
    Theme {
        name: "terminal",
        inherit: true,
        border: Color::Default,
        title: Color::Default,
        // Index 3 follows the emulator's palette (amber/yellow in every scheme),
        // preserving the pre-theme needs-attention glyph exactly.
        accent: Color::Indexed(3),
        sel: Color::Default,
        dim: Color::Default,
        chip: Color::Default,
    }
}

fn theme_catppuccin() -> Theme {
    Theme {
        name: "catppuccin",
        inherit: false,
        border: rgb(0x6c, 0x70, 0x86), // overlay0
        title: rgb(0x89, 0xb4, 0xfa),  // blue
        accent: rgb(0xfa, 0xb3, 0x87), // peach
        sel: rgb(0x31, 0x32, 0x44),    // surface0
        dim: rgb(0xa6, 0xad, 0xc8),    // subtext0
        chip: rgb(0xf3, 0x8b, 0xa8),   // red
    }
}

fn theme_tokyo_night() -> Theme {
    Theme {
        name: "tokyo-night",
        inherit: false,
        border: rgb(0x56, 0x5f, 0x89), // comment
        title: rgb(0x7a, 0xa2, 0xf7),  // blue
        accent: rgb(0xff, 0x9e, 0x64), // orange
        sel: rgb(0x33, 0x3a, 0x54),    // bg_dark-ish selection
        dim: rgb(0x96, 0x9d, 0xc4),    // fg_gutter
        chip: rgb(0xf7, 0x76, 0x8e),   // red
    }
}

fn theme_gruvbox() -> Theme {
    Theme {
        name: "gruvbox",
        inherit: false,
        border: rgb(0x92, 0x83, 0x74), // gray
        title: rgb(0x83, 0xa5, 0x98),  // blue
        accent: rgb(0xfe, 0x80, 0x19), // orange
        sel: rgb(0x3c, 0x38, 0x36),    // bg1
        dim: rgb(0xa8, 0x99, 0x84),    // fg4
        chip: rgb(0xfb, 0x49, 0x34),   // red
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// The four shipped theme names, in display order. Adding a palette later is a
/// new `theme_*` fn, a match arm in [`Theme::from_name`], and a name here.
pub const THEME_NAMES: [&str; 4] = ["terminal", "catppuccin", "tokyo-night", "gruvbox"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_theme_falls_back_with_a_notice() {
        let (t, warn) = Theme::from_name("solarized-light");
        assert_eq!(t.name, "terminal");
        let w = warn.expect("unknown theme must warn");
        assert!(w.0.contains("solarized-light"), "{}, got {w:?}", w.0);
        assert!(w.0.contains("terminal"));
    }

    #[test]
    fn empty_name_is_terminal_silently() {
        // An unset config key reads as "" and means "no preference", not a typo.
        let (t, warn) = Theme::from_name("");
        assert_eq!(t.name, "terminal");
        assert!(warn.is_none(), "no preference is not a warning");
    }

    #[test]
    fn each_shipped_name_resolves() {
        for n in THEME_NAMES {
            let (t, warn) = Theme::from_name(n);
            assert_eq!(t.name, n, "{n} should resolve to itself");
            assert!(warn.is_none(), "{n} should not warn");
        }
    }

    #[test]
    fn terminal_is_a_true_no_op_on_color() {
        // The load-bearing property: under terminal every role resolves to
        // Default fg/bg, so the flag set alone carries every visual distinction
        // and a pre-theme render is byte-identical.
        let t = theme_terminal();
        for role in [
            Role::Border,
            Role::Title,
            Role::Chip,
            Role::Subtitle,
            Role::Tab(true),
            Role::Tab(false),
            Role::Footer,
            Role::Body,
            Role::BodySel,
            Role::BodyHead,
            Role::ScrollTrack,
            Role::ScrollThumb,
        ] {
            let (fg, bg, _) = cell_style(role, &t);
            assert_eq!(fg, Color::Default, "{role:?} fg must be Default under terminal");
            assert_eq!(bg, Color::Default, "{role:?} bg must be Default under terminal");
        }
    }

    #[test]
    fn terminal_accent_preserves_the_pre_theme_glyph() {
        // The one exception to "terminal is all Default": the needs-attention
        // accent stays Indexed(3) so the warning glyph does not silently change.
        assert_eq!(theme_terminal().accent, Color::Indexed(3));
    }

    #[test]
    fn named_themes_color_the_chrome() {
        // A named theme must actually differ from terminal on the chrome roles,
        // otherwise the picker offers no choice.
        let term = theme_terminal();
        for t in [theme_catppuccin(), theme_tokyo_night(), theme_gruvbox()] {
            assert!(!t.inherit);
            assert_ne!(
                cell_style(Role::Border, &t).0,
                cell_style(Role::Border, &term).0,
                "{} border should differ from terminal",
                t.name
            );
            assert_ne!(
                cell_style(Role::Title, &t).0,
                cell_style(Role::Title, &term).0,
                "{} title should differ from terminal",
                t.name
            );
        }
    }

    #[test]
    fn selected_row_is_a_cut_out_under_terminal_and_a_highlight_under_named() {
        // terminal: normal video (flags 0) = the existing cut-out.
        let (_, _, flags) = cell_style(Role::BodySel, &theme_terminal());
        assert_eq!(flags, 0);
        // named: a sel-colored background.
        let (_, bg, _) = cell_style(Role::BodySel, &theme_catppuccin());
        assert_eq!(bg, theme_catppuccin().sel);
    }
}
