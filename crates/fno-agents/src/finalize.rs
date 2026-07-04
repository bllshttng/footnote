//! `fno-agents finalize` (control-plane step 6, ab-f8e5f214): the terminal-only
//! WRITER the stop-hook shim invokes on a terminal-allow `loop-check` decision.
//!
//! It re-homes the mechanical session side-effects out of the skill's
//! pre-promise bash so they fire in EVERY mode (attended, autonomous, megawalk
//! worker) and survive context compaction:
//!
//! - **Always** (any terminal reason): one ledger session-record, carrying
//!   `graph_node_id` + `provider_id` + scalar `session_id` + `cost_usd` + a new
//!   `termination_reason`, so a node's true cost and full session list roll up
//!   by grouping ledger entries on `graph_node_id` (US7).
//! - **Ship only** (`DonePRGreen` / `DoneAdvisory`): plan stamp + graduate and a
//!   mechanical git-derived handoff artifact.
//!
//! ## Why this does not break the read-only stop hook
//!
//! `loop-check` stays a pure read-only DECISION verb; `finalize` is a separate
//! WRITER the shim runs AFTER the allow decision. Nothing `finalize` writes is
//! read by a future `loop-check` decision as a gate:
//!
//! - `loop-check`'s budget axis reads ledger `cost_usd` filtered by THIS
//!   session's `session_id`. `finalize`'s terminal ledger row for the same
//!   session can only push a re-fire toward termination (a higher cost trips
//!   `Budget`, which is itself terminal-allow), never away from it - and a
//!   re-fire early-returns on the `session_finalized` event anyway.
//! - A DIFFERENT session's `loop-check` filters by its own `session_id`, so it
//!   never reads this session's finalize row.
//!
//! ## Non-fatal + idempotent
//!
//! Every sub-step is independently non-fatal: a failure logs to stderr and is
//! recorded in a `session_finalize_failed` event, but never changes the exit
//! code (the promise is honored regardless - side-effects never block).
//! `session_finalized` is emitted ONLY when every attempted sub-step succeeded;
//! a partial failure leaves it unemitted so a later stop-hook fire retries the
//! remaining work (each shelled script is itself idempotent: ledger flock +
//! scalar-session-id dedup, first-writer-wins stamp, filename-keyed handoff).
//!
//! The proven Python helpers (`fno.cost._session_cost`, `fno.cost._register`,
//! `fno.plan._stamp`, all in-package modules run via `python3 -m`) do the
//! cost/dedup/flock/stamp work; this verb is a thin orchestrator (Locked
//! Decision 6 - avoids the Python->Rust byte-parity trap), so the shim keeps
//! its Rust-only dependency surface (Domain Pitfall).

use crate::loopcheck::{emit_to_both, now_rfc3339_utc};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Terminal reasons that ran an actual ship (PR landed or advisory-complete).
/// Only these run the completion side-effects (stamp/graduate/handoff); every
/// terminal reason runs the ledger record.
const SHIP_REASONS: &[&str] = &["DonePRGreen", "DoneAdvisory"];

/// Terminal reasons that signal a STUCK session: the loop-check verb saw no
/// forward progress, or the budget cap tripped, and let the session exit
/// without shipping. These get a postmortem artifact the autocorrect monthly
/// review consumes via `~/.fno/corrections.log` (ab-1a92b677: re-homed here
/// after the control-plane wedge dropped the old stop-hook generator; moved
/// again from ~/.claude/ to ~/.fno/ per the placement rule, ab-f063 Wave 2).
/// A ship or a benign NoWork/Interrupted/Aborted terminal is not "stuck".
const POSTMORTEM_REASONS: &[&str] = &["NoProgress", "Budget"];

// ── arg parsing ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct FinalizeArgs {
    state: Option<PathBuf>,
    transcript: Option<PathBuf>,
    cwd: Option<PathBuf>,
    reason: Option<String>,
    // Overrides (primarily for tests / non-default layouts).
    events: Option<PathBuf>,
    global_events: Option<PathBuf>,
    settings: Option<PathBuf>,
    handoffs_dir: Option<PathBuf>,
    postmortems_dir: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<FinalizeArgs, String> {
    let mut a = FinalizeArgs::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let take = |it: &mut std::slice::Iter<String>| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--state" => a.state = Some(PathBuf::from(take(&mut it)?)),
            "--transcript" => a.transcript = Some(PathBuf::from(take(&mut it)?)),
            "--cwd" => a.cwd = Some(PathBuf::from(take(&mut it)?)),
            "--reason" => a.reason = Some(take(&mut it)?),
            "--events" => a.events = Some(PathBuf::from(take(&mut it)?)),
            "--global-events" => a.global_events = Some(PathBuf::from(take(&mut it)?)),
            "--settings" => a.settings = Some(PathBuf::from(take(&mut it)?)),
            "--handoffs-dir" => a.handoffs_dir = Some(PathBuf::from(take(&mut it)?)),
            "--postmortems-dir" => a.postmortems_dir = Some(PathBuf::from(take(&mut it)?)),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(a)
}

const HELP: &str = "fno-agents finalize - terminal-only side-effect writer (step 6)\n\
Usage: fno-agents finalize --state <target-state.md> --cwd <project-root> --reason <TerminationReason> \\\n\
                           [--transcript <transcript.jsonl>] [--events <p>] [--global-events <p>] \\\n\
                           [--settings <p>] [--handoffs-dir <p>] [--postmortems-dir <p>]\n\
Reason values: DonePRGreen|DoneAdvisory|NoWork|Budget|NoProgress|Interrupted|Aborted";

// ── manifest fields finalize reads directly ────────────────────────────────

/// The three manifest fields finalize needs itself (everything else is read by
/// the shelled Python helpers from the same manifest path).
#[derive(Debug, Default)]
struct ManifestFields {
    /// Target-minted session id: idempotency key, handoff filename, event data.
    session_id: Option<String>,
    /// Claude transcript UUID: positional arg to fno.cost._session_cost / _register.
    claude_transcript_id: Option<String>,
    /// Plan to stamp/graduate (ship branch only). Empty/absent -> skip.
    plan_path: Option<String>,
    /// Feature title for the handoff header.
    input: Option<String>,
    /// Backlog node id (lives in the manifest BODY, below the frontmatter).
    graph_node_id: Option<String>,
    /// Cross-project plan: graduation must wait for ALL project PRs, so the
    /// expected URL count is derived from the plan's `projects:` map, never 1.
    cross_project: bool,
}

/// Scan the WHOLE manifest (frontmatter AND body) for the keys we need.
/// `graph_node_id`/`target_claim_*` live below the closing `---`, so a
/// frontmatter-only parse (like loop-check's) would miss them.
fn parse_manifest_fields(content: &str) -> ManifestFields {
    let mut m = ManifestFields::default();
    for line in content.lines() {
        let line = line.trim();
        // Skip markdown headings and frontmatter fences; a `key: value` match
        // below is all we want.
        if line.is_empty() || line.starts_with('#') || line == "---" {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
        // First non-empty wins (frontmatter precedes body); never overwrite a
        // real value with a later blank.
        let set = |slot: &mut Option<String>, val: &str| {
            if slot.is_none() && !val.is_empty() && val != "null" {
                *slot = Some(val.to_string());
            }
        };
        match k {
            "session_id" => set(&mut m.session_id, v),
            // Current key is claude_session_id; accept the pre-rename
            // claude_transcript_id as a fallback for one release. `set` keeps the
            // first non-empty value, so the current key (written first) wins.
            "claude_session_id" | "claude_transcript_id" => set(&mut m.claude_transcript_id, v),
            "plan_path" => set(&mut m.plan_path, v),
            "input" => set(&mut m.input, v),
            "graph_node_id" => set(&mut m.graph_node_id, v),
            "cross_project" => m.cross_project = v == "true",
            _ => {}
        }
    }
    m
}

// ── idempotency ─────────────────────────────────────────────────────────────

/// Inspect prior `session_finalized` events for this session_id.
///
/// Returns:
/// - `Some(true)`  - a prior finalize completed a SHIP (stamp/graduate/handoff
///   ran); the session is fully done, nothing more to do on any later fire.
/// - `Some(false)` - a prior finalize completed but only the non-ship ledger
///   record (the always-branch); the ledger row exists, but the ship
///   side-effects have NOT run.
/// - `None`        - no completed finalize yet (or only `session_finalize_failed`,
///   which is intentionally not counted so a later fire retries).
///
/// A successful finalize is recorded ONLY when every attempted sub-step
/// succeeded, so a partially-failed prior run leaves this `None` and the next
/// fire retries. The `ship` flag distinguishes a non-ship terminal (Budget /
/// NoProgress / ...) from a real ship, so a session that terminated non-ship
/// first and then ships within the same session still runs its ship
/// side-effects on the ship fire (the lockout bug, sigma-review HIGH).
fn prior_finalize_ship(project_events: &Path, session_id: &str) -> Option<bool> {
    let content = fs::read_to_string(project_events).ok()?;
    let mut seen = None;
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("session_finalized")
            || val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id)
        {
            continue;
        }
        let ship = val
            .pointer("/data/ship")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if ship {
            return Some(true); // a completed ship is terminal-complete
        }
        seen = Some(false);
    }
    seen
}

