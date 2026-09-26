//! Retirement removes what it retires: the sweep stops a worker and the
//! same pass deletes the harness session. The active-surface seam runs for
//! real (a fake `claude` on PATH logs every argv it is asked for), so these
//! tests witness the commands retirement actually issues - a staged outcome
//! could never see an `rm` it never ran.

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
        Self::install_as(worker, session, false)
    }

    /// `pre_stopped` answers `stopped` from the first read, as if the
    /// harness finished the session on its own - no fno stop ever ran.
    fn install_as(worker: &str, session: &str, pre_stopped: bool) -> Self {
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
    elif [ -f "{flags}/stop_ran" ] || [ -f "{flags}/pre_stopped" ]; then
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
        crate::write_exec_stub(&bin_dir, "claude", &script);
        if pre_stopped {
            std::fs::write(flags.join("pre_stopped"), "").unwrap();
        }
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

/// PATH + daemon-dir + agents-home (+ optional HOME) swap, restored on
/// drop. The env lock serializes every test that mutates process state the
/// sweep reads. The agents home points at the sweep's own tmp root: the
/// stop runner resolves its state root eagerly, and an undeclared `$HOME`
/// root panics under test.
struct EnvSwap {
    old_path: Option<std::ffi::OsString>,
    old_daemon: Option<std::ffi::OsString>,
    old_agents_home: Option<std::ffi::OsString>,
    old_home: Option<std::ffi::OsString>,
}

impl EnvSwap {
    fn to(
        bin: &std::path::Path,
        daemon: &std::path::Path,
        agents_home: &std::path::Path,
        home: Option<&std::path::Path>,
    ) -> Self {
        let old_path = std::env::var_os("PATH");
        let old_daemon = std::env::var_os(crate::claude_roster::DAEMON_DIR_ENV);
        let old_agents_home = std::env::var_os("FNO_AGENTS_HOME");
        let old_home = std::env::var_os("HOME");
        let joined = format!(
            "{}:{}",
            bin.display(),
            old_path.as_deref().unwrap_or_default().to_string_lossy()
        );
        std::env::set_var("PATH", joined);
        std::env::set_var(crate::claude_roster::DAEMON_DIR_ENV, daemon);
        std::env::set_var("FNO_AGENTS_HOME", agents_home);
        if let Some(home) = home {
            std::env::set_var("HOME", home);
        }
        Self {
            old_path,
            old_daemon,
            old_agents_home,
            old_home,
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
        match &self.old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
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
        &crate::gc_native::apply_active_surface_removal,
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

/// AC1-HP: retiring a claude thread row stops the worker and removes the
/// harness session: the confirmed stop runs before the `rm`, the job dir is
/// gone, and the receipt's active-surface effect reads confirmed-removed.
/// The production seam function runs for real - a staged outcome could never
/// witness the `rm` this test exists to catch.
#[test]
fn retiring_a_claude_thread_row_removes_the_harness_session() {
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
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root(), None);

    let summary = production_sweep(&home, quiet);

    assert_eq!(
        summary.retired.len(),
        1,
        "the quiet row retires: {:?}",
        summary.retired
    );
    let log = fake.argv_log();
    let lines: Vec<&str> = log.lines().collect();
    let stop = lines
        .iter()
        .position(|l| *l == "stop abcd1234")
        .expect("the confirmed stop ran");
    let rm = lines
        .iter()
        .position(|l| *l == "rm abcd1234")
        .expect("retirement removes the harness session");
    assert!(stop < rm, "the stop precedes the rm: {log}");
    assert!(
        !fake.job_state("abcd1234").exists(),
        "the job dir is gone with the session"
    );
    assert_eq!(
        staged_active_surface(&home, "claude", "abcd1234-1111-2222-3333-444444444444").as_deref(),
        Some("confirmed-removed"),
        "the receipt names the session removed"
    );
}

/// AC3-HP: the codex arm of the removal cascade edits the harness's own
/// session index: the dropped row's line goes, every other session's line
/// stays.
#[test]
fn retiring_a_codex_row_removes_its_session_index_line() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let codex_home = tempfile::tempdir().unwrap();
    let old_codex_home = std::env::var_os("CODEX_HOME");
    std::env::set_var("CODEX_HOME", codex_home.path());
    let index = codex_home.path().join("session_index.jsonl");
    std::fs::write(
        &index,
        "{\"session_id\":\"sess-codex-kept\",\"name\":\"kept\"}\n{\"session_id\":\"sess-codex-dropped\",\"name\":\"dropped\"}\n",
    )
    .unwrap();

    let mut e = state::RegistryEntry::default();
    e.name = "worker-codex".into();
    e.short_id = "codex99".into();
    e.origin = Some("spawn".into());
    e.harness = Some("codex".into());
    e.harness_session_id = Some("sess-codex-dropped".into());
    e.created_at = "2026-09-01T00:00:00Z".into();

    let outcome = crate::gc_native::apply_active_surface_removal(&e);
    assert!(
        matches!(outcome, crate::daemon::CascadeOutcome::Removed),
        "{outcome:?}"
    );
    let after = std::fs::read_to_string(&index).unwrap();
    assert!(
        !after.contains("sess-codex-dropped"),
        "the dropped session's line is gone: {after}"
    );
    assert!(
        after.contains("sess-codex-kept"),
        "the sibling session's line stays: {after}"
    );
    match &old_codex_home {
        Some(v) => std::env::set_var("CODEX_HOME", v),
        None => std::env::remove_var("CODEX_HOME"),
    }
}

/// AC1-EDGE: an `update_registry` write that drops rows with no receipt on
/// disk stages both receipts naming the remover, and removes both harness
/// sessions: the fake `claude` logs the `rm`, and the codex session index
/// loses only the dropped row's line.
#[test]
fn an_update_registry_drop_stages_receipts_and_removes_both_harness_sessions() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_dir, home) = staged_graph_home();
    let fake = FakeClaude::install("abcd1234", "abcd1234-1111-2222-3333-444444444444");
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root(), None);
    let codex_home = tempfile::tempdir().unwrap();
    let old_codex_home = std::env::var_os("CODEX_HOME");
    std::env::set_var("CODEX_HOME", codex_home.path());
    let index = codex_home.path().join("session_index.jsonl");
    std::fs::write(
        &index,
        "{\"session_id\":\"sess-codex-kept\",\"name\":\"kept\"}\n{\"session_id\":\"sess-codex-dropped\",\"name\":\"dropped\"}\n",
    )
    .unwrap();

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
    let claude_effects = claude_receipt["effects"]
        .as_array()
        .expect("effects recorded");
    assert_eq!(
        claude_effects.len(),
        1,
        "one active-surface effect on the claude receipt: {claude_receipt}"
    );
    assert_eq!(claude_effects[0]["op"], "active-surface");
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
    let codex_effects = codex_receipt["effects"]
        .as_array()
        .expect("effects recorded");
    assert_eq!(
        codex_effects.len(),
        1,
        "one active-surface effect on the codex receipt: {codex_receipt}"
    );
    assert_eq!(codex_effects[0]["op"], "active-surface");
    let log = fake.argv_log();
    assert!(
        log.lines().any(|l| l == "rm abcd1234"),
        "the write door removes the claude session: {log}"
    );
    let index_after = std::fs::read_to_string(&index).unwrap();
    assert!(
        !index_after.contains("sess-codex-dropped"),
        "the codex index loses the dropped session: {index_after}"
    );
    assert!(
        index_after.contains("sess-codex-kept"),
        "the codex index keeps the sibling session: {index_after}"
    );
    match &old_codex_home {
        Some(v) => std::env::set_var("CODEX_HOME", v),
        None => std::env::remove_var("CODEX_HOME"),
    }
}

