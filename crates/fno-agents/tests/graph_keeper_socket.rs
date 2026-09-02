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
    let dir = std::env::temp_dir().join(format!(
        "fno-gsk{}_{}_{}",
        std::process::id(),
        tag,
        n
    ));
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
    assert!(
        !sock.exists(),
        "an explicit shutdown unlinks its socket"
    );
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
    let result = ok_result(rpc(&mut stream, 2, "op", json!({
        "name": "append_progress_note",
        "params": {"node_id": "ab-missing", "note": {"ts": "t", "text": "x"}}
    })));
    assert_eq!(result["op"]["found"], false, "absent node answers found=false");
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
    assert!(
        stderr.contains("already has a live listener"),
        "the refusal names the socket: {stderr}"
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
