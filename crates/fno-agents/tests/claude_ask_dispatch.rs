//! Integration tests for `claude_ask::dispatch_claude_ask` (ab-cc926b4e, W3).
//!
//! Exercises the orchestration: validation (exit 2), create (registry write +
//! `<short_id>\n` stdout + events), and follow-up (live socket -> reply, status
//! stamping, orphan handling). `AgentsHome::at` / `ClaudeHome::at` pin temp
//! dirs; the fake `claude` is injected via `extra_env` PATH (no global env).

use fno_agents::claude_ask::{dispatch_claude_ask, dispatch_claude_spawn, ClaudeHome};
use fno_agents::paths::AgentsHome;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abi-ask-dispatch-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

fn install_fake_claude(bin_dir: &Path) {
    let script = r#"#!/bin/sh
name=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--name" ]; then name="$a"; fi
  prev="$a"
done
if [ -n "$FAKE_CLAUDE_ARGV" ]; then printf '%s\n' "$@" > "$FAKE_CLAUDE_ARGV"; fi
printf 'backgrounded · 7c5dcf5d · %s\n' "$name"
exit 0
"#;
    let path = bin_dir.join("claude");
    fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(bin_dir: &Path) -> String {
    format!("{}:/usr/bin:/bin", bin_dir.display())
}

/// Seed `registry.json` with a single claude entry (Python AgentEntry shape).
fn seed_claude_registry(home: &AgentsHome, name: &str, short_id: &str) {
    let body = format!(
        r#"{{"schema_version":3,"agents":[{{"name":"{}","provider":"claude","cwd":"/tmp","claude_short_id":"{}","status":"live","created_at":"2026-05-27T00:00:00Z","log_path":null}}]}}"#,
        name, short_id
    );
    let path = home.registry_json();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn write_state(jobs: &Path, state: &str, updated: &str, result: &str) {
    fs::create_dir_all(jobs).unwrap();
    fs::write(
        jobs.join("state.json"),
        format!(
            r#"{{"state":"{}","updatedAt":"{}","output":{{"result":"{}"}}}}"#,
            state, updated, result
        ),
    )
    .unwrap();
}

// --- validation (exit 2) ---

#[test]
fn validation_rejects_bad_inputs() {
    let home = AgentsHome::at(tmpdir("val-home"));
    let ch = ClaudeHome::at(tmpdir("val-claude"));
    let cwd = tmpdir("val-cwd");
    let d = Duration::from_secs(1);

    let empty_name =
        dispatch_claude_ask(&home, &ch, "", "hi", "abilities", &cwd, false, Some(d), &[]);
    assert_eq!(empty_name.exit_code, 2);

    let sep_name = dispatch_claude_ask(
        &home,
        &ch,
        "a/b",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(d),
        &[],
    );
    assert_eq!(sep_name.exit_code, 2);

    let shortid_shape = dispatch_claude_ask(
        &home,
        &ch,
        "7c5dcf5d",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(d),
        &[],
    );
    assert_eq!(shortid_shape.exit_code, 2);

    let empty_msg = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "   ",
        "abilities",
        &cwd,
        false,
        Some(d),
        &[],
    );
    assert_eq!(empty_msg.exit_code, 2);

    let bad_from = dispatch_claude_ask(&home, &ch, "alice", "hi", "a<b", &cwd, false, Some(d), &[]);
    assert_eq!(bad_from.exit_code, 2);
    assert!(bad_from.stderr.contains("XML-unsafe"));
}

// --- create ---

// Task 1.3a: ask never creates. Create-machinery coverage repointed to dispatch_claude_spawn.
#[test]
fn ask_unknown_name_exits_16_not_create() {
    // dispatch_claude_ask on an unknown name must exit 16 (not create).
    let home = AgentsHome::at(tmpdir("ask-nocreat-home"));
    let ch = ClaudeHome::at(tmpdir("ask-nocreat-claude"));
    let cwd = tmpdir("ask-nocreat-cwd");
    let bin = tmpdir("ask-nocreat-bin");
    install_fake_claude(&bin);
    let path = path_with(&bin);

    let out = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "hello",
        "abilities",
        &cwd,
        false,
        None,
        &[("PATH", path.as_str())],
    );
    assert_eq!(
        out.exit_code, 16,
        "ask unknown name must exit 16, got {} ({})",
        out.exit_code, out.stderr
    );
    assert!(out.stderr.contains("unknown agent"), "{}", out.stderr);
    assert!(out.stderr.contains("spawn"), "{}", out.stderr);
    // No registry row written.
    assert!(
        !home.registry_json().exists()
            || !fs::read_to_string(home.registry_json())
                .unwrap()
                .contains("alice")
    );
}

