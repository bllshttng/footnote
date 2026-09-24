//! The claude sessions store, read as indexes: the socket index the
//! liveness ladder's socket rung pays one walk for, and the single-scan
//! liveness reads built on it.

use crate::claude_ask::ClaudeHome;
use crate::client_verbs::{row_liveness_with_indexed, RowLiveness};
use crate::truth_probe::family1_truth_state;
use serde_json::Value;

/// Silence on every rung is `Unknown`. The ladder NEVER returns `Dead`: only
/// a positive death proof may, and absence never is one. A missing or
/// unreadable transcript falls through the third rung and lands `Unknown`.
///
/// Resume keeps its own inline copy of rungs 1 and 3 because it must also
/// read the truth VALUE (`done`/`stalled` route the relaunch arm, which the
/// verdict type has no room for); its behavior is pinned identical by tests,
/// except that the ladder's rung 3 is silent on an exit-proven row.
pub fn row_liveness(entry: &crate::state::RegistryEntry, claude_home: &ClaudeHome) -> RowLiveness {
    let sockets = sessions_socket_index(claude_home);
    row_liveness_indexed(entry, &sockets)
}

/// One scan of claude's session records: `jobId -> messagingSocketPath` for
/// every live-shaped bg session file, first-sorted-wins (the same pick
/// `locate_session` makes). The socket rung's cost is this walk, so a sweep
/// probing N rows pays it once, not N times.
pub fn sessions_socket_index(
    claude_home: &ClaudeHome,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for path in claude_home.session_records() {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if v.get("kind").and_then(Value::as_str) != Some("bg") {
            continue;
        }
        let (Some(job), Some(sock)) = (
            v.get("jobId").and_then(Value::as_str),
            v.get("messagingSocketPath").and_then(Value::as_str),
        ) else {
            continue;
        };
        if job.is_empty() || sock.is_empty() || out.contains_key(job) {
            continue;
        }
        out.insert(job.to_string(), sock.to_string());
    }
    out
}

/// [`row_liveness`] against a prebuilt [`sessions_socket_index`] - the form
/// a sweep uses, so N probed rows cost one dir scan. The truth read is the
/// production one.
pub(crate) fn row_liveness_indexed(
    entry: &crate::state::RegistryEntry,
    sockets: &std::collections::HashMap<String, String>,
) -> RowLiveness {
    row_liveness_with_indexed(entry, sockets, None, family1_truth_state)
}

