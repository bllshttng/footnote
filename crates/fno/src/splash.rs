//! The launch splash: the animated `Ｆ[no]` mark the mux client draws when it
//! starts an interactive session. One pixel = two terminal cells wide, one
//! row tall. Theme colors only: the solid ink is the terminal's own text
//! color, knocked-out cells clear to the base, and the footer's muted slots
//! ride the theme's `dim`. A keypress skips to the last frame; not a TTY,
//! CI, or reduced motion prints the last frame only, once.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{Color as CtColor, Print, SetForegroundColor};
use crossterm::terminal;

use crate::proto::Color;
use crate::theme::Theme;

/// F glyph, 4x6 px (`X` = filled with the text color).
const F_GLYPH: [&str; 6] = ["XXXX", "X...", "XXX.", "X...", "X...", "X..."];

/// Knock-out glyphs, 6 rows; `#` stays solid stamp, `.` is knocked out to
/// the base color.
const G_N: [&str; 6] = ["...", ".#.", ".#.", ".#.", ".#.", ".#."];
const G_O: [&str; 6] = ["...", ".#.", ".#.", ".#.", ".#.", "..."];
const G_LBRACK: [&str; 6] = ["..", ".#", ".#", ".#", ".#", ".."];
const G_RBRACK: [&str; 6] = ["..", "#.", "#.", "#.", "#.", ".."];

/// The gap between the F and the stamp: 1.5 px = 3 cells.
const GAP_CELLS: u16 = 3;
const STAMP_ROWS: usize = 8;
/// Final art: F (4 px) + gap (1.5 px) + stamp (17 px) = 22.5 px = 45 cells,
/// 8 block rows plus the footer row.
const ART_CELLS: u16 = 8 + GAP_CELLS + 17 * 2;
const ART_ROWS: u16 = STAMP_ROWS as u16 + 1;
/// Read window after the last frame lands, so the finished mark is seen
/// before the first UI paint takes over. Still skippable.
const FINAL_HOLD: Duration = Duration::from_millis(500);

/// One splash pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Px {
    Solid,
    Knock,
}

/// What a frame shows: `F alone` grows the stamp 9 -> 17 px (left edge
/// fixed, `no` centered), the brackets knock out last, then the footer
/// prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    /// Stamp width in px; `None` = the F alone.
    stamp_px: Option<usize>,
    brackets: bool,
    footer: bool,
}

const FINAL: Frame = Frame {
    stamp_px: Some(17),
    brackets: true,
    footer: true,
};

/// The frame on screen at `t` seconds (the brand study's beat sheet).
fn frame_at(t: f64) -> Frame {
    let stamp_px = if t < 0.60 {
        None
    } else if t < 1.00 {
        Some(9)
    } else if t < 1.17 {
        Some(11)
    } else if t < 1.33 {
        Some(13)
    } else if t < 1.50 {
        Some(15)
    } else {
        Some(17)
    };
    Frame {
        stamp_px,
        brackets: t >= 2.25,
        footer: t >= 2.25,
    }
}

/// The animation's beat sheet: (cumulative seconds, frame).
fn schedule() -> Vec<(f64, Frame)> {
    [0.00, 0.60, 1.00, 1.17, 1.33, 1.50, 2.25]
        .into_iter()
        .map(|t| (t, frame_at(t)))
        .collect()
}

fn glyph_row(g: &[&str; 6], r: usize) -> Vec<Px> {
    g[r].chars()
        .map(|c| if c == '#' { Px::Solid } else { Px::Knock })
        .collect()
}

/// One stamp row at `block_row` (0..8): rows 0 and 7 are the solid band
/// above/below the F; 1..=6 carry the glyph grids.
fn stamp_row(width: usize, block_row: usize, brackets: bool) -> Vec<Px> {
    if block_row == 0 || block_row == STAMP_ROWS - 1 {
        return vec![Px::Solid; width];
    }
    let g = block_row - 1;
    if brackets {
        // '##' + [ + '#' + n + '#' + o + '#' + ] + '##' = 17 px.
        let mut px = vec![Px::Solid; 2];
        px.extend(glyph_row(&G_LBRACK, g));
        px.push(Px::Solid);
        px.extend(glyph_row(&G_N, g));
        px.push(Px::Solid);
        px.extend(glyph_row(&G_O, g));
        px.push(Px::Solid);
        px.extend(glyph_row(&G_RBRACK, g));
        px.extend(vec![Px::Solid; 2]);
        px
    } else {
        // Pads hold the inner 7 px (n + separator + o) centered.
        let inner: Vec<Px> = glyph_row(&G_N, g)
            .into_iter()
            .chain(std::iter::once(Px::Solid))
            .chain(glyph_row(&G_O, g))
            .collect();
        let mut px = vec![Px::Solid; (width - inner.len()) / 2];
        px.extend(inner);
        px.resize(width, Px::Solid);
        px
    }
}

