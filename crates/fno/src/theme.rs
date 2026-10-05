//! Mux chrome themes : a named palette the chrome reads. `terminal` is
//! the no-op that inherits the emulator's own colors, so every existing render
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
    /// the palette fields (except `brand`/`needs_you`, which stay `Indexed(3)`
    /// so the needs-attention glyph does not regress). The switch that makes
    /// byte-identity a single branch in [`cell_style`].
    pub inherit: bool,
    /// The border color: every border paints it - the focused pane frame
    /// (via `pane_paint`'s outline arm) and every modal/popover border. A
    /// named theme defaults it to its own `brand` (the ruling: borders wear
    /// the brand), and `mux.theme.border` overrides it everywhere at once.
    /// `terminal` keeps its Indexed(3) brand; under `inherit` the field is
    /// read only for the border role, so the rest of the chrome render stays
    /// byte-identical while an override still reaches every border.
    pub border: Color,
    pub title: Color,
    /// The brand accent: selection, the active tab, the focused frame's name
    /// label, the `[no]` stamp's surroundings. `Indexed(3)` under `terminal` because index
    /// 3 follows the emulator's own palette, so it is the one color that
    /// cannot clash.
    pub brand: Color,
    /// The needs-you accent: a question or block waiting on the user (the
    /// lattice's Blocked `▲`). Deliberately NOT the brand color: attention and
    /// selection are different states, and a theme that paints them alike
    /// makes a waiting worker look like a chosen one. Every theme pairs its
    /// brand with a needs-you from its own palette.
    pub needs_you: Color,
    /// The activity ramp's three-step failure color: a cell's failed share
    /// picks ok (no failures), warn (under half), or error (half or more).
    /// `Indexed` under `terminal` (the emulator's palette owns contrast
    /// there); every named theme picks palette-family values that clear
    /// 4.5:1 against its own base - pinned by the contrast test below.
    pub ok: Color,
    pub warn: Color,
    pub error: Color,
    pub sel: Color,
    pub dim: Color,
    pub chip: Color,
    /// The `[no]` stamp's label color, painted with INVERSE so the label is
    /// the background and the terminal's own fg carries the letters. Off-white
    /// on a dark theme, ink on a light one.
    pub stamp: Color,
    /// The theme's own ground: what OSC 11 sets and what every drawn cell
    /// fills with so a terminal that ignores the OSC still shows the base.
    /// `Default` under `terminal` - the emulator's own scheme IS the ground
    /// there, and fno paints nothing over it.
    pub base: Color,
    /// The built-in a user theme inherited from, naming its 16-slot OSC
    /// palette table ([`terminal16_slot`]). Empty for the built-ins. A user
    /// theme keeps its parent's palette slots unless the parent has none.
    pub inherit_from: &'static str,
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
    /// The selected row's cell - a cut-out under `terminal`, a `sel` highlight
    /// under a named theme.
    BodySel,
    /// A plain-body popup's emphasis text (the key column, a section
    /// heading): the theme's brand accent, bold, on the plain ground - never
    /// a band. `brand` survives the `terminal` inherit branch (the field
    /// doc), so the emphasis reads under every theme.
    BodyAccent,
    /// The cursor row of a plain-body popup: a filled band on the plain
    /// ground. Under `terminal` INVERSE is the band; under a named theme
    /// the `sel` surface, as [`Role::BodySel`].
    BodyCursor,
    /// A disabled (greyed) body entry: present but inert to arrow, Enter, and
    /// click. DIM under `terminal`, the theme's `dim` color under a named theme.
    BodyDim,
    ScrollTrack,
    /// The `Ｆ[no]` brand mark's `[no]`: the reverse-video stamp. Off-white
    /// label on a dark theme, ink label on a light one.
    Stamp,
    ScrollThumb,
    /// A body cell on the theme ground: plain text on the terminal's own bg,
    /// under EVERY theme. The old body was an INVERSE block, which read as
    /// one pale fill under a named theme (the emulator's fg swapped in as
    /// bg), so no view carries it any more.
    #[default]
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
    /// A literal-color span (the colors tab's swatches). The color IS the
    /// content, so it resolves above the theme split and never shifts with
    /// the theme.
    Swatch(Color),
}

impl Theme {
    /// Resolve a theme by name. An unknown or empty name falls back to the
    /// default (`footnote-superscript`) and returns a notice through the same
    /// channel a refused keymap rebind uses: a config that is quietly ignored
    /// is indistinguishable from one that was never written (`client.rs`
    /// keymap notices make the same argument). Never silent.
    pub fn from_name(name: &str) -> (Theme, Option<KeymapWarning>) {
        let t = match name.trim() {
            "" | "footnote-superscript" => Some(theme_footnote_superscript()),
            "footnote-paper" => Some(theme_footnote_paper()),
            "terminal" => Some(theme_terminal()),
            "catppuccin" => Some(theme_catppuccin()),
            "tokyo-night" => Some(theme_tokyo_night()),
            "gruvbox" => Some(theme_gruvbox()),
            _ => None,
        };
        match t {
            Some(t) => (t, None),
            None => (
                theme_footnote_superscript(),
                Some(KeymapWarning(format!(
                    "unknown mux theme {name:?}, using footnote-superscript"
                ))),
            ),
        }
    }

