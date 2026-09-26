//! Mux chrome themes : a named palette the chrome reads. `terminal` is
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
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
    #[default]
    Body,
    /// The selected row's cell - a cut-out under `terminal`, a `sel` highlight
    /// under a named theme.
    BodySel,
    /// A `PopupRow::Header` cell inside the body.
    BodyHead,
    /// A disabled (greyed) body entry: present but inert to arrow, Enter, and
    /// click. DIM under `terminal`, the theme's `dim` color under a named theme.
    BodyDim,
    ScrollTrack,
    ScrollThumb,
    /// A backlog panel's body cell: plain text on the terminal's own bg. The
    /// old body was an INVERSE block, which read as one pale fill under a
    /// named theme (the emulator's fg swapped in as bg), so the backlog
    /// views dropped it: the panel carries no standing bg at all, under any
    /// theme.
    PanelBody,
    /// A backlog panel's heading (lane name, column header, section label,
    /// node title): the theme's bold fg slot - the terminal's own fg with
    /// BOLD carrying the rank - on the terminal's own bg.
    PanelHead,
    /// A backlog panel's handle (node id) or field label: the emulator's
    /// index-3 accent slot, so no hard color enters under any theme.
    PanelLabel,
    /// A backlog panel's meta line: the emulator's index-8 dim slot. Index
    /// 8, not the DIM flag: DIM washes the default fg out on a light
    /// terminal, while index 8 stays a readable gray under both.
    PanelMeta,
    /// A pill: the bracketed `status · priority` token on a detail title
    /// line. The emulator's index-3 accent slot, on the terminal's own bg.
    PanelPill,
    /// A thin rule under a heading or lane name: the index-8 dim slot.
    PanelRule,
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
    // The backlog panel's slots sit above the theme split and stay
    // palette-following under EVERY theme: attributes plus emulator-palette
    // indexes, never a fixed color. A named theme's `title` is a fixed Rgb -
    // pale blue on a light terminal is the exact wash-out the Solarized Light
    // screenshots showed - while an index resolves through whatever palette
    // the emulator runs, so the same render reads on dark and light both.
    match role {
        Role::PanelBody => return (Color::Default, Color::Default, 0),
        // The heading rides the theme's bold fg slot: the terminal's own fg
        // with BOLD carrying the rank - never a fixed color that can wash
        // out on a disagreeing palette.
        Role::PanelHead => return (Color::Default, Color::Default, cell_flags::BOLD),
        Role::PanelLabel | Role::PanelPill => return (Color::Indexed(3), Color::Default, 0),
        // Index 8, not the DIM flag: the merged band evidence showed DIM
        // washing the default fg out on a light terminal, while index 8 stays
        // a readable gray under both.
        Role::PanelMeta | Role::PanelRule => return (Color::Indexed(8), Color::Default, 0),
        _ => {}
    }
    if t.inherit {
        // terminal: Default everywhere, the flags carry every distinction. This
        // branch is what keeps pre-theme renders byte-identical.
        return match role {
            Role::BodySel => (Color::Default, Color::Default, 0),
            Role::BodyHead | Role::Title | Role::Chip | Role::Tab(true) | Role::ScrollThumb => (
                Color::Default,
                Color::Default,
                cell_flags::INVERSE | cell_flags::BOLD,
            ),
            Role::BodyDim
            | Role::Subtitle
            | Role::Tab(false)
            | Role::Footer
            | Role::ScrollTrack => (
                Color::Default,
                Color::Default,
                cell_flags::INVERSE | cell_flags::DIM,
            ),
            // The backlog panel's slots resolved above the theme split.
            // Body, Border: plain inverse.
            _ => (Color::Default, Color::Default, cell_flags::INVERSE),
        };
    }
    // Named theme: chrome takes explicit colors on the default bg; the body
    // stays the inverse block so content reads in the emulator's own colors.
    match role {
        Role::Body => (Color::Default, Color::Default, cell_flags::INVERSE),
        // The selected row's fg is the theme's `title` (a light color in every
        // shipped palette), not `Color::Default`: every shipped `sel` is a dark
        // background, so a `Default` fg would read dark-on-dark on a light
        // terminal. An explicit light fg stays readable regardless of the
        // emulator's default pair, the property INVERSE gives the Body row.
        Role::BodySel => (t.title, t.sel, cell_flags::BOLD),
        Role::BodyHead => (
            Color::Default,
            Color::Default,
            cell_flags::INVERSE | cell_flags::BOLD,
        ),
        // A disabled body entry: the theme's dim color, still on the inverse
        // body block (matching Body/BodySel/BodyHead) so the row stays part of
        // the block instead of punching a plain-background hole in it.
        Role::BodyDim => (t.dim, Color::Default, cell_flags::INVERSE | cell_flags::DIM),
        Role::Border => (t.border, Color::Default, 0),
        Role::Title => (t.title, Color::Default, cell_flags::BOLD),
        Role::Chip => (t.chip, Color::Default, cell_flags::BOLD),
        Role::Subtitle => (t.dim, Color::Default, 0),
        Role::Tab(true) => (t.accent, Color::Default, cell_flags::BOLD),
        Role::Tab(false) => (t.dim, Color::Default, 0),
        Role::Footer => (t.dim, Color::Default, 0),
        Role::ScrollTrack => (t.dim, Color::Default, cell_flags::DIM),
        Role::ScrollThumb => (t.border, Color::Default, cell_flags::BOLD),
        // The panel slots resolved above the theme split; unreachable keeps
        // a future role from silently inheriting a body style.
        _ => unreachable!("panel roles resolve above the theme split"),
    }
}

