//! The merge-refusal carrier: ONE vocabulary table and the Rust engine over
//! it (x-8151, standing law d-450caaeb: Rust is the product, Python is the
//! compatibility shell). The `/target`-family spellings and the carrier
//! tokens live in `merge_posture.toml` (canonical, `include_str!`ed here;
//! `build.rs` distributes the byte copy Python reads), and the posture
//! semantics live beside each language's reader, unit-tested in both.
//!
//! Semantics, unchanged since their review rounds: a family message with the
//! flag arms the carrier; a family message with a bare token OUTSIDE flag
//! position neither arms nor clears (ambiguous, round 11); a family message
//! with no token clears an inherited carrier loudly (the message is
//! authoritative, round 8); a non-family message clears NOTHING (an operator's
//! exported carrier is a documented control input, and a leak errs toward
//! refusing merges, the safe side). The word-padded flag match keeps
//! `--no-merge-guard` (a different flag) from counting (round 8).

use std::sync::OnceLock;

const TABLE: &str = include_str!("merge_posture.toml");

struct Carrier {
    spellings: Vec<String>,
    flag: String,
    legacy_token: String,
}

fn table() -> &'static Carrier {
    static CELL: OnceLock<Carrier> = OnceLock::new();
    CELL.get_or_init(|| {
        let raw: toml::Value = toml::from_str(TABLE).expect("merge_posture.toml must parse");
        let family = &raw["target_family"];
        let carrier = &raw["carrier"];
        Carrier {
            spellings: family["spellings"]
                .as_array()
                .expect("target_family.spellings must be a list")
                .iter()
                .map(|v| {
                    v.as_str()
                        .expect("spellings entries must be strings")
                        .to_string()
                })
                .collect(),
            flag: carrier["flag"].as_str().expect("carrier.flag").to_string(),
            legacy_token: carrier["legacy_token"]
                .as_str()
                .expect("carrier.legacy_token")
                .to_string(),
        }
    })
}

fn is_family_spelling(token: &str) -> bool {
    table().spellings.iter().any(|s| s == token)
}

/// True when the message's first whitespace token is a /target-family command
/// spelling. An empty or all-whitespace message has no first token.
pub fn is_target_family(message: &str) -> bool {
    match message.split_whitespace().next() {
        Some(first) => is_family_spelling(first),
        None => false,
    }
}

/// True when a /target-family message carries the carrier flag. The family
/// gate is load-bearing: a `/think` or `/review` prompt that MENTIONS the
/// flag arms no carrier, and neither does prose.
pub fn message_carries_no_merge(message: &str) -> bool {
    is_target_family(message) && padded(message).contains(&format!(" {} ", table().flag))
}

/// True when a /target-family message carries the bare legacy token.
/// Word-padded, so the flag form never matches here.
fn message_carries_bare_token(message: &str) -> bool {
    is_target_family(message) && padded(message).contains(&format!(" {} ", table().legacy_token))
}

fn padded(message: &str) -> String {
    format!(" {} ", message)
}

/// Rewrite the legacy bare token to the flag in a /target-family command.
/// Scoped to the two positions the legacy injectors actually produced
/// (round 12): directly after the verb, or trailing. A MID-STRING token is
/// left alone on purpose: a /target argument is free text, and rewriting the
/// word anywhere would mutate prompt text the operator typed.
pub fn normalize_legacy_no_merge(command: &str) -> String {
    let legacy = table().legacy_token.as_str();
    let flag = table().flag.as_str();
    let mut parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() || !is_family_spelling(parts[0]) {
        return command.to_string();
    }
    if parts.len() >= 2 && parts[1] == legacy {
        parts[1] = flag;
    } else if parts.len() >= 3 && parts[parts.len() - 1] == legacy {
        let last = parts.len() - 1;
        parts[last] = flag;
    } else {
        return command.to_string();
    }
    parts.join(" ")
}

/// Insert the carrier flag into a /target-family command, right after the
/// verb token. Skipped when a standalone flag is already present. Non-family
/// commands pass through untouched.
pub fn inject_no_merge(command: &str) -> String {
    let flag = table().flag.as_str();
    if !is_target_family(command) || padded(command).contains(&format!(" {flag} ")) {
        return command.to_string();
    }
    let mut parts: Vec<&str> = command.split_whitespace().collect();
    parts.insert(1, flag);
    parts.join(" ")
}

/// Remove the carrier from a /target-family command: the first standalone
/// flag and the first standalone legacy token. A pathological id like
/// `no-merger-x` is never touched (standalone tokens only), and non-family
/// commands pass through so a prose brief's text is never mangled.
pub fn strip_no_merge(command: &str) -> String {
    if !is_target_family(command) {
        return command.to_string();
    }
    let mut parts: Vec<&str> = command.split_whitespace().collect();
    for token in [table().flag.as_str(), table().legacy_token.as_str()] {
        if let Some(pos) = parts[1..].iter().position(|p| *p == token) {
            parts.remove(pos + 1);
        }
    }
    parts.join(" ")
}

/// The env posture a message dictates for the carrier env key. `Hold` covers
/// both the ambiguous bare-token case and non-family prose (which clears
/// nothing).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EnvAction {
    Arm,
    Hold,
    Clear,
}

