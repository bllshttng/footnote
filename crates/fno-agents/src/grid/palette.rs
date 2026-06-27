//! OSC 10/11 terminal-theme palette for grid chrome (E5c AC-4).
//!
//! Three layers:
//!
//! 1. **Pure parse** (`parse_osc_color`) -- unit-tested, no I/O.
//! 2. **Pure palette** (`Palette`) with `fixed()` / `from_terminal()` -- unit-tested.
//! 3. **Thin I/O** (`query_terminal_palette`) -- TTY-gated, bounded-read, never hangs.

use std::io::{self, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

use crate::grid::pane::CellColor;

// ── Layer 1: pure parse ────────────────────────────────────────────────────

/// Parse an OSC 10 or 11 color report.
///
/// Accepts `ESC ] 10 ; rgb:RR/GG/BB BEL` or ST (`ESC \`) terminated.
/// Channels may be 1, 2, or 4 hex digits; 4-digit channels take the high byte.
/// Tolerates a garbage prefix before the `rgb:` token.
///
/// Returns `None` on anything malformed (missing `rgb:`, wrong channel count,
/// non-hex digits, empty input, partial response, etc.).
pub fn parse_osc_color(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let s = std::str::from_utf8(bytes).ok()?;
    // Find the rgb: token anywhere (tolerates garbage / OSC header prefix).
    let pos = s.find("rgb:")?;
    let after = &s[pos + 4..];
    // Strip trailing BEL / ST (ESC \) and ASCII whitespace.
    let after = after.trim_end_matches(|c: char| {
        c == '\x07' || c == '\x1b' || c == '\\' || c.is_ascii_whitespace()
    });
    // Exactly 3 slash-separated channel fields.
    let mut iter = after.splitn(3, '/');
    let r_str = iter.next()?;
    let g_str = iter.next()?;
    let b_str = iter.next()?.trim_end_matches(|c: char| {
        c == '\x07' || c == '\x1b' || c == '\\' || c.is_ascii_whitespace()
    });
    let r = hex_to_u8(r_str)?;
    let g = hex_to_u8(g_str)?;
    let b = hex_to_u8(b_str)?;
    Some((r, g, b))
}

/// Convert a 1-, 2-, or 4-hex-digit string to a u8.
/// - 1 digit: replicate nibble (e.g. `f` → `0xff`).
/// - 2 digits: direct parse (e.g. `ff` → `0xff`).
/// - 4 digits: take high byte (e.g. `ffff` → `0xff`, `1234` → `0x12`).
fn hex_to_u8(s: &str) -> Option<u8> {
    let s = s.trim();
    match s.len() {
        1 => {
            let v = u8::from_str_radix(s, 16).ok()?;
            Some(v << 4 | v)
        }
        2 => u8::from_str_radix(s, 16).ok(),
        4 => {
            let v = u16::from_str_radix(s, 16).ok()?;
            Some((v >> 8) as u8)
        }
        _ => None,
    }
}

// ── Layer 2: pure palette ──────────────────────────────────────────────────

/// Chrome color palette derived from an OSC 10/11 terminal query.
///
/// `Palette::fixed()` returns `CellColor::Default` for every field -- identical
/// to the pre-palette chrome behavior. The no-answer / non-TTY / error path
/// always returns `fixed()`, so the degrade path is a true no-op for the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// Queried terminal foreground (or `Default`).
    pub fg: CellColor,
    /// Queried terminal background (or `Default`).
    pub bg: CellColor,
    /// Box-drawing / border chrome color (~40% fg blended toward bg).
    pub border: CellColor,
    /// Dimmed accent chrome (~25% fg blended toward bg).
    pub dim: CellColor,
}

impl Palette {
    /// The fixed (no-theme) palette: all `CellColor::Default`.
    ///
    /// INVARIANT: this MUST be byte-identical to the pre-palette chrome
    /// behavior.  When the terminal does not answer, this value is returned and
    /// the grid chrome is completely unchanged.
    pub fn fixed() -> Self {
        Palette {
            fg: CellColor::Default,
            bg: CellColor::Default,
            border: CellColor::Default,
            dim: CellColor::Default,
        }
    }

