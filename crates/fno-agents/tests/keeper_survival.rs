//! The keeper's survival contract, proven against real processes.
//!
//! Unit tests prove frames and the ring. These tests prove the claim the
//! module exists for: a keeper outlives its launcher, answers Identify with
//! the CHILD's pid, and a keeper whose child exits unlinks its socket and
//! exits. Every assertion names a pid; a survivor count proves nothing.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn keeper_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fno-agents-worker")
}

fn scratch_sock(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fno-keeper-test-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("keeper.sock")
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 is the existence probe; no signal is delivered.
    let hit = unsafe { libc::kill(pid as libc::pid_t, 0) };
    hit == 0
}

fn ppid_of(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .ok()
        .or_else(|| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
        })
}

/// Spawn the keeper through a launcher shell that backgrounds it and exits,
/// so the keeper's parent is a process that WILL die. Returns the keeper's
/// pid (the launcher's `$!`), read from the launcher's stdout - a named pid,
/// not a scan.
fn spawn_via_launcher(cfg_args: &str) -> u32 {
    spawn_via_launcher_lane("--pane", cfg_args)
}

/// The same launcher, naming the lane token: `--pane` (the mux server's
/// spelling) or `--keeper` (the lane-B thread spawn's).
fn spawn_via_launcher_lane(lane: &str, cfg_args: &str) -> u32 {
    let script = format!(
        "{} {} {} >/dev/null 2>&1 & echo $!",
        keeper_bin(),
        lane,
        cfg_args
    );
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stdout(Stdio::piped())
        .output()
        .expect("launcher runs");
    assert!(out.status.success(), "launcher failed: {script}");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("launcher echoed the keeper pid")
}

/// Connect to the keeper and read its Identify reply (bounded).
fn identify(sock: &PathBuf) -> serde_json::Value {
    // The connect retry carries the same deadline as the read: a keeper
    // that died at startup never binds, and an unbounded connect loop on
    // its absent socket spins the test forever instead of failing.
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        if let Ok(s) = UnixStream::connect(sock) {
            break s;
        }
        assert!(
            Instant::now() < connect_deadline,
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
        // Skip to the IdentifyReply frame.
        let mut consumed = 0usize;
        while let decoded @ (fno_agents::pane_keeper::Decode::Frame(..)
        | fno_agents::pane_keeper::Decode::Violation(_)) =
            fno_agents::pane_keeper::decode(&buf[consumed..])
        {
            match decoded {
                fno_agents::pane_keeper::Decode::Frame(
                    fno_agents::pane_keeper::Frame::IdentifyReply(payload),
                    used,
                ) => {
                    return serde_json::from_slice(&payload).expect("identify reply is json");
                }
                // Ring replay may carry Output frames before/around the
                // reply; keep scanning.
                fno_agents::pane_keeper::Decode::Frame(_, used) => consumed += used,
                fno_agents::pane_keeper::Decode::Violation(reason) => {
                    panic!("protocol violation reading identify reply: {reason}")
                }
                fno_agents::pane_keeper::Decode::NeedMore => break,
            }
        }
    }
    panic!(
        "no identify reply within the deadline; got {} bytes",
        buf.len()
    );
}

struct KillGuard(u32);

