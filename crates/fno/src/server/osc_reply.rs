//! The pane's terminal-query answers. A client (claude's `auto` theme, since
//! 2.1.284) probes the palette with OSC 10/11/4 `?` queries, and a pane
//! nobody views has no terminal to answer: the query falls into the mux's
//! emulator and dies, and the client falls back to dark on a light ground.
//! The mux answers from the ACTIVE THEME, so `auto` resolves to the ground
//! the mux actually paints.

use crate::proto::Color;
use crate::theme::{terminal16_slot, Theme};

/// The replies `bytes` demand, concatenated: one per OSC 10/11/4 `?` query
/// found. Pure so the shapes are unit-testable; the caller writes the bytes
/// to the pane's stdin.
pub(crate) fn replies(bytes: &[u8], t: &Theme) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, slot) in queries(bytes) {
        let color = match (kind, slot) {
            (11, _) => Some(t.base),
            (10, _) => Some(t.stamp),
            (4, Some(n)) => terminal16_slot(n, t),
            _ => None,
        };
        if let Some(hex) = color.and_then(rgb16) {
            match (kind, slot) {
                (4, Some(n)) => {
                    out.extend_from_slice(format!("\x1b]4;{n};rgb:{hex}\x1b\\").as_bytes())
                }
                (k, _) => out.extend_from_slice(format!("\x1b]{k};rgb:{hex}\x1b\\").as_bytes()),
            }
        }
    }
    out
}

/// A color as the 16-bit-per-channel `rgb:rrrr/gggg/bbbb` form terminals
/// reply in. `Default`/`Indexed` have no absolute color of their own here.
fn rgb16(c: Color) -> Option<String> {
    let Color::Rgb(r, g, b) = c else {
        return None;
    };
    Some(format!(
        "{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}",
        r, r, g, g, b, b
    ))
}

/// The OSC color queries in one output chunk: `(kind, slot)` per `?` query
/// (`slot` is `Some` only for OSC 4). Sets (`4;0;#ff0000`) are ignored; only
/// queries answer.
///
/// ponytail: a query straddling two chunks is missed (no carry buffer);
/// emulators issue their probes in one write at startup, so the read chunk
/// holds it whole. Add a carry buffer if a client is seen splitting queries.
fn queries(bytes: &[u8]) -> Vec<(u8, Option<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b']' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let mut j = start;
        let mut end = None;
        while j < bytes.len() {
            if bytes[j] == 0x07 {
                end = Some(j);
                break;
            }
            if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                end = Some(j);
                break;
            }
            j += 1;
        }
        let Some(end) = end else {
            break; // truncated tail: nothing complete left in this chunk
        };
        out.extend(parse_query(&bytes[start..end]));
        i = if bytes[j] == 0x07 { j + 1 } else { j + 2 };
    }
    out
}

