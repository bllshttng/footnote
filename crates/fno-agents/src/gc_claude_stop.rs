//! The claude stop arm of the retirement sweep, moved out of `gc_sweep`
//! (x-d8bc change 1a) so the file stays under its shrink-only cap.
//!
//! The stop confirmation polls two witnesses (change 1b): the daemon
//! `roster.json` and the `claude agents` state. A single post-stop roster
//! read raced the supervisor's own teardown - `claude stop` exits 0, the
//! roster still lists the session, and the row held as `stop refused` for
//! another tick. Both witnesses answer the same question, so the first one
//! that says gone confirms.

use crate::state;

/// Stop a claude row's session before the row drops. The roster is the exited
/// proof: a session the live roster no longer lists is already gone, and
/// running `claude stop` on it would fail on every future sweep, wedging the
/// row in `stop_refused` forever. A roster read that FAILS holds the row -
/// a torn read is not an exited proof. An unreachable session id (no short
/// id, no session id) holds too: the sweep cannot reach the session, so it
/// must not drop the row and orphan the sideline entry. A successful stop
/// EXIT is a receipt, not a proof: after the stop, absence is confirmed by
/// polling both witnesses.
pub(crate) fn stop_claude_confirmed(e: &state::RegistryEntry) -> bool {
    let Some(short) = e
        .transport_short()
        .map(str::to_string)
        .or_else(|| roster_short(&e.harness_session_id))
    else {
        return false;
    };
    let sid = e.harness_session_id.as_deref();
    if roster_lists(&short, sid) == Some(false) {
        return true;
    }
    let stopped = {
        let short = short.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| {
                    rt.block_on(async {
                        matches!(
                            crate::lifecycle_child::bounded_claude_stop(&short, std::time::Duration::from_secs(15))
                                .await,
                            Ok(Ok(output)) if output.status.success()
                        )
                    })
                })
                .unwrap_or(false)
        })
        .join()
        .unwrap_or(false)
    };
    if !stopped {
        return false;
    }
    stop_claude_confirmed_with(
        &short,
        sid,
        &|_| true, // the real stop already ran; the core must not run it twice
        &roster_lists,
        &agents_roster_state,
        &std::thread::sleep,
        15,
    )
}

/// The injectable stop-confirmation core (x-d8bc change 1b). The stop ran
/// when `stop` answers true; after it, poll up to `polls` times, one second
/// apart, and confirm on the first poll where the roster stops listing the
/// session or the `claude agents` state reads terminal. A stop that did not
/// run returns false with no poll.
pub(crate) fn stop_claude_confirmed_with(
    short: &str,
    sid: Option<&str>,
    stop: &dyn Fn(&str) -> bool,
    listed: &dyn Fn(&str, Option<&str>) -> Option<bool>,
    agents_state: &dyn Fn(&str) -> Option<String>,
    sleep: &dyn Fn(std::time::Duration),
    polls: u32,
) -> bool {
    // The early return stands inside the core: a session the roster already
    // stopped listing is confirmed without running the stop.
    if listed(short, sid) == Some(false) {
        return true;
    }
    if !stop(short) {
        return false;
    }
    for _ in 0..polls {
        sleep(std::time::Duration::from_secs(1));
        if listed(short, sid) == Some(false) {
            return true;
        }
        if agents_state(short)
            .as_deref()
            .is_some_and(crate::claude_roster::is_terminal_roster_state)
        {
            return true;
        }
    }
    false
}

/// The second witness: the terminal-state reader over one live
/// `claude agents --json --all` snapshot. A read that fails answers None -
/// an unread witness never confirms.
fn agents_roster_state(short: &str) -> Option<String> {
    crate::claude_roster::read_all_agents()
        .find(short)
        .and_then(|row| row.state.clone())
}

/// Whether the live roster still lists the session, by short id or session
/// id. `None` when the roster cannot be read: a torn read is not an exited
/// proof in either direction.
fn roster_lists(short: &str, sid: Option<&str>) -> Option<bool> {
    roster_lists_in(&crate::claude_roster::default_roster_path(), short, sid)
}

fn roster_lists_in(path: &std::path::Path, short: &str, sid: Option<&str>) -> Option<bool> {
    let roster = crate::claude_roster::ClaudeRoster::load(path).ok()?;
    Some(roster.find(short).is_some() || sid.is_some_and(|sid| roster.find(sid).is_some()))
}