/// A run of same-styled cells on one visual row; `fg: None` = the
/// terminal's own text color.
struct Seg {
    text: String,
    fg: Option<CtColor>,
}

fn seg(text: &str, fg: Option<CtColor>) -> Seg {
    Seg {
        text: text.to_string(),
        fg,
    }
}

/// The F's cells, pre-joined: one 4-px row per block row, 2 cells per px.
fn f_cells(block_row: usize) -> String {
    if block_row == 0 || block_row == STAMP_ROWS - 1 {
        return " ".repeat(8);
    }
    F_GLYPH[block_row - 1]
        .chars()
        .map(|c| if c == 'X' { "██" } else { "  " })
        .collect()
}

fn px_cells(px: &[Px]) -> String {
    let mut out = String::with_capacity(px.len() * 2);
    for p in px {
        out.push_str(match p {
            Px::Solid => "██",
            Px::Knock => "  ",
        });
    }
    out
}

fn footer_segs(version: &str, dim: CtColor) -> Vec<Seg> {
    // The version is the one variable-length part; clamp it so the footer
    // always fits the art width and can never wrap on a minimum terminal.
    const FIXED: usize = "footnote".len() + 3 + "fno ".len() + 3 + "idea to shipped PR".len();
    let budget = (ART_CELLS as usize).saturating_sub(FIXED);
    let version: String = version.chars().take(budget.max(1)).collect();
    // Words in the text color, connectors and version in overlay1.
    let parts: [(&str, Option<CtColor>); 6] = [
        ("footnote", None),
        (" · ", Some(dim)),
        ("fno ", Some(dim)),
        (&version, Some(dim)),
        (" · ", Some(dim)),
        ("idea to shipped PR", None),
    ];
    let total: usize = parts.iter().map(|(s, _)| s.chars().count()).sum();
    let lead = (ART_CELLS as usize).saturating_sub(total) / 2;
    let mut segs = vec![seg(&" ".repeat(lead), None)];
    for (text, fg) in parts {
        segs.push(seg(text, fg));
    }
    // The row repaints in place, so it carries the full 45-cell width.
    let painted = lead + total;
    segs.push(seg(
        &" ".repeat(ART_CELLS as usize - painted.min(ART_CELLS as usize)),
        None,
    ));
    segs
}

/// The frame's visual rows: 8 block rows (F + gap + stamp) plus the footer
/// on the final frame.
fn visual_rows(frame: Frame, version: &str, dim: CtColor) -> Vec<Vec<Seg>> {
    let mut rows = Vec::with_capacity(ART_ROWS as usize);
    for r in 0..STAMP_ROWS {
        let mut segs = vec![
            seg(&f_cells(r), None),
            seg(&" ".repeat(GAP_CELLS as usize), None),
        ];
        if let Some(w) = frame.stamp_px {
            segs.push(seg(&px_cells(&stamp_row(w, r, frame.brackets)), None));
        }
        rows.push(segs);
    }
    if frame.footer {
        rows.push(footer_segs(version, dim));
    }
    rows
}

/// Draw origin, centered on the FINAL art so the stamp's left edge never
/// moves as the stamp widens. `None` when the terminal is too small.
fn origin_for(cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols < ART_CELLS || rows < ART_ROWS {
        return None;
    }
    Some(((cols - ART_CELLS) / 2, (rows - ART_ROWS) / 2))
}

