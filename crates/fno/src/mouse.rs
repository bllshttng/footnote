//! Client-side mouse: parse the SGR (1006) mouse reports the outer terminal
//! sends while reporting is on, so `handle_stdin` can hit-test them against the
//! layout and forward pane-rect events for server-side routing (brief Locked
//! 2). Only SGR is enabled and parsed (Domain: legacy X10 truncates at column
//! 223). Everything here is pure and unit-tested; enabling/disabling reporting
//! on the terminal and the hit-test live in `client.rs`.

use crate::proto::{MouseButton, MouseKind};

/// The escape that enables SGR mouse reporting: 1002 (button press/release +
/// drag motion) and 1003 (any-motion tracking, so a bare pointer move with no
/// button held is reported for hover - focus-follows-mouse + sideline highlight)
/// with the 1006 extended encoding. The client keeps this on for its whole
/// lifetime; `client::MODE_RESET` (which lists 1003l) turns it back off on exit.
/// 1003 reports every cell the pointer crosses; the client's hover debounce is
/// what makes that flood safe.
pub const ENABLE: &[u8] = b"\x1b[?1002h\x1b[?1003h\x1b[?1006h";

/// The 3-byte SGR mouse prefix `ESC [ <`. Unambiguous - nothing else the
/// terminal sends begins with it - so a trailing partial can be carried across
/// reads without disturbing the key scanner's paste/arrow prefixes.
const PREFIX: &[u8] = b"\x1b[<";

/// A parsed SGR mouse report in OUTER-terminal coordinates (0-based), plus the
/// Shift modifier. Shift is the native-selection escape hatch (AC3-EDGE): the
/// client drops shifted events rather than intercept them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseReport {
    pub row: u16,
    pub col: u16,
    pub kind: MouseKind,
    pub shift: bool,
}

/// Pull complete SGR mouse reports out of a stdin chunk, returning them plus the
/// passthrough bytes (everything that is NOT a mouse report) for the key
/// scanner. `carry` holds a trailing partial `ESC[<...` across reads; only that
/// unambiguous 3+ byte prefix is buffered, so a report fragmented before its
/// third byte falls through to the scanner (a documented cosmetic residual -
/// terminals emit mouse reports atomically in practice).
pub fn extract_mouse(carry: &mut Vec<u8>, chunk: &[u8]) -> (Vec<MouseReport>, Vec<u8>) {
    let mut buf = std::mem::take(carry);
    buf.extend_from_slice(chunk);
    let mut reports = Vec::new();
    let mut pass = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let tail = &buf[i..];
        let full_prefix = tail.len() >= 3 && tail[..3] == *PREFIX;
        if full_prefix {
            match parse_sgr(tail) {
                Sgr::Complete(rep, len) => {
                    reports.push(rep);
                    i += len;
                }
                // Body not finished: carry the whole ESC[<... tail for next read.
                Sgr::Partial => {
                    carry.extend_from_slice(tail);
                    return (reports, pass);
                }
                // Not a real report after all: forward the ESC and resync at i+1.
                Sgr::Malformed => {
                    pass.push(buf[i]);
                    i += 1;
                }
            }
        } else {
            // A trailing 1- or 2-byte partial (`ESC` or `ESC[`) is ALSO a paste/
            // arrow prefix, so it must go to the scanner, not be carried here.
            pass.push(buf[i]);
            i += 1;
        }
    }
    (reports, pass)
}

enum Sgr {
    /// A report and the total bytes consumed (prefix + body + terminator).
    Complete(MouseReport, usize),
    /// The terminator has not arrived yet - carry and wait.
    Partial,
    /// Matched the prefix but the body is not a valid report - resync.
    Malformed,
}