/// Resolve a claude short id from the live roster by session id.
fn roster_short(session_id: &Option<String>) -> Option<String> {
    let sid = session_id.as_deref()?.trim();
    if sid.is_empty() {
        return None;
    }
    let roster = crate::claude_roster::ClaudeRoster::load_default().ok()?;
    roster.find(sid).map(|w| w.short_id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-worker roster in the confirmed live shape (the shape the
    /// claude_roster parse test accepts).
    const ONE_WORKER_ROSTER: &str = r#"{
  "proto": 1,
  "supervisorPid": 4242,
  "updatedAt": 1751049130000,
  "workers": {
    "ee99ff00": {
      "pid": 5002,
      "sessionId": "ee99ff00-7777-8888-9999-aaaabbbbcccc",
      "ptySock": "/tmp/cc-daemon-501/deadbeef/pty/ee99ff00.pty.sock",
      "startedAt": 1751049050000,
      "attempt": 2,
      "cwd": "/Users/x/code/other",
      "dispatch": {"source": "fleet"}
    }
  }
}"#;

    fn roster_file(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-roster-lists-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roster.json");
        std::fs::write(&path, ONE_WORKER_ROSTER).unwrap();
        path
    }

    #[test]
    fn roster_lists_by_short_id_session_id_or_not_at_all() {
        let path = roster_file("listed");
        assert_eq!(
            roster_lists_in(&path, "ee99ff00", None),
            Some(true),
            "listed by short id"
        );
        assert_eq!(
            roster_lists_in(&path, "ee99ff00-7777-8888-9999-aaaabbbbcccc", None),
            Some(true),
            "listed by session id"
        );
        assert_eq!(
            roster_lists_in(&path, "deadbeef", None),
            Some(false),
            "an unknown session is not listed"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn torn_roster_read_is_unknown_not_gone() {
        let dir = std::env::temp_dir().join(format!("fno-roster-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("roster.json")).unwrap();
        assert_eq!(
            roster_lists_in(&dir.join("roster.json"), "ee99ff00", None),
            None,
            "a torn read is not an exited proof"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC2-HP: the stop exits 0, the roster still lists the session on the
    /// first read, and drops on the third: confirmed.
    #[test]
    fn a_roster_that_drops_after_the_stop_confirms() {
        let reads = std::cell::Cell::new(0u32);
        let listed = |_: &str, _: Option<&str>| {
            let n = reads.get();
            reads.set(n + 1);
            Some(n < 2)
        };
        let confirmed = stop_claude_confirmed_with(
            "ee99ff00",
            None,
            &|_| true,
            &listed,
            &|_| None,
            &|_| {},
            15,
        );
        assert!(confirmed, "the roster drop is the confirmation");
    }

    /// AC2-ERR: both witnesses stay live past the poll bound - unconfirmed,
    /// and the polls actually ran.
    #[test]
    fn witnesses_staying_live_exhaust_the_polls() {
        let ran = std::cell::Cell::new(false);
        let polls = std::cell::Cell::new(0u32);
        let stop = |_: &str| {
            ran.set(true);
            true
        };
        let listed = |_: &str, _: Option<&str>| {
            if ran.get() {
                polls.set(polls.get() + 1);
            }
            Some(true)
        };
        let confirmed = stop_claude_confirmed_with(
            "ee99ff00",
            None,
            &stop,
            &listed,
            &|_| Some("working".into()),
            &|_| {},
            3,
        );
        assert!(!confirmed, "a live session never confirms");
        assert_eq!(polls.get(), 3, "every post-stop poll consulted the roster");
    }

    /// AC2-ERR: a stop that exits non-zero holds without polling.
    #[test]
    fn a_failed_stop_polls_nothing() {
        let ran = std::cell::Cell::new(false);
        let polls = std::cell::Cell::new(0u32);
        let stop = |_: &str| {
            ran.set(true);
            false
        };
        let listed = |_: &str, _: Option<&str>| {
            if ran.get() {
                polls.set(polls.get() + 1);
            }
            Some(true)
        };
        let confirmed =
            stop_claude_confirmed_with("ee99ff00", None, &stop, &listed, &|_| None, &|_| {}, 5);
        assert!(!confirmed);
        assert_eq!(polls.get(), 0, "no poll after a failed stop");
    }

    /// AC2-EDGE: the roster still lists the session but `claude agents`
    /// already reads a terminal state - confirmed on that state.
    #[test]
    fn a_terminal_agents_state_confirms_without_a_roster_drop() {
        let confirmed = stop_claude_confirmed_with(
            "ee99ff00",
            None,
            &|_| true,
            &|_, _| Some(true),
            &|_| Some("stopped".into()),
            &|_| {},
            15,
        );
        assert!(confirmed, "the terminal state is the confirmation");
    }

    /// The early return stands: a session the roster already stopped
    /// listing is confirmed without running the stop.
    #[test]
    fn an_absent_session_confirms_before_the_stop() {
        let ran = std::cell::Cell::new(false);
        let confirmed = stop_claude_confirmed_with(
            "ee99ff00",
            None,
            &|_| {
                ran.set(true);
                true
            },
            &|_, _| Some(false),
            &|_| None,
            &|_| {},
            15,
        );
        assert!(confirmed);
        assert!(!ran.get(), "the stop never ran");
    }
}
