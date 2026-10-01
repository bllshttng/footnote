//! The keeper's survival contract, proven against real processes.
//!
//! Unit tests prove frames and the ring. These tests prove the claim the
//! module exists for: a keeper outlives its launcher, answers Identify with
//! the CHILD's pid, and a keeper whose child exits unlinks its socket and
//! exits. Every assertion names a pid; a survivor count proves nothing.

use fno::pty::KEEPER_PROTOCOL_VERSION;
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
            Ok(_) => buf.extend_from_slice(&chunk),
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
                    _,
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
    assert_eq!(
        reply["v"],
        u64::from(KEEPER_PROTOCOL_VERSION),
        "protocol version rides the reply: {reply}"
    );
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
        assert!(
            reply["session_id"].is_null(),
            "no id in the argv means null in the reply: {reply}"
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
    assert_eq!(
        reply["v"],
        u64::from(KEEPER_PROTOCOL_VERSION),
        "protocol version rides the reply: {reply}"
    );
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

#[test]
fn identify_reports_drift_and_the_pane_survives_a_rewritten_binary() {
    // AC3-EDGE: a pane keeper whose binary was rewritten answers Identify
    // with drift "drifted"; the keeper and its child stay alive - a pane
    // keeper gets no self-retire, because exiting ends the pane.
    let dir = std::env::temp_dir().join(format!("fno-pane-drift-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("worker-copy");
    std::fs::copy(keeper_bin(), &copy).unwrap();
    let sock = dir.join("drift.sock");
    let script = format!(
        "{} --pane --sock {} --session t2 --pane-key 22 --cwd /tmp -- sleep 300 >/dev/null 2>&1 & echo $!",
        copy.display(),
        sock.display()
    );
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stdout(Stdio::piped())
        .output()
        .expect("launcher runs");
    let pid: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("launcher echoed the keeper pid");
    let fresh = identify(&sock);
    if fresh.get("drift").is_some() {
        assert_eq!(fresh["drift"], "fresh", "unrewritten copy reads fresh");
    }
    std::fs::remove_file(&copy).unwrap();
    std::fs::write(&copy, b"newer build bytes").unwrap();
    // The live re-stat at Identify time reads the rewrite.
    let drifted = identify(&sock);
    assert_eq!(
        drifted["drift"], "drifted",
        "rewritten build reads drifted: {drifted}"
    );
    assert!(
        drifted["build"]["size"].is_u64(),
        "the build fingerprint rides the reply: {drifted}"
    );
    // The keeper and its child stay alive (no self-retire for a pane).
    assert!(alive(pid), "the keeper survives");
    let child = drifted["child_pid"].as_u64().expect("child pid");
    assert!(alive(child as u32), "the pane's child survives");
    // Cleanup.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
        libc::kill(child as libc::pid_t, libc::SIGKILL);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The self-exit contract: a keeper spawned with SIGTERM blocked (the mask
/// the mux server gives a real keeper) dies to SIGTERM, and a keeper whose
/// socket DIRECTORY is deleted ends itself and its child once two polls
/// pass with no subscriber seated. A keeper whose dir still exists stays
/// up and answers Identify - the re-adoption contract. Exit is read with
/// `try_wait`, never `kill(pid, 0)`, which reads a zombie as alive.
#[test]
fn keeper_ends_itself_on_sigterm_and_when_its_socket_dir_is_deleted() {
    use std::os::unix::process::CommandExt;

    fn spawn_blocked_keeper(sock: &PathBuf, pane_key: u32) -> std::process::Child {
        let mut cmd = Command::new(keeper_bin());
        cmd.arg("--pane")
            .arg("--sock")
            .arg(sock)
            .arg("--session")
            .arg("t")
            .arg("--pane-key")
            .arg(pane_key.to_string())
            .arg("--cwd")
            .arg("/tmp")
            .arg("--")
            .arg("sleep")
            .arg("300")
            .env("FNO_KEEPER_ORPHAN_POLL_MS", "200")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // The same mask the mux server gives a real keeper: SIGTERM blocked
        // in the child before exec.
        unsafe {
            cmd.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
                if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        cmd.spawn().expect("keeper spawns")
    }

    fn wait_exit(child: &mut std::process::Child, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_child_gone(pid: u32, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while alive(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        true
    }

    let scratch = std::env::temp_dir().join(format!("fno-keeper-orphan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let dir_a = scratch.join("a/panes");
    let dir_b = scratch.join("b/panes");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    let sock_a = dir_a.join("k.sock");
    let sock_b = dir_b.join("k.sock");

    let mut a = spawn_blocked_keeper(&sock_a, 31);
    let mut b = spawn_blocked_keeper(&sock_b, 32);
    let child_a = identify(&sock_a)["child_pid"].as_u64().unwrap() as u32;
    let child_b = identify(&sock_b)["child_pid"].as_u64().unwrap() as u32;
    let _child_a = KillGuard(child_a);
    let _child_b = KillGuard(child_b);
    let _keeper_a = KillGuard(a.id());
    let _keeper_b = KillGuard(b.id());

    // Orphan leg: A's whole tree gone. The Identify probe's seat clears
    // when its connection drops; one short sleep lets that land before the
    // first poll. Two 200 ms polls later the keeper ends its child, exits,
    // and the main wait unlinks nothing (the dir is already gone).
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_dir_all(scratch.join("a")).unwrap();
    assert!(
        wait_exit(&mut a, 10),
        "keeper A must end itself once its socket dir is gone"
    );
    assert!(
        wait_child_gone(child_a, 10),
        "keeper A must end its child when its socket dir is gone"
    );

    // Survival control: B's dir still exists and no server attached - it
    // stays up and answers Identify. One sleep covers five polls, so a
    // keeper miscounting dir-gone would already be out.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        b.try_wait().expect("try_wait").is_none(),
        "keeper B must stay alive while its socket dir exists"
    );
    identify(&sock_b);

    // SIGTERM leg: the blocked signal is delivered to the sigwait thread;
    // the child dies, the keeper exits, and its socket file is unlinked.
    // SAFETY: SIGTERM to the test's own keeper.
    unsafe {
        libc::kill(b.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(wait_exit(&mut b, 10), "keeper B must die to SIGTERM");
    assert!(
        wait_child_gone(child_b, 10),
        "keeper B must end its child on SIGTERM"
    );
    assert!(
        !sock_b.exists(),
        "keeper B unlinks its socket on the SIGTERM exit"
    );

    std::fs::remove_dir_all(&scratch).ok();
}
