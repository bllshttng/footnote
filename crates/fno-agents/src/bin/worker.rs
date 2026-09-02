//! `fno-agents-worker` entrypoint. Three lanes:
//!
//! - `--keeper`: the keeper. Owns a hosted child's pty master in its own
//!   process (setsid'd, SIGHUP-immune), so the child outlives whatever
//!   launched it and a fresh mux server re-adopts a pane instead of
//!   re-spawning it. `--pane` is the alias the mux server's call sites spell
//!   it by; a pane-less lane-B thread is the same keeper with no pane behind
//!   it. This is the lane retired at G4 and restored once the crashes it
//!   predicted kept biting: the deleted worker.rs module doc named the undo
//!   ("supervisor fd-keeper"). Callers build this argv; humans never type it.
//! - `--stream`: the claude stream-json adoption lane.
//! - `--store-keeper`: the graph store keeper (graph_keeper.rs). Owns one
//!   graph file, serves reads and locked mutations over its own socket, and
//!   outlives the daemon the same way the pane keeper does. Callers build
//!   this argv; humans never type it.
//!
//! The worker ignores SIGHUP so a stray hangup (e.g. the controlling
//! terminal going away) cannot take it - and therefore the PTY child -
//! down. Outlasting the daemon is the whole point.

use fno_agents::stream_worker::{self, SessionClaim, StreamWorkerConfig};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Defensively ignore SIGHUP (both lanes): the worker must outlive the daemon
    // and any terminal disconnect (the child's survival depends on this process
    // living). SAFETY: SIG_IGN for SIGHUP is a standard, async-signal-safe call.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    // `version [--json]`: report the baked-in build rev so `fno doctor update` can
    // verify this bin is the SAME build as its triad siblings. Checked BEFORE
    // the --stream gate so `version` is not rejected as "not --stream".
    if matches!(
        args.first().map(String::as_str),
        Some("version" | "-V" | "--version")
    ) {
        fno_agents::version::print_version(args.iter().any(|a| a == "--json"));
        return;
    }

    // Three lanes: `--keeper` (the keeper: pty master ownership outlives the
    // mux server; `--pane` is the alias its call sites spell it by),
    // `--stream` (claude stream-json adoption, launched by the daemon's
    // spawn_claude_stream_lane), and `--store-keeper` (the graph store).
    // Everything else refuses, truthfully.
    if args.iter().any(|a| a == "--store-keeper") {
        if let Err(msg) = store_keeper_lane(&args) {
            eprintln!("fno-agents-worker: {msg}");
            std::process::exit(2);
        }
        return;
    }
    if args.iter().any(|a| a == "--keeper" || a == "--pane") {
        if let Err(msg) = pane_keeper_lane(&args) {
            eprintln!("fno-agents-worker: {msg}");
            std::process::exit(2);
        }
        return;
    }
    if !args.iter().any(|a| a == "--stream") {
        eprintln!(
            "fno-agents-worker: pass a lane: --keeper (alias --pane), --stream \
             (claude stream-json adoption), or --store-keeper (graph store)"
        );
        std::process::exit(2);
    }
    run_stream_lane(args);
}

/// `--store-keeper` entrypoint: parse, then run the store keeper to
/// completion. A normal lifecycle (Shutdown frame) ends inside with exit(0).
fn store_keeper_lane(args: &[String]) -> Result<(), String> {
    let cfg = fno_agents::graph_keeper::parse_store_keeper_args(args)?;
    fno_agents::graph_keeper::run(cfg)
}

/// `--keeper` / `--pane` entrypoint: parse, then run the keeper to completion.
fn pane_keeper_lane(args: &[String]) -> Result<(), String> {
    let cfg = fno_agents::pane_keeper::parse_pane_args(args)?;
    fno_agents::pane_keeper::run(cfg)
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
