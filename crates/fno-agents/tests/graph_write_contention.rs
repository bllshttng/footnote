//! The whole-graph write cycle under contention: a stale base survives
//! three intervening commits, a real conflict names the row that moved,
//! concurrent disjoint writers all land, and a commit needs no base_digests.
//! Properties, never durations: no sleeps, no timing assertions.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WORKER_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-worker");

const TAG_REQUEST: u8 = 1;
const TAG_RESPONSE: u8 = 4;

struct Keeper {
    child: Child,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_home(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("fno-gwc{}_{}_{}", std::process::id(), tag, n));
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

fn begin(stream: &mut UnixStream, id: u64) -> Value {
    let reply = rpc(stream, id, "begin", json!({}));
    assert_eq!(reply["ok"], json!(true), "begin must succeed: {reply}");
    reply["result"].clone()
}

fn commit(
    stream: &mut UnixStream,
    id: u64,
    base_version: &str,
    row: Value,
    base_plan_rungs: Value,
) -> Value {
    let mut params = json!({
        "base_version": base_version,
        "base_plan_rungs": base_plan_rungs,
        "changed": [row],
        "removed": [],
        "plan_rungs": {},
    });
    if base_plan_rungs.is_null() {
        params["base_plan_rungs"] = json!({});
    }
    rpc(stream, id, "commit_rows", params)
}

fn write_graph(tag: &str) -> (PathBuf, PathBuf, PathBuf, Keeper) {
    let home = temp_home(tag);
    let graph = home.join("graph.json");
    fno_agents::graph_store::seed_rows(&graph, &[
        json!({"id":"x-a","slug":"x-a","title":"a","type":"feature","status":"ready","priority":"p2"}),
        json!({"id":"x-b","slug":"x-b","title":"b","type":"feature","status":"ready","priority":"p2"}),
        json!({"id":"x-c","slug":"x-c","title":"c","type":"feature","status":"ready","priority":"p2"}),
        json!({"id":"x-d","slug":"x-d","title":"d","type":"feature","status":"ready","priority":"p2"}),
    ]).unwrap();
    let sock = home.join("graph.json.store.sock");
    let keeper = spawn_keeper(tag, &graph, &sock);
    wait_for_socket(&sock);
    (home, graph, sock, keeper)
}

fn read_rows(graph: &Path) -> Vec<Value> {
    // graph.db is the only store; the json file is a frozen mirror under it.
    fno_agents::graph_store::read_rows(graph).unwrap()
}

fn row_title<'a>(rows: &'a [Value], id: &str) -> &'a str {
    rows.iter()
        .find(|row| row["id"] == json!(id))
        .map(|row| row["title"].as_str().unwrap())
        .unwrap_or_else(|| panic!("row {id} missing"))
}

