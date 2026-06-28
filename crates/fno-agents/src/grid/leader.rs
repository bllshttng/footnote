//! Leader-key input model for the railless tiled grid (x-b563, Phase 1).
//!
//! On the tiled compositor path the operator now *lives in drive* - every bare
//! keystroke reaches the focused agent - and reaches the multiplexer through a
//! single configurable **leader** key (default `Ctrl-Space`). Press the leader,
//! then one command key, to issue a mux command without leaving the agent;
//! double-tap the leader to send its literal byte to the agent so no key is ever
//! permanently stolen (tmux `send-prefix`). This retires the WATCH/DRIVE modal
//! dance (Locked Decision 1) on the tiled surface.
//!
//! This module is PURE: it resolves the configured leader, detects it across the
//! terminal encodings it can arrive as, and runs the two-state machine. The run
//! loop owns every side effect (claim, byte forwarding, render) and maps a
//! post-leader command key through the former WATCH keymap (`key_to_input(_,
//! Mode::Watch)`) so the mux command set is exactly today's WATCH keys with no
//! duplication. The rail surface keeps its own RailNav/PaneDrive model; bringing
//! the leader there is Phase 2 (x-d97d), because the rail's browse-many-claim-one
//! ergonomics do not map onto "always driving" under the exclusive driver claim.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};

/// The configured leader key, resolved from `config.grid.leader_key`.
/// Defaults to `Ctrl-Space`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaderKey {
    /// The base key the leader is bound to. For the `Ctrl-Space` default this is
    /// `Char(' ')`; [`is_leader`] additionally accepts the `Ctrl-@`/NUL
    /// encodings that some terminals deliver instead (see Domain Pitfalls).
    code: KeyCode,
}

impl Default for LeaderKey {
    fn default() -> Self {
        // Ctrl-Space (Locked Decision 2).
        LeaderKey {
            code: KeyCode::Char(' '),
        }
    }
}

impl LeaderKey {
    /// Compact label for footers / hints, e.g. `^Space` or `^A`. Derived from
    /// the configured key so the UI never names a leader the operator did not
    /// bind (gemini review on PR #79).
    pub fn format_compact(&self) -> String {
        match self.code {
            KeyCode::Char(' ') => "^Space".to_string(),
            KeyCode::Char(c) => format!("^{}", c.to_ascii_uppercase()),
            _ => "^Space".to_string(),
        }
    }

    /// Verbose label for prose, e.g. `Ctrl-Space` or `Ctrl-A`.
    pub fn format_verbose(&self) -> String {
        match self.code {
            KeyCode::Char(' ') => "Ctrl-Space".to_string(),
            KeyCode::Char(c) => format!("Ctrl-{}", c.to_ascii_uppercase()),
            _ => "Ctrl-Space".to_string(),
        }
    }
}

/// Parse `config.grid.leader_key` into a [`LeaderKey`].
///
/// Accepts `ctrl-space` (the default) and `ctrl-<letter>` (e.g. `ctrl-a`,
/// `ctrl-g`). `c-` is accepted as a short prefix. Refuses the three keys Locked
/// Decision 2 excludes - `ctrl-m` (IS Enter / `0x0D`), `ctrl-c` (interrupt), and
/// `ctrl-b` (Claude Code backgrounds the agent) - and any unrecognized value, by
/// falling back to the `Ctrl-Space` default rather than binding a hostile key.
pub fn parse_leader_key(raw: &str) -> LeaderKey {
    let s = raw.trim().to_ascii_lowercase();
    let rest = s
        .strip_prefix("ctrl-")
        .or_else(|| s.strip_prefix("c-"))
        .unwrap_or("");
    match rest {
        "space" | " " => LeaderKey::default(),
        // A single a-z letter, minus the excluded set.
        l if l.len() == 1 && l.as_bytes()[0].is_ascii_lowercase() => {
            let c = l.as_bytes()[0] as char;
            if matches!(c, 'm' | 'c' | 'b') {
                LeaderKey::default()
            } else {
                LeaderKey {
                    code: KeyCode::Char(c),
                }
            }
        }
        _ => LeaderKey::default(),
    }
}

