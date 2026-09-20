//! The client-resume strategy: the pane stops, the client relaunches the
//! session detached.
//!
//! Two measurements disagree about whether claude keeps its id here.
//! `reentry.rs` records that on 2.1.272 a STOPPED session continues under
//! the same id, while the Python spawn door says a revival forks a new one.
//! They describe different doors, and a conversion cannot afford to guess
//! between them. So this strategy never promises the id: it reads the
//! resumed id back off the roster and compares.
//!
//! A differing id without `--allow-new-id` is a rollback, not a shrug. A
//! differing id on a CROWNED row is refused even with the flag, because
//! moving a crown to a new session id is succession - a separate operation
//! with its own attribution.

/// What to do with the id the relaunched session actually reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdVerdict {
    /// Same id. The conversion completes silently.
    Kept,
    /// A new id the caller authorized on an uncrowned row. The old id is
    /// recorded as the related id and the receipt says so out loud.
    AcceptedNew { old: String, new: String },
    /// Stop the new session and put the row back.
    RollBack(String),
}

/// Compare the id read back from the roster against the one converted.
///
/// `crowned` is read ONCE before any mutation, so a crown granted mid-move
/// cannot flip the answer after the session was already relaunched.
pub fn id_verdict(
    original: &str,
    read_back: Option<&str>,
    allow_new_id: bool,
    crowned: bool,
) -> IdVerdict {
    let Some(read_back) = read_back.filter(|id| !id.is_empty()) else {
        // No id at all is not "a new id": nothing proves the relaunched
        // session is this session, so it is never adopted.
        return IdVerdict::RollBack(
            "the relaunched session reported no id, so it cannot be proven to be this session"
                .to_string(),
        );
    };
    if read_back == original {
        return IdVerdict::Kept;
    }
    if crowned {
        return IdVerdict::RollBack(format!(
            "the relaunch minted {read_back} rather than {original}, and this row is crowned; \
             moving a crown to a new session id is succession, not conversion"
        ));
    }
    if !allow_new_id {
        return IdVerdict::RollBack(format!(
            "the relaunch minted {read_back} rather than {original}. Converting would change the \
             session id, which no caller authorized; re-run with --allow-new-id to accept it"
        ));
    }
    IdVerdict::AcceptedNew {
        old: original.to_string(),
        new: read_back.to_string(),
    }
}

/// The launch axes carried from the LIVE writer's own argv, never from a
/// default. A conversion that relaunched on the account default model would
/// silently downgrade a session the operator had pinned.
///
/// Only flags with a measured spelling on the claude resume arm are carried;
/// anything else on that argv belongs to the pane lane and is dropped rather
/// than guessed at.
pub fn carried_flags(argv: &[String]) -> Vec<String> {
    const CARRIED: [&str; 4] = ["--model", "--effort", "--permission-mode", "--add-dir"];
    let mut out = Vec::new();
    let mut iter = argv.iter().peekable();
    while let Some(arg) = iter.next() {
        if let Some((flag, value)) = arg.split_once('=') {
            if CARRIED.contains(&flag) && !value.is_empty() {
                out.push(flag.to_string());
                out.push(value.to_string());
            }
            continue;
        }
        if !CARRIED.contains(&arg.as_str()) {
            continue;
        }
        // A flag whose value is missing or is itself a flag carries nothing:
        // appending a bare `--model` to the relaunch argv makes the launch
        // fail rather than keep the pin.
        match iter.peek() {
            Some(value) if !value.starts_with('-') => {
                out.push(arg.clone());
                out.push((*value).clone());
                iter.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_id_is_kept() {
        assert_eq!(
            id_verdict("sid", Some("sid"), false, false),
            IdVerdict::Kept
        );
        // The flags are irrelevant when the id survives.
        assert_eq!(id_verdict("sid", Some("sid"), true, true), IdVerdict::Kept);
    }

    #[test]
    fn a_new_id_rolls_back_unless_it_was_authorized() {
        let verdict = id_verdict("old", Some("new"), false, false);
        let IdVerdict::RollBack(reason) = verdict else {
            panic!("an unauthorized new id must roll back");
        };
        // Both ids are named: the operator needs to know what it minted.
        assert!(reason.contains("old"), "{reason}");
        assert!(reason.contains("new"), "{reason}");
        assert!(reason.contains("--allow-new-id"), "{reason}");

        assert_eq!(
            id_verdict("old", Some("new"), true, false),
            IdVerdict::AcceptedNew {
                old: "old".to_string(),
                new: "new".to_string()
            }
        );
    }

    #[test]
    fn a_crowned_row_refuses_a_new_id_even_when_it_is_authorized() {
        let IdVerdict::RollBack(reason) = id_verdict("old", Some("new"), true, true) else {
            panic!("a crowned row must roll back");
        };
        assert!(reason.contains("succession"), "{reason}");
    }

    #[test]
    fn no_id_at_all_rolls_back_rather_than_counting_as_new() {
        for read_back in [None, Some("")] {
            let IdVerdict::RollBack(reason) = id_verdict("old", read_back, true, false) else {
                panic!("an absent id must roll back even with the flag");
            };
            assert!(reason.contains("no id"), "{reason}");
        }
    }

    #[test]
    fn the_live_writers_pins_are_carried_in_both_spellings() {
        let argv = [
            "claude",
            "--model",
            "glm-5.3-flash[1m]",
            "--effort=high",
            "--permission-mode",
            "acceptEdits",
            "--add-dir",
            "/repo/extra",
        ]
        .map(str::to_string);
        assert_eq!(
            carried_flags(&argv),
            vec![
                "--model",
                "glm-5.3-flash[1m]",
                "--effort",
                "high",
                "--permission-mode",
                "acceptEdits",
                "--add-dir",
                "/repo/extra"
            ]
        );
    }

    #[test]
    fn a_pane_only_flag_is_dropped_and_a_valueless_pin_carries_nothing() {
        // `--dangerously-skip-permissions` belongs to the pane lane; the
        // resume arm has no measured spelling for it here.
        let argv = ["claude", "--dangerously-skip-permissions", "--model"].map(str::to_string);
        assert!(
            carried_flags(&argv).is_empty(),
            "{:?}",
            carried_flags(&argv)
        );

        // A bare `--model` followed by another flag carries nothing: a
        // dangling flag makes the relaunch fail rather than keep the pin.
        let argv = ["claude", "--model", "--add-dir", "/repo"].map(str::to_string);
        assert_eq!(carried_flags(&argv), vec!["--add-dir", "/repo"]);

        // An empty equals value is not a value.
        let argv = ["claude", "--model="].map(str::to_string);
        assert!(carried_flags(&argv).is_empty());
    }
}