/// The OSC bodies' queries: `(11, None)` for `11;?`, one `(4, Some(n))` per
/// `4;<n>;?` slot in the interleaved form (`4;1;?;2;?`). Empty for anything
/// else (sets, other OSC numbers, garbage).
fn parse_query(body: &[u8]) -> Vec<(u8, Option<u8>)> {
    let Some(s) = std::str::from_utf8(body).ok() else {
        return Vec::new();
    };
    let Some((head, rest)) = s.split_once(';') else {
        return Vec::new();
    };
    match head {
        "10" | "11" if rest.trim() == "?" => vec![(head.parse().unwrap(), None)],
        "4" => rest
            .split(';')
            .collect::<Vec<_>>()
            .chunks(2)
            .filter_map(|pair| match pair {
                [slot, mark] if mark.trim() == "?" => {
                    slot.trim().parse::<u8>().ok().map(|n| (4, Some(n)))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The `COLORFGBG` value matching a theme's ground: rxvt `fg;bg` slots, and
/// a bg of 15 (white) is the light signal the client's own ladder reads
/// (`colorfgbg_is_light`). Exported at spawn so a harness resolving its
/// theme before its first OSC query lands on the right ground.
pub(crate) fn colorfgbg(t: &Theme) -> &'static str {
    if crate::theme::is_light(t) {
        "0;15"
    } else {
        "15;0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper() -> Theme {
        Theme::from_name("footnote-paper").0
    }

    #[test]
    fn a_ground_query_gets_the_theme_base() {
        let r = replies(b"\x1b]11;?\x1b\\", &paper());
        assert_eq!(r, b"\x1b]11;rgb:f7f7/f7f7/f7f7\x1b\\".to_vec());
    }

    #[test]
    fn bel_terminated_queries_answer_too() {
        let r = replies(b"\x1b]10;?\x07", &paper());
        assert_eq!(r, b"\x1b]10;rgb:1414/1414/1414\x1b\\".to_vec());
    }

    #[test]
    fn palette_queries_answer_each_slot_and_sets_are_ignored() {
        let r = replies(b"\x1b]4;1;?;2;?\x1b\\", &paper());
        assert_eq!(
            r,
            b"\x1b]4;1;rgb:9696/5353/5151\x1b\\\x1b]4;2;rgb:4646/7777/4848\x1b\\".to_vec()
        );
        assert!(replies(b"\x1b]4;0;#ff0000\x1b\\", &paper()).is_empty());
        assert!(replies(b"\x1b]0;title\x07", &paper()).is_empty());
    }

    #[test]
    fn chunks_without_a_complete_query_stay_silent() {
        assert!(replies(b"\x1b]11;?", &paper()).is_empty());
        assert!(replies(b"text only", &paper()).is_empty());
    }

    #[test]
    fn colorfgbg_tracks_the_ground() {
        assert_eq!(colorfgbg(&paper()), "0;15");
        let (dark, _) = Theme::from_name("footnote-superscript");
        assert_eq!(colorfgbg(&dark), "15;0");
    }
}

/// The theme at a pane's own directory: the same config ladder a client
/// launched there would walk. The SERVER's cwd names no project (a
/// detached bootstrap lands wherever the daemonizer left it), so the
/// pane's cwd is the truth the query answer must match.
pub(super) fn theme_at(cwd: &str) -> crate::theme::Theme {
    crate::digest_overlay::theme_for(std::path::Path::new(cwd)).0
}

impl super::Core {
    /// Whether an attached client FOCUSES this pane: the client loopback
    /// forwards a focused pane's real terminal replies into its stdin, so
    /// the server must not double-answer; an unfocused pane's probe would
    /// otherwise die in the mux emulator (the dark-on-light bug).
    fn pane_is_focused_somewhere(&self, pid: u64) -> bool {
        self.clients
            .iter()
            .any(|c| self.viewed_tab(c.view).is_some_and(|t| t.focus == pid))
    }

    /// Answer the OSC 10/11/4 `?` color queries in a pane's output with the
    /// active theme's colors, written to the pane's stdin. Focused panes are
    /// skipped (the client loopback owns their replies).
    pub(super) fn answer_color_queries(&self, pid: u64, bytes: &[u8]) {
        if let Some(reply) = self.osc_reply_for(pid, bytes) {
            if let Some(entry) = self.panes.get(&pid) {
                let _ = entry.pty.write_input(&reply);
            }
        }
    }

    /// The reply `pid`'s output demands, or `None`: a focused pane defers to
    /// the client loopback, an unhosted pane has no ground to answer from,
    /// and output without a query demands nothing.
    pub(super) fn osc_reply_for(&self, pid: u64, bytes: &[u8]) -> Option<Vec<u8>> {
        // The byte scan is free; the theme load is config IO on the drain
        // path, so no query means no IO.
        if queries(bytes).is_empty() {
            return None;
        }
        if self.pane_is_focused_somewhere(pid) {
            return None;
        }
        let entry = self.panes.get(&pid)?;
        let reply = replies(bytes, &theme_at(&entry.cwd));
        (!reply.is_empty()).then_some(reply)
    }
}
