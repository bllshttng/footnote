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
//! - legacy failures remain non-fatal; generic failures return nonzero to retry.
//! - archived/missing manifest: legacy no-op, generic retry.

use std::fs;
use std::os::unix::fs::PermissionsExt;
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
    bin_dir: PathBuf,
    gh_calls: PathBuf,
    fno_calls: PathBuf,
    outstanding_store: PathBuf,
    pr_info: PathBuf,
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
    let bin_dir = root.join("bin");
    let gh_calls = cwd.join("gh-calls.log");
    fs::create_dir_all(&bin_dir).unwrap();
    let gh = bin_dir.join("gh");
    fs::write(
        &gh,
        "#!/bin/sh\nprintf 'gh %s\\n' \"$*\" >> \"$GH_CALLS_LOG\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

    // `fno` stub (x-32f3 HALF TWO): a tiny python3 double for `fno inbox
    // outstanding --json` / `fno inbox outstanding ask`, keyed off
    // FNO_STUB_STORE / FNO_STUB_CALLS_LOG so a test can assert the filing
    // (and its dedup) without depending on the real Python outstanding store.
    let fno_stub = bin_dir.join("fno");
    fs::write(
        &fno_stub,
        "#!/usr/bin/env python3\n\
         import json, os, sys\n\
         args = sys.argv[1:]\n\
         calls_log = os.environ.get('FNO_STUB_CALLS_LOG')\n\
         if calls_log:\n\
         \x20   open(calls_log, 'a').write('fno ' + ' '.join(args) + '\\n')\n\
         store = os.environ.get('FNO_STUB_OUTSTANDING_STORE')\n\
         if args[:3] == ['inbox', 'outstanding', '--json']:\n\
         \x20   if store and os.path.exists(store):\n\
         \x20       sys.stdout.write(open(store).read())\n\
         \x20   else:\n\
         \x20       sys.stdout.write(json.dumps({'carveouts': {'total': 0, 'by_kind': {}, 'oldest_ts': None}, 'questions': []}))\n\
         \x20   sys.exit(0)\n\
         if args[:3] == ['inbox', 'outstanding', 'ask']:\n\
         \x20   question = args[3] if len(args) > 3 else ''\n\
         \x20   node = args[args.index('--node') + 1] if '--node' in args else None\n\
         \x20   session_id = os.environ.get('FNO_STUB_SESSION_ID', '')\n\
         \x20   data = {'carveouts': {'total': 0, 'by_kind': {}, 'oldest_ts': None}, 'questions': [\n\
         \x20       {'id': 'q-test', 'ts': '2026-01-01T00:00:00Z', 'question': question, 'session_id': session_id, 'cwd': None, 'node': node}\n\
         \x20   ]}\n\
         \x20   if store:\n\
         \x20       open(store, 'w').write(json.dumps(data))\n\
         \x20   print('q-test')\n\
         \x20   sys.exit(0)\n\
         if args[:3] == ['do', 'pr', 'info']:\n\
         \x20   p = os.environ.get('FNO_STUB_PR_INFO', 'pr-info.json')\n\
         \x20   if os.path.exists(p):\n\
         \x20       sys.stdout.write(open(p).read())\n\
         \x20       sys.exit(0)\n\
         \x20   sys.exit(1)\n\
         sys.exit(1)\n",
    )
    .unwrap();
    fs::set_permissions(&fno_stub, fs::Permissions::from_mode(0o755)).unwrap();

    // REST `fno do pr info` payload the stubs serve: finalize resolves the
    // branch PR through the CLI's REST reader now, not gh, so the harness
    // answers from a file a test can delete to play "no open PR".
    let pr_info = cwd.join("pr-info.json");
    fs::write(
        &pr_info,
        "{\"pr\": 358, \"url\": \"https://github.com/o/r/pull/358\", \"state\": \"OPEN\"}",
    )
    .unwrap();

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
    // fno.cli stub: finalize's post-stamp frontmatter validation shells
    // `python3 -m fno.cli do plan validate <path>`; record it and pass, so the
    // call is observable and the loud non-fatal branch stays quiet in a
    // healthy run.
    fs::write(
        pypath.join("fno/cli.py"),
        "import sys\n\
         args = sys.argv[1:]\n\
         if args[:3] == ['do', 'plan', 'validate']:\n\
         \x20    open('calls.log','a').write('plan-validate %s\\n' % ' '.join(args[3:]))\n\
         \x20    sys.exit(0)\n\
         sys.exit(1)\n",
    )
    .unwrap();
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
        bin_dir,
        gh_calls,
        fno_calls: root.join("fno-calls.log"),
        outstanding_store: root.join("outstanding-store.json"),
        pr_info,
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
        .env(
            "PATH",
            format!(
                "{}:{}",
                env.bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GH_CALLS_LOG", &env.gh_calls)
        .current_dir(&env.cwd)
        .output()
        .expect("run finalize")
}

/// Same as `run_finalize`, plus `--transcript` and the `fno` stub's env vars
/// (x-32f3 HALF TWO tests): a separate helper rather than widening
/// `run_finalize`'s signature and rippling through every existing call site.
fn run_finalize_with_transcript(
    env: &Env,
    reason: &str,
    session_id: &str,
    transcript: &Path,
) -> std::process::Output {
    Command::new(BIN)
        .arg("finalize")
        .arg("--state")
        .arg(&env.state)
        .arg("--cwd")
        .arg(&env.cwd)
        .arg("--reason")
        .arg(reason)
        .arg("--transcript")
        .arg(transcript)
        .arg("--events")
        .arg(&env.events)
        .arg("--global-events")
        .arg(&env.global_events)
        .arg("--handoffs-dir")
        .arg(&env.handoffs)
        .arg("--postmortems-dir")
        .arg(&env.postmortems)
        .env("PYTHONPATH", &env.pypath)
        .env(
            "PATH",
            format!(
                "{}:{}",
                env.bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GH_CALLS_LOG", &env.gh_calls)
        .env("FNO_STUB_CALLS_LOG", &env.fno_calls)
        .env("FNO_STUB_OUTSTANDING_STORE", &env.outstanding_store)
        .env("FNO_STUB_SESSION_ID", session_id)
        .current_dir(&env.cwd)
        .output()
        .expect("run finalize with transcript")
}

fn prepare_real_plan_stamp(env: &Env, expected_url_count: Option<u32>) {
    fs::remove_file(env.pypath.join("fno/plan/_stamp.py")).unwrap();
    fs::write(
        env.pypath.join("fno/__init__.py"),
        "from pkgutil import extend_path\n__path__ = extend_path(__path__, __name__)\n",
    )
    .unwrap();
    fs::write(
        env.pypath.join("fno/plan/__init__.py"),
        "from pkgutil import extend_path\n__path__ = extend_path(__path__, __name__)\n",
    )
    .unwrap();
    let expected = expected_url_count
        .map(|count| format!("expected_url_count: {count}\n"))
        .unwrap_or_default();
    fs::write(
        env.cwd.join("plan.md"),
        format!(
            "---\nnode: ab-testnode\nstatus: ready\ncreated: 2026-08-02\n{expected}---\n# Plan\n"
        ),
    )
    .unwrap();
}

fn run_finalize_real_stamp(env: &Env, reason: &str) -> std::process::Output {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let pythonpath = format!(
        "{}:{}",
        env.pypath.display(),
        repo.join("cli/src").display()
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
        .env("PYTHONPATH", pythonpath)
        .env(
            "PATH",
            format!(
                "{}:{}",
                env.bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GH_CALLS_LOG", &env.gh_calls)
        .current_dir(&env.cwd)
        .output()
        .expect("run finalize with real plan stamp")
}

fn write_delivery_verdict(env: &Env, session_id: &str, complete: bool) {
    let requirements = if complete {
        serde_json::json!([{
            "deliverable_id": "output",
            "evidence_id": "artifact-ready",
            "subject_kind": "artifact",
            "subject_id": "artifact-1",
            "result": "passed",
            "producers": ["adapter:test"],
            "source_revisions": ["artifact-sha"],
            "diagnostics": []
        }])
    } else {
        serde_json::json!([])
    };
    fs::write(
        &env.events,
        serde_json::json!({
            "ts": "2026-08-02T12:00:00Z",
            "type": "delivery_verdict_evaluated",
            "source": "target",
            "data": {
                "evaluator_version": "delivery-evaluator.v1",
                "session_id": session_id,
                "work_order_node_id": "ab-testnode",
                "attempt_id": "attempt-1",
                "aggregate": "passed",
                "fact_revision": "sha256:abc",
                "required_requirements": [{
                    "deliverable_id": "output",
                    "evidence_id": "artifact-ready"
                }],
                "requirements": requirements,
                "diagnostics": []
            }
        })
        .to_string()
            + "\n",
    )
    .unwrap();
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

#[test]
fn finalize_binary_closes_the_shadow_run_after_success() {
    let run = "20260823T060900Z-cx73523-e04109";
    let env = setup(run, false);
    let run_log = env.cwd.join(".fno/run-log.jsonl");
    fno_agents::run_state::append_transition(
        &run_log,
        run,
        fno_agents::run_state::RunEvent::DispatchClassified,
    )
    .unwrap();
    fno_agents::run_state::append_transition(
        &run_log,
        run,
        fno_agents::run_state::RunEvent::TerminalDecided,
    )
    .unwrap();

    let out = run_finalize(&env, "DonePRGreen");
    assert!(out.status.success(), "finalize must succeed: {:?}", out);
    assert_eq!(
        fno_agents::run_state::fold_run_state(&run_log, run).unwrap(),
        fno_agents::run_state::RunState::Closed
    );
}

#[test]
fn finalize_binary_repairs_a_prior_ship_before_returning() {
    let run = "20260823T060900Z-cx73523-e04109";
    let env = setup(run, false);
    let run_log = env.cwd.join(".fno/run-log.jsonl");
    fno_agents::run_state::append_transition(
        &run_log,
        run,
        fno_agents::run_state::RunEvent::DispatchClassified,
    )
    .unwrap();
    fno_agents::run_state::append_transition(
        &run_log,
        run,
        fno_agents::run_state::RunEvent::TerminalDecided,
    )
    .unwrap();
    fs::write(
        &env.events,
        format!(
            "{{\"type\":\"session_finalized\",\"data\":{{\"session_id\":\"{run}\",\"ship\":true}}}}\n"
        ),
    )
    .unwrap();

    let out = run_finalize(&env, "DonePRGreen");
    assert!(
        out.status.success(),
        "repair finalize must succeed: {:?}",
        out
    );
    assert_eq!(
        fno_agents::run_state::fold_run_state(&run_log, run).unwrap(),
        fno_agents::run_state::RunState::Closed
    );
}

/// x-8fc0: a ship reason now gets a completion eval too (the trigger fires on
/// every reason but NoWork), but its body is the lighter eval prose, never
/// the stuck-triage "(stuck: exited without shipping)" wording. NoWork is the
/// sole exclusion: nothing happened, so nothing is written.
#[test]
fn finalize_completion_eval_on_ship_not_benign() {
    let ship = setup("S-noprm-ship", false);
    assert!(run_finalize(&ship, "DonePRGreen").status.success());
    let pms = postmortem_files(&ship);
    assert_eq!(pms.len(), 1, "ship reason must write a completion eval");
    let pm = fs::read_to_string(&pms[0]).unwrap();
    assert!(
        pm.starts_with("# Completion eval:"),
        "ship reason gets the eval prose, not the stuck-postmortem prose: {pm}"
    );
    assert!(
        !pm.contains("stuck: exited without shipping"),
        "ship reason must not carry stuck-triage wording: {pm}"
    );
    let benign = setup("S-noprm-nowork", false);
    assert!(run_finalize(&benign, "NoWork").status.success());
    assert!(
        postmortem_files(&benign).is_empty(),
        "NoWork is benign, must NOT write a completion eval"
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

/// A ship reason runs ledger + stamp + handoff and emits session_finalized.
/// Ship stamps `in_review` only; it does NOT graduate (done = merged, x-f34f). (AC5-HP.)
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
    assert!(
        c.contains("plan-validate"),
        "canonical do plan validation must run after stamp: {c}"
    );
    assert!(
        !c.contains("stamp-plan graduate"),
        "graduate must NOT fire at ship (done = merged): {c}"
    );
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

/// An advisory ship (DoneAdvisory) has no merge event, so ship IS its
/// completion: it stamps AND graduates the plan to done, unlike a code ship
/// (DonePRGreen) which stamps `in_review` only and flips at merge (codex P2, x-f34f).
#[test]
fn finalize_advisory_ship_graduates() {
    let env = setup("S-adv", false);
    let out = run_finalize(&env, "DoneAdvisory");
    assert!(out.status.success());
    let c = calls(&env);
    assert!(c.contains("stamp-plan stamp"), "advisory ship stamps: {c}");
    assert!(
        c.contains("stamp-plan graduate"),
        "advisory ship graduates to done (no merge event to flip it): {c}"
    );
}

#[test]
fn generic_completion_finalize_consumes_selected_verdict_without_pr_paths() {
    let env = setup("S-delivery", false);
    fs::write(
        &env.events,
        serde_json::json!({
            "ts": "2026-08-02T12:00:00Z",
            "type": "delivery_verdict_evaluated",
            "source": "target",
            "data": {
                "evaluator_version": "delivery-evaluator.v1",
                "session_id": "S-delivery",
                "work_order_node_id": "ab-testnode",
                "attempt_id": "attempt-1",
                "aggregate": "passed",
                "fact_revision": "sha256:abc",
                "required_requirements": [{
                    "deliverable_id": "output",
                    "evidence_id": "artifact-ready"
                }],
                "requirements": [{
                    "deliverable_id": "output",
                    "evidence_id": "artifact-ready",
                    "subject_kind": "artifact",
                    "subject_id": "artifact-1",
                    "result": "passed",
                    "producers": ["adapter:test"],
                    "source_revisions": ["artifact-sha"],
                    "diagnostics": []
                }],
                "diagnostics": []
            }
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let out = run_finalize(&env, "DoneDelivery");

    assert!(out.status.success());
    let c = calls(&env);
    assert!(c.contains("register-task reason=DoneDelivery"));
    assert!(c.contains("stamp-plan stamp"));
    assert!(c.contains("stamp-plan graduate"));
    assert!(!c.contains("verify-advise"));
    let handoff = fs::read_to_string(&handoff_files(&env)[0]).unwrap();
    assert!(handoff.contains("fno-delivery://ab-testnode/attempt-1/sha256:abc"));
    assert!(handoff.contains("generic delivery receipt"));
    assert!(fs::read_to_string(&env.gh_calls)
        .unwrap_or_default()
        .is_empty());
    assert_eq!(count_event(&env.events, "termination", "S-delivery"), 1);
    assert!(events_text(&env.events).contains("DoneDelivery"));
}

#[test]
fn generic_completion_finalize_rejects_incomplete_selected_verdict() {
    let env = setup("S-delivery-incomplete", false);
    fs::write(
        &env.events,
        serde_json::json!({
            "ts": "2026-08-02T12:00:00Z",
            "type": "delivery_verdict_evaluated",
            "source": "target",
            "data": {
                "evaluator_version": "delivery-evaluator.v1",
                "session_id": "S-delivery-incomplete",
                "work_order_node_id": "ab-testnode",
                "attempt_id": "attempt-1",
                "aggregate": "passed",
                "fact_revision": "sha256:abc",
                "required_requirements": [{
                    "deliverable_id": "output",
                    "evidence_id": "artifact-ready"
                }],
                "requirements": [],
                "diagnostics": []
            }
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let out = run_finalize(&env, "DoneDelivery");

    assert!(!out.status.success());
    assert!(!calls(&env).contains("stamp-plan"));
    assert!(handoff_files(&env).is_empty());
    assert!(events_text(&env.events).contains("delivery_receipt"));
    assert_eq!(
        count_event(&env.events, "termination", "S-delivery-incomplete"),
        0
    );
}

#[test]
fn generic_completion_finalize_does_not_revive_an_older_passing_verdict() {
    let env = setup("S-delivery-newest", false);
    write_delivery_verdict(&env, "S-delivery-newest", true);
    let mut events = events_text(&env.events);
    let mut newer: serde_json::Value =
        serde_json::from_str(events.lines().next().unwrap()).unwrap();
    newer["ts"] = serde_json::json!("2026-08-02T12:01:00Z");
    newer["data"]["requirements"] = serde_json::json!([]);
    events.push_str(&newer.to_string());
    events.push('\n');
    fs::write(&env.events, events).unwrap();

    let out = run_finalize(&env, "DoneDelivery");

    assert!(!out.status.success());
    assert!(!calls(&env).contains("stamp-plan"));
    assert!(handoff_files(&env).is_empty());
    assert!(events_text(&env.events).contains("delivery_receipt"));
    assert_eq!(
        count_event(&env.events, "termination", "S-delivery-newest"),
        0
    );
}

#[test]
fn generic_completion_finalize_rejects_a_newest_unbound_verdict() {
    let env = setup("S-delivery-unbound", false);
    write_delivery_verdict(&env, "S-delivery-unbound", true);
    let mut events = events_text(&env.events);
    let mut newer: serde_json::Value =
        serde_json::from_str(events.lines().next().unwrap()).unwrap();
    newer["ts"] = serde_json::json!("2026-08-02T12:01:00Z");
    newer["data"].as_object_mut().unwrap().remove("session_id");
    events.push_str(&newer.to_string());
    events.push('\n');
    fs::write(&env.events, events).unwrap();

    let out = run_finalize(&env, "DoneDelivery");

    assert!(!out.status.success());
    assert!(!calls(&env).contains("stamp-plan"));
    assert!(handoff_files(&env).is_empty());
    assert_eq!(
        count_event(&env.events, "termination", "S-delivery-unbound"),
        0
    );
}

#[test]
fn generic_completion_finalize_real_stamp_reaches_done_with_bound_receipt() {
    let env = setup("S-delivery-real", false);
    prepare_real_plan_stamp(&env, None);
    write_delivery_verdict(&env, "S-delivery-real", true);

    let out = run_finalize_real_stamp(&env, "DoneDelivery");

    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan = fs::read_to_string(env.cwd.join("plan.md")).unwrap();
    assert!(plan.contains("status: done"), "{plan}");
    assert!(
        plan.contains("fno-delivery://ab-testnode/attempt-1/sha256:abc"),
        "{plan}"
    );
    assert!(plan.contains("S-delivery-real"), "{plan}");
}

#[test]
fn generic_completion_finalize_real_stamp_respects_declared_url_count() {
    let env = setup("S-delivery-count", false);
    prepare_real_plan_stamp(&env, Some(2));
    write_delivery_verdict(&env, "S-delivery-count", true);

    let out = run_finalize_real_stamp(&env, "DoneDelivery");

    assert!(out.status.success());
    let plan = fs::read_to_string(env.cwd.join("plan.md")).unwrap();
    assert!(plan.contains("status: in_review"), "{plan}");
    assert!(plan.contains("expected_url_count: 2"), "{plan}");
    assert!(!plan.contains("status: done"), "{plan}");
}

#[test]
fn generic_completion_finalize_real_stamp_rejects_incomplete_receipt() {
    let env = setup("S-delivery-real-bad", false);
    prepare_real_plan_stamp(&env, None);
    write_delivery_verdict(&env, "S-delivery-real-bad", false);

    let out = run_finalize_real_stamp(&env, "DoneDelivery");

    assert!(!out.status.success());
    let plan = fs::read_to_string(env.cwd.join("plan.md")).unwrap();
    assert!(plan.contains("status: ready"), "{plan}");
    assert!(!plan.contains("fno-delivery://"), "{plan}");
}

#[test]
fn generic_completion_finalize_missing_selected_event_fails_closed() {
    let env = setup("S-delivery-missing", false);

    let out = run_finalize(&env, "DoneDelivery");

    assert!(!out.status.success());
    assert!(!calls(&env).contains("stamp-plan"));
    assert!(handoff_files(&env).is_empty());
    assert_eq!(
        count_event(&env.events, "session_finalize_failed", "S-delivery-missing"),
        1
    );
    assert!(events_text(&env.events).contains("delivery_receipt"));
    assert_eq!(
        count_event(&env.events, "termination", "S-delivery-missing"),
        0
    );
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

#[test]
fn generic_finalize_missing_or_unbound_manifest_requires_retry() {
    let missing = setup("S-generic-gone", false);
    fs::remove_file(&missing.state).unwrap();
    assert!(!run_finalize(&missing, "DoneDelivery").status.success());

    for session_line in ["", "session_id: ''\n"] {
        let unbound = setup("S-generic-unbound", false);
        fs::write(
            &unbound.state,
            format!(
                "---\n{session_line}created_at: 2026-08-02T12:00:00Z\nattended: true\n---\ngraph_node_id: ab-testnode\n"
            ),
        )
        .unwrap();
        assert!(!run_finalize(&unbound, "DoneDelivery").status.success());
        assert_eq!(
            count_event(&unbound.events, "termination", "S-generic-unbound"),
            0
        );
    }
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
        !c.contains("stamp-plan graduate"),
        "ship fire stamps only; done = merged, no graduate (x-f34f): {c}"
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
    // The quota adapter finalize shells for review evidence carries gh-shaped
    // argv (`fno-gh-coverage pr view --json reviews,comments`), so the same
    // body answers it; without the adapter on PATH every optional-app read
    // failed closed as a spawn error and the evidence cases never ran.
    write_shim(&bin, "fno-gh-coverage", gh_body);
    // `fno` records its argv and answers the REST PR read from the payload
    // file (FNO_STUB_PR_INFO below); a missing file plays "no open PR".
    write_shim(
        &bin,
        "fno",
        "#!/bin/sh\n\
         echo \"fno $*\" >> calls.log\n\
         case \"$*\" in\n\
         \x20\x20 'do pr info'*)\n\
         \x20\x20   if [ -f \"$FNO_STUB_PR_INFO\" ]; then cat \"$FNO_STUB_PR_INFO\"; exit 0; fi\n\
         \x20\x20   exit 1 ;;\n\
         esac\n",
    );
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
        // Pin the config chain to this temp project. `$FNO_CONFIG` is
        // the SOLE source when set, so without this a developer who exports it
        // gets a run that never reads the project config.toml a test just wrote,
        // and one who does not gets the real `~/.fno/config.toml`. Either way the
        // auto-merge argv assertions would pass or fail for the wrong reason. The
        // file need not exist: an unreadable candidate yields the same defaults.
        .env("FNO_CONFIG", env.cwd.join(".fno/config.toml"))
        .env("FNO_STUB_PR_INFO", &env.pr_info)
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

/// AC1-FR: a node id but no open PR (the REST reader fails) -> no stamp call,
/// still exit 0.
#[test]
fn finalize_skips_stamp_when_no_pr() {
    let env = setup("S-nopr", false);
    fs::remove_file(&env.pr_info).unwrap();
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

// ── arm auto-merge at the green gate, not at PR creation (x-1951) ────────────
// The unit tests cover the `should_arm_auto_merge` predicate; these prove the
// WIRING - that a real finalize run actually reaches `gh pr merge --auto` on an
// approved green terminal and never on any other. A predicate tested only in
// isolation is the decorative-guard shape: correct, and never called.

/// Like GH_PR_358 but records every argv, so a `pr merge` call is observable.
const GH_PR_358_LOGGING: &str = "#!/bin/sh\n\
     echo \"gh $*\" >> calls.log\n\
     case \"$2\" in view) echo '{\"number\": 358, \"url\": \"https://github.com/o/r/pull/358\"}' ;; esac\n";

const GH_OPTIONAL_REVIEWED: &str = "#!/bin/sh\n\
     echo \"gh $*\" >> calls.log\n\
     case \"$*\" in\n\
       *reviews,comments*) echo '{\"headRefOid\":\"abc123def456abc123def456abc123def456abc1\",\"baseRefName\":\"main\",\"reviews\":[{\"author\":{\"login\":\"chatgpt-codex-connector[bot]\"},\"state\":\"COMMENTED\",\"commit\":{\"oid\":\"abc123def456abc123def456abc123def456abc1\"}}],\"comments\":[]}' ;;\n\
       *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
     esac\n";

const GH_OPTIONAL_OUTSTANDING: &str = "#!/bin/sh\n\
     echo \"gh $*\" >> calls.log\n\
     case \"$*\" in\n\
       *reviews,comments*) echo '{\"headRefOid\":\"abc123def456abc123def456abc123def456abc1\",\"baseRefName\":\"main\",\"reviews\":[],\"comments\":[]}' ;;\n\
       *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
     esac\n";

const GH_OPTIONAL_USAGE_LIMITED: &str = "#!/bin/sh\n\
     echo \"gh $*\" >> calls.log\n\
     case \"$*\" in\n\
       *reviews,comments*) echo '{\"headRefOid\":\"abc123def456abc123def456abc123def456abc1\",\"baseRefName\":\"main\",\"reviews\":[],\"comments\":[{\"author\":{\"login\":\"chatgpt-codex-connector[bot]\"},\"body\":\"You have reached your Codex usage limits for code reviews\"}]}' ;;\n\
       *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
     esac\n";

const GH_OPTIONAL_REVIEWED_AFTER_USAGE_LIMIT: &str = "#!/bin/sh\n\
     echo \"gh $*\" >> calls.log\n\
     case \"$*\" in\n\
       *reviews,comments*) echo '{\"headRefOid\":\"abc123def456abc123def456abc123def456abc1\",\"baseRefName\":\"main\",\"reviews\":[{\"author\":{\"login\":\"chatgpt-codex-connector[bot]\"},\"state\":\"COMMENTED\",\"commit\":{\"oid\":\"abc123def456abc123def456abc123def456abc1\"}}],\"comments\":[{\"author\":{\"login\":\"chatgpt-codex-connector[bot]\"},\"body\":\"You have reached your Codex usage limits for code reviews\"}]}' ;;\n\
       *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
     esac\n";

const GH_OPTIONAL_READ_FAILS: &str = "#!/bin/sh\n\
     echo \"gh $*\" >> calls.log\n\
     case \"$*\" in\n\
       *reviews,comments*) echo 'review API unavailable' >&2; exit 1 ;;\n\
       *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
     esac\n";

const GH_OPTIONAL_MALFORMED: &str = "#!/bin/sh\n\
     echo \"gh $*\" >> calls.log\n\
     case \"$*\" in\n\
       *reviews,comments*) echo '{}' ;;\n\
       *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
     esac\n";

/// Rewrite the manifest with an explicit merge posture, keeping the node id.
fn set_posture(env: &Env, session_id: &str, approved: bool) {
    fs::write(
        &env.state,
        format!(
            "---\n\
             session_id: {session_id}\n\
             created_at: 2026-06-07T00:00:00Z\n\
             input: \"ab-test feature\"\n\
             plan_path: \"plan.md\"\n\
             provider: claude\n\
             auto_merge_approved: {approved}\n\
             auto_merge_source: config\n\
             claude_transcript_id: tid-{session_id}\n\
             ---\n\
             # Target Session State\n\
             graph_node_id: ab-testnode\n"
        ),
    )
    .unwrap();
}

/// AC2-HP: an approved posture on the one green-and-reviewed terminal arms
/// GitHub's native auto-merge, at the gate rather than at PR creation.
#[test]
fn finalize_arms_auto_merge_on_approved_green_terminal() {
    let env = setup("S-arm", false);
    set_posture(&env, "S-arm", true);
    write_auto_merge_config(&env, "[auto_merge]\nenabled = true\n");
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_PR_358_LOGGING);
    assert!(
        out.status.success(),
        "arming must never raise the exit code"
    );
    let c = calls(&env);
    assert!(
        c.contains("gh pr merge 358 --auto --merge"),
        "approved DonePRGreen must arm auto-merge: {c}"
    );
    assert!(
        !c.contains("reviews,comments"),
        "empty optional_apps must add no evidence read: {c}"
    );
}

/// Write a `config.auto_merge` block into the temp project. `run_finalize_shimmed`
/// pins `$FNO_CONFIG` here, so this is the sole config the child reads.
fn write_auto_merge_config(env: &Env, body: &str) {
    fs::write(env.cwd.join(".fno/config.toml"), body).unwrap();
}

fn configure_optional_codex(env: &Env) {
    write_auto_merge_config(
        env,
        "[auto_merge]\nenabled = true\n\
         [review]\noptional_apps = [\"chatgpt-codex-connector\"]\n",
    );
}

fn finalized_event(env: &Env, session_id: &str) -> serde_json::Value {
    events_text(&env.events)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| {
            event.get("type").and_then(|v| v.as_str()) == Some("session_finalized")
                && event.pointer("/data/session_id").and_then(|v| v.as_str()) == Some(session_id)
        })
        .expect("session_finalized event")
}

#[test]
fn finalize_arms_when_configured_optional_app_reviewed() {
    let env = setup("S-optional-reviewed", false);
    set_posture(&env, "S-optional-reviewed", true);
    configure_optional_codex(&env);
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_OPTIONAL_REVIEWED);
    assert!(out.status.success());
    let c = calls(&env);
    assert!(c.contains("--json reviews,comments"), "evidence read: {c}");
    assert!(c.contains("gh pr merge 358 --auto --merge"), "arm: {c}");
    let event = finalized_event(&env, "S-optional-reviewed");
    assert_eq!(event.pointer("/data/auto_merge_armed"), Some(&true.into()));
    // x-9d11: the terminal event names which input set the posture.
    assert_eq!(
        event.pointer("/data/auto_merge_source"),
        Some(&"config".into())
    );
    assert!(event.pointer("/data/auto_merge_blocked_reason").is_none());
}

#[test]
fn finalize_event_source_reads_unknown_on_pre_provenance_manifest() {
    // AC4-ERR: a manifest predating auto_merge_source must surface `unknown`,
    // never a guessed origin. setup()'s stock manifest carries no source line.
    let env = setup("S-nosource", false);
    let out = run_finalize_shimmed(&env, "DoneAdvisory", GH_PR_358_LOGGING);
    assert!(out.status.success());
    let event = finalized_event(&env, "S-nosource");
    assert_eq!(
        event.pointer("/data/auto_merge_source"),
        Some(&"unknown".into())
    );
}

#[test]
fn finalize_withholds_arm_when_optional_review_outstanding() {
    let env = setup("S-optional-outstanding", false);
    set_posture(&env, "S-optional-outstanding", true);
    configure_optional_codex(&env);
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_OPTIONAL_OUTSTANDING);
    assert!(out.status.success());
    assert!(!calls(&env).contains("gh pr merge"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("optional-review-outstanding:chatgpt-codex-connector"),
        "stderr: {stderr}"
    );
    let event = finalized_event(&env, "S-optional-outstanding");
    assert_eq!(event.pointer("/data/auto_merge_armed"), Some(&false.into()));
    assert_eq!(
        event
            .pointer("/data/auto_merge_blocked_reason")
            .and_then(|v| v.as_str()),
        Some("optional-review-outstanding:chatgpt-codex-connector")
    );
}

#[test]
fn finalize_usage_limit_comment_is_not_clean_optional_review() {
    let env = setup("S-optional-limited", false);
    set_posture(&env, "S-optional-limited", true);
    configure_optional_codex(&env);
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_OPTIONAL_USAGE_LIMITED);
    assert!(out.status.success());
    assert!(!calls(&env).contains("gh pr merge"));
    let event = finalized_event(&env, "S-optional-limited");
    assert_eq!(
        event
            .pointer("/data/auto_merge_blocked_reason")
            .and_then(|v| v.as_str()),
        Some("optional-review-usage-limited:chatgpt-codex-connector")
    );
}

#[test]
fn finalize_arms_when_coverage_satisfied_despite_usage_limited_optional() {
    // x-0eaf: a covered review_coverage event (a local lane reviewed) overrides
    // the usage-limited optional-app withhold, so a quota-dead bot cannot wedge
    // the autonomous merge when a local lane covered the diff.
    let env = setup("S-optional-limited-covered", false);
    set_posture(&env, "S-optional-limited-covered", true);
    configure_optional_codex(&env);
    let covered = "{\"ts\":\"2026-08-05T12:00:00Z\",\"type\":\"review_coverage\",\
\"source\":\"hook\",\"data\":{\"pr\":358,\"coverage\":\"covered\",\
\"reviewed_count\":1,\"head_sha\":\"abc\"}}\n";
    fs::write(&env.events, covered).unwrap();
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_OPTIONAL_USAGE_LIMITED);
    assert!(out.status.success());
    assert!(
        calls(&env).contains("gh pr merge 358 --auto --merge"),
        "covered event must override the usage-limited optional withhold: {}",
        calls(&env)
    );
    let event = finalized_event(&env, "S-optional-limited-covered");
    assert_eq!(event.pointer("/data/auto_merge_armed"), Some(&true.into()));
    assert!(event.pointer("/data/auto_merge_blocked_reason").is_none());
}

#[test]
fn finalize_completed_review_wins_over_stale_usage_limit_comment() {
    let env = setup("S-optional-recovered", false);
    set_posture(&env, "S-optional-recovered", true);
    configure_optional_codex(&env);
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_OPTIONAL_REVIEWED_AFTER_USAGE_LIMIT);
    assert!(out.status.success());
    assert!(calls(&env).contains("gh pr merge 358 --auto --merge"));
    let event = finalized_event(&env, "S-optional-recovered");
    assert_eq!(event.pointer("/data/auto_merge_armed"), Some(&true.into()));
    assert!(event.pointer("/data/auto_merge_blocked_reason").is_none());
}

#[test]
fn finalize_live_auto_merge_switch_vetoes_an_approved_run() {
    let env = setup("S-live-switch-off", false);
    set_posture(&env, "S-live-switch-off", true);
    write_auto_merge_config(&env, "[auto_merge]\nenabled = true\n");
    write_auto_merge_config(&env, "[auto_merge]\nenabled = false\n");

    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_PR_358_LOGGING);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("config.auto_merge.enabled=false"),
        "stderr must name the live-switch veto: {stderr}"
    );
    let event = finalized_event(&env, "S-live-switch-off");
    assert_eq!(event.pointer("/data/auto_merge_armed"), Some(&false.into()));
    assert_eq!(
        event
            .pointer("/data/auto_merge_blocked_reason")
            .and_then(|v| v.as_str()),
        Some("config.auto_merge.enabled=false")
    );
}

#[test]
fn finalize_optional_review_read_failure_withholds_arm() {
    for (session_id, gh) in [
        ("S-optional-read-failed", GH_OPTIONAL_READ_FAILS),
        ("S-optional-malformed", GH_OPTIONAL_MALFORMED),
    ] {
        let env = setup(session_id, false);
        set_posture(&env, session_id, true);
        configure_optional_codex(&env);
        let out = run_finalize_shimmed(&env, "DonePRGreen", gh);
        assert!(out.status.success());
        assert!(!calls(&env).contains("gh pr merge"));
        let event = finalized_event(&env, session_id);
        assert_eq!(
            event
                .pointer("/data/auto_merge_blocked_reason")
                .and_then(|v| v.as_str()),
            Some("optional-review-read-failed")
        );
    }
}

/// Round 3, PR 917: a review object on a commit that is not the PR head is
/// Stale under the shared predicate, and arming on it re-arms the post-green
/// race this gate exists to close. The old scan counted any non-empty state
/// review regardless of commit, so finalize armed where loop-check's coverage
/// refused - the reader-divergence class. The cwd is not a git repo, so the
/// resolver's non-exact-head arm fails closed to Stale, which is precisely the
/// behavior under test.
#[test]
fn finalize_withholds_arm_when_optional_review_is_stale() {
    let env = setup("S-optional-stale", false);
    set_posture(&env, "S-optional-stale", true);
    configure_optional_codex(&env);
    let gh = "#!/bin/sh\n\
         echo \"gh $*\" >> calls.log\n\
         case \"$*\" in\n\
           *reviews,comments*) echo '{\"headRefOid\":\"abc123def456abc123def456abc123def456abc1\",\"baseRefName\":\"main\",\"reviews\":[{\"author\":{\"login\":\"chatgpt-codex-connector[bot]\"},\"state\":\"COMMENTED\",\"commit\":{\"oid\":\"9999999999999999999999999999999999999999\"}}],\"comments\":[]}' ;;\n\
           *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
         esac\n";
    let out = run_finalize_shimmed(&env, "DonePRGreen", gh);
    assert!(out.status.success());
    assert!(!calls(&env).contains("gh pr merge"));
    let event = finalized_event(&env, "S-optional-stale");
    assert_eq!(
        event
            .pointer("/data/auto_merge_blocked_reason")
            .and_then(|v| v.as_str()),
        Some("optional-review-stale:chatgpt-codex-connector")
    );
}

/// Round 3, PR 917: the clean-pass comment lane. Codex posts its clean pass as
/// a pinned ISSUE comment, not a review object (measured PR #947); the old
/// scan never read comments for a pass, so a flawlessly-reviewed PR withheld
/// arming forever while loop-check counted the same comment. The pinned sha
/// equals the PR head, so the pass is Fresh without a git call.
#[test]
fn finalize_arms_when_optional_clean_pass_comment_pins_the_head() {
    let env = setup("S-optional-clean-pass", false);
    set_posture(&env, "S-optional-clean-pass", true);
    configure_optional_codex(&env);
    let gh = "#!/bin/sh\n\
         echo \"gh $*\" >> calls.log\n\
         case \"$*\" in\n\
           *reviews,comments*) echo '{\"headRefOid\":\"abc123def456abc123def456abc123def456abc1\",\"baseRefName\":\"main\",\"reviews\":[],\"comments\":[{\"author\":{\"login\":\"chatgpt-codex-connector[bot]\"},\"createdAt\":\"2026-08-17T00:00:00Z\",\"body\":\"Codex Review: Didn’t find any major issues. Bravo. Reviewed commit: abc123def456abc123def456abc123def456abc1\"}]}' ;;\n\
           *'pr view'*) echo '{\"number\":358,\"url\":\"https://github.com/o/r/pull/358\"}' ;;\n\
         esac\n";
    let out = run_finalize_shimmed(&env, "DonePRGreen", gh);
    assert!(out.status.success());
    assert!(
        calls(&env).contains("gh pr merge 358 --auto --merge"),
        "a pinned clean-pass comment must satisfy the optional check: {}",
        calls(&env)
    );
    let event = finalized_event(&env, "S-optional-clean-pass");
    assert_eq!(event.pointer("/data/auto_merge_armed"), Some(&true.into()));
    assert!(event.pointer("/data/auto_merge_blocked_reason").is_none());
}

/// The configured merge strategy reaches the argv. Before this, `--merge`
/// was hardcoded, so a squash-only repo was armed with a method it forbids -
/// and since arming is log-only, GitHub's rejection was one stderr line and
/// auto-merge simply never worked.
#[test]
fn finalize_arms_with_the_configured_merge_strategy() {
    for strategy in ["squash", "rebase"] {
        let env = setup("S-strategy", false);
        set_posture(&env, "S-strategy", true);
        write_auto_merge_config(
            &env,
            &format!("[auto_merge]\nenabled = true\nmerge_strategy = \"{strategy}\"\n"),
        );
        let out = run_finalize_shimmed(&env, "DonePRGreen", GH_PR_358_LOGGING);
        assert!(out.status.success());
        let c = calls(&env);
        assert!(
            c.contains(&format!("--{strategy}")),
            "configured {strategy} must reach the argv: {c}"
        );
        assert!(
            !c.contains("--merge"),
            "the hardcoded --merge must not survive a {strategy} config: {c}"
        );
    }
}

/// An out-of-allowlist value degrades to `--merge` rather than reaching
/// `gh` as an unknown flag. Mirrors the bash and Pydantic coercers.
#[test]
fn finalize_arms_with_merge_on_an_invalid_strategy() {
    let env = setup("S-badstrategy", false);
    set_posture(&env, "S-badstrategy", true);
    write_auto_merge_config(
        &env,
        "[auto_merge]\nenabled = true\nmerge_strategy = \"octopus\"\n",
    );
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_PR_358_LOGGING);
    assert!(out.status.success());
    let c = calls(&env);
    assert!(c.contains("--merge"), "invalid -> merge fallback: {c}");
    assert!(!c.contains("--octopus"), "never pass through to gh: {c}");
}

/// x-9d11: the arm NEVER carries --delete-branch, in either config state.
/// gh exits right after queueing and GitHub's server-side merge deletes
/// nothing, so the flag bought nothing at arm time - while its LOCAL delete
/// attempt was the x-7267 false-failure shape. Remote cleanup after a
/// `fno do pr merge` merge is this verb's post-merge step, not an arm flag.
#[test]
fn finalize_arm_never_carries_delete_branch() {
    for body in [
        "[auto_merge]\nenabled = true\n",
        "[auto_merge]\nenabled = true\ndelete_branch_on_merge = true\n",
        "[auto_merge]\nenabled = true\ndelete_branch_on_merge = false\n",
    ] {
        let env = setup("S-delbr", false);
        set_posture(&env, "S-delbr", true);
        write_auto_merge_config(&env, body);
        let out = run_finalize_shimmed(&env, "DonePRGreen", GH_PR_358_LOGGING);
        assert!(out.status.success());
        let c = calls(&env);
        assert!(
            !c.contains("--delete-branch"),
            "the arm must never carry --delete-branch ({body}): {c}"
        );
    }
}

/// AC6-EDGE: a refused posture is never overridden, so the human-merge path is
/// not taxed at all. This is the assertion that catches an inverted gate.
#[test]
fn finalize_never_arms_auto_merge_when_posture_refuses() {
    let env = setup("S-noarm", false);
    set_posture(&env, "S-noarm", false);
    configure_optional_codex(&env);
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_PR_358_LOGGING);
    assert!(out.status.success());
    assert!(
        !calls(&env).contains("gh pr merge"),
        "a refused posture must never arm, even on a green terminal"
    );
    assert!(
        !calls(&env).contains("reviews,comments"),
        "a refused posture must not pay for optional-review evidence"
    );
}

/// AC7-EDGE: a manifest with no posture key at all (minted before the field
/// existed) defaults to refusing - absence must never manufacture a grant.
#[test]
fn finalize_never_arms_auto_merge_without_a_posture_key() {
    let env = setup("S-nokey", false);
    write_auto_merge_config(&env, "[auto_merge]\nenabled = true\n");
    let out = run_finalize_shimmed(&env, "DonePRGreen", GH_PR_358_LOGGING);
    assert!(out.status.success());
    assert!(
        !calls(&env).contains("gh pr merge"),
        "an absent auto_merge_approved must default to no grant"
    );
}

/// The other terminals never arm, however approving the posture. `DoneAdvisory`
/// is the other SHIP_REASONS member (a doc ship with no PR) and
/// `DoneAwaitingMerge` is by definition a merge a human performs, so arming
/// either would merge something no gate greened.
#[test]
fn finalize_never_arms_auto_merge_on_a_non_green_terminal() {
    for reason in ["DoneAdvisory", "DoneAwaitingMerge", "Budget", "NoProgress"] {
        let env = setup("S-other", false);
        set_posture(&env, "S-other", true);
        write_auto_merge_config(&env, "[auto_merge]\nenabled = true\n");
        let out = run_finalize_shimmed(&env, reason, GH_PR_358_LOGGING);
        assert!(out.status.success(), "{reason} must exit 0");
        assert!(
            !calls(&env).contains("gh pr merge"),
            "{reason} must never arm auto-merge"
        );
    }
}

// ── x-cdc7 HALF ONE: WIP-commit at every terminal (e2e) ─────────────────────

/// The specimen this fix exists for, at the wiring level: a worker dies
/// mid-flight holding uncommitted work. Assert the work is ON THE BRANCH
/// after `finalize` runs, not merely that some function returned a sha.
#[test]
fn finalize_wip_commits_a_dirty_worktree_at_any_terminal() {
    let env = setup("S-wip", false);
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&env.cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
    };
    git(&["init", "-q", "."]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    // `.fno/` is gitignored in the real repo (session state, never committed);
    // mirror that here so finalize's OWN bookkeeping writes (events.jsonl,
    // the gh/fno stub call logs) don't read as leftover dirt from the rescue
    // commit's point of view.
    fs::write(
        env.cwd.join(".gitignore"),
        ".fno/\ncalls.log\ngh-calls.log\n",
    )
    .unwrap();
    fs::write(env.cwd.join("committed.txt"), "base").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "base"]);
    // Dirty the tree exactly like a killed worker would: a modification plus
    // a brand-new untracked file.
    fs::write(env.cwd.join("committed.txt"), "changed mid-flight").unwrap();
    fs::write(env.cwd.join("in_flight.txt"), "950 insertions worth").unwrap();

    let out = run_finalize(&env, "NoProgress");
    assert!(out.status.success(), "{out:?}");

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&env.cwd)
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "the worktree must be clean after a rescue commit: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    let log = Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(&env.cwd)
        .output()
        .unwrap();
    let subject = String::from_utf8_lossy(&log.stdout);
    assert!(
        subject.contains("WIP") && subject.contains("NoProgress"),
        "{subject}"
    );
}

/// Every OTHER test in this file runs `finalize` against `env.cwd` with no
/// `.git` at all - this asserts that is a deliberate no-op (finalize still
/// exits 0, no repo is ever created), not an untested gap.
#[test]
fn finalize_no_wip_commit_when_cwd_is_not_a_git_worktree() {
    let env = setup("S-nowip", false);
    fs::write(env.cwd.join("stray.txt"), "not a git repo").unwrap();
    let out = run_finalize(&env, "NoProgress");
    assert!(out.status.success());
    assert!(!env.cwd.join(".git").exists());
}

// ── x-32f3 HALF TWO: mandatory outstanding-question filing (e2e) ───────────

fn write_transcript(path: &Path, last_message: &str) {
    let line = serde_json::json!({"role": "assistant", "content": last_message}).to_string();
    fs::write(path, line + "\n").unwrap();
}

fn fno_calls(env: &Env) -> String {
    fs::read_to_string(&env.fno_calls).unwrap_or_default()
}

/// The specimen this fix exists for: a worker idles seven hours on an
/// unanswered question, then dies with `fno inbox outstanding` empty the whole
/// time. Assert the question survives the death.
#[test]
fn finalize_files_outstanding_question_on_stuck_terminal() {
    let env = setup("S-outq", false);
    let transcript = env.cwd.join("transcript.jsonl");
    write_transcript(
        &transcript,
        "mouse-mode root cause identified; awaiting operator's terminal/mux info",
    );

    let out = run_finalize_with_transcript(&env, "NoProgress", "S-outq", &transcript);
    assert!(out.status.success(), "{out:?}");

    let c = fno_calls(&env);
    assert!(c.contains("outstanding ask"), "{c}");
    assert!(
        env.outstanding_store.exists(),
        "the stub must have recorded the filed question"
    );
}

/// Dedup reads real state, not finalize's own once-per-session idempotency
/// (a different, ledger-gated mechanism): pre-seed the store as if the
/// question was already filed by some other path, and assert `finalize`
/// still refuses to double-file it.
#[test]
fn finalize_does_not_refile_an_already_open_question_for_this_session() {
    let env = setup("S-outq2", false);
    let transcript = env.cwd.join("transcript.jsonl");
    write_transcript(
        &transcript,
        "mouse-mode root cause identified; awaiting operator's terminal/mux info",
    );
    fs::write(
        &env.outstanding_store,
        serde_json::json!({
            "carveouts": {"total": 0, "by_kind": {}, "oldest_ts": null},
            "questions": [{
                "id": "q-existing",
                "ts": "2026-01-01T00:00:00Z",
                "question": "prior",
                "session_id": "S-outq2",
                "cwd": null,
                "node": null
            }]
        })
        .to_string(),
    )
    .unwrap();

    let out = run_finalize_with_transcript(&env, "NoProgress", "S-outq2", &transcript);
    assert!(out.status.success());
    assert!(
        !fno_calls(&env).contains("outstanding ask"),
        "a session with an already-open question must not file a second one"
    );
}

#[test]
fn finalize_files_nothing_when_no_question_is_detected() {
    let env = setup("S-outq3", false);
    let transcript = env.cwd.join("transcript.jsonl");
    write_transcript(&transcript, "implemented the fix and pushed a commit");

    let out = run_finalize_with_transcript(&env, "NoProgress", "S-outq3", &transcript);
    assert!(out.status.success());
    assert!(!fno_calls(&env).contains("outstanding ask"));
}

/// A clean ship is never a stuck-with-a-question terminal, however the
/// transcript happens to read - the check only runs on the STUCK bucket.
#[test]
fn finalize_files_nothing_on_a_ship_terminal_regardless_of_transcript() {
    let env = setup("S-outq4", false);
    let transcript = env.cwd.join("transcript.jsonl");
    write_transcript(&transcript, "awaiting operator's decision before shipping");

    let out = run_finalize_with_transcript(&env, "DonePRGreen", "S-outq4", &transcript);
    assert!(out.status.success());
    assert!(
        !fno_calls(&env).contains("outstanding ask"),
        "a clean ship must never be treated as a stuck-with-a-question terminal"
    );
}
