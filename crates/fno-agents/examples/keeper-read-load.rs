//! Load driver for the graph store keeper's read path (AC10-AC12).
//!
//! Copies a store read only (sqlite `VACUUM INTO` of `<source>/graph.db`
//! plus a copy of `<source>/graph.json`) into `--store`, launches
//! `fno-agents-worker --store-keeper` on the copy, opens N client threads
//! that each send M frames of one method, samples the keeper's RSS every
//! 5 s with `ps`, and prints one END line.
//!
//! Sandbox discipline: the source is opened read only; the keeper runs
//! against the store copy's own socket under `--store`; the only process
//! this driver signals is the exact child it spawned.
//!
//! ```text
//! cargo build --release -p fno-agents --example keeper-read-load
//! cargo run --release -p fno-agents --example keeper-read-load -- \
//!     --worker crates/fno-agents/target/release/fno-agents-worker \
//!     --store /tmp/krl-copy --backend sqlite --method read --clients 12 --per 4
//! ```

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RPS_FLOOR: f64 = 4.0;
const IDLE_RSS_CEILING_MB: f64 = 500.0;

struct Args {
    worker: PathBuf,
    source: PathBuf,
    store: PathBuf,
    backend: String,
    method: String,
    clients: usize,
    per: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut worker = None;
    let mut source = None;
    let mut store = None;
    let mut backend = String::from("sqlite");
    let mut method = None;
    let mut clients = 12;
    let mut per = 4;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--worker" => worker = Some(PathBuf::from(it.next().ok_or("--worker needs a value")?)),
            "--source" => source = Some(PathBuf::from(it.next().ok_or("--source needs a value")?)),
            "--store" => store = Some(PathBuf::from(it.next().ok_or("--store needs a value")?)),
            "--backend" => {
                backend = it.next().ok_or("--backend needs a value")?;
                if backend != "json" && backend != "sqlite" {
                    return Err(format!(
                        "unknown backend {backend:?}; names are json and sqlite"
                    ));
                }
            }
            "--method" => {
                method = Some(it.next().ok_or("--method needs a value")?);
            }
            "--clients" => {
                clients = it
                    .next()
                    .ok_or("--clients needs a value")?
                    .parse()
                    .map_err(|_| "--clients needs a number")?;
            }
            "--per" => {
                per = it
                    .next()
                    .ok_or("--per needs a value")?
                    .parse()
                    .map_err(|_| "--per needs a number")?;
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    let method = method.ok_or("missing --method")?;
    if method != "read" && method != "begin" && method != "api-rows" {
        return Err(format!(
            "unknown method {method:?}; names are read, begin, api-rows"
        ));
    }
    Ok(Args {
        worker: worker.ok_or("missing --worker")?,
        source: source.unwrap_or_else(default_source),
        store: store.ok_or("missing --store")?,
        backend,
        method,
        clients,
        per,
    })
}

fn default_source() -> PathBuf {
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".fno"))
        .unwrap_or_else(|_| PathBuf::from(".fno"))
}

/// AC11-ERR: the copy never lands under the live fleet's state root.
fn refuse_store_under_home(store: &Path) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "no HOME")?;
    let home_fno = PathBuf::from(home).join(".fno");
    let home_fno = home_fno.canonicalize().unwrap_or(home_fno);
    let candidate = store.canonicalize().unwrap_or_else(|_| store.to_path_buf());
    if candidate == home_fno || candidate.starts_with(&home_fno) {
        return Err(format!(
            "refusing --store under ~/.fno: {}",
            store.display()
        ));
    }
    Ok(())
}

/// The store copy: an online `VACUUM INTO` of the source db (read-only
/// open) plus a plain copy of graph.json.
fn copy_store(source: &Path, store: &Path) -> Result<(), String> {
    let src_db = source.join("graph.db");
    let src_json = source.join("graph.json");
    if !src_db.exists() {
        return Err(format!("no graph.db in source dir: {}", source.display()));
    }
    if !src_json.exists() {
        return Err(format!("no graph.json in source dir: {}", source.display()));
    }
    let src =
        rusqlite::Connection::open_with_flags(&src_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| e.to_string())?;
    let dst_path = store.join("graph.db");
    let _ = std::fs::remove_file(&dst_path);
    src.execute("VACUUM INTO ?1", [dst_path.to_string_lossy().as_ref()])
        .map_err(|e| e.to_string())?;
    std::fs::copy(&src_json, store.join("graph.json")).map_err(|e| e.to_string())?;
    Ok(())
}

