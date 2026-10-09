//! A pane spawn's keeper handshake runs off the core loop: while a `pane run`
//! waits on a keeper that never answers Identify (bounded at 3 s), keys typed
//! into a live pane keep echoing. Before the move, the whole server froze for
//! the handshake.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{connect_with_retry, spawn_server, worker_bin, FakeClient, Scratch};
use fno::proto::{
    read_msg_sync, write_msg_sync, ClientMsg, ControlVerb, PanePlacement, ServerMsg, BUILD_VERSION,
    PROTO_VERSION,
};

/// A keeper binary that execs the real worker, until `stall` exists: then it
/// marks `stalled` and sleeps without ever binding its socket, so the
/// server's handshake waits out its whole bound.
fn stalling_keeper(dir: &Path, stall: &Path, stalled: &Path) -> PathBuf {
    let stub = dir.join("keeper-stub.sh");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\nif [ -e '{}' ]; then : > '{}'; exec sleep 30; fi\nexec '{}' \"$@\"\n",
            stall.display(),
            stalled.display(),
            worker_bin().display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

fn run_pane(sock: &Path, cwd: &Path) -> Result<u64, String> {
    let mut stream = connect_with_retry(sock);
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write_msg_sync(
        &mut stream,
        &ClientMsg::Control {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.into(),
            verb: ControlVerb::PaneRun {
                cwd: cwd.to_string_lossy().into_owned(),
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                cols: None,
                rows: None,
                claim: false,
                placement: PanePlacement::default(),
                worker: None,
            },
        },
    )
    .unwrap();
    match read_msg_sync(&mut stream).unwrap() {
        ServerMsg::PaneSpawned { pane_id, .. } => Ok(pane_id),
        ServerMsg::Err { msg, .. } => Err(msg),
        other => panic!("unexpected pane-run reply: {other:?}"),
    }
}

#[test]
fn stalled_keeper_handshake_keeps_keys_echoing() {
    let scratch = Scratch::new("stalled-keeper");
    let stall = scratch.0.join("stall");
    let stalled = scratch.0.join("stalled");
    let stub = stalling_keeper(&scratch.0, &stall, &stalled);
    let sock = scratch.0.join("s.sock");
    let _server = spawn_server(
        &sock,
        &[
            ("SHELL", "/bin/sh"),
            ("FNO_AGENTS_WORKER_BIN", stub.to_str().unwrap()),
        ],
    );
    let cwd = scratch.0.join("w");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut c = FakeClient::attach(&sock, 24, 80, cwd.to_str().unwrap());
    let pane = c
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    c.wait_prompt(pane);

    // Every keeper launched from here on stalls its handshake.
    std::fs::write(&stall, "").unwrap();
    let run = {
        let (sock, cwd) = (sock.clone(), cwd.clone());
        std::thread::spawn(move || run_pane(&sock, &cwd))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stalled.exists() {
        assert!(Instant::now() < deadline, "the keeper stub never started");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut typed = String::new();
    for key in ["k", "q", "z"] {
        typed.push_str(key);
        let want = format!("$ {typed}");
        let t0 = Instant::now();
        c.input(key.as_bytes());
        c.wait_pane_text(5, pane, |t| t.contains(&want));
        let took = t0.elapsed();
        assert!(
            took < Duration::from_millis(300),
            "key {key:?} took {took:?} to echo while a keeper handshake was in flight"
        );
    }
    assert!(
        !run.is_finished(),
        "the echoes must be measured while the spawn is still in its handshake"
    );

    // The stalled keeper still costs the spawn nothing: the handshake times
    // out, the pane falls back inline (unkept), and the run lands.
    run.join()
        .unwrap()
        .expect("pane run lands on the inline fallback");
}
