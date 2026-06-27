//! OSC (Operating System Command) capture for the readiness read loop (E6.1).
//!
//! alacritty's `vte::ansi::Processor` parses OSC sequences and dispatches them
//! to the terminal's event listener - which is `VoidListener` in
//! [`crate::screen`], so the OSC title/progress strings are discarded. The
//! manifest engine (E6.2) wants OSC title as a detection region: claude's
//! braille-spinner "working" signal lives in the window title, where it
//! survives scrollback, wrap, and resize that break grid-scraping. So this
//! module re-scans the same byte stream the grid sees and keeps the latest
//! title (OSC 0/2) and progress (OSC 9;4), reassembling sequences split across
//! PTY reads.
//!
//! Hand-rolled rather than a second `vte` parser: the OSC grammar is a tiny
//! state machine, herdr captures OSC the same way (`pane/osc.rs`), and an
//! explicit cross-read buffer is exactly what the reassembly test pins. The
//! state and buffer are struct fields, so a sequence split across `feed` calls
//! reassembles with no per-call setup.
//!
//! ponytail: deliberately naive about the grammar's dark corners - it handles
//! the 7-bit `ESC ]` introducer with BEL or `ESC \` (ST) terminators only, and
//! does not skip DCS/APC/PM/SOS string bodies (`ESC P/_/^/X`). The real grid is
//! parsed by alacritty's full `vte` processor; this scanner only feeds OSC
//! detection regions, so its sole failure mode on adversarial/binary input is a
//! spurious title, never a wrong grid. Harden (C1 forms, string-body skipping)
//! if a real agent's output trips it.

const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;

/// Cap on a single OSC body. Titles are short; this only stops a runaway or
/// binary stream (an unterminated OSC) from growing the buffer without bound.
const MAX_OSC_BODY: usize = 4096;

/// Parser state for an in-progress OSC sequence. Held across [`OscCapture::feed`]
/// calls so a sequence split across PTY reads reassembles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Outside any escape sequence.
    #[default]
    Ground,
    /// Saw `ESC`; waiting to see if it introduces an OSC (`]`).
    Escape,
    /// Inside an OSC body, accumulating until BEL or ST (`ESC \`).
    Osc,
    /// Inside an OSC body, saw `ESC`; a following `\` completes the ST terminator.
    OscEscape,
}

/// Captures the latest OSC title (OSC 0/2) and progress (OSC 9;4) from a PTY
/// byte stream. Feed it the same bytes as the grid, then read [`title`] /
/// [`progress`] from the snapshot. "Latest wins": a new title OSC overwrites the
/// previous one, mirroring how a terminal's window title behaves.
///
/// [`title`]: OscCapture::title
/// [`progress`]: OscCapture::progress
#[derive(Debug, Clone, Default)]
pub struct OscCapture {
    state: State,
    buffer: Vec<u8>,
    /// Set when a body exceeds `MAX_OSC_BODY`. A truncated body's title is
    /// unknowable, so an overflowed sequence is dropped at the terminator rather
    /// than published as a bogus prefix. Reset at each new OSC start.
    overflowed: bool,
    title: Option<String>,
    progress: Option<String>,
}