fn to_ct(c: Color) -> CtColor {
    match c {
        Color::Default => CtColor::Reset,
        Color::Indexed(i) => CtColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}

fn draw(frame: Frame, theme: &Theme) {
    let Some((origin_col, origin_row)) = terminal::size()
        .ok()
        .and_then(|(cols, rows)| origin_for(cols, rows))
    else {
        return;
    };
    let dim = to_ct(theme.dim);
    let mut out = std::io::stdout().lock();
    for (i, segs) in visual_rows(frame, crate::proto::BUILD_VERSION, dim)
        .into_iter()
        .enumerate()
    {
        let _ = queue!(out, MoveTo(origin_col, origin_row + i as u16));
        for s in segs {
            if let Some(fg) = s.fg {
                let _ = queue!(out, SetForegroundColor(fg));
            }
            let _ = queue!(out, Print(&s.text));
        }
        let _ = queue!(out, SetForegroundColor(CtColor::Reset));
    }
    let _ = out.flush();
}

/// Whether the animation may run at all: a TTY, no CI, no reduced-motion
/// request. Everything else prints the last frame only, once.
fn animated_env(is_tty: bool, ci: Option<&str>, reduced: Option<&str>) -> bool {
    is_tty
        && ci.map_or(true, str::is_empty)
        && !reduced.map_or(false, |v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
}

fn animated() -> bool {
    animated_env(
        std::io::stdout().is_terminal(),
        std::env::var("CI").ok().as_deref(),
        std::env::var("REDUCED_MOTION").ok().as_deref(),
    )
}

/// Draw the splash before the first UI paint. `rx` is the raw stdin
/// channel: any byte skips to the last frame.
pub async fn run(rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>, theme: &Theme) {
    let fits = terminal::size()
        .ok()
        .and_then(|(cols, rows)| origin_for(cols, rows))
        .is_some();
    if !fits {
        // Nowhere to draw the art; do not spend the beat sheet on nothing.
        return;
    }
    if !animated() {
        draw(FINAL, theme);
        return;
    }
    let start = Instant::now();
    for (i, (at, frame)) in schedule().into_iter().enumerate() {
        if i > 0 {
            let wait = Duration::from_secs_f64(at).saturating_sub(start.elapsed());
            match tokio::time::timeout(wait, rx.recv()).await {
                // A keypress (or stdin closing) skips to the last frame.
                Ok(Some(_)) | Ok(None) => {
                    if frame != FINAL {
                        draw(FINAL, theme);
                    }
                    return;
                }
                Err(_elapsed) => {}
            }
        }
        draw(frame, theme);
    }
    // Hold the finished mark briefly so the last beat reads. Plain sleep:
    // consuming a keystroke here would eat the user's first input.
    tokio::time::sleep(FINAL_HOLD).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_timeline_matches_the_beat_sheet() {
        assert_eq!(frame_at(0.0).stamp_px, None);
        assert_eq!(frame_at(0.59).stamp_px, None);
        assert_eq!(
            frame_at(0.60),
            Frame {
                stamp_px: Some(9),
                brackets: false,
                footer: false
            }
        );
        assert_eq!(frame_at(0.99).stamp_px, Some(9));
        assert_eq!(frame_at(1.00).stamp_px, Some(11));
        assert_eq!(frame_at(1.16).stamp_px, Some(11));
        assert_eq!(frame_at(1.17).stamp_px, Some(13));
        assert_eq!(frame_at(1.33).stamp_px, Some(15));
        assert_eq!(frame_at(1.49).stamp_px, Some(15));
        assert_eq!(
            frame_at(1.50),
            Frame {
                stamp_px: Some(17),
                brackets: false,
                footer: false
            }
        );
        assert_eq!(
            frame_at(2.24),
            Frame {
                stamp_px: Some(17),
                brackets: false,
                footer: false
            }
        );
        assert_eq!(frame_at(2.25), FINAL);
        assert_eq!(frame_at(9.9), FINAL);
    }

    #[test]
    fn growing_stamp_centers_no_and_keeps_the_row_solid() {
        let solid = |w: usize| vec![Px::Solid; w];
        // The pad rows above and below the F are solid at every width.
        for w in [9, 11, 13, 15, 17] {
            assert_eq!(stamp_row(w, 0, false), solid(w));
            assert_eq!(stamp_row(w, 7, false), solid(w));
        }
        // Glyph band row 0 of the grids is all knocked out except the
        // separators: pad + n="..." + '#' + o="..." + pad.
        assert_eq!(
            stamp_row(9, 1, false),
            [
                Px::Solid,
                Px::Knock,
                Px::Knock,
                Px::Knock,
                Px::Solid,
                Px::Knock,
                Px::Knock,
                Px::Knock,
                Px::Solid
            ],
        );
        // 9 px = 1 px pad each side of the 7 px inner mark.
        assert_eq!(stamp_row(9, 2, false).len(), 9);
        assert_eq!(stamp_row(15, 4, false).len(), 15);
        assert_eq!(stamp_row(15, 4, false)[0], Px::Solid);
        assert_eq!(stamp_row(15, 4, false)[3], Px::Solid);
        assert_eq!(stamp_row(15, 4, false)[7], Px::Solid);
    }

    #[test]
    fn final_stamp_row_is_the_17px_mark() {
        let row = stamp_row(17, 3, true);
        assert_eq!(row.len(), 17);
        // '##' caps.
        assert_eq!(&row[..2], &[Px::Solid, Px::Solid]);
        assert_eq!(&row[15..], &[Px::Solid, Px::Solid]);
        // Grid row 3: [=".#", n=".#.", o=".#.", ]="#." with solid separators
        // between: S S | K S | S | K S K | S | K S K | S | S K | S S.
        assert_eq!(
            row,
            [
                Px::Solid,
                Px::Solid,
                Px::Knock,
                Px::Solid,
                Px::Solid,
                Px::Knock,
                Px::Solid,
                Px::Knock,
                Px::Solid,
                Px::Knock,
                Px::Solid,
                Px::Knock,
                Px::Solid,
                Px::Solid,
                Px::Knock,
                Px::Solid,
                Px::Solid,
            ],
        );
    }

    #[test]
    fn f_cells_draw_the_four_by_six_glyph() {
        assert_eq!(f_cells(0), " ".repeat(8));
        assert_eq!(f_cells(1), "████████");
        assert_eq!(f_cells(2), "██      ");
        assert_eq!(f_cells(3), "██████  ");
        assert_eq!(f_cells(4), "██      ");
        assert_eq!(f_cells(6), "██      ");
        assert_eq!(f_cells(7), " ".repeat(8));
    }

    #[test]
    fn visual_rows_match_the_art_geometry() {
        let dim = CtColor::Reset;
        // F alone: 8 rows, 11 cells (F + gap).
        let alone = visual_rows(frame_at(0.0), "0.0.0", dim);
        assert_eq!(alone.len(), 8);
        for row in &alone {
            assert_eq!(
                row.iter().map(|s| s.text.chars().count()).sum::<usize>(),
                11
            );
        }
        // Final: 9 rows, 45 cells each; footer names the product and version.
        let fin = visual_rows(FINAL, "9.9.9", dim);
        assert_eq!(fin.len(), ART_ROWS as usize);
        for row in &fin {
            assert_eq!(
                row.iter().map(|s| s.text.chars().count()).sum::<usize>(),
                45
            );
        }
        let footer: String = fin[8].iter().map(|s| s.text.as_str()).collect();
        assert!(footer.contains("footnote"));
        assert!(footer.contains("fno 9.9.9"));
        assert!(footer.contains("idea to shipped PR"));
        assert!(footer.trim_start().starts_with("footnote"));
    }

    #[test]
    fn a_long_version_never_overflows_the_footer_row() {
        let fin = visual_rows(FINAL, "10.100.100-beta.7+x", CtColor::Reset);
        let width: usize = fin[8].iter().map(|s| s.text.chars().count()).sum();
        assert_eq!(width, ART_CELLS as usize);
    }

    #[test]
    fn origin_centers_the_final_art_and_refuses_tiny_terms() {
        assert_eq!(origin_for(100, 40), Some((27, 15)));
        assert_eq!(origin_for(45, 9), Some((0, 0)));
        assert_eq!(origin_for(44, 9), None);
        assert_eq!(origin_for(100, 8), None);
    }

    #[test]
    fn animation_gates() {
        assert!(animated_env(true, None, None));
        assert!(animated_env(true, Some(""), None));
        assert!(!animated_env(false, None, None));
        assert!(!animated_env(true, Some("1"), None));
        assert!(!animated_env(true, Some("true"), None));
        assert!(!animated_env(true, None, Some("1")));
        assert!(!animated_env(true, None, Some("yes")));
        assert!(animated_env(true, None, Some("0")));
        assert!(animated_env(true, None, Some("")));
    }
}