    /// Derive a palette from the terminal's queried fg and bg RGB triples.
    ///
    /// `border` blends 40% fg toward bg (a mid-tone that reads as a border on
    /// both light and dark backgrounds).  `dim` blends 25% fg toward bg.
    pub fn from_terminal(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> Self {
        let border_rgb = blend(fg, bg, 40);
        let dim_rgb = blend(fg, bg, 25);
        Palette {
            fg: CellColor::Rgb(fg.0, fg.1, fg.2),
            bg: CellColor::Rgb(bg.0, bg.1, bg.2),
            border: CellColor::Rgb(border_rgb.0, border_rgb.1, border_rgb.2),
            dim: CellColor::Rgb(dim_rgb.0, dim_rgb.1, dim_rgb.2),
        }
    }
}

/// Blend `fg` toward `bg` by `pct` percent (0 = pure bg, 100 = pure fg).
///
/// Integer arithmetic: each channel = fg_ch * pct / 100 + bg_ch * (100-pct) / 100.
fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), pct: u8) -> (u8, u8, u8) {
    let ch = |f: u8, b: u8| -> u8 {
        (f as u16 * pct as u16 / 100 + b as u16 * (100 - pct as u16) / 100) as u8
    };
    (ch(fg.0, bg.0), ch(fg.1, bg.1), ch(fg.2, bg.2))
}

// ── Layer 3: thin, safe I/O ───────────────────────────────────────────────

/// Query the terminal for its default fg/bg colors via OSC 10 and OSC 11.
///
/// Safety contract:
/// - Returns `Palette::fixed()` immediately when stdout is NOT a TTY.
/// - Writes queries to stderr (where the grid renders) and reads from stdin.
/// - Hard-bounded total wall time: ≤ ~120 ms (100 ms timeout + syscall slack).
/// - On ANY failure (I/O error, timeout, malformed response): `Palette::fixed()`.
/// - NEVER blocks indefinitely.
///
/// **Call after `enable_raw_mode()` and before the `EventStream` loop.**
/// Raw mode makes stdin deliver the response bytes without line-buffering.
pub fn query_terminal_palette() -> Palette {
    // Guard: stdout not a TTY → skip entirely.
    if !io::stdout().is_terminal() {
        return Palette::fixed();
    }
    // ponytail: Palette::fixed() on any inner failure; no partial-palette state.
    query_with_bounded_timeout(Duration::from_millis(100)).unwrap_or_else(Palette::fixed)
}

fn query_with_bounded_timeout(timeout: Duration) -> Option<Palette> {
    // Write both OSC 10 and OSC 11 queries to stderr (the grid's draw surface).
    let mut stderr = io::stderr();
    stderr.write_all(b"\x1b]10;?\x07\x1b]11;?\x07").ok()?;
    stderr.flush().ok()?;

    // Bounded read from stdin (fd 0) via libc poll(2).
    let mut accum: Vec<u8> = Vec::with_capacity(128);
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let ms = remaining.as_millis().min(i32::MAX as u128) as i32;

        #[cfg(unix)]
        {
            // poll(2) on stdin fd 0 with the remaining timeout.
            let mut fds = [libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            }];
            // SAFETY: `fds` is valid, length 1, and `ms` is non-negative.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
            if ready <= 0 {
                break; // timeout or error
            }
            if fds[0].revents & libc::POLLIN == 0 {
                break;
            }
        }
        #[cfg(not(unix))]
        {
            // Non-Unix (Windows): no bounded-read primitive without a new dep.
            // Fall back immediately so the query path is a safe no-op there.
            let _ = ms;
            break;
        }

        let mut tmp = [0u8; 64];
        match io::stdin().read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                accum.extend_from_slice(&tmp[..n]);
                // Stop early once we have both OSC responses.
                if count_osc_terminators(&accum) >= 2 {
                    break;
                }
                // Safety backstop: never buffer more than 1 KiB.
                if accum.len() > 1024 {
                    break;
                }
            }
        }
    }

    parse_two_osc_responses(&accum)
}

/// Count OSC response terminators in `bytes`: BEL (`\x07`) or ST (`ESC \`).
fn count_osc_terminators(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\x07' {
            count += 1;
            i += 1;
        } else if bytes[i] == b'\x1b' && bytes.get(i + 1) == Some(&b'\\') {
            count += 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    count
}

