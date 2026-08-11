//! Render a composed [`Frame`] to standalone HTML, so a chrome change can be
//! LOOKED AT rather than argued about from cell assertions.
//!
//! A cell assertion proves a flag is set. It cannot tell you that reverse video
//! plus bold washes a prompt out to light-on-light, which is the class of defect
//! this module exists to make visible: the operator reports what they can see,
//! and the reply has to be evidence in the same currency.
//!
//! The colour model deliberately imitates what mainstream terminals actually do,
//! not what the spec permits:
//!
//! - `INVERSE` swaps the resolved foreground and background, as SGR 7 does.
//! - `BOLD` brightens the foreground, which is the DEFAULT in iTerm2,
//!   Terminal.app and GNOME Terminal ("draw bold text in bright colors"). This
//!   is the whole reason `BOLD` over `INVERSE` is a bug: the brightening lands
//!   on the swapped-in background.
//! - `DIM` drops the foreground toward the background.
//!
//! Test-only: this is a debugging lens on the render path, never shipped chrome.

use crate::proto::{cell_flags, Cell, Color, Frame};

/// A terminal theme's default pair. Chrome has to survive BOTH: the same
/// `INVERSE` cell that reads beautifully on a dark theme can wash out on a light
/// one, so a single-theme check certifies nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub name: &'static str,
}

/// Close to the common dark defaults (Tomorrow Night / One Dark family).
pub const DARK: Theme = Theme {
    fg: (0xc5, 0xc8, 0xc6),
    bg: (0x1d, 0x1f, 0x21),
    name: "dark",
};

/// Close to the common light defaults (Solarized Light / macOS Basic family).
pub const LIGHT: Theme = Theme {
    fg: (0x33, 0x33, 0x33),
    bg: (0xff, 0xff, 0xff),
    name: "light",
};

pub const THEMES: [Theme; 2] = [DARK, LIGHT];

/// The xterm 256-colour cube, enough of it for chrome (the 16 system colours
/// plus a linear approximation of the cube and greys).
fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    const SYSTEM: [(u8, u8, u8); 16] = [
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
    match i {
        0..=15 => SYSTEM[i as usize],
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

fn resolve(c: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match c {
        Color::Default => fallback,
        Color::Indexed(i) => indexed_rgb(i),
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
    let mut fg = resolve(cell.fg, theme.fg);
    let mut bg = resolve(cell.bg, theme.bg);
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

/// Write `frame` to `<dir>/<name>.html`, where `dir` is `$FNO_UX_SHOTS` or, by
/// default, this crate's own `target/ux-shots`. Defaulting rather than requiring
/// the env var is deliberate: `cargo test ux_shot` should leave the pictures on
/// disk without anyone having to know a variable exists. Inside `target/` so the
/// output is gitignored by construction - generated evidence, never committed.
///
/// Best-effort by construction - a failed write returns `None` and the caller
/// carries on, because the assertions are the gate and these files are evidence.
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
    fn worst_contrast_reports_the_losing_theme() {
        // A default-coloured inverse cell is strong on dark and weaker on light,
        // so the worst case must name light. This is the guard that stops a
        // single-theme spot check from certifying chrome.
        let (_, theme) = worst_contrast(&cell(cell_flags::INVERSE | cell_flags::BOLD));
        assert_eq!(theme.name, "light");
    }
}
