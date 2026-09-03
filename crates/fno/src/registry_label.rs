//! The registry-label grammar, one predicate for every fno-side consumer: the
//! server's pre-subprocess check and the TUI's input filter share it.

/// The registry-label grammar: 1..=64 chars from `[A-Za-z0-9_-]`. The same
/// rule the Rust client's `valid_agent_name` (fno-agents) enforces; the crates
/// do not link, only shell, so the two copies answer the same question.
pub fn valid_agent_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