/// AC1-PROC: a cursor-agent row's cascade reaps its leaked worker-server
/// children. The reaped tree is
/// detached (the spawner exits, the branch reparents): a process the test
/// itself owns would sit as an unreaped zombie after the SIGTERM, and a
/// zombie answers the reap's survival probe.
#[cfg(unix)]
#[test]
fn retiring_a_cursor_agent_row_still_reaps_its_worker_server() {
    use std::io::BufRead;
    let bin_dir = tempfile::tempdir().unwrap();
    let worker_server = crate::write_exec_stub(
        bin_dir.path(),
        "cursor-agent-worker-server",
        "#!/bin/sh\nsleep 30\n",
    );
    let owner_script = crate::write_exec_stub(
        bin_dir.path(),
        "owner.sh",
        &format!("#!/bin/sh\n'{}' 30 & wait\n", worker_server.display()),
    );
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

    let outcome = crate::gc_native::apply_active_surface_removal(&e);
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
/// node and a halted planner both retire, and the removal runs for the row
/// the fake roster lists - the row it does not list reads already-absent
/// and still retires.
#[test]
fn the_planner_routes_retire_and_rm_the_harness_session() {
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
    let _swap = EnvSwap::to(&fake_a.bin_dir(), &fake_a.daemon_dir(), home.root(), None);
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
        log.lines().any(|l| l == "rm bpaaa111"),
        "the listed planner's session is removed: {log}"
    );
    assert!(
        log.lines().any(|l| l == "stop bpaaa111"),
        "the retirement stops each worker: {log}"
    );
}