// ── public entry ────────────────────────────────────────────────────────────

/// `fno-agents finalize ...`. Returns a process exit code: 0 for the normal
/// (always-non-fatal) path, 2 only for CLI misuse. Side-effect failures never
/// raise the exit code - the promise is honored regardless.
pub fn run_finalize(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|a| a == "-h" || a == "--help" || a == "help")
    {
        println!("{HELP}");
        return 0;
    }
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("finalize: {msg}\n{HELP}");
            return 2;
        }
    };
    let (Some(state), Some(cwd), Some(reason)) = (a.state, a.cwd, a.reason) else {
        eprintln!("finalize: --state, --cwd and --reason are required\n{HELP}");
        return 2;
    };

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let project_events = a.events.unwrap_or_else(|| cwd.join(".fno/events.jsonl"));
    let global_events = a.global_events.unwrap_or_else(|| {
        home.clone()
            .unwrap_or_else(|| cwd.clone())
            .join(".fno/events.jsonl")
    });

    // Read the manifest. A missing/unreadable manifest is the delegated-session
    // path: handoff.sh archived it AND already wrote that session's ledger
    // record before archival, so there is nothing to finalize here. Non-fatal.
    let content = match fs::read_to_string(&state) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "finalize: manifest {} unreadable ({e}); nothing to finalize (likely delegated/archived)",
                state.display()
            );
            return 0;
        }
    };
    let m = parse_manifest_fields(&content);
    let Some(session_id) = m.session_id.clone() else {
        eprintln!("finalize: manifest has no session_id; skipping (cannot dedup)");
        return 0;
    };

    let ship = SHIP_REASONS.contains(&reason.as_str());

    // Idempotency, ship-aware (sigma-review HIGH): a prior COMPLETED ship means
    // the whole session is done. A prior non-ship finalize means only the ledger
    // row exists; if THIS fire is also non-ship there is nothing new to do, but
    // if THIS fire is a SHIP it must still run the ship side-effects - a session
    // that hit a non-ship terminal (Budget / NoProgress) and then shipped within
    // the same session would otherwise never get stamped/graduated/handed off.
    let mut skip_ledger = false;
    match prior_finalize_ship(&project_events, &session_id) {
        Some(true) => {
            eprintln!("finalize: session {session_id} already finalized (ship); early-return");
            return 0;
        }
        Some(false) if !ship => {
            eprintln!(
                "finalize: session {session_id} ledger already recorded (non-ship); early-return"
            );
            return 0;
        }
        Some(false) => {
            // Ledger row already written by the prior non-ship finalize; skip the
            // redundant ledger step (register-task would dedup it anyway) and run
            // only the ship side-effects below.
            skip_ledger = true;
        }
        None => {}
    }

    // Transcript UUID for the cost/ledger scripts: prefer the manifest's
    // canonical claude_transcript_id, fall back to the --transcript basename.
    let transcript_uuid = m
        .claude_transcript_id
        .clone()
        .or_else(|| {
            a.transcript
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    let mut failed: Vec<String> = Vec::new();

    // ── ALWAYS: ledger session-record (skipped only when a prior non-ship
    //    finalize already wrote this session's row) ──────────────────────────
    let ledger_written = if skip_ledger {
        true // the prior non-ship finalize already wrote the row
    } else {
        match write_ledger_record(&cwd, &state, &transcript_uuid, &reason) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("finalize: ledger record failed: {e}");
                failed.push("ledger".into());
                false
            }
        }
    };

    // ── SHIP ONLY: stamp/graduate + handoff ────────────────────────────────
    let mut stamped = false;
    let mut handoff_path: Option<String> = None;
    if ship {
        let plan = m.plan_path.clone().unwrap_or_default();
        if !plan.is_empty() {
            // Cross-project plans must graduate only after ALL project PRs land;
            // derive the count from the plan's projects map and never hard-code 1.
            let expected = derive_expected_url_count(&cwd, &plan, m.cross_project);
            let do_graduate = !m.cross_project || expected.is_some();
            match stamp_and_graduate(&cwd, &plan, &session_id, expected, do_graduate) {
                Ok(()) => stamped = true,
                Err(step) => {
                    eprintln!("finalize: {step} failed");
                    failed.push(step);
                }
            }
        }
        match write_handoff(
            &cwd,
            &state,
            &session_id,
            &m,
            &transcript_uuid,
            a.handoffs_dir.as_deref(),
            a.settings.as_deref(),
            home.as_deref(),
        ) {
            Ok(p) => handoff_path = Some(p),
            Err(e) => {
                eprintln!("finalize: handoff failed: {e}");
                failed.push("handoff".into());
            }
        }
    }

    // ── STUCK ONLY: postmortem artifact (ab-1a92b677) ──────────────────────
    // A NoProgress/Budget terminal means the session gave up without shipping.
    // Re-home the BLOCKED-postmortem generator the wedge dropped when the stop
    // hook became a thin shim: write a structured artifact + a corrections.log
    // pointer so the autocorrect monthly review can mechanically consume what
    // went wrong. Non-fatal and idempotent (filename keyed by date+session) like
    // every other sub-step. A ship reason is never in POSTMORTEM_REASONS, so a
    // session that hit Budget/NoProgress and later shipped writes the postmortem
    // exactly once (on the stuck fire), never on the ship fire.
    let mut postmortem_path: Option<String> = None;
    if POSTMORTEM_REASONS.contains(&reason.as_str()) {
        match write_postmortem(
            &cwd,
            &session_id,
            &m,
            &reason,
            a.transcript.as_deref(),
            a.postmortems_dir.as_deref(),
            a.settings.as_deref(),
            home.as_deref(),
        ) {
            Ok(p) => postmortem_path = Some(p),
            Err(e) => {
                eprintln!("finalize: postmortem failed: {e}");
                failed.push("postmortem".into());
            }
        }
    }

    // ── emit terminal event ────────────────────────────────────────────────
    let mut data = json!({
        "session_id": session_id,
        "termination_reason": reason,
        "ship": ship,
        "ledger_written": ledger_written,
        "stamped": stamped,
        "handoff_path": handoff_path,
        "postmortem_path": postmortem_path,
        "graph_node_id": m.graph_node_id,
    });
    if failed.is_empty() {
        emit_to_both(&project_events, &global_events, "session_finalized", data);
    } else {
        data["failed_steps"] = json!(failed);
        // session_finalized intentionally NOT emitted: a later fire retries the
        // failed step (each shelled helper is idempotent).
        emit_to_both(
            &project_events,
            &global_events,
            "session_finalize_failed",
            data,
        );
    }
    0
}

// ── ledger (always) ─────────────────────────────────────────────────────────

/// Build a `python3` command (for `-m <module>`) rooted at `cwd`, injecting the
/// repo's `cli/src` onto PYTHONPATH when running from a source checkout so the
/// in-package `fno.*` modules import without an installed/editable package
/// (codex PR #515 P1). When the stop hook resolves the checkout-built binary,
/// these children otherwise run with only `cwd` on `sys.path`, where `fno` is
/// not importable, so every terminal finalize silently failed to write the
/// ledger / stamp the plan. In an installed environment `cli/src` is not found
/// relative to the binary, PYTHONPATH is left untouched, and the installed
/// `fno` package is used.
fn py_module(cwd: &Path) -> Command {
    let mut cmd = Command::new(py_interpreter());
    cmd.current_dir(cwd);
    if let Some(src) = repo_cli_src() {
        let joined = match std::env::var_os("PYTHONPATH") {
            Some(prev) if !prev.is_empty() => {
                // APPEND (not prepend): cli/src is only a fallback that resolves
                // `fno` when nothing else does (the codex P1 source-checkout
                // case). An existing PYTHONPATH - a deliberate override, or the
                // finalize_e2e stub package - must keep precedence, so we add
                // cli/src AFTER it rather than shadowing it.
                let mut s = prev;
                s.push(":");
                s.push(&src);
                s
            }
            _ => std::ffi::OsString::from(&src),
        };
        cmd.env("PYTHONPATH", joined);
    }
    cmd
}

/// Locate `<repo>/cli/src` by walking up from the running binary until an
/// ancestor holds `cli/src/fno/__init__.py`. Returns `None` when the binary is
/// not inside a source checkout (e.g. an installed wheel), so PYTHONPATH stays
/// unset and the installed package is used instead.
fn repo_cli_src() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    for anc in exe.ancestors() {
        if anc.join("cli/src/fno/__init__.py").is_file() {
            return Some(anc.join("cli/src").to_string_lossy().into_owned());
        }
    }
    None
}

