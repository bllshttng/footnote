//! x-4e30 idle-reaper verification: the server-lifecycle guarantees that stop
//! the orphan leak + idle CPU burn. Headless, `ServerProc`-only where it can
//! be (no attached TUI client), so it runs in CI despite the bg-agent openpty
//! ENXIO pitfall; the one autospawn case drives the `fno mux pane` subprocess
//! surface (same seam as `script_api_e2e`), never a controlling PTY.
//!
//! Coverage: AC1-HP (unattached server spawns no claim-sweep subprocess),
//! AC1-EDGE/AC1-FR (a never-attached server self-exits within grace under the
//! marker), AC2-EDGE (a server without the marker persists). AC3-EDGE
//! (client-less script sessions survive) rides `script_api_e2e` under the
//! marker, not here: a sub-second grace against a silent `sleep` pane would
//! false-fail.

mod common;
use common::{spawn_server, Scratch};

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Poll `child` until it exits or `secs` elapse. Returns the exit status if it
/// self-exited, else `None` (still running).
fn wait_exit(child: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll until `sock` no longer exists (the server unlinked it via SocketGuard
/// on shutdown). Returns true if it vanished within `secs`.
fn wait_socket_gone(sock: &Path, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if !sock.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll until `sock` exists (the server bound its listener). Returns true if it
/// appeared within `secs`.
fn wait_socket_present(sock: &Path, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if sock.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn idle_no_sweep() {
    // AC1-HP: an unattached server does no periodic subprocess work. The
    // work-queue reader is gated on client_count>0, so with zero clients it
    // never fork/exec's `fno-agents claim sweep`. Point FNO_AGENTS_BIN at a
    // stub that records each invocation; assert it is never called while the
    // server sits idle. Long grace so the reaper does not exit mid-observation.
    let scratch = Scratch::new("idle_no_sweep");
    let sock = scratch.0.join("work.sock");
    let counter = scratch.0.join("sweep-calls");
    let stub = scratch.0.join("fake-fno-agents");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\necho x >> {:?}\nexit 0\n", counter),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let _server = spawn_server(
        &sock,
        &[
            ("FNO_AGENTS_BIN", stub.to_str().unwrap()),
            ("FNO_IDLE_EXIT_GRACE_MS", "60000"),
        ],
    );
    assert!(
        wait_socket_present(&sock, 5),
        "server never bound its socket"
    );

    // A few tick intervals: an ungated reader would have called the stub ~3x.
    std::thread::sleep(Duration::from_millis(3000));
    let calls = std::fs::read_to_string(&counter).unwrap_or_default();
    assert!(
        calls.trim().is_empty(),
        "unattached server spawned {} claim-sweep subprocess(es); expected 0",
        calls.lines().count()
    );
}

#[test]
fn reap_within_grace() {
    // AC1-EDGE / AC1-FR: under the FNO_E2E marker a server that never receives
    // a client self-exits once the grace window elapses from startup - no
    // parent, no Drop guard, no SIGTERM needed. This is the SIGKILL/abort
    // orphan cover: nothing here kills the server; it reaps itself.
    let scratch = Scratch::new("reap_within_grace");
    let sock = scratch.0.join("work.sock");
    // spawn_server sets FNO_E2E=1; a short grace so the case is fast.
    let mut server = spawn_server(&sock, &[("FNO_IDLE_EXIT_GRACE_MS", "500")]);
    let status = wait_exit(&mut server.0, 8);
    assert!(
        status.is_some(),
        "marked, never-attached server did not self-exit within grace"
    );
}

#[test]
fn prod_persists() {
    // AC2-EDGE: without the FNO_E2E marker a server MUST persist on zero
    // clients (the production persistent-session invariant). Spawn with the
    // marker explicitly removed - independent of any ambient FNO_E2E in the
    // test env - and assert it is still alive well past a would-be grace.
    let scratch = Scratch::new("prod_persists");
    let sock = scratch.0.join("work.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["--server"])
        .arg(&sock)
        .env("FNO_MUX_DIR", &scratch.0)
        .env_remove("FNO_E2E")
        .env("FNO_IDLE_EXIT_GRACE_MS", "500")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait_socket_present(&sock, 5),
        "server never bound its socket"
    );
    // Well past the 500ms would-be grace: an unmarked server never reaps.
    assert!(
        wait_exit(&mut child, 3).is_none(),
        "unmarked server self-exited; production persistence broken"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn autospawn_reaped() {
    // AC1-FR (autospawn path): a server the CLIENT autospawns is setsid'd
    // (ppid==1) and untracked by any harness Drop guard - only a server-side,
    // client-presence reaper can reap it. `fno mux pane run` reaches the same
    // connect_or_spawn -> spawn_server path, which inherits the env (no
    // env_clear), so the marker propagates. The one-shot disconnects, leaving
    // a client-less server whose lone pane goes silent; under a short grace it
    // reaps and unlinks its socket.
    let scratch = Scratch::new("autospawn_reaped");
    let sock = scratch.0.join("work.sock");
    let dir = scratch.0.to_str().unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_fno"))
        .args([
            "mux",
            "pane",
            "run",
            "--session",
            "work",
            "--cwd",
            dir,
            "--",
            "/bin/sh",
            "-c",
            "sleep 300",
        ])
        .env("FNO_MUX_DIR", &scratch.0)
        .env("FNO_E2E", "1")
        .env("FNO_IDLE_EXIT_GRACE_MS", "800")
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs");
    assert!(
        run.status.success(),
        "pane run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        wait_socket_present(&sock, 5),
        "autospawned server never bound its socket"
    );
    // The one-shot is gone, the pane is silent: the setsid server reaps and
    // SocketGuard unlinks. Generous window to absorb CI load.
    assert!(
        wait_socket_gone(&sock, 15),
        "client-autospawned server did not reap within grace"
    );
}
