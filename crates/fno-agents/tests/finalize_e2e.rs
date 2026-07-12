//! Integration tests for `fno-agents finalize` (control-plane step 6,
//! ab-f8e5f214): the terminal-only side-effect writer.
//!
//! These drive the REAL built binary against a hermetic temp env, with the
//! Python helpers replaced by tiny in-package module stubs (in a temp
//! PYTHONPATH `fno/cost/*` + `fno/plan/_stamp.py` package) that record their
//! invocations to a `calls.log` and can be told to fail. This
//! lets us assert the orchestration contract without depending on the real
//! ledger/flock/stamp machinery (those are covered by their own Python tests):
//!
//! - ALWAYS branch: ledger session-record fires on every terminal reason.
//! - SHIP branch: stamp/graduate + handoff fire only on DonePRGreen/DoneAdvisory.
//! - idempotency: a prior `session_finalized` event short-circuits a re-fire.
//! - non-fatal: a failing sub-step emits `session_finalize_failed`, never
//!   raises the exit code, and lets the remaining steps run.
//! - archived/missing manifest (delegated path): no-op, exit 0.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_fno-agents");

struct Env {
    _tmp: TempDir,
    cwd: PathBuf,
    pypath: PathBuf,
    state: PathBuf,
    events: PathBuf,
    global_events: PathBuf,
    handoffs: PathBuf,
    postmortems: PathBuf,
    calls_log: PathBuf,
}

/// Build a hermetic env. `register_fails` makes the register-task stub exit 1.
fn setup(session_id: &str, register_fails: bool) -> Env {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let cwd = root.join("proj");
    // PYTHONPATH root for the in-package stubs: finalize now runs the cost +
    // stamp helpers as `python3 -m fno.cost._session_cost`,
    // `fno.cost._register`, and `fno.plan._stamp`, so we shadow the real
    // package with fake `fno/cost/*` + `fno/plan/_stamp.py` modules resolved
    // off this dir (set in run_finalize's env).
    let pypath = root.join("pypath");
    let handoffs = root.join("handoffs");
    let postmortems = root.join("postmortems");
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::create_dir_all(pypath.join("fno/plan")).unwrap();
    fs::create_dir_all(pypath.join("fno/cost")).unwrap();
    fs::create_dir_all(&handoffs).unwrap();

    let calls_log = cwd.join("calls.log");

    // Manifest (frontmatter + body graph_node_id, like the real one).
    let state = cwd.join(".fno/target-state.md");
    fs::write(
        &state,
        format!(
            "---\n\
             session_id: {session_id}\n\
             created_at: 2026-06-07T00:00:00Z\n\
             input: \"ab-test feature\"\n\
             plan_path: \"plan.md\"\n\
             provider: claude\n\
             claude_transcript_id: tid-{session_id}\n\
             ---\n\
             # Target Session State\n\
             graph_node_id: ab-testnode\n"
        ),
    )
    .unwrap();
    // A plan file so stamp/graduate stubs have a target (content irrelevant).
    fs::write(cwd.join("plan.md"), "---\nstatus: ready\n---\n").unwrap();

    // Package markers + the in-package stubs resolved off PYTHONPATH
    // (run_finalize sets PYTHONPATH=<pypath>) so finalize's `python3 -m
    // fno.<pkg>.<mod>` children run THESE stubs, not the real package.
    fs::write(pypath.join("fno/__init__.py"), "").unwrap();
    fs::write(pypath.join("fno/plan/__init__.py"), "").unwrap();
    fs::write(pypath.join("fno/cost/__init__.py"), "").unwrap();
    // fno.cost._session_cost stub: record the call, emit valid cost JSON on
    // stdout (matches the old session-cost.py stub's calls.log line + output).
    fs::write(
        pypath.join("fno/cost/_session_cost.py"),
        "import sys, json, os\n\
         open('calls.log','a').write('session-cost\\n')\n\
         print(json.dumps({'cost_usd': 1.23, 'tokens': {'total': 100, 'cache_read': 10}, 'duration_minutes': 5.0, 'primary_model': 'claude-opus', 'compactions': 0}))\n",
    )
    .unwrap();
    // fno.cost._register stub: record the call (+ its --termination-reason),
    // fail if asked. Mirrors the old register-task.py stub's calls.log line so
    // the call-shape assertions stay equivalent.
    let reg = if register_fails {
        "import sys\n\
         open('calls.log','a').write('register-task FAIL\\n')\n\
         sys.exit(1)\n"
    } else {
        "import sys\n\
         tr = ''\n\
         if '--termination-reason' in sys.argv:\n\
         \x20   tr = sys.argv[sys.argv.index('--termination-reason')+1]\n\
         cj = '--cost-json' in sys.argv\n\
         open('calls.log','a').write('register-task reason=%s costjson=%s\\n' % (tr, cj))\n"
    };
    fs::write(pypath.join("fno/cost/_register.py"), reg).unwrap();
    // fno.plan._stamp stub: records the subcommand (stamp|graduate) to
    // calls.log, mirroring the prior stub's `stamp-plan %s` line so the
    // call-shape assertions stay equivalent.
    fs::write(
        pypath.join("fno/plan/_stamp.py"),
        "import sys\n\
         sub = sys.argv[1] if len(sys.argv) > 1 else '?'\n\
         open('calls.log','a').write('stamp-plan %s\\n' % sub)\n",
    )
    .unwrap();
    // fno.verify_advise stub (W6): record the full argv so the ship tests can
    // assert the flag shape finalize passes (a rename on either side of the
    // Rust->Python boundary fails here, not silently in production).
    fs::write(
        pypath.join("fno/verify_advise.py"),
        "import sys\n\
         open('calls.log','a').write('verify-advise %s\\n' % ' '.join(sys.argv[1:]))\n",
    )
    .unwrap();

    Env {
        _tmp: tmp,
        cwd,
        pypath,
        state,
        events: root.join("proj/.fno/events.jsonl"),
        global_events: root.join("global-events.jsonl"),
        handoffs,
        postmortems,
        calls_log,
    }
}

