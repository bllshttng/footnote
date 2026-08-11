//! Render a composed [`Frame`] to standalone HTML, so a chrome change can be
//! LOOKED AT rather than argued about from cell assertions.
//!
//! A cell assertion proves a flag is set. It cannot tell you a prompt washed
//! out, which is what an operator actually reports.
//!
//! The colour model imitates what terminals do, not what the spec permits:
//! `INVERSE` swaps the resolved pair, `BOLD` brightens the foreground BEFORE
//! that swap (the default in iTerm2, Terminal.app and GNOME Terminal), `DIM`
//! drops it toward the background.
//!
//! Test-only: a lens on the render path, never shipped chrome.

use crate::proto::{cell_flags, Cell, Color, Frame};

/// A terminal theme: its default pair AND its 16-colour palette.
///
/// The palette is the half that is easy to forget. An `Indexed` colour is a
/// LOOKUP into the reader's scheme, so checking one against an idealised xterm
/// palette certifies something nobody runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub name: &'static str,
    /// ANSI 0-15 as this scheme defines them.
    pub ansi: [(u8, u8, u8); 16],
}

/// The idealised xterm 16, where 0 is black and 15 is white. No real scheme
/// ships exactly this, which is why it must not be the only palette a contrast
/// check sees; the two named schemes below carry their own.
const XTERM_16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcc, 0x33, 0x33),
    (0x33, 0xcc, 0x33),
    (0xcc, 0xcc, 0x33),
    (0x33, 0x66, 0xcc),
    (0xcc, 0x33, 0xcc),
    (0x33, 0xcc, 0xcc),
    (0xcc, 0xcc, 0xcc),
    (0x66, 0x66, 0x66),
    (0xff, 0x66, 0x66),
    (0x66, 0xff, 0x66),
    (0xff, 0xff, 0x66),
    (0x66, 0x99, 0xff),
    (0xff, 0x66, 0xff),
    (0x66, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

/// Close to the common dark defaults (Tomorrow Night / One Dark family).
pub const DARK: Theme = Theme {
    fg: (0xc5, 0xc8, 0xc6),
    bg: (0x1d, 0x1f, 0x21),
    name: "dark",
    ansi: XTERM_16,
};

/// Solarized Light, transcribed from `iTerm2 Solarized Light` as Ghostty ships
/// it. A REAL light scheme, not a white background wearing [`XTERM_16`]: that
/// pairing is itself a fiction, since the xterm 16 were picked for dark
/// terminals, and it reports a yellow accent at 1.7:1 that no light scheme
/// actually ships (Solarized darkens its yellow to `#b58900` for exactly this).
pub const LIGHT: Theme = Theme {
    fg: (0x65, 0x7b, 0x83),
    bg: (0xfd, 0xf6, 0xe3),
    name: "solarized light",
    ansi: [
        (0x07, 0x36, 0x42),
        (0xdc, 0x32, 0x2f),
        (0x85, 0x99, 0x00),
        (0xb5, 0x89, 0x00),
        (0x26, 0x8b, 0xd2),
        (0xd3, 0x36, 0x82),
        (0x2a, 0xa1, 0x98),
        (0xbb, 0xb5, 0xa2),
        (0x00, 0x2b, 0x36),
        (0xcb, 0x4b, 0x16),
        (0x58, 0x6e, 0x75),
        (0x65, 0x7b, 0x83),
        (0x83, 0x94, 0x96),
        (0x6c, 0x71, 0xc4),
        (0x93, 0xa1, 0xa1),
        (0xfd, 0xf6, 0xe3),
    ],
};

/// Catppuccin Macchiato, transcribed from the palette Ghostty ships at
/// `Ghostty.app/Contents/Resources/ghostty/themes/Catppuccin Macchiato`.
///
/// A scheme this project is actually read in, and the counter-example that pays
/// for the `ansi` field: index 0 is `#494d64` and index 15 is `#b8c0e0`, so
/// 0-on-15 measures 4.6:1 here against 21:1 on the idealised palette. The
/// Catppuccin / Nord / Gruvbox family all compress their ends this way.
pub const MACCHIATO: Theme = Theme {
    fg: (0xca, 0xd3, 0xf5),
    bg: (0x24, 0x27, 0x3a),
    name: "catppuccin macchiato",
    ansi: [
        (0x49, 0x4d, 0x64),
        (0xed, 0x87, 0x96),
        (0xa6, 0xda, 0x95),
        (0xee, 0xd4, 0x9f),
        (0x8a, 0xad, 0xf4),
        (0xf5, 0xbd, 0xe6),
        (0x8b, 0xd5, 0xca),
        (0xa5, 0xad, 0xcb),
        (0x5b, 0x60, 0x78),
        (0xec, 0x74, 0x86),
        (0x8c, 0xcf, 0x7f),
        (0xe1, 0xc6, 0x82),
        (0x78, 0xa1, 0xf6),
        (0xf2, 0xa9, 0xdd),
        (0x63, 0xcb, 0xc0),
        (0xb8, 0xc0, 0xe0),
    ],
};

pub const THEMES: [Theme; 3] = [DARK, LIGHT, MACCHIATO];

/// Resolve a palette index against `theme`: its own 16 for the system colours,
/// the standard cube and greyscale ramp above that.
fn indexed_rgb(i: u8, theme: Theme) -> (u8, u8, u8) {
    match i {
        0..=15 => theme.ansi[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

fn resolve(c: Color, fallback: (u8, u8, u8), theme: Theme) -> (u8, u8, u8) {
    match c {
        Color::Default => fallback,
        Color::Indexed(i) => indexed_rgb(i, theme),
        Color::Rgb(r, g, b) => (r, g, b),
    }
}

/// Brighten toward white, the way a terminal renders bold as a bright colour.
fn brighten((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
    let up = |v: u8| v.saturating_add(((255 - v) as f32 * 0.45) as u8);
    (up(r), up(g), up(b))
}

/// Blend `fg` toward `bg`, the way a terminal renders SGR 2 (faint).
fn dim(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let mix = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
    (mix(fg.0, bg.0), mix(fg.1, bg.1), mix(fg.2, bg.2))
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The (foreground, background) a terminal would actually paint this cell in.
/// Exposed so a test can assert the CONTRAST a rule produces, not merely which
/// flag bits are set.
pub fn cell_colors(cell: &Cell, theme: Theme) -> ((u8, u8, u8), (u8, u8, u8)) {
    let mut fg = resolve(cell.fg, theme.fg, theme);
    let mut bg = resolve(cell.bg, theme.bg, theme);
    // Bold brightens the foreground BEFORE the inverse swap, exactly as a
    // terminal does: SGR 1 sets a foreground attribute and SGR 7 then swaps the
    // resolved pair. That order is what turns bold-over-inverse into a bright
    // BACKGROUND with the theme's background colour as the text.
    if cell.flags & cell_flags::BOLD != 0 {
        fg = brighten(fg);
    }
    if cell.flags & cell_flags::DIM != 0 {
        fg = dim(fg, bg);
    }
    let inverse =
        (cell.flags & cell_flags::INVERSE != 0) ^ (cell.flags & cell_flags::SELECTED != 0);
    if inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

/// Relative luminance (WCAG), for the contrast ratio below.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let ch = |v: u8| {
        let s = v as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
}

/// The WCAG contrast ratio between a cell's painted text and its background,
/// from 1.0 (invisible) to 21.0. The readability floor a chrome cell must clear
/// is asserted by the tests that call this.
pub fn contrast_ratio(cell: &Cell, theme: Theme) -> f64 {
    let (fg, bg) = cell_colors(cell, theme);
    let (a, b) = (luminance(fg), luminance(bg));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// The contrast `theme` gives its own body text - every ordinary character its
/// user reads all day.
///
/// This, not an absolute ratio, is the bar chrome has to clear. An absolute
/// floor asks the modal to be MORE readable than the scheme its user chose,
/// which can only be met by overriding their colours, which is the thing that
/// already failed here once. Solarized Light sits at 4.1:1 on purpose.
pub fn body_contrast(theme: Theme) -> f64 {
    contrast_ratio(
        &Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        },
        theme,
    )
}

/// The WORST contrast this cell reaches across [`THEMES`]. Chrome is judged on
/// this, not on its best case: a prompt that is only legible on the theme the
/// author happened to run is the defect, not the fix.
pub fn worst_contrast(cell: &Cell) -> (f64, Theme) {
    THEMES
        .iter()
        .map(|t| (contrast_ratio(cell, *t), *t))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .expect("THEMES is non-empty")
}

fn escape(c: char) -> String {
    match c {
        '&' => "&amp;".into(),
        '<' => "&lt;".into(),
        '>' => "&gt;".into(),
        ' ' => "\u{a0}".into(),
        '\0' => "\u{a0}".into(),
        other => other.to_string(),
    }
}

/// Render `frame` as a standalone HTML page titled `title`, once per theme, so
/// the two are side by side and a theme-dependent washout is visible rather than
/// argued about.
pub fn frame_html(frame: &Frame, title: &str) -> String {
    let panes: String = THEMES
        .iter()
        .map(|t| {
            format!(
                "<section><h2>{}</h2><div class=\"screen\" style=\"background:{}\">{}</div></section>",
                t.name,
                hex(t.bg),
                frame_body(frame, *t)
            )
        })
        .collect();
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
<style>\
body{{background:#0b0c0d;color:#c5c8c6;font:14px/1.15 'SF Mono',Menlo,'DejaVu Sans Mono',monospace;margin:0;padding:18px}}\
h1{{font:600 13px/1.4 -apple-system,system-ui,sans-serif;color:#9aa0a6;margin:0 0 14px;letter-spacing:.04em;text-transform:uppercase}}\
h2{{font:600 11px/1.4 -apple-system,system-ui,sans-serif;color:#6b7075;margin:0 0 6px;letter-spacing:.08em;text-transform:uppercase}}\
section{{margin:0 0 22px}}\
.screen{{display:inline-block;padding:8px;border-radius:6px;max-width:100%;overflow-x:auto}}\
.row{{white-space:pre;height:1.15em}}\
span{{white-space:pre}}\
</style><h1>{title}</h1>{panes}"
    )
}

fn frame_body(frame: &Frame, theme: Theme) -> String {
    let cols = frame.cols as usize;
    let mut body = String::new();
    for r in 0..frame.rows as usize {
        body.push_str("<div class=\"row\">");
        // Coalesce runs of identical style so the page stays small enough to
        // open instantly at 200x50.
        let mut c = 0usize;
        while c < cols {
            let cell = &frame.cells[r * cols + c];
            if cell.flags & cell_flags::WIDE_SPACER != 0 {
                c += 1;
                continue;
            }
            let (fg, bg) = cell_colors(cell, theme);
            let bold = cell.flags & cell_flags::BOLD != 0;
            let under = cell.flags & cell_flags::UNDERLINE != 0;
            let italic = cell.flags & cell_flags::ITALIC != 0;
            let mut run = String::new();
            while c < cols {
                let n = &frame.cells[r * cols + c];
                if n.flags & cell_flags::WIDE_SPACER != 0 {
                    c += 1;
                    continue;
                }
                let (nfg, nbg) = cell_colors(n, theme);
                if (nfg, nbg) != (fg, bg)
                    || (n.flags & cell_flags::BOLD != 0) != bold
                    || (n.flags & cell_flags::UNDERLINE != 0) != under
                    || (n.flags & cell_flags::ITALIC != 0) != italic
                {
                    break;
                }
                run.push_str(&escape(n.c));
                c += 1;
            }
            body.push_str(&format!(
                "<span style=\"color:{};background:{}{}{}{}\">{run}</span>",
                hex(fg),
                hex(bg),
                if bold { ";font-weight:700" } else { "" },
                if under {
                    ";text-decoration:underline"
                } else {
                    ""
                },
                if italic { ";font-style:italic" } else { "" },
            ));
        }
        body.push_str("</div>");
    }
    body
}

/// Write `frame` to `<dir>/<name>.html`; `dir` is `$FNO_UX_SHOTS`, else this
/// crate's `target/ux-shots` so `cargo test ux_shot` leaves the pictures on disk
/// with no variable to know about, gitignored by construction.
///
/// Best-effort: a failed write returns `None`. The assertions are the gate.
pub fn write_shot(frame: &Frame, name: &str, title: &str) -> Option<std::path::PathBuf> {
    let dir = match std::env::var_os("FNO_UX_SHOTS").filter(|d| !d.is_empty()) {
        Some(d) => std::path::PathBuf::from(d),
        None => std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/ux-shots"),
    };
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{name}.html"));
    std::fs::write(&path, frame_html(frame, title)).ok()?;
    // A plain-text twin beside it: the HTML is for eyes, this is for a terminal,
    // a diff, or a CI log where nobody can open a browser.
    let _ = std::fs::write(
        dir.join(format!("{name}.txt")),
        crate::vt::frame_text(frame),
    );
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(flags: u8) -> Cell {
        Cell {
            c: 'x',
            fg: Color::Default,
            bg: Color::Default,
            flags,
        }
    }

    #[test]
    fn bold_over_inverse_costs_contrast_on_a_light_theme() {
        // The mechanism, made measurable. Bold sets a BRIGHTER foreground and
        // inverse then swaps the pair, so the brightening lands on what became
        // the background. On a light theme that pulls the two toward each other
        // (dark-ish background, white text lifting toward it); on a dark theme it
        // happens to push them apart. Chrome has to hold up on both, which is
        // why the modal now paints explicit colours instead of inheriting.
        let plain = contrast_ratio(&cell(cell_flags::INVERSE), LIGHT);
        let bolded = contrast_ratio(&cell(cell_flags::INVERSE | cell_flags::BOLD), LIGHT);
        assert!(
            plain > bolded,
            "on a light theme bold over inverse should LOSE contrast: \
             plain {plain:.2} vs bold {bolded:.2}"
        );
    }

    #[test]
    fn inverse_swaps_the_pair() {
        let ((fr, _, _), (br, _, _)) = cell_colors(&cell(cell_flags::INVERSE), DARK);
        assert_eq!((fr, br), (DARK.bg.0, DARK.fg.0));
    }

    #[test]
    fn palette_extremes_are_not_extremes_in_a_real_scheme() {
        // The premise that killed a fix: "index 0 and index 15 are the two
        // colours every scheme keeps at the ends, so painting them is
        // theme-independent." It is false, and this is the counter-example.
        //
        // Left here as an executable one, not a comment, because the idea is
        // reasonable enough to be reinvented: whoever next reaches for a
        // "guaranteed contrast" indexed pair gets the numbers instead of the
        // argument.
        let pair = |fg, bg| Cell {
            c: 'x',
            fg,
            bg,
            flags: 0,
        };
        let extremes = pair(Color::Indexed(0), Color::Indexed(15));
        let ideal = contrast_ratio(&extremes, DARK); // idealised xterm 16
        let real = contrast_ratio(&extremes, MACCHIATO);
        assert!(
            ideal > 20.0,
            "on an idealised palette 0-on-15 is black on white: {ideal:.2}"
        );
        assert!(
            real < 5.0,
            "in Macchiato 0 is #494d64 and 15 is #b8c0e0, so the same cell is \
             muted: expected under 5:1, got {real:.2}"
        );
        // And the thing it was supposed to improve on beats it there, which is
        // the whole lesson: a scheme's default pair is the one pair its author
        // guaranteed readable, because every character its user reads uses it.
        let inverted = pair(Color::Default, Color::Default);
        let inverted = Cell {
            flags: cell_flags::INVERSE,
            ..inverted
        };
        // Compared against the scheme's own body text, not against `real`:
        // coupling the two leaves 7% of headroom and flips if either palette
        // entry is re-transcribed, for reasons unrelated to the lesson.
        let inherited = contrast_ratio(&inverted, MACCHIATO);
        assert!(
            inherited >= body_contrast(MACCHIATO) - 0.01,
            "inheriting and inverting ({inherited:.2}) should keep the scheme's \
             own contrast, where naming the palette extremes gives {real:.2}"
        );
    }

    #[test]
    fn worst_contrast_reports_the_losing_theme() {
        // Bold over inverse brightens what became the background, so its worst
        // case is a LIGHT-background scheme. Asserted as the property rather
        // than a theme name, which is what actually makes it a guard against a
        // single-theme spot check.
        let (_, theme) = worst_contrast(&cell(cell_flags::INVERSE | cell_flags::BOLD));
        assert!(
            luminance(theme.bg) > luminance(theme.fg),
            "expected a light-background scheme to be the worst case, got {}",
            theme.name
        );
    }

    #[test]
    fn inverting_the_default_pair_costs_a_scheme_nothing() {
        // The guarantee that replaced the absolute floor: reversing a theme's
        // own pair keeps its exact contrast on every theme, so chrome built that
        // way is never less readable than the text around it. No named pair can
        // promise this - `palette_extremes_are_not_extremes_in_a_real_scheme`
        // is the counter-example.
        for theme in THEMES {
            let inverted = contrast_ratio(&cell(cell_flags::INVERSE), theme);
            let body = body_contrast(theme);
            assert!(
                (inverted - body).abs() < 0.01,
                "{}: inverted {inverted:.2} should equal body {body:.2}",
                theme.name
            );
        }
    }
}