#[test]
fn spawn_writes_python_readable_row_and_emits_done() {
    // Create-machinery coverage: dispatch_claude_spawn writes the registry row.
    let home = AgentsHome::at(tmpdir("create-home"));
    let ch = ClaudeHome::at(tmpdir("create-claude"));
    let cwd = tmpdir("create-cwd");
    let bin = tmpdir("create-bin");
    install_fake_claude(&bin);
    let path = path_with(&bin);

    let out = dispatch_claude_spawn(
        &home,
        &ch,
        "alice",
        "hello",
        "abilities",
        &cwd,
        false,
        None,
        &[("PATH", path.as_str())],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
        false, // surface_cwd: explicit --cwd, no default move (x-85fe)
    );
    assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
    // spawn returns JSON receipt, not bare short_id.
    let receipt: serde_json::Value =
        serde_json::from_str(out.stdout.trim_end_matches('\n')).unwrap();
    assert_eq!(receipt["provider"], "claude");
    assert_eq!(receipt["status"], "live");
    let short_id = receipt["short_id"].as_str().unwrap();
    assert_eq!(short_id, "7c5dcf5d");

    // Registry row is Python-readable: the claude jobId lives in short_id (v9),
    // project_root skipped when empty. Parse to be format-agnostic.
    let reg = fs::read_to_string(home.registry_json()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&reg).unwrap();
    let row = &v["agents"][0];
    assert_eq!(row["short_id"], "7c5dcf5d");
    assert_eq!(row["harness"], "claude");
    assert_eq!(row["status"], "live");
    assert!(
        row.get("project_root").is_none(),
        "leaked rust-only key: {}",
        reg
    );
    // v9: short_id now legitimately carries the claude jobId (asserted above),
    // so it is present, not skipped -- the old "short_id is none" check is gone.

    // agent_ask_done emitted.
    let events = fs::read_to_string(home.events_jsonl()).unwrap();
    assert!(events.contains("\"kind\":\"agent_ask_done\""), "{}", events);
}

// x-dfa4: --yolo is no longer a claude no-op - it maps to
// --permission-mode bypassPermissions (AC4-HP for the bg lane) and the
// misleading "no effect" note is gone. The applied mode is named in the receipt.
#[test]
fn spawn_yolo_maps_to_bypass_permissions() {
    let home = AgentsHome::at(tmpdir("yolo-home"));
    let ch = ClaudeHome::at(tmpdir("yolo-claude"));
    let cwd = tmpdir("yolo-cwd");
    let bin = tmpdir("yolo-bin");
    install_fake_claude(&bin);
    let path = path_with(&bin);
    let argv_file = cwd.join("argv.txt");

    let out = dispatch_claude_spawn(
        &home,
        &ch,
        "bob",
        "hi",
        "abilities",
        &cwd,
        true,
        None,
        &[
            ("PATH", path.as_str()),
            ("FAKE_CLAUDE_ARGV", argv_file.to_str().unwrap()),
        ],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
        false, // surface_cwd: explicit --cwd, no default move (x-85fe)
    );
    assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
    assert!(
        !out.stderr.contains("--yolo has no effect"),
        "the no-op note must be gone: {}",
        out.stderr
    );
    let argv = std::fs::read_to_string(&argv_file).unwrap();
    assert!(
        argv.contains("--permission-mode") && argv.contains("bypassPermissions"),
        "yolo must map to bypassPermissions; argv: {argv}"
    );
    let receipt: serde_json::Value =
        serde_json::from_str(out.stdout.trim_end_matches('\n')).unwrap();
    assert_eq!(receipt["permission_mode"], "bypassPermissions");
}