    /// Resolve a theme by name across the built-ins AND a user theme table
    /// (config `[mux.themes.<name>]`, materialized by
    /// [`crate::digest_overlay::user_themes`]). Built-ins resolve first, so a
    /// user theme cannot shadow a shipped name. An unknown name still falls
    /// back to the default carrying the same warning [`Theme::from_name`]
    /// returns.
    pub fn from_name_in(name: &str, user: &[(String, Theme)]) -> (Theme, Option<KeymapWarning>) {
        let trimmed = name.trim();
        let (t, warn) = Self::from_name(trimmed);
        if warn.is_none() {
            return (t, None);
        }
        match user.iter().find(|(n, _)| n.as_str() == trimmed) {
            Some((_, t)) => (*t, None),
            None => (t, warn),
        }
    }

    /// The default theme (`footnote-superscript`), the one every render
    /// assumes when no config names one and the terminal gives no signal
    /// otherwise.
    pub fn default_theme() -> Theme {
        theme_footnote_superscript()
    }

    /// The default for a terminal that reports its ground: a light background
    /// picks the paper twin, everything else (dark, unknown, unset) the
    /// superscript default.
    pub fn default_for(light_background: bool) -> Theme {
        if light_background {
            theme_footnote_paper()
        } else {
            theme_footnote_superscript()
        }
    }
}

/// `(fg, bg, flags)` for a cell of `role` under `t`. The single place a role
/// becomes concrete style, so a new chrome element adds a variant here and is
/// colored consistently by both overlay families.
pub fn cell_style(role: Role, t: &Theme) -> (Color, Color, u8) {
    // A literal swatch is content, not chrome: it resolves above the theme
    // split so the painted color never shifts with the theme.
    if let Role::Swatch(c) = role {
        return (c, Color::Default, 0);
    }
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
            // A plain-body popup's cursor row: the one filled band on the
            // plain ground (the inverse block's old job, now per-row).
            Role::BodyCursor => (Color::Default, Color::Default, cell_flags::INVERSE),
            Role::Title | Role::Chip | Role::Tab(true) | Role::ScrollThumb => (
                Color::Default,
                Color::Default,
                cell_flags::INVERSE | cell_flags::BOLD,
            ),
            // Tab(false) left the inverse arms: a whole strip of filled
            // chips read as one selected row. Only the ACTIVE tab is
            // filled; an inactive one is plain dim text.
            Role::Tab(false) => (Color::Default, Color::Default, cell_flags::DIM),
            Role::BodyDim | Role::Subtitle | Role::Footer | Role::ScrollTrack => (
                Color::Default,
                Color::Default,
                cell_flags::INVERSE | cell_flags::DIM,
            ),
            // Amber under `terminal` too: `brand` survives the inherit branch
            // (see the field doc), so the key column and section headings
            // carry the accent under every theme.
            Role::BodyAccent => (t.brand, Color::Default, cell_flags::BOLD),
            // The mark's stamp is plain reverse video under `terminal`: the
            // emulator's own pair is the stamp.
            Role::Stamp => (Color::Default, Color::Default, cell_flags::INVERSE),
            // The backlog panel's slots resolved above the theme split.
            // Border and the chrome slots: plain inverse.
            // The border is the exception: it reads `t.border` under inherit
            // too, because the field IS the terminal theme's own amber
            // (Indexed(3)), and an explicit mux.theme.border override has to
            // recolor every border under every theme, this one included.
            Role::Border => (t.border, Color::Default, 0),
            _ => (Color::Default, Color::Default, cell_flags::INVERSE),
        };
    }
    // Named theme: chrome takes explicit colors on the default bg; the body
    // rides the theme ground (PanelBody resolved above the split), so
    // content reads in the emulator's own colors under every theme.
    match role {
        // The selected row's fg is the theme's `title`, not `Color::Default`:
        // every shipped `sel` is a surface the theme picked to sit under its
        // own title color, so an explicit fg stays readable regardless of the
        // emulator's default pair (a dark sel bg on a light terminal would
        // read dark-on-dark with a Default fg).
        Role::BodySel => (t.title, t.sel, cell_flags::BOLD),
        // A plain-body popup's cursor band: the `sel` surface, as BodySel.
        Role::BodyCursor => (t.title, t.sel, cell_flags::BOLD),
        Role::BodyAccent => (t.brand, Color::Default, cell_flags::BOLD),
        // background. The inverse block it used to sit on turned the light
        // `dim` into the BACKGROUND, so the row rendered light-on-light and
        // near invisible under every named theme (the screenshot review).
        Role::BodyDim => (t.dim, Color::Default, 0),
        Role::Border => (t.border, Color::Default, 0),
        Role::Title => (t.title, Color::Default, cell_flags::BOLD),
        Role::Chip => (t.chip, Color::Default, cell_flags::BOLD),
        Role::Subtitle => (t.dim, Color::Default, 0),
        Role::Tab(true) => (t.brand, Color::Default, cell_flags::BOLD),
        // Inactive tabs read in the theme's full text color, not dim: a dim
        // label on the theme ground blends into it (the user's screenshot),
        // and an unpicked section is still a choice worth reading.
        Role::Tab(false) => (t.title, Color::Default, 0),
        Role::Footer => (t.dim, Color::Default, 0),
        Role::ScrollTrack => (t.dim, Color::Default, cell_flags::DIM),
        Role::ScrollThumb => (t.border, Color::Default, cell_flags::BOLD),
        // The stamp: INVERSE with the theme's label color as the fg - the
        // swap makes the label the background and the terminal's own fg the
        // letters, which is what a reverse-video stamp is.
        Role::Stamp => (t.stamp, Color::Default, cell_flags::INVERSE),
        // The panel slots resolved above the theme split; unreachable keeps
        // a future role from silently inheriting a body style.
        _ => unreachable!("panel roles resolve above the theme split"),
    }
}

