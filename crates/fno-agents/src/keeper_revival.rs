//! How an exited keeper-lane thread row comes back.
//!
//! After the resume door proves the row's contract carries a same-id revival
//! (`[harness.<h>.keeper]` lists `resume_session_id`), this module composes
//! the pieces that already exist: the spawn gate's revival admission
//! (`resume_gate::admit_revival`), the keeper socket probe
//! (`daemon::probe_keeper_socket`), the worker binary
//! (`daemon::resolve_worker_bin`), the thread socket path and row flip
//! (`convert::keeper_rebind`), and the keeper mail lane. It starts
//! `fno-agents-worker --keeper` on the row's own `interactive_resume` form,
//! proves the child stayed up, and flips the SAME registry row live.
//!
//! Port seam: the Python spawn lane (`_lane_b_thread_spawn`) is the other
//! keeper launcher. When the keeper spawn ports to Rust it should call this
//! module's launch step with an `interactive_create` form and delete the
//! Python leg.

use crate::harness_capabilities::HarnessContract;
use crate::state::RegistryEntry;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long the revival waits for the fresh keeper's first Identify: the
/// same bound the Python lane-B spawn allows its keeper to answer in.
const IDENTIFY_WINDOW: Duration = Duration::from_secs(10);
/// Poll cadence while waiting for that first Identify.
const IDENTIFY_POLL: Duration = Duration::from_millis(250);

/// Everything the launcher spawns: the full keeper argv (worker binary
/// first), where it runs, where its stderr goes, and the identity it is
/// stamped with. Built before launch so a test's launcher can record it.
pub(crate) struct LaunchSpec {
    pub argv: Vec<String>,
    pub sock: PathBuf,
    pub cwd: PathBuf,
    pub keeper_log: PathBuf,
    pub agent_name: String,
    pub harness: String,
    pub session_id: String,
}

/// The keeper child argv and the effective posture word. `base` is the
/// rendered `interactive_resume` form the door built; the posture tokens and
/// the model axis are the only additions, so the revival relaunches the same
/// conversation the contract names instead of re-deriving the row's extras.
pub(crate) fn revival_argv(
    contract: &HarnessContract,
    harness: &str,
    row: &RegistryEntry,
    base: &[String],
) -> Result<(Vec<String>, String), String> {
    let mut argv = base.to_vec();
    let posture = crate::agy_launch::keeper_posture(
        harness,
        "thread",
        row.requested_permission_mode.as_deref(),
        false,
    )?;
    argv.extend(posture.tokens);
    if let Some(model) = revival_model(contract, harness, row) {
        argv.push("--model".to_string());
        argv.push(model);
    }
    Ok((argv, posture.effective))
}

/// The model axis the keeper lane takes, the row's requested model first -
/// the same precedence a fresh spawn resolves. Empty answers never ride.
fn revival_model(contract: &HarnessContract, harness: &str, row: &RegistryEntry) -> Option<String> {
    let takes_model = contract
        .capabilities(harness)
        .ok()
        .and_then(|caps| caps.keeper.as_ref())
        .map(|keeper| keeper.takes_model)
        .unwrap_or(false);
    if !takes_model {
        return None;
    }
    row.requested_model
        .clone()
        .or_else(|| row.model.clone())
        .filter(|m| !m.is_empty())
}

/// The production wrapper: real gate, real probe, real launcher. Loads the
/// row fresh so the posture reads the registry's own answer, and derives the
/// thread socket path the keeper sweep binds by.
pub(crate) fn revive(
    home: &crate::paths::AgentsHome,
    row_name: &str,
    harness: &str,
    session_id: &str,
    cwd: &str,
    base_argv: &[String],
    message: Option<&str>,
) -> i32 {
    let row = match crate::state::load_registry(&home.registry_json())
        .ok()
        .and_then(|registry| registry.entries.into_iter().find(|e| e.name == row_name))
    {
        Some(row) => row,
        None => {
            eprintln!("fno agents resume: row {row_name} is no longer in the registry");
            return 13;
        }
    };
    let state_root = home.root().parent().unwrap_or(home.root());
    let sock = crate::convert::keeper_rebind::thread_socket_path(state_root, row_name);
    revive_with(
        home,
        &row,
        harness,
        session_id,
        cwd,
        base_argv,
        message,
        &sock,
        || crate::resume_gate::admit_revival(home, "resume", row_name, Path::new(cwd)),
        |sock| crate::daemon::probe_keeper_socket(sock, Duration::from_secs(2)),
        launch_keeper,
        terminate_keeper,
    )
}