// --- follow-up ---

#[test]
fn followup_socket_reply_stamps_live_and_emits() {
    use std::os::unix::net::UnixListener;
    let home = AgentsHome::at(tmpdir("fu-home"));
    let claude_root = tmpdir("fu-claude");
    let ch = ClaudeHome::at(&claude_root);
    let cwd = tmpdir("fu-cwd");

    seed_claude_registry(&home, "alice", "abcd1234");

    let sessions = claude_root.join(".claude").join("sessions");
    let jobs = claude_root.join(".claude").join("jobs").join("abcd1234");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    let sock = short_sock();
    let listener = UnixListener::bind(&sock).unwrap();
    fs::write(
        sessions.join("999.json"),
        format!(
            r#"{{"jobId":"abcd1234","kind":"bg","messagingSocketPath":"{}","sessionId":"s1"}}"#,
            sock.to_str().unwrap()
        ),
    )
    .unwrap();

    let jobs_t = jobs.clone();
    let handle = std::thread::spawn(move || loop {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let _ = conn.read_to_end(&mut buf);
        if buf.is_empty() {
            continue; // liveness probe
        }
        write_state(&jobs_t, "completed", "2026-05-27T10:00:09Z", "HELLO-BACK");
        break;
    });

    let out = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "ping",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(3)),
        &[],
    );
    handle.join().unwrap();
    cleanup_sock(&sock);
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, "HELLO-BACK"); // no trailing newline on follow-up

    let events = fs::read_to_string(home.events_jsonl()).unwrap();
    assert!(
        events.contains("\"kind\":\"agent_followup_started\""),
        "{}",
        events
    );
    assert!(
        events.contains("\"kind\":\"agent_followup_done\""),
        "{}",
        events
    );
    assert!(events.contains("\"backend\":\"socket\""), "{}", events);
}

#[test]
fn followup_orphan_socket_null_exit_13_stamps_orphaned() {
    let home = AgentsHome::at(tmpdir("orph-home"));
    let claude_root = tmpdir("orph-claude");
    let ch = ClaudeHome::at(&claude_root);
    let cwd = tmpdir("orph-cwd");

    seed_claude_registry(&home, "alice", "abcd1234");
    let sessions = claude_root.join(".claude").join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("1.json"),
        r#"{"jobId":"abcd1234","kind":"bg","messagingSocketPath":null}"#,
    )
    .unwrap();

    let out = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "ping",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(2)),
        &[],
    );
    assert_eq!(out.exit_code, 13);
    assert!(out.stderr.contains("suspended"), "{}", out.stderr);

    // status stamped orphaned in the registry.
    let reg = fs::read_to_string(home.registry_json()).unwrap();
    assert!(reg.contains("\"orphaned\""), "{}", reg);
}

/// Short, collision-free AF_UNIX path under /tmp (macOS sun_path is ~104 chars,
/// so the long /var/folders tempdir won't fit). A process-wide atomic counter
/// guarantees uniqueness even when two tests mint a path in the same nanosecond
/// (an observed EEXIST flake otherwise). Caller unlinks via `cleanup_sock`.
fn short_sock() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    PathBuf::from(format!(
        "/tmp/abiask{}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        n
    ))
}

/// Unlink a bound socket file so /tmp/abiask*.sock doesn't accumulate
/// (UnixListener::drop does not remove the path).
fn cleanup_sock(sock: &Path) {
    let _ = fs::remove_file(sock);
}

