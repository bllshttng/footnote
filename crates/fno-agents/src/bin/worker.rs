//! `fno-agents-worker` entrypoint. Post-G4 (x-f54c) only the `--stream`
//! claude stream-json adoption lane survives; the PTY worker lane retired.
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

    // `version [--json]`: report the baked-in build rev so `fno update` can
    // verify this bin is the SAME build as its triad siblings. Checked BEFORE
    // the --stream gate so `version` is not rejected as "not --stream".
    if matches!(
        args.first().map(String::as_str),
        Some("version" | "-V" | "--version")
    ) {
        fno_agents::version::print_version(args.iter().any(|a| a == "--json"));
        return;
    }

    // Post-G4 (x-f54c): the PTY worker lane retired with daemon PTY hosting. The
    // only surviving lane is `--stream` (claude stream-json adoption), launched
    // by the daemon's spawn_claude_stream_lane.
    if !args.iter().any(|a| a == "--stream") {
        eprintln!(
            "fno-agents-worker: the PTY worker lane was retired at G4; only --stream \
             (claude stream-json adoption) remains"
        );
        std::process::exit(2);
    }
    run_stream_lane(args);
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
