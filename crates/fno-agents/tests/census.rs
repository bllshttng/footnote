//! The census's keeper walk against a real keeper (x-f188): a spawned store
//! keeper is found by the ps walk, classified by its build self-report, and
//! its row carries the sock the restart leg cycles.

use base64::Engine as _;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const WORKER_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-worker");

fn short_home(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("fno-census{}_{}_{}", std::process::id(), tag, n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

fn rpc(stream: &mut UnixStream, id: u64, method: &str, params: Value) -> Value {
    let req = json!({"id": id, "method": method, "params": params});
    let payload = serde_json::to_vec(&req).unwrap();
    let mut frame = vec![1u8]; // TAG_REQUEST
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    stream.write_all(&frame).unwrap();
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 4, "response tag");
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn census_finds_a_store_keeper_by_self_report() {
    let home = short_home("walk");
    let graph = home.join("graph.json");
    std::fs::write(&graph, "{\"entries\": []}").unwrap();
    let sock = home.join("graph.json.store.sock");
    let _keeper = Command::new(WORKER_BIN)
        .args([
            "--store-keeper",
            "--sock",
            sock.to_str().unwrap(),
            "--graph",
            graph.to_str().unwrap(),
            "--session",
            "census-walk",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn keeper");
    wait_for_socket(&sock);

    let rows = fno_agents::census::census().await;
    let mine: Vec<&Value> = rows
        .iter()
        .filter(|r| {
            r["component"] == "store-keeper" && r["sock"] == sock.display().to_string()
        })
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "our keeper is one row: {}",
        serde_json::to_string(&rows).unwrap_or_default()
    );
    let row = mine[0];
    assert_eq!(row["verdict"], "current", "self-report classifies: {row}");
    assert_eq!(row["evidence"], "build self-report");
    assert_eq!(
        row["sock"],
        sock.display().to_string(),
        "the restart leg cycles by this sock"
    );
    assert_eq!(row["on_restart"], "cycles; the next read respawns it");
    assert!(row["pid"].is_u64(), "the row names the pid");
    // The census answers a real RPC through the same socket.
    let mut stream = UnixStream::connect(&sock).unwrap();
    let reply = rpc(&mut stream, 1, "read", json!({}));
    assert_eq!(reply["ok"], true);
}
