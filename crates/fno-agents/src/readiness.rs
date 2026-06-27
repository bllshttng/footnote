//! Readiness detection (design module `readiness.rs`).
//!
//! A PTY-managed agent is "ready" when its CLI is waiting for input (prompt
//! drawn, not mid-render, not at an auth wall). The daemon must not send `ask`
//! input before readiness or the keystrokes land in the wrong UI state.
//!
//! Open Question #9 is a load-bearing constraint here: AgentRelay's "total
//! bytes > 500 -> assume ready" generic fallback is **rejected**. A banner can
//! emit 500 bytes without the CLI being ready, and Gemini's "Waiting for auth"
//! is a known false-ready. So:
//!
//! - Per-CLI [`ReadinessDetector`] impls are mandatory; there is no generic
//!   byte-count detector.
//! - Absence of a per-CLI signal is [`ReadinessError::UnknownReadinessSignal`],
//!   surfaced as a runtime error, never a guessed `true`.
//!
//! Wave 1 shipped the trait + the [`ScreenView`] seam + a fully-specified test
//! detector. Wave 2 adds the [`ScreenView`] construction (in [`crate::screen`],
//! backed by `alacritty_terminal`) and the real per-CLI
//! [`CodexReadinessDetector`] / [`GeminiReadinessDetector`] impls below
//! (Open Questions #2/#3).

/// A read-only view of the terminal screen the detector inspects. It is built
/// from the terminal grid in [`crate::screen`] after feeding it the PTY output
/// stream; the trait depends only on this shape so the substrate stays
/// decoupled from the terminal-emulator crate.
#[derive(Debug, Clone, Copy)]
pub struct ScreenView<'a> {
    /// The visible screen contents, rows joined by `\n`, trailing blanks
    /// trimmed. What a human would see.
    pub visible_text: &'a str,
    /// Cursor position (0-based). Some prompts are only distinguishable by
    /// where the cursor rests.
    pub cursor_row: usize,
    pub cursor_col: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReadinessError {
    /// No per-CLI readiness signal is available for this provider. Surfaced as
    /// a runtime error rather than a guessed readiness (Open Question #9).
    #[error("no readiness signal available for provider '{provider}'; refusing to guess")]
    UnknownReadinessSignal { provider: String },
}

/// Per-CLI readiness signal. Implementations inspect a [`ScreenView`] and
/// return whether the CLI is waiting for input. Implementations MUST NOT infer
/// readiness from raw byte counts.
pub trait ReadinessDetector: Send + Sync {
    /// `&str` (not `&'static str`) so a detector can name a provider known only
    /// at runtime (the daemon's wildcard NoSignalDetector carries the real
    /// provider name rather than the literal "unknown"; cv-789fdba0).
    fn provider_name(&self) -> &str;

    /// `Ok(true)` when the CLI is ready for input, `Ok(false)` when not yet,
    /// `Err(UnknownReadinessSignal)` when this provider has no usable signal.
    fn is_ready(&self, screen: &ScreenView) -> Result<bool, ReadinessError>;
}

/// Detector for providers that genuinely have no readiness signal yet. Always
/// errors; exists so the daemon can register a placeholder and get the
/// fail-loud behavior instead of a silent false-positive. (A previous design
/// would have returned `Ok(true)` here; that is the bug Open Question #9 bans.)
pub struct NoSignalDetector {
    pub provider: String,
}

impl ReadinessDetector for NoSignalDetector {
    fn provider_name(&self) -> &str {
        &self.provider
    }

