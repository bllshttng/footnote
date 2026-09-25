//! The revival, proven through the built binary.
//!
//! Unit tests prove the ordering behind seams. These tests prove the real
//! path: the resume door routes an exited agy thread row into a fresh
//! keeper, the same registry row comes back live under the same session id,
//! a pi row keeps its refusal, and the message delivery rides the keeper
//! mail lane.

use fno_agents::harness_capabilities::HarnessContract;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SID: &str = "11111111-2222-3333-4444-555555555555";
const PI_SID: &str = "22222222-3333-4444-5555-666666666666";

struct Fixture {
    root: TempDirGuard,
    home: PathBuf,
    bins: PathBuf,
    name: String,
}

/// Tempdir whose guard lives in the struct, so it drops last.
struct TempDirGuard(tempfile::TempDir);

impl Fixture {
    /// One exited agy thread row, a fake `agy` on PATH that logs its argv and
    /// runs `body`, and the worker binary env a revival needs.
    fn new(body: &str) -> Self {
        let root = TempDirGuard(tempfile::tempdir().unwrap());
        let home = root.0.path().join("agents");
        let bins = root.0.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bins).unwrap();
        let name = format!("t-revive-{}", std::process::id());
        let cwd = root.0.path().join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let row = json!({
            "name": name,
            "harness": "agy",
            "substrate": "thread",
            "status": "exited",
            "exited_at": "2026-09-23T00:00:00Z",
            "cwd": cwd,
            "log_path": cwd.join(format!("{name}.log")),
            "created_at": "2026-09-22T00:00:00Z",
            "harness_session_id": SID,
        });
        write_registry(&home, &[row]);
        write_executable(
            &bins.join("agy"),
            &format!("#!/bin/sh\necho \"launch: $@\" >> \"$FAKE_AGY_LOG\"\n{body}"),
        );
        Self {
            root,
            home,
            bins,
            name,
        }
    }

    fn sock(&self) -> PathBuf {
        self.root
            .0
            .path()
            .join("mux")
            .join("threads")
            .join(format!("{}.sock", self.name))
    }

    fn log(&self) -> PathBuf {
        self.root.0.path().join("agy.log")
    }

    fn registry_row(&self) -> Value {
        let raw = fs::read_to_string(self.home.join("registry.json")).unwrap();
        let registry: Value = serde_json::from_str(&raw).unwrap();
        registry["entries"]
            .as_array()
            .or_else(|| registry["agents"].as_array())
            .expect("registry rows")
            .iter()
            .find(|row| row["name"] == self.name.as_str())
            .cloned()
            .expect("the row survives")
    }

    /// Run `fno-agents resume <name>` the way an operator's caller does:
    /// null stdin, no terminal, the fixture home and PATH.
    fn resume(&self, extra: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_fno-agents"))
            .args(["resume", &self.name])
            .args(extra)
            .env_clear()
            .envs(fno_agents::test_run::self_owner_env())
            .env("FNO_AGENTS_HOME", &self.home)
            .env("HOME", self.root.0.path())
            // The fake's own helpers (dd, stty) live on the std paths; the
            // fixture's bins come first, so `agy` still resolves to the fake.
            .env("PATH", format!("{}:/usr/bin:/bin", self.bins.display()))
            .env(
                "FNO_AGENTS_WORKER_BIN",
                env!("CARGO_BIN_EXE_fno-agents-worker"),
            )
            .env("FNO_SPAWN_GATE", "0")
            .env("FAKE_AGY_LOG", self.log())
            .stdin(Stdio::null())
            .output()
            .expect("fno-agents starts")
    }

    /// Kill whatever the revival left live: the flipped row names the keeper
    /// and child pids. Best-effort - a failed revival leaves nothing.
    fn kill_revived(&self) {
        let row = self.registry_row();
        for key in ["pid", "keeper_child_pid"] {
            if let Some(pid) = row[key].as_u64() {
                // SAFETY: SIGKILL to the pid the row itself named.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
}

struct KillGuard(u32);

impl Drop for KillGuard {
    fn drop(&mut self) {
        // SAFETY: SIGKILL to the keeper pid a test started.
        unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
    }
}

fn write_registry(home: &Path, rows: &[Value]) {
    fs::write(
        home.join("registry.json"),
        serde_json::to_vec(&json!({
            "schema_version": fno_agents::state::REGISTRY_SCHEMA_VERSION,
            "agents": rows,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

/// A raw Identify against the revived keeper's socket, read back bounded.
fn identify(sock: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        if let Ok(s) = UnixStream::connect(sock) {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "no keeper ever bound {}",
            sock.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(&fno_agents::pane_keeper::encode(
            &fno_agents::pane_keeper::Frame::Identify,
        ))
        .unwrap();
    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk),
        }
        if let fno_agents::pane_keeper::Decode::Frame(
            fno_agents::pane_keeper::Frame::IdentifyReply(payload),
            _,
        ) = fno_agents::pane_keeper::decode(&buf)
        {
            return serde_json::from_slice(&payload).unwrap();
        }
    }
    panic!("no IdentifyReply from {}", sock.display());
}

/// The revival path, end to end: exit 0, the live line, the row live on the
/// thread socket, a raw Identify answering the same session id, and exactly
/// one launch of the resume form with the lane-default posture.
#[test]
fn resume_revives_an_exited_agy_thread_row_on_a_fresh_keeper() {
    // AC7-HP
    let fixture = Fixture::new("printf '? for shortcuts\\n'\nexec /bin/sleep 300\n");
    let output = fixture.resume(&[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("is live on its keeper"), "stdout: {stdout}");
    let sock = fixture.sock();
    let row = fixture.registry_row();
    assert_eq!(row["status"], "live", "{row}");
    assert_eq!(
        row["messaging_socket_path"],
        sock.to_string_lossy().as_ref(),
        "{row}"
    );
    assert_eq!(row["harness_session_id"], SID, "{row}");
    let reply = identify(&sock);
    assert_eq!(reply["session_id"], SID, "{reply}");
    let keeper_pid = reply["keeper_pid"].as_u64().unwrap() as u32;
    let child_pid = reply["child_pid"].as_u64().unwrap() as u32;
    let _keeper = KillGuard(keeper_pid);
    let _child = KillGuard(child_pid);
    let log = fs::read_to_string(fixture.log()).unwrap();
    assert_eq!(log.matches("launch:").count(), 1, "{log}");
    assert!(
        log.contains(&format!(
            "--conversation {SID} --dangerously-skip-permissions"
        )),
        "{log}"
    );
    // Cleanup is best-effort from here; the guards above do the killing.
    fixture.kill_revived();
}

/// A harness child that dies at once is caught by the proof window: exit 1,
/// the row untouched, the socket gone, and keeper.log named for the autopsy.
#[test]
fn a_child_that_exits_at_once_leaves_the_row_exited_and_names_keeper_log() {
    // AC8-ERR
    let fixture = Fixture::new("exit 1\n");
    let output = fixture.resume(&[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("keeper.log"), "{stderr}");
    let row = fixture.registry_row();
    assert_eq!(row["status"], "exited", "{row}");
    assert!(!fixture.sock().exists(), "the keeper unlinked its socket");
}

/// The contract decides: pi's keeper row carries no resume_session_id, so
/// the door refuses by name and launches nothing, even with the binary on
/// PATH.
#[test]
fn a_pi_thread_row_keeps_a_refusal_that_names_its_keeper_row() {
    // AC9-ERR
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("agents");
    let bins = root.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bins).unwrap();
    let name = format!("t-revive-pi-{}", std::process::id());
    let cwd = root.path().join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    let row = json!({
        "name": name,
        "harness": "pi",
        "substrate": "thread",
        "status": "exited",
        "exited_at": "2026-09-23T00:00:00Z",
        "cwd": cwd,
        "log_path": cwd.join(format!("{name}.log")),
        "created_at": "2026-09-22T00:00:00Z",
        "harness_session_id": PI_SID,
    });
    write_registry(&home, &[row]);
    let pi_log = root.path().join("pi.log");
    write_executable(
        &bins.join("pi"),
        &format!(
            "#!/bin/sh\necho \"launch: $@\" >> \"{}\"\nexec /bin/sleep 300\n",
            pi_log.display()
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["resume", &name])
        .env_clear()
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_AGENTS_HOME", &home)
        .env("HOME", root.path())
        .env("PATH", &bins)
        .env(
            "FNO_AGENTS_WORKER_BIN",
            env!("CARGO_BIN_EXE_fno-agents-worker"),
        )
        .env("FNO_SPAWN_GATE", "0")
        .stdin(Stdio::null())
        .output()
        .expect("fno-agents starts");
    assert_eq!(output.status.code(), Some(13), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[harness.pi.keeper] does not carry resume_session_id"),
        "{stderr}"
    );
    assert!(
        !pi_log.exists(),
        "the fake pi was execed in-terminal: nothing may launch on a refused row"
    );
}

/// A second resume of a live keeper row is the already-live line, and it
/// never launches a second keeper.
#[test]
fn a_second_resume_of_a_revived_row_launches_nothing() {
    // AC10-EDGE
    let fixture = Fixture::new("printf '? for shortcuts\\n'\nexec /bin/sleep 300\n");
    let first = fixture.resume(&[]);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let reply = identify(&fixture.sock());
    let keeper_pid = reply["keeper_pid"].as_u64().unwrap() as u32;
    let child_pid = reply["child_pid"].as_u64().unwrap() as u32;
    let _keeper = KillGuard(keeper_pid);
    let _child = KillGuard(child_pid);
    let log_before = fs::read_to_string(fixture.log()).unwrap();
    assert_eq!(log_before.matches("launch:").count(), 1);

    let second = fixture.resume(&[]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("already live"), "stdout: {stdout}");
    let log_after = fs::read_to_string(fixture.log()).unwrap();
    assert_eq!(
        log_after.matches("launch:").count(),
        1,
        "the fake log must still name one launch"
    );
}

/// The revival carries the nudge ladder's text: the live line first, then
/// the message typed into the revived TUI through the keeper mail lane.
#[test]
fn a_revived_row_delivers_a_message_through_the_keeper_socket() {
    // AC11-HP
    let fixture = Fixture::new(
        // The keeper's pty is raw, so the typed envelope arrives as bytes
        // ending in CR, never a NL: log byte-wise, not line-wise.
        "stty raw -echo 2>/dev/null\nwhile :; do\n  c=$(dd bs=1 count=1 2>/dev/null)\n  [ -n \"$c\" ] || break\n  printf '%s' \"$c\" >> \"$FAKE_AGY_LOG\"\ndone\n",
    );
    let output = fixture.resume(&["--message", "ping"]);
    // AC11 pins the live line and the delivered text. The delivery verdict is
    // the mail lane's own: agy keeps no greppable receipt, so the lane answers
    // not-confirmed after typing, and the revival reports that as its failure
    // per the plan (exit 1, row stays live).
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("is live on its keeper"), "stdout: {stdout}");
    assert_eq!(
        fixture.registry_row()["status"],
        "live",
        "{:?}",
        fixture.registry_row()
    );
    let reply = identify(&fixture.sock());
    let keeper_pid = reply["keeper_pid"].as_u64().unwrap() as u32;
    let child_pid = reply["child_pid"].as_u64().unwrap() as u32;
    let _keeper = KillGuard(keeper_pid);
    let _child = KillGuard(child_pid);
    // The paste settles on the hosted TUI's own enter delay; the typed line
    // lands in the fake's stdin log within the delivery budget.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let log = fs::read_to_string(fixture.log()).unwrap_or_default();
        if log.contains("ping") {
            break;
        }
        assert!(Instant::now() < deadline, "ping never landed: {log}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The contract row is the eligibility rule; this pins the two harnesses
/// the packaged contract carries today, so a data flip without its journey
/// cannot slip past this file silently.
#[test]
fn the_packaged_contract_names_the_revivable_keeper_lanes() {
    let contract = HarnessContract::packaged().unwrap();
    let carries = |harness: &str| {
        contract
            .capabilities(harness)
            .ok()
            .and_then(|caps| caps.keeper.as_ref())
            .map(|keeper| {
                keeper
                    .carries
                    .iter()
                    .any(|axis| axis == "resume_session_id")
            })
            .unwrap_or(false)
    };
    assert!(carries("agy"));
    assert!(carries("cursor-agent"));
    assert!(!carries("pi"));
    assert!(!carries("grok"));
}