fn write_bg_session(sessions: &Path, pid: &str, job: &str, sock: &Path) {
    fs::write(
        sessions.join(format!("{}.json", pid)),
        format!(
            r#"{{"jobId":"{}","kind":"bg","messagingSocketPath":"{}","sessionId":"s1"}}"#,
            job,
            sock.to_str().unwrap()
        ),
    )
    .unwrap();
}

// AC7: live session but state.json never transitions -> poll timeout -> exit 15.
#[test]
fn followup_poll_timeout_exit_15() {
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let home = AgentsHome::at(tmpdir("to-home"));
    let claude_root = tmpdir("to-claude");
    let ch = ClaudeHome::at(&claude_root);
    let cwd = tmpdir("to-cwd");
    seed_claude_registry(&home, "alice", "abcd1234");
    let sessions = claude_root.join(".claude").join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(claude_root.join(".claude").join("jobs").join("abcd1234")).unwrap();
    let sock = short_sock();
    let listener = UnixListener::bind(&sock).unwrap();
    write_bg_session(&sessions, "999", "abcd1234", &sock);

    // Accept (probe + send) but never write a terminal state.json.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        while !stop_t.load(Ordering::Relaxed) {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = Vec::new();
                let _ = conn.read_to_end(&mut buf);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    let out = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "ping",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_millis(250)),
        &[],
    );
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    cleanup_sock(&sock);

    assert_eq!(out.exit_code, 15);
    assert!(out.stderr.contains("no reply within"), "{}", out.stderr);
    let events = fs::read_to_string(home.events_jsonl()).unwrap();
    assert!(events.contains("\"stage\":\"poll-timeout\""), "{}", events);
}

// AC10: two concurrent followups on the same agent serialize on the per-agent
// flock (no deadlock); both get a reply.
#[test]
fn concurrent_followups_serialize_on_flock() {
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    let home = Arc::new(AgentsHome::at(tmpdir("cc-home")));
    let claude_root = tmpdir("cc-claude");
    let ch = Arc::new(ClaudeHome::at(&claude_root));
    seed_claude_registry(&home, "alice", "abcd1234");
    let sessions = claude_root.join(".claude").join("sessions");
    let jobs = claude_root.join(".claude").join("jobs").join("abcd1234");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    let sock = short_sock();
    let listener = UnixListener::bind(&sock).unwrap();
    write_bg_session(&sessions, "999", "abcd1234", &sock);

    // On each delivered envelope, advance state.json with a monotonically
    // increasing updatedAt so each serialized followup sees a fresh terminal.
    let jobs_t = jobs.clone();
    let seq = Arc::new(AtomicU64::new(1));
    let seq_t = seq.clone();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_t = done.clone();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        while !done_t.load(Ordering::Relaxed) {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = Vec::new();
                let _ = conn.read_to_end(&mut buf);
                if !buf.is_empty() {
                    let n = seq_t.fetch_add(1, Ordering::SeqCst);
                    write_state(
                        &jobs_t,
                        "completed",
                        &format!("2026-05-27T10:00:{:02}Z", n + 10),
                        "OK",
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(3));
        }
    });

    let mut workers = Vec::new();
    for _ in 0..2 {
        let h = home.clone();
        let c = ch.clone();
        workers.push(std::thread::spawn(move || {
            let cwd = std::env::temp_dir();
            dispatch_claude_ask(
                &h,
                &c,
                "alice",
                "ping",
                "abilities",
                &cwd,
                false,
                Some(Duration::from_secs(5)),
                &[],
            )
        }));
    }
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    done.store(true, Ordering::Relaxed);
    let _ = handle.join();
    cleanup_sock(&sock);

    // Both followups completed (flock serialized them, no deadlock).
    for r in &results {
        assert_eq!(r.exit_code, 0, "stderr={}", r.stderr);
        assert_eq!(r.stdout, "OK");
    }
}

