//! A fake shared codex app-server daemon, for tests.
//!
//! The thread driver is a WebSocket client of `$CODEX_HOME/app-server-control/
//! app-server-control.sock`, so a fake daemon is a unix listener at that path
//! that speaks the upgrade. Scenario scripts stay the newline-delimited-JSON
//! programs they already were: this bridges one child process per connection,
//! pumping text frames to its stdin and its stdout lines back out as frames.
//!
//! It lives in the library rather than in each test module because there are
//! two doors onto it (the crate's unit tests and its integration tests) and a
//! second copy is a second fake daemon to keep in step with the real protocol.
#![doc(hidden)]

use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::Message;

/// Serializes every `CODEX_HOME` mutation in a test binary. Two fake-daemon
/// tests running concurrently would restore the var under each other's feet
/// and point the driver at the operator's REAL codex daemon mid-test. That is
/// not a flake, it is a live model call from a unit test, so the lock is
/// load-bearing rather than tidy.
pub static CODEX_HOME_LOCK: Mutex<()> = Mutex::new(());

/// Run `body` with `CODEX_HOME` pointed at a temporary root whose control
/// socket is served by `script`.
pub async fn with_fake_codex_daemon(script: &str, body: impl std::future::Future<Output = ()>) {
    let _guard = CODEX_HOME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Short and directly under /tmp on purpose: a unix socket path is capped
    // at 104 bytes on macOS, and $TMPDIR there is a long per-user sandbox
    // path that overruns the cap once the control subdirectory is appended.
    // `tempfile` is a dev-dependency and this module ships in the library, so
    // the unique name is minted here rather than borrowed.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let home = PathBuf::from(format!(
        "/tmp/fno-fake-codex-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let control = home.join("app-server-control");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&control).expect("control dir");
    let socket = control.join("app-server-control.sock");
    let script_path = home.join("fake-app-server.py");
    std::fs::write(&script_path, script).expect("write script");

    let listener = UnixListener::bind(&socket).expect("bind control socket");
    let serving = script_path.clone();
    let server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let serving = serving.clone();
            tokio::spawn(async move { bridge_connection(stream, serving).await });
        }
    });

    let saved = std::env::var_os("CODEX_HOME");
    std::env::set_var("CODEX_HOME", &home);
    body.await;
    match saved {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    server.abort();
    let _ = std::fs::remove_dir_all(&home);
}

/// One connection: upgrade, spawn the scenario script, pump both ways.
/// One Text frame in -> one stdin line; one stdout line -> one Text frame.
async fn bridge_connection(stream: UnixStream, script: PathBuf) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut frames) = ws.split();
    let mut child = match tokio::process::Command::new("python3")
        .arg(&script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let mut lines = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            frame = frames.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if stdin.write_all(format!("{text}\n").as_bytes()).await.is_err()
                        || stdin.flush().await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if sink.send(Message::Text(line.into())).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            },
        }
    }
}