/// The `agent_row_reaped` event this sweep emitted for one row, read off
/// the events journal.
fn reaped_event(home: &AgentsHome, short_id: &str) -> Option<serde_json::Value> {
    let events = crate::events::committed_journal_text(&home.events_jsonl());
    events
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == "agent_row_reaped" && event["data"]["short_id"] == short_id)
        .map(|event| event["data"].clone())
}

/// AC2-HP: the event's `resumable` verdict is measured off the staged
/// receipt, never a constant. A transcript present in the row's own store
/// reads resumable true, with the basis named.
#[test]
fn the_reaped_event_measures_resumable_off_the_receipt() {
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
    // The transcript staged where the REAL store index looks: a temp HOME
    // whose projects tree holds one quiet `<session id>.jsonl`.
    let store_home = tempfile::tempdir().unwrap();
    let projects = store_home
        .path()
        .join(".claude")
        .join("projects")
        .join("work");
    std::fs::create_dir_all(&projects).unwrap();
    let store_transcript = quiet_transcript(
        &projects,
        "abcd1234-1111-2222-3333-444444444444.jsonl",
        2 * 3600,
    );
    let fake = FakeClaude::install("abcd1234", "abcd1234-1111-2222-3333-444444444444");
    let _swap = EnvSwap::to(
        &fake.bin_dir(),
        &fake.daemon_dir(),
        home.root(),
        Some(store_home.path()),
    );
    let store_dir = home.root().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let quiet = quiet_transcript(&store_dir, "q.jsonl", 2 * 3600);
    let summary = production_sweep(&home, quiet);

    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    let event = reaped_event(&home, "abcd1234").expect("the retirement emits its event");
    assert_eq!(event["resumable"], true, "{event}");
    assert_eq!(event["resumable_basis"], "transcript-present", "{event}");
    // The store transcript is the locator's evidence, so it is the file the
    // basis names; the classification transcript only staged the quiet.
    assert!(store_transcript.exists());
}

/// AC2-ERR: a transcript gone from the row's own store reads resumable
/// false with the `no-transcript` basis, even though the receipt itself
/// staged fine.
#[test]
fn a_transcript_gone_reads_no_transcript_even_with_a_receipt() {
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
    // The real store reads EMPTY: the temp HOME holds no projects tree.
    let store_home = tempfile::tempdir().unwrap();
    let fake = FakeClaude::install("abcd1234", "abcd1234-1111-2222-3333-444444444444");
    let _swap = EnvSwap::to(
        &fake.bin_dir(),
        &fake.daemon_dir(),
        home.root(),
        Some(store_home.path()),
    );
    let store_dir = home.root().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let quiet = quiet_transcript(&store_dir, "q.jsonl", 2 * 3600);
    let summary = production_sweep(&home, quiet);

    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    let event = reaped_event(&home, "abcd1234").expect("the retirement emits its event");
    assert_eq!(event["resumable"], false, "{event}");
    assert_eq!(event["resumable_basis"], "no-transcript", "{event}");
}