/// Parse one SGR report from a slice that starts with `ESC [ <`. Body form:
/// `Cb ; Cx ; Cy (M|m)` - decimal button/coords, `M` press/motion, `m` release.
fn parse_sgr(s: &[u8]) -> Sgr {
    let body = &s[3..];
    let mut end = None;
    for (k, &b) in body.iter().enumerate() {
        if b == b'M' || b == b'm' {
            end = Some(k);
            break;
        }
        if !(b.is_ascii_digit() || b == b';') || k > 24 {
            return Sgr::Malformed; // runaway or junk: not a mouse report
        }
    }
    let Some(end) = end else { return Sgr::Partial };
    let released = body[end] == b'm';
    let parts: Vec<&[u8]> = body[..end].split(|&b| b == b';').collect();
    if parts.len() != 3 {
        return Sgr::Malformed;
    }
    let nums: Option<Vec<u32>> = parts
        .iter()
        .map(|p| std::str::from_utf8(p).ok()?.parse().ok())
        .collect();
    let Some(nums) = nums else {
        return Sgr::Malformed;
    };
    let (cb, cx, cy) = (nums[0], nums[1], nums[2]);
    let shift = cb & 4 != 0;
    let kind = if cb & 64 != 0 {
        // Wheel: low bit picks direction (0 up, 1 down).
        if cb & 1 == 0 {
            MouseKind::WheelUp
        } else {
            MouseKind::WheelDown
        }
    } else if cb & 32 != 0 && cb & 3 == 3 {
        // Motion bit set with the no-button code (cb=35 + modifiers): 1003
        // any-motion tracking with nothing held. This is hover, not a drag.
        MouseKind::Move
    } else {
        let button = match cb & 3 {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => return Sgr::Malformed, // 3 = no-button code, only valid with motion (Move above)
        };
        if cb & 32 != 0 {
            MouseKind::Drag(button)
        } else if released {
            MouseKind::Release(button)
        } else {
            MouseKind::Press(button)
        }
    };
    // Coords are 1-based; convert to 0-based (a stray 0 clamps to 0).
    let col = cx.saturating_sub(1).min(u16::MAX as u32) as u16;
    let row = cy.saturating_sub(1).min(u16::MAX as u32) as u16;
    Sgr::Complete(
        MouseReport {
            row,
            col,
            kind,
            shift,
        },
        3 + end + 1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(bytes: &[u8]) -> (Vec<MouseReport>, Vec<u8>) {
        let mut carry = Vec::new();
        extract_mouse(&mut carry, bytes)
    }

    #[test]
    fn parses_left_press_release_drag_and_wheel() {
        // ESC[<0;10;5M = left press at col 10,row 5 (1-based) -> 9,4 (0-based).
        let (r, pass) = one(b"\x1b[<0;10;5M");
        assert!(pass.is_empty());
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0],
            MouseReport {
                row: 4,
                col: 9,
                kind: MouseKind::Press(MouseButton::Left),
                shift: false
            }
        );
        assert_eq!(
            one(b"\x1b[<0;10;5m").0[0].kind,
            MouseKind::Release(MouseButton::Left)
        );
        assert_eq!(
            one(b"\x1b[<32;3;3M").0[0].kind,
            MouseKind::Drag(MouseButton::Left)
        );
        assert_eq!(one(b"\x1b[<64;1;1M").0[0].kind, MouseKind::WheelUp);
        assert_eq!(one(b"\x1b[<65;1;1M").0[0].kind, MouseKind::WheelDown);
    }

    #[test]
    fn no_button_motion_parses_as_move() {
        // cb = 32 (motion) | 3 (no-button code) = 35: 1003 any-motion hover.
        // Distinct from a drag (cb=32|button, e.g. 32 parsed above as Drag(Left)).
        let (r, _) = one(b"\x1b[<35;10;5M");
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0],
            MouseReport {
                row: 4,
                col: 9,
                kind: MouseKind::Move,
                shift: false
            }
        );
        // A shifted move still decodes (cb = 35 | 4 = 39) - the client drops it.
        assert_eq!(one(b"\x1b[<39;2;2M").0[0].kind, MouseKind::Move);
    }

    #[test]
    fn shift_modifier_is_flagged() {
        // cb = 0 (left) | 4 (shift) = 4.
        let (r, _) = one(b"\x1b[<4;2;2M");
        assert!(r[0].shift, "shift bit decoded");
    }

    #[test]
    fn non_mouse_bytes_pass_through_untouched() {
        let (r, pass) = one(b"echo hi\r");
        assert!(r.is_empty());
        assert_eq!(pass, b"echo hi\r");
    }

    #[test]
    fn mouse_interleaved_with_typed_bytes_splits_cleanly() {
        let (r, pass) = one(b"ab\x1b[<0;5;5Mcd");
        assert_eq!(r.len(), 1);
        assert_eq!(pass, b"abcd");
    }

    #[test]
    fn arrow_escape_is_not_mistaken_for_mouse() {
        // ESC[C (right arrow) shares ESC[ but not the `<`: it must pass through
        // so the key scanner can handle it.
        let (r, pass) = one(b"\x1b[C");
        assert!(r.is_empty());
        assert_eq!(pass, b"\x1b[C");
    }

    #[test]
    fn partial_report_carries_across_reads() {
        let mut carry = Vec::new();
        let (r1, p1) = extract_mouse(&mut carry, b"\x1b[<0;10;");
        assert!(r1.is_empty() && p1.is_empty(), "held for the terminator");
        assert!(!carry.is_empty(), "tail carried");
        let (r2, p2) = extract_mouse(&mut carry, b"5M");
        assert_eq!(r2.len(), 1);
        assert!(p2.is_empty());
        assert!(carry.is_empty(), "carry drained");
    }

    #[test]
    fn two_byte_prefix_split_is_not_carried_leaving_scanner_untouched() {
        // A chunk ending in `ESC[` must forward (paste/arrow prefixes belong to
        // the key scanner); only the full ESC[< carries.
        let mut carry = Vec::new();
        let (r, pass) = extract_mouse(&mut carry, b"\x1b[");
        assert!(r.is_empty());
        assert_eq!(pass, b"\x1b[");
        assert!(carry.is_empty(), "ambiguous 2-byte prefix not carried");
    }
}