/// Which live claude process holds `session_id`, read from claude's own
/// per-process records (one `<pid>.json` per running process under each
/// root's sessions dir, removed on clean exit). `create_ms` answers the
/// pid's epoch create time, so a record's
/// `procStart` proves the incarnation: a pid whose create time disagrees
/// with the record is a recycle, and a pid with no create time is a crash
/// leftover. Both are skipped, never named as the holder.
pub(crate) fn session_record_holder(
    dirs: &[std::path::PathBuf],
    session_id: &str,
    create_ms: &dyn Fn(u32) -> Option<i64>,
) -> crate::pane_stop::SessionHolder {
    use crate::pane_stop::SessionHolder;
    if session_id.is_empty() {
        return SessionHolder::Unmeasured("row carries no claude session id".into());
    }
    let short = session_id.chars().take(8).collect::<String>();
    let mut verified: Vec<u32> = Vec::new();
    let mut verified_interactive: Option<u32> = None;
    let mut verified_bg: Option<(String, u32)> = None;
    let mut unverified: Option<(u32, String)> = None;
    let mut dirs_read = 0usize;
    let mut first_dir = String::new();
    for dir in dirs {
        if first_dir.is_empty() {
            first_dir = dir.display().to_string();
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        dirs_read += 1;
        let mut paths: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            // The dir holds these records plus session transcripts
            // (measured 2026-09-21: 12,851 .md files beside 22 records).
            // Skip everything else before reading.
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if v.get("sessionId").and_then(Value::as_str) != Some(session_id) {
                continue;
            }
            let Some(pid_val) = v.get("pid").and_then(Value::as_u64) else {
                continue;
            };
            if pid_val == 0 || pid_val == 1 || pid_val > i32::MAX as u64 {
                continue;
            }
            let pid = pid_val as u32;
            let Some(ms) = create_ms(pid) else {
                continue; // the process is gone: a crash leftover
            };
            let Some(raw_start) = v.get("procStart").and_then(Value::as_str) else {
                unverified.get_or_insert((pid, "(missing)".into()));
                continue;
            };
            // claude space-pads single-digit days ("Sep  1"); collapse the
            // runs so one format parses both shapes.
            let collapsed = raw_start.split_whitespace().collect::<Vec<_>>().join(" ");
            match chrono::NaiveDateTime::parse_from_str(&collapsed, "%a %b %d %H:%M:%S %Y") {
                Ok(start) => {
                    let record_s = start.and_utc().timestamp();
                    if (ms / 1000 - record_s).abs() <= 1 {
                        if !verified.contains(&pid) {
                            verified.push(pid);
                        }
                        if v.get("kind").and_then(Value::as_str) == Some("interactive") {
                            verified_interactive.get_or_insert(pid);
                        } else if verified_bg.is_none() {
                            let job = v
                                .get("jobId")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_string();
                            verified_bg = Some((job, pid));
                        }
                    }
                    // else: the pid slot changed hands since the record -
                    // a recycled pid is never the holder.
                }
                Err(_) => {
                    unverified.get_or_insert((pid, raw_start.to_string()));
                }
            }
        }
    }
    if verified.len() >= 2 {
        let names = verified
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return SessionHolder::Held {
            pid: None,
            proven: true,
            why: format!("pids {names} both hold claude session {short}"),
        };
    }
    if let Some(pid) = verified_interactive {
        return SessionHolder::Held {
            pid: Some(pid),
            proven: true,
            why: format!(
                "pid {pid} holds claude session {short} (interactive record, start time matches)"
            ),
        };
    }
    if let Some((job, pid)) = verified_bg {
        return SessionHolder::Held {
            pid: None,
            proven: true,
            why: format!(
                "claude bg job {job} (pid {pid}) holds session {short}; a bg job stops through claude, not a signal"
            ),
        };
    }
    if let Some((pid, raw_start)) = unverified {
        return SessionHolder::Unmeasured(format!(
            "pid {pid} records session {short} but its procStart {raw_start:?} does not parse; \
             refusing to name an unverified holder"
        ));
    }
    if dirs_read == 0 {
        return SessionHolder::Unmeasured(format!("claude sessions dir unreadable: {first_dir}"));
    }
    SessionHolder::NotHeld(format!(
        "no live claude process records session {short} ({dirs_read} record dir{} read)",
        if dirs_read == 1 { "" } else { "s" }
    ))
}

/// The `reentry-plan holder <session-id>...` action body: one holder read
/// per id over the ambient record dirs, one JSON object on stdout keyed by
/// session id. Exit 0 whenever it printed; the caller reads `held`, never
/// the exit code.
pub fn run_holder_action(session_ids: &[String]) -> i32 {
    let dirs = session_record_dirs();
    let mut out = serde_json::Map::new();
    for sid in session_ids {
        let answer = session_record_holder(&dirs, sid, &|pid| {
            crate::claims::process_create_time_ms(pid as i32)
        });
        out.insert(sid.clone(), holder_json(&answer));
    }
    println!("{}", Value::Object(out));
    0
}

