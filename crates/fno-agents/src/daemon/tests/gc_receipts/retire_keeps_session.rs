//! Retirement keeps what resume needs: the sweep stops a worker and the
//! harness session outlives the row. The active-surface seam runs for real
//! (a fake `claude` on PATH logs every argv it is asked for), so these tests
//! witness the commands retirement actually issues - a staged outcome could
//! never see an `rm` it never ran.

use super::*;
use super::{quiet_transcript, stage_graph, staged_graph_home, uniform_ages};
use crate::gc_sweep::{self, GcSummary};

/// The fake `claude` world: a PATH-shimmed binary that logs each argv and
/// answers `agents --json --all` from flag files, a daemon dir whose roster
/// lists the worker, and a job dir the harness `rm` arm deletes - what the
/// real `claude rm` does to session state.
struct FakeClaude {
    env_dir: tempfile::TempDir,
}

impl FakeClaude {
    fn install(worker: &str, session: &str) -> Self {
        let env_dir = tempfile::tempdir().unwrap();
        let bin_dir = env_dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let log = env_dir.path().join("argv.log");
        let flags = env_dir.path().join("flags");
        std::fs::create_dir_all(&flags).unwrap();
        let daemon = env_dir.path().join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        let roster = format!(
            r#"{{"proto":1,"supervisorPid":4242,"updatedAt":1751049130000,"workers":{{"{worker}":{{"pid":5002,"sessionId":"{session}","ptySock":"/tmp/fake/pty/{worker}.sock","startedAt":1751049050000,"attempt":2,"cwd":"/tmp","dispatch":{{"source":"fleet"}}}}}}}}"#
        );
        std::fs::write(daemon.join("roster.json"), roster).unwrap();
        let job_dir = env_dir.path().join("claude-home").join("jobs").join(worker);
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::write(job_dir.join("state.json"), "{}").unwrap();
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case "$1" in
  agents)
    if [ -f "{flags}/rm_ran" ]; then
      printf '[]\n'
    elif [ -f "{flags}/stop_ran" ]; then
      printf '[{{"id":"{worker}","state":"stopped","session_id":"{session}"}}]\n'
    else
      printf '[{{"id":"{worker}","state":"running","session_id":"{session}"}}]\n'
    fi
    ;;
  stop)
    touch "{flags}/stop_ran"
    ;;
  rm)
    touch "{flags}/rm_ran"
    rm -rf "{job_dir}"
    ;;
esac
"#,
            log = log.display(),
            flags = flags.display(),
            worker = worker,
            session = session,
            job_dir = job_dir.display(),
        );
        let bin = bin_dir.join("claude");
        std::fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { env_dir }
    }

    fn bin_dir(&self) -> std::path::PathBuf {
        self.env_dir.path().join("bin")
    }

    fn daemon_dir(&self) -> std::path::PathBuf {
        self.env_dir.path().join("daemon")
    }

    fn argv_log(&self) -> String {
        std::fs::read_to_string(self.env_dir.path().join("argv.log")).unwrap_or_default()
    }

    fn job_state(&self, worker: &str) -> std::path::PathBuf {
        self.env_dir
            .path()
            .join("claude-home")
            .join("jobs")
            .join(worker)
            .join("state.json")
    }
}

/// PATH + daemon-dir + agents-home swap, restored on drop. The env lock
/// serializes every test that mutates process state the sweep reads. The
/// agents home points at the sweep's own tmp root: the stop runner resolves
/// its state root eagerly, and an undeclared `$HOME` root panics under test.
struct EnvSwap {
    old_path: Option<std::ffi::OsString>,
    old_daemon: Option<std::ffi::OsString>,
    old_agents_home: Option<std::ffi::OsString>,
}

