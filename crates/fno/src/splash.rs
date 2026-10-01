//! The launch splash: the animated `Ｆ[no]` mark the mux client draws when it
//! starts an interactive session. One pixel = two terminal cells wide, one
//! row tall. Theme colors only: the solid ink is the terminal's own text
//! color, knocked-out cells clear to the base, and the footer's muted slots
//! ride the theme's `dim`. A keypress skips to the last frame; not a TTY,
//! CI, or reduced motion prints the last frame only, once.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crossterm::cursor::{MoveToColumn, MoveUp};
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
        // The bracket layout is drawn at 17 px; any other width pads with
        // KNOCK (never Solid) so a row can never overhang its neighbors -
        // the bottom-right nub the operator screenshotted.
        if px.len() > width {
            px.truncate(width);
        } else {
            px.resize(width, Px::Knock);
        }
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
    // stays near the art width. The longer tagline leaves less room, so the
    // floor keeps a full semver core ("0.10.0") on the tightest fit and only
    // the widest footers may wrap on a minimum terminal.
    const FIXED: usize = "footnote".len() + 3 + "fno ".len() + 3 + "say f[no] to mostly done".len();
    let budget = (ART_CELLS as usize).saturating_sub(FIXED).max(6);
    let version: String = version.chars().take(budget).collect();
    // Words in the text color, connectors and version in overlay1.
    let parts: [(&str, Option<CtColor>); 6] = [
        ("footnote", None),
        (" · ", Some(dim)),
        ("fno ", Some(dim)),
        (&version, Some(dim)),
        (" · ", Some(dim)),
        ("say f[no] to mostly done", None),
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

/// The inline banner needs the art's width so rows never wrap; height is
/// free on the normal screen (it scrolls).
fn art_fits() -> bool {
    matches!(terminal::size(), Ok((cols, _)) if cols >= ART_CELLS)
}

fn to_ct(c: Color) -> CtColor {
    match c {
        Color::Default => CtColor::Reset,
        Color::Indexed(i) => CtColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}

fn draw(frame: Frame, theme: &Theme) {
    let dim = to_ct(theme.dim);
    let mut out = std::io::stdout().lock();
    for (i, segs) in visual_rows(frame, crate::proto::BUILD_VERSION, dim)
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            let _ = queue!(out, Print("\r\n"));
        }
        let _ = queue!(out, MoveToColumn(0));
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

/// Walk the cursor back to the top row of the previously printed block, so
/// the next frame repaints in place. `None` = first frame, nothing to undo.
fn rewind(last_rows: Option<usize>) {
    let Some(n) = last_rows else { return };
    let mut out = std::io::stdout().lock();
    let _ = queue!(out, MoveToColumn(0));
    let _ = queue!(out, MoveUp((n - 1) as u16));
    let _ = out.flush();
}

/// Whether the animation may run at all: a TTY, no CI, no reduced-motion
/// request. Everything else prints the last frame only, once.
fn animated_env(is_tty: bool, ci: Option<&str>, reduced: Option<&str>) -> bool {
    is_tty
        && ci.map_or(true, str::is_empty)
        && !reduced.map_or(false, |v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
}

pub(crate) fn animated() -> bool {
    animated_env(
        std::io::stdout().is_terminal(),
        std::env::var("CI").ok().as_deref(),
        std::env::var("REDUCED_MOTION").ok().as_deref(),
    )
}

/// The terminal's block cursor parked after the last written cell reads as
/// a dark toe past the art. Hide it for the whole animation; this guard
/// reveals it again on every exit path, skip and early return included.
struct Reveal;
impl Drop for Reveal {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?25h");
        let _ = out.flush();
    }
}

/// Draw the splash before the first UI paint. `rx` is the raw stdin
/// channel: any byte skips to the last frame. The chunk that ended the
/// animation is handed back through `tx` WHOLE, so typed-ahead input and
/// multi-byte escape sequences reach the UI exactly as they would have
/// without the splash.
pub async fn run(
    rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    theme: &Theme,
) {
    if !art_fits() {
        // Nowhere to draw the art without wrapping; do not spend the beat
        // sheet on nothing.
        return;
    }
    let mut out = std::io::stdout();
    // Hide the block cursor for the animation; Reveal undoes it on every
    // exit path. SIGTERM and the client's own Drop both show it separately.
    let _reveal = Reveal;
    let _ = out.write_all(b"\x1b[?25l");
    // Start on a fresh line below the shell prompt, so the banner paints on
    // open ground and ends up in scrollback verbatim.
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
    if !animated() {
        draw(FINAL, theme);
        let _ = out.write_all(b"\r\n");
        let _ = out.flush();
        return;
    }
    let mut last_rows: Option<usize> = None;
    let start = Instant::now();
    for (at, frame) in schedule() {
        let wait = Duration::from_secs_f64(at).saturating_sub(start.elapsed());
        if wait > Duration::ZERO {
            match tokio::time::timeout(wait, rx.recv()).await {
                // A keypress (or stdin closing) skips to the last frame.
                Ok(Some(chunk)) => {
                    rewind(last_rows);
                    draw(FINAL, theme);
                    let _ = out.write_all(b"\r\n");
                    let _ = out.flush();
                    let _ = tx.send(chunk).await;
                    return;
                }
                Ok(None) => {
                    rewind(last_rows);
                    draw(FINAL, theme);
                    let _ = out.write_all(b"\r\n");
                    let _ = out.flush();
                    return;
                }
                Err(_elapsed) => {}
            }
        }
        rewind(last_rows);
        // The cursor ends ON the block's last row: rewind walks back
        // printed-1 lines, so last_rows counts what draw actually prints
        // (STAMP_ROWS art rows, plus the footer row when the beat has one),
        // not ART_ROWS.
        last_rows = Some(STAMP_ROWS + frame.footer as usize);
        draw(frame, theme);
    }
    // Hold the finished mark briefly so the last beat reads. Plain sleep:
    // consuming a keystroke here would eat the user's first input.
    tokio::time::sleep(FINAL_HOLD).await;
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
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
        // Final: 9 rows; the 8 art rows are 45 cells. The footer row runs a
        // couple of cells wider now that the tagline carries the README
        // wording: 8 + 3 + 4 + version + 3 + 24.
        let fin = visual_rows(FINAL, "9.9.9", dim);
        assert_eq!(fin.len(), ART_ROWS as usize);
        for row in fin.iter().take(8) {
            assert_eq!(
                row.iter().map(|s| s.text.chars().count()).sum::<usize>(),
                45
            );
        }
        assert_eq!(
            fin[8].iter().map(|s| s.text.chars().count()).sum::<usize>(),
            47
        );
        let footer: String = fin[8].iter().map(|s| s.text.as_str()).collect();
        assert!(footer.contains("footnote"));
        assert!(footer.contains("fno 9.9.9"));
        assert!(footer.contains("say f[no] to mostly done"));
        assert!(footer.trim_start().starts_with("footnote"));
    }

    #[test]
    fn a_long_version_never_overflows_the_footer_row() {
        let fin = visual_rows(FINAL, "10.100.100-beta.7+x", CtColor::Reset);
        let width: usize = fin[8].iter().map(|s| s.text.chars().count()).sum();
        // The clamp floor keeps six version chars; the fixed parts around it
        // are 42 cells, so 48 is the widest the footer can ever run.
        assert_eq!(width, 48);
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

    #[test]
    fn every_stamp_row_paints_equal_width() {
        // The user's screenshot: the bottom stamp row rendered one px wider
        // than the rows above, a nub at the stamp's bottom-right. Every
        // stamp row must carry exactly `width` px in cells.
        for brackets in [false, true] {
            for w in [9usize, 11, 13, 15, 17] {
                let rows: Vec<Vec<Px>> =
                    (0..STAMP_ROWS).map(|r| stamp_row(w, r, brackets)).collect();
                for (r, row) in rows.iter().enumerate() {
                    assert_eq!(row.len(), w, "stamp row {r} at width {w}");
                    let cells = px_cells(row);
                    assert_eq!(
                        cells.chars().count(),
                        2 * w,
                        "stamp row {r} cells at width {w}"
                    );
                }
            }
        }
        // And the composed visual rows of the FINAL frame agree: the F
        // column (8) + gap (3) + stamp (34) on every line.
        let rows = visual_rows(
            FINAL,
            "9.9",
            to_ct(crate::theme::Theme::from_name("footnote-superscript").0.dim),
        );
        let widths: Vec<usize> = rows
            .iter()
            .map(|segs| segs.iter().map(|s| s.text.chars().count()).sum())
            .collect();
        for (i, w) in widths.iter().enumerate() {
            assert_eq!(w, &(11 + 2 * 17), "row {i} width");
        }
    }
}