/// The wire shape one holder read prints: `held` maps Held|NotHeld|Unmeasured
/// to true|false|null, `proven` rides only a verified match, and `pid` is
/// named when the read knows one. Pure so tests pin it without a filesystem.
fn holder_json(holder: &crate::pane_stop::SessionHolder) -> Value {
    let (held, proven, pid, why) = match holder {
        crate::pane_stop::SessionHolder::Held { pid, proven, why } => (
            Value::from(true),
            Value::from(*proven),
            pid.map(|p| Value::from(p)).unwrap_or(Value::Null),
            Value::from(why.as_str()),
        ),
        crate::pane_stop::SessionHolder::NotHeld(why) => (
            Value::from(false),
            Value::from(false),
            Value::Null,
            Value::from(why.as_str()),
        ),
        crate::pane_stop::SessionHolder::Unmeasured(why) => (
            Value::Null,
            Value::from(false),
            Value::Null,
            Value::from(why.as_str()),
        ),
    };
    serde_json::json!({"held": held, "proven": proven, "pid": pid, "why": why})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_stop::SessionHolder;
    use crate::state::{InsideLegReport, InsideLegState, RegistryEntry};
    use crate::AgentStatus;

    fn claude_row(status: AgentStatus) -> RegistryEntry {
        RegistryEntry {
            name: "row".into(),
            legacy_provider: "claude".into(),
            status,
            claude_session_uuid: Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into()),
            ..Default::default()
        }
    }

    fn ladder(entry: &RegistryEntry) -> RowLiveness {
        let no_sockets = std::collections::HashMap::new();
        row_liveness_with_indexed(entry, &no_sockets, None, |_| Some("working".into()))
    }

    // Measured 2026-09-14: 7 of 23 stored-exited rows served alive because a
    // transcript tail still read `working` long after the worker stopped.
    #[test]
    fn a_working_transcript_does_not_outrank_a_recorded_exit() {
        assert_eq!(
            ladder(&claude_row(AgentStatus::Exited)),
            RowLiveness::Unknown
        );
        let mut stamped = claude_row(AgentStatus::Orphaned);
        stamped.exited_at = Some("2026-08-01T00:00:02Z".into());
        assert_eq!(ladder(&stamped), RowLiveness::Unknown);
    }

    #[test]
    fn a_working_transcript_still_proves_a_row_with_no_exit_proof() {
        assert_eq!(ladder(&claude_row(AgentStatus::Live)), RowLiveness::Alive);
        // A revive that set Live but kept an old stamp is not an exit proof.
        let mut revived = claude_row(AgentStatus::Live);
        revived.exited_at = Some("2026-08-01T00:00:02Z".into());
        assert_eq!(ladder(&revived), RowLiveness::Alive);
    }

    #[test]
    fn a_heartbeat_past_the_exit_stamp_still_resurrects_an_exited_row() {
        let mut e = claude_row(AgentStatus::Exited);
        e.exited_at = Some("2026-08-01T00:00:02Z".into());
        e.inside_leg = Some(InsideLegReport {
            state: InsideLegState::Working,
            seq: 3,
            reason: None,
            received_at: "2026-08-01T00:00:30Z".into(),
            ttl_ms: None,
        });
        assert_eq!(ladder(&e), RowLiveness::Alive);
    }

    // ──: session records name the live holder ────────────────────

    const T0: &str = "Mon Sep 21 16:27:44 2026"; // epoch 1_790_008_064
    const T0_MS: i64 = 1_790_008_064_000;

    fn record_json(pid: u32, session: &str, proc_start: &str, kind: &str) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"{session}","procStart":"{proc_start}","kind":"{kind}"}}"#
        )
    }

    fn stage(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

    /// AC10-HP: one interactive record whose pid is live and whose
    /// procStart matches that pid's create time names the holder.
    #[test]
    fn x1530_ac10_live_interactive_record_names_the_holder() {
        let tmp = tempfile::tempdir().unwrap();
        stage(
            &tmp.path().join("sessions"),
            "24896.json",
            &record_json(24896, SID, T0, "interactive"),
        );
        let dirs = vec![tmp.path().join("sessions")];
        let answer = session_record_holder(&dirs, SID, &|pid| {
            if pid == 24896 {
                Some(T0_MS)
            } else {
                None
            }
        });
        match answer {
            SessionHolder::Held {
                pid: Some(24896),
                proven: true,
                why,
            } => assert!(
                why.contains("interactive record, start time matches"),
                "{why}"
            ),
            other => panic!("expected Held pid 24896, got {other:?}"),
        }
    }

    /// A live bg record under an account root feeds the socket index from a
    /// ClaudeHome carrying that root, and the holder read walks the same
    /// root's sessions dir.
    #[test]
    fn x1530_ac8_socket_index_and_holder_read_account_roots() {
        let home = tempfile::tempdir().unwrap();
        let acct = tempfile::tempdir().unwrap();
        let sessions = acct.path().join("sessions");
        stage(
            &sessions,
            "4242.json",
            r#"{"jobId":"feedc0de","kind":"bg","messagingSocketPath":"/tmp/acct-live.sock","sessionId":"bbbb2222-1111-2222-3333-444444444444","pid":4242}"#,
        );
        stage(
            &sessions,
            "24896.json",
            &record_json(24896, SID, T0, "interactive"),
        );
        let ch = ClaudeHome::at(home.path()).with_extra_roots([acct.path().to_path_buf()]);

        let sockets = sessions_socket_index(&ch);
        assert_eq!(
            sockets.get("feedc0de").map(String::as_str),
            Some("/tmp/acct-live.sock")
        );

        let answer = session_record_holder(&ch.sessions_dirs(), SID, &|pid| {
            if pid == 24896 {
                Some(T0_MS)
            } else {
                None
            }
        });
        match answer {
            SessionHolder::Held {
                pid: Some(24896),
                proven: true,
                ..
            } => {}
            other => panic!("expected Held pid 24896 via account root, got {other:?}"),
        }
    }

    /// Positive control: the reader names THIS test process from a real
    /// create-time probe, so the parser and the tolerance are exercised
    /// against a real clock, not only staged closures.
    #[test]
    fn x1530_ac10_control_the_readers_own_pid_is_found_by_a_real_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let create_ms =
            crate::claims::process_create_time_ms(me as i32).expect("this test process is alive");
        let proc_start = chrono::DateTime::from_timestamp(create_ms / 1000, 0)
            .unwrap()
            .format("%a %b %d %H:%M:%S %Y")
            .to_string();
        stage(
            &tmp.path().join("sessions"),
            &format!("{me}.json"),
            &record_json(me, SID, &proc_start, "interactive"),
        );
        let dirs = vec![tmp.path().join("sessions")];
        let answer = session_record_holder(&dirs, SID, &|pid| {
            crate::claims::process_create_time_ms(pid as i32)
        });
        match answer {
            SessionHolder::Held {
                pid: Some(found),
                proven: true,
                ..
            } => assert_eq!(found, me),
            other => panic!("expected Held naming pid {me}, got {other:?}"),
        }
    }

    /// AC11-ERR: a gone pid, a recycled pid (procStart an hour off), an
    /// unparseable procStart on a live pid, and an unreadable dir each
    /// answer their own shape.
    #[test]
    fn x1530_ac11_gone_recycled_unparseable_and_unreadable_each_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        stage(
            &sessions,
            "101.json",
            &record_json(101, SID, T0, "interactive"),
        );
        stage(
            &sessions,
            "102.json",
            &record_json(102, SID, T0, "interactive"),
        );
        stage(
            &sessions,
            "103.json",
            &record_json(103, SID, "not a date", "interactive"),
        );
        let dirs = vec![sessions.clone()];
        let probe = |pid: u32| match pid {
            101 => None,                    // gone: crash leftover
            102 => Some(T0_MS + 3_600_000), // recycled: an hour off
            103 => Some(T0_MS),             // live, procStart unparseable
            _ => None,
        };
        let answer = session_record_holder(&dirs, SID, &probe);
        assert!(
            matches!(
                &answer,
                SessionHolder::Unmeasured(text)
                    if text.contains("pid 103") && text.contains("not a date")
            ),
            "{answer:?}"
        );

        // And with the unparseable record removed, the dir holds only a
        // gone pid and a recycled pid: NotHeld, with the dir counted.
        std::fs::remove_file(sessions.join("103.json")).unwrap();
        let answer = session_record_holder(&dirs, SID, &probe);
        match answer {
            SessionHolder::NotHeld(why) => assert!(why.contains("(1 record dir read)"), "{why}"),
            other => panic!("expected NotHeld, got {other:?}"),
        }
    }

    /// AC11-ERR: read_dir failing on every dir answers Unmeasured naming
    /// the first dir.
    #[test]
    fn x1530_ac11_unreadable_dir_answers_unmeasured() {
        let answer = session_record_holder(
            &[std::path::PathBuf::from("/nonexistent-x1530/sessions")],
            SID,
            &|_| None,
        );
        assert!(matches!(
            &answer,
            SessionHolder::Unmeasured(text) if text.contains("/nonexistent-x1530/sessions")
        ));
    }

    /// AC12-EDGE: a live bg record answers Held with no pid - a bg job
    /// stops through claude, not a signal.
    #[test]
    fn x1530_ac12_bg_record_holds_without_a_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        stage(
            &sessions,
            "500.json",
            &format!(
                r#"{{"pid":500,"sessionId":"{SID}","procStart":"{T0}","kind":"bg","jobId":"job-7"}}"#
            ),
        );
        let dirs = vec![sessions];
        let answer = session_record_holder(&dirs, SID, &|pid| {
            if pid == 500 {
                Some(T0_MS)
            } else {
                None
            }
        });
        match answer {
            SessionHolder::Held {
                pid: None,
                proven: true,
                why,
            } => assert!(
                why.contains("bg job job-7 (pid 500)") && why.contains("stops through claude"),
                "{why}"
            ),
            other => panic!("expected Held without pid, got {other:?}"),
        }
    }

    /// AC12-EDGE: a .md file and a record for another session are both
    /// ignored before the pid probe.
    #[test]
    fn x1530_ac12_md_files_and_other_sessions_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        stage(&sessions, "transcript.md", "not a record");
        stage(
            &sessions,
            "600.json",
            &record_json(600, "bbbbbbbb-9999", T0, "interactive"),
        );
        stage(&sessions, "no-pid.json", r#"{"sessionId":"aaaaaaaa-0000"}"#);
        let dirs = vec![sessions];
        // create_ms panics if asked: nothing matching may reach the probe.
        let answer = session_record_holder(&dirs, SID, &|_| {
            panic!("no ignored record may reach the create-time probe")
        });
        assert!(matches!(answer, SessionHolder::NotHeld(_)));
    }

    /// AC12-EDGE: a space-padded single-digit day parses to the right
    /// epoch second.
    #[test]
    fn x1530_ac12_space_padded_day_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        stage(
            &sessions,
            "700.json",
            &record_json(700, SID, "Tue Sep  1 03:04:05 2026", "interactive"),
        );
        let dirs = vec![sessions];
        let answer = session_record_holder(&dirs, SID, &|pid| {
            if pid == 700 {
                Some(1_788_231_845_000) // 2026-09-01T03:04:05Z
            } else {
                None
            }
        });
        assert!(matches!(
            answer,
            SessionHolder::Held {
                pid: Some(700),
                proven: true,
                ..
            }
        ));
    }

    /// Specimen measured 2026-09-23: a claude thread killed -9 and stopped
    /// in fno, then resumed outside fno with `claude --resume`. The resumed
    /// process's own record (cwd and display fields dropped) proves the
    /// authority the registry falsifier must ask: kind interactive,
    /// procStart equal to that pid's create time.
    #[test]
    fn a_session_resumed_outside_fno_is_held_by_its_record() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        stage(
            &sessions,
            "65491.json",
            r#"{"pid":65491,"sessionId":"bb2731c9-ad46-4303-a80d-152c68e91a4e","startedAt":1790199219816,"procStart":"Wed Sep 23 21:33:39 2026","version":"2.1.281","kind":"interactive","entrypoint":"cli","pidDomain":"darwin"}"#,
        );
        let dirs = vec![sessions];
        let answer = session_record_holder(&dirs, "bb2731c9-ad46-4303-a80d-152c68e91a4e", &|pid| {
            if pid == 65491 {
                Some(1_790_199_219_000)
            } else {
                None
            }
        });
        assert!(
            matches!(answer, SessionHolder::Held { proven: true, .. }),
            "{answer:?}"
        );
    }

    /// holder_json: Held prints held true with the proven flag and the pid
    /// when the read knows one.
    #[test]
    fn holder_json_maps_held_to_true_with_proven_and_pid() {
        let v = holder_json(&SessionHolder::Held {
            pid: Some(65491),
            proven: true,
            why: "pid 65491 holds claude session bb2731c9".into(),
        });
        assert_eq!(v["held"], serde_json::json!(true));
        assert_eq!(v["proven"], serde_json::json!(true));
        assert_eq!(v["pid"], serde_json::json!(65491));
        assert!(v["why"].as_str().unwrap().contains("65491"));
    }

    /// holder_json: a Held bg read carries no pid and still reads held true.
    #[test]
    fn holder_json_maps_a_bg_held_without_a_pid() {
        let v = holder_json(&SessionHolder::Held {
            pid: None,
            proven: true,
            why: "bg job".into(),
        });
        assert_eq!(v["held"], serde_json::json!(true));
        assert_eq!(v["proven"], serde_json::json!(true));
        assert!(v["pid"].is_null());
    }

    /// holder_json: NotHeld and Unmeasured never read held true - Unmeasured
    /// prints held null, and neither is a cancellation.
    #[test]
    fn holder_json_never_reads_unmeasured_or_not_held_as_held() {
        let not_held = holder_json(&SessionHolder::NotHeld("no record".into()));
        assert_eq!(not_held["held"], serde_json::json!(false));
        assert_eq!(not_held["proven"], serde_json::json!(false));
        assert!(not_held["pid"].is_null());

        let unmeasured = holder_json(&SessionHolder::Unmeasured("procStart bad".into()));
        assert_eq!(unmeasured["held"], serde_json::json!(null));
        assert_eq!(unmeasured["proven"], serde_json::json!(false));
        assert!(unmeasured["pid"].is_null());
    }
}