/// `(fg, bg, flags)` for a sideline highlight band: accent text on the
/// surface band. Selection, hover, and the focused row share one pair - the
/// focused row's distinction rides its glyph marks, never a louder fill
/// (the operator's color ruling: selection is the surface0 band, never a
/// full brand fill). Both legs are explicit colors that answer each other's
/// contrast, so the band reads identically on a dark and a light terminal:
/// INVERSE would make the terminal's own background the text color and DIM
/// washes the text toward the band. Neither belongs in a band.
pub fn band_style(t: &Theme) -> (Color, Color, u8) {
    if t.inherit {
        // The palette's own surface pair: accent text on the deep index, so
        // both legs follow the emulator's scheme instead of painting a pale
        // gray bar over it.
        (Color::Indexed(3), Color::Indexed(0), 0)
    } else {
        // A named theme bands on its `sel` surface with `stamp` text: the
        // neutral text-on-surface pair. The brand belongs to the glyph and
        // state word (the highlight pass restores the lane accent there),
        // never to the whole band.
        (t.stamp, t.sel, 0)
    }
}

/// The chosen sideline card's fill: the theme accent across all 3 lines with
/// the base tone as text - the same leg-swap the `[no]` stamp uses - so the
/// selection reads as one unmistakable accent block on a dark and a light
/// terminal. The operator's 2026-10-04 sideline ruling supersedes the
/// surface-band selection for the card layout; the list layout keeps
/// [`band_style`].
pub fn chosen_card_style(t: &Theme) -> (Color, Color, u8) {
    if t.inherit {
        (Color::Indexed(0), Color::Indexed(3), 0)
    } else {
        (t.base, t.brand, 0)
    }
}

/// The muted text tone for plain rows: the theme's tested dim under a named
/// theme, the palette's own gray under the inherit theme (byte-identity with
/// the pre-theme render).
pub fn dim_fg(t: &Theme) -> Color {
    if t.inherit {
        Color::Indexed(8)
    } else {
        t.dim
    }
}

fn theme_terminal() -> Theme {
    Theme {
        name: "terminal",
        inherit_from: "",
        inherit: true,
        // Indexed(3), the brand: the pane-frame outline reads this field
        // directly (cell_style never does under `inherit` - byte-identity),
        // so the focused frame keeps its amber under terminal.
        border: Color::Indexed(3),
        title: Color::Default,
        // Index 3 follows the emulator's palette (amber/yellow in every scheme),
        // preserving the pre-theme needs-attention glyph exactly. Brand and
        // needs-you share it: terminal paints no color of its own, so the two
        // roles keep the byte-identical pre-theme render.
        brand: Color::Indexed(3),
        needs_you: Color::Indexed(3),
        ok: Color::Indexed(2),
        warn: Color::Indexed(3),
        error: Color::Indexed(1),
        sel: Color::Default,
        dim: Color::Default,
        chip: Color::Default,
        stamp: Color::Default,
        base: Color::Default,
    }
}

/// The footnote brand theme, dark twin (the Telemetry palette: the token
/// table in `internal/fno/design/brand-telemetry-palette.md`).
pub(crate) fn theme_footnote_superscript() -> Theme {
    Theme {
        name: "footnote-superscript",
        inherit_from: "",
        inherit: false,
        border: rgb(0xe8, 0xe8, 0xe8), // text (the monochrome ruling)
        title: rgb(0xe8, 0xe8, 0xe8),  // text
        brand: rgb(0xe8, 0xe8, 0xe8),  // text
        needs_you: rgb(0xc5, 0xb7, 0x84), // needs-you yellow
        ok: rgb(0x9c, 0xc4, 0x9c),     // palette green
        warn: rgb(0xc5, 0xb7, 0x84),   // palette yellow
        error: rgb(0xe1, 0xa6, 0xa3),  // palette red
        sel: rgb(0x2b, 0x2b, 0x2b),    // surface0
        dim: rgb(0xb4, 0xb4, 0xb4),    // subtext0
        chip: rgb(0xe1, 0xa6, 0xa3),   // red accent
        stamp: rgb(0xe8, 0xe8, 0xe8),  // off-white stamp label
        base: rgb(0x14, 0x14, 0x14),   // base: the theme ground
    }
}