impl EnvSwap {
    fn to(bin: &std::path::Path, daemon: &std::path::Path, agents_home: &std::path::Path) -> Self {
        let old_path = std::env::var_os("PATH");
        let old_daemon = std::env::var_os(crate::claude_roster::DAEMON_DIR_ENV);
        let old_agents_home = std::env::var_os("FNO_AGENTS_HOME");
        let joined = format!(
            "{}:{}",
            bin.display(),
            old_path.as_deref().unwrap_or_default().to_string_lossy()
        );
        std::env::set_var("PATH", joined);
        std::env::set_var(crate::claude_roster::DAEMON_DIR_ENV, daemon);
        std::env::set_var("FNO_AGENTS_HOME", agents_home);
        Self {
            old_path,
            old_daemon,
            old_agents_home,
        }
    }
}

impl Drop for EnvSwap {
    fn drop(&mut self) {
        match &self.old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        match &self.old_daemon {
            Some(d) => std::env::set_var(crate::claude_roster::DAEMON_DIR_ENV, d),
            None => std::env::remove_var(crate::claude_roster::DAEMON_DIR_ENV),
        }
        match &self.old_agents_home {
            Some(h) => std::env::set_var("FNO_AGENTS_HOME", h),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
    }
}

/// One quiet claude thread row on a done node, staged the way the sweep
/// classifies it would-retire: transcript quiet past the grace, no inside
/// leg, no pane, no mux. The graph session id IS the row's session id, so
/// the reverse join resolves provenance.
fn stage_kept_row(
    dir: &std::path::Path,
    home: &AgentsHome,
    name: &str,
    worker: &str,
    session: &str,
) {
    stage_graph(
        dir,
        json!([{
            "id": "n-finished",
            "status": "done",
            "sessions": [{
                "phase": "do",
                "harness": "claude",
                "session_id": session,
                "started_at": "2026-09-01T00:00:00Z",
                "ended_at": "2026-09-01T01:00:00Z",
            }],
        }]),
    );
    crate::state::update_registry(&home.registry_json(), |r| {
        let mut e = state::RegistryEntry::default();
        e.name = name.to_string();
        e.short_id = worker.to_string();
        e.origin = Some("spawn".into());
        e.harness = Some("claude".into());
        e.harness_session_id = Some(session.to_string());
        e.created_at = "2026-09-01T00:00:00Z".into();
        r.entries.push(e);
    })
    .unwrap();
}

/// The production seam set, exactly as the daemon shell wires it (`gc.rs`
/// `gc_sweep`): the real graph read, the real stop routing, the real retire
/// surface and mux-member functions. Only the transcript store and the age
/// read are staged - they locate the quiet, they touch no harness.
fn production_sweep(home: &AgentsHome, quiet: std::path::PathBuf) -> GcSummary {
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let stop_home = home.clone();
    gc_sweep::run(
        home,
        &emitter,
        900,
        false,
        7,
        &crate::gc_sweep::read_graph_entries,
        &move |_| Some(vec![quiet.clone()]),
        &uniform_ages(2 * 3600),
        &move |e| gc_sweep::stop_row_process(&stop_home, e),
        &crate::gc_native::apply_retire_surface,
        &crate::gc_native::apply_mux_member_retirement,
        &crate::claude_roster::read_all_agents,
        &gc_sweep::production_tree_probe,
        &crate::daemon::rm_take_worktree,
    )
}

/// The receipt's `active-surface` effect outcome, read off the receipt the
/// sweep persisted for this session.
fn staged_active_surface(home: &AgentsHome, harness: &str, session: &str) -> Option<String> {
    let path = crate::receipt::reap_receipt_path_for(home, harness, session);
    let raw = std::fs::read_to_string(path).ok()?;
    let receipt: serde_json::Value = serde_json::from_str(&raw).ok()?;
    receipt["effects"]
        .as_array()?
        .iter()
        .find(|e| e["op"] == "active-surface")
        .and_then(|e| e["outcome"].as_str())
        .map(str::to_string)
}

/// AC1-HP: retiring a claude thread row runs the confirmed stop and nothing
/// else. No `rm` reaches the harness, the job dir survives, and the
/// receipt's active-surface effect reads not-applicable. The production seam
/// function runs for real - a staged outcome could never witness the `rm`
/// this test exists to catch.
#[test]
fn retiring_a_claude_thread_row_keeps_the_harness_session() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (dir, home) = staged_graph_home();
    stage_kept_row(
        dir.path(),
        &home,
        "worker-kept",
        "abcd1234",
        "abcd1234-1111-2222-3333-444444444444",
    );
    let store_dir = home.root().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let quiet = quiet_transcript(&store_dir, "q.jsonl", 2 * 3600);

