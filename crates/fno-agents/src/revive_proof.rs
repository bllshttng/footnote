//! One op: did a `spawn --resume` fork come up? The revival liveness gate.
//!
//! A fork of a claude thread worker can return a session id while the new
//! session never writes a transcript line and its job state reads `blocked`:
//! a revival that reads as success while the slot is spent and mail queues to
//! nothing. This op polls for proof of life - a non-empty transcript for the
//! fork's own session id under any claude root, and a job state outside the
//! wedged pair - and prints one JSON verdict, stopping a fork that never came
//! up. Python's revival path refuses on `ok: false`; the poll lives here
//! because the readers (session records, job state, transcript layout)
//! already live in this crate and Rust is the product. Reached as
//! `claude-birth-exec revive-proof ...` (law d-fe66560a allows no new client
//! action); direct dispatch, no daemon RPC.
use crate::claude_ask::{read_state_json, resolve_session_uuid, ClaudeHome};
use crate::subprocess_ask::wait_with_grace;
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a fork gets to prove it came up. The node's verify clause: a
/// fork that never starts must refuse within about a minute.
const WINDOW: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_secs(1);
const WEDGED: [&str; 2] = ["blocked", "stopped"];

/// The fork's own transcript: a non-empty `<projects>/*/<uuid>.jsonl` under
/// any root. The one-level scan per root mirrors announce.rs::transcript_path;
/// the slug is not computed because the scan reads every slug dir.
fn find_transcript(home: &ClaudeHome, uuid: &str) -> Option<PathBuf> {
    for dir in home.project_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join(format!("{uuid}.jsonl"));
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.len() > 0 {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Poll for proof of life. Success: the transcript plus a job state outside
/// the wedged pair (an unreadable state with a transcript passes: a session
/// streaming lines is up). Past the window: `ok: false` naming both reads.
pub fn verify(
    home: &ClaudeHome,
    short_id: &str,
    window: Duration,
    poll: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + window;
    let mut transcript: Option<PathBuf> = None;
    let mut job_state;
    loop {
        if transcript.is_none() {
            transcript =
                resolve_session_uuid(home, short_id).and_then(|u| find_transcript(home, &u));
        }
        job_state = read_state_json(&home.jobs_dir_for(short_id))
            .ok()
            .and_then(|s| {
                if s.state.is_empty() {
                    None
                } else {
                    Some(s.state)
                }
            });
        let wedged = job_state.as_deref().is_some_and(|s| WEDGED.contains(&s));
        if let (Some(t), false) = (&transcript, wedged) {
            return json!({
                "ok": true,
                "transcript": t.display().to_string(),
                "job_state": job_state,
            });
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(poll);
    }
    json!({
        "ok": false,
        "transcript": transcript.as_ref().map(|t| t.display().to_string()),
        "job_state": job_state,
        "reason": format!(
            "transcript {}, job state {} after {}s",
            if transcript.is_some() { "present" } else { "absent" },
            job_state.as_deref().unwrap_or("unknown"),
            window.as_secs()
        ),
    })
}

/// Client-local entry (`fno-agents revive-proof --short-id <id>`). Prints the
/// JSON verdict on stdout and exits 0; the verdict carries ok/fail, so a
/// refusal is data, not an exit code. Usage errors exit 2.
pub fn run_revive_proof(args: &[String]) -> i32 {
    let mut short_id: Option<&String> = None;
    let mut window = WINDOW;
    let mut poll = POLL;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--short-id" if i + 1 < args.len() => {
                short_id = Some(&args[i + 1]);
                i += 2;
            }
            "--window-secs" if i + 1 < args.len() => {
                window = Duration::from_secs(args[i + 1].parse().unwrap_or(60));
                i += 2;
            }
            "--poll-secs" if i + 1 < args.len() => {
                poll = Duration::from_millis(args[i + 1].parse().unwrap_or(1000));
                i += 2;
            }
            _ => {
                eprintln!("usage: revive-proof --short-id <id> [--window-secs N] [--poll-secs N]");
                return 2;
            }
        }
    }
    let Some(short_id) = short_id else {
        eprintln!("usage: revive-proof --short-id <id> [--window-secs N] [--poll-secs N]");
        return 2;
    };
    let mut verdict = verify(&ClaudeHome::from_env(), &short_id, window, poll);
    if verdict["ok"] == true {
        // stderr, inherited: the Python caller pipes only stdout, so this is
        // the line the operator sees - the receipt names a path that exists.
        eprintln!(
            "spawn: revival liveness verified for {short_id}; transcript {}",
            verdict["transcript"].as_str().unwrap_or("unknown")
        );
    } else {
        // Stop the fork's session: the raw `claude stop` shellout takes no
        // fno lock (the caller holds the per-agent flock), and a failed stop
        // is reported, never swallowed. wait_with_grace bounds it (SIGTERM,
        // then SIGKILL after 5s).
        let stopped = Command::new("claude")
            .args(["stop", &short_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|mut child| wait_with_grace(child.id(), &mut child, 30.0).0 == 0)
            .unwrap_or(false);
        verdict["stopped"] = json!(stopped);
        if !stopped {
            verdict["reason"] = json!(format!(
                "{}; the fork's stop failed - it still holds its slot",
                verdict["reason"].as_str().unwrap_or_default()
            ));
        }
    }
    println!("{verdict}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "revive-proof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Stage one session record: the fork's job, kind bg, with a sessionId.
    fn stage_session(home: &PathBuf, job: &str, uuid: &str) {
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let record = format!(
            r#"{{"jobId":"{job}","kind":"bg","sessionId":"{uuid}","messagingSocketPath":null}}"#
        );
        fs::write(sessions.join("101.json"), record).unwrap();
    }

    fn stage_transcript(home: &PathBuf, uuid: &str, body: &str) {
        let dir = home.join(".claude").join("projects").join("-tmp-work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{uuid}.jsonl")), body).unwrap();
    }

    fn stage_state(home: &PathBuf, job: &str, state: &str) {
        let dir = home.join(".claude").join("jobs").join(job);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("state.json"),
            format!(r#"{{"state":"{state}","updatedAt":null}}"#),
        )
        .unwrap();
    }

    #[test]
    fn refuses_a_fork_that_never_wrote_a_transcript() {
        let home = tmpdir();
        stage_session(&home, "7c5dcf5d", "sess-1");
        stage_state(&home, "7c5dcf5d", "blocked");
        let v = verify(
            &ClaudeHome::at(&home),
            "7c5dcf5d",
            Duration::from_millis(150),
            Duration::from_millis(25),
        );
        assert_eq!(v["ok"], false);
        assert_eq!(v["job_state"], "blocked");
        assert!(v["reason"].as_str().unwrap().contains("transcript absent"));
    }

    #[test]
    fn passes_once_transcript_and_live_state_arrive() {
        let home = tmpdir();
        stage_session(&home, "7c5dcf5d", "sess-1");
        stage_state(&home, "7c5dcf5d", "idle");
        stage_transcript(&home, "sess-1", "{}");
        let v = verify(
            &ClaudeHome::at(&home),
            "7c5dcf5d",
            Duration::from_secs(2),
            Duration::from_millis(25),
        );
        assert_eq!(v["ok"], true);
        assert!(v["transcript"].as_str().unwrap().ends_with("sess-1.jsonl"));
    }

    #[test]
    fn refuses_when_state_stays_wedged_despite_a_transcript() {
        let home = tmpdir();
        stage_session(&home, "7c5dcf5d", "sess-1");
        stage_state(&home, "7c5dcf5d", "blocked");
        stage_transcript(&home, "sess-1", "{}");
        let v = verify(
            &ClaudeHome::at(&home),
            "7c5dcf5d",
            Duration::from_millis(150),
            Duration::from_millis(25),
        );
        assert_eq!(v["ok"], false);
        assert!(v["reason"].as_str().unwrap().contains("transcript present"));
    }

    #[test]
    fn an_unreadable_state_with_a_transcript_passes() {
        let home = tmpdir();
        stage_session(&home, "7c5dcf5d", "sess-1");
        stage_transcript(&home, "sess-1", "{}");
        let v = verify(
            &ClaudeHome::at(&home),
            "7c5dcf5d",
            Duration::from_secs(2),
            Duration::from_millis(25),
        );
        assert_eq!(v["ok"], true);
        assert_eq!(v["job_state"], serde_json::Value::Null);
    }

    #[test]
    fn an_unresolvable_session_id_refuses() {
        let home = tmpdir();
        let v = verify(
            &ClaudeHome::at(&home),
            "ffffffff",
            Duration::from_millis(150),
            Duration::from_millis(25),
        );
        assert_eq!(v["ok"], false);
        assert!(v["reason"].as_str().unwrap().contains("transcript absent"));
    }
}
