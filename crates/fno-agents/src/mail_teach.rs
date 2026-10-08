//! The fmail read verb's once-per-session teaching state.
//!
//! A delivered mail opens with one header line whose id is the only cue the
//! receiver gets; the header shipped without teaching the read verb, and no
//! hook, skill or spawn brief on main mentions it. One line teaches it, at
//! most once per session and again after each compaction: every carrier
//! (the session-start drain, the post-compact reinject, and the header-only
//! render for a harness with no hooks at all) asks this module whether the
//! lesson is due and prints [`crate::chats::teach_line`] only when it is.
//!
//! State is one session-keyed file, `<home>/mail_teach/<session>.json`,
//! holding the newest compaction boundary epoch the lesson was taught at
//! (`taught_boundary_epoch`, 0 = taught with none observed). The lesson is
//! due when the file is missing (never taught) or the transcript now holds a
//! boundary newer than the taught one. Boundary shapes are the two the
//! compaction reader knows (claude `compact_boundary`, codex `compacted`);
//! a transcript that carries neither never re-teaches by boundary, and the
//! missing-file case still teaches a session its first lesson.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::paths::AgentsHome;

fn teach_path(home: &AgentsHome, session: &str) -> PathBuf {
    home.root()
        .join("mail_teach")
        .join(format!("{session}.json"))
}

fn taught_boundary(path: &Path) -> Option<i64> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("taught_boundary_epoch")
        .and_then(serde_json::Value::as_i64)
}

/// The transcript a session's boundary reads come from: the caller's
/// explicit path, else the registry row the harness reported at SessionStart.
/// `registry` hands in a registry the caller already loaded (the render
/// path), so a due check costs no second locked load. An unreadable
/// registry reads as no transcript, never as an error.
fn resolve_transcript(
    home: &AgentsHome,
    session: &str,
    explicit: Option<&Path>,
    registry: Option<&crate::state::Registry>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let loaded;
    let registry = match registry {
        Some(rows) => rows,
        None => {
            loaded = crate::state::try_load_registry(&home.root().join("registry.json"))
                .ok()
                .flatten()?;
            &loaded
        }
    };
    registry
        .entries
        .iter()
        .find(|row| {
            row.harness_session_id.as_deref() == Some(session)
                || row.session_id.as_deref() == Some(session)
        })
        .and_then(|row| row.transcript_path.as_deref())
        .map(PathBuf::from)
}