    let fake = FakeClaude::install("abcd1234", "abcd1234-1111-2222-3333-444444444444");
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root());

    let summary = production_sweep(&home, quiet);

    assert_eq!(
        summary.retired.len(),
        1,
        "the quiet row retires: {:?}",
        summary.retired
    );
    let log = fake.argv_log();
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        lines.iter().any(|l| *l == "stop abcd1234"),
        "the confirmed stop ran: {log}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("rm ")),
        "retirement must not rm the harness session: {log}"
    );
    assert!(
        fake.job_state("abcd1234").exists(),
        "the job dir survives retirement"
    );
    assert_eq!(
        staged_active_surface(&home, "claude", "abcd1234-1111-2222-3333-444444444444").as_deref(),
        Some("not-applicable"),
        "the receipt names the session kept, not removed"
    );
}

/// AC1-RM: the split is total. The retire surface keeps the session; the
/// removal surface (`fno agents rm`'s cascade) is still the one door that
/// runs `claude rm`.
#[test]
fn rm_still_removes_what_retirement_keeps() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_dir, home) = staged_graph_home();
    let fake = FakeClaude::install("abcd1234", "abcd1234-1111-2222-3333-444444444444");
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root());
    let mut e = state::RegistryEntry::default();
    e.name = "worker-rm".into();
    e.short_id = "abcd1234".into();
    e.origin = Some("spawn".into());
    e.harness = Some("claude".into());
    e.harness_session_id = Some("abcd1234-1111-2222-3333-444444444444".into());
    e.created_at = "2026-09-01T00:00:00Z".into();

    let kept = crate::gc_native::apply_retire_surface(&e);
    assert!(
        matches!(kept, crate::daemon::CascadeOutcome::NotApplicable),
        "{kept:?}"
    );
    let kept_log = fake.argv_log();
    assert!(
        !kept_log.lines().any(|l| l.starts_with("rm ")),
        "the retire surface never rms: {kept_log}"
    );

    let removed = crate::gc_native::apply_active_surface_removal(&e);
    assert!(
        matches!(removed, crate::daemon::CascadeOutcome::Removed),
        "{removed:?}"
    );
    let removed_log = fake.argv_log();
    assert!(
        removed_log.lines().any(|l| l == "rm abcd1234"),
        "the removal surface still rms: {removed_log}"
    );
}

