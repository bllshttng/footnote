//! `fno-agents-worker` entrypoint (Wave 3, Outcome B). Argv parsing ->
//! `worker::run`.
//!
//! Usage (the daemon builds this argv; humans never type it):
//! ```text
//! fno-agents-worker --short-id wkA --home /path/.fno/agents \
//!     --cwd /work [--rows 24] [--cols 80] [--ring-bytes N] -- <provider argv...>
//! ```
//!
//! The worker ignores SIGHUP so a stray hangup (e.g. the controlling terminal
//! going away) cannot take it — and therefore the PTY child — down. Outlasting
//! the daemon is the whole point.

use fno_agents::pty::DEFAULT_OUTPUT_RING_BYTES;
use fno_agents::stream_worker::{self, SessionClaim, StreamWorkerConfig};
use fno_agents::worker::{run, WorkerConfig};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Defensively ignore SIGHUP (both lanes): the worker must outlive the daemon
    // and any terminal disconnect (the child's survival depends on this process
    // living). SAFETY: SIG_IGN for SIGHUP is a standard, async-signal-safe call.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    // The stream-json lane (claude adoption) is selected by `--stream`; the
    // default lane is the PTY worker (codex/gemini).
    if args.iter().any(|a| a == "--stream") {
        run_stream_lane(args);
        return;
    }

    let cfg = match parse_args(args) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("fno-agents-worker: {msg}");
            std::process::exit(2);
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno-agents-worker: cannot build runtime: {e}");
            std::process::exit(1);
        }
    };

    match rt.block_on(run(cfg)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("fno-agents-worker: {e}");
            std::process::exit(1);
        }
    }
}

/// `fno-agents-worker --stream` entrypoint: the claude stream-json host lane.
/// Usage (the daemon builds this argv; humans never type it):
/// ```text
/// fno-agents-worker --stream --short-id sw1 --home /path/.fno/agents \
///     --cwd /work [--session-uuid <uuid>] [--holder <claim-holder>] \
///     -- claude -p --resume <uuid> --input-format stream-json ...
/// ```
fn run_stream_lane(args: Vec<String>) {
    let cfg = match parse_stream_args(args) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("fno-agents-worker: {msg}");
            std::process::exit(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno-agents-worker: cannot build runtime: {e}");
            std::process::exit(1);
        }
    };
    match rt.block_on(stream_worker::run(cfg)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("fno-agents-worker: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_stream_args(args: Vec<String>) -> Result<StreamWorkerConfig, String> {
    let mut short_id = None;
    let mut home = None;
    let mut cwd = None;
    let mut session_uuid = None;
    let mut holder = None;
    let mut argv: Vec<String> = Vec::new();

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--stream" => {} // mode selector, consumed in main
            "--short-id" => short_id = it.next(),
            "--home" => home = it.next().map(PathBuf::from),
            "--cwd" => cwd = it.next().map(PathBuf::from),
            "--session-uuid" => session_uuid = it.next(),
            "--holder" => holder = it.next(),
            "--" => {
                argv.extend(it.by_ref());
                break;
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }

    let short_id = short_id.ok_or("missing --short-id")?;
    let home = home.ok_or("missing --home")?;
    let cwd = cwd.unwrap_or_else(std::env::temp_dir);
    if argv.is_empty() {
        return Err("missing provider argv after `--`".into());
    }

    // The claim uuid + holder travel as a pair: one without the other is
    // meaningless (and would silently skip the orphan-time release).
    let session_claim = match (session_uuid, holder) {
        (Some(u), Some(h)) => Some(SessionClaim {
            session_uuid: u,
            claim_holder: h,
        }),
        (None, None) => None,
        _ => return Err("--session-uuid and --holder must be given together".into()),
    };

    let mut cfg = StreamWorkerConfig::new(short_id, home, cwd, argv);
    cfg.session_claim = session_claim;
    Ok(cfg)
}

fn parse_args(args: Vec<String>) -> Result<WorkerConfig, String> {
    let mut short_id = None;
    let mut home = None;
    let mut cwd = None;
    let mut rows = 24u16;
    let mut cols = 80u16;
    let mut ring_bytes = DEFAULT_OUTPUT_RING_BYTES;
    let mut argv: Vec<String> = Vec::new();

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--short-id" => short_id = it.next(),
            "--home" => home = it.next().map(PathBuf::from),
            "--cwd" => cwd = it.next().map(PathBuf::from),
            "--rows" => {
                rows = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--rows needs a number")?
            }
            "--cols" => {
                cols = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--cols needs a number")?
            }
            "--ring-bytes" => {
                ring_bytes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--ring-bytes needs a number")?
            }
            "--" => {
                argv.extend(it.by_ref());
                break;
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }

    let short_id = short_id.ok_or("missing --short-id")?;
    let home = home.ok_or("missing --home")?;
    let cwd = cwd.unwrap_or_else(std::env::temp_dir);
    if argv.is_empty() {
        return Err("missing provider argv after `--`".into());
    }

    let mut cfg = WorkerConfig::new(short_id, home, cwd, argv);
    cfg.rows = rows;
    cfg.cols = cols;
    cfg.ring_bytes = ring_bytes;
    Ok(cfg)
}