/// Whether the lesson is due, and with `record`, stamps it taught in the
/// same breath so two carriers in one turn cannot both teach. A boundary
/// newer than the taught one re-teaches; an unreadable transcript only ever
/// leaves the never-taught case. Best-effort by contract: a state write that
/// fails still answers, so a carrier can never fail a delivery for it.
pub fn teach_if_due(
    home: &AgentsHome,
    session: &str,
    transcript: Option<&Path>,
    registry: Option<&crate::state::Registry>,
    record: bool,
) -> bool {
    let path = teach_path(home, session);
    let taught = taught_boundary(&path);
    let observed = resolve_transcript(home, session, transcript, registry)
        .as_deref()
        .and_then(crate::compaction::newest_boundary_epoch);
    let due = match (taught, observed) {
        (None, _) => true,
        (Some(taught), Some(boundary)) => boundary > taught,
        (Some(_), None) => false,
    };
    if due && record {
        let dir = path.parent();
        if let Some(dir) = dir.filter(|d| d.exists() || std::fs::create_dir_all(d).is_ok()) {
            let body = json!({
                "session": session,
                "taught_boundary_epoch": observed.unwrap_or(0),
            });
            let tmp = dir.join(format!(".{}.tmp-{}", session, std::process::id()));
            if std::fs::File::create(&tmp)
                .and_then(|mut f| f.write_all(body.to_string().as_bytes()))
                .and_then(|_| std::fs::rename(&tmp, &path))
                .is_err()
            {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    due
}

// ---------------------------------------------------------------------------
// CLI verb: `fno-agents mail-teach [--session <id> | --self]
//           [--transcript <path>] [--mark]`
//
// Prints the teaching line when the lesson is due, records the taught
// boundary when --mark rides along, and prints nothing otherwise. The hook
// carriers append stdout; silence is the no-op. A missing --session and no
// self identity is silence (hooks never block on this), exit 0.
// ---------------------------------------------------------------------------

pub fn run_mail_teach(args: &[String]) -> i32 {
    let mut session: Option<String> = None;
    let mut self_mode = false;
    let mut transcript: Option<PathBuf> = None;
    let mut record = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--session" => match it.next() {
                Some(v) => session = Some(v.clone()),
                None => {
                    eprintln!("mail-teach: --session needs a value");
                    return 2;
                }
            },
            "--self" => self_mode = true,
            "--transcript" => match it.next() {
                Some(v) => transcript = Some(PathBuf::from(v)),
                None => {
                    eprintln!("mail-teach: --transcript needs a value");
                    return 2;
                }
            },
            "--mark" => record = true,
            "--help" => {
                println!(
                    "usage: fno-agents mail-teach (--session <id> | --self) [--transcript <path>] [--mark]"
                );
                return 0;
            }
            other => {
                eprintln!("mail-teach: unexpected argument {other:?}");
                return 2;
            }
        }
    }
    let session = match (session, self_mode) {
        (Some(s), _) => s,
        (None, true) => {
            let home = AgentsHome::from_env();
            match crate::spawn_context::resolve_self_identity(
                &|k| std::env::var(k).ok(),
                None,
                None,
                &home,
            )
            .session_id
            {
                Some(sid) => sid,
                None => return 0,
            }
        }
        (None, false) => {
            eprintln!("mail-teach: --session <id> or --self is required");
            return 2;
        }
    };
    let home = AgentsHome::from_env();
    if teach_if_due(&home, &session, transcript.as_deref(), None, record) {
        println!("{}", crate::chats::teach_line());
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HomePin {
        dir: PathBuf,
    }

    impl HomePin {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mail-teach-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn home(&self) -> AgentsHome {
            AgentsHome::at(self.dir.clone())
        }
    }

    impl Drop for HomePin {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn transcript_with_boundary(dir: &Path, epoch: i64) -> PathBuf {
        let path = dir.join(format!("transcript-{epoch}.jsonl"));
        let ts = chrono::DateTime::from_timestamp(epoch, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"{ts}\"}}\n\
                 {{\"type\":\"summary\",\"subtype\":\"compact_boundary\",\"timestamp\":\"{ts}\"}}\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn teach_state_contract() {
        // The lesson lifecycle the node's test names: a fresh session is
        // due (its first header teaches), the stamp silences every later
        // carrier, and a NEWER transcript boundary makes it due exactly
        // once again.
        let pin = HomePin::new("state");
        let home = pin.home();
        let t1 = transcript_with_boundary(&pin.dir, 1_000);
        assert!(
            teach_if_due(&home, "s1", None, None, false),
            "never taught is due"
        );
        assert!(
            teach_if_due(&home, "s1", Some(&t1), None, true),
            "record stamps the lesson"
        );
        assert_eq!(
            taught_boundary(&teach_path(&home, "s1")),
            Some(1_000),
            "the stamp holds the boundary the lesson rode"
        );
        assert!(
            !teach_if_due(&home, "s1", Some(&t1), None, true),
            "taught stays silent"
        );
        let t2 = transcript_with_boundary(&pin.dir, 2_000);
        assert!(
            teach_if_due(&home, "s1", Some(&t2), None, true),
            "a newer boundary re-teaches"
        );
        assert!(
            !teach_if_due(&home, "s1", Some(&t2), None, true),
            "and only once"
        );
        // A harness whose transcript carries no known boundary shape (pi,
        // opencode) reads as "no boundary observed": the stamp silences it
        // forever after the first lesson. A missing transcript leaves the
        // never-taught case due.
        let plain = pin.dir.join("plain.jsonl");
        std::fs::write(&plain, "{\"type\":\"assistant\"}\n").unwrap();
        assert!(teach_if_due(&home, "s2", Some(&plain), None, true));
        assert!(!teach_if_due(&home, "s2", Some(&plain), None, true));
        assert!(
            teach_if_due(&home, "s3", None, None, false),
            "a missing transcript leaves never-taught due"
        );
        // The lesson names the LIVE read verb: the const the dispatch arm
        // itself matches on, so a rename follows in the same edit.
        let line = crate::chats::teach_line();
        assert!(
            line.contains(&format!(
                "fno agents mail {} <id>",
                crate::chats::MAIL_READ_VERB
            )),
            "{line}"
        );
        assert!(line.contains("`fmail-<id>`"), "{line}");
    }
}