/// The footnote brand theme, light twin (Footnote Paper: the same palette
/// with the lightness ladder flipped). The ink is the dark twin's ground:
/// one mirror pair (e8e8e8 on 141414, 141414 on f7f7f7 paper), swapped.
pub(crate) fn theme_footnote_paper() -> Theme {
    Theme {
        name: "footnote-paper",
        inherit_from: "",
        inherit: false,
        border: rgb(0x14, 0x14, 0x14), // the dark twin's ground (the mirror pair)
        title: rgb(0x14, 0x14, 0x14),  // text
        brand: rgb(0x14, 0x14, 0x14),  // ink
        needs_you: rgb(0x79, 0x68, 0x23), // needs-you olive
        ok: rgb(0x46, 0x77, 0x48),     // palette green
        warn: rgb(0x79, 0x68, 0x23),   // palette yellow
        error: rgb(0x8f, 0x4d, 0x4b),  // the palette red darkened one step: the
        // palette red itself lands 4.15:1 on paper
        sel: rgb(0xd7, 0xd7, 0xd7),   // surface0
        dim: rgb(0x40, 0x40, 0x40),   // subtext0 (505050 read too light)
        chip: rgb(0x96, 0x53, 0x51),  // red accent
        stamp: rgb(0x14, 0x14, 0x14), // ink stamp label
        base: rgb(0xf7, 0xf7, 0xf7),  // base: the theme ground
    }
}

fn theme_catppuccin() -> Theme {
    Theme {
        name: "catppuccin",
        inherit_from: "",
        inherit: false,
        border: rgb(0x89, 0xb4, 0xfa),    // blue (the theme's primary)
        title: rgb(0x89, 0xb4, 0xfa),     // blue
        brand: rgb(0x89, 0xb4, 0xfa),     // blue - its signature primary, not orange
        needs_you: rgb(0xf9, 0xe2, 0xaf), // yellow
        ok: rgb(0xa6, 0xe3, 0xa1),        // green
        warn: rgb(0xf9, 0xe2, 0xaf),      // yellow
        error: rgb(0xf3, 0x8b, 0xa8),     // red
        sel: rgb(0x31, 0x32, 0x44),       // surface0
        dim: rgb(0xa6, 0xad, 0xc8),       // subtext0
        chip: rgb(0xf3, 0x8b, 0xa8),      // red
        stamp: rgb(0xcd, 0xd6, 0xf4),     // text
        base: rgb(0x1e, 0x1e, 0x2e),      // base
    }
}

fn theme_tokyo_night() -> Theme {
    Theme {
        name: "tokyo-night",
        inherit_from: "",
        inherit: false,
        border: rgb(0x7a, 0xa2, 0xf7),    // blue (the theme's primary)
        title: rgb(0x7a, 0xa2, 0xf7),     // blue
        brand: rgb(0x7a, 0xa2, 0xf7),     // blue - its signature primary, not orange
        needs_you: rgb(0xe0, 0xaf, 0x68), // yellow
        ok: rgb(0x9e, 0xce, 0x6a),        // green
        warn: rgb(0xe0, 0xaf, 0x68),      // yellow
        error: rgb(0xf7, 0x76, 0x8e),     // red
        sel: rgb(0x33, 0x3a, 0x54),       // bg_dark-ish selection
        dim: rgb(0x96, 0x9d, 0xc4),       // fg_gutter
        chip: rgb(0xf7, 0x76, 0x8e),      // red
        stamp: rgb(0xa9, 0xb1, 0xd6),     // fg
        base: rgb(0x1a, 0x1b, 0x26),      // base
    }
}