/// AC1-EDGE: an `update_registry` write that drops rows with no receipt on
/// disk stages both receipts naming the remover, and touches neither
/// harness: no `claude rm`, and the codex session index is byte-identical.
#[test]
fn an_update_registry_drop_stages_receipts_and_keeps_both_harnesses() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (dir, home) = staged_graph_home();
    let fake = FakeClaude::install("abcd1234", "abcd1234-1111-2222-3333-444444444444");
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root());
    let codex_home = tempfile::tempdir().unwrap();
    let old_codex_home = std::env::var_os("CODEX_HOME");
    std::env::set_var("CODEX_HOME", codex_home.path());
    let index = codex_home.path().join("session_index.jsonl");
    std::fs::write(
        &index,
        "{\"id\":\"sess-codex-kept\",\"name\":\"kept\"}\n{\"id\":\"sess-codex-dropped\",\"name\":\"dropped\"}\n",
    )
    .unwrap();
    let index_before = std::fs::read(&index).unwrap();

    crate::state::update_registry(&home.registry_json(), |r| {
        let mut claude_row = state::RegistryEntry::default();
        claude_row.name = "worker-edge".into();
        claude_row.short_id = "abcd1234".into();
        claude_row.origin = Some("spawn".into());
        claude_row.harness = Some("claude".into());
        claude_row.harness_session_id = Some("abcd1234-1111-2222-3333-444444444444".into());
        claude_row.created_at = "2026-09-01T00:00:00Z".into();
        let mut codex_row = state::RegistryEntry::default();
        codex_row.name = "worker-codex".into();
        codex_row.short_id = "codex99".into();
        codex_row.origin = Some("spawn".into());
        codex_row.harness = Some("codex".into());
        codex_row.harness_session_id = Some("sess-codex-dropped".into());
        codex_row.created_at = "2026-09-01T00:00:00Z".into();
        r.entries.push(claude_row);
        r.entries.push(codex_row);
    })
    .unwrap();
    crate::state::update_registry(&home.registry_json(), |r| r.entries.clear()).unwrap();

    let claude_receipt: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(crate::receipt::reap_receipt_path_for(
            &home,
            "claude",
            "abcd1234-1111-2222-3333-444444444444",
        ))
        .expect("the claude drop stages its receipt"),
    )
    .unwrap();
    assert!(
        claude_receipt["removed_by"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "the receipt names the remover: {claude_receipt}"
    );
    let codex_receipt: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(crate::receipt::reap_receipt_path_for(
            &home,
            "codex",
            "sess-codex-dropped",
        ))
        .expect("the codex drop stages its receipt"),
    )
    .unwrap();
    assert!(
        codex_receipt["removed_by"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "the receipt names the remover: {codex_receipt}"
    );
    let log = fake.argv_log();
    assert!(
        !log.lines().any(|l| l.starts_with("rm ")),
        "a registry write never rms the harness session: {log}"
    );
    assert_eq!(
        std::fs::read(&index).unwrap(),
        index_before,
        "the codex session index is byte-identical"
    );
    match &old_codex_home {
        Some(v) => std::env::set_var("CODEX_HOME", v),
        None => std::env::remove_var("CODEX_HOME"),
    }
}

/// AC1-PROC: the one cascade arm retirement keeps. A cursor-agent row with a
/// live worker-server child still has that leaked process reaped, while its
/// remote session state is untouched by definition. The reaped tree is
/// detached (the spawner exits, the branch reparents): a process the test
/// itself owns would sit as an unreaped zombie after the SIGTERM, and a
/// zombie answers the reap's survival probe.
#[cfg(unix)]
#[test]
fn retiring_a_cursor_agent_row_still_reaps_its_worker_server() {
    use std::io::BufRead;
    let bin_dir = tempfile::tempdir().unwrap();
    let worker_server = bin_dir.path().join("cursor-agent-worker-server");
    std::fs::write(&worker_server, "#!/bin/sh\nsleep 30\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&worker_server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let owner_script = bin_dir.path().join("owner.sh");
    std::fs::write(
        &owner_script,
        format!("#!/bin/sh\n'{}' 30 & wait\n", worker_server.display()),
    )
    .unwrap();
    std::fs::set_permissions(&owner_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The double fork: the outer shell prints the detached owner's pid and
    // exits, so the owner (and the worker server it holds) reparent to
    // launchd and nothing in the reaped tree is this test's child. The
    // owner's own stdio is cut: a shared stdout pipe would hold this read
    // open until the whole branch dies.
    let mut outer = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "'{}' >/dev/null 2>&1 & echo $!",
            owner_script.display()
        ))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut pid_line = String::new();
    std::io::BufReader::new(outer.stdout.take().unwrap())
        .read_line(&mut pid_line)
        .unwrap();
    let owner: u32 = pid_line.trim().parse().unwrap();
    let owner_start = crate::daemon::process_start_time(owner).unwrap();
    let _ = outer.wait();
    // The census reads `ps`; give the new branch a moment to appear in it.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut e = state::RegistryEntry::default();
    e.name = "cursor-row".into();
    e.short_id = "cursor-row".into();
    e.origin = Some("spawn".into());
    e.harness = Some("cursor-agent".into());
    e.pid = Some(owner);
    e.pid_start_time = Some(owner_start);
    e.created_at = "2026-09-01T00:00:00Z".into();

    let outcome = crate::gc_native::apply_retire_surface(&e);
    assert!(
        matches!(outcome, crate::daemon::CascadeOutcome::Removed),
        "the worker server was reaped: {outcome:?}"
    );
    // The owner shell's `wait` returns the moment its reaped child dies, and
    // the shell then exits: its death IS the death proof.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && crate::daemon::process_start_time(owner).is_some()
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        crate::daemon::process_start_time(owner).is_none(),
        "the leaked worker-server branch is gone"
    );
}