// Codex P2: spawn with `claude` not on PATH -> exit 14 (config error).
// Task 1.3a: coverage repointed from dispatch_claude_ask to dispatch_claude_spawn.
#[test]
fn spawn_missing_cli_exit_14() {
    let home = AgentsHome::at(tmpdir("m14-home"));
    let ch = ClaudeHome::at(tmpdir("m14-claude"));
    let cwd = tmpdir("m14-cwd");
    let empty_bin = tmpdir("m14-bin"); // no `claude` here, no /usr/bin
    let path = format!("{}", empty_bin.display());
    let out = dispatch_claude_spawn(
        &home,
        &ch,
        "alice",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(5)),
        &[("PATH", path.as_str())],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
        false, // surface_cwd: explicit --cwd, no default move (x-85fe)
    );
    assert_eq!(out.exit_code, 14, "stderr={}", out.stderr);
}

#[test]
fn create_corrupt_registry_exit_12_no_spawn() {
    // Codex P2: a corrupt registry must fail (exit 12) BEFORE spawning claude.
    let home = AgentsHome::at(tmpdir("corrupt-home"));
    let ch = ClaudeHome::at(tmpdir("corrupt-claude"));
    let cwd = tmpdir("corrupt-cwd");
    let bin = tmpdir("corrupt-bin");
    install_fake_claude(&bin);
    let path = path_with(&bin);
    // Write a registry that is valid JSON but the wrong schema shape so the
    // typed read errors (not merely an empty/missing file).
    fs::create_dir_all(home.registry_json().parent().unwrap()).unwrap();
    fs::write(
        home.registry_json(),
        "{\"schema_version\":\"not-an-int\",\"agents\":\"nope\"}",
    )
    .unwrap();
    let out = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "hi",
        "abilities",
        &cwd,
        false,
        None,
        &[("PATH", path.as_str())],
    );
    assert_eq!(out.exit_code, 12, "stderr={}", out.stderr);
}

#[test]
fn followup_missing_short_id_exit_12() {
    let home = AgentsHome::at(tmpdir("noid-home"));
    let ch = ClaudeHome::at(tmpdir("noid-claude"));
    let cwd = tmpdir("noid-cwd");
    // registry entry with no short id on file
    let body = r#"{"schema_version":3,"agents":[{"name":"alice","provider":"claude","cwd":"/tmp","status":"live","created_at":"2026-05-27T00:00:00Z","log_path":null}]}"#;
    fs::create_dir_all(home.registry_json().parent().unwrap()).unwrap();
    fs::write(home.registry_json(), body).unwrap();

    let out = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "ping",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(2)),
        &[],
    );
    assert_eq!(out.exit_code, 12);
    assert!(out.stderr.contains("no short id"), "{}", out.stderr);
}

#[test]
fn followup_interactive_claude_row_refuses_worker_short() {
    // x-1b1e (codex review P2): an interactive stream-json claude row carries the
    // daemon WORKER id in short_id, not a --bg jobId. `ask` followup must NOT
    // route that worker id to a jobId-expecting locate_session; it refuses (exit
    // 12), same as a row with no jobId at all.
    let home = AgentsHome::at(tmpdir("interactive-home"));
    let ch = ClaudeHome::at(tmpdir("interactive-claude"));
    let cwd = tmpdir("interactive-cwd");
    let body = r#"{"schema_version":3,"agents":[{"name":"host1","provider":"claude","cwd":"/tmp","status":"live","short_id":"wk-host1","host_mode":"interactive","created_at":"2026-05-27T00:00:00Z","log_path":null}]}"#;
    fs::create_dir_all(home.registry_json().parent().unwrap()).unwrap();
    fs::write(home.registry_json(), body).unwrap();

    let out = dispatch_claude_ask(
        &home,
        &ch,
        "host1",
        "ping",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(2)),
        &[],
    );
    assert_eq!(out.exit_code, 12, "{}", out.stderr);
    assert!(out.stderr.contains("no short id"), "{}", out.stderr);
}