fn run_finalize(env: &Env, reason: &str) -> std::process::Output {
    Command::new(BIN)
        .arg("finalize")
        .arg("--state")
        .arg(&env.state)
        .arg("--cwd")
        .arg(&env.cwd)
        .arg("--reason")
        .arg(reason)
        .arg("--events")
        .arg(&env.events)
        .arg("--global-events")
        .arg(&env.global_events)
        .arg("--handoffs-dir")
        .arg(&env.handoffs)
        .arg("--postmortems-dir")
        .arg(&env.postmortems)
        // Shadow the real `fno` package with the PYTHONPATH stub so finalize's
        // `python3 -m fno.cost._session_cost`, `fno.cost._register`, and
        // `fno.plan._stamp` children resolve the test stubs. Set to the bare
        // pypath (PYTHONPATH entries prepend to sys.path) so the stubs win over
        // any site-packages/editable install of the real package.
        .env("PYTHONPATH", &env.pypath)
        .current_dir(&env.cwd)
        .output()
        .expect("run finalize")
}

fn calls(env: &Env) -> String {
    fs::read_to_string(&env.calls_log).unwrap_or_default()
}
fn events_text(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}
fn count_event(p: &Path, kind: &str, session_id: &str) -> usize {
    events_text(p)
        .lines()
        .filter(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .map(|v| {
                    v.get("type").and_then(|t| t.as_str()) == Some(kind)
                        && v.pointer("/data/session_id").and_then(|s| s.as_str())
                            == Some(session_id)
                })
                .unwrap_or(false)
        })
        .count()
}
fn handoff_files(env: &Env) -> Vec<PathBuf> {
    fs::read_dir(&env.handoffs)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}
