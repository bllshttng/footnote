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
    Keeper { child }
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
            "name": "append_wave_note",
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
fn a_lost_commit_rows_reply_is_recoverable_over_a_fresh_socket() {
    let home = short_home("lost-write");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let _keeper = spawn_keeper("lost-write-test", &graph, &sock);
    wait_for_socket(&sock);

    let mut stream = UnixStream::connect(&sock).unwrap();
    let begin = ok_result(rpc(&mut stream, 1, "begin", json!({})));
    let params = json!({
        "request_id": "r1",
        "base_version": begin["version"],
        "base_digests": begin["base_digests"],
        "base_plan_rungs": {},
        "changed": [{"id": "x-disconnected", "title": "disconnected"}],
        "removed": [],
        "plan_rungs": {},
    });
    let request = json!({"id": 2, "method": "commit_rows", "params": params});
    write_frame(
        &mut stream,
        TAG_REQUEST,
        serde_json::to_vec(&request).unwrap().as_slice(),
    );
    drop(stream);

    let mut status_stream = UnixStream::connect(&sock).unwrap();
    let mut status = ok_result(rpc(
        &mut status_stream,
        3,
        "write_status",
        json!({"request_id": "r1"}),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while status["state"] == json!("in_flight") {
        assert!(Instant::now() < deadline, "commit_rows never reached done");
        std::thread::sleep(Duration::from_millis(20));
        status = ok_result(rpc(
            &mut status_stream,
            4,
            "write_status",
            json!({"request_id": "r1"}),
        ));
    }
    assert_eq!(status["state"], json!("done"));
    assert_eq!(status["reply"]["ok"], json!(true));
    assert_eq!(status["reply"]["result"]["entries"], Value::Null);
    assert_eq!(status["reply"]["result"]["entries_elided"], json!(true));
    let rows = ok_result(rpc(&mut status_stream, 5, "read", json!({})));
    assert_eq!(rows["entries"][0]["id"], json!("x-disconnected"));
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
            "name": "append_wave_note",
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
            "name": "append_wave_note",
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
fn keeper_holds_its_seat_lock() {
    // The flock is the single-flight gate and the open file description is
    // what holds it: a SERVING keeper must still hold <sock>.lock. A lock
    // this process can take while the keeper answers is the dropped-guard
    // fault (the take_seat File was never bound, so the flock closed at the
    // end of the if condition, before the guarded body ran).
    let home = short_home("seatlock");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let _keeper = spawn_keeper("seatlock-test", &graph, &sock);
    wait_for_socket(&sock); // positive control: the keeper bound and serves
    let lock_path = PathBuf::from(format!("{}.lock", sock.display()));
    let probe = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap();
    match probe.try_lock() {
        Ok(()) => panic!(
            "a serving keeper does not hold its seat lock: this process acquired {}",
            lock_path.display()
        ),
        Err(e) => {
            let io_err: std::io::Error = e.into();
            assert_eq!(
                io_err.kind(),
                std::io::ErrorKind::WouldBlock,
                "the seat lock must be held by the serving keeper ({}): {io_err}",
                lock_path.display()
            );
        }
    }
}

#[test]
fn a_keeper_whose_socket_was_rebound_by_another_exits_and_leaves_the_new_socket() {
    // AC2-ERR: when the path no longer names the inode this keeper bound,
    // an idle keeper exits EXIT_SEAT_OWNED=3 - the same verdict the
    // pre-bind ladder spells - WITHOUT unlinking: the rebound socket (the
    // new listener's) stays in place.
    let home = short_home("rebound");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let a_stderr = home.join("rebound-a.stderr");
    let mut a = Keeper {
        child: Command::new(WORKER_BIN)
            .args([
                "--store-keeper",
                "--sock",
                sock.to_str().unwrap(),
                "--graph",
                graph.to_str().unwrap(),
                "--session",
                "rebound-a",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&a_stderr).unwrap()))
            .spawn()
            .expect("spawn keeper A"),
    };
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
    let stderr = std::fs::read_to_string(&a_stderr).unwrap_or_default();
    assert_eq!(
        exited.code(),
        Some(3),
        "seat loss exits EXIT_SEAT_OWNED=3, got {exited}: {stderr}"
    );
    assert!(
        stderr.contains(&sock.display().to_string()),
        "the seat-loss line names the socket: {stderr}"
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
            "name": "append_wave_note",
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

// The 2026-09-14 temp repro, ported. Two idea-style
// appenders race two note loops on one keeper; no ok-replied append may be
// lost and no note write may be reverted.
// ---------------------------------------------------------------

#[test]
fn two_concurrent_idea_commits_survive_concurrent_note_writes() {
    let home = short_home("race");
    let graph = home.join("graph.json");
    // 500 seed rows: enough that a whole-file publish can drop foreign rows,
    // small enough that a debug build's publish stays well inside the 10s
    // lock deadline (the 3000-row repro form held the flock for seconds per
    // write and starved the keeper instead of racing it).
    let seed: Vec<Value> = (0..500)
        .map(|i| {
            json!({
                "id": format!("r-{i:04}"),
                "slug": format!("slug-r-{i:04}"),
                "title": format!("seed {i}"),
                "type": "feature",
                "status": "ready",
                "priority": "p2",
            })
        })
        .collect();
    std::fs::write(
        &graph,
        serde_json::to_string(&json!({ "entries": seed })).unwrap(),
    )
    .unwrap();
    let sock = home.join("graph.json.store.sock");
    let keeper = spawn_keeper("race-test", &graph, &sock);
    wait_for_socket(&sock);

    // Two note loops: 12 in-process replace_state writes each, on rows the
    // appenders never touch. This is the shape every `fno backlog note`
    // takes: read the whole graph outside the lock, publish it whole.
    let note_graph = graph.clone();
    let note_handles: Vec<_> = ["r-0000", "r-0001"]
        .iter()
        .map(|node| {
            let g = note_graph.clone();
            let node = node.to_string();
            std::thread::spawn(move || {
                let mut ok = 0u64;
                for i in 0..12 {
                    let rev =
                        fno_agents::backlog::node_state::current_revision(&g, &node).unwrap_or(0);
                    let input = fno_agents::backlog::node_state::StateWriteInput {
                        node_id: node.clone(),
                        body: format!("race note {i}"),
                        if_revision: Some(rev),
                        source_session_id: None,
                        source_harness: None,
                        reads: None,
                    };
                    if fno_agents::backlog::node_state::replace_state(&g, &input).is_ok() {
                        ok += 1;
                    }
                    // Yield the file lock between writes: a tight in-process
                    // loop re-acquires flock before a starving waiter is
                    // scheduled, and the keeper's 10s deadline fires instead
                    // of the race this test exists to exercise.
                    std::thread::sleep(Duration::from_millis(5));
                }
                (node, ok)
            })
        })
        .collect();

    // Two idea-style appenders: 30 begin + commit_rows appends each through
    // the keeper socket, retrying kind conflict like cmd_idea does.
    let appender_handles: Vec<_> = (0..2)
        .map(|w| {
            let s = sock.clone();
            std::thread::spawn(move || {
                let prefix = ['a', 'b'][w];
                let mut stream = UnixStream::connect(&s).unwrap();
                let mut landed: Vec<String> = Vec::new();
                let mut conflicts = 0u64;
                for i in 0..20 {
                    let id = format!("{prefix}-app-{i:04}");
                    let row = json!({
                        "id": id,
                        "slug": format!("slug-{id}"),
                        "title": format!("appended {id}"),
                        "type": "feature",
                        "status": "intake",
                        "priority": "p2",
                    });
                    let mut landed_here = false;
                    for attempt in 0..30u64 {
                        let begin = ok_result(rpc(&mut stream, attempt, "begin", json!({})));
                        let version = begin["version"].as_str().unwrap().to_string();
                        let digests = begin["base_digests"].clone();
                        let reply = rpc(
                            &mut stream,
                            attempt,
                            "commit_rows",
                            json!({
                                "base_version": version,
                                "base_digests": digests,
                                "changed": [row],
                                "removed": [],
                                "attempt": attempt + 1,
                            }),
                        );
                        if reply.get("ok") == Some(&json!(true)) {
                            landed.push(id.clone());
                            landed_here = true;
                            break;
                        }
                        let kind = reply["error"]["kind"].as_str().unwrap_or("");
                        assert_eq!(
                            kind, "conflict",
                            "appender {w} row {id}: unexpected error: {reply}"
                        );
                        conflicts += 1;
                    }
                    assert!(
                        landed_here,
                        "appender {w}: row {id} never landed in 30 attempts"
                    );
                }
                (landed, conflicts)
            })
        })
        .collect();

    let note_out: Vec<(String, u64)> = note_handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    let appender_out: Vec<(Vec<String>, u64)> = appender_handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();

    // Positive control: the race must have produced contention. The repro
    // measured 19 commit_rows conflicts; zero means the threads never
    // interleaved and this run proves nothing.
    let total_conflicts: u64 = appender_out.iter().map(|(_, c)| c).sum();
    assert!(
        total_conflicts > 0,
        "did not race: zero commit_rows conflicts across both appenders"
    );

    // Every append that answered ok must be in the final file.
    let final_raw = std::fs::read_to_string(&graph).unwrap();
    let final_graph: Value = serde_json::from_str(&final_raw).unwrap();
    let final_ids: std::collections::BTreeSet<String> = final_graph["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect();
    for (landed, _) in &appender_out {
        for id in landed {
            assert!(
                final_ids.contains(id),
                "append {id} answered ok but is missing from the final graph"
            );
        }
    }

    // Each note node's final revision equals its successful write count: a
    // reverted note write (clobbered by a stale whole-file publish) reads as
    // a revision below the count of ok answers.
    for (node, ok) in &note_out {
        let row = final_graph["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"].as_str() == Some(node.as_str()))
            .unwrap();
        let revision = row["current_state"]["revision"].as_u64().unwrap();
        assert_eq!(
            revision, *ok,
            "node {node} answered ok {ok} times but its final revision is {revision}"
        );
    }

    let _ = std::fs::remove_file(home.join("graph.json.store.sock.lock"));
    drop(keeper);
}

// A shutdown never cuts an in-flight request, and a
// request that read an ok reply always has its row in the file.
// ---------------------------------------------------------------

#[test]
fn a_shutdown_mid_commit_never_loses_an_ok_reply() {
    let home = short_home("shutrace");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let sock = home.join("graph.json.store.sock");
    let mut keeper = spawn_keeper("shutrace-test", &graph, &sock);
    wait_for_socket(&sock);

    // Pre-stage six commit_rows requests on separate connections. Each
    // handler thread holds an inflight guard from handling through the reply
    // write, so a Shutdown that acks proves every reply was already written.
    // A connection whose request never entered handling is cut legally: it
    // records as a hangup outcome, never a panic.
    let staged_sock = sock.clone();
    let (staged_tx, staged_rx) = std::sync::mpsc::channel::<()>();
    let staged = std::thread::spawn(move || {
        let mut conns: Vec<(UnixStream, String)> = Vec::new();
        let mut outcomes: Vec<(String, bool)> = Vec::new();
        for i in 0..6 {
            let mut s = match UnixStream::connect(&staged_sock) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let id = format!("m-app-{i:04}");
            write_frame(
                &mut s,
                TAG_REQUEST,
                &serde_json::to_vec(&json!({"id": i, "method": "begin", "params": {}})).unwrap(),
            );
            let (tag, payload) = match read_frame(&mut s) {
                Some(frame) => frame,
                None => {
                    // Cut before the begin was served: a hangup outcome.
                    outcomes.push((id, false));
                    continue;
                }
            };
            assert_eq!(tag, TAG_RESPONSE);
            let reply: Value = serde_json::from_slice(&payload).unwrap();
            if reply.get("ok") != Some(&json!(true)) {
                outcomes.push((id, false));
                continue;
            }
            let begin = &reply["result"];
            let version = begin["version"].as_str().unwrap_or_default().to_string();
            let row = json!({
                "id": id,
                "slug": format!("slug-{id}"),
                "title": format!("mid-commit {id}"),
                "type": "feature",
                "status": "intake",
                "priority": "p2",
            });
            let req = json!({
                "id": 100 + i,
                "method": "commit_rows",
                "params": {
                    "base_version": version,
                    "base_digests": begin["base_digests"],
                    "changed": [row],
                    "removed": [],
                },
            });
            let payload = serde_json::to_vec(&req).unwrap();
            let mut f = vec![TAG_REQUEST];
            f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            f.extend_from_slice(&payload);
            s.write_all(&f).unwrap();
            s.flush().unwrap();
            conns.push((s, id));
        }
        // Signal main BEFORE reading replies: every commit frame is staged
        // and every handler that took one is mid-drain, which is the exact
        // window the contract is about.
        let _ = staged_tx.send(());
        // Read every staged reply with its outcome kind.
        for (mut s, id) in conns {
            match read_frame(&mut s) {
                Some((tag, payload)) => {
                    assert_eq!(tag, TAG_RESPONSE);
                    let reply: Value = serde_json::from_slice(&payload).unwrap();
                    outcomes.push((id, reply.get("ok") == Some(&json!(true))));
                }
                None => outcomes.push((id, false)),
            }
        }
        outcomes
    });
    // Shut down while the staged handlers drain.
    let _ = staged_rx.recv_timeout(Duration::from_secs(30));
    let mut s = UnixStream::connect(&sock).unwrap();
    write_frame(&mut s, TAG_SHUTDOWN, &[]);
    let (tag, payload) = read_frame(&mut s).expect("shutdown reply");
    assert_eq!(tag, TAG_RESPONSE);
    let reply: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        reply["result"], "shutdown",
        "an unblocked keeper acks: {reply}"
    );
    let outcomes = staged.join().unwrap();

    // Late arrivals: sent after the ack, they meet the dying keeper and read
    // a hangup BEFORE any publish.
    let mut late_ok = 0;
    for i in 0..2 {
        let late = UnixStream::connect(&sock);
        match late {
            Err(_) => continue, // socket already unlinked: hangup by refusal
            Ok(mut s) => {
                let begin = rpc(&mut s, 900 + i as u64, "begin", json!({}));
                // Either the frame round-trips (keeper still draining) or the
                // stream is cut; both are legal, only ok-published rows count.
                if begin.get("ok") == Some(&json!(true)) {
                    late_ok += 1;
                }
            }
        }
    }
    let _ = late_ok;

    // Reap: the keeper exits 0 on its own.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match keeper.child.try_wait().expect("reap check") {
            Some(status) => {
                assert_eq!(status.code(), Some(0), "shutdown exits 0: {status}");
                break;
            }
            None => {
                assert!(Instant::now() < deadline, "keeper did not exit");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    assert!(!sock.exists(), "shutdown unlinks its socket");

    // THE CONTRACT: every staged connection that read an ok reply has its
    // row in the file. A connection cut before its reply has no row (it
    // never read ok).
    let final_raw = std::fs::read_to_string(&graph).unwrap();
    let final_graph: Value = serde_json::from_str(&final_raw).unwrap();
    let final_ids: std::collections::BTreeSet<String> = final_graph["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect();
    let mut ok_replies = 0;
    for (id, ok) in &outcomes {
        if *ok {
            ok_replies += 1;
            assert!(
                final_ids.contains(id),
                "commit {id} answered ok but is missing from the file"
            );
        }
    }
    assert!(
        ok_replies >= 1,
        "positive control: at least one staged commit must have answered ok, got {outcomes:?}"
    );
    let _ = std::fs::remove_file(home.join("graph.json.store.sock.lock"));
}
