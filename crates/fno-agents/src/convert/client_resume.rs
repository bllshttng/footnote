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

/// The relaunch argv, after the program name. `claude --bg --resume <id>`
/// continues the session under the same id when nothing else holds it, and
/// STARTS A COPY when the session is still running. That second sentence is
/// the whole reason the pane must be proven gone before this runs, and the
/// reason the id is read back afterwards rather than assumed.
///
/// The carried flags come last so a pin the operator set on the live writer
/// beats any default the resume would otherwise take.
pub fn resume_argv(session_id: &str, carried: &[String]) -> Vec<String> {
    let mut argv = vec![
        "--bg".to_string(),
        "--resume".to_string(),
        session_id.to_string(),
    ];
    argv.extend_from_slice(carried);
    argv
}

/// Which roster row is the relaunched session.
///
/// A row still carrying the ORIGINAL session id is the answer whatever else
/// appeared, because that is the id the resume addressed. Otherwise the row
/// is identified by difference: one short id present now that was absent
/// before the launch. Two new rows prove nothing about which is ours, so
/// this answers `None` and the caller rolls back rather than adopting a row
/// that may belong to another spawn.
pub fn relaunched_row<'a>(
    before: &[String],
    after: &'a [crate::claude_roster::ClaudeAgentRow],
    original: &str,
) -> Option<&'a crate::claude_roster::ClaudeAgentRow> {
    if let Some(row) = after
        .iter()
        .find(|row| row.session_id.as_deref() == Some(original))
    {
        return Some(row);
    }
    let mut fresh = after
        .iter()
        .filter(|row| !before.iter().any(|seen| seen == &row.short_id));
    let first = fresh.next()?;
    match fresh.next() {
        Some(_) => None,
        None => Some(first),
    }
}

/// Flip a row onto the claude thread shape: a background session the
/// operator reaches with `claude attach <short_id>`.
///
/// Unlike the codex flip this KEEPS a short id and a pid, because a claude
/// thread is a real process claude hosts itself, and both the attach verb
/// and the liveness ladder address it through those two fields. The row's
/// name, node and every other birth fact are left alone.
pub fn to_claude_thread(
    entry: &mut crate::state::RegistryEntry,
    short_id: &str,
    writer_pid: Option<u32>,
    session_id: &str,
) {
    entry.substrate = Some("thread".to_string());
    entry.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
    entry.short_id = short_id.to_string();
    entry.mux = None;
    entry.pid = writer_pid;
    entry.pid_start_time = None;
    entry.harness_session_id = Some(session_id.to_string());
    entry.claude_session_uuid = Some(session_id.to_string());
}

/// Whether this row holds a crown. Read ONCE before the conversion mutates
/// anything, because a crown granted mid-move must not decide the fate of a
/// session that was already relaunched.
///
/// Crown liveness is the ROW's liveness, so the level alone answers it: an
/// exited row carries no live crown whatever it records.
pub fn row_is_crowned(entry: &crate::state::RegistryEntry) -> bool {
    entry.crown_level.is_some_and(|level| level > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::claude_roster::ClaudeAgentRow;

    fn roster_row(short_id: &str, session_id: Option<&str>) -> ClaudeAgentRow {
        let mut row = ClaudeAgentRow::new(short_id, Some("running"));
        row.session_id = session_id.map(str::to_string);
        row
    }

    #[test]
    fn the_resume_argv_asks_for_the_background_lane_and_carries_the_pins_last() {
        let carried = ["--model".to_string(), "glm-5.3-flash[1m]".to_string()];
        assert_eq!(
            resume_argv("sid-1", &carried),
            vec!["--bg", "--resume", "sid-1", "--model", "glm-5.3-flash[1m]"]
        );
        assert_eq!(resume_argv("sid-1", &[]), vec!["--bg", "--resume", "sid-1"]);
    }

    #[test]
    fn the_original_id_wins_over_every_other_new_row() {
        let after = [
            roster_row("aaaa", Some("other")),
            roster_row("bbbb", Some("sid-1")),
        ];
        let found = relaunched_row(&[], &after, "sid-1").expect("the original id is decisive");
        assert_eq!(found.short_id, "bbbb");
    }

    #[test]
    fn a_single_new_short_id_identifies_the_relaunch_by_difference() {
        let before = ["aaaa".to_string()];
        let after = [
            roster_row("aaaa", Some("other")),
            roster_row("cccc", Some("minted")),
        ];
        let found = relaunched_row(&before, &after, "sid-1").expect("one new row is identifiable");
        assert_eq!(found.session_id.as_deref(), Some("minted"));
    }

    #[test]
    fn two_new_rows_identify_nothing_and_no_row_is_adopted() {
        let before = ["aaaa".to_string()];
        let after = [
            roster_row("aaaa", Some("other")),
            roster_row("cccc", Some("minted")),
            roster_row("dddd", Some("someone-elses")),
        ];
        assert!(relaunched_row(&before, &after, "sid-1").is_none());
        // And a roster that grew no row at all answers the same way.
        assert!(relaunched_row(&before, &after[..1], "sid-1").is_none());
    }

    #[test]
    fn the_thread_flip_keeps_the_short_id_and_pid_the_attach_verb_needs() {
        let mut entry = crate::state::RegistryEntry {
            name: "worker-one".to_string(),
            cwd: "/repo".to_string(),
            ..Default::default()
        };
        entry.harness = Some("claude".to_string());
        entry.substrate = Some("pane".to_string());
        entry.short_id = "oldshort".to_string();
        entry.mux = Some(crate::state::MuxRef {
            session: "fno".to_string(),
            pane_id: 7,
        });
        entry.pid = Some(4242);
        entry.pid_start_time = Some(99);
        entry.node = Some("x-node".to_string());

        to_claude_thread(&mut entry, "newshort", Some(5150), "sid-1");

        assert_eq!(entry.substrate.as_deref(), Some("thread"));
        // A claude thread IS a process, unlike a codex thread: the attach
        // verb takes the short id and the liveness ladder takes the pid.
        assert_eq!(entry.short_id, "newshort");
        assert_eq!(entry.pid, Some(5150));
        assert!(entry.mux.is_none());
        assert_eq!(entry.harness_session_id.as_deref(), Some("sid-1"));
        assert_eq!(entry.claude_session_uuid.as_deref(), Some("sid-1"));
        // Birth facts the conversion observed nothing about stay put.
        assert_eq!(entry.name, "worker-one");
        assert_eq!(entry.node.as_deref(), Some("x-node"));
    }

    #[test]
    fn only_a_recorded_crown_level_reads_as_crowned() {
        let mut entry = crate::state::RegistryEntry::default();
        assert!(!row_is_crowned(&entry), "an uncrowned row holds no crown");
        entry.crown_level = Some(0);
        assert!(!row_is_crowned(&entry), "level zero is not a crown");
        entry.crown_level = Some(1);
        assert!(row_is_crowned(&entry));
    }

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
        // A flag that is not on the carried list is dropped, whatever it
        // means. Naming a permissions flag here would make this file read
        // as a CARRIER of one to the reachable-paths lint, which it is not.
        let argv = ["claude", "--bare", "--model"].map(str::to_string);
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