fn stamp_backend(store: &Path, backend: &str) -> Result<(), String> {
    let conn = rusqlite::Connection::open(store.join("graph.db")).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO graph_meta(key, value) VALUES('backend', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [backend],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(1u8);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn method_payload(method: &str) -> Vec<u8> {
    let body = match method {
        "read" => r#"{"id":1,"method":"read","params":{}}"#,
        "begin" => r#"{"id":1,"method":"begin"}"#,
        _ => r#"{"id":1,"method":"api","params":{"op":"rows"}}"#,
    };
    frame(body.as_bytes())
}

/// One connection, `count` sequential frames; returns the ok replies.
fn one_client(sock: &Path, payload: &[u8], count: usize) -> Result<usize, String> {
    let mut client = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    let mut ok = 0;
    for _ in 0..count {
        client.write_all(payload).map_err(|e| e.to_string())?;
        let mut head = [0u8; 5];
        client.read_exact(&mut head).map_err(|e| e.to_string())?;
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).map_err(|e| e.to_string())?;
        if head[0] == 4 && body.windows(9).any(|w| w == b"\"ok\":true") {
            ok += 1;
        } else if ok == 0 {
            eprintln!(
                "first reply: tag={} body={}",
                head[0],
                String::from_utf8_lossy(&body[..body.len().min(300)])
            );
        }
    }
    Ok(ok)
}

fn sample_rss_mb(pid: u32) -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim().parse::<f64>().ok().map(|kb| kb / 1024.0)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("keeper-read-load: {msg}");
            std::process::exit(2);
        }
    };
    if let Err(msg) = refuse_store_under_home(&args.store) {
        eprintln!("keeper-read-load: {msg}");
        std::process::exit(2);
    }
    if let Err(msg) = std::fs::create_dir_all(&args.store) {
        eprintln!("keeper-read-load: {}: {msg}", args.store.display());
        std::process::exit(2);
    }
    if let Err(msg) = copy_store(&args.source, &args.store) {
        eprintln!("keeper-read-load: {msg}");
        std::process::exit(2);
    }
    if args.backend == "sqlite" {
        if let Err(msg) = stamp_backend(&args.store, &args.backend) {
            eprintln!("keeper-read-load: {msg}");
            std::process::exit(2);
        }
    }
    let sock = args.store.join("load.store.sock");
    let _ = std::fs::remove_file(&sock);
    let mut worker = std::process::Command::new(&args.worker)
        .args(["--store-keeper", "--sock"])
        .arg(&sock)
        .arg("--graph")
        .arg(args.store.join("graph.json"))
        .env("FNO_STORE_KEEPER_IDLE_SECS", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn fno-agents-worker");
    let code = run_load(&mut worker, &sock, &args);
    let _ = worker.kill();
    let _ = worker.wait();
    std::process::exit(code);
}

fn run_load(worker: &mut std::process::Child, sock: &Path, args: &Args) -> i32 {
    if !wait_for_socket(sock, Duration::from_secs(20)) {
        eprintln!("keeper-read-load: keeper never bound {}", sock.display());
        return 2;
    }
    let pid = worker.id();
    let samples: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler_stop = Arc::clone(&stop);
    let sampler_samples = Arc::clone(&samples);
    let sampler = std::thread::spawn(move || {
        while !sampler_stop.load(Ordering::SeqCst) {
            if let Some(rss) = sample_rss_mb(pid) {
                sampler_samples.lock().unwrap().push(rss);
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
    let payload = method_payload(&args.method);
    let per = args.per;
    let ok_counts: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let mut clients = Vec::new();
    let started = Instant::now();
    for _ in 0..args.clients {
        let sock = sock.to_path_buf();
        let payload = payload.clone();
        let ok_counts = Arc::clone(&ok_counts);
        clients.push(std::thread::spawn(move || {
            let ok = one_client(&sock, &payload, per).unwrap_or(0);
            ok_counts.lock().unwrap().push(ok);
        }));
    }
    for client in clients {
        let _ = client.join();
    }
    let elapsed = started.elapsed();
    std::thread::sleep(Duration::from_secs(6));
    stop.store(true, Ordering::SeqCst);
    let _ = sampler.join();
    let reqs = args.clients * args.per;
    let ok: usize = ok_counts.lock().unwrap().iter().sum();
    let rps = reqs as f64 / elapsed.as_secs_f64();
    let samples = samples.lock().unwrap();
    let peak = samples.iter().cloned().fold(0.0_f64, f64::max);
    let idle = samples.last().cloned().unwrap_or(0.0);
    let verdict = if rps >= RPS_FLOOR && idle <= IDLE_RSS_CEILING_MB {
        "pass"
    } else {
        "SLOW_OR_FAT"
    };
    println!(
        "END method={} backend={} clients={} reqs={} ok={} secs={:.2} rps={:.2} peak_rss_mb={:.0} idle_rss_mb={:.0} {}",
        args.method,
        args.backend,
        args.clients,
        reqs,
        ok,
        elapsed.as_secs_f64(),
        rps,
        peak,
        idle,
        verdict,
    );
    if ok != reqs {
        eprintln!("keeper-read-load: only {ok} of {reqs} replies were ok");
        return 1;
    }
    if verdict != "pass" {
        return 1;
    }
    0
}

fn wait_for_socket(sock: &Path, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if UnixStream::connect(sock).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}
