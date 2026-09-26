//! Arm or lift a busy-mode hold for ANOTHER session (transport-only client
//! action, registered in no client menu - the shrink law allows no new
//! client verbs; the mux server's keystroke arm and `king cancel` reach it
//! through the binary path like the other early dispatches).
//!
//! The hold is the registry row's `delivery_policy = "bus-only"` stamp plus
//! the sidecar clock `fno.mail.hold` reads. This action is the CROSS-SESSION
//! arm source the Python verb cannot be: `fno agents mail hold` resolves
//! identity from the calling process's own harness markers, and a marker
//! naming another live session's row is a leaked marker the self-identity
//! detector refuses by design. The files written here are byte-identical to
//! what the Python writer (`_write` in cli/src/fno/mail/hold.py) produces,
//! so every Python reader (the injector gate, `notify-self`, `hold-release`)
//! sees one hold, never two dialects.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::paths::AgentsHome;
use crate::state::update_registry;

/// The idle window the server arm runs, in minutes: hold.py's own
/// `DEFAULT_MINUTES`, so a hold the server armed reads exactly like one the
/// operator armed by hand.
pub(crate) const DEFAULT_MINUTES: u64 = 5;

/// The canonical mailbox address: the first eight characters of the session
/// identity key (harness_identity.canonical_handle). UUID-family ids compare
/// case-insensitively; opencode's `ses_` ids do not.
fn identity_key(session_id: &str) -> String {
    if session_id.starts_with("ses_") {
        session_id.to_string()
    } else {
        session_id.to_lowercase()
    }
}

fn canonical_handle(session_id: &str) -> String {
    identity_key(session_id).chars().take(8).collect()
}

/// The state root the Python `hold_dir()` resolves to: `$FNO_HOME`, else
/// `$HOME/.fno`. Config-file `state_dir` overrides are not read here, the
/// same bound every other Rust writer in the fleet runs under.
fn state_root() -> PathBuf {
    if let Some(home) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(home);
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".fno"))
        .unwrap_or_else(|| PathBuf::from(".fno"))
}

fn hold_sidecar_path(handle: &str) -> PathBuf {
    state_root()
        .join("mail-hold")
        .join(format!("{handle}.json"))
}

/// Find the registry row whose harness session id is `session_id`
/// (case-normalized), returning its index.
fn row_for_session(registry: &crate::state::Registry, session_id: &str) -> Option<usize> {
    let wanted = identity_key(session_id);
    registry.entries.iter().position(|e| {
        e.harness_session_id
            .as_deref()
            .map(|sid| identity_key(sid) == wanted)
            .unwrap_or(false)
    })
}

/// Stamp the row `bus-only` (or clear the stamp for `--off`) under the
/// cross-language registry lock. Returns the matched row's session id, or
/// None when no row carries it (fail-closed: no row, no clock).
fn set_policy(session_id: &str, policy: Option<&str>) -> Option<String> {
    let path = AgentsHome::shared_registry_json();
    let matched = update_registry(&path, |registry| {
        let i = row_for_session(registry, session_id)?;
        registry.entries[i].delivery_policy = policy.map(str::to_string);
        registry.entries[i].harness_session_id.clone()
    })
    .ok()?;
    matched
}

/// Write the sidecar clock in hold.py `_write`'s exact shape: one JSON
/// object, Python's `, `/`: ` separators and key order, trailing newline,
/// atomic via temp file + rename. Idle clock: `until = now + window`,
/// `ceiling = until + window` (the 2x window arm).
fn write_idle_clock(handle: &str, window_s: u64) -> std::io::Result<()> {
    let now = chrono::Utc::now();
    let until = now + chrono::Duration::seconds(window_s as i64);
    let ceiling = until + chrono::Duration::seconds(window_s as i64);
    let until_s = until.format("%Y-%m-%dT%H:%M:%SZ");
    let ceiling_s = ceiling.format("%Y-%m-%dT%H:%M:%SZ");
    let payload = format!(
        "{{\"until\": \"{until_s}\", \"window_s\": {window}, \
         \"clock_kind\": \"idle\", \"ceiling\": \"{ceiling_s}\"}}\n",
        window = window_s
    );
    let dir = state_root().join("mail-hold");
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(payload.as_bytes())?;
    }
    std::fs::rename(&tmp, hold_sidecar_path(handle))
}

/// Spawn the Python release timer detached (stdio null, own process group):
/// the third drain trigger that lifts the hold and delivers the digest with
/// no further input. The timer re-reads the clock every poll, so a re-arm
/// simply keeps it sleeping, and its designed exit is a vanished clock.
fn spawn_release_timer(handle: &str) {
    let mut cmd = Command::new(crate::scrape::fno_py());
    cmd.args(["agents", "mail", "hold-release", "--handle", handle])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Err(exc) = cmd.spawn() {
        // The hold still lifts on the next send attempt or prompt; say why
        // the clock-alone lift will not fire.
        eprintln!("mail-hold: release timer did not start: {exc}");
    }
}