impl OscCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw PTY output. Safe to call with an OSC sequence split across
    /// reads (even mid-multibyte-UTF-8): bytes accumulate in `buffer` and are
    /// only decoded once the terminator arrives.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == ESC {
                        self.state = State::Escape;
                    }
                }
                State::Escape => {
                    if b == b']' {
                        self.buffer.clear();
                        self.overflowed = false;
                        self.state = State::Osc;
                    } else {
                        // Some other escape (CSI, plain ESC, ...): we only track
                        // OSC. Re-arm on a back-to-back ESC so it isn't dropped.
                        self.state = if b == ESC {
                            State::Escape
                        } else {
                            State::Ground
                        };
                    }
                }
                State::Osc => match b {
                    BEL => {
                        self.finish();
                        self.state = State::Ground;
                    }
                    ESC => self.state = State::OscEscape,
                    _ => {
                        if self.buffer.len() < MAX_OSC_BODY {
                            self.buffer.push(b);
                        } else {
                            // Body too long: stop accumulating and mark it so the
                            // terminator drops it instead of publishing a prefix.
                            self.overflowed = true;
                        }
                    }
                },
                State::OscEscape => {
                    if b == b'\\' {
                        // ST terminator (ESC \).
                        self.finish();
                        self.state = State::Ground;
                    } else if b == b']' {
                        // `ESC ]` is a new OSC introducer: an unterminated OSC
                        // ran straight into the next one. Drop the partial body
                        // and start the new sequence rather than losing it.
                        self.buffer.clear();
                        self.overflowed = false;
                        self.state = State::Osc;
                    } else {
                        // ESC inside the body not followed by `\` or `]`: the
                        // OSC is interrupted. Drop the partial body and return to
                        // Ground, re-arming if this byte is itself an ESC (the
                        // only byte Ground reacts to, so nothing else is lost).
                        self.buffer.clear();
                        self.state = if b == ESC {
                            State::Escape
                        } else {
                            State::Ground
                        };
                    }
                }
            }
        }
    }

    /// The most recent OSC window title (OSC 0 or OSC 2), if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The most recent OSC 9;4 progress payload (the part after `9;`), if any.
    pub fn progress(&self) -> Option<&str> {
        self.progress.as_deref()
    }

    /// Parse a completed OSC body (`Ps;Pt...`, terminator already stripped) and
    /// update the captured title/progress.
    fn finish(&mut self) {
        // A body that overflowed MAX_OSC_BODY was truncated mid-stream, so its
        // title/progress is unknowable: publish nothing rather than a bogus
        // prefix. (The `ESC ]` restart path still lets a later valid OSC win.)
        if self.overflowed {
            self.buffer.clear();
            self.overflowed = false;
            return;
        }
        // Decode lazily here, so a body split mid-multibyte across feeds is fine.
        if let Ok(body) = std::str::from_utf8(&self.buffer) {
            if let Some((code, rest)) = body.split_once(';') {
                match code {
                    // OSC 0 (icon name + window title) and OSC 2 (window title)
                    // set the title. OSC 1 (icon name only) is intentionally not
                    // treated as the title.
                    "0" | "2" => self.title = Some(rest.to_string()),
                    // OSC 9;4;state;pct is the ConEmu / Windows-Terminal progress
                    // sequence. Bare OSC 9 is an iTerm2 notification (not
                    // progress), so gate on the "4" subcode. Store the payload
                    // after "9;" raw; the engine matches it as a region string.
                    // ponytail: raw payload, no state/pct struct until a rule needs one.
                    "9" if rest.split(';').next() == Some("4") => {
                        self.progress = Some(rest.to_string())
                    }
                    _ => {}
                }
            }
        }
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_title_split_across_two_feeds_reassembles() {
        // AC-E6-1: a split OSC sequence across two reads reassembles.
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]2;hel");
        assert_eq!(osc.title(), None, "not terminated yet, nothing captured");
        osc.feed(b"lo world\x07");
        assert_eq!(osc.title(), Some("hello world"));
    }

    #[test]
    fn esc_introducer_split_from_bracket() {
        // The ESC and the `]` land in different reads.
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b");
        osc.feed(b"]2;ok\x07");
        assert_eq!(osc.title(), Some("ok"));
    }

    #[test]
    fn st_terminator_accepted() {
        // ESC \ (ST) terminates an OSC just like BEL.
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]0;title here\x1b\\");
        assert_eq!(osc.title(), Some("title here"));
    }

    #[test]
    fn braille_spinner_title_survives_mid_codepoint_split() {
        // claude's "working" signal is a braille spinner in the title. Split the
        // 3-byte braille codepoint (U+280B = e2 a0 8b) across two feeds.
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]2;\xe2\xa0");
        osc.feed(b"\x8b Compiling\x07");
        assert_eq!(osc.title(), Some("\u{280b} Compiling"));
    }

    #[test]
    fn osc_9_4_is_progress_but_bare_osc_9_is_not() {
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]9;4;1;50\x07");
        assert_eq!(osc.progress(), Some("4;1;50"));
        // A bare OSC 9 (iTerm2 notification) must not be read as progress.
        let mut other = OscCapture::new();
        other.feed(b"\x1b]9;build done\x07");
        assert_eq!(other.progress(), None);
    }

    #[test]
    fn latest_title_wins() {
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]2;first\x07\x1b]2;second\x07");
        assert_eq!(osc.title(), Some("second"));
    }

    #[test]
    fn unterminated_osc_running_into_next_osc_captures_the_second() {
        // An OSC with no BEL/ST, immediately followed by another OSC: the `ESC ]`
        // mid-body is the next sequence's introducer, not garbage to abort on.
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]2;first\x1b]2;second\x07");
        assert_eq!(osc.title(), Some("second"));
    }

    #[test]
    fn oversized_osc_body_is_dropped_not_published_truncated() {
        // A body longer than MAX_OSC_BODY is truncated mid-stream; publishing the
        // prefix would be a bogus title, so the terminator must drop it.
        let mut osc = OscCapture::new();
        let mut seq = b"\x1b]2;".to_vec();
        seq.extend(std::iter::repeat(b'x').take(MAX_OSC_BODY + 100));
        seq.push(BEL);
        osc.feed(&seq);
        assert_eq!(
            osc.title(),
            None,
            "truncated oversized title must not publish"
        );
        // A later well-formed OSC still wins (overflow flag reset on new start).
        osc.feed(b"\x1b]2;ok\x07");
        assert_eq!(osc.title(), Some("ok"));
    }

    #[test]
    fn osc_1_icon_name_is_not_a_title() {
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]1;iconname\x07");
        assert_eq!(osc.title(), None);
    }

    #[test]
    fn interrupted_osc_does_not_capture_garbage() {
        // An ESC that is not part of an ST aborts the OSC body.
        let mut osc = OscCapture::new();
        osc.feed(b"\x1b]2;par\x1b[0mtial\x07");
        // The body was interrupted by a CSI (`ESC [ 0 m`); no title captured,
        // and the parser is back in a sane state for the next sequence.
        assert_eq!(osc.title(), None);
        osc.feed(b"\x1b]2;clean\x07");
        assert_eq!(osc.title(), Some("clean"));
    }
}