/// One open-work claude row whose harness already reads `stopped`, with or
/// without fno's own stop record.
fn stage_stopped_row(
    dir: &std::path::Path,
    home: &AgentsHome,
    name: &str,
    worker: &str,
    session: &str,
    node_status: &str,
    phase: &str,
    with_stop: bool,
) {
    stage_graph(
        dir,
        json!([{
            "id": "n-open",
            "status": node_status,
            "sessions": [{
                "phase": phase,
                "harness": "claude",
                "session_id": session,
                "started_at": "2026-09-01T00:00:00Z",
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
        if with_stop {
            e.stop = Some(state::StopRecord {
                by: "stop-verb".into(),
                at: "2026-09-15T00:00:00Z".into(),
                reason: None,
            });
        }
        r.entries.push(e);
    })
    .unwrap();
}

/// AC3-HP: a row fno stopped (a stop record on the row) whose harness state
/// reads `stopped` keeps for open work - the record is fno's memory that IT
/// ended the session, so `stopped` never reads as finished work.
#[test]
fn a_row_fno_stopped_keeps_for_open_work() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (dir, home) = staged_graph_home();
    stage_stopped_row(
        dir.path(),
        &home,
        "worker-stopped",
        "abcd1234",
        "abcd1234-1111-2222-3333-444444444444",
        "in_progress",
        "do",
        true,
    );
    let fake = FakeClaude::install_as("abcd1234", "abcd1234-1111-2222-3333-444444444444", true);
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root(), None);
    let store_dir = home.root().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let quiet = quiet_transcript(&store_dir, "q.jsonl", 2 * 3600);
    let summary = production_sweep(&home, quiet);

    assert!(
        summary.retired.is_empty(),
        "a stopped-by-fno row never retires: {:?}",
        summary.retired
    );
    assert!(
        summary
            .kept_open_work_stale
            .iter()
            .any(|(id, _, _, _)| id == "abcd1234"),
        "the row keeps for open work: {:?}",
        summary.kept_open_work_stale
    );
    assert!(
        !summary
            .retired
            .iter()
            .any(|(_, basis)| basis.contains("session terminal")),
        "no session-terminal basis: {:?}",
        summary.retired
    );
    assert!(
        !fake.argv_log().lines().any(|l| l.starts_with("rm ")),
        "restored removal never reaches a kept open-work row: {}",
        fake.argv_log()
    );
}

/// AC3-HP, planner half: a planner fno stopped holds as PlanningUnclosed
/// (its node still needs its plan), never retires as halted-finished.
#[test]
fn a_planner_fno_stopped_holds_as_unclosed() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (dir, home) = staged_graph_home();
    stage_stopped_row(
        dir.path(),
        &home,
        "bp-stopped",
        "bpabcd12",
        "bbbb3333-1111-2222-3333-444444444444",
        "ready",
        "blueprint",
        true,
    );
    crate::state::update_registry(&home.registry_json(), |r| {
        if let Some(row) = r.entries.iter_mut().find(|e| e.short_id == "bpabcd12") {
            row.inside_leg = Some(state::InsideLegReport {
                state: state::InsideLegState::Done,
                seq: 4,
                reason: None,
                received_at: "2026-09-01T00:00:00Z".into(),
                ttl_ms: None,
            });
        }
    })
    .unwrap();
    let fake = FakeClaude::install_as("bpabcd12", "bbbb3333-1111-2222-3333-444444444444", true);
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root(), None);
    let store_dir = home.root().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let quiet = quiet_transcript(&store_dir, "q.jsonl", 2 * 3600);
    let summary = production_sweep(&home, quiet);

    assert!(
        summary.retired.is_empty(),
        "a stopped planner never retires: {:?}",
        summary.retired
    );
    assert!(
        summary
            .kept_planning_unclosed
            .iter()
            .any(|(id, _)| id == "bpabcd12"),
        "the planner holds as unclosed: {:?}",
        summary.kept_planning_unclosed
    );
}

/// AC3-EDGE: the same row with NO stop record - the harness stopped itself -
/// still takes today's terminal release and retires.
#[test]
fn a_row_the_harness_stopped_itself_still_releases() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (dir, home) = staged_graph_home();
    stage_stopped_row(
        dir.path(),
        &home,
        "worker-selfstopped",
        "abcd1234",
        "abcd1234-1111-2222-3333-444444444444",
        "in_progress",
        "do",
        false,
    );
    let fake = FakeClaude::install_as("abcd1234", "abcd1234-1111-2222-3333-444444444444", true);
    let _swap = EnvSwap::to(&fake.bin_dir(), &fake.daemon_dir(), home.root(), None);
    let store_dir = home.root().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let quiet = quiet_transcript(&store_dir, "q.jsonl", 2 * 3600);
    let summary = production_sweep(&home, quiet);

    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    assert!(
        summary.retired[0]
            .1
            .contains("session terminal: harness state stopped"),
        "{:?}",
        summary.retired[0]
    );
}

/// The wiring tripwire: each retirement door's source must pass
/// `apply_active_surface_removal` as its active-surface seam. These doors
/// call a live truth probe or need git trees, so no unit test drives them
/// end to end; this read is the one in-process check that fails, naming the
/// file and door, when a door is swapped to a seam that removes nothing.
#[test]
fn every_retirement_door_wires_the_removal_cascade() {
    let doors: &[(&str, &str, &str, &str)] = &[
        (
            "gc.rs",
            "gc_sweep",
            "pub fn gc_sweep(",
            include_str!("../../../gc.rs"),
        ),
        (
            "gc.rs",
            "gc_sweep_release",
            "pub fn gc_sweep_release(",
            include_str!("../../../gc.rs"),
        ),
        (
            "gc.rs",
            "gc_sweep_dry_run",
            "pub fn gc_sweep_dry_run(",
            include_str!("../../../gc.rs"),
        ),
        (
            "roster_reap.rs",
            "roster_reap",
            "pub fn roster_reap(",
            include_str!("../../../roster_reap.rs"),
        ),
        (
            "merge_reap.rs",
            "consume_merge_cleanup_requests",
            "pub(crate) fn consume_merge_cleanup_requests(",
            include_str!("../../../merge_reap.rs"),
        ),
    ];
    for (file, door, sig, src) in doors {
        let start = src
            .find(sig)
            .unwrap_or_else(|| panic!("signature for {door} not found in {file}"));
        let rest = &src[start..];
        let end = rest.find("\npub").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("crate::gc_native::apply_active_surface_removal"),
            "{file}: door {door} must wire the removal cascade (apply_active_surface_removal)"
        );
    }
}