/// `fno-agents mail-hold --session <id> [--minutes N] | [--off]`
///
/// Arm (default): stamp `bus-only` on the row, write the idle clock, spawn
/// the release timer. `--off`: clear the clock and unstamp the policy, so a
/// cancelled crown's mail delivers normally instead of holding forever on a
/// stamped row with no clock (the never-lapses state). No row for the
/// session: exit 3, nothing written.
pub fn run_mail_hold(args: &[String]) -> i32 {
    let mut session: Option<&String> = None;
    let mut minutes = DEFAULT_MINUTES;
    let mut off = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => session = iter.next(),
            "--minutes" => match iter.next().and_then(|v| v.parse::<u64>().ok()) {
                // The window feeds `minutes * 60` seconds into a chrono
                // i64 duration: bound it well inside both overflows.
                Some(n) if (1..=1_000_000).contains(&n) => minutes = n,
                _ => {
                    eprintln!("mail-hold: --minutes must be an integer between 1 and 1000000");
                    return 2;
                }
            },
            "--off" => off = true,
            other => {
                eprintln!("mail-hold: unknown argument {other:?}");
                return 2;
            }
        }
    }
    let Some(session_id) = session else {
        eprintln!("mail-hold: --session <session-id> is required");
        return 2;
    };
    if off {
        match set_policy(session_id, None) {
            Some(_) => {
                let _ = std::fs::remove_file(hold_sidecar_path(&canonical_handle(session_id)));
                0
            }
            None => {
                eprintln!("mail-hold: no registry row carries session {session_id}");
                3
            }
        }
    } else {
        let Some(matched) = set_policy(session_id, Some("bus-only")) else {
            eprintln!("mail-hold: no registry row carries session {session_id}");
            return 3;
        };
        let handle = canonical_handle(&matched);
        if let Err(exc) = write_idle_clock(&handle, minutes * 60) {
            eprintln!("mail-hold: could not write the clock for {handle}: {exc}");
            return 1;
        }
        spawn_release_timer(&handle);
        println!("mail-hold: bus-only armed for {handle} ({minutes}m idle)");
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin FNO_AGENTS_HOME (registry) and FNO_HOME (hold sidecars) to one
    /// tempdir for `f`, under the crate-wide env lock (claim_verbs idiom).
    /// FNO_PY points at `true` so the detached release-timer spawn is a
    /// no-op the test never waits on. Prior values are restored, so an
    /// ambient FNO_HOME survives the test.
    fn with_hold_env(f: impl FnOnce(&std::path::Path)) {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior: Vec<(String, Option<std::ffi::OsString>)> =
            ["FNO_AGENTS_HOME", "FNO_HOME", "FNO_PY"]
                .iter()
                .map(|k| (k.to_string(), std::env::var_os(k)))
                .collect();
        let td = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_AGENTS_HOME", td.path());
        std::env::set_var("FNO_HOME", td.path());
        std::env::set_var("FNO_PY", "true");
        f(td.path());
        for (key, value) in prior {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
    }

    fn registry_row(name: &str, session: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name, "status": "live", "cwd": "/repo", "harness": "claude",
            "harness_session_id": session,
            "created_at": "2026-09-26T00:00:00Z",
        })
    }

    fn write_registry(dir: &std::path::Path, rows: serde_json::Value) {
        let doc = serde_json::json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": rows,
        });
        std::fs::write(dir.join("registry.json"), doc.to_string()).unwrap();
    }

    fn clock(dir: &std::path::Path, handle: &str) -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(dir.join("mail-hold").join(format!("{handle}.json"))).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn arming_a_registered_session_stamps_the_row_and_writes_the_idle_clock() {
        with_hold_env(|dir| {
            write_registry(
                dir,
                serde_json::json!([registry_row(
                    "worker",
                    "CCCCCCCC-1111-2222-3333-444455556666"
                )]),
            );
            let code = run_mail_hold(&[
                "--session".into(),
                "cccccccc-1111-2222-3333-444455556666".into(),
            ]);
            assert_eq!(code, 0);
            // The row is stamped and the sidecar reads as hold.py would read it.
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert_eq!(
                registry.entries[0].delivery_policy.as_deref(),
                Some("bus-only")
            );
            let row = clock(dir, "cccccccc");
            assert_eq!(row["clock_kind"], "idle");
            assert_eq!(row["window_s"], 300);
            assert!(row["ceiling"].as_str().unwrap() > row["until"].as_str().unwrap());
        });
    }

    #[test]
    fn arming_an_unregistered_session_refuses_and_writes_nothing() {
        with_hold_env(|dir| {
            write_registry(
                dir,
                serde_json::json!([registry_row(
                    "worker",
                    "cccccccc-1111-2222-3333-444455556666"
                )]),
            );
            let code = run_mail_hold(&[
                "--session".into(),
                "dddddddd-1111-2222-3333-444455556666".into(),
            ]);
            assert_eq!(code, 3);
            assert!(
                !dir.join("mail-hold").join("dddddddd.json").exists(),
                "no sidecar for a session no row carries"
            );
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert!(registry.entries[0].delivery_policy.is_none());
        });
    }

    #[test]
    fn off_clears_both_the_clock_and_the_stamp() {
        with_hold_env(|dir| {
            write_registry(
                dir,
                serde_json::json!([registry_row(
                    "worker",
                    "cccccccc-1111-2222-3333-444455556666"
                )]),
            );
            assert_eq!(
                run_mail_hold(&[
                    "--session".into(),
                    "cccccccc-1111-2222-3333-444455556666".into()
                ]),
                0
            );
            assert_eq!(
                run_mail_hold(&[
                    "--session".into(),
                    "cccccccc-1111-2222-3333-444455556666".into(),
                    "--off".into()
                ]),
                0
            );
            assert!(
                !dir.join("mail-hold").join("cccccccc.json").exists(),
                "the clock file is gone"
            );
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert!(registry.entries[0].delivery_policy.is_none());
        });
    }

    #[test]
    fn missing_session_argument_refuses() {
        assert_eq!(run_mail_hold(&[]), 2);
        assert_eq!(
            run_mail_hold(&[
                "--minutes".into(),
                "0".into(),
                "--session".into(),
                "x-cccccccc".into()
            ]),
            2
        );
    }
}