    fn is_ready(&self, _screen: &ScreenView) -> Result<bool, ReadinessError> {
        Err(ReadinessError::UnknownReadinessSignal {
            provider: self.provider.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Per-CLI detectors (Wave 2). Codex = Open Question #2; Gemini = Open Question
// #3. Both follow the same discipline: never claim readiness from byte counts;
// reject known false-ready walls (auth / trust prompts) and mid-work states;
// claim ready only on a positive prompt-glyph signal.
//
// SMOKE-PINNING NOTE: the exact prompt glyphs each CLI draws when idle are
// best-known from the providers' documentation and the US4 captures, but have
// not been pinned against a live interactive TUI in this PR (running an
// interactive CLI headlessly risks an auth/trust hang). The detectors are
// CONSERVATIVE by construction: the only failure mode of a wrong glyph is a
// false-NOT-ready (the daemon waits/retries), never a false-ready (input sent
// into the wrong UI state) - which is exactly the bias Open Question #9
// mandates. `cli/scripts/smoke/capture-readiness-grid.sh` regenerates the grid
// fixtures against the live CLIs; tune `PROMPT_GLYPHS` from a capture.
// ---------------------------------------------------------------------------

/// Prompt indicators a modern CLI composer draws on its idle input line. A
/// match on the last non-blank line's trailing glyph is the positive readiness
/// signal. Centralized so a smoke capture tunes one place.
pub const PROMPT_GLYPHS: &[char] = &['\u{276f}', '\u{203a}', '\u{2595}']; // ❯ › ▌

/// Visible substrings that mean the CLI is mid-work and NOT accepting input,
/// even if a prompt glyph is also on screen.
const BUSY_MARKERS: &[&str] = &[
    "esc to interrupt",
    "Esc to interrupt",
    "Working",
    "Thinking",
];

/// Visible substrings for a blocking wall the operator must clear first (auth,
/// trust). The Gemini "Waiting for auth" false-ready (Open Question #3) lives
/// here.
const WALL_MARKERS: &[&str] = &[
    "Waiting for auth",
    "waiting for auth",
    "Do you trust",
    "Login required",
];

/// How many trailing non-blank lines form the "status region" the busy/wall
/// markers are matched against. These CLIs draw their composer + status bar
/// (the spinner, "esc to interrupt", auth wall) in the bottom few rows; model
/// reply text scrolls ABOVE it. Scoping the marker match to this region stops a
/// normal reply that happens to contain "Working"/"Thinking" from pinning the
/// detector permanently not-ready (Codex review P1).
const STATUS_REGION_LINES: usize = 3;

/// Shared readiness decision: not-ready under any wall or busy marker *in the
/// bottom status region*; otherwise ready only when the last non-blank line
/// ends with a recognized prompt glyph. Never guesses from byte counts (Open
/// Question #9).
fn prompt_ready(screen: &ScreenView, glyphs: &[char]) -> bool {
    let nonblank: Vec<&str> = screen
        .visible_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let Some(last) = nonblank.last() else {
        return false; // blank screen: nothing drawn yet, not ready
    };
    // Status region = the last few non-blank lines (where the composer/status
    // bar lives), NOT the whole scrollback.
    let region_start = nonblank.len().saturating_sub(STATUS_REGION_LINES);
    let region = nonblank[region_start..].join("\n");
    if WALL_MARKERS.iter().any(|m| region.contains(m)) {
        return false;
    }
    if BUSY_MARKERS.iter().any(|m| region.contains(m)) {
        return false;
    }
    let tail = last.trim_end();
    glyphs.iter().any(|g| tail.ends_with(*g))
}

/// Provider-agnostic readiness check for the grid attention scanner
/// (fu-grid-pagination / ab-82dddd5f). codex + gemini share [`PROMPT_GLYPHS`]
/// and the same [`prompt_ready`] logic, and the grid only ever hosts those
/// two (claude is excluded, grid Locked Decision 6), so a single shared check
/// is correct here. Run client-side on a pane's `Term` snapshot to flag an
/// off-screen agent waiting for input. Inherits the same wall / busy-marker
/// discipline as the per-CLI detectors: never a byte-count guess, and a
/// "Waiting for auth" wall reads as not-waiting. A brief false / late badge
/// is tolerable for an awareness hint, but a false-ready into a busy state is
/// not - hence the shared `prompt_ready` bias.
pub fn screen_is_waiting(screen: &ScreenView) -> bool {
    prompt_ready(screen, PROMPT_GLYPHS)
}

/// Codex interactive-composer readiness (Open Question #2).
pub struct CodexReadinessDetector;

impl ReadinessDetector for CodexReadinessDetector {
    fn provider_name(&self) -> &str {
        "codex"
    }

    fn is_ready(&self, screen: &ScreenView) -> Result<bool, ReadinessError> {
        Ok(prompt_ready(screen, PROMPT_GLYPHS))
    }
}

/// Gemini interactive-composer readiness (Open Question #3). Shares the prompt
/// glyph set with codex; the load-bearing difference is rejecting the
/// "Waiting for auth" wall, handled by the shared `WALL_MARKERS`.
pub struct GeminiReadinessDetector;

impl ReadinessDetector for GeminiReadinessDetector {
    fn provider_name(&self) -> &str {
        "gemini"
    }

    fn is_ready(&self, screen: &ScreenView) -> Result<bool, ReadinessError> {
        Ok(prompt_ready(screen, PROMPT_GLYPHS))
    }
}

/// Claude interactive-composer readiness (inside-out-multiplexer E1). Claude's
/// TUI composer draws the same prompt-glyph family as codex/gemini and its
/// mid-turn "esc to interrupt" status line is already in [`BUSY_MARKERS`], so
/// the shared [`prompt_ready`] logic applies unchanged. Threshold tuning against
/// a live capture is Claude's Discretion (the design's readiness-detector
/// bullet); the conservative bias (a wrong glyph is false-NOT-ready, never
/// false-ready) holds here as for the other CLIs.
pub struct ClaudeReadinessDetector;

impl ReadinessDetector for ClaudeReadinessDetector {
    fn provider_name(&self) -> &str {
        "claude"
    }

    fn is_ready(&self, screen: &ScreenView) -> Result<bool, ReadinessError> {
        Ok(prompt_ready(screen, PROMPT_GLYPHS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal grid-pattern detector to exercise the trait. Real Codex/Gemini
    /// detectors (Wave 2) match smoke-captured prompts; this stand-in matches a
    /// trailing prompt glyph on the cursor row and rejects a known false-ready
    /// auth banner, mirroring the discipline the real impls must follow.
    struct PromptGlyphDetector;
    impl ReadinessDetector for PromptGlyphDetector {
        fn provider_name(&self) -> &str {
            "test-cli"
        }
        fn is_ready(&self, screen: &ScreenView) -> Result<bool, ReadinessError> {
            if screen.visible_text.contains("Waiting for auth") {
                return Ok(false); // known false-ready (the Gemini trap)
            }
            Ok(screen.visible_text.trim_end().ends_with('\u{276f}')) // matches "❯"
        }
    }

    fn view(text: &str) -> ScreenView<'_> {
        ScreenView {
            visible_text: text,
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    #[test]
    fn ready_on_prompt_glyph() {
        let d = PromptGlyphDetector;
        assert_eq!(d.is_ready(&view("project \u{276f}")), Ok(true));
        assert_eq!(d.is_ready(&view("loading...")), Ok(false));
    }

    #[test]
    fn auth_wall_is_not_ready() {
        let d = PromptGlyphDetector;
        // Even with a trailing glyph, the auth banner must report not-ready.
        assert_eq!(d.is_ready(&view("Waiting for auth \u{276f}")), Ok(false));
    }

    #[test]
    fn no_signal_detector_errors_never_guesses() {
        let d = NoSignalDetector {
            provider: "opencode".to_string(),
        };
        // The detector reports the real provider name (not the literal
        // "unknown") in both provider_name() and the error (cv-789fdba0).
        assert_eq!(d.provider_name(), "opencode");
        assert_eq!(
            d.is_ready(&view("anything at all, 9999 bytes of banner")),
            Err(ReadinessError::UnknownReadinessSignal {
                provider: "opencode".into()
            })
        );
    }

    #[test]
    fn codex_detector_ready_on_idle_prompt() {
        let d = CodexReadinessDetector;
        assert_eq!(d.provider_name(), "codex");
        // Idle composer: last non-blank line ends with a prompt glyph.
        assert_eq!(
            d.is_ready(&view(
                "codex 0.130\n\n  build feature X\n\u{276f} ".trim_end()
            )),
            Ok(true)
        );
    }

    #[test]
    fn codex_detector_not_ready_while_working() {
        let d = CodexReadinessDetector;
        // Busy marker overrides any prompt glyph also on screen.
        assert_eq!(
            d.is_ready(&view("running tool...\nEsc to interrupt\n\u{276f}")),
            Ok(false)
        );
    }

    #[test]
    fn codex_detector_not_ready_without_prompt_signal() {
        let d = CodexReadinessDetector;
        // No prompt glyph anywhere: refuse to claim ready (no byte-count guess).
        assert_eq!(
            d.is_ready(&view("loading a 5000 byte banner of text")),
            Ok(false)
        );
    }

    #[test]
    fn gemini_detector_rejects_waiting_for_auth_false_ready() {
        let d = GeminiReadinessDetector;
        assert_eq!(d.provider_name(), "gemini");
        // The documented Gemini trap: prompt glyph present but auth wall up.
        assert_eq!(
            d.is_ready(&view("Waiting for auth...\n\u{276f}")),
            Ok(false)
        );
    }

    #[test]
    fn gemini_detector_ready_on_idle_prompt() {
        let d = GeminiReadinessDetector;
        assert_eq!(
            d.is_ready(&view("Gemini ready\n\u{203a} ".trim_end())),
            Ok(true)
        );
    }

    #[test]
    fn busy_word_in_model_reply_above_status_region_does_not_block() {
        // A normal reply mentioning "Working" / "Thinking" scrolls ABOVE the
        // composer; only the bottom status region gates readiness (Codex P1).
        let d = CodexReadinessDetector;
        let screen = "I am Working on the Thinking task you asked about.\n\
                      Here is a long reply that mentions Working again.\n\
                      filler line\n\
                      another filler\n\
                      \u{276f} ";
        assert_eq!(d.is_ready(&view(screen.trim_end())), Ok(true));
    }
}