/// The interpreter finalize's Python helpers run under. In a source checkout,
/// prefer the repo's `cli/.venv` python: bare `python3` on PATH (e.g. Homebrew's
/// `/opt/homebrew/opt/python@3.x`) resolves the `fno` package off PYTHONPATH but
/// lacks fno's third-party deps (pydantic, ...), so `import fno.config` raised
/// ModuleNotFoundError and every terminal finalize logged `ledger record failed`
/// / `stamp failed` and wrote no termination_reason row. The venv has both fno
/// and its deps. PYTHONPATH entries still precede site-packages, so the
/// finalize_e2e stub package keeps precedence over the venv's installed `fno`.
/// Falls back to `python3` when no venv is found (installed-wheel or bare
/// environment).
fn py_interpreter() -> String {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return "python3".to_string(),
    };
    for anc in exe.ancestors() {
        let venv = anc.join("cli/.venv/bin/python3");
        if venv.is_file() {
            return venv.to_string_lossy().into_owned();
        }
    }
    "python3".to_string()
}

/// Run `python3 -m fno.cost._session_cost` for cost, then
/// `python3 -m fno.cost._register` to append exactly one ledger row carrying
/// graph_node_id + provider_id + session_id + cost + termination_reason.
/// Dedup/flock stay in _register (proven). A missing transcript yields
/// cost=null - the row still lands (US7-ERR).
fn write_ledger_record(
    cwd: &Path,
    state: &Path,
    transcript_uuid: &str,
    reason: &str,
) -> Result<(), String> {
    // Cost JSON (best-effort: empty string -> register-task records cost=null).
    let cost_json = if transcript_uuid.is_empty() {
        String::new()
    } else {
        match py_module(cwd)
            .arg("-m")
            .arg("fno.cost._session_cost")
            .arg("--json")
            .arg(transcript_uuid)
            .output()
        {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            Ok(out) => {
                eprintln!(
                    "finalize: fno.cost._session_cost exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                String::new()
            }
            Err(e) => {
                eprintln!("finalize: fno.cost._session_cost spawn failed: {e}");
                String::new()
            }
        }
    };

    let mut cmd = py_module(cwd);
    cmd.arg("-m")
        .arg("fno.cost._register")
        .arg(state)
        .arg(transcript_uuid)
        .arg("--termination-reason")
        .arg(reason);
    if !cost_json.is_empty() {
        cmd.arg("--cost-json").arg(&cost_json);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("fno.cost._register spawn failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "fno.cost._register exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ── stamp + graduate (ship only) ─────────────────────────────────────────────

/// Run `python3 -m fno.plan._stamp` stamp then (optionally) graduate. Both are
/// idempotent/conditional (no-op on empty plan_path or already-done). Returns
/// Err("stamp"|"graduate") naming the first failing step.
///
/// `expected_url_count` is passed to `stamp` ONLY when known (cross-project
/// plans, derived from the `projects:` map). For single-project plans it is
/// None: the stamp module keeps any count the plan already declares (first-
/// writer-wins; e.g. a decomposed epic's `set-expected`) and otherwise graduate
/// defaults to 1. We never hard-code 1, which would prematurely graduate a
/// multi-repo plan after its first PR (codex P1).
///
/// `do_graduate` is false only when a cross-project plan's count could not be
/// derived: stamp the URL but skip graduate, so the plan can never graduate
/// before all project PRs land (conservative; graduation then happens on a
/// later fire or via reconcile).
fn stamp_and_graduate(
    cwd: &Path,
    plan_path: &str,
    session_id: &str,
    expected_url_count: Option<u32>,
    do_graduate: bool,
) -> Result<(), String> {
    let pr_url = gh_pr_url(cwd);
    let mut stamp = py_module(cwd);
    stamp
        .arg("-m")
        .arg("fno.plan._stamp")
        .arg("stamp")
        .arg("--plan-path")
        .arg(plan_path)
        .arg("--session-id")
        .arg(session_id);
    if let Some(n) = expected_url_count {
        stamp.arg("--expected-url-count").arg(n.to_string());
    }
    if let Some(url) = &pr_url {
        stamp.arg("--url").arg(url);
    }
    let out = stamp.output().map_err(|_| "stamp".to_string())?;
    if !out.status.success() {
        eprintln!(
            "finalize: fno.plan._stamp stamp exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Err("stamp".into());
    }

    if !do_graduate {
        eprintln!(
            "finalize: cross-project plan with no derivable expected_url_count; stamped but skipping graduate (avoids premature graduation)"
        );
        return Ok(());
    }

    let out = py_module(cwd)
        .arg("-m")
        .arg("fno.plan._stamp")
        .arg("graduate")
        .arg("--plan-path")
        .arg(plan_path)
        .output()
        .map_err(|_| "graduate".to_string())?;
    if !out.status.success() {
        eprintln!(
            "finalize: fno.plan._stamp graduate exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Err("graduate".into());
    }
    Ok(())
}

/// Derive the expected URL count for graduation. Returns `None` for a
/// single-project plan (let fno.plan._stamp keep any declared count, else
/// default to 1) and `Some(n)` for a cross-project plan, counting the direct keys under
/// the plan's frontmatter `projects:` map. Returns `None` for a cross-project
/// plan whose count can't be read (missing/garbled projects map) so the caller
/// can skip graduate rather than guess. This restores the pre-promise contract:
/// cross-project graduation waits for ALL project PRs (codex P1).
fn derive_expected_url_count(cwd: &Path, plan_path: &str, cross_project: bool) -> Option<u32> {
    if !cross_project {
        return None;
    }
    // A folder plan stores the projects map in 00-INDEX.md; a file plan in itself.
    let base = cwd.join(plan_path);
    let doc = if base.is_dir() {
        base.join("00-INDEX.md")
    } else {
        base
    };
    let content = fs::read_to_string(&doc).ok()?;

    let mut in_fm = false;
    let mut in_projects = false;
    let mut child_indent: Option<usize> = None;
    let mut count: u32 = 0;
    for line in content.lines() {
        let t = line.trim();
        if t == "---" {
            if !in_fm {
                in_fm = true;
                continue;
            }
            break; // end of frontmatter
        }
        if !in_fm {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if !in_projects {
            if indent == 0 && t.starts_with("projects:") {
                in_projects = true;
            }
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if indent == 0 {
            break; // next top-level frontmatter key ends the projects map
        }
        match child_indent {
            None => {
                child_indent = Some(indent);
                count += 1;
            }
            Some(ci) if indent == ci => count += 1,
            _ => {} // deeper-nested key under a project entry; not a project
        }
    }
    if count >= 1 {
        Some(count)
    } else {
        None
    }
}

// ── mechanical handoff artifact (ship only) ──────────────────────────────────

/// Write a git-derived end-of-session summary to the persistent handoffs dir.
/// Filename keyed by session-id so a re-run overwrites rather than duplicating.
#[allow(clippy::too_many_arguments)]
fn write_handoff(
    cwd: &Path,
    state: &Path,
    session_id: &str,
    m: &ManifestFields,
    transcript_uuid: &str,
    handoffs_override: Option<&Path>,
    settings_override: Option<&Path>,
    home: Option<&Path>,
) -> Result<String, String> {
    let dir = resolve_handoffs_dir(handoffs_override, settings_override, cwd, home);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let date = &now_rfc3339_utc()[..10]; // YYYY-MM-DD
    let sid_prefix: String = session_id.chars().take(16).collect();
    let file = dir.join(format!("{date}-{sid_prefix}-handoff.md"));

    let title = m.input.clone().unwrap_or_else(|| "Untitled".into());
    let plan = m.plan_path.clone().unwrap_or_else(|| "-".into());
    let node = m.graph_node_id.clone().unwrap_or_else(|| "-".into());
    let pr = gh_pr_url(cwd).unwrap_or_else(|| "-".into());
    let diffstat = git_capture(cwd, &["diff", "--stat", "origin/main...HEAD"])
        .filter(|s| !s.trim().is_empty())
        .or_else(|| git_capture(cwd, &["diff", "--stat", "HEAD~5..HEAD"]))
        .unwrap_or_else(|| "(diff unavailable)".into());
    let commits = git_capture(cwd, &["log", "--oneline", "origin/main..HEAD"])
        .filter(|s| !s.trim().is_empty())
        .or_else(|| git_capture(cwd, &["log", "--oneline", "-10"]))
        .unwrap_or_else(|| "(log unavailable)".into());
    let cost = handoff_cost_line(cwd, transcript_uuid);

    let body = format!(
        "# Session handoff: {title}\n\n\
         - session: `{session_id}`\n\
         - node: `{node}`\n\
         - plan: `{plan}`\n\
         - PR: {pr}\n\
         - cost: {cost}\n\
         - generated: {generated} (mechanical, by `fno-agents finalize`)\n\n\
         ## Files changed (origin/main...HEAD)\n\n```\n{diffstat}\n```\n\n\
         ## Commits\n\n```\n{commits}\n```\n",
        generated = now_rfc3339_utc(),
    );

    // Keep the manifest path referenced so the variable is meaningful even when
    // we add fields later; it is the canonical source of the fields above.
    let _ = state;
    fs::write(&file, body).map_err(|e| format!("write {}: {e}", file.display()))?;
    Ok(file.to_string_lossy().into_owned())
}

/// One-line cost summary for the handoff header, sourced from the in-package
/// _session_cost module (`python3 -m fno.cost._session_cost`).
fn handoff_cost_line(cwd: &Path, transcript_uuid: &str) -> String {
    if transcript_uuid.is_empty() {
        return "(unavailable)".into();
    }
    // Route through py_module so this shares the interpreter + PYTHONPATH
    // resolution used by the ledger write; a raw `python3` here (no PYTHONPATH,
    // no venv) was the source of the recurring `handoff cost: ... exit` errors.
    match py_module(cwd)
        .arg("-m")
        .arg("fno.cost._session_cost")
        .arg("--json")
        .arg(transcript_uuid)
        .output()
    {
        Ok(out) if out.status.success() => {
            match serde_json::from_slice::<Value>(&out.stdout) {
                Ok(v) => match v.get("cost_usd").and_then(|c| c.as_f64()) {
                    Some(c) => format!("${c:.2}"),
                    None => "(unavailable)".into(),
                },
                Err(e) => {
                    // Surface a crashed/garbage cost module (mirrors
                    // write_ledger_record): a reader must tell "no transcript"
                    // from "fno.cost._session_cost emitted non-JSON".
                    eprintln!(
                        "finalize: handoff cost: fno.cost._session_cost emitted non-JSON: {e}"
                    );
                    "(unavailable)".into()
                }
            }
        }
        Ok(out) => {
            eprintln!(
                "finalize: handoff cost: fno.cost._session_cost exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            "(unavailable)".into()
        }
        Err(e) => {
            eprintln!("finalize: handoff cost: fno.cost._session_cost spawn failed: {e}");
            "(unavailable)".into()
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Resolve the persistent handoffs directory:
///   1. explicit `--handoffs-dir`
///   2. `$HANDOFFS_DIR`
///   3. `config.paths.handoffs_dir` from project then global settings.yaml,
///      with `~` and `{project}` expanded (skipped if it still has `{...}`)
///   4. vault-derived `<vault>/internal/<project>/handoffs/` when
///      `obsidian.enabled` + `obsidian.vault` are set (placement rule,
///      ab-f063 Wave 2 - mirrors `paths.handoffs_dir()` in the Python CLI)
///   5. fallback `~/.fno/handoffs/<project>`
///
/// Pure-Rust resolution: it never shells `fno`, so the verb keeps its Python-CLI
/// independence (it only ever runs the in-package metric modules via
/// `python3 -m`).
fn resolve_handoffs_dir(
    override_dir: Option<&Path>,
    settings_override: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(d) = override_dir {
        return d.to_path_buf();
    }
    if let Some(d) = env_dir_unless_null("HANDOFFS_DIR") {
        return d;
    }
    let project = resolve_project_name(settings_override, home, cwd);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(s) = settings_override {
        candidates.push(s.to_path_buf());
    }
    candidates.push(cwd.join(".fno/settings.yaml"));
    if let Some(h) = home {
        candidates.push(h.join(".fno/settings.yaml"));
    }
    for sp in &candidates {
        if let Some(raw) = read_path_setting(sp, "handoffs_dir") {
            if let Some(expanded) = expand_handoffs_template(&raw, home, &project) {
                return expanded;
            }
        }
    }
    for sp in &candidates {
        if let Some(vault) = read_obsidian_vault(sp) {
            if let Some(vroot) = resolve_vault_root(&vault, home) {
                return vroot.join("internal").join(&project).join("handoffs");
            }
        }
    }
    let base = home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    base.join(".fno/handoffs").join(project)
}

/// Read `obsidian.vault` from a settings.yaml, but only when `obsidian.enabled`
/// is also true - mirrors `paths.vault_root()` in the Python CLI (both must be
/// set). Indent-aware (unlike `read_path_setting`) because `enabled:`/`vault:`
/// are generic key names reused by other settings.yaml sections
/// (`post_merge.enabled`, `think_spawn.enabled`, ...); a flat scan would risk
/// picking up an unrelated section's value.
fn read_obsidian_vault(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let mut block_indent: Option<usize> = None;
    let mut enabled = false;
    let mut vault: Option<String> = None;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some(bi) = block_indent {
            if indent <= bi {
                break; // dedented out of the obsidian: block
            }
            if let Some(rest) = t.strip_prefix("enabled:") {
                enabled = rest.trim().eq_ignore_ascii_case("true");
            } else if let Some(rest) = t.strip_prefix("vault:") {
                let v = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                if !v.is_empty() && !v.eq_ignore_ascii_case("null") {
                    vault = Some(v.to_string());
                }
            }
        } else if t == "obsidian:" {
            block_indent = Some(indent);
        }
    }
    if enabled {
        vault
    } else {
        None
    }
}

/// Expand a vault name to its filesystem root - mirrors `paths.vault_root()`:
/// a bare name (e.g. `c3po`) maps to `~/c3po`; an already-absolute or
/// `~`-prefixed value is honored as-is.
fn resolve_vault_root(vault: &str, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = vault.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    if vault == "~" {
        return home.map(Path::to_path_buf);
    }
    if Path::new(vault).is_absolute() {
        return Some(PathBuf::from(vault));
    }
    home.map(|h| h.join(vault))
}

/// Read a `<key>:` path value from a settings.yaml (any indent level). The
/// `config.paths.*` keys (`handoffs_dir`, `postmortems_dir`, ...) are
/// distinctive enough that a flat scan is safe.
/// Read a dir from an env var, treating an empty or literal-"null" value as
/// unset. emit_shell never emits "null", but a stale/hand-edited environment
/// can, and trusting it verbatim is what wrote `./null/` inside the repo
/// (x-54c2). Mirrors the same guard in read_path_setting.
fn env_dir_unless_null(key: &str) -> Option<PathBuf> {
    let v = std::env::var_os(key)?;
    // Only the string-decodable "null"/empty sentinel is filtered; a non-UTF-8
    // value (valid arbitrary-byte path on Unix) is preserved verbatim, matching
    // the original var_os behavior (gemini review).
    if let Some(s) = v.to_str() {
        let t = s.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("null") {
            return None;
        }
        return Some(PathBuf::from(t));
    }
    Some(PathBuf::from(v))
}

fn read_path_setting(path: &Path, key: &str) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let prefix = format!("{key}:");
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix(&prefix) {
            let v = rest
                .split('#')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(|c| c == '"' || c == '\'');
            // A YAML `null` scalar (`key: null`) is the "use default" sentinel,
            // not a literal path. emit_shell writes `postmortems_dir: null` for an
            // unset path, so reading it as the string "null" sent the writer to
            // `./null/<date>-<sid>.md` inside the repo (x-54c2). Treat it as absent
            // so the caller falls through to `~/.fno/<dir>`.
            if !v.is_empty() && !v.eq_ignore_ascii_case("null") {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Expand `~` and `{project}` in a handoffs_dir template. Returns None when the
/// result still contains an unresolved `{...}` token (e.g. `{vault}`), so the
/// caller falls back rather than writing to a literal-brace path.
fn expand_handoffs_template(raw: &str, home: Option<&Path>, project: &str) -> Option<PathBuf> {
    let mut s = raw.to_string();
    // Cannot expand a leading ~ without a home; return None so the caller falls
    // back to the default dir rather than writing to a literal "~..." path
    // (gemini review).
    if let Some(stripped) = s.strip_prefix("~/") {
        let h = home?;
        s = h.join(stripped).to_string_lossy().into_owned();
    } else if s == "~" {
        let h = home?;
        s = h.to_string_lossy().into_owned();
    }
    s = s.replace("{project}", project);
    if s.contains('{') {
        return None;
    }
    Some(PathBuf::from(s))
}

/// Project name = basename of the MAIN worktree (the first `git worktree list
/// --porcelain` entry), so a linked worktree resolves to "abilities", not the
/// worktree directory name. The porcelain first-entry is robust across layouts
/// (--separate-git-dir, bare) where the `--git-common-dir` parent is wrong
/// (gemini review HIGH). Falls back to the cwd basename.
fn repo_project_name(cwd: &Path) -> String {
    if let Some(porcelain) = git_capture(cwd, &["worktree", "list", "--porcelain"]) {
        if let Some(path_str) = porcelain
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("worktree "))
        {
            if let Some(name) = Path::new(path_str.trim()).file_name() {
                return name.to_string_lossy().into_owned();
            }
        }
    }
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into())
}

/// Resolve the `{project}` path token. Prefers `config.project.id` from
/// project-local then global settings.yaml (matching the Python resolver
/// `fno.paths._resolve`, paths.py:293-303, which reads project.id first and the
/// repo basename only as a fallback), then falls back to the git main-worktree
/// basename via `repo_project_name`. Non-fatal: a missing or malformed settings
/// file or an unset/`null` project.id degrades to the basename, so unconfigured
/// installs are unchanged. Uses the SAME project-then-global candidate order the
/// callers already use for `config.paths.*_dir`.
fn resolve_project_name(
    settings_override: Option<&Path>,
    home: Option<&Path>,
    cwd: &Path,
) -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(s) = settings_override {
        candidates.push(s.to_path_buf());
    }
    candidates.push(cwd.join(".fno/settings.yaml"));
    if let Some(h) = home {
        candidates.push(h.join(".fno/settings.yaml"));
    }
    for sp in candidates {
        if let Some(id) = read_project_id(&sp) {
            return id;
        }
    }
    repo_project_name(cwd)
}

/// Read the project id from a settings.yaml, matching the Python loader:
/// `config.project.id` is canonical; a deprecated top-level `project.id` is the
/// fallback (the loader lifts legacy `project.id` into `config.project` only
/// when the canonical value is unset, so config.project wins —
/// `cli/src/fno/config/__init__.py:1982-1990`). An empty/`null` value, an
/// unreadable file, or an id outside `[A-Za-z0-9._-]` all yield None so the
/// caller falls back to the basename.
fn read_project_id(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let id = yaml_scalar_at(&content, &["config", "project", "id"])
        .or_else(|| yaml_scalar_at(&content, &["project", "id"]))?;
    // The Python settings model rejects ids outside [A-Za-z0-9._-]
    // (config/__init__.py:176-185). A hand-edited invalid value (e.g. `foo/bar`)
    // must never be spliced into a `{project}` path segment, so degrade to the
    // basename rather than write artifacts outside the project dir.
    valid_project_id(&id).then_some(id)
}

/// Project ids are restricted to `[A-Za-z0-9._-]`, matching the Python
/// `validate_project_id` regex. ASCII byte check (no `regex` dependency).
fn valid_project_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Read a scalar at a dotted key path (e.g. `config.project.id`) from
/// block-style YAML via indent tracking. Scoping to the full path means a
/// false-positive key elsewhere (a `project:` block under another section, or a
/// legacy top-level one) cannot be mistaken for the canonical value. Inline
/// comments and surrounding quotes are stripped; an empty/`null` leaf yields
/// None. Only block-mapping style is understood (the settings.yaml schema);
/// flow style degrades to None and the caller falls back.
/// ponytail: bespoke path reader to stay off a YAML dependency, matching the
/// existing `read_path_setting` hand-scan; swap for serde_yaml if settings
/// parsing ever needs more than scalar lookups.
fn yaml_scalar_at(content: &str, path: &[&str]) -> Option<String> {
    // indents[i] = indentation of the i-th matched path segment's key line.
    let mut indents: Vec<usize> = Vec::new();
    // Indent of the leaf level's direct children, so a deeper grandchild key
    // with the same leaf name cannot be misread as the target.
    let mut leaf_indent: Option<usize> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let clean = trimmed.split('#').next().unwrap_or("").trim();
        if clean.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        // Pop matched segments we have dedented out of; resetting leaf_indent so
        // the next sibling subtree restarts its direct-child tracking.
        while indents.last().is_some_and(|&top| indent <= top) {
            indents.pop();
            leaf_indent = None;
        }
        let depth = indents.len();
        if depth + 1 == path.len() {
            // Leaf level: only accept the parent's direct children.
            let li = *leaf_indent.get_or_insert(indent);
            if indent != li {
                continue;
            }
            if let Some(rest) = clean.strip_prefix(&format!("{}:", path[depth])) {
                let v = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                return (!v.is_empty() && !v.eq_ignore_ascii_case("null")).then(|| v.to_string());
            }
        } else if clean == format!("{}:", path[depth]) {
            // Matched an intermediate mapping key; descend.
            indents.push(indent);
            leaf_indent = None;
        }
    }
    None
}

/// Best-effort PR URL for the current HEAD/branch via `gh`.
fn gh_pr_url(cwd: &Path) -> Option<String> {
    let out = Command::new("gh")
        .args(["pr", "view", "--json", "url", "-q", ".url"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Run `git <args>` in cwd, returning trimmed stdout on success.
fn git_capture(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

// ── postmortem artifact (stuck terminals only, ab-1a92b677) ───────────────────

/// Write a structured postmortem for a stuck (NoProgress/Budget) session to the
/// postmortems dir, then best-effort append a corrections.log pointer so the
/// autocorrect monthly review consumes it. Filename keyed by date + session-id
/// prefix so a retry overwrites rather than duplicating (idempotent).
#[allow(clippy::too_many_arguments)]
fn write_postmortem(
    cwd: &Path,
    session_id: &str,
    m: &ManifestFields,
    reason: &str,
    transcript: Option<&Path>,
    postmortems_override: Option<&Path>,
    settings_override: Option<&Path>,
    home: Option<&Path>,
) -> Result<String, String> {
    let dir = resolve_postmortems_dir(postmortems_override, settings_override, home, cwd);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let now = now_rfc3339_utc();
    // Defensive slice: now_rfc3339_utc() always returns a full RFC3339 string,
    // but never index a str blindly (gemini review). Falls back to the whole
    // string if it were ever shorter than the date prefix.
    let date = now.get(..10).unwrap_or(&now); // YYYY-MM-DD
    let sid_short: String = session_id.chars().take(16).collect();
    let file = dir.join(format!("{date}-{sid_short}.md"));

    let node = m.graph_node_id.clone().unwrap_or_else(|| "-".into());
    let plan = m.plan_path.clone().unwrap_or_else(|| "-".into());
    let title = m.input.clone().unwrap_or_else(|| "Untitled".into());
    let last_msg = transcript
        .and_then(last_assistant_text)
        .unwrap_or_else(|| "(transcript unavailable)".into());
    let commits = git_capture(cwd, &["log", "--oneline", "-10"])
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(log unavailable)".into());
    let tree = git_capture(cwd, &["status", "--short"])
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(clean)".into());

    let body = format!(
        "# Postmortem: {sid_short}\n\n\
         - session: `{session_id}`\n\
         - termination: **{reason}** (stuck: exited without shipping)\n\
         - node: `{node}`\n\
         - plan: `{plan}`\n\
         - feature: {title}\n\
         - generated: {now} (mechanical, by `fno-agents finalize`)\n\n\
         ## Last assistant message\n\n```\n{last_msg}\n```\n\n\
         ## Recent commits\n\n```\n{commits}\n```\n\n\
         ## Working tree\n\n```\n{tree}\n```\n\n\
         ## Triage\n\n\
         A `{reason}` terminal means `fno-agents loop-check` saw no forward \
         progress (or the budget cap tripped) and let the session exit. Review \
         the last message and working tree above: was the agent blocked on an \
         external dependency, looping without committing, or done but unable to \
         emit a promise? Feed recurring patterns back into the rules.\n",
    );
    fs::write(&file, &body).map_err(|e| format!("write {}: {e}", file.display()))?;

    append_corrections_pointer(home, &file, reason, &last_msg);
    Ok(file.to_string_lossy().into_owned())
}

/// Resolve the postmortems dir: explicit override -> `$POSTMORTEMS_DIR`
/// (exported by emit_shell.py from config.paths.postmortems_dir) -> the
/// `--settings` override file then project then global settings.yaml
/// `postmortems_dir:` -> default `~/.fno/postmortems`. Pure-Rust; never
/// shells `fno` (Domain Pitfall), mirroring resolve_handoffs_dir (which also
/// honors `--settings`, codex P2).
fn resolve_postmortems_dir(
    override_dir: Option<&Path>,
    settings_override: Option<&Path>,
    home: Option<&Path>,
    cwd: &Path,
) -> PathBuf {
    if let Some(d) = override_dir {
        return d.to_path_buf();
    }
    if let Some(d) = env_dir_unless_null("POSTMORTEMS_DIR") {
        return d;
    }
    let project = resolve_project_name(settings_override, home, cwd);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(s) = settings_override {
        candidates.push(s.to_path_buf());
    }
    candidates.push(cwd.join(".fno/settings.yaml"));
    if let Some(h) = home {
        candidates.push(h.join(".fno/settings.yaml"));
    }
    for sp in candidates {
        if let Some(raw) = read_path_setting(&sp, "postmortems_dir") {
            if let Some(expanded) = expand_handoffs_template(&raw, home, &project) {
                return expanded;
            }
        }
    }
    let base = home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    base.join(".fno/postmortems")
}

/// Best-effort: the newest assistant text message in the transcript JSONL, used
/// as the "what was it doing when it got stuck" signal. Bounded to keep the
/// artifact readable. Returns None on any read/parse miss.
fn last_assistant_text(transcript: &Path) -> Option<String> {
    let content = fs::read_to_string(transcript).ok()?;
    for line in content.lines().rev() {
        let line = line.trim();
        // Cheap pre-filter: an assistant entry always carries the literal
        // "assistant" (its role), so skip the JSON parse for the many user /
        // tool-output lines that don't (gemini review). No false negatives.
        if line.is_empty() || !line.contains("assistant") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = val
            .pointer("/message/role")
            .or_else(|| val.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if role != "assistant" {
            continue;
        }
        let text = assistant_text_blocks(&val);
        if !text.trim().is_empty() {
            return Some(text.chars().take(4000).collect());
        }
    }
    None
}

/// Join the text blocks of a transcript assistant entry: string content, or an
/// array of content blocks (tool_use/tool_result blocks skipped).
fn assistant_text_blocks(val: &Value) -> String {
    if let Some(s) = val.pointer("/message/content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = val.pointer("/message/content").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
    }
    // Top-level `{"role":"assistant","content":"..."}` shape (matches
    // loopcheck::extract_assistant_text and the hook tests; codex P2). Without
    // this, last_assistant_text accepts the role but records no message.
    if let Some(s) = val.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    String::new()
}

/// Best-effort: append a pointer line to `~/.fno/corrections.log` so the
/// autocorrect monthly review picks the postmortem up. Only writes when the log
/// already exists (the autocorrect feature creates it) - never creates it.
/// Format mirrors the pre-wedge generator:
/// `{ts} | S1 | target-postmortem | {path} | {reason}: {detail_truncated}`.
///
/// Lives under ~/.fno/, not ~/.claude/, per the placement rule (ab-f063 Wave
/// 2). Resolution order mirrors scripts/lib/corrections-lock.sh's
/// corrections_log_path(): POSTMORTEM_CORRECTIONS_LOG override, then
/// FNO_HOME, then home-relative default.
fn append_corrections_pointer(home: Option<&Path>, postmortem: &Path, reason: &str, detail: &str) {
    let log = match std::env::var_os("POSTMORTEM_CORRECTIONS_LOG") {
        Some(p) => PathBuf::from(p),
        None => match std::env::var_os("FNO_HOME") {
            Some(p) => PathBuf::from(p).join("corrections.log"),
            None => match home {
                Some(h) => h.join(".fno/corrections.log"),
                None => return,
            },
        },
    };
    if !log.is_file() {
        return; // autocorrect not enabled here; nothing to feed
    }
    let detail_trunc: String = detail.replace(['\n', '\r'], " ").chars().take(80).collect();
    let detail_trunc = if detail_trunc.trim().is_empty() {
        "-".to_string()
    } else {
        detail_trunc
    };
    let line = format!(
        "{} | S1 | target-postmortem | {} | {reason}: {detail_trunc}\n",
        now_rfc3339_utc(),
        postmortem.display(),
    );
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&log) {
        let _ = f.write_all(line.as_bytes());
    }
}

// ── unit tests (process-free) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_required_and_optional() {
        let a = parse_args(&[
            "--state".into(),
            "/x/state.md".into(),
            "--cwd".into(),
            "/x".into(),
            "--reason".into(),
            "DonePRGreen".into(),
            "--transcript".into(),
            "/t/abc.jsonl".into(),
        ])
        .unwrap();
        assert_eq!(a.state.unwrap(), PathBuf::from("/x/state.md"));
        assert_eq!(a.reason.unwrap(), "DonePRGreen");
        assert_eq!(a.transcript.unwrap(), PathBuf::from("/t/abc.jsonl"));
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        assert!(parse_args(&["--bogus".into()]).is_err());
    }

    #[test]
    fn manifest_reads_frontmatter_and_body_keys() {
        let content = "---\n\
            session_id: 20260607T220509Z-42092-ceefb9\n\
            plan_path: \"internal/fno/design/step6.md\"\n\
            input: \"ab-f8e5f214 no-merge\"\n\
            claude_transcript_id: de977b03-aaaa\n\
            ---\n\
            # Target Session State\n\
            graph_node_id: ab-f8e5f214\n\
            target_claim_key: \"node:ab-f8e5f214\"\n";
        let m = parse_manifest_fields(content);
        assert_eq!(
            m.session_id.as_deref(),
            Some("20260607T220509Z-42092-ceefb9")
        );
        assert_eq!(m.plan_path.as_deref(), Some("internal/fno/design/step6.md"));
        assert_eq!(m.claude_transcript_id.as_deref(), Some("de977b03-aaaa"));
        assert_eq!(m.graph_node_id.as_deref(), Some("ab-f8e5f214"));
        assert_eq!(m.input.as_deref(), Some("ab-f8e5f214 no-merge"));
    }

    #[test]
    fn manifest_reads_new_claude_session_id_key() {
        // The current key is claude_session_id (renamed from
        // claude_transcript_id). A manifest written by the new minter carries an
        // infix-tagged session_id and the new claude key; both must parse.
        let content = "---\n\
            session_id: 20260630T192705Z-cl52366-8979b6\n\
            claude_session_id: 26bf185f-a747-4624\n\
            ---\n";
        let m = parse_manifest_fields(content);
        assert_eq!(
            m.session_id.as_deref(),
            Some("20260630T192705Z-cl52366-8979b6")
        );
        assert_eq!(
            m.claude_transcript_id.as_deref(),
            Some("26bf185f-a747-4624")
        );
    }

    #[test]
    fn manifest_null_and_blank_are_skipped() {
        let m = parse_manifest_fields("plan_path: null\nsession_id: \nclaude_transcript_id: x\n");
        assert!(m.plan_path.is_none());
        assert!(m.session_id.is_none());
        assert_eq!(m.claude_transcript_id.as_deref(), Some("x"));
    }

    #[test]
    fn ship_reasons_gate() {
        assert!(SHIP_REASONS.contains(&"DonePRGreen"));
        assert!(SHIP_REASONS.contains(&"DoneAdvisory"));
        for non_ship in ["Budget", "NoProgress", "Interrupted", "Aborted", "NoWork"] {
            assert!(!SHIP_REASONS.contains(&non_ship));
        }
    }

    #[test]
    fn prior_finalize_ship_reads_ship_flag_and_session() {
        let dir = std::env::temp_dir().join(format!("finalize-idem-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("events.jsonl");
        // S1: a non-ship finalize (Budget); S2: a ship finalize.
        fs::write(
            &log,
            "{\"ts\":\"t\",\"type\":\"loop_check\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\"}}\n\
             {\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\",\"ship\":false}}\n\
             {\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S2\",\"ship\":true}}\n",
        )
        .unwrap();
        assert_eq!(
            prior_finalize_ship(&log, "S1"),
            Some(false),
            "non-ship prior"
        );
        assert_eq!(prior_finalize_ship(&log, "S2"), Some(true), "ship prior");
        assert_eq!(prior_finalize_ship(&log, "S3"), None, "no prior for S3");
        assert_eq!(
            prior_finalize_ship(&dir.join("missing.jsonl"), "S1"),
            None,
            "missing log -> None"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ship_flag_wins_regardless_of_event_order() {
        // A non-ship finalize followed by a ship finalize for the SAME session
        // must report Some(true) (the lockout-bug fix: a ship is terminal-complete).
        let dir = std::env::temp_dir().join(format!("finalize-order-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("events.jsonl");
        fs::write(
            &log,
            "{\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\",\"ship\":false}}\n\
             {\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\",\"ship\":true}}\n",
        )
        .unwrap();
        assert_eq!(prior_finalize_ship(&log, "S1"), Some(true));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_failed_event_does_not_count_as_finalized() {
        // A session_finalize_failed must NOT satisfy the idempotency guard, so
        // a later fire retries.
        let dir = std::env::temp_dir().join(format!("finalize-retry-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("events.jsonl");
        fs::write(
            &log,
            "{\"ts\":\"t\",\"type\":\"session_finalize_failed\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\"}}\n",
        )
        .unwrap();
        assert_eq!(prior_finalize_ship(&log, "S1"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrections_pointer_prefers_fno_home_over_claude_dir() {
        // ab-f063 Wave 2: corrections.log lives under ~/.fno/, not ~/.claude/.
        // FNO_HOME must win over a bare `home` fallback so an operator's
        // override (and the shared bash corrections_log_path() convention)
        // stays in sync with this Rust writer.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fno_home = std::env::temp_dir().join(format!("fin-corr-fh-{}", std::process::id()));
        let unused_home = std::env::temp_dir().join(format!("fin-corr-uh-{}", std::process::id()));
        let _ = fs::create_dir_all(&fno_home);
        let _ = fs::create_dir_all(&unused_home);
        let log_path = fno_home.join("corrections.log");
        fs::write(&log_path, "").unwrap();

        std::env::remove_var("POSTMORTEM_CORRECTIONS_LOG");
        std::env::set_var("FNO_HOME", &fno_home);
        append_corrections_pointer(Some(&unused_home), Path::new("/tmp/pm.md"), "Budget", "detail");
        std::env::remove_var("FNO_HOME");

        let contents = fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("target-postmortem"), "{contents}");
        // The old ~/.claude/ location must not be touched.
        assert!(!unused_home.join(".claude").exists());
        let _ = fs::remove_dir_all(&fno_home);
        let _ = fs::remove_dir_all(&unused_home);
    }

    #[test]
    fn corrections_pointer_falls_back_to_home_dot_fno() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("fin-corr-home-{}", std::process::id()));
        let fno_dir = home.join(".fno");
        fs::create_dir_all(&fno_dir).unwrap();
        let log_path = fno_dir.join("corrections.log");
        fs::write(&log_path, "").unwrap();

        std::env::remove_var("POSTMORTEM_CORRECTIONS_LOG");
        std::env::remove_var("FNO_HOME");
        append_corrections_pointer(Some(&home), Path::new("/tmp/pm.md"), "NoProgress", "d");

        let contents = fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("target-postmortem"), "{contents}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_handoffs_dir_uses_vault_when_obsidian_enabled() {
        // ab-f063 Wave 2: no explicit handoffs_dir override, obsidian enabled
        // with a vault -> <vault>/internal/<project>/handoffs/, matching
        // paths.handoffs_dir() in the Python CLI (not the old ~/.fno/handoffs
        // fallback).
        let dir = std::env::temp_dir().join(format!("fin-hd-vault-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "config:\n  project:\n    id: demo\n  obsidian:\n    enabled: true\n    vault: myvault\n",
        );
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join("myvault/internal/demo/handoffs"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_handoffs_dir_ignores_vault_when_obsidian_disabled() {
        // obsidian.enabled: false must NOT take the vault branch even though
        // vault: is set - falls through to the ~/.fno/handoffs/<project> default.
        let dir = std::env::temp_dir().join(format!("fin-hd-novault-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "config:\n  project:\n    id: demo\n  obsidian:\n    enabled: false\n    vault: myvault\n",
        );
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join(".fno/handoffs/demo"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_handoffs_dir_indent_scan_ignores_other_sections_enabled_key() {
        // A generic `enabled:` key in an earlier, unrelated section must not be
        // mistaken for obsidian.enabled (flat-scan-by-key would get this wrong;
        // the indent-aware block scan must not).
        let dir = std::env::temp_dir().join(format!("fin-hd-indent-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "config:\n  project:\n    id: demo\n  post_merge:\n    enabled: false\n  obsidian:\n    enabled: true\n    vault: myvault\n",
        );
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join("myvault/internal/demo/handoffs"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn handoffs_template_expands_tilde_and_project() {
        let home = PathBuf::from("/home/user");
        let got = expand_handoffs_template(
            "~/myvault/internal/{project}/handoffs/",
            Some(&home),
            "demo",
        );
        assert_eq!(
            got,
            Some(PathBuf::from("/home/user/myvault/internal/demo/handoffs/"))
        );
    }

    #[test]
    fn handoffs_template_none_home_falls_back() {
        // No home -> a ~ template cannot expand -> None so the caller uses the
        // default dir instead of writing a literal "~..." path (gemini review).
        assert_eq!(
            expand_handoffs_template("~/myvault/internal/{project}/handoffs/", None, "demo"),
            None
        );
        // A non-tilde absolute template still expands fine without a home.
        assert_eq!(
            expand_handoffs_template("/srv/{project}/handoffs", None, "demo"),
            Some(PathBuf::from("/srv/demo/handoffs"))
        );
    }

    #[test]
    fn handoffs_template_unresolved_brace_falls_back() {
        let home = PathBuf::from("/home/user");
        // {vault} cannot be resolved here -> None so the caller uses the fallback.
        assert_eq!(
            expand_handoffs_template("{vault}/fno/{project}/handoffs", Some(&home), "demo"),
            None
        );
    }

    #[test]
    fn read_path_setting_parses_value() {
        let dir = std::env::temp_dir().join(format!("finalize-set-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("settings.yaml");
        fs::write(
            &f,
            "config:\n  paths:\n    handoffs_dir: ~/myvault/internal/{project}/handoffs/  # note\n    postmortems_dir: ~/pm\n",
        )
        .unwrap();
        assert_eq!(
            read_path_setting(&f, "handoffs_dir").as_deref(),
            Some("~/myvault/internal/{project}/handoffs/")
        );
        assert_eq!(
            read_path_setting(&f, "postmortems_dir").as_deref(),
            Some("~/pm")
        );
        assert_eq!(read_path_setting(&f, "absent_key"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_path_setting_null_is_absent() {
        // emit_shell writes `postmortems_dir: null` for an unset path; reading it
        // as the literal "null" wrote `./null/` inside the repo (x-54c2). It must
        // read as absent so resolve_*_dir falls through to the `~/.fno` default.
        let dir = std::env::temp_dir().join(format!("finalize-null-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("settings.yaml");
        fs::write(&f, "    postmortems_dir: null\n    handoffs_dir: NULL\n").unwrap();
        assert_eq!(read_path_setting(&f, "postmortems_dir"), None);
        assert_eq!(read_path_setting(&f, "handoffs_dir"), None);

        // With the env override absent, the null settings value must resolve to
        // the absolute global default, never a relative `./null`.
        if std::env::var_os("POSTMORTEMS_DIR").is_none() {
            let home = PathBuf::from("/home/user");
            let resolved = resolve_postmortems_dir(None, Some(&f), Some(&home), &dir);
            assert_eq!(resolved, PathBuf::from("/home/user/.fno/postmortems"));
            assert!(resolved.is_absolute());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn postmortem_reasons_gate() {
        // Stuck terminals get a postmortem; ships and benign terminals do not.
        for stuck in ["NoProgress", "Budget"] {
            assert!(POSTMORTEM_REASONS.contains(&stuck));
        }
        for not_stuck in [
            "DonePRGreen",
            "DoneAdvisory",
            "NoWork",
            "Interrupted",
            "Aborted",
        ] {
            assert!(!POSTMORTEM_REASONS.contains(&not_stuck));
        }
    }

    #[test]
    fn resolve_postmortems_dir_prefers_override_then_settings_then_default() {
        let cwd = std::env::temp_dir().join(format!("finalize-pmdir-{}", std::process::id()));
        let _ = fs::create_dir_all(&cwd);
        let home = cwd.join("home");
        let ovr = cwd.join("explicit");
        std::env::remove_var("POSTMORTEMS_DIR");
        assert_eq!(
            resolve_postmortems_dir(Some(&ovr), None, Some(&home), &cwd),
            ovr,
            "explicit override wins"
        );
        // A `--settings` override file with postmortems_dir is honored (codex P2).
        let settings = cwd.join("custom-settings.yaml");
        fs::write(
            &settings,
            "config:\n  paths:\n    postmortems_dir: /srv/pm\n",
        )
        .unwrap();
        assert_eq!(
            resolve_postmortems_dir(None, Some(&settings), Some(&home), &cwd),
            PathBuf::from("/srv/pm"),
            "--settings postmortems_dir is honored"
        );
        // No override, no env, no settings -> ~/.fno/postmortems.
        assert_eq!(
            resolve_postmortems_dir(None, None, Some(&home), &cwd),
            home.join(".fno/postmortems")
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn assistant_text_blocks_handles_string_and_array() {
        let s = serde_json::json!({"message": {"content": "hi"}});
        assert_eq!(assistant_text_blocks(&s), "hi");
        let arr = serde_json::json!({"message": {"content": [
            {"type": "text", "text": "a"},
            {"type": "tool_use", "name": "x"},
            {"type": "text", "text": "b"}
        ]}});
        assert_eq!(assistant_text_blocks(&arr), "a b");
        // Top-level {"content": "..."} shape (codex P2 fallback).
        let top = serde_json::json!({"role": "assistant", "content": "top-level"});
        assert_eq!(assistant_text_blocks(&top), "top-level");
        assert_eq!(assistant_text_blocks(&serde_json::json!({})), "");
    }

    #[test]
    fn last_assistant_text_reads_top_level_content_shape() {
        // codex P2: a top-level {"role":"assistant","content":"..."} transcript
        // entry must yield its message, not "(transcript unavailable)".
        let dir = std::env::temp_dir().join(format!("finalize-lat-top-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let t = dir.join("transcript.jsonl");
        fs::write(
            &t,
            "{\"role\":\"assistant\",\"content\":\"top-level final\"}\n",
        )
        .unwrap();
        assert_eq!(last_assistant_text(&t).as_deref(), Some("top-level final"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_assistant_text_picks_newest_assistant_entry() {
        let dir = std::env::temp_dir().join(format!("finalize-lat-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let t = dir.join("transcript.jsonl");
        fs::write(
            &t,
            "{\"message\":{\"role\":\"assistant\",\"content\":\"old\"}}\n\
             {\"message\":{\"role\":\"user\",\"content\":\"ignored\"}}\n\
             {\"message\":{\"role\":\"assistant\",\"content\":\"newest\"}}\n",
        )
        .unwrap();
        assert_eq!(last_assistant_text(&t).as_deref(), Some("newest"));
        assert_eq!(last_assistant_text(&dir.join("missing.jsonl")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_postmortem_writes_artifact_with_reason_and_node() {
        let dir = std::env::temp_dir().join(format!("finalize-pm-{}", std::process::id()));
        let pmdir = dir.join("postmortems");
        let _ = fs::create_dir_all(&dir);
        let m = ManifestFields {
            graph_node_id: Some("ab-1a92b677".into()),
            plan_path: Some("plan.md".into()),
            input: Some("a stuck feature".into()),
            ..Default::default()
        };
        let path = write_postmortem(
            &dir,
            "20260607T010101Z-1-abc",
            &m,
            "NoProgress",
            None,
            Some(&pmdir),
            None,
            Some(&dir),
        )
        .expect("postmortem written");
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("termination: **NoProgress**"));
        assert!(body.contains("ab-1a92b677"));
        assert!(body.contains("a stuck feature"));
        assert!(body.contains("(transcript unavailable)"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_expected_url_count_cases() {
        let dir = std::env::temp_dir().join(format!("finalize-xpc-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        // Single-project always -> None (let stamp/graduate default to 1).
        assert_eq!(derive_expected_url_count(&dir, "plan.md", false), None);

        // Cross-project plan with a 2-key projects map (with nested sub-keys
        // that must NOT be counted) -> Some(2). (codex P1 regression.)
        let plan = dir.join("xproj.md");
        fs::write(
            &plan,
            "---\nstatus: ready\nscope: cross-project\nprojects:\n  alpha:\n    repo: a\n    branch: x\n  beta:\n    repo: b\nwaves:\n  - 1\n---\n# plan\n",
        )
        .unwrap();
        assert_eq!(
            derive_expected_url_count(&dir, "xproj.md", true),
            Some(2),
            "counts direct project keys only, not nested repo/branch"
        );

        // Cross-project but no projects map -> None so the caller skips graduate.
        let nomap = dir.join("nomap.md");
        fs::write(&nomap, "---\nstatus: ready\n---\n# plan\n").unwrap();
        assert_eq!(derive_expected_url_count(&dir, "nomap.md", true), None);

        // Folder plan: projects map lives in 00-INDEX.md.
        let folder = dir.join("folderplan");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("00-INDEX.md"),
            "---\nprojects:\n  one:\n    repo: o\n  two:\n    repo: t\n  three:\n    repo: h\n---\n",
        )
        .unwrap();
        assert_eq!(derive_expected_url_count(&dir, "folderplan", true), Some(3));

        let _ = fs::remove_dir_all(&dir);
    }

    // ── project name resolution (x-44e7) ──────────────────────────────────

    fn write_settings(dir: &Path, body: &str) {
        let cfg = dir.join(".fno");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(cfg.join("settings.yaml"), body).unwrap();
    }

    #[test]
    fn project_id_parses_nested_scalar() {
        let dir = std::env::temp_dir().join(format!("fin-projid-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("settings.yaml");
        // basename of the dir differs from project.id on purpose.
        fs::write(
            &f,
            "config:\n  project:\n    id: fno\n  obsidian:\n    id: ignored\n",
        )
        .unwrap();
        assert_eq!(read_project_id(&f).as_deref(), Some("fno"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_null_and_missing_are_unset() {
        let dir = std::env::temp_dir().join(format!("fin-projnull-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let null = dir.join("null.yaml");
        fs::write(&null, "config:\n  project:\n    id: null\n").unwrap();
        assert_eq!(read_project_id(&null), None, "null id -> unset");
        let empty = dir.join("empty.yaml");
        fs::write(&empty, "config:\n  project: {}\n").unwrap();
        assert_eq!(read_project_id(&empty), None, "no id key -> unset");
        assert_eq!(
            read_project_id(&dir.join("absent.yaml")),
            None,
            "missing file -> unset"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_project_name_prefers_project_id_over_basename() {
        // Dir basename is "footnote-like"; project.id is "fno".
        let dir = std::env::temp_dir().join(format!("fin-rpn-pref-{}", std::process::id()));
        let cwd = dir.join("footnote-like");
        let _ = fs::create_dir_all(&cwd);
        write_settings(&cwd, "config:\n  project:\n    id: fno\n");
        let home = dir.join("home"); // no settings -> not consulted before cwd
        let _ = fs::create_dir_all(&home);
        assert_eq!(resolve_project_name(None, Some(&home), &cwd), "fno");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_project_name_falls_back_to_basename() {
        // No project.id anywhere -> git/cwd basename (here, the cwd dir name).
        let dir = std::env::temp_dir().join(format!("fin-rpn-fb-{}", std::process::id()));
        let cwd = dir.join("regready-ccld-pipeline");
        let _ = fs::create_dir_all(&cwd);
        let home = dir.join("home");
        let _ = fs::create_dir_all(&home);
        assert_eq!(
            resolve_project_name(None, Some(&home), &cwd),
            "regready-ccld-pipeline"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_project_name_local_over_global() {
        // Project-local project.id wins over the global one.
        let dir = std::env::temp_dir().join(format!("fin-rpn-lg-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(&cwd, "config:\n  project:\n    id: fno\n");
        write_settings(&home, "config:\n  project:\n    id: other\n");
        assert_eq!(resolve_project_name(None, Some(&home), &cwd), "fno");
        let _ = fs::remove_dir_all(&dir);
    }

    fn write_yaml(dir: &Path, name: &str, body: &str) -> PathBuf {
        let _ = fs::create_dir_all(dir);
        let f = dir.join(name);
        fs::write(&f, body).unwrap();
        f
    }

    #[test]
    fn project_id_ignores_false_positive_block_and_inline_comments() {
        // A `project:` under another section appears BEFORE config.project, and
        // both config: and project: carry inline comments (gemini HIGH).
        let dir = std::env::temp_dir().join(format!("fin-fp-{}", std::process::id()));
        let f = write_yaml(
            &dir,
            "s.yaml",
            "other_tool:\n  project:\n    id: wrong\nconfig: # cfg\n  project: # proj\n    id: right\n",
        );
        assert_eq!(read_project_id(&f).as_deref(), Some("right"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_config_wins_over_legacy_top_level() {
        // Legacy top-level project.id is only a fallback; config.project wins
        // (config/__init__.py:1982-1990).
        let dir = std::env::temp_dir().join(format!("fin-legacy-{}", std::process::id()));
        let win = write_yaml(
            &dir,
            "win.yaml",
            "project:\n  id: legacy\nconfig:\n  project:\n    id: canon\n",
        );
        assert_eq!(read_project_id(&win).as_deref(), Some("canon"));
        // No canonical block -> legacy top-level is the fallback.
        let fb = write_yaml(&dir, "fb.yaml", "project:\n  id: legacy\n");
        assert_eq!(read_project_id(&fb).as_deref(), Some("legacy"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_rejects_invalid_chars() {
        // A hand-edited id with a path separator must not reach a path segment
        // (codex P2; mirrors validate_project_id). Falls back to None.
        let dir = std::env::temp_dir().join(format!("fin-inval-{}", std::process::id()));
        let f = write_yaml(&dir, "s.yaml", "config:\n  project:\n    id: foo/bar\n");
        assert_eq!(read_project_id(&f), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_skips_grandchild_id_key() {
        // A deeper `id:` under a nested sub-mapping is not the project id.
        let dir = std::env::temp_dir().join(format!("fin-gc-{}", std::process::id()));
        let f = write_yaml(
            &dir,
            "s.yaml",
            "config:\n  project:\n    nested:\n      id: deep\n    id: good\n",
        );
        assert_eq!(read_project_id(&f).as_deref(), Some("good"));
        let _ = fs::remove_dir_all(&dir);
    }
}