impl Drop for KillGuard {
    fn drop(&mut self) {
        // SAFETY: SIGKILL to a test child; leaked keepers would haunt the
        // next run's socket paths.
        unsafe {
            libc::kill(self.0 as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[test]
fn pane_keeper_outlives_parent() {
    let sock = scratch_sock("outlives");
    let _ = std::fs::remove_file(&sock);
    let keeper_pid = spawn_via_launcher(&format!(
        "--sock {} --session t --pane-key 7 --cwd /tmp -- sleep 300",
        sock.display()
    ));
    let _keeper = KillGuard(keeper_pid);

    // The launcher is long gone: the keeper's parent must already be init
    // (or the reaper it delegates to). Poll: reparenting is asynchronous.
    let launcher_gone = Instant::now();
    while alive(keeper_pid) && ppid_of(keeper_pid).is_none_or(|p| p != 1) {
        assert!(
            launcher_gone.elapsed() < Duration::from_secs(10),
            "keeper pid {keeper_pid} never reparented to init (ppid {:?})",
            ppid_of(keeper_pid)
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // The named keeper is alive, and its CHILD is alive - read through the
    // protocol, never inferred from a process count.
    assert!(
        alive(keeper_pid),
        "keeper pid {keeper_pid} must survive its launcher"
    );
    let reply = identify(&sock);
    assert_eq!(reply["v"], 1, "protocol version rides the reply: {reply}");
    let child_pid = reply["child_pid"].as_u64().expect("child_pid in reply") as u32;
    assert_ne!(child_pid, 0, "the child pid is a real pid");
    assert_ne!(
        child_pid, keeper_pid,
        "the reply names the CHILD, not the keeper"
    );
    assert!(
        alive(child_pid),
        "child pid {child_pid} must be alive after the launcher died"
    );
    assert_eq!(
        ppid_of(child_pid),
        Some(keeper_pid),
        "the surviving child is still parented by the surviving keeper"
    );
    assert_eq!(
        reply["keeper_pid"], keeper_pid,
        "the reply names its own pid"
    );
    let _child = KillGuard(child_pid);
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn keeper_child_exit_unlinks_socket_and_exits() {
    let sock = scratch_sock("exit");
    let _ = std::fs::remove_file(&sock);
    let keeper_pid = spawn_via_launcher(&format!(
        "--sock {} --session t --pane-key 8 --cwd /tmp -- true",
        sock.display()
    ));

    // AC5-ERR: the child (`true`) exits at once; the keeper sends the exit
    // frame, unlinks its socket, and exits. Poll for BOTH observables.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let keeper_gone = !alive(keeper_pid);
        let socket_gone = !sock.exists();
        if keeper_gone && socket_gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "keeper (pid {keeper_pid}, alive={}) / socket (exists={}) after child exit",
            !keeper_gone,
            !socket_gone
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn identify_names_cwd_and_argv() {
    let sock = scratch_sock("identify");
    let _ = std::fs::remove_file(&sock);
    let keeper_pid = spawn_via_launcher(&format!(
        "--sock {} --session ident --pane-key 9 --cwd /tmp -- sleep 60",
        sock.display()
    ));
    let _keeper = KillGuard(keeper_pid);
    let _child = KillGuard({
        let reply = identify(&sock);
        assert_eq!(reply["cwd"], "/tmp", "cwd rides the reply: {reply}");
        assert_eq!(
            reply["argv"],
            serde_json::json!(["sleep", "60"]),
            "the provider argv rides the reply: {reply}"
        );
        reply["child_pid"].as_u64().unwrap() as u32
    });
}

/// A scratch-dir stub provider: a shebang script that ignores its args and
/// sleeps - the quiet long-lived child that still carries a
/// `--session-id <id>` the way a lane-B create form renders one (the same
/// shape the Python journey test's stub `pi` has).
fn write_stub_provider(dir: &PathBuf) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let stub = dir.join("stub-pi");
    std::fs::write(&stub, "#!/bin/sh\nexec /bin/sleep 300\n").unwrap();
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
    stub
}

/// The lane-B spelling (x-889a): `--keeper` runs the SAME lane a pane
/// spawn's `--pane` does - same keeper, same protocol - just with no pane
/// behind it. Proven against a real process, not the parse alone.
#[test]
fn keeper_lane_flag_runs_and_answers_the_session_id() {
    let sock = scratch_sock("keeper-flag");
    let _ = std::fs::remove_file(&sock);
    let cwd = scratch_sock("keeper-flag-cwd");
    let _ = std::fs::remove_dir_all(&cwd);
    let stub = write_stub_provider(&cwd);
    let keeper_pid = spawn_via_launcher_lane(
        "--keeper",
        &format!(
            "--sock {} --session lane-b --pane-key 11 --cwd {} -- \
             {} --session-id sid-lane-b-1",
            sock.display(),
            cwd.display(),
            stub.display()
        ),
    );
    let _keeper = KillGuard(keeper_pid);
    let reply = identify(&sock);
    assert_eq!(reply["v"], 1, "protocol version rides the reply: {reply}");
    assert_eq!(
        reply["session_id"], "sid-lane-b-1",
        "the id fno minted before launch rides the reply: {reply}"
    );
    let child_pid = reply["child_pid"].as_u64().expect("child_pid in reply") as u32;
    let _child = KillGuard(child_pid);
    assert!(
        alive(child_pid),
        "the pane-less keeper still hosts a live child"
    );
    assert_eq!(
        ppid_of(child_pid),
        Some(keeper_pid),
        "the child is parented by the keeper, not by any server"
    );
    let _ = std::fs::remove_file(&sock);
}

/// AC2-HP's negative half: an argv that carries no session id answers null,
/// never a guess. A pane's argv (`sleep 60`) is the everyday case.
#[test]
fn keeper_lane_identify_answers_null_when_the_argv_carries_no_id() {
    let sock = scratch_sock("identify-nosid");
    let _ = std::fs::remove_file(&sock);
    let keeper_pid = spawn_via_launcher(&format!(
        "--sock {} --session ident --pane-key 12 --cwd /tmp -- sleep 60",
        sock.display()
    ));
    let _keeper = KillGuard(keeper_pid);
    let reply = identify(&sock);
    assert!(
        reply["session_id"].is_null(),
        "no id in the argv means null in the reply: {reply}"
    );
    let _child = KillGuard(reply["child_pid"].as_u64().unwrap() as u32);
}
