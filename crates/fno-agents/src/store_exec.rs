//! The one-shot store lane: `--store-exec` serves ONE store request on
//! stdin/stdout and exits. A client that must not leave a resident process
//! behind still gets the full dispatch: a keeper's memory grows with the
//! requests it serves (measured 2026-09-17 at ~3.6 GB/hour with the graph
//! resident), so the leak-proof shape is one process per request. Callers
//! build this argv; humans never type it.

use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use crate::graph_keeper::{err_reply, handle_request, StoreState, MAX_FRAME_BYTES};

/// Parsed `--store-exec` lane argv: `--store-exec --graph <path>
/// [--canonical] [--events <path>] [--lock-timeout-secs N]`. No socket and
/// no session: the lane serves one request on stdin/stdout and exits.
#[derive(Debug)]
pub struct ExecConfig {
    pub graph: PathBuf,
    pub canonical: bool,
    pub lock_timeout: Duration,
    pub events: Option<PathBuf>,
}

pub fn parse_store_exec_args(args: &[String]) -> Result<ExecConfig, String> {
    let mut graph: Option<String> = None;
    let mut canonical = false;
    let mut lock_timeout = crate::graph_store::DEFAULT_LOCK_TIMEOUT;
    let mut events = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store-exec" => {}
            "--graph" => graph = Some(it.next().ok_or("--graph needs a value")?.clone()),
            "--canonical" => canonical = true,
            "--lock-timeout-secs" => {
                let v: u64 = it
                    .next()
                    .ok_or("--lock-timeout-secs needs a value")?
                    .parse()
                    .map_err(|_| "--lock-timeout-secs needs a number")?;
                lock_timeout = Duration::from_secs(v);
            }
            "--events" => events = Some(PathBuf::from(it.next().ok_or("--events needs a value")?)),
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    Ok(ExecConfig {
        graph: PathBuf::from(graph.ok_or("missing --graph")?),
        canonical,
        lock_timeout,
        events,
    })
}

/// The shared `StoreState` constructor: the resident `run()` and the
/// one-shot `run_exec()` build the same state, so an answer cannot depend on
/// which lane served it.
pub(crate) fn fresh_store_state(
    graph: PathBuf,
    canonical: bool,
    lock_timeout: Duration,
    events: Option<PathBuf>,
    sock_ino: Option<(u64, u64)>,
    startup_fp: Option<crate::drift::ExeFingerprint>,
) -> StoreState {
    StoreState {
        graph,
        canonical,
        lock_timeout,
        gate: std::sync::RwLock::new(()),
        inflight: std::sync::RwLock::new(()),
        cache: std::sync::RwLock::new(None),
        fill: std::sync::Mutex::new(()),
        file_opens: std::sync::atomic::AtomicU64::new(0),
        snapshots: std::sync::Mutex::new(std::collections::VecDeque::new()),
        write_ledger: std::sync::Mutex::new(std::collections::VecDeque::new()),
        gate_metrics: std::sync::Mutex::new(crate::graph_keeper::GateMetrics::new()),
        last_write: std::sync::Mutex::new(None),
        render_in_flight: std::sync::atomic::AtomicBool::new(false),
        render_failures: std::sync::atomic::AtomicU32::new(0),
        last_render_attempt: std::sync::Mutex::new(None),
        events,
        sock_ino,
        startup_fp,
    }
}

/// The serving half of the lane, split from stdin/stdout so it stays
/// testable without a process.
///
/// No render pass here, deliberately: the Python client already renders the
/// canonical views after a landed publish (`_finish_mutation`), so an exec
/// render would run the pass twice for every CLI write - and a synchronous
/// `fno backlog render-views` inside the child re-enters the store client,
/// which wedges under the test sandbox. The resident trigger's remaining
/// job (native mux writes rendering graph.md) rides with the resident
/// keeper itself.
fn exec_reply(cfg: &ExecConfig, payload: &[u8]) -> Value {
    if payload.len() > MAX_FRAME_BYTES {
        return err_reply(
            0,
            "malformed_frame",
            format!("request of {} bytes exceeds the cap", payload.len()),
        );
    }
    let state = fresh_store_state(
        cfg.graph.clone(),
        cfg.canonical,
        cfg.lock_timeout,
        cfg.events.clone(),
        None,
        None,
    );
    handle_request(&state, payload)
}

/// `--store-exec` lifecycle: read ONE request JSON (the same
/// `{"id","method","params"}` envelope the framed clients send) from stdin,
/// serve it through `handle_request` on a fresh state, print the reply
/// envelope on stdout, exit. Exit 0 on an ok reply, 1 otherwise; the reply
/// is the completion record, so a lost reply means the process died - the
/// same terminal state the socket path's `write_status` resolution reaches.
pub fn run_exec(cfg: ExecConfig) -> Result<(), String> {
    use std::io::Read as _;

    let mut payload = Vec::new();
    std::io::stdin()
        .read_to_end(&mut payload)
        .map_err(|e| format!("cannot read request from stdin: {e}"))?;
    let reply = exec_reply(&cfg, &payload);
    let ok = reply.get("ok").and_then(Value::as_bool) == Some(true);
    println!("{reply}");
    if ok {
        Ok(())
    } else {
        Err("store request failed (see reply on stdout)".into())
    }
}