fn theme_gruvbox() -> Theme {
    Theme {
        name: "gruvbox",
        inherit_from: "",
        inherit: false,
        border: rgb(0x8e, 0xc0, 0x7c), // aqua (the theme's signature accent)
        title: rgb(0x83, 0xa5, 0x98),  // blue
        brand: rgb(0x8e, 0xc0, 0x7c),  // aqua - its signature accent, not orange
        needs_you: rgb(0xfa, 0xbd, 0x2f), // yellow
        ok: rgb(0xb8, 0xbb, 0x26),     // green
        warn: rgb(0xfa, 0xbd, 0x2f),   // yellow
        error: rgb(0xfc, 0x5f, 0x45),  // the gruvbox red lightened one step:
        // the raw red lands 4.3:1 on its base
        sel: rgb(0x3c, 0x38, 0x36),   // bg1
        dim: rgb(0xa8, 0x99, 0x84),   // fg4
        chip: rgb(0xfb, 0x49, 0x34),  // red
        stamp: rgb(0xeb, 0xdb, 0xb2), // fg1
        base: rgb(0x28, 0x28, 0x28),  // base
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// The Terminal 16 palette of the two footnote themes (the token tables in
/// internal/fno/design/brand-telemetry-palette.md): the color a colors-tab
/// swatch paints for ANSI slot `slot` under the active theme. `None` = the
/// theme defines no terminal palette of its own, or `slot` sits outside the
/// 16 ANSI slots; the swatch then rides `Indexed(slot)` (the emulator
/// resolves it) and shows no hex.
pub fn terminal16_slot(slot: u8, theme: &Theme) -> Option<Color> {
    const DARK: [Color; 16] = [
        rgb(0x40, 0x40, 0x40), // black
        rgb(0xe1, 0xa6, 0xa3), // red
        rgb(0x9c, 0xc4, 0x9c), // green
        rgb(0xc5, 0xb7, 0x84), // yellow
        rgb(0x9f, 0xb8, 0xe5), // blue
        rgb(0xd7, 0xa6, 0xc6), // magenta
        rgb(0x83, 0xc6, 0xbd), // cyan
        rgb(0xce, 0xce, 0xce), // white
        rgb(0x55, 0x55, 0x55), // bright black (gray)
        rgb(0xdc, 0x92, 0x8d), // bright red (light_red)
        rgb(0x82, 0xb8, 0x88), // bright green (light_green)
        rgb(0xb8, 0xa9, 0x65), // bright yellow (light_yellow)
        rgb(0x8c, 0xa8, 0xe2), // bright blue (light_blue)
        rgb(0xd0, 0x92, 0xb9), // bright magenta (light_magenta)
        rgb(0x5e, 0xbb, 0xb2), // bright cyan (light_cyan)
        rgb(0xe8, 0xe8, 0xe8), // bright white (light_white)
    ];
    const LIGHT: [Color; 16] = [
        // Slots 0 and 8 carry dim TEXT on the paper ground
        // (claude tool-call grays, codex dim lines); bfbfbf read 1.7:1 on
        // f7f7f7. 606060 clears 4.5:1 with room, 6e6e6e lands on it.
        rgb(0x60, 0x60, 0x60), // black
        rgb(0x96, 0x53, 0x51), // red
        rgb(0x46, 0x77, 0x48), // green
        rgb(0x79, 0x68, 0x23), // yellow
        rgb(0x4c, 0x68, 0x9d), // blue
        rgb(0x8b, 0x53, 0x79), // magenta
        rgb(0x08, 0x79, 0x70), // cyan
        rgb(0x3c, 0x3c, 0x3c), // white
        rgb(0x6e, 0x6e, 0x6e), // bright black (gray)
        rgb(0xae, 0x5a, 0x56), // bright red (light_red)
        rgb(0x46, 0x88, 0x4f), // bright green (light_green)
        rgb(0x89, 0x76, 0x15), // bright yellow (light_yellow)
        rgb(0x55, 0x74, 0xb7), // bright blue (light_blue)
        rgb(0xa2, 0x5b, 0x89), // bright magenta (light_magenta)
        rgb(0x06, 0x89, 0x80), // bright cyan (light_cyan)
        rgb(0x29, 0x29, 0x29), // bright white (light_white)
    ];
    let i = slot as usize;
    if i >= 16 {
        return None;
    }
    // A user theme rides its parent's 16-slot table (`inherit_from`); a
    // theme with neither its own name nor a slot-defining parent paints no
    // palette and only the OSC 11/10 ground.
    let table = match theme.name {
        "footnote-superscript" => Some(&DARK),
        "footnote-paper" => Some(&LIGHT),
        _ => match theme.inherit_from {
            "footnote-superscript" => Some(&DARK),
            "footnote-paper" => Some(&LIGHT),
            _ => None,
        },
    };
    table.map(|t| t[i])
}

/// The `#rrggbb` string of an RGB color, for the hex a swatch shows beside
/// the name; non-RGB colors resolve through the emulator and name no hex.
pub fn color_hex(c: Color) -> Option<String> {
    let Color::Rgb(r, g, b) = c else {
        return None;
    };
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

/// Whether the theme's ground is light: the same signal [`terminal16_slot`]
/// tables were tuned against and the spawn path's `COLORFGBG` encodes. All
/// three channels must clear the midpoint, so a saturated warm ground never
/// reads light on its red channel alone.
pub fn is_light(t: &Theme) -> bool {
    matches!(t.base, Color::Rgb(r, g, b) if r > 127 && g > 127 && b > 127)
}

/// The OSC sequences that paint the terminal ground under a footnote theme:
/// default background (11), default foreground (10), and the 16 palette
/// slots (4), all from the theme's own tokens. `None` = the theme defines no
/// ground (`terminal`, or any theme whose base is `Default`); the caller
/// then emits nothing.
pub fn ground_set(t: &Theme) -> Option<Vec<u8>> {
    let base = color_hex(t.base)?;
    let fg = color_hex(t.stamp).unwrap_or_else(|| base.clone());
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(format!("\x1b]11;{base}\x1b\\\x1b]10;{fg}\x1b\\").as_bytes());
    for slot in 0..16u8 {
        if let Some(c) = terminal16_slot(slot, t) {
            let hex = color_hex(c)?;
            out.extend_from_slice(format!("\x1b]4;{slot};{hex}\x1b\\").as_bytes());
        }
    }
    Some(out)
}

/// Reset the terminal's default background (111), foreground (110) and the
/// palette (104) back to the user's own scheme. Harmless when never set.
pub const GROUND_RESTORE: &[u8] = b"\x1b]111\x1b\\\x1b]110\x1b\\\x1b]104\x1b\\";

/// The terminal theme files' text for one theme: the same token tables
/// [`terminal16_slot`] reads, rendered per terminal. `None` = no ground or
/// an unknown terminal.
pub fn terminal_theme_file(t: &Theme, terminal: &str) -> Option<String> {
    let base = color_hex(t.base)?;
    let fg = color_hex(t.stamp)?;
    let slots: Vec<(u8, u8, u8)> = (0..16u8)
        .map(|s| match terminal16_slot(s, t) {
            Some(Color::Rgb(r, g, b)) => (r, g, b),
            _ => (0, 0, 0),
        })
        .collect();
    match terminal {
        "ghostty" => Some(format!(
            "palette = {}\nbackground = {base}\nforeground = {fg}\n",
            slots
                .iter()
                .enumerate()
                .map(|(s, (r, g, b))| format!("{s}=#{r:02x}{g:02x}{b:02x}"))
                .collect::<Vec<_>>()
                .join(",")
        )),
        "kitty" => Some(format!(
            "background {base}\nforeground {fg}\n{}",
            slots
                .iter()
                .enumerate()
                .map(|(s, (r, g, b))| format!("color{s} #{r:02x}{g:02x}{b:02x}\n"))
                .collect::<String>()
        )),
        "iterm" => {
            // iTerm colors are 0.0-1.0 float components in a JSON dict.
            let comp = |f: u8| format!("{:.6}", f as f64 / 255.0);
            let mut out = String::from("{\n");
            for (s, (r, g, b)) in slots.iter().enumerate() {
                out.push_str(&format!(
                    "  \"Ansi {s} Color\" : {{ \"Red Component\" : {r}, \"Green Component\" : {g}, \"Blue Component\" : {b} }}",
                    r = comp(*r),
                    g = comp(*g),
                    b = comp(*b)
                ));
                out.push_str(if s == 15 { "\n" } else { ",\n" });
            }
            out.push_str("}\n");
            Some(out)
        }
        "wezterm" => Some(format!(
            "[colors]\nbackground = '{base}'\nforeground = '{fg}'\nansi = [{}]\nbrights = [{}]",
            slots[..8]
                .iter()
                .map(|(r, g, b)| format!("'#{r:02x}{g:02x}{b:02x}',"))
                .collect::<String>(),
            slots[8..]
                .iter()
                .map(|(r, g, b)| format!("'#{r:02x}{g:02x}{b:02x}',"))
                .collect::<String>()
        )),
        _ => None,
    }
}

/// The shipped theme names, in display order. Adding a palette later is a
/// new `theme_*` fn, a match arm in [`Theme::from_name`], and a name here.
pub const THEME_NAMES: [&str; 6] = [
    "footnote-superscript",
    "footnote-paper",
    "terminal",
    "catppuccin",
    "tokyo-night",
    "gruvbox",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_ladder_warns_unknown_and_defaults_silently() {
        let (t, warn) = Theme::from_name("solarized-light");
        assert_eq!(t.name, "footnote-superscript");
        let w = warn.expect("unknown theme must warn");
        assert!(w.0.contains("solarized-light"), "{}, got {w:?}", w.0);
        assert!(w.0.contains("footnote-superscript"));
        // An unset config key reads as "" and means "no preference", not a
        // typo: the default lands silently.
        let (t, warn) = Theme::from_name("");
        assert_eq!(t.name, "footnote-superscript");
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
    fn a_light_background_defaults_to_paper() {
        assert_eq!(Theme::default_for(true).name, "footnote-paper");
        assert_eq!(Theme::default_for(false).name, "footnote-superscript");
        assert_eq!(Theme::default_theme().name, "footnote-superscript");
    }

    /// The activity ramp's color ends must stay legible on the card ground
    /// in every painted theme: WCAG AA text contrast, 4.5:1, against the
    /// theme's own base. `terminal` is exempt - it paints no color of its
    /// own; the emulator's palette owns contrast there.
    #[test]
    fn activity_ramp_colors_clear_wcag_against_their_ground() {
        fn lum(c: Color) -> Option<f64> {
            match c {
                Color::Rgb(r, g, b) => {
                    let ch = |v: u8| {
                        let s = f64::from(v) / 255.0;
                        if s <= 0.03928 {
                            s / 12.92
                        } else {
                            ((s + 0.055) / 1.055).powf(2.4)
                        }
                    };
                    Some(0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b))
                }
                _ => None,
            }
        }
        for n in THEME_NAMES {
            let (t, _) = Theme::from_name(n);
            if t.inherit {
                continue;
            }
            let Some(base) = lum(t.base) else {
                panic!("{n}: the card ground must be a concrete color");
            };
            for (role, c) in [("ok", t.ok), ("warn", t.warn), ("error", t.error)] {
                let Some(fg) = lum(c) else {
                    panic!("{n}: {role} must be a concrete color");
                };
                let (hi, lo) = if fg > base { (fg, base) } else { (base, fg) };
                let ratio = (hi + 0.05) / (lo + 0.05);
                assert!(ratio >= 4.5, "{n} {role} {c:?} on {base:?}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn every_named_theme_pairs_a_distinct_brand_and_needs_you() {
        // Attention and selection are different states, so no theme
        // may paint them the same color. `terminal` is the one exemption: it
        // paints no color of its own and both roles ride the emulator's
        // index 3, which is exactly the byte-identical pre-theme render.
        for n in THEME_NAMES {
            let (t, _) = Theme::from_name(n);
            if t.inherit {
                continue;
            }
            assert_ne!(
                t.brand, t.needs_you,
                "{n} must not paint selection and attention alike"
            );
        }
    }

    #[test]
    fn terminal_is_a_true_no_op_on_color() {
        // The load-bearing property: under terminal every role resolves to
        // Default fg/bg, so the flag set alone carries every visual distinction
        // and a pre-theme render is byte-identical. Border is the one named
        // exception: it reads the theme's own Indexed(3) amber (and any
        // mux.theme.border override), the same field the pane frame paints.
        let t = theme_terminal();
        for role in [
            Role::Title,
            Role::Chip,
            Role::Subtitle,
            Role::Tab(true),
            Role::Tab(false),
            Role::Footer,
            Role::PanelBody,
            Role::BodySel,
            Role::BodyCursor,
            Role::BodyDim,
            Role::ScrollTrack,
            Role::ScrollThumb,
            Role::Stamp,
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
        assert_eq!(theme_terminal().needs_you, Color::Indexed(3));
        assert_eq!(theme_terminal().brand, Color::Indexed(3));
    }

    #[test]
    fn named_themes_color_the_chrome() {
        // A named theme must actually differ from terminal on the chrome roles,
        // otherwise the picker offers no choice.
        let term = theme_terminal();
        for t in [
            theme_footnote_superscript(),
            theme_footnote_paper(),
            theme_catppuccin(),
            theme_tokyo_night(),
            theme_gruvbox(),
        ] {
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
    fn borders_paint_the_theme_brand_and_the_three_themes_pick_a_signature_accent() {
        // The ruling: every border paints the running theme's brand. Each
        // named theme's border field names its own brand value, and the
        // three orange-family brands become each palette's signature accent
        // (catppuccin blue, tokyo-night blue, gruvbox aqua); the footnote
        // themes went monochrome: their text color (superscript) and their
        // ink (paper).
        let expected = [
            ("footnote-superscript", (0xe8, 0xe8, 0xe8)),
            ("footnote-paper", (0x14, 0x14, 0x14)),
            ("catppuccin", (0x89, 0xb4, 0xfa)),
            ("tokyo-night", (0x7a, 0xa2, 0xf7)),
            ("gruvbox", (0x8e, 0xc0, 0x7c)),
        ];
        for (name, (r, g, b)) in expected {
            let (t, warn) = Theme::from_name(name);
            assert!(warn.is_none(), "{name}");
            let brand = format!("#{:02x}{:02x}{:02x}", r, g, b);
            assert_eq!(t.brand, Color::Rgb(r, g, b), "{name} brand");
            assert_eq!(t.border, t.brand, "{name}: borders wear the brand");
            assert_eq!(
                cell_style(Role::Border, &t).0,
                t.brand,
                "{name}: Role::Border paints {brand}"
            );
        }
        // terminal keeps its own brand on the border (the pane-frame outline
        // reads the field directly); cell_style stays byte-identical.
        assert_eq!(theme_terminal().border, Color::Indexed(3));
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
    fn the_stamp_is_reverse_video_under_every_theme() {
        // The mark's [no] is a stamp in every theme: INVERSE, with the
        // theme's stamp label as the fg (the swap makes it the label bg).
        for n in THEME_NAMES {
            let t = Theme::from_name(n).0;
            let (fg, bg, flags) = cell_style(Role::Stamp, &t);
            assert_eq!(fg, t.stamp, "{n} stamp fg is the label color");
            assert_eq!(bg, Color::Default, "{n} stamp letters are the terminal's");
            assert_eq!(
                flags & cell_flags::INVERSE,
                cell_flags::INVERSE,
                "{n} stamp is reverse video"
            );
        }
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
            theme_footnote_superscript(),
            theme_footnote_paper(),
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
    fn modal_bodies_never_paint_inverse_under_named_themes() {
        // The one-rule body: under a named theme no body role resolves to the
        // INVERSE block - the ground is the one fill. (Under `terminal` the
        // cursor band and dim rows keep INVERSE by design; byte-identity.)
        for t in [
            theme_footnote_superscript(),
            theme_footnote_paper(),
            theme_catppuccin(),
            theme_tokyo_night(),
            theme_gruvbox(),
        ] {
            for role in [Role::BodySel, Role::BodyCursor, Role::BodyDim] {
                let (_, _, flags) = cell_style(role, &t);
                assert!(
                    flags & cell_flags::INVERSE == 0,
                    "{role:?} must not carry INVERSE under {}",
                    t.name
                );
            }
        }
    }

    #[test]
    fn panel_hierarchy_roles_are_distinct_under_both_theme_kinds() {
        // The whole point of the hierarchy: head, label, meta and body must
        // resolve to DIFFERENT styles, or every line reads at one weight
        // again (the defect the user reported).
        for t in [theme_terminal(), theme_footnote_superscript()] {
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
        for t in [
            theme_footnote_superscript(),
            theme_footnote_paper(),
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
    #[test]
    fn ground_set_paints_background_foreground_and_palette() {
        let (t, _) = Theme::from_name("footnote-superscript");
        let bytes = ground_set(&t).expect("superscript defines a ground");
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.starts_with("\x1b]11;#141414\x1b\\"),
            "OSC 11 sets the base first: {s:?}"
        );
        assert!(
            s.contains("\x1b]10;#e8e8e8\x1b\\"),
            "OSC 10 sets the stamp fg: {s:?}"
        );
        assert!(
            s.contains("\x1b]4;1;#e1a6a3\x1b\\"),
            "OSC 4 slot 1 from the dark table: {s:?}"
        );
        assert_eq!(s.matches("\x1b]4;").count(), 16, "all 16 slots");
        // The ground restore resets all three classes.
        assert_eq!(
            GROUND_RESTORE,
            b"\x1b]111\x1b\\\x1b]110\x1b\\\x1b]104\x1b\\"
        );
        // No ground under terminal: nothing to set, nothing to leak.
        let (term, _) = Theme::from_name("terminal");
        assert!(ground_set(&term).is_none());
    }

    #[test]
    fn committed_terminal_theme_files_match_the_token_tables() {
        // The asset files are generated from the SAME DARK/LIGHT tables
        // terminal16_slot reads; this pins them so the two can never drift.

        let (t, _) = Theme::from_name("footnote-superscript");
        let ghostty = terminal_theme_file(&t, "ghostty").expect("ghostty file");
        assert!(
            ghostty.contains("palette = 0=#404040,1=#e1a6a3,"),
            "{ghostty:?}"
        );
        assert!(ghostty.contains("background = #141414"));
        let kitty = terminal_theme_file(&t, "kitty").expect("kitty file");
        assert!(kitty.contains("color1 #e1a6a3"));
        let wez = terminal_theme_file(&t, "wezterm").expect("wezterm file");
        assert!(wez.contains("ansi = ['#404040','#e1a6a3',"));
        let iterm = terminal_theme_file(&t, "iterm").expect("iterm file");
        assert!(iterm.contains("\"Ansi 1 Color\""));
        assert!(terminal_theme_file(&t, "alacritty").is_none());
        let (term, _) = Theme::from_name("terminal");
        assert!(terminal_theme_file(&term, "ghostty").is_none());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/terminal");
        for name in ["footnote-superscript", "footnote-paper"] {
            let (t, _) = Theme::from_name(name);
            for (term, ext) in [
                ("ghostty", ""),
                ("kitty", ".conf"),
                ("iterm", ".itermcolors"),
                ("wezterm", ".toml"),
            ] {
                let want = terminal_theme_file(&t, term).expect("generated text");
                let path = dir.join(term).join(format!("{name}{ext}"));
                let got = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                assert_eq!(got, want, "drift in {}", path.display());
            }
        }
    }

    #[test]
    fn paper_text_slots_clear_4_5_to_1_on_the_paper_ground() {
        // Slots 0 and 8 carry dim text (claude grays, codex dim
        // lines) on the f7f7f7 ground; bfbfbf/a8a8a8 read 1.7:1 and 2.2:1.
        // Text slots clear WCAG 4.5:1 against the theme's own base.
        let (t, _) = Theme::from_name("footnote-paper");
        let Color::Rgb(br, bg_, bb) = t.base else {
            panic!("paper has an rgb ground");
        };
        let lum = |(r, g, b): (u8, u8, u8)| {
            let ch = |v: u8| {
                let s = f64::from(v) / 255.0;
                if s <= 0.03928 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
        };
        let lb = lum((br, bg_, bb));
        let ratio = |c: Color| {
            let Color::Rgb(r, g, b) = c else {
                panic!("expected rgb");
            };
            let lf = lum((r, g, b));
            let (hi, lo) = if lb > lf { (lb, lf) } else { (lf, lb) };
            (hi + 0.05) / (lo + 0.05)
        };
        for slot in [0u8, 8] {
            let c = terminal16_slot(slot, &t).expect("paper defines its 16");
            assert!(
                ratio(c) >= 4.5,
                "slot {slot} is {:.2}:1 on the paper ground",
                ratio(c)
            );
        }
        assert!(ratio(t.dim) >= 4.5, "dim is {:.2}:1", ratio(t.dim));
    }
}