/// AC1-PLAN: the two planner retirement routes reach the same staging
/// function, so the same seam change covers them: a planner on a superseded
/// node and a halted planner both retire, and the fake `claude` logs no
/// `rm` for either.
#[test]
fn the_planner_routes_retire_without_rming_the_harness_session() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([
            {
                "id": "n-plan-a",
                "status": "superseded",
                "sessions": [{
                    "phase": "blueprint",
                    "harness": "claude",
                    "session_id": "aaaa1111-1111-2222-3333-444444444444",
                    "started_at": "2026-09-01T00:00:00Z",
                }],
            },
            {
                "id": "n-plan-b",
                "status": "ready",
                "sessions": [{
                    "phase": "blueprint",
                    "harness": "claude",
                    "session_id": "bbbb2222-1111-2222-3333-444444444444",
                    "started_at": "2026-09-01T00:00:00Z",
                }],
            },
        ]),
    );
    crate::state::update_registry(&home.registry_json(), |r| {
        let mut a = state::RegistryEntry::default();
        a.name = "bp-moved".into();
        a.short_id = "bpaaa111".into();
        a.origin = Some("spawn".into());
        a.harness = Some("claude".into());
        a.harness_session_id = Some("aaaa1111-1111-2222-3333-444444444444".into());
        a.created_at = "2026-09-01T00:00:00Z".into();
        let mut b = state::RegistryEntry::default();
        b.name = "bp-halted".into();
        b.short_id = "bpbbb222".into();
        b.origin = Some("spawn".into());
        b.harness = Some("claude".into());
        b.harness_session_id = Some("bbbb2222-1111-2222-3333-444444444444".into());
        b.created_at = "2026-09-01T00:00:00Z".into();
        b.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Done,
            seq: 4,
            reason: None,
            received_at: "2026-09-01T00:00:00Z".into(),
            ttl_ms: None,
        });
        r.entries.push(a);
        r.entries.push(b);
    })
    .unwrap();

    let fake_a = FakeClaude::install("bpaaa111", "aaaa1111-1111-2222-3333-444444444444");
    let _swap = EnvSwap::to(&fake_a.bin_dir(), &fake_a.daemon_dir(), home.root());
    let store_dir = home.root().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let quiet = quiet_transcript(&store_dir, "q.jsonl", 2 * 3600);
    let summary = production_sweep(&home, quiet);

    assert_eq!(summary.retired.len(), 2, "{:?}", summary.retired);
    let bases: Vec<&str> = summary.retired.iter().map(|(_, b)| b.as_str()).collect();
    assert!(
        bases
            .iter()
            .any(|b| *b == "planning finished on n-plan-a: node superseded"),
        "{bases:?}"
    );
    assert!(
        bases
            .iter()
            .any(|b| *b == "planning halted on n-plan-b: turn ended with no plan"),
        "{bases:?}"
    );
    let log = fake_a.argv_log();
    assert!(
        !log.lines().any(|l| l.starts_with("rm ")),
        "neither planner route rms the harness session: {log}"
    );
    assert!(
        log.lines().any(|l| l == "stop bpaaa111"),
        "the retirement stops each worker: {log}"
    );
}