/// Does this key event carry the configured leader?
///
/// Ctrl chords reach a TUI in more than one encoding. For the `Ctrl-Space`
/// default we accept `Char(' ')+CONTROL`, the `Ctrl-@`/`Char('@')+CONTROL`
/// alias, and the bare NUL (`Char('\0')` / [`KeyCode::Null`]) that many
/// terminals send for Ctrl-Space. For a `ctrl-<letter>` leader we accept
/// `Char(letter)+CONTROL` and the raw control byte (`Char((letter-'a'+1) as
/// char)`) some terminals deliver instead.
pub fn is_leader(key: &KeyEvent, leader: &LeaderKey) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match leader.code {
        KeyCode::Char(' ') => {
            // Ctrl-Space and its NUL/Ctrl-@ aliases.
            matches!(key.code, KeyCode::Null)
                || matches!(key.code, KeyCode::Char('\0'))
                || (ctrl && matches!(key.code, KeyCode::Char(' ') | KeyCode::Char('@')))
        }
        KeyCode::Char(letter) => {
            let raw = (letter as u8 - b'a' + 1) as char;
            (ctrl && key.code == KeyCode::Char(letter)) || key.code == KeyCode::Char(raw)
        }
        _ => false,
    }
}

/// The bytes a `send-prefix` (double-tap) writes to the focused agent: the
/// literal control byte the leader stands for. `Ctrl-Space` -> NUL (`0x00`);
/// `ctrl-<letter>` -> `0x01..=0x1a`.
pub fn leader_bytes(leader: &LeaderKey) -> Vec<u8> {
    match leader.code {
        KeyCode::Char(' ') => vec![0x00],
        KeyCode::Char(letter) if letter.is_ascii_lowercase() => {
            vec![letter as u8 - b'a' + 1]
        }
        _ => vec![0x00],
    }
}

/// Resolve `config.grid.leader_key` for the grid, mirroring the candidate
/// precedence of the other Rust-side settings readers (`agents_config`):
/// `$FNO_CONFIG` (the sole source when set), then `<cwd>/.fno/settings.yaml`,
/// then the global `$FNO_GLOBAL_SETTINGS_PATH` / `$HOME/.fno/settings.yaml`. Any
/// miss or parse failure degrades to the `Ctrl-Space` default, so a typo can
/// never leave the leader unreachable.
pub fn resolve_leader_key(cwd: &Path) -> LeaderKey {
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return read_grid_leader_key_file(Path::new(&explicit))
            .map(|s| parse_leader_key(&s))
            .unwrap_or_default();
    }
    if let Some(s) = read_grid_leader_key_file(&cwd.join(".fno/settings.yaml")) {
        return parse_leader_key(&s);
    }
    let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/settings.yaml")));
    if let Some(g) = global {
        if let Some(s) = read_grid_leader_key_file(&g) {
            return parse_leader_key(&s);
        }
    }
    LeaderKey::default()
}

fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    match std::env::var_os(key) {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn read_grid_leader_key_file(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    read_grid_leader_key(&content)
}

/// Scan a settings.yaml body for `config: > grid: > leader_key:`. Indent-unit
/// agnostic (2- or 4-space), mirroring `agents_config`'s nesting scanner. Returns
/// the raw value (e.g. `"ctrl-a"`); `None` when absent or `null` so the caller
/// falls through to the `Ctrl-Space` default.
pub(crate) fn read_grid_leader_key(content: &str) -> Option<String> {
    let unit = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .find(|&i| i > 0)
        .unwrap_or(2);
    let level = |line: &str| -> usize { (line.len() - line.trim_start().len()) / unit };
    let mut in_config = false;
    let mut in_grid = false;
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match level(line) {
            0 => {
                in_config = t.starts_with("config:");
                in_grid = false;
            }
            1 if in_config => {
                in_grid = t.starts_with("grid:");
            }
            2 if in_grid => {
                if let Some(rest) = t.strip_prefix("leader_key:") {
                    let v = rest
                        .split('#')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'');
                    if !v.is_empty() && !v.eq_ignore_ascii_case("null") {
                        return Some(v.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Two-state input router for the leader model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeaderState {
    /// Resting: every non-leader key forwards to the focused agent (drive).
    #[default]
    Normal,
    /// The leader was pressed; the NEXT key is a mux command (or a second leader
    /// press, which is `send-prefix`). Resolved by exactly one subsequent key.
    Pending,
}

/// What the run loop should do with a key, given the current [`LeaderState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderDecision {
    /// Was `Normal`, saw the leader: enter `Pending`, show the LEADER cue.
    EnterPending,
    /// Was `Pending`, saw the leader again (double-tap): send the literal leader
    /// byte to the focused agent (`send-prefix`); return to `Normal`.
    SendPrefix,
    /// Was `Pending`, saw a non-leader key: run it as a mux command (the caller
    /// maps it through the WATCH keymap); return to `Normal`.
    Command(KeyEvent),
    /// Was `Normal`, saw a non-leader key: forward it to the focused agent.
    Forward,
}

/// Step the leader machine. Returns the next state and the decision. Pure - the
/// caller performs the side effect named by [`LeaderDecision`]. `Pending` is
/// always resolved by exactly one key (no key is silently swallowed: a non-leader
/// key in `Pending` becomes a `Command`, which the caller either executes or
/// reports as unknown - it is never forwarded to the agent).
pub fn step(
    state: LeaderState,
    key: &KeyEvent,
    leader: &LeaderKey,
) -> (LeaderState, LeaderDecision) {
    let leading = is_leader(key, leader);
    match (state, leading) {
        (LeaderState::Normal, true) => (LeaderState::Pending, LeaderDecision::EnterPending),
        (LeaderState::Normal, false) => (LeaderState::Normal, LeaderDecision::Forward),
        (LeaderState::Pending, true) => (LeaderState::Normal, LeaderDecision::SendPrefix),
        (LeaderState::Pending, false) => (LeaderState::Normal, LeaderDecision::Command(*key)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parse_defaults_to_ctrl_space() {
        assert_eq!(parse_leader_key("ctrl-space"), LeaderKey::default());
        assert_eq!(parse_leader_key("  Ctrl-Space "), LeaderKey::default());
        assert_eq!(parse_leader_key("c-space"), LeaderKey::default());
        // Unknown / malformed -> default, never a hostile binding.
        assert_eq!(parse_leader_key("garbage"), LeaderKey::default());
        assert_eq!(parse_leader_key(""), LeaderKey::default());
    }

    #[test]
    fn parse_accepts_ctrl_letter_but_refuses_excluded() {
        assert_eq!(
            parse_leader_key("ctrl-a"),
            LeaderKey {
                code: KeyCode::Char('a')
            }
        );
        assert_eq!(
            parse_leader_key("ctrl-g"),
            LeaderKey {
                code: KeyCode::Char('g')
            }
        );
        // Locked Decision 2 exclusions fall back to the default.
        assert_eq!(parse_leader_key("ctrl-m"), LeaderKey::default());
        assert_eq!(parse_leader_key("ctrl-c"), LeaderKey::default());
        assert_eq!(parse_leader_key("ctrl-b"), LeaderKey::default());
    }

    #[test]
    fn is_leader_accepts_ctrl_space_encodings() {
        let l = LeaderKey::default();
        assert!(is_leader(&k(KeyCode::Char(' '), KeyModifiers::CONTROL), &l));
        assert!(is_leader(&k(KeyCode::Char('@'), KeyModifiers::CONTROL), &l)); // Ctrl-@
        assert!(is_leader(&k(KeyCode::Char('\0'), KeyModifiers::NONE), &l)); // NUL
        assert!(is_leader(&k(KeyCode::Null, KeyModifiers::NONE), &l));
        // A bare space (no ctrl) is NOT the leader - it types to the agent.
        assert!(!is_leader(&k(KeyCode::Char(' '), KeyModifiers::NONE), &l));
        assert!(!is_leader(
            &k(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &l
        ));
    }

    #[test]
    fn is_leader_accepts_ctrl_letter_encodings() {
        let l = parse_leader_key("ctrl-a");
        assert!(is_leader(&k(KeyCode::Char('a'), KeyModifiers::CONTROL), &l));
        assert!(is_leader(&k(KeyCode::Char('\x01'), KeyModifiers::NONE), &l)); // raw Ctrl-A byte
        assert!(!is_leader(&k(KeyCode::Char('a'), KeyModifiers::NONE), &l));
        assert!(!is_leader(
            &k(KeyCode::Char(' '), KeyModifiers::CONTROL),
            &l
        ));
    }

    #[test]
    fn leader_bytes_are_the_control_byte() {
        assert_eq!(leader_bytes(&LeaderKey::default()), vec![0x00]); // Ctrl-Space
        assert_eq!(leader_bytes(&parse_leader_key("ctrl-a")), vec![0x01]);
        assert_eq!(leader_bytes(&parse_leader_key("ctrl-g")), vec![0x07]);
    }

    #[test]
    fn step_normal_then_command_returns_to_normal() {
        let l = LeaderKey::default();
        let leader = k(KeyCode::Char(' '), KeyModifiers::CONTROL);
        // Normal + leader -> Pending / EnterPending.
        let (s, d) = step(LeaderState::Normal, &leader, &l);
        assert_eq!((s, d), (LeaderState::Pending, LeaderDecision::EnterPending));
        // Pending + a command key -> Normal / Command(key), exactly one key.
        let tab = k(KeyCode::Tab, KeyModifiers::NONE);
        let (s, d) = step(LeaderState::Pending, &tab, &l);
        assert_eq!(s, LeaderState::Normal);
        assert_eq!(d, LeaderDecision::Command(tab));
    }

    #[test]
    fn step_double_tap_is_send_prefix() {
        let l = LeaderKey::default();
        let leader = k(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let (s, d) = step(LeaderState::Pending, &leader, &l);
        assert_eq!((s, d), (LeaderState::Normal, LeaderDecision::SendPrefix));
    }

    #[test]
    fn step_normal_non_leader_forwards() {
        let l = LeaderKey::default();
        let a = k(KeyCode::Char('a'), KeyModifiers::NONE);
        let (s, d) = step(LeaderState::Normal, &a, &l);
        assert_eq!((s, d), (LeaderState::Normal, LeaderDecision::Forward));
    }

    #[test]
    fn format_helpers_reflect_the_configured_key() {
        assert_eq!(LeaderKey::default().format_compact(), "^Space");
        assert_eq!(LeaderKey::default().format_verbose(), "Ctrl-Space");
        assert_eq!(parse_leader_key("ctrl-a").format_compact(), "^A");
        assert_eq!(parse_leader_key("ctrl-a").format_verbose(), "Ctrl-A");
        assert_eq!(parse_leader_key("ctrl-g").format_compact(), "^G");
    }

    #[test]
    fn read_grid_leader_key_parses_nested() {
        let yaml = "config:\n  grid:\n    leader_key: ctrl-a\n";
        assert_eq!(read_grid_leader_key(yaml).as_deref(), Some("ctrl-a"));
    }

    #[test]
    fn read_grid_leader_key_absent_is_none() {
        assert_eq!(read_grid_leader_key("schema_version: 1\n"), None);
        // leader_key under a different block must not match.
        let yaml = "config:\n  target:\n    leader_key: ctrl-a\n";
        assert_eq!(read_grid_leader_key(yaml), None);
        // null sentinel -> absent -> default.
        let yaml = "config:\n  grid:\n    leader_key: null\n";
        assert_eq!(read_grid_leader_key(yaml), None);
    }

    #[test]
    fn read_grid_leader_key_handles_four_space_and_quotes() {
        let yaml = "config:\n    grid:\n        leader_key: \"ctrl-g\"  # comment\n";
        assert_eq!(read_grid_leader_key(yaml).as_deref(), Some("ctrl-g"));
    }
}