/// The seam the unit tests drive: every machine touch arrives as a closure,
/// so the ordering and the guards here are provable without a gate daemon or
/// a real keeper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn revive_with<A, P, L, K>(
    home: &crate::paths::AgentsHome,
    row: &RegistryEntry,
    harness: &str,
    session_id: &str,
    cwd: &str,
    base_argv: &[String],
    message: Option<&str>,
    sock: &Path,
    admit: A,
    probe: P,
    mut launch: L,
    kill: K,
) -> i32
where
    A: FnOnce() -> Result<crate::spawn_gate::GateGuard, i32>,
    P: Fn(&Path) -> crate::daemon::KeeperProbe,
    L: FnMut(&LaunchSpec) -> Result<u32, String>,
    K: Fn(u32),
{
    let row_name = &row.name;
    // Held to the end: this is the machine-wide spawn-gate mutex on the bg
    // substrate, so it also serializes two concurrent revivals of one row.
    let _admission = match admit() {
        Ok(guard) => guard,
        Err(code) => {
            crate::resume_gate::release_revival_claims(session_id);
            return code;
        }
    };
    let contract = match HarnessContract::packaged() {
        Ok(contract) => contract,
        Err(error) => {
            eprintln!("fno agents resume: resume contract is unavailable: {error}");
            crate::resume_gate::release_revival_claims(session_id);
            return 13;
        }
    };
    let (completed_argv, posture_word) = match revival_argv(&contract, harness, row, base_argv) {
        Ok(completed) => completed,
        Err(reason) => {
            eprintln!("fno agents resume: {row_name}: {reason}");
            crate::resume_gate::release_revival_claims(session_id);
            return 13;
        }
    };
    let keeper_log = home.root().join(row_name).join("keeper.log");
    // A live keeper at this path is the session itself; silence is never
    // death; a file with nobody behind it is a kill -9 remainder.
    match probe(sock) {
        crate::daemon::KeeperProbe::Answered(reply) => {
            let (answered, _, _) = reply_parts(&reply);
            let answered = answered.as_deref().unwrap_or("");
            if answered == session_id {
                println!(
                    "fno agents resume: {row_name} is already live on its keeper at {}; \
                     nothing was launched",
                    sock.display()
                );
                return 0;
            }
            eprintln!(
                "fno agents resume: {row_name}: the keeper at {} answers session \
                 {answered:?}, not the resumed {session_id:?}; nothing was launched",
                sock.display()
            );
            crate::resume_gate::release_revival_claims(session_id);
            return 13;
        }
        crate::daemon::KeeperProbe::Silent => {
            eprintln!(
                "fno agents resume: {row_name}: the keeper at {} accepted the probe and \
                 stayed silent; silence never proves death, so nothing was launched",
                sock.display()
            );
            crate::resume_gate::release_revival_claims(session_id);
            return 13;
        }
        crate::daemon::KeeperProbe::NoListener => {
            let _ = std::fs::remove_file(sock);
        }
    }
    let mut argv = vec![crate::daemon::resolve_worker_bin()
        .to_string_lossy()
        .into_owned()];
    argv.extend([
        "--keeper".to_string(),
        "--sock".to_string(),
        sock.to_string_lossy().into_owned(),
        "--session".to_string(),
        row_name.clone(),
        "--pane-key".to_string(),
        session_id.to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
        "--".to_string(),
    ]);
    argv.extend(completed_argv);
    let spec = LaunchSpec {
        argv,
        sock: sock.to_path_buf(),
        cwd: PathBuf::from(cwd),
        keeper_log: keeper_log.clone(),
        agent_name: row_name.clone(),
        harness: harness.to_string(),
        session_id: session_id.to_string(),
    };
    let launched = match launch(&spec) {
        Ok(pid) => pid,
        Err(reason) => {
            eprintln!(
                "fno agents resume: {row_name}: failed to launch the keeper on {}: {reason}; \
                 see {}",
                sock.display(),
                keeper_log.display()
            );
            crate::resume_gate::release_revival_claims(session_id);
            return 1;
        }
    };
    // The keeper binds the socket only after hosting the child, so the first
    // Identify that names BOTH this session id AND this keeper pid proves the
    // launch is ours. Another pid answering the same session id means a
    // concurrent revival won the socket; ours exited refusing to bind a
    // served socket, and the winner owns the row flip.
    let deadline = std::time::Instant::now() + IDENTIFY_WINDOW;
    let mut verified: Option<(u32, Option<u32>)> = None;
    while verified.is_none() {
        if let crate::daemon::KeeperProbe::Answered(reply) = probe(sock) {
            let (answered_sid, keeper_pid, child_pid) = reply_parts(&reply);
            match (answered_sid, keeper_pid) {
                (Some(s), Some(p)) if s == session_id && p == launched => {
                    verified = Some((p, child_pid));
                }
                (Some(s), Some(_)) if s == session_id => {
                    println!(
                        "fno agents resume: {row_name} is live on its keeper at {} \
                         (a concurrent resume revived it); nothing more was launched",
                        sock.display()
                    );
                    return 0;
                }
                (answered, keeper_pid) => {
                    kill(launched);
                    eprintln!(
                        "fno agents resume: {row_name}: the keeper at {} answered session \
                         {answered:?} pid {keeper_pid:?}, not this revival (pid {launched}); \
                         see {}",
                        sock.display(),
                        keeper_log.display()
                    );
                    crate::resume_gate::release_revival_claims(session_id);
                    return 1;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            kill(launched);
            eprintln!(
                "fno agents resume: {row_name}: the keeper at {} never answered Identify \
                 within {}s; see {}",
                sock.display(),
                IDENTIFY_WINDOW.as_secs(),
                keeper_log.display()
            );
            crate::resume_gate::release_revival_claims(session_id);
            return 1;
        }
        std::thread::sleep(IDENTIFY_POLL);
    }
    // Proof of life, then the flip: a keeper that exits right after launch
    // unlinks its socket, and a row flipped onto that corpse would read live
    // while hosting nothing.
    std::thread::sleep(crate::pane_relaunch::PANE_PROOF_WINDOW);
    if let crate::daemon::KeeperProbe::NoListener = probe(sock) {
        let tail = last_line(&keeper_log);
        append_resume_event(
            home,
            "agent_resume_failed",
            row_name,
            harness,
            session_id,
            cwd,
            sock,
            Some("keeper-child-exited"),
        );
        eprintln!(
            "fno agents resume: {row_name}: the {harness} child exited within {}s; the row \
             is unchanged. Last keeper.log line: {tail}",
            crate::pane_relaunch::PANE_PROOF_WINDOW.as_secs()
        );
        crate::resume_gate::release_revival_claims(session_id);
        return 1;
    }
    let Some((keeper_pid, child_pid)) = verified else {
        unreachable!("the identify loop leaves only on the verified answer")
    };
    let outcome = crate::convert::keeper_rebind::RebindOutcome {
        socket: sock.to_string_lossy().into_owned(),
        keeper_pid,
        child_pid: child_pid.unwrap_or(0),
        session_id: session_id.to_string(),
        keeper_start_time: crate::daemon::process_start_time(keeper_pid),
    };
    // Compare-and-set: the row must still exist, must not be live, and must
    // still carry the resumed session id. Guards read before any mutation
    // (the write path persists whatever the closure left behind), so a failed
    // guard leaves the registry exactly as the other writer left it - and no
    // keeper runs that no row tracks.
    let flip = crate::state::update_registry(&home.registry_json(), |reg| -> Result<(), String> {
        let entry = reg
            .entries
            .iter_mut()
            .find(|e| &e.name == row_name)
            .ok_or_else(|| format!("row {row_name} is no longer in the registry"))?;
        if entry.status == crate::AgentStatus::Live {
            return Err("the row is already live; the reconcile sweep owns liveness".to_string());
        }
        if entry.harness_session_id.as_deref() != Some(session_id) {
            return Err(format!(
                "the row now carries session {:?}, not the resumed {session_id:?}",
                entry.harness_session_id.as_deref().unwrap_or("")
            ));
        }
        entry.status = crate::AgentStatus::Live;
        crate::convert::keeper_rebind::flipped_row(entry, &outcome);
        Ok(())
    });
    // The closure's own guard refusal rides back nested: `Ok(Err(reason))`.
    let flip_failure = match flip {
        Err(error) => Err(error.to_string()),
        Ok(Err(reason)) => Err(reason),
        Ok(Ok(())) => Ok(()),
    };
    if let Err(reason) = flip_failure {
        kill(keeper_pid);
        eprintln!(
            "fno agents resume: {row_name}: the keeper was stopped and the row left as the \
             other writer left it: {reason}"
        );
        crate::resume_gate::release_revival_claims(session_id);
        return 13;
    }
    append_resume_event(
        home,
        "agent_resumed",
        row_name,
        harness,
        session_id,
        cwd,
        sock,
        None,
    );
    println!(
        "fno agents resume: {row_name} is live on its keeper {} (keeper pid {keeper_pid}, \
         child pid {}, posture {posture_word}); fno mux thread {row_name} opens a view",
        sock.display(),
        outcome.child_pid
    );
    if let Some(text) = message.map(str::trim).filter(|t| !t.is_empty()) {
        if let Err(reason) = crate::mail_inject::deliver_via_keeper_socket(
            session_id,
            text,
            crate::mail_inject::DEFAULT_ATTEMPTS,
            crate::mail_inject::DEFAULT_INTERVAL_MS,
            crate::mail_inject::enter_delay_for_harness(harness),
        ) {
            eprintln!(
                "fno agents resume: revived {row_name}, but the message was not delivered: \
                 {reason}; resend with fno agents mail send {row_name}"
            );
            return 1;
        }
    }
    0
}

/// The identify reply's three facts, each read back and never assumed:
/// session id re-derived from the argv (the same read the keeper sweep and
/// the rebind verify apply), the keeper pid, and the child pid.
fn reply_parts(reply: &Value) -> (Option<String>, Option<u32>, Option<u32>) {
    let argv: Vec<String> = reply
        .get("argv")
        .and_then(Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let pid = |key: &str| {
        reply
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
    };
    (
        crate::pane_keeper::session_id_from_argv(&argv),
        pid("keeper_pid"),
        pid("child_pid"),
    )
}

fn last_line(path: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return "(unreadable)".to_string();
    };
    content
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(empty)")
        .to_string()
}

fn append_resume_event(
    home: &crate::paths::AgentsHome,
    kind: &str,
    row_name: &str,
    harness: &str,
    session_id: &str,
    cwd: &str,
    sock: &Path,
    reason: Option<&str>,
) {
    let mut fields: Vec<(&str, Value)> = vec![
        ("name", Value::String(row_name.to_string())),
        ("provider", Value::String(harness.to_string())),
        ("session_id", Value::String(session_id.to_string())),
        ("cwd", Value::String(cwd.to_string())),
        ("substrate", Value::String("thread".to_string())),
        ("socket", Value::String(sock.to_string_lossy().into_owned())),
    ];
    if let Some(reason) = reason {
        fields.push(("reason", Value::String(reason.to_string())));
    }
    crate::client_verbs::append_agents_event(
        &crate::client_verbs::trace_events_path(home),
        kind,
        &fields,
    );
}

/// The production launcher: a fresh session for the keeper (the same
/// detachment `start_new_session=True` gives the Python spawn), stdio dark
/// except stderr appended to the keeper log the Python lane writes, and the
/// identity stamp a spawn child carries.
fn launch_keeper(spec: &LaunchSpec) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]);
    command.current_dir(&spec.cwd);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    if let Some(parent) = spec.keeper_log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.keeper_log)
        .map_err(|e| format!("open {}: {e}", spec.keeper_log.display()))?;
    command.stderr(std::process::Stdio::from(log));
    // The keeper binds the socket itself but never makes its directory; the
    // Python spawn lane mkdirs the same parent before its Popen.
    if let Some(parent) = spec.sock.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    command.process_group(0);
    crate::claims::stamp_command_env(
        &mut command,
        Some(&spec.agent_name),
        &spec.harness,
        Some(&spec.session_id),
    );
    command.env("FNO_AGENT_SESSION_ID", &spec.session_id);
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|e| format!("spawn {}: {e}", spec.argv[0]))
}