fn postmortem_files(env: &Env) -> Vec<PathBuf> {
    fs::read_dir(&env.postmortems)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

/// Every terminal reason writes the ledger record; a NON-ship reason runs
/// neither stamp/graduate nor the handoff artifact. (AC7-HP always-branch.)
#[test]
fn finalize_ledger_every_exit() {
    let env = setup("S-budget", false);
    let out = run_finalize(&env, "Budget");
    assert!(out.status.success(), "finalize must exit 0");
    let c = calls(&env);
    assert!(
        c.contains("register-task reason=Budget"),
        "ledger record must fire: {c}"
    );
    assert!(
        !c.contains("stamp-plan"),
        "non-ship reason must NOT stamp: {c}"
    );
    assert!(
        !c.contains("verify-advise"),
        "non-ship reason must NOT run the verifier advisory: {c}"
    );
    assert!(
        handoff_files(&env).is_empty(),
        "non-ship reason must NOT write a handoff"
    );
    // Budget is a STUCK terminal: it gets a postmortem (ab-1a92b677).
    let pms = postmortem_files(&env);
    assert_eq!(pms.len(), 1, "Budget terminal must write one postmortem");
    let pm = fs::read_to_string(&pms[0]).unwrap();
    assert!(
        pm.contains("termination: **Budget**"),
        "postmortem names the reason: {pm}"
    );
    assert!(pm.contains("ab-testnode"), "postmortem names the node");
    assert_eq!(count_event(&env.events, "session_finalized", "S-budget"), 1);
    // Mirrored to the global log too.
    assert_eq!(
        count_event(&env.global_events, "session_finalized", "S-budget"),
        1
    );
}

/// A ship reason (and a benign NoWork) is NOT "stuck": no postmortem written.
/// (ab-1a92b677 negative case.)
#[test]
fn finalize_no_postmortem_on_ship_or_benign() {
    let ship = setup("S-noprm-ship", false);
    assert!(run_finalize(&ship, "DonePRGreen").status.success());
    assert!(
        postmortem_files(&ship).is_empty(),
        "ship reason must NOT write a postmortem"
    );
    let benign = setup("S-noprm-nowork", false);
    assert!(run_finalize(&benign, "NoWork").status.success());
    assert!(
        postmortem_files(&benign).is_empty(),
        "NoWork is benign, must NOT write a postmortem"
    );
}

/// A stuck session that terminated Interrupted or Aborted (gave up mid-wedge or
/// got cancelled) now writes a postmortem - the widened corpus (x-42f6 US2).
#[test]
fn finalize_postmortem_on_interrupted_or_aborted() {
    for (sid, reason) in [("S-interrupted", "Interrupted"), ("S-aborted", "Aborted")] {
        let env = setup(sid, false);
        assert!(run_finalize(&env, reason).status.success());
        let pms = postmortem_files(&env);
        assert_eq!(pms.len(), 1, "{reason} terminal must write one postmortem");
        let pm = fs::read_to_string(&pms[0]).unwrap();
        assert!(
            pm.contains(&format!("termination: **{reason}**")),
            "postmortem names the reason: {pm}"
        );
    }
}

/// A ship reason runs ledger + stamp + graduate + handoff and emits
/// session_finalized. (AC5-HP.)
#[test]
fn finalize_ship_gated() {
    let env = setup("S-ship", false);
    let out = run_finalize(&env, "DonePRGreen");
    assert!(out.status.success());
    let c = calls(&env);
    assert!(
        c.contains("register-task reason=DonePRGreen"),
        "ledger: {c}"
    );
    assert!(c.contains("stamp-plan stamp"), "stamp must fire: {c}");
    assert!(c.contains("stamp-plan graduate"), "graduate must fire: {c}");
    // W6 verifier advisory rides the ship branch with the manifest's fields;
    // this line is the Rust->Python flag-shape contract (a flag rename on
    // either side fails here).
    let adv = c
        .lines()
        .find(|l| l.starts_with("verify-advise"))
        .expect("ship fire runs verify_advise");
    for want in [
        "--node-id ab-testnode",
        "--session-id S-ship",
        "--reason DonePRGreen",
        "--plan-path plan.md",
        "--events",
        "--global-events",
    ] {
        assert!(
            adv.contains(want),
            "verify-advise argv missing {want}: {adv}"
        );
    }
    assert_eq!(handoff_files(&env).len(), 1, "exactly one handoff artifact");
    let handoff = fs::read_to_string(&handoff_files(&env)[0]).unwrap();
    assert!(
        handoff.contains("ab-testnode"),
        "handoff names the node: {handoff}"
    );
    assert!(handoff.contains("S-ship"), "handoff names the session");
    assert_eq!(count_event(&env.events, "session_finalized", "S-ship"), 1);
}

/// Idempotency: N stop-hook fires after a successful finalize produce exactly
/// one ledger row, one stamp, one handoff, one session_finalized. (AC5-EDGE.)
#[test]
fn finalize_idempotent_across_refires() {
    let env = setup("S-idem", false);
    for _ in 0..4 {
        let out = run_finalize(&env, "DonePRGreen");
        assert!(out.status.success());
    }
    let c = calls(&env);
    assert_eq!(
        c.matches("register-task").count(),
        1,
        "exactly one ledger call: {c}"
    );
    assert_eq!(
        c.matches("stamp-plan stamp").count(),
        1,
        "exactly one stamp: {c}"
    );
    assert_eq!(handoff_files(&env).len(), 1, "exactly one handoff");
    assert_eq!(count_event(&env.events, "session_finalized", "S-idem"), 1);
}

/// Non-fatal partial failure: a failing ledger step emits
/// session_finalize_failed (naming the step), does NOT emit session_finalized,
/// still runs stamp/handoff, and the process exits 0. A later fire then retries
/// (no session_finalized guard yet). (AC5-ERR.)
#[test]
fn finalize_nonfatal_partial_failure() {
    let env = setup("S-fail", true); // register-task stub exits 1
    let out = run_finalize(&env, "DonePRGreen");
    assert!(
        out.status.success(),
        "side-effect failure must NOT raise exit code"
    );
    let c = calls(&env);
    assert!(
        c.contains("register-task FAIL"),
        "ledger was attempted: {c}"
    );
    assert!(
        c.contains("stamp-plan stamp"),
        "stamp still runs after ledger failure: {c}"
    );
    assert_eq!(
        count_event(&env.events, "session_finalize_failed", "S-fail"),
        1,
        "a failure event is emitted"
    );
    assert_eq!(
        count_event(&env.events, "session_finalized", "S-fail"),
        0,
        "session_finalized NOT emitted on partial failure (so a re-fire retries)"
    );
    // The failure event names the failing step.
    let txt = events_text(&env.events);
    assert!(
        txt.contains("\"ledger\""),
        "failed_steps names ledger: {txt}"
    );
}

/// An archived/missing manifest (the delegated-session path: handoff.sh moved
/// it and already wrote the ledger row) is a clean no-op, exit 0, no events.
#[test]
fn finalize_missing_manifest_is_noop() {
    let env = setup("S-gone", false);
    fs::remove_file(&env.state).unwrap();
    let out = run_finalize(&env, "DonePRGreen");
    assert!(out.status.success());
    assert!(
        !env.calls_log.exists() || calls(&env).is_empty(),
        "no scripts run"
    );
    assert!(
        events_text(&env.events).is_empty(),
        "no events on missing manifest"
    );
}

/// Per-node rollup: three sessions on the same node each leave one ledger
/// record carrying their own reason; grouping on graph_node_id yields all
/// three. We assert the per-session register-task call shape (the real ledger
/// dedup/rollup is covered by the Python register-task tests). (AC7-HP/FR.)
#[test]
fn finalize_three_sessions_one_node() {
    for (sid, reason) in [
        ("node-shipped", "DonePRGreen"),
        ("node-delegated", "delegated"),
        ("node-budget", "Budget"),
    ] {
        let env = setup(sid, false);
        let out = run_finalize(&env, reason);
        assert!(out.status.success());
        let c = calls(&env);
        assert!(
            c.contains(&format!("register-task reason={reason}")),
            "session {sid} records reason={reason}: {c}"
        );
        // Only the shipped session runs the completion side-effects.
        if reason == "DonePRGreen" {
            assert!(c.contains("stamp-plan stamp"), "shipped session stamps");
        } else {
            assert!(!c.contains("stamp-plan"), "{sid} ({reason}) must not stamp");
        }
    }
}

/// `delegated` is a non-ship reason: ledger row only, no stamp/handoff. This is
/// exactly what handoff.sh invokes against the archived manifest. (AC7-EDGE,
/// the finalize half; the handoff-call wiring is covered by the bash test.)
#[test]
fn finalize_delegated_is_ledger_only() {
    let env = setup("S-deleg", false);
    let out = run_finalize(&env, "delegated");
    assert!(out.status.success());
    let c = calls(&env);
    assert!(
        c.contains("register-task reason=delegated"),
        "ledger row: {c}"
    );
    assert!(
        !c.contains("stamp-plan"),
        "delegated must not stamp/graduate: {c}"
    );
    assert!(
        handoff_files(&env).is_empty(),
        "delegated must not write a handoff"
    );
    assert_eq!(count_event(&env.events, "session_finalized", "S-deleg"), 1);
}

/// sigma-review HIGH (the lockout fix): a session that hit a NON-ship terminal
/// (Budget) and then ships within the same session MUST still run
/// stamp/graduate/handoff on the ship fire. The prior non-ship session_finalized
/// must not lock the ship side-effects out; a further fire then early-returns on
/// the recorded ship.
#[test]
fn finalize_nonship_then_ship_runs_ship_sideeffects() {
    let env = setup("S-recover", false);
    assert!(run_finalize(&env, "Budget").status.success()); // fire 1: non-ship
    assert!(run_finalize(&env, "DonePRGreen").status.success()); // fire 2: ship
    let c = calls(&env);
    // Ledger written ONCE (the Budget fire); the ship fire skips the redundant
    // ledger step (register-task would dedup) but DOES run the ship side-effects.
    assert_eq!(
        c.matches("register-task").count(),
        1,
        "ledger written once: {c}"
    );
    assert!(
        c.contains("stamp-plan stamp"),
        "ship fire must stamp after a non-ship terminal: {c}"
    );
    assert!(
        c.contains("stamp-plan graduate"),
        "ship fire must graduate: {c}"
    );
    assert_eq!(handoff_files(&env).len(), 1, "ship fire writes the handoff");
    assert_eq!(
        count_event(&env.events, "session_finalized", "S-recover"),
        2,
        "two finalized events: ship:false then ship:true"
    );
    // Fire 3: now the ship is recorded -> early-return, no extra stamp.
    assert!(run_finalize(&env, "DonePRGreen").status.success());
    assert_eq!(
        calls(&env).matches("stamp-plan stamp").count(),
        1,
        "stamp ran exactly once across all fires"
    );
}

/// W6 never-wedge lock: a FAILING verifier advisory (exit 1) must not hold
/// session_finalized open for retry, must not appear in failed_steps, and must
/// not raise the exit code. The advisory is log-only by contract.
#[test]
fn finalize_verify_advise_failure_never_wedges() {
    let env = setup("S-advfail", false);
    fs::write(
        env.pypath.join("fno/verify_advise.py"),
        "import sys\n\
         open('calls.log','a').write('verify-advise FAIL\\n')\n\
         sys.stderr.write('advisory exploded')\n\
         sys.exit(1)\n",
    )
    .unwrap();
    let out = run_finalize(&env, "DonePRGreen");
    assert!(out.status.success(), "advisory failure must not raise exit");
    assert!(calls(&env).contains("verify-advise FAIL"), "advisory ran");
    assert_eq!(
        count_event(&env.events, "session_finalized", "S-advfail"),
        1,
        "session_finalized still emitted despite the advisory failure"
    );
    assert_eq!(
        count_event(&env.events, "session_finalize_failed", "S-advfail"),
        0,
        "advisory failure never lands in failed_steps"
    );
}

/// DoneAdvisory is the second ship reason: it must run stamp/graduate + handoff.
#[test]
fn finalize_doneadvisory_ships() {
    let env = setup("S-adv", false);
    assert!(run_finalize(&env, "DoneAdvisory").status.success());
    let c = calls(&env);
    assert!(
        c.contains("register-task reason=DoneAdvisory"),
        "ledger: {c}"
    );
    assert!(
        c.contains("stamp-plan stamp"),
        "DoneAdvisory is a ship reason: {c}"
    );
    assert_eq!(handoff_files(&env).len(), 1);
}

/// AC7-ERR: no recoverable transcript -> the ledger row still lands, but
/// register-task is invoked WITHOUT --cost-json (cost_usd becomes null) and
/// session-cost.py is not run at all.
#[test]
fn finalize_cost_null_when_no_transcript() {
    let env = setup("S-nocost", false);
    // Rewrite the manifest dropping claude_transcript_id; run_finalize passes no
    // --transcript, so finalize has no transcript uuid to cost against.
    fs::write(
        &env.state,
        "---\nsession_id: S-nocost\ncreated_at: 2026-06-07T00:00:00Z\ninput: \"x\"\nplan_path: \"plan.md\"\nprovider: claude\n---\n# Target Session State\ngraph_node_id: ab-testnode\n",
    )
    .unwrap();
    assert!(run_finalize(&env, "Budget").status.success());
    let c = calls(&env);
    assert!(
        c.contains("register-task reason=Budget costjson=False"),
        "no transcript -> ledger row without --cost-json (cost=null): {c}"
    );
    assert!(
        !c.contains("session-cost"),
        "session-cost.py skipped when no transcript uuid: {c}"
    );
}

// ── node<->PR pr_number backstop stamp (x-280d) ──────────────────────────────
// finalize shells `gh pr view` and `fno backlog update`; these tests shim both
// onto PATH (mirroring the PYTHONPATH-stub pattern for the Python helpers) to
// assert the stamp fires on a non-ship terminal, skips when there's no PR/node,
// and never raises the exit code.

fn write_shim(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Run finalize with `gh` + `fno` shims on PATH. `gh_body` is a full shell
/// script; the `fno` shim records its argv to calls.log (same file the Python
/// stubs use) and exits 0.
fn run_finalize_shimmed(env: &Env, reason: &str, gh_body: &str) -> std::process::Output {
    let bin = env.cwd.join("shimbin");
    fs::create_dir_all(&bin).unwrap();
    write_shim(&bin, "gh", gh_body);
    write_shim(&bin, "fno", "#!/bin/sh\necho \"fno $*\" >> calls.log\n");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(BIN)
        .arg("finalize")
        .arg("--state")
        .arg(&env.state)
        .arg("--cwd")
        .arg(&env.cwd)
        .arg("--reason")
        .arg(reason)
        .arg("--events")
        .arg(&env.events)
        .arg("--global-events")
        .arg(&env.global_events)
        .arg("--handoffs-dir")
        .arg(&env.handoffs)
        .arg("--postmortems-dir")
        .arg(&env.postmortems)
        .env("PYTHONPATH", &env.pypath)
        .env("PATH", path)
        .current_dir(&env.cwd)
        .output()
        .expect("run finalize")
}

const GH_PR_358: &str =
    "#!/bin/sh\necho '{\"number\": 358, \"url\": \"https://github.com/o/r/pull/358\"}'\n";

/// AC1-HP + AC2-HP: a node-driven session with an open PR stamps pr_number even
/// on a NON-ship terminal (DoneAwaitingMerge - the terminal in_review covers).
#[test]
fn finalize_stamps_pr_number_on_nonship() {
    let env = setup("S-stamp", false);
    let out = run_finalize_shimmed(&env, "DoneAwaitingMerge", GH_PR_358);
    assert!(out.status.success(), "stamp path must exit 0");
    let c = calls(&env);
    assert!(
        c.contains(
            "fno backlog update ab-testnode --pr-number 358 --pr-url https://github.com/o/r/pull/358"
        ),
        "finalize must stamp pr_number on a non-ship terminal with an open PR: {c}"
    );
}

/// AC1-FR: a node id but no open PR (gh fails) -> no stamp call, still exit 0.
#[test]
fn finalize_skips_stamp_when_no_pr() {
    let env = setup("S-nopr", false);
    let out = run_finalize_shimmed(&env, "Budget", "#!/bin/sh\nexit 1\n");
    assert!(out.status.success(), "no-PR skip must exit 0");
    assert!(
        !calls(&env).contains("fno backlog update"),
        "no open PR -> no pr_number stamp call"
    );
}

/// AC2-FR: a raw-prose session (graph_node_id null) -> stamp skipped entirely,
/// even though gh would return a PR.
#[test]
fn finalize_skips_stamp_when_no_node() {
    let env = setup("S-nonode", false);
    fs::write(
        &env.state,
        "---\nsession_id: S-nonode\ncreated_at: 2026-06-07T00:00:00Z\ninput: \"x\"\nplan_path: \"plan.md\"\nprovider: claude\n---\n# Target Session State\n",
    )
    .unwrap();
    let out = run_finalize_shimmed(&env, "Budget", GH_PR_358);
    assert!(out.status.success());
    assert!(
        !calls(&env).contains("fno backlog update"),
        "no node id -> no pr_number stamp call"
    );
}
