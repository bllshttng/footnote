//! A human's own fno always gets in. Gates, brakes and timeouts may warn or
//! slow agents, never refuse the user's start or attach. Each test drives the
//! real `fno` client in a pty and fails on the pre-fix build.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{spawn_server, ClientHarness, Scratch};

const HUMAN_ADMIT: &str = "admitting a human's own start anyway";

/// Over the fleet ceiling, a human's `fno` still starts its server and
/// attaches. A second live `fno` process makes the census count 1, which
/// meets a ceiling of 1, so the pre-fix client refused its own server spawn.
#[test]
fn human_start_passes_the_fleet_process_ceiling() {
    let scratch = Scratch::new("human-start-ceiling");
    let _fleet = spawn_server(&scratch.0.join("fleet.sock"), &[]);
    common::connect_with_retry(&scratch.0.join("fleet.sock"));
    let mut h = ClientHarness::spawn_with(&scratch, &[("FNO_PROCESS_ADMISSION_MAX", "1")]);
    h.wait_prompt(30);
    let raw = h.raw_output();
    assert!(
        raw.contains(HUMAN_ADMIT),
        "the ceiling must have been met and waived:\n{raw}"
    );
}

/// A server already over its ceiling still gives a human attach its shell.
/// The attaching client is itself a live `fno`, so the server's census meets
/// the ceiling of 1; the pre-fix server answered `cannot start a shell`.
#[test]
fn human_attach_passes_the_fleet_process_ceiling() {
    let scratch = Scratch::new("human-attach-ceiling");
    let _server = spawn_server(&scratch.main_sock(), &[("FNO_PROCESS_ADMISSION_MAX", "1")]);
    common::connect_with_retry(&scratch.main_sock());
    let mut h = ClientHarness::spawn_session(&scratch, "main");
    h.wait_prompt(30);
    let log = std::fs::read_to_string(scratch.0.join("server.log")).unwrap_or_default();
    assert!(
        log.contains(HUMAN_ADMIT),
        "the ceiling must have been met and waived:\n{log}"
    );
}

/// A server that answers the attach only after 12 seconds (a loaded machine)
/// still gets the human in. The pre-fix client quit at 10 seconds with
/// `server did not answer the attach`.
#[test]
fn human_attach_waits_out_a_stalled_server() {
    let scratch = Scratch::new("human-attach-stall");
    let upstream = scratch.0.join("up.sock");
    let _server = spawn_server(&upstream, &[]);
    common::connect_with_retry(&upstream);
    let listener = UnixListener::bind(scratch.main_sock()).unwrap();
    // Stall only the first Attach connection. A probe (Query) passes through,
    // so the stall always lands on the handshake the test is about.
    let stalled = AtomicBool::new(false);
    std::thread::spawn(move || {
        for mut client in listener.incoming().flatten() {
            let mut len = [0u8; 4];
            if client.read_exact(&mut len).is_err() {
                continue;
            }
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            if client.read_exact(&mut body).is_err() {
                continue;
            }
            let attach = String::from_utf8_lossy(&body).starts_with("{\"Attach\"");
            let delay = if attach && !stalled.swap(true, Ordering::SeqCst) {
                Duration::from_secs(12)
            } else {
                Duration::ZERO
            };
            let mut server = UnixStream::connect(&upstream).unwrap();
            server.write_all(&len).unwrap();
            server.write_all(&body).unwrap();
            pipe(
                client.try_clone().unwrap(),
                server.try_clone().unwrap(),
                Duration::ZERO,
            );
            pipe(server, client, delay);
        }
    });
    let mut h = ClientHarness::spawn_session(&scratch, "main");
    h.wait_prompt(40);
    let raw = h.raw_output();
    assert!(
        raw.contains("server is busy, still waiting"),
        "the wait must be named:\n{raw}"
    );
}

/// Copy `from` into `to` after `delay`, closing `to`'s write side at EOF.
fn pipe(mut from: UnixStream, mut to: UnixStream, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let mut buf = [0u8; 8192];
        while let Ok(n) = from.read(&mut buf) {
            if n == 0 || to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Write);
    });
}