/// Decide the env posture for `message`, given the inherited carrier value.
/// The loud-clear note is `Some` only when a non-empty inherited carrier is
/// actually cleared, so the operator hears about a real state change, not a
/// no-op.
pub fn posture_action(message: &str, prior: Option<&str>) -> (EnvAction, Option<String>) {
    if message_carries_no_merge(message) {
        return (EnvAction::Arm, None);
    }
    if message_carries_bare_token(message) || !is_target_family(message) {
        return (EnvAction::Hold, None);
    }
    let note = prior.filter(|p| !p.is_empty()).map(|_| {
        "fno agents spawn: inherited TARGET_NO_MERGE cleared; the \
             /target-family message carries no --no-merge flag and the \
             message is authoritative"
            .to_string()
    });
    (EnvAction::Clear, note)
}

/// Apply the posture to THIS process's environment so every child spawned
/// below inherits the carrier (the Rust spawn lane's one application point,
/// `client::maybe_run_spawn`). The Python spawn lane applies the same
/// verdict from the same table; both are idempotent, so a lane that runs
/// twice converges instead of flip-flopping. Returns whether a loud-clear
/// note was emitted (the caller prints it on its own stderr).
pub fn apply_env_from_message(message: &str) -> bool {
    let prior = std::env::var("TARGET_NO_MERGE").ok();
    let (action, note) = posture_action(message, prior.as_deref());
    match action {
        EnvAction::Arm => std::env::set_var("TARGET_NO_MERGE", "1"),
        EnvAction::Clear => std::env::remove_var("TARGET_NO_MERGE"),
        EnvAction::Hold => {}
    }
    if let Some(text) = note {
        eprintln!("{text}");
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_pins_the_carrier_vocabulary() {
        // Table-fed: the spellings the canonical table ships are the ones the
        // engine answers from.
        assert!(!table().spellings.is_empty());
        assert!(is_target_family("/target x-1"));
        assert!(is_target_family("/fno:target x-1"));
        assert!(is_target_family("$fno:target x-1"));
        assert!(!is_target_family(""));
        assert!(!is_target_family("   "));
        assert!(!is_target_family("/think x-1"));
        assert!(!is_target_family("target x-1"));
        assert!(!is_target_family("/targets x-1"));
    }

    #[test]
    fn carries_needs_family_and_the_word_padded_flag() {
        assert!(message_carries_no_merge("/target --no-merge x-1"));
        assert!(message_carries_no_merge("$fno:target x-1 --no-merge"));
        assert!(!message_carries_no_merge("/target x-1"));
        // A different flag with the same prefix is not the carrier (round 8).
        assert!(!message_carries_no_merge("/target --no-merge-guard x-1"));
        // Prose mentioning the flag arms nothing (x-9d11).
        assert!(!message_carries_no_merge(
            "please run /target --no-merge for me"
        ));
        assert!(!message_carries_no_merge("/think --no-merge x-1"));
    }

    #[test]
    fn legacy_rewrite_is_position_scoped() {
        assert_eq!(
            normalize_legacy_no_merge("/target no-merge x-1"),
            "/target --no-merge x-1"
        );
        assert_eq!(
            normalize_legacy_no_merge("/target x-1 no-merge"),
            "/target x-1 --no-merge"
        );
        // Mid-string stays free text (round 10).
        assert_eq!(
            normalize_legacy_no_merge("/target fix the no-merge carrier bug"),
            "/target fix the no-merge carrier bug"
        );
        assert_eq!(
            normalize_legacy_no_merge("/think no-merge x-1"),
            "/think no-merge x-1"
        );
        assert_eq!(normalize_legacy_no_merge("/target"), "/target");
    }

    #[test]
    fn inject_is_idempotent_and_family_scoped() {
        assert_eq!(inject_no_merge("/target x-1"), "/target --no-merge x-1");
        assert_eq!(
            inject_no_merge("/target --no-merge x-1"),
            "/target --no-merge x-1"
        );
        assert_eq!(inject_no_merge("/think x-1"), "/think x-1");
        assert_eq!(
            inject_no_merge("/target --no-merge-guard x-1"),
            "/target --no-merge --no-merge-guard x-1"
        );
    }

    #[test]
    fn strip_removes_both_spellings_once() {
        assert_eq!(strip_no_merge("/target --no-merge x-1"), "/target x-1");
        assert_eq!(strip_no_merge("/target no-merge x-1"), "/target x-1");
        assert_eq!(strip_no_merge("/target x-1"), "/target x-1");
        assert_eq!(
            strip_no_merge("/think --no-merge x-1"),
            "/think --no-merge x-1"
        );
        // A pathological id is never touched (standalone tokens only).
        assert_eq!(strip_no_merge("/target no-merger-x"), "/target no-merger-x");
    }

    #[test]
    fn env_action_matrix() {
        // Flag arms.
        assert_eq!(
            posture_action("/target --no-merge x-1", None),
            (EnvAction::Arm, None)
        );
        // Bare token: ambiguous, neither arms nor clears (round 11).
        assert_eq!(
            posture_action("/target no-merge x-1", Some("1")),
            (EnvAction::Hold, None)
        );
        // Family, no token: clears loudly when a carrier was inherited.
        let (action, note) = posture_action("/target x-1", Some("1"));
        assert_eq!(action, EnvAction::Clear);
        assert!(note.unwrap().contains("inherited TARGET_NO_MERGE cleared"));
        // ... and silently when none was.
        assert_eq!(
            posture_action("/target x-1", None),
            (EnvAction::Clear, None)
        );
        // Prose clears NOTHING, even over an inherited carrier.
        assert_eq!(
            posture_action("fix the bug", Some("1")),
            (EnvAction::Hold, None)
        );
    }
}