/// AC1-HP + AC11-HP: a base three commits behind still commits when no
/// interleaving writer touched its rows, and the caller's own retry begins
/// did not evict it.
#[test]
fn a_base_three_commits_behind_still_commits_when_rows_are_disjoint() {
    let (_home, graph, sock, _keeper) = write_graph("stale-disjoint");
    let mut stream = UnixStream::connect(&sock).unwrap();

    let first = begin(&mut stream, 1);
    let base = first["version"].as_str().unwrap().to_string();
    // Two more begins, the shape of a caller's own retry attempts: they
    // must not cost the caller its base.
    let _ = begin(&mut stream, 2);
    let _ = begin(&mut stream, 3);

    // Three intervening commits, each landing on a row the final mutation
    // never touches.
    for (i, id) in ["x-b", "x-c", "x-d"].iter().enumerate() {
        let snap = begin(&mut stream, 10 + i as u64);
        let version = snap["version"].as_str().unwrap().to_string();
        let mut row = snap["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == json!(id))
            .cloned()
            .unwrap();
        row["title"] = json!(format!("{id}-moved"));
        let reply = commit(&mut stream, 20 + i as u64, &version, row, json!({}));
        assert_eq!(reply["ok"], json!(true), "intervening commit {id}: {reply}");
    }

    // The final mutation rides the ORIGINAL base version.
    let mut row = first["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!("x-a"))
        .cloned()
        .unwrap();
    row["title"] = json!("a-moved");
    let reply = commit(&mut stream, 30, &base, row, json!({}));
    assert_eq!(
        reply["ok"],
        json!(true),
        "a disjoint stale-base commit must land: {reply}"
    );

    let rows = read_rows(&graph);
    assert_eq!(row_title(&rows, "x-a"), "a-moved");
    assert_eq!(row_title(&rows, "x-b"), "x-b-moved");
    assert_eq!(row_title(&rows, "x-c"), "x-c-moved");
    assert_eq!(row_title(&rows, "x-d"), "x-d-moved");
}

/// AC2-ERR: a stale base whose row an interleaving writer changed conflicts
/// and names that row alone.
#[test]
fn a_stale_base_over_a_moved_row_conflicts_naming_that_row() {
    let (_home, _graph, sock, _keeper) = write_graph("stale-conflict");
    let mut stream = UnixStream::connect(&sock).unwrap();

    let first = begin(&mut stream, 1);
    let base = first["version"].as_str().unwrap().to_string();

    // An interleaving writer moves x-a.
    let snap = begin(&mut stream, 2);
    let version = snap["version"].as_str().unwrap().to_string();
    let mut row = snap["entries"].as_array().unwrap()[0].clone();
    assert_eq!(row["id"], json!("x-a"));
    row["title"] = json!("a-theirs");
    let reply = commit(&mut stream, 3, &version, row, json!({}));
    assert_eq!(reply["ok"], json!(true), "intervening commit: {reply}");

    // The stale mutation changes the same row.
    let mut mine = first["entries"].as_array().unwrap()[0].clone();
    mine["title"] = json!("a-mine");
    let reply = commit(&mut stream, 4, &base, mine, json!({}));
    assert_eq!(
        reply["ok"],
        json!(false),
        "same-row stale commit must conflict"
    );
    assert_eq!(reply["error"]["kind"], json!("conflict"), "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("x-a"),
        "must name the moved row: {message}"
    );
}

/// AC3-HP: N concurrent whole-graph writers on disjoint nodes all land.
#[test]
fn concurrent_writers_on_disjoint_nodes_all_land() {
    const WRITERS: usize = 4;
    let (_home, graph, sock, _keeper) = write_graph("concurrent");
    let sock_for = |_: usize| sock.clone();
    let handles: Vec<_> = (0..WRITERS)
        .map(|i| {
            let sock = sock_for(i);
            std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&sock).unwrap();
                let snap = begin(&mut stream, 1);
                let version = snap["version"].as_str().unwrap().to_string();
                let id = format!("x-{}", ['a', 'b', 'c', 'd'][i]);
                let mut row = snap["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["id"] == json!(id))
                    .cloned()
                    .unwrap();
                row["title"] = json!(format!("{id}-w{i}"));
                commit(&mut stream, 2, &version, row, json!({}))
            })
        })
        .collect();
    for (i, handle) in handles.into_iter().enumerate() {
        let reply = handle.join().unwrap();
        assert_eq!(
            reply["ok"],
            json!(true),
            "writer {i} must land under contention: {reply}"
        );
    }
    let rows = read_rows(&graph);
    for (i, c) in ['a', 'b', 'c', 'd'].iter().enumerate() {
        let id = format!("x-{c}");
        assert_eq!(row_title(&rows, &id), format!("{id}-w{i}"));
    }
}

/// A commit carrying non-empty base_plan_rungs does not conflict on an
/// untouched row: the rung map feeds both sides of the compare.
#[test]
fn nonempty_base_plan_rungs_do_not_conflict_on_an_untouched_row() {
    let (_home, _graph, sock, _keeper) = write_graph("rungs");
    let mut stream = UnixStream::connect(&sock).unwrap();

    let first = begin(&mut stream, 1);
    let base = first["version"].as_str().unwrap().to_string();

    for (i, id) in ["x-b", "x-c", "x-d"].iter().enumerate() {
        let snap = begin(&mut stream, 10 + i as u64);
        let version = snap["version"].as_str().unwrap().to_string();
        let mut row = snap["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == json!(id))
            .cloned()
            .unwrap();
        row["title"] = json!(format!("{id}-moved"));
        let reply = commit(&mut stream, 20 + i as u64, &version, row, json!({}));
        assert_eq!(reply["ok"], json!(true), "intervening commit {id}: {reply}");
    }

    let mut row = first["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!("x-a"))
        .cloned()
        .unwrap();
    row["title"] = json!("a-moved");
    let reply = commit(&mut stream, 30, &base, row, json!({"x-a": "done"}));
    assert_eq!(
        reply["ok"],
        json!(true),
        "runge commit over a stale base must land: {reply}"
    );
}

/// AC10-EDGE: a commit_rows that omits base_digests entirely commits rather
/// than refusing.
#[test]
fn a_commit_that_omits_base_digests_commits() {
    let (_home, graph, sock, _keeper) = write_graph("omit");
    let mut stream = UnixStream::connect(&sock).unwrap();

    let snap = begin(&mut stream, 1);
    let version = snap["version"].as_str().unwrap().to_string();
    let mut row = snap["entries"].as_array().unwrap()[0].clone();
    row["title"] = json!("a-written");
    let reply = rpc(
        &mut stream,
        2,
        "commit_rows",
        json!({
            "base_version": version,
            "base_plan_rungs": {},
            "changed": [row],
            "removed": [],
            "plan_rungs": {},
        }),
    );
    assert_eq!(
        reply["ok"],
        json!(true),
        "commit_rows without base_digests must commit: {reply}"
    );
    let rows = read_rows(&graph);
    assert_eq!(row_title(&rows, "x-a"), "a-written");
}
