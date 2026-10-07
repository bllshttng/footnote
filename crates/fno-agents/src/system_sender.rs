//! The one system-sender name table (design R9): every daemon arm that mails
//! a session is named `fno/<arm>`, and every legacy stamp on an old bus row
//! maps to its new name, so the read model marks rows `system` from this one
//! table and the mux never matches names itself.
//!
//! Mail never comes from a bare `fno`: the renderer and the bus-append door
//! refuse it with the remedy named (design R13), because a bare `fno` could
//! never be answered and read as a system voice with no arm.

/// The daemon arms that send mail, and the `fno/<arm>` name each one uses.
pub const SYSTEM_ARMS: &[&str] = &[
    "lead-settle",
    "pr-nudge",
    "burn-watch",
    "fleet-incident",
    "mail-hold",
    "questions",
    "notice-router",
];

/// The legacy stamps old bus rows carry, and the `fno/<arm>` name each one
/// reads as. A stamp is recognized with or without its dash.
pub const LEGACY_ALIASES: &[(&str, &str)] = &[
    ("lead-settle", "fno/lead-settle"),
    ("pr-nudge", "fno/pr-nudge"),
    ("burn-watch", "fno/burn-watch"),
    ("fleet-incident", "fno/fleet-incident"),
    ("fno-mail-hold", "fno/mail-hold"),
];

/// The `fno/<arm>` name for an arm word (`lead-settle` -> `fno/lead-settle`).
/// An unknown arm is still allowed - a new daemon arm takes the prefix and
/// the read model marks it system from the prefix, so the table lists the
/// arms only for documentation and tests.
pub fn system_name(arm: &str) -> String {
    format!("fno/{arm}")
}

/// The canonical `fno/<already-prefixed>` or legacy stamp read: `lead-settle`
/// and `fno/lead-settle` and `fno-mail-hold` all read `fno/lead-settle` and
/// `fno/mail-hold` respectively. An unknown sender reads unchanged.
pub fn canonical(sender: &str) -> &str {
    for (legacy, name) in LEGACY_ALIASES {
        if sender == *legacy {
            return name;
        }
    }
    sender
}

/// True when the sender is a system voice: the `fno/` prefix, a legacy stamp
/// that maps to one, or the bare `fno` the old floor produced. Old rows
/// stamped `lead-settle` or bare `fno` read system; the read model marks
/// them from this table, never by name matching.
pub fn is_system_sender(sender: &str) -> bool {
    sender == "fno"
        || sender.starts_with("fno/")
        || LEGACY_ALIASES.iter().any(|(legacy, _)| sender == *legacy)
}

/// The bare-`fno` guard (R13): a sender resolving to exactly `fno` is not a
/// name. The refusal names the fix; a caller that ignores it produces a row
/// the fleet cannot answer.
pub fn guard_sender(sender: &str) -> Result<(), String> {
    if sender == "fno" {
        return Err("mail sender 'fno' is not a name: pass --from-name fno/<arm> (the daemon arm or verb that sends), or send from a session".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_names_map_and_the_bare_fno_guard_hold() {
        assert_eq!(canonical("lead-settle"), "fno/lead-settle");
        assert_eq!(canonical("fno-mail-hold"), "fno/mail-hold");
        assert_eq!(canonical("fno/lead-settle"), "fno/lead-settle");
        assert_eq!(canonical("candor"), "candor");
        assert!(is_system_sender("fno/pr-nudge"));
        assert!(is_system_sender("burn-watch"));
        assert!(!is_system_sender("candor"));
        assert!(is_system_sender("fno"));
        assert!(guard_sender("fno").is_err());
        assert!(guard_sender("fno/fleet-incident").is_ok());
        assert!(guard_sender("candor").is_ok());
        let msg = guard_sender("fno").unwrap_err();
        assert!(msg.contains("--from-name fno/<arm>"), "{msg}");
        for arm in SYSTEM_ARMS {
            assert_eq!(system_name(arm), format!("fno/{arm}"));
        }
    }
}