/// Stop a keeper this revival started or named: SIGTERM, then SIGKILL once
/// the grace passes, so a wedged keeper never outlives its refusal.
fn terminate_keeper(pid: u32) {
    // SAFETY: pid targets the keeper this process launched; SIGTERM carries
    // no payload.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..20 {
        // This process launched the keeper, so waitpid reaps the corpse a
        // kill(pid, 0) probe would still see (a zombie answers the existence
        // probe). ECHILD means it was never ours: fall back to the probe.
        // SAFETY: WNOHANG wait and signal 0 deliver nothing.
        let reaped =
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        let gone = reaped == pid as libc::pid_t
            || (reaped < 0 && unsafe { libc::kill(pid as libc::pid_t, 0) } != 0);
        if gone {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: SIGKILL carries no payload.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    fn agy_row() -> RegistryEntry {
        RegistryEntry {
            name: "t-revive".to_string(),
            harness: Some("agy".to_string()),
            harness_session_id: Some(SID.to_string()),
            cwd: "/tmp".to_string(),
            status: crate::AgentStatus::Exited,
            created_at: "2026-09-22T00:00:00Z".to_string(),
            ..Default::default()
        }
    }

    fn base_argv() -> Vec<String> {
        vec![
            "agy".to_string(),
            "--conversation".to_string(),
            SID.to_string(),
        ]
    }

    fn harness() -> HarnessContract {
        HarnessContract::packaged().expect("packaged harness contract")
    }

    fn home() -> (tempfile::TempDir, crate::paths::AgentsHome) {
        let dir = tempfile::tempdir().unwrap();
        let home = crate::paths::AgentsHome::at(dir.path().join("agents"));
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(
            home.registry_json(),
            serde_json::to_vec(&crate::state::Registry {
                schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
                entries: vec![agy_row()],
            })
            .unwrap(),
        )
        .unwrap();
        (dir, home)
    }

    fn identify(keeper_pid: u32, child_pid: u32) -> Value {
        identify_for(SID, keeper_pid, child_pid)
    }

    fn identify_for(sid: &str, keeper_pid: u32, child_pid: u32) -> Value {
        json!({
            "keeper_pid": keeper_pid,
            "child_pid": child_pid,
            "argv": ["agy", "--conversation", sid, "--dangerously-skip-permissions"],
        })
    }

    /// The probe answers each listed verdict in order, then NoListener
    /// forever - the shape of a probe that watches a real socket come and go.
    fn probe_steps(
        steps: Vec<crate::daemon::KeeperProbe>,
    ) -> impl Fn(&Path) -> crate::daemon::KeeperProbe {
        let next = Mutex::new(steps.into_iter());
        move |_| {
            next.lock()
                .unwrap()
                .next()
                .unwrap_or(crate::daemon::KeeperProbe::NoListener)
        }
    }

    fn no_admit() -> Result<crate::spawn_gate::GateGuard, i32> {
        Ok(crate::spawn_gate::GateGuard::default())
    }

    struct Harness {
        launched: Mutex<Vec<Vec<String>>>,
        killed: Mutex<Vec<u32>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                launched: Mutex::new(Vec::new()),
                killed: Mutex::new(Vec::new()),
            }
        }
        fn launch(&self, spec: &LaunchSpec) -> Result<u32, String> {
            self.launched.lock().unwrap().push(spec.argv.clone());
            Ok(4242)
        }
        fn kill(&self, pid: u32) {
            self.killed.lock().unwrap().push(pid);
        }
        fn argv(&self) -> Vec<String> {
            self.launched
                .lock()
                .unwrap()
                .first()
                .cloned()
                .unwrap_or_default()
        }
    }

    fn events_of(home: &crate::paths::AgentsHome) -> String {
        std::fs::read_to_string(crate::client_verbs::trace_events_path(home)).unwrap_or_default()
    }

    fn registry_bytes(home: &crate::paths::AgentsHome) -> Vec<u8> {
        std::fs::read(home.registry_json()).unwrap()
    }

    fn read_row(home: &crate::paths::AgentsHome) -> RegistryEntry {
        crate::state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn revival_launches_the_keeper_on_the_rows_own_form_and_flips_the_row() {
        // AC1-HP
        let (_dir, home) = home();
        let row = agy_row();
        let calls = Harness::new();
        let probe = probe_steps(vec![
            crate::daemon::KeeperProbe::NoListener,
            crate::daemon::KeeperProbe::Answered(identify(4242, 4243)),
            crate::daemon::KeeperProbe::Answered(identify(4242, 4243)),
        ]);
        let code = revive_with(
            &home,
            &row,
            "agy",
            SID,
            "/tmp",
            &base_argv(),
            None,
            Path::new("/state/mux/threads/t-revive.sock"),
            no_admit,
            probe,
            |spec| calls.launch(spec),
            |pid| calls.kill(pid),
        );
        assert_eq!(code, 0);
        let argv = calls.argv();
        assert!(argv[0].ends_with("fno-agents-worker"), "{:?}", argv[0]);
        assert_eq!(
            argv[1..].join(" "),
            format!(
                "--keeper --sock /state/mux/threads/t-revive.sock \
                 --session t-revive --pane-key {SID} --cwd /tmp -- agy --conversation {SID} \
                 --dangerously-skip-permissions"
            )
        );
        let row = read_row(&home);
        assert_eq!(row.status, crate::AgentStatus::Live);
        assert_eq!(row.substrate.as_deref(), Some("thread"));
        assert!(row.mux.is_none());
        assert_eq!(
            row.messaging_socket_path.as_deref(),
            Some("/state/mux/threads/t-revive.sock")
        );
        assert_eq!(row.pid, Some(4242), "pid is the KEEPER on a thread row");
        assert_eq!(row.keeper_child_pid, Some(4243));
        assert_eq!(row.harness_session_id.as_deref(), Some(SID));
        let events = events_of(&home);
        assert_eq!(events.matches("\"agent_resumed\"").count(), 1, "{events}");
        assert!(events.contains("\"substrate\":\"thread\""), "{events}");
    }

    #[test]
    fn revival_argv_carries_the_model_and_the_recorded_mode() {
        // AC2-HP
        let contract = harness();
        let mut cursor_row = agy_row();
        cursor_row.harness = Some("cursor-agent".to_string());
        cursor_row.requested_model = Some("gpt-5".to_string());
        let base = vec![
            "cursor-agent".to_string(),
            "--resume".to_string(),
            SID.to_string(),
            "--trust".to_string(),
        ];
        let (argv, word) = revival_argv(&contract, "cursor-agent", &cursor_row, &base).unwrap();
        assert_eq!(
            argv,
            vec![
                "cursor-agent".to_string(),
                "--resume".to_string(),
                SID.to_string(),
                "--trust".to_string(),
                "--model".to_string(),
                "gpt-5".to_string(),
            ]
        );
        assert_eq!(word, "default");

        let mut agy = agy_row();
        agy.requested_permission_mode = Some("plan".to_string());
        let (argv, word) = revival_argv(&contract, "agy", &agy, &base_argv()).unwrap();
        assert!(
            !argv.iter().any(|t| t == "--dangerously-skip-permissions"),
            "{argv:?}"
        );
        assert!(argv.windows(2).any(|w| w == ["--mode", "plan"]), "{argv:?}");
        assert_eq!(word, "plan");
    }

    #[test]
    fn a_gate_refusal_returns_its_code_and_touches_nothing() {
        // AC3-ERR
        let (_dir, home) = home();
        let row = agy_row();
        let calls = Harness::new();
        let before = registry_bytes(&home);
        let code = revive_with(
            &home,
            &row,
            "agy",
            SID,
            "/tmp",
            &base_argv(),
            None,
            Path::new("/state/mux/threads/t-revive.sock"),
            || Err(83),
            probe_steps(vec![]),
            |spec| calls.launch(spec),
            |pid| calls.kill(pid),
        );
        assert_eq!(code, 83);
        assert!(calls.launched.lock().unwrap().is_empty());
        assert_eq!(
            registry_bytes(&home),
            before,
            "the registry is byte-identical"
        );
    }

    #[test]
    fn an_already_live_keeper_and_a_silent_one_never_launch() {
        // AC4-EDGE
        let (_dir, home) = home();
        let row = agy_row();
        let calls = Harness::new();
        let sock = Path::new("/state/mux/threads/t-revive.sock");
        let code = revive_with(
            &home,
            &row,
            "agy",
            SID,
            "/tmp",
            &base_argv(),
            None,
            sock,
            no_admit,
            probe_steps(vec![crate::daemon::KeeperProbe::Answered(identify(
                900, 901,
            ))]),
            |spec| calls.launch(spec),
            |pid| calls.kill(pid),
        );
        assert_eq!(code, 0);
        assert!(calls.launched.lock().unwrap().is_empty());

        let calls = Harness::new();
        let code = revive_with(
            &home,
            &row,
            "agy",
            SID,
            "/tmp",
            &base_argv(),
            None,
            sock,
            no_admit,
            probe_steps(vec![crate::daemon::KeeperProbe::Silent]),
            |spec| calls.launch(spec),
            |pid| calls.kill(pid),
        );
        assert_eq!(code, 13);
        assert!(calls.launched.lock().unwrap().is_empty());
    }

    #[test]
    fn a_child_that_dies_in_the_proof_window_leaves_the_row_exited() {
        // AC5-ERR
        let (_dir, home) = home();
        let row = agy_row();
        let calls = Harness::new();
        let code = revive_with(
            &home,
            &row,
            "agy",
            SID,
            "/tmp",
            &base_argv(),
            None,
            Path::new("/state/mux/threads/t-revive.sock"),
            no_admit,
            probe_steps(vec![
                crate::daemon::KeeperProbe::NoListener,
                crate::daemon::KeeperProbe::Answered(identify(4242, 4243)),
                crate::daemon::KeeperProbe::NoListener,
            ]),
            |spec| calls.launch(spec),
            |pid| calls.kill(pid),
        );
        assert_eq!(code, 1);
        assert_eq!(read_row(&home).status, crate::AgentStatus::Exited);
        let events = events_of(&home);
        assert!(events.contains("\"agent_resume_failed\""), "{events}");
        assert!(!events.contains("\"agent_resumed\""), "{events}");
    }

    #[test]
    fn a_row_rebound_to_another_session_stops_the_keeper_and_keeps_the_other_writers_row() {
        // AC6-EDGE
        let (_dir, home) = home();
        let mut row = agy_row();
        let calls = Harness::new();
        // The registry row changes identity while the launch runs.
        let mutated = agy_row();
        std::fs::write(
            home.registry_json(),
            serde_json::to_vec(&crate::state::Registry {
                schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
                entries: vec![mutated],
            })
            .unwrap(),
        )
        .unwrap();
        row.harness_session_id = Some("99999999-8888-7777-6666-555555555555".to_string());
        let code = revive_with(
            &home,
            &row,
            "agy",
            "99999999-8888-7777-6666-555555555555",
            "/tmp",
            &base_argv(),
            None,
            Path::new("/state/mux/threads/t-revive.sock"),
            no_admit,
            probe_steps(vec![
                crate::daemon::KeeperProbe::NoListener,
                crate::daemon::KeeperProbe::Answered(identify_for(
                    "99999999-8888-7777-6666-555555555555",
                    4242,
                    4243,
                )),
                crate::daemon::KeeperProbe::Answered(identify_for(
                    "99999999-8888-7777-6666-555555555555",
                    4242,
                    4243,
                )),
            ]),
            |spec| calls.launch(spec),
            |pid| calls.kill(pid),
        );
        assert_eq!(code, 13);
        assert_eq!(calls.killed.lock().unwrap().last(), Some(&4242));
        // Content-equal, not byte-equal: the write path re-serializes whatever
        // it read, so "as the other writer left it" is the ROW, not the bytes.
        let after = read_row(&home);
        assert_eq!(after.status, crate::AgentStatus::Exited);
        assert_eq!(after.harness_session_id.as_deref(), Some(SID));
        assert_eq!(after.name, "t-revive");
    }
}