/// The text color on a highlight band: the dark anchor. Band backgrounds are
/// light in every palette this paints - accents read as highlights on a dark
/// terminal and index 7 is the scheme's light gray - so dark text is the
/// readable pick. The contrast tests hold that floor per theme.
pub const BAND_TEXT: Color = Color::Rgb(0, 0, 0);

/// `(fg, bg, flags)` for a sideline highlight band. `chosen` is the focused
/// agent's accent band: dark text on the accent surface. Selection and hover
/// share the cursor band - a subtle surface under accent text - and the
/// chosen color wins where they collide. Both legs are explicit colors that
/// answer each other's contrast, so the band reads identically on a dark and
/// a light terminal: INVERSE would make the terminal's own background the
/// text color and DIM washes the text toward the band. Neither belongs in a
/// band.
pub fn band_style(chosen: bool, t: &Theme) -> (Color, Color, u8) {
    if chosen {
        return (BAND_TEXT, t.accent, 0);
    }
    if t.inherit {
        // The palette's own surface pair: accent text on the deep index, so
        // both legs follow the emulator's scheme instead of painting a pale
        // gray bar over it.
        (Color::Indexed(3), Color::Indexed(0), 0)
    } else {
        // A named theme pairs its accent with its `sel` surface.
        (t.accent, t.sel, 0)
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
            Role::BodyDim,
            Role::ScrollTrack,
            Role::ScrollThumb,
        ] {
            let (fg, bg, _) = cell_style(role, &t);
            assert_eq!(
                fg,
                Color::Default,
                "{role:?} fg must be Default under terminal"
            );
            assert_eq!(
                bg,
                Color::Default,
                "{role:?} bg must be Default under terminal"
            );
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
        // named: a sel-colored background and an explicit (non-Default) fg, so a
        // dark sel bg stays readable on a light terminal as well as a dark one.
        let (fg, bg, _) = cell_style(Role::BodySel, &theme_catppuccin());
        assert_eq!(bg, theme_catppuccin().sel);
        assert_ne!(fg, Color::Default, "named-theme sel fg must be explicit");
        assert_eq!(fg, theme_catppuccin().title);
    }

    #[test]
    fn panel_roles_never_carry_inverse_or_a_bg_under_any_theme() {
        // The pale-panel fix: the backlog panel paints on the terminal's own
        // bg. INVERSE would swap the emulator's fg in as the bg - the exact
        // pale fill the user screenshotted - so no panel role may carry it,
        // under the inherit theme or a named one, and no panel role may
        // carry a bg.
        for t in [
            theme_terminal(),
            theme_catppuccin(),
            theme_tokyo_night(),
            theme_gruvbox(),
        ] {
            for role in [
                Role::PanelBody,
                Role::PanelHead,
                Role::PanelLabel,
                Role::PanelMeta,
                Role::PanelPill,
                Role::PanelRule,
            ] {
                let (fg, bg, flags) = cell_style(role, &t);
                assert_eq!(
                    bg,
                    Color::Default,
                    "{role:?} bg must stay the terminal's under {}",
                    t.name
                );
                assert!(
                    flags & cell_flags::INVERSE == 0,
                    "{role:?} must not carry INVERSE under {}",
                    t.name
                );
                assert!(
                    flags & cell_flags::DIM == 0 || role == Role::PanelMeta,
                    "unexpected DIM on {role:?} under {}",
                    t.name
                );
                let _ = fg;
            }
        }
    }

    #[test]
    fn panel_hierarchy_roles_are_distinct_under_both_theme_kinds() {
        // The whole point of the hierarchy: head, label, meta and body must
        // resolve to DIFFERENT styles, or every line reads at one weight
        // again (the defect the user reported).
        for t in [theme_terminal(), theme_catppuccin()] {
            let head = cell_style(Role::PanelHead, &t);
            let label = cell_style(Role::PanelLabel, &t);
            let meta = cell_style(Role::PanelMeta, &t);
            let body = cell_style(Role::PanelBody, &t);
            assert_ne!(head, body, "head vs body under {}", t.name);
            assert_ne!(label, body, "label vs body under {}", t.name);
            assert_ne!(meta, body, "meta vs body under {}", t.name);
            assert_ne!(head, label, "head vs label under {}", t.name);
            assert_ne!(head, meta, "head vs meta under {}", t.name);
            assert_ne!(label, meta, "label/pill vs meta under {}", t.name);
        }
    }

    #[test]
    fn panel_slots_stay_palette_following_under_named_themes() {
        // D1: attributes and palette indexes only. A fixed Rgb in a panel
        // slot washes out the moment the emulator's palette disagrees with
        // the theme's - the pale-blue lane names on Solarized Light.
        for t in [theme_catppuccin(), theme_tokyo_night(), theme_gruvbox()] {
            for role in [
                Role::PanelBody,
                Role::PanelHead,
                Role::PanelLabel,
                Role::PanelMeta,
                Role::PanelPill,
                Role::PanelRule,
            ] {
                let (fg, bg, flags) = cell_style(role, &t);
                assert!(
                    !matches!(fg, Color::Rgb(..)),
                    "{role:?} fg must stay palette-following under {}",
                    t.name
                );
                assert_eq!(bg, Color::Default, "{role:?} bg under {}", t.name);
                assert_eq!(
                    flags & cell_flags::INVERSE,
                    0,
                    "{role:?} INVERSE under {}",
                    t.name
                );
            }
        }
    }
}