/// Parse a `Palette` from the raw bytes of a combined OSC 10 + OSC 11 response.
///
/// Expects both an fg (OSC 10) and bg (OSC 11) `rgb:` token in order.
/// Returns `None` if either is missing or malformed.
fn parse_two_osc_responses(bytes: &[u8]) -> Option<Palette> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut rgb_offsets = s.match_indices("rgb:").map(|(i, _)| i);
    let fg_pos = rgb_offsets.next()?;
    let bg_pos = rgb_offsets.next()?;
    let fg = parse_osc_color(&bytes[fg_pos..])?;
    let bg = parse_osc_color(&bytes[bg_pos..])?;
    Some(Palette::from_terminal(fg, bg))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Layer 1: parse_osc_color ───────────────────────────────────────────

    #[test]
    fn parse_osc_4digit_bel_terminated() {
        // 4-digit channels: take the high byte (ffff→ff, 0000→00, 8888→88).
        let input = b"\x1b]10;rgb:ffff/0000/8888\x07";
        assert_eq!(parse_osc_color(input), Some((0xff, 0x00, 0x88)));
    }

    #[test]
    fn parse_osc_2digit_bel_terminated() {
        let input = b"\x1b]10;rgb:ff/00/88\x07";
        assert_eq!(parse_osc_color(input), Some((0xff, 0x00, 0x88)));
    }

    #[test]
    fn parse_osc_st_terminated() {
        // ST = ESC \  (0x1b 0x5c)
        let input = b"\x1b]11;rgb:1234/5678/9abc\x1b\\";
        assert_eq!(parse_osc_color(input), Some((0x12, 0x56, 0x9a)));
    }

    #[test]
    fn parse_osc_form_11_accepted() {
        let input = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc_color(input), Some((0xff, 0xff, 0xff)));
    }

    #[test]
    fn parse_osc_garbage_prefix_tolerated() {
        let input = b"junk\x1b]10;rgb:ab/cd/ef\x07";
        assert_eq!(parse_osc_color(input), Some((0xab, 0xcd, 0xef)));
    }

    #[test]
    fn parse_osc_empty_returns_none() {
        assert_eq!(parse_osc_color(b""), None);
    }

    #[test]
    fn parse_osc_partial_returns_none() {
        // Missing the third channel.
        assert_eq!(parse_osc_color(b"\x1b]10;rgb:ff/00"), None);
    }

    #[test]
    fn parse_osc_malformed_no_rgb_token() {
        assert_eq!(parse_osc_color(b"\x1b]10;norgb\x07"), None);
    }

    // ── Layer 2: Palette ──────────────────────────────────────────────────

    #[test]
    fn palette_fixed_all_default() {
        let p = Palette::fixed();
        assert_eq!(p.fg, CellColor::Default, "fixed fg must be Default");
        assert_eq!(p.bg, CellColor::Default, "fixed bg must be Default");
        assert_eq!(p.border, CellColor::Default, "fixed border must be Default");
        assert_eq!(p.dim, CellColor::Default, "fixed dim must be Default");
    }

    #[test]
    fn palette_from_terminal_uses_queried_fg_bg() {
        let p = Palette::from_terminal((200, 200, 200), (20, 20, 20));
        assert_eq!(p.fg, CellColor::Rgb(200, 200, 200));
        assert_eq!(p.bg, CellColor::Rgb(20, 20, 20));
    }

    #[test]
    fn palette_from_terminal_border_distinct_from_bg() {
        let p = Palette::from_terminal((200, 200, 200), (20, 20, 20));
        assert_ne!(p.border, p.bg, "border must differ from bg when fg != bg");
    }

    #[test]
    fn palette_from_terminal_border_blend_math() {
        // Lock the blend math:
        // border = blend(fg, bg, 40%), fg=(200,200,200), bg=(0,0,0)
        // ch = 200 * 40 / 100 + 0 * 60 / 100 = 80
        let p = Palette::from_terminal((200, 200, 200), (0, 0, 0));
        assert_eq!(
            p.border,
            CellColor::Rgb(80, 80, 80),
            "border blend: 40% of 200 toward 0 = 80"
        );
    }

    #[test]
    fn palette_from_terminal_dim_blend_math() {
        // dim = blend(fg, bg, 25%), fg=(200,200,200), bg=(0,0,0)
        // ch = 200 * 25 / 100 + 0 * 75 / 100 = 50
        let p = Palette::from_terminal((200, 200, 200), (0, 0, 0));
        assert_eq!(
            p.dim,
            CellColor::Rgb(50, 50, 50),
            "dim blend: 25% of 200 toward 0 = 50"
        );
    }
}
