//! The store keeper's socket lifecycle: serving, re-adoption, explicit
//! shutdown versus survival, and the double-keeper refusal. The protocol
//! shape tests mirror keeper_survival.rs (the pane keeper's own).

use base64::Engine as _;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WORKER_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-worker");

const TAG_REQUEST: u8 = 1;
const TAG_SHUTDOWN: u8 = 2;
const TAG_IDENTIFY: u8 = 3;
const TAG_RESPONSE: u8 = 4;
const TAG_IDENTIFY_REPLY: u8 = 5;

struct Keeper {
    child: Child,
    sock: PathBuf,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn short_home(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("fno-gsk{}_{}_{}", std::process::id(), tag, n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_keeper(tag: &str, graph: &Path, sock: &Path) -> Keeper {
    let child = Command::new(WORKER_BIN)
        .args([
            "--store-keeper",
            "--sock",
            sock.to_str().unwrap(),
            "--graph",
            graph.to_str().unwrap(),
            "--session",
            tag,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn store keeper");
    Keeper {
        child,
        sock: sock.to_path_buf(),
    }
}

fn wait_for_socket(sock: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if UnixStream::connect(sock).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "store socket never appeared: {}",
            sock.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn write_frame(stream: &mut UnixStream, tag: u8, payload: &[u8]) {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(tag);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

fn read_frame(stream: &mut UnixStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).ok()?;
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).ok()?;
    Some((header[0], payload))
}

fn rpc(stream: &mut UnixStream, id: u64, method: &str, params: Value) -> Value {
    let req = json!({"id": id, "method": method, "params": params});
    write_frame(
        stream,
        TAG_REQUEST,
        serde_json::to_vec(&req).unwrap().as_slice(),
    );
    let (tag, payload) = read_frame(stream).expect("a response frame");
    assert_eq!(tag, TAG_RESPONSE, "responses ride the response tag");
    let v: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(v.get("id"), Some(&json!(id)), "reply correlates");
    v
}

fn ok_result(reply: Value) -> Value {
    assert_eq!(
        reply.get("ok"),
        Some(&json!(true)),
        "rpc must succeed: {reply}"
    );
    reply["result"].clone()
}

#[test]
fn keeper_serves_reads_ops_and_shutdown_over_its_socket() {
    let home = short_home("serve");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let mut keeper = spawn_keeper("serve-test", &graph, &sock);
    wait_for_socket(&sock);

    let mut stream = UnixStream::connect(&sock).unwrap();

    // Identify carries the protocol version.
    write_frame(&mut stream, TAG_IDENTIFY, &[]);
    let (tag, payload) = read_frame(&mut stream).unwrap();
    assert_eq!(tag, TAG_IDENTIFY_REPLY);
    let id: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(id["v"], 1, "protocol version rides Identify");
    assert_eq!(id["graph"], graph.display().to_string());

    // A read of an empty graph returns entries, never a refusal.
    let result = ok_result(rpc(&mut stream, 1, "read", json!({})));
    assert_eq!(result["entries"].as_array().unwrap().len(), 0);

    // An op against an absent node answers found=false, never an error.
    let result = ok_result(rpc(
        &mut stream,
        2,
        "op",
        json!({
            "name": "append_progress_note",
            "params": {"node_id": "ab-missing", "note": {"ts": "t", "text": "x"}}
        }),
    ));
    assert_eq!(result["op"]["found"], false);
    drop(stream);

    // Explicit shutdown: ack, exit, unlink.
    let mut stream = UnixStream::connect(&sock).unwrap();
    write_frame(&mut stream, TAG_SHUTDOWN, &[]);
    let (tag, payload) = read_frame(&mut stream).unwrap();
    assert_eq!(tag, TAG_RESPONSE);
    let reply: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reply["result"], "shutdown");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match keeper.child.try_wait().expect("reap check") {
            Some(_) => break,
            None => {
                assert!(Instant::now() < deadline, "keeper did not exit on shutdown");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    assert!(!sock.exists(), "an explicit shutdown unlinks its socket");
}

#[test]
fn keeper_keeps_serving_after_its_client_hangs_up() {
    let home = short_home("survive");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let _keeper = spawn_keeper("survive-test", &graph, &sock);
    wait_for_socket(&sock);

    // First connection: a read, then an abrupt hangup.
    {
        let mut stream = UnixStream::connect(&sock).unwrap();
        let _ = ok_result(rpc(&mut stream, 1, "read", json!({})));
    }
    // The keeper keeps: the next connection is served.
    let mut stream = UnixStream::connect(&sock).unwrap();
    let result = ok_result(rpc(
        &mut stream,
        2,
        "op",
        json!({
            "name": "append_progress_note",
            "params": {"node_id": "ab-missing", "note": {"ts": "t", "text": "x"}}
        }),
    ));
    assert_eq!(
        result["op"]["found"], false,
        "absent node answers found=false"
    );
    // And the answer names the node, never an empty graph substitute.
    drop(stream);

    // A double keeper is a loud refusal, not a stolen socket.
    let out = Command::new(WORKER_BIN)
        .args([
            "--store-keeper",
            "--sock",
            sock.to_str().unwrap(),
            "--graph",
            graph.to_str().unwrap(),
        ])
        .output()
        .expect("run second keeper");
    assert!(
        !out.status.success(),
        "a second keeper on a live socket must refuse"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(3),
        "a second keeper exits EXIT_SEAT_OWNED=3: {stderr}"
    );
    assert!(
        stderr.contains("owned by a live keeper"),
        "the refusal names the socket and the owner: {stderr}"
    );
}

#[test]
fn a_wedged_writer_answers_lock_timeout_inside_its_deadline() {
    // A foreign flock on graph.json.lock (the Python interop shape) wedges
    // the store: the keeper's op surfaces lock_timeout as an error kind
    // instead of blocking the caller past the deadline.
    let home = short_home("wedge");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let _keeper = spawn_keeper("wedge-test", &graph, &sock);
    wait_for_socket(&sock);

    let lock_path = PathBuf::from(format!("{}.lock", graph.canonicalize().unwrap().display()));
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&lock_path)
        .unwrap();
    holder.try_lock().expect("foreign wedge lock");

    let mut stream = UnixStream::connect(&sock).unwrap();
    let started = Instant::now();
    let reply = rpc(
        &mut stream,
        1,
        "op",
        json!({
            "name": "append_progress_note",
            "params": {"node_id": "ab-x", "note": {"ts": "t", "text": "y"}}
        }),
    );
    let elapsed = started.elapsed();
    assert_eq!(
        reply["error"]["kind"], "lock_timeout",
        "a wedged writer answers lock_timeout: {reply}"
    );
    assert!(
        elapsed < Duration::from_secs(12),
        "the deadline bounds the wait, got {elapsed:?}"
    );
}

#[test]
fn read_file_returns_the_bytes_load_graph_validates() {
    let home = short_home("bytes");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let _keeper = spawn_keeper("bytes-test", &graph, &sock);
    wait_for_socket(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    let result = ok_result(rpc(&mut stream, 1, "read_file", json!({})));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(result["bytes_b64"].as_str().unwrap())
        .unwrap();
    let on_disk = std::fs::read(&graph).unwrap();
    assert_eq!(bytes, on_disk, "read_file returns the real file bytes");
    assert!(
        result["sha256"].as_str().unwrap().starts_with("sha256:"),
        "the digest labels its algorithm"
    );
}

// x-f188 change 2: one store keeper per socket.
// ---------------------------------------------------------------

#[test]
fn concurrent_spawns_settle_on_one_keeper_and_losers_exit_three() {
    // AC2-HP: four keepers race for one graph's socket; one seat wins and
    // the other three exit 3 rather than each binding a second socket object
    // onto an unlinked path. Measured 2026-09-10: three losers were still
    // running on one socket, each holding a parsed 15MB graph.
    let home = short_home("seat");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    // Each racer's stderr lands in its own file: an unexpected exit names its
    // path (seat refusal, self-retire, bind failure) instead of a bare code.
    let stderr_of = |i: usize| home.join(format!("racer-{i}.stderr"));
    let mut keepers: Vec<Child> = (0..4)
        .map(|i| {
            Command::new(WORKER_BIN)
                .args([
                    "--store-keeper",
                    "--sock",
                    sock.to_str().unwrap(),
                    "--graph",
                    graph.to_str().unwrap(),
                    "--session",
                    "seat-race",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::from(std::fs::File::create(stderr_of(i)).unwrap()))
                .spawn()
                .expect("spawn racing keeper")
        })
        .collect();
    let explain =
        |i: usize| -> String { std::fs::read_to_string(stderr_of(i)).unwrap_or_default() };
    wait_for_socket(&sock);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut exited_three = 0;
    while Instant::now() < deadline {
        exited_three = 0;
        for (i, k) in keepers.iter_mut().enumerate() {
            if let Some(status) = k.try_wait().unwrap() {
                assert_eq!(
                    status.code(),
                    Some(3),
                    "racer {i} exits EXIT_SEAT_OWNED=3, got {status}: {}",
                    explain(i)
                );
                exited_three += 1;
            }
        }
        if exited_three == 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The seat winner still serves BEFORE any cleanup kill.
    let mut stream = UnixStream::connect(&sock).unwrap();
    let result = ok_result(rpc(&mut stream, 1, "read", json!({})));
    assert_eq!(result["entries"].as_array().unwrap().len(), 0);
    drop(stream);
    for k in keepers.iter_mut() {
        let _ = k.kill();
        let _ = k.wait();
    }
    assert_eq!(
        exited_three,
        3,
        "three losers must exit 3; racer stderr: 0={:?} 1={:?} 2={:?} 3={:?}",
        explain(0),
        explain(1),
        explain(2),
        explain(3)
    );
}

#[test]
fn a_keeper_whose_socket_was_rebound_by_another_exits_and_leaves_the_new_socket() {
    // AC2-ERR: when the path no longer names the inode this keeper bound,
    // an idle keeper exits WITHOUT unlinking - the rebound socket (the new
    // keeper's) stays in place.
    let home = short_home("rebound");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let mut a = spawn_keeper("rebound-a", &graph, &sock);
    wait_for_socket(&sock);
    use std::os::unix::fs::MetadataExt;
    let old_ino = std::fs::metadata(&sock).unwrap().ino();
    // Another process rebinds the path: unlink, bind a fresh listener.
    drop(UnixStream::connect(&sock).unwrap());
    std::fs::remove_file(&sock).unwrap();
    let reborn = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let new_ino = std::fs::metadata(&sock).unwrap().ino();
    assert_ne!(old_ino, new_ino, "the rebind must produce a fresh inode");

    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = loop {
        if let Some(status) = a.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "stale keeper never exited");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        exited.code().is_none() || exited.code() == Some(0),
        "seat loss is a clean exit, got {exited}"
    );
    // The rebound socket still has a live listener behind it: A never
    // unlinked what it does not own.
    let probe = UnixStream::connect(&sock);
    drop(reborn);
    assert!(
        probe.is_ok(),
        "the new keeper's socket must survive A's exit"
    );
    // Keep the graph file's lock tidy for the fixture home.
    let _ = std::fs::remove_file(home.join("graph.json.store.sock.lock"));
    let _ = old_ino;
}

// x-f188 change 3: build drift self-report, self-retire, busy Shutdown.
// ---------------------------------------------------------------

#[test]
fn a_keeper_on_a_rewritten_binary_self_retires_when_idle() {
    let home = short_home("drift");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\"entries\": []}").unwrap();
    let sock = home.join("graph.json.store.sock");
    let copy = home.join("worker-copy");
    std::fs::copy(WORKER_BIN, &copy).unwrap();
    let mut keeper = Command::new(&copy)
        .args([
            "--store-keeper",
            "--sock",
            sock.to_str().unwrap(),
            "--graph",
            graph.to_str().unwrap(),
            "--session",
            "drift",
        ])
        .env("FNO_STORE_KEEPER_DRIFT_CHECK_SECS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn keeper from copy");
    wait_for_socket(&sock);
    std::fs::remove_file(&copy).unwrap();
    std::fs::write(&copy, b"newer build bytes").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = keeper.try_wait().unwrap() {
            assert_eq!(status.code(), Some(0), "retire exits 0, got {status}");
            break;
        }
        assert!(Instant::now() < deadline, "stale keeper never self-retired");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!sock.exists(), "a retiring keeper unlinks its own socket");
    let _ = std::fs::remove_file(home.join("graph.json.store.sock.lock"));
}

#[test]
fn a_shutdown_during_a_mutation_answers_busy_and_keeps_serving() {
    // AC3-ERR: write ops blocked on a foreign graph-file flock hold the
    // keeper's write gate. Three chained ops keep the gate held past the
    // Shutdown ladder's bound (both paced by --lock-timeout-secs 2): the
    // keeper answers kind busy inside lock_timeout and keeps serving.
    let home = short_home("busy");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\"entries\": []}").unwrap();
    let sock = home.join("graph.json.store.sock");
    let mut keeper = spawn_keeper("busy-test", &graph, &sock);
    wait_for_socket(&sock);
    let lock_path = PathBuf::from(format!("{}.lock", graph.canonicalize().unwrap().display()));
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&lock_path)
        .unwrap();
    holder.try_lock().expect("foreign wedge lock");

    // Six chained write ops keep the gate held well past the Shutdown
    // ladder's bound (lock_timeout 10s): each op holds the gate for its own
    // 10s flock wait, so the ladder expires into the busy reply instead of
    // slipping into the gap between two ops.
    let op_sock = sock.clone();
    let op_thread = std::thread::spawn(move || {
        // Pre-stage every request BEFORE Shutdown: each handler thread then
        // queues on the write gate, and the gate hands over between ops
        // without a connect gap the Shutdown ladder could slip into.
        let req = json!({
            "name": "append_progress_note",
            "params": {"node_id": "ab-x", "note": {"ts": "t", "text": "y"}}
        });
        let frame = {
            let payload =
                serde_json::to_vec(&json!({"id": 1, "method": "op", "params": req})).unwrap();
            let mut f = vec![TAG_REQUEST];
            f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            f.extend_from_slice(&payload);
            f
        };
        let mut streams = Vec::new();
        for _ in 0..6 {
            if let Ok(mut s) = UnixStream::connect(&op_sock) {
                let _ = s.write_all(&frame);
                streams.push(s);
            }
        }
        for mut s in streams {
            let _ = read_frame(&mut s);
        }
    });
    // Give the ops a real head start: on a loaded runner 300ms let Shutdown
    // reach a still-free gate, and the keeper answered ok instead of busy.
    std::thread::sleep(Duration::from_secs(2));
    let mut s = UnixStream::connect(&sock).unwrap();
    write_frame(&mut s, TAG_SHUTDOWN, &[]);
    let started = Instant::now();
    let (tag, payload) = read_frame(&mut s).expect("busy reply frame");
    let elapsed = started.elapsed();
    assert_eq!(tag, TAG_RESPONSE, "busy rides the response tag");
    let reply: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reply["ok"], false, "busy is a refusal: {reply}");
    assert_eq!(reply["error"]["kind"], "busy", "kind is busy: {reply}");
    assert!(
        elapsed < Duration::from_secs(12),
        "busy answers inside lock_timeout, got {elapsed:?}"
    );
    // The keeper kept serving: Identify still answers after the busy.
    let mut probe = UnixStream::connect(&sock).unwrap();
    write_frame(&mut probe, TAG_IDENTIFY, &[]);
    let (itag, _ipayload) = read_frame(&mut probe).expect("identify after busy");
    assert_eq!(itag, TAG_IDENTIFY_REPLY, "the keeper kept serving");
    // Unwedge and reap.
    drop(holder);
    let _ = op_thread.join();
    let _ = keeper.child.kill();
    let _ = keeper.child.wait();
    let _ = std::fs::remove_file(home.join("graph.json.store.sock.lock"));
}
