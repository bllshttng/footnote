#![allow(unused_imports)]

/// Integration tests for `fno-agents loop-check` verb.
///
/// Each test drives the public `run_loop_check` function directly (no process
/// spawn), using temporary directories for all file I/O.  gh/git are mocked
/// via the `FNO_LOOPCHECK_GH_BIN` / `FNO_LOOPCHECK_GIT_BIN` env overrides so
/// tests never hit the network.
///
/// All tests assert exactly which JSON fields the output carries and that
/// `target-state.md` bytes are unmodified after any fire (read-only invariant).
use fno_agents::loopcheck::run_loop_check;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Write an executable shell script to `dir/<name>` that prints `body` to
/// stdout and exits 0.  Returns the path.
fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    // Probe-exec until the script actually runs. A parallel test's fork can
    // inherit the just-written fd (CLOEXEC closes it only at the CHILD's
    // exec), so the verb under test exec'ing this script can hit ETXTBSY,
    // read the mock as "unavailable", and degrade fail-open - the
    // ac1_fr/ac2_edge/ac5_hp "allow where block expected" CI flake family.
    // Mock bodies are side-effect-free echoes, so one probe run is harmless;
    // any non-ETXTBSY outcome (nonzero exits included) proves exec works.
    for _ in 0..100 {
        match std::process::Command::new(&path)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => break,
        }
    }
    path
}

/// Build the two mock bin scripts (gh + git) in a temp dir and return the dir
/// so it is not dropped early.  The caller sets `FNO_LOOPCHECK_GH_BIN` and
/// `FNO_LOOPCHECK_GIT_BIN` to the returned paths.
struct MockBins {
    _dir: TempDir,
    pub gh: PathBuf,
    pub git: PathBuf,
}

impl MockBins {
    /// `gh pr view` returns `{"state":"OPEN","number":1,"headRefName":"main"}`;
    /// `gh pr checks` returns a passing check JSON; `gh pr view --json
    /// reviews,comments` returns one completed review by the DEFAULT required
    /// bot (chatgpt-codex-connector) with state COMMENTED - the shape both
    /// bots actually emit (verified on PR #447); proves COMMENTED counts as a
    /// completed pass (AC1-HP, "not approval-state").
    fn green() -> Self {
        let dir = TempDir::new().unwrap();
        let gh = make_script(
            dir.path(),
            "gh",
            r#"
# version probe (availability check)
if echo "$*" | grep -q -- "--version"; then
  echo 'gh version 2.x'
  exit 0
fi
# gh pr view --json state,number,headRefName
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}'
  exit 0
fi
# gh pr checks --json name,state,bucket (real schema: bucket is the rollup;
# `conclusion` is NOT an available field on this subcommand)
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
# gh pr view --json reviews,comments
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
        );
        let git = make_script(
            dir.path(),
            "git",
            r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000001""#,
        );
        MockBins { _dir: dir, gh, git }
    }

    /// gh always exits 1 (simulates outage / transient failure) EXCEPT for
    /// --version (availability probe must succeed so the code treats it as
    /// "gh present but commands failing").
    fn failing_gh() -> Self {
        let dir = TempDir::new().unwrap();
        let gh = make_script(
            dir.path(),
            "gh",
            r#"if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
exit 1"#,
        );
        let git = make_script(
            dir.path(),
            "git",
            r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000001""#,
        );
        MockBins { _dir: dir, gh, git }
    }

    /// gh not present (path is empty; the env var points to /dev/null for git).
    fn no_gh() -> (PathBuf, PathBuf) {
        // Return non-existent paths; callers unset FNO_LOOPCHECK_GH_BIN so the
        // code falls through to PATH where no gh exists in the test env.
        (
            PathBuf::from("/nonexistent/gh"),
            PathBuf::from("/nonexistent/git"),
        )
    }

    /// CI red: `gh pr checks` returns a fail bucket.
    fn ci_red() -> Self {
        let dir = TempDir::new().unwrap();
        let gh = make_script(
            dir.path(),
            "gh",
            r#"
if echo "$*" | grep -q -- "--version"; then
  echo 'gh version 2.x'; exit 0
fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":7,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000007"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"unit-tests","state":"FAILURE","bucket":"fail"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"codex[bot]"},"state":"APPROVED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
        );
        let git = make_script(
            dir.path(),
            "git",
            r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000007""#,
        );
        MockBins { _dir: dir, gh, git }
    }

    /// No PR: `gh pr view` exits 1 with gh's real no-PR stderr (distinct
    /// from an outage, which exits 1 with other stderr - see failing_gh).
    /// --version exits 0 so gh is detected as available.
    fn no_pr() -> Self {
        let dir = TempDir::new().unwrap();
        let gh = make_script(
            dir.path(),
            "gh",
            r#"if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
echo 'no pull requests found for branch "feat"' >&2
exit 1"#,
        );
        let git = make_script(
            dir.path(),
            "git",
            r#"echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1""#,
        );
        MockBins { _dir: dir, gh, git }
    }

    /// x-8b64 (E): PR merged out-of-band. `gh pr view` reports state=MERGED.
    /// CI is FAILURE and there are NO reviews - proving the merge short-circuits
    /// both the CI and review reads (a merged PR is terminal). git HEAD matches
    /// headRefOid so done()'s head_shipped guard passes.
    fn merged() -> Self {
        let dir = TempDir::new().unwrap();
        let gh = make_script(
            dir.path(),
            "gh",
            r#"
if echo "$*" | grep -q -- "--version"; then
  echo 'gh version 2.x'; exit 0
fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"MERGED","number":42,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000042"}'
  exit 0
fi
# These must NOT be reached for a merged PR; return red/empty so the test
# fails loudly if the short-circuit ever regresses.
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"unit-tests","state":"FAILURE","bucket":"fail"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[],"comments":[]}'
  exit 0
fi
exit 1
"#,
        );
        let git = make_script(
            dir.path(),
            "git",
            r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000042""#,
        );
        MockBins { _dir: dir, gh, git }
    }
}

/// Write a config.toml to `<cwd>/.fno/config.toml` so tests are
/// isolated from the real `$HOME/.fno/config.toml`. Pins the standard
/// review gate (`required_bots: [chatgpt-codex-connector]`) because the PRODUCT
/// default is now an EMPTY required_bots list (fresh installs complete without a
/// configured review bot). Every test that calls this helper historically ran
/// under the codex gate (a blank settings used to resolve to the codex default),
/// so pinning it here makes that assumption explicit and keeps gate-mechanics
/// tests gated. Tests of the no-gate path set their own settings instead. Must
/// be called after `fs::create_dir_all(cwd.join(".fno"))`.
fn isolate_settings(cwd: &Path) {
    // ab-098967b4: disable the P2 inbox-nudge shell-out so in-process decide()
    // calls never spawn `fno agents nudge-peek` (latency + real-bus side
    // effects). Idempotent set; never unset, so it is parallel-safe.
    std::env::set_var("FNO_NUDGE_DISABLED", "1");
    fs::write(
        cwd.join(".fno/config.toml"),
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();
}

// ── fixture builders ──────────────────────────────────────────────────────────

/// A minimal valid target-state.md for a NEW (non-legacy) session.
fn new_manifest(session_id: &str, created_at: &str, attended: bool) -> String {
    format!(
        "---\nsession_id: {session_id}\ncreated_at: {created_at}\nattended: {}\n---\n",
        if attended { "true" } else { "false" }
    )
}

fn manifest_with_budget(
    session_id: &str,
    created_at: &str,
    wall_cap_min: Option<u64>,
    cost_cap: Option<f64>,
) -> String {
    let mut s =
        format!("---\nsession_id: {session_id}\ncreated_at: {created_at}\nattended: true\n");
    if let Some(m) = wall_cap_min {
        s.push_str(&format!("budget_wall_clock_cap_minutes: {m}\n"));
    }
    if let Some(c) = cost_cap {
        s.push_str(&format!("budget_cost_cap_usd: {c}\n"));
    }
    s.push_str("---\n");
    s
}

fn legacy_manifest(session_id: &str, status: &str) -> String {
    format!(
        "---\nsession_id: {session_id}\ncreated_at: 2026-06-04T00:00:00Z\nstatus: {status}\n---\n"
    )
}

/// A minimal transcript JSONL where the last assistant message contains text.
fn transcript_with_promise() -> String {
    let msg = serde_json::json!({
        "message": {
            "role": "assistant",
            "content": "Done! <promise>MISSION COMPLETE</promise>"
        }
    });
    serde_json::to_string(&msg).unwrap() + "\n"
}

fn transcript_with_aborted() -> String {
    let msg = serde_json::json!({
        "message": {
            "role": "assistant",
            "content": "<aborted reason=\"user cancel\">session aborted</aborted>"
        }
    });
    serde_json::to_string(&msg).unwrap() + "\n"
}

fn transcript_empty() -> String {
    // A user message only - no assistant message.
    let msg = serde_json::json!({
        "message": { "role": "user", "content": "go" }
    });
    serde_json::to_string(&msg).unwrap() + "\n"
}

/// Parse the stdout JSON decision from run_loop_check return value.
#[derive(Debug, serde::Deserialize)]
struct Decision {
    decision: String,
    termination_reason: Option<String>,
    message: String,
    fires: u64,
    #[allow(dead_code)]
    fingerprint: Option<String>,
}

/// Run the verb, capture stdout, parse the JSON decision.
fn fire(args: &[&str]) -> (i32, Decision) {
    // run_loop_check writes JSON to stdout via println!; we capture it via
    // an in-process pipe simulation.  For simplicity, call via the public
    // function which returns the JSON string.
    let mut args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    // Hermeticity: never let the developer's real ~/.fno/config.toml
    // merge under test-local settings (the global+local merge is exercised
    // by the bash e2e harness, which controls HOME per case).
    args_owned.push("--global-settings".to_string());
    args_owned.push("/nonexistent/global-settings.yaml".to_string());
    let (code, json_str) = fno_agents::loopcheck::run_loop_check_capture(&args_owned);
    let d: Decision = serde_json::from_str(&json_str).expect(&format!(
        "run_loop_check returned non-JSON (code={code}): {json_str}"
    ));
    (code, d)
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// AC1-HP: promise with green PR -> DonePRGreen, exit 0, termination event.
#[test]
fn ac1_hp_promise_green_pr_done() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();

    // Create .fno dir for events
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-hp1", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let manifest_before = fs::read(&manifest_path).unwrap();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0, "exit code must be 0 for allow");
    assert_eq!(d.decision, "allow");
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "expected DonePRGreen but got {:?}",
        d.termination_reason
    );
    assert_eq!(d.fires, 1);

    // Verify: target-state.md bytes unchanged (read-only invariant)
    let manifest_after = fs::read(&manifest_path).unwrap();
    assert_eq!(
        manifest_before, manifest_after,
        "target-state.md must not be mutated"
    );

    // Verify: termination event appended to project events
    let events_path = cwd.join(".fno/events.jsonl");
    assert!(events_path.exists(), "project events.jsonl must exist");
    let events_content = fs::read_to_string(&events_path).unwrap();
    assert!(
        events_content.contains("\"termination\""),
        "termination event expected in events.jsonl"
    );
    assert!(
        events_content.contains("DonePRGreen"),
        "DonePRGreen in termination event"
    );
}

/// x-81d9 (c) / AC3-UI: an unparseable `.fno/config.toml` must emit a
/// `loop_check_settings_unparseable` event (and fail the login gate closed),
/// never silently zero the required bots and ship unreviewed.
#[test]
fn ac3_ui_unparseable_settings_emits_event() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    // Deliberately malformed YAML (unclosed flow sequence). No isolate_settings.
    fs::write(
        cwd.join(".fno/config.toml"),
        "[review]\nrequired_bots = [\"codex\", \"gemini\"\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-unparse", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let (_code, _d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("loop_check_settings_unparseable"),
        "unparseable settings must emit loop_check_settings_unparseable; events: {events}"
    );
}

/// x-81d9 (c) regression (peer review): an unparseable LOCAL config.toml must
/// fail the gate closed even when a parseable GLOBAL file declares an empty
/// github_apps gate. resolved_required_bots prefers github_apps over
/// required_bots, so the fail-closed sentinel must be pinned into github_apps
/// too - otherwise the merge keeps the global (empty) gate and ships unreviewed.
#[test]
fn unparseable_local_settings_not_outranked_by_global_github_apps() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    // GLOBAL: a parseable, empty github_apps gate (the worst case - no bots).
    let global = cwd.join("global.yaml");
    fs::write(&global, "[review]\ngithub_apps = []\n").unwrap();
    // LOCAL: unparseable (the exact bug this PR targets).
    fs::write(
        cwd.join(".fno/config.toml"),
        "[review]\ngithub_apps = [\"codex\"\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-merge", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    // Call the verb directly (NOT via `fire`, which forces --global-settings
    // /nonexistent); pass our own global so the merge overlay runs.
    let args: Vec<String> = vec![
        "loop-check".into(),
        "--state".into(),
        manifest_path.to_str().unwrap().into(),
        "--transcript".into(),
        transcript_path.to_str().unwrap().into(),
        "--cwd".into(),
        cwd.to_str().unwrap().into(),
        "--now".into(),
        "2026-06-05T00:30:00Z".into(),
        format!("--gh-bin={}", mock.gh.display()),
        format!("--git-bin={}", mock.git.display()),
        "--global-settings".into(),
        global.to_str().unwrap().into(),
    ];
    let (_code, json_str) = fno_agents::loopcheck::run_loop_check_capture(&args);
    let d: Decision = serde_json::from_str(&json_str).unwrap();
    assert_ne!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "an unparseable local settings.yaml must not ship green via a global empty gate; got: {json_str}"
    );
}

/// batch-lane Wave 2/3 (x-6cdf): a batched unit terminates as DoneBatched on
/// its promise even with NO PR (its commits ship via the batch PR, not its
/// own). The no_pr mock proves the batched arm short-circuits BEFORE run_done,
/// which would otherwise block forever waiting for a per-node PR.
#[test]
fn batched_unit_promise_done_batched() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    // A batched manifest: batched:true, and NOT no_ship/advisory (so it must not
    // fall into the DoneAdvisory arm - which would wrongly graduate the plan).
    fs::write(
        &manifest_path,
        "---\nsession_id: sess-batch1\ncreated_at: 2026-07-01T00:00:00Z\nattended: false\nbatched: true\n---\n",
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::no_pr();
    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-07-01T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0, "exit code must be 0 for allow");
    assert_eq!(d.decision, "allow");
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DoneBatched"),
        "expected DoneBatched (no per-node PR) but got {:?}",
        d.termination_reason
    );

    let events_content = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    assert!(
        events_content.contains("DoneBatched"),
        "DoneBatched in termination event"
    );
}

/// A batched manifest with NO promise yet must NOT terminate: the member is
/// still working. Fail-safe - it blocks (keep looping) rather than falsely
/// closing an unfinished batch member.
#[test]
fn batched_unit_without_promise_blocks() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        "---\nsession_id: sess-batch2\ncreated_at: 2026-07-01T00:00:00Z\nattended: false\nbatched: true\n---\n",
    )
    .unwrap();
    // No promise in the transcript.
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-07-01T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);
    assert_eq!(d.decision, "block", "no promise -> keep working");
    assert_eq!(d.termination_reason, None);
}

/// x-8b64 (E): promise with an out-of-band MERGED PR -> DonePRGreen, even
/// though CI is red and NO required bot reviewed. The merge is terminal; the
/// stop-hook must stop re-poking a finished session.
#[test]
fn out_of_band_merged_pr_done() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();

    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-merged1", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::merged();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0, "exit code must be 0 for allow");
    assert_eq!(d.decision, "allow");
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "a merged PR must terminate DonePRGreen despite red CI / no review; got {:?} ({})",
        d.termination_reason,
        d.message
    );
}

/// AC1-ERR: gh outage never passes a promise -> block + loop_check_gh_error.
#[test]
fn ac1_err_gh_outage_blocks_promise() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-err1", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::failing_gh();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0, "exit code must be 0 even when blocking");
    assert_eq!(d.decision, "block");
    assert!(d.termination_reason.is_none(), "no termination on block");

    // loop_check_gh_error event must exist
    let events_path = cwd.join(".fno/events.jsonl");
    let events = fs::read_to_string(&events_path).unwrap_or_default();
    assert!(
        events.contains("loop_check_gh_error"),
        "loop_check_gh_error event expected; events: {events}"
    );
    // No termination event
    assert!(
        !events.contains("\"termination\""),
        "no termination event on gh failure"
    );
}

/// AC1-UI: CI red -> block, message names the failing check.
#[test]
fn ac1_ui_ci_red_block_names_check() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-ui1", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::ci_red();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(d.decision, "block");
    assert!(
        d.message.contains("unit-tests") || d.message.contains("CI") || d.message.contains("ci"),
        "block message should name the failing check; got: {}",
        d.message
    );
}

/// AC1-EDGE: no PR yet, no promise -> block with continue message, fingerprint
/// event records pr_state=none.
#[test]
fn ac1_edge_no_pr_block_with_fingerprint() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-edge1", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();
    let manifest_before = fs::read(&manifest_path).unwrap();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(d.termination_reason.is_none());

    // Read-only invariant
    assert_eq!(fs::read(&manifest_path).unwrap(), manifest_before);

    // Fingerprint event with pr_state=none
    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("loop_check"),
        "loop_check event expected; got: {events}"
    );
    assert!(
        events.contains("none"),
        "pr_state=none expected in fingerprint event"
    );
}

/// AC1-FR: read-only invariant - manifest bytes unchanged across any fire.
#[test]
fn ac1_fr_manifest_readonly() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-ro", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let before = fs::read(&manifest_path).unwrap();

    let _ = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    let after = fs::read(&manifest_path).unwrap();
    assert_eq!(before, after, "manifest must not be mutated by any fire");
}

/// Cancel sentinel present (mtime >= created_at) -> Interrupted.
#[test]
fn cancel_sentinel_interrupted() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let sentinel_path = cwd.join(".fno/.target-cancelled");

    fs::write(
        &manifest_path,
        new_manifest("sess-cancel", "2026-06-04T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    fs::write(&sentinel_path, "").unwrap(); // mtime = now, after created_at

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Interrupted"));
}

/// Legacy manifest with status: COMPLETE -> allow + loop_check_legacy_manifest.
#[test]
fn ac4_edge_legacy_complete_allows_exit() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(&manifest_path, legacy_manifest("sess-legacy", "COMPLETE")).unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");

    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("loop_check_legacy_manifest"),
        "legacy event expected; got: {events}"
    );
}

/// AC3-HP: budget trip via FLAT budget_cap key (ab-41b13d9d fold-in proof).
#[test]
fn ac3_hp_budget_flat_key_trips_cost() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // Manifest with NO nested budget block; the settings.yaml has flat budget_cap
    fs::write(
        &manifest_path,
        new_manifest("sess-budget", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // Settings file with flat budget_cap: 0.01 (very low)
    let settings_path = cwd.join(".fno/config.toml");
    fs::write(&settings_path, "budget_cap = 0.01\n").unwrap();

    // Ledger with cost > 0.01 for this session
    let ledger_path = cwd.join(".fno/ledger.json");
    let ledger = serde_json::json!([
        {"session_id": "sess-budget", "cost_usd": 0.05, "tokens": 1000}
    ]);
    fs::write(&ledger_path, serde_json::to_string(&ledger).unwrap()).unwrap();

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        "--ledger",
        ledger_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Budget"));

    // Verify axis=cost in the termination event
    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("cost"),
        "axis=cost expected in Budget termination event; got: {events}"
    );
}

/// Wall-clock budget trip.
#[test]
fn wall_clock_budget_trips() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    // Manifest created 2h ago, wall cap = 60 min -> trip
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        manifest_with_budget("sess-wall", "2026-06-05T00:00:00Z", Some(60), None),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T02:30:00Z", // 2.5h after created_at, cap=60min
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Budget"));

    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(events.contains("wall_clock"), "axis=wall_clock expected");
}

/// <aborted> tag -> Aborted termination.
#[test]
fn aborted_tag_terminates() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-aborted", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_aborted()).unwrap();

    let mock = MockBins::green();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Aborted"));
}

/// no_ship manifest + promise -> DoneAdvisory (no done() PR reads needed).
#[test]
fn no_ship_advisory_terminates() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    let manifest = "---\nsession_id: sess-nship\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\nno_ship: true\n---\n";
    fs::write(&manifest_path, manifest).unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("DoneAdvisory"));
}

/// AC2-HP: N=3 consecutive identical fingerprints (unattended) -> NoProgress.
#[test]
fn ac2_hp_fingerprint_backstop_no_progress() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");

    // Unattended session (N=3)
    fs::write(
        &manifest_path,
        new_manifest("sess-backstop", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // gh says no PR -> pr_state=none for all fires
    let mock = MockBins::no_pr();
    let args_base = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];

    // Fire 1 and 2: block, no termination
    let (_, d1) = fire(&args_base);
    assert_eq!(d1.decision, "block");
    let (_, d2) = fire(&args_base);
    assert_eq!(d2.decision, "block");

    // Fire 3: should trip backstop -> NoProgress
    let (_, d3) = fire(&args_base);
    assert_eq!(d3.decision, "allow");
    assert_eq!(d3.termination_reason.as_deref(), Some("NoProgress"));
    assert_eq!(d3.fires, 3);
}

/// AC2-EDGE: 4th-component change (new review timestamp) resets counter.
#[test]
fn ac2_edge_review_ts_change_resets_counter() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");

    // Unattended (N=3)
    fs::write(
        &manifest_path,
        new_manifest("sess-edge2", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // First gh mock: no review activity
    let dir1 = TempDir::new().unwrap();
    let gh1 = make_script(
        dir1.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":3,"headRefName":"feat"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git1 = make_script(
        dir1.path(),
        "git",
        r#"echo "cccccccccccccccccccccccccccccccccccccccc""#,
    );

    let args1 = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", gh1.display()),
        &format!("--git-bin={}", git1.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];

    // Fire 1 and 2: same fingerprint
    let (_, d1) = fire(&args1);
    assert_eq!(d1.decision, "block");
    let (_, d2) = fire(&args1);
    assert_eq!(d2.decision, "block");

    // Fire 3 with a NEW review timestamp -> counter should reset, no NoProgress
    let dir2 = TempDir::new().unwrap();
    let gh2 = make_script(
        dir2.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":3,"headRefName":"feat"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"APPROVED","submittedAt":"2026-06-05T02:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git2 = make_script(
        dir2.path(),
        "git",
        r#"echo "cccccccccccccccccccccccccccccccccccccccc""#,
    );

    let args2 = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", gh2.display()),
        &format!("--git-bin={}", git2.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];

    let (_, d3) = fire(&args2);
    // The 4th component changed -> not a backstop trip
    assert_eq!(
        d3.decision, "block",
        "counter should reset due to 4th-component change; got {:?}",
        d3.termination_reason
    );
    assert!(
        d3.termination_reason.is_none(),
        "no NoProgress when fingerprint changed"
    );
}

/// AC2-FR + AC3-HP (ab-223d2dae D): "done but mute" - PR green + reviewed,
/// no promise -> the MUTE_PROBE_N=2 probe runs done() at the second
/// unchanged fire -> DonePRGreen (late), not NoProgress, and ~2 fires
/// instead of the full backstop streak.
#[test]
fn ac2_fr_done_but_mute_resolves_done_pr_green() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");

    // Unattended (N=3), green PR, no promise
    fs::write(
        &manifest_path,
        new_manifest("sess-mute", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::green();
    let args = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];

    // Fire 1: streak 1 < MUTE_PROBE_N -> no probe, plain block.
    let (_, d1) = fire(&args);
    assert_eq!(d1.decision, "block");

    // Fire 2: streak 2 hits the mute probe; done() sees green PR ->
    // DonePRGreen (not NoProgress) without waiting out the backstop streak.
    let (_, d2) = fire(&args);
    assert_eq!(d2.decision, "allow");
    assert_eq!(
        d2.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "done-but-mute must resolve as DonePRGreen at the probe fire"
    );
    assert_eq!(d2.fires, 2, "the mute probe fires at 2, not backstop_n");
}

/// AC5-HP: declared no-CI (ci.declared_none: true) -> CI read skipped, DonePRGreen reachable.
#[test]
fn ac5_hp_declared_no_ci_skipped() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let settings_path = cwd.join(".fno/config.toml");

    fs::write(
        &manifest_path,
        new_manifest("sess-noci", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();
    fs::write(&settings_path, "[ci]\ndeclared_none = true\n").unwrap();

    // gh: returns no checks (empty array) but we declared no-ci so it should skip
    let dir = TempDir::new().unwrap();
    let gh = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":5,"headRefName":"main","headRefOid":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee""#,
    );

    let (_, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);

    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));
}

/// AC5-HP (fail-closed): no CI flag + empty checks -> fail closed (block).
#[test]
fn ac5_hp_no_ci_flag_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-noci2", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // No settings file -> no declared_none; gh returns empty checks
    let dir = TempDir::new().unwrap();
    let gh = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":5,"headRefName":"main","headRefOid":"ffffffffffffffffffffffffffffffffffffffff"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "ffffffffffffffffffffffffffffffffffffffff""#,
    );

    let (_, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);

    assert_eq!(d.decision, "block");
    // Message should mention declaring ci.declared_none
    assert!(
        d.message.contains("declared_none") || d.message.contains("no checks"),
        "message should mention no checks or declared_none; got: {}",
        d.message
    );
}

/// AC5-ERR: gh absent + unattended + no advisory -> Interrupted.
#[test]
fn ac5_err_no_gh_unattended_interrupted() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // Unattended, no advisory flag
    fs::write(
        &manifest_path,
        new_manifest("sess-nogh-unatt", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // Point to non-existent gh binary; git also non-existent
    let (_, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--gh-bin=/nonexistent/gh",
        "--git-bin=/nonexistent/git",
    ]);

    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Interrupted"));
}

/// AC5-ERR: gh absent + attended -> block with advisory mode, loop_advisory_mode event.
#[test]
fn ac5_err_no_gh_attended_advisory_block() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // Attended
    fs::write(
        &manifest_path,
        new_manifest("sess-nogh-att", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let (_, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--gh-bin=/nonexistent/gh",
        "--git-bin=/nonexistent/git",
    ]);

    // Attended + no gh -> advisory mode -> block (keep working)
    assert_eq!(d.decision, "block");

    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("loop_advisory_mode"),
        "loop_advisory_mode event expected; got: {events}"
    );
}

/// Corrupt manifest -> allow + note on stderr (never panics, never traps).
#[test]
fn corrupt_manifest_allows_exit() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(&manifest_path, "this is not yaml frontmatter at all").unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0, "corrupt manifest must not exit non-zero");
    assert_eq!(
        d.decision, "allow",
        "corrupt manifest -> allow (never trap)"
    );
}

/// Events are appended to BOTH project and global paths.
#[test]
fn events_appended_to_both_project_and_global() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let global_dir = tmp.path().join("global_abilities");
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    fs::create_dir_all(&global_dir).unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let global_events = global_dir.join("events.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-dual", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let _ = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--global-events",
        global_events.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert!(
        cwd.join(".fno/events.jsonl").exists(),
        "project events.jsonl must exist"
    );
    assert!(global_events.exists(), "global events.jsonl must exist");

    let proj_events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    let glob_events = fs::read_to_string(&global_events).unwrap();
    assert!(
        !proj_events.is_empty() && !glob_events.is_empty(),
        "both event files must have content"
    );
}

/// AC5-ERR: CLI misuse (no --state flag) -> exit code 2 from parse_args
/// validation, never a panic downstream.
#[test]
fn cli_misuse_exits_2() {
    let args: Vec<String> = vec!["loop-check".to_string()];
    let (code, json) = fno_agents::loopcheck::run_loop_check_capture(&args);
    assert_eq!(code, 2, "missing --state must exit 2 (CLI misuse): {json}");
    assert!(
        json.contains("--state is required"),
        "error JSON must name the missing flag; got: {json}"
    );
}

/// AC5-UI: golden fingerprint - the exact fingerprint string for a known
/// world state must survive the enum refactor byte-identically.
/// head_sha|pr_state|ci_conclusion|latest_review_ts
#[test]
fn ac5_ui_golden_fingerprint_byte_identical() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-golden", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.fingerprint.as_deref(),
        Some("deadbeefdeadbeefdeadbeefdeadbeef00000001|OPEN|SUCCESS|2026-06-05T01:00:00Z"),
        "fingerprint format must be byte-identical to the pre-enum string"
    );
}

/// AC5-ERR (a): gh binary absent + attended + no intent -> block in advisory
/// mode, loop_advisory_mode event emitted each fire.
#[test]
fn gh_absent_attended_blocks_advisory() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-adv1", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let (gh, git) = MockBins::no_gh();
    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:10:00Z",
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(
        d.decision, "block",
        "advisory mode without intent must block"
    );
    assert!(d.termination_reason.is_none());
    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    assert!(
        events.contains("\"loop_advisory_mode\""),
        "loop_advisory_mode event expected: {events}"
    );
}

/// AC5-ERR (b): gh absent + attended + promise -> DoneAdvisory (promise alone
/// is the completion signal when gh reads are impossible).
#[test]
fn gh_absent_attended_promise_done_advisory() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-adv2", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let (gh, git) = MockBins::no_gh();
    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:10:00Z",
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("DoneAdvisory"));
    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    assert!(events.contains("\"loop_advisory_mode\""));
    assert!(events.contains("DoneAdvisory"));
}

/// AC5-ERR (c): gh absent + unattended + no declared advisory -> Interrupted
/// with a termination event (unattended cannot run without gh).
#[test]
fn gh_absent_unattended_interrupted() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-adv3", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let (gh, git) = MockBins::no_gh();
    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:10:00Z",
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Interrupted"));
    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    assert!(
        events.contains("\"termination\"") && events.contains("Interrupted"),
        "termination(Interrupted) event expected: {events}"
    );
}

/// codex P1 on #447: a green PR whose head != local HEAD (unpushed local
/// commit) must NOT terminate DonePRGreen; the block message names the push.
#[test]
fn promise_green_pr_with_unpushed_head_blocks() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-unpushed", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // green() PR head is deadbeef...0001; local git stub reports a DIFFERENT sha.
    let mock = MockBins::green();
    let dir = TempDir::new().unwrap();
    let git_ahead = make_script(
        dir.path(),
        "git",
        r#"echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaunpushed1""#,
    );

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", git_ahead.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(
        d.decision, "block",
        "unpushed local HEAD must block: {}",
        d.message
    );
    assert!(d.termination_reason.is_none());
    assert!(
        d.message.contains("push"),
        "block message should tell the agent to push; got: {}",
        d.message
    );
}

// ── step 2: required_bots review gate (US1) ──────────────────────────────────

/// gh mock: green CI + head-shipped, but only a NON-required bot (gemini)
/// reviewed. Under the codex-only default this must block.
fn green_gemini_only_reviewed() -> MockBins {
    let dir = TempDir::new().unwrap();
    let gh = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":9,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000009"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000009""#,
    );
    MockBins { _dir: dir, gh, git }
}

/// AC1-ERR + AC1-UI: PR green and head-shipped but the required bot (codex
/// default) has not reviewed -> block, message names the missing bot, no
/// termination. A lone gemini COMMENTED review must no longer flip reviewed
/// (the PR #390 miss).
#[test]
fn ac1_err_missing_required_bot_blocks_naming_bot() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-reqbot1", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = green_gemini_only_reviewed();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(
        d.decision, "block",
        "gemini-only review must not satisfy the codex-required default: {}",
        d.message
    );
    assert!(d.termination_reason.is_none());
    assert!(
        d.message.contains("chatgpt-codex-connector"),
        "block message must name the missing required bot; got: {}",
        d.message
    );
    assert!(
        d.message.contains("has not reviewed"),
        "block message must say the bot has not reviewed; got: {}",
        d.message
    );
}

/// AC1-EDGE: two-bot required list, only one reviewed -> reviewed=false,
/// block names the missing bot (and only the missing one).
#[test]
fn ac1_edge_two_bot_config_one_missing_blocks() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\", \"gemini-code-assist\"]\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-reqbot2", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // Only codex reviewed; gemini-code-assist is required but missing.
    let mock = MockBins::green();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(d.decision, "block");
    assert!(
        d.message.contains("gemini-code-assist"),
        "block must name the missing bot; got: {}",
        d.message
    );
    assert!(
        !d.message.contains("chatgpt-codex-connector"),
        "block must not name the bot that DID review; got: {}",
        d.message
    );
}

/// AC1-FR: late review recovery - fire 1 blocks (codex missing), codex then
/// posts its review, fire 2 terminates DonePRGreen. The new review timestamp
/// advances the fingerprint between the fires (no false NoProgress).
#[test]
fn ac1_fr_late_review_then_done() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-latefr", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // Fire 1: gemini-only -> block. (Same head sha as green().)
    let dir1 = TempDir::new().unwrap();
    let gh1 = make_script(
        dir1.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"COMMENTED","submittedAt":"2026-06-05T00:50:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git1 = make_script(
        dir1.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000001""#,
    );

    let args1 = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:55:00Z",
        &format!("--gh-bin={}", gh1.display()),
        &format!("--git-bin={}", git1.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];
    let (_, d1) = fire(&args1);
    assert_eq!(d1.decision, "block");
    assert!(d1.message.contains("chatgpt-codex-connector"));

    // Fire 2: codex review arrives (newer ts; green() carries it at 01:00).
    let mock2 = MockBins::green();
    let args2 = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:05:00Z",
        &format!("--gh-bin={}", mock2.gh.display()),
        &format!("--git-bin={}", mock2.git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];
    let (_, d2) = fire(&args2);
    assert_eq!(d2.decision, "allow");
    assert_eq!(
        d2.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "late review must complete the session: {}",
        d2.message
    );
    // AC1-FR's first clause: the fingerprint's timestamp component advanced
    // between the fires (no false NoProgress window in between).
    assert_ne!(
        d1.fingerprint, d2.fingerprint,
        "the late review's timestamp must advance the fingerprint"
    );
}

// ── step 2: inline findings gate (US2) ────────────────────────────────────────

/// Green CI + codex reviewed, with a parameterized /pulls/N/comments payload
/// (Read 4) and commits payload. The mock writes the JSON to files so shell
/// quoting stays trivial.
fn findings_mock(comments_json: &str, commits_json: &str) -> MockBins {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("comments.json"), comments_json).unwrap();
    fs::write(dir.path().join("commits.json"), commits_json).unwrap();
    let comments_path = dir.path().join("comments.json");
    let commits_path = dir.path().join("commits.json");
    let gh = make_script(
        dir.path(),
        "gh",
        &format!(
            r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{{"state":"OPEN","number":4,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000004"}}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{{"name":"ci","state":"SUCCESS","bucket":"pass"}}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  cat "{comments}"
  exit 0
fi
if echo "$*" | grep -q "commits"; then
  cat "{commits}"
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{{"reviews":[{{"author":{{"login":"chatgpt-codex-connector"}},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}}],"comments":[]}}'
  exit 0
fi
exit 1
"#,
            comments = comments_path.display(),
            commits = commits_path.display()
        ),
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000004""#,
    );
    MockBins { _dir: dir, gh, git }
}

fn fire_findings(cwd: &Path, mock: &MockBins) -> (i32, Decision) {
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T02:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ])
}

fn findings_cwd(session: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    fs::write(
        cwd.join("target-state.md"),
        new_manifest(session, "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(cwd.join("transcript.jsonl"), transcript_with_promise()).unwrap();
    tmp
}

const CODEX_P1_NO_REPLY: &str = r#"[
  {"id": 100, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Off-by-one",
   "path": "src/x.rs", "line": 42, "created_at": "2026-06-05T01:10:00Z"}
]"#;

/// AC2-ERR + AC2-UI: unaddressed P1 blocks; message carries path:line and the
/// remedy. Also proves the inline ts feeds the fingerprint's 4th component
/// (01:10 > the 01:00 review ts).
#[test]
fn ac2_err_unaddressed_p1_blocks_with_path_line() {
    let tmp = findings_cwd("sess-p1");
    let mock = findings_mock(CODEX_P1_NO_REPLY, r#"{"commits":[]}"#);

    let (code, d) = fire_findings(tmp.path(), &mock);

    assert_eq!(code, 0);
    assert_eq!(
        d.decision, "block",
        "unaddressed P1 must block: {}",
        d.message
    );
    assert!(d.termination_reason.is_none());
    assert!(
        d.message.contains("src/x.rs:42"),
        "message must carry the finding's path:line; got: {}",
        d.message
    );
    assert!(
        d.message.contains("wontfix:") && d.message.contains("reply in-thread"),
        "message must name the remedy; got: {}",
        d.message
    );
    assert!(
        d.fingerprint
            .as_deref()
            .unwrap_or("")
            .ends_with("|2026-06-05T01:10:00Z"),
        "inline finding ts must feed the fingerprint 4th component; got: {:?}",
        d.fingerprint
    );
}

/// AC2-HP (commit arm): P1 + non-bot in-thread reply + commit after the
/// finding -> addressed -> DonePRGreen.
#[test]
fn ac2_hp_addressed_via_commit_terminates() {
    let comments = r#"[
  {"id": 100, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Off-by-one",
   "path": "src/x.rs", "line": 42, "created_at": "2026-06-05T01:10:00Z"},
  {"id": 101, "in_reply_to_id": 100,
   "user": {"login": "bllshttng"},
   "body": "Fixed in deadbeef.",
   "created_at": "2026-06-05T01:20:00Z"}
]"#;
    let commits = r#"{"commits":[{"committedDate":"2026-06-05T01:30:00Z"}]}"#;
    let tmp = findings_cwd("sess-p1fix");
    let mock = findings_mock(comments, commits);

    let (_, d) = fire_findings(tmp.path(), &mock);
    assert_eq!(d.decision, "allow", "addressed P1 must pass: {}", d.message);
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));
}

/// AC2-FR (wontfix arm): P1 + non-bot reply carrying wontfix:, no fix commit
/// -> addressed -> DonePRGreen.
#[test]
fn ac2_fr_wontfix_reply_terminates_without_commit() {
    let comments = r#"[
  {"id": 100, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Off-by-one",
   "path": "src/x.rs", "line": 42, "created_at": "2026-06-05T01:10:00Z"},
  {"id": 101, "in_reply_to_id": 100,
   "user": {"login": "bllshttng"},
   "body": "wontfix: intentional, documented in the design doc.",
   "created_at": "2026-06-05T01:20:00Z"}
]"#;
    // Only commit predates the finding: the wontfix arm must carry alone.
    let commits = r#"{"commits":[{"committedDate":"2026-06-05T00:30:00Z"}]}"#;
    let tmp = findings_cwd("sess-p1wf");
    let mock = findings_mock(comments, commits);

    let (_, d) = fire_findings(tmp.path(), &mock);
    assert_eq!(d.decision, "allow", "wontfix must address: {}", d.message);
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));
}

// ── step 2: declared no-review repo (US3) ─────────────────────────────────────

/// Green PR + CI mock whose review endpoints (reviews / pulls comments /
/// commits) all FAIL: proves Reads 3+4 are genuinely skipped, not just
/// tolerated, when the repo declares `required_bots: []`.
fn green_reviews_unreachable() -> MockBins {
    let dir = TempDir::new().unwrap();
    let gh = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":11,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000011"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000011""#,
    );
    MockBins { _dir: dir, gh, git }
}

/// AC3-HP + AC3-UI: `required_bots: []` skips Reads 3+4 (reviewed=true) and
/// the loop_check event records review_skipped (observable, not silent).
#[test]
fn ac3_hp_empty_required_bots_skips_review_reads() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nrequired_bots = []\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-norev", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // Review endpoints all fail: a skip is the only way this passes.
    let mock = green_reviews_unreachable();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(
        d.decision, "allow",
        "declared no-review repo must pass on PR+CI alone: {}",
        d.message
    );
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));

    // AC3-UI: the skip is recorded in the loop_check event.
    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("\"review_skipped\":true"),
        "loop_check event must record review_skipped; got: {events}"
    );
}

/// AC3-ERR: a malformed (non-list) required_bots parses to None, which under
/// the fresh-install default (empty required_bots) means no review gate. A
/// malformed value does NOT enforce a gate; maintainers must pin a valid list.
#[test]
fn ac3_err_malformed_required_bots_no_gate() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nrequired_bots = \"gemini\"\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-malf", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // Only gemini reviewed, but with no required bots the review axis does not
    // gate at all; green CI then lets the session complete.
    let mock = green_gemini_only_reviewed();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_ne!(
        d.decision, "block",
        "malformed required_bots now means no review gate, not a codex block: {}",
        d.message
    );
    assert!(
        !d.message.contains("chatgpt-codex-connector"),
        "no default bot should gate under the empty default; got: {}",
        d.message
    );
}

/// AC3-EDGE: per-session no_external skips review even when required_bots is
/// non-empty (orthogonal to repo config).
#[test]
fn ac3_edge_no_external_orthogonal_to_required_bots() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let manifest = "---\nsession_id: sess-noext\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\nno_external: true\n---\n";
    fs::write(&manifest_path, manifest).unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = green_reviews_unreachable();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.decision, "allow",
        "no_external must skip review per-session: {}",
        d.message
    );
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));
}

// ── x-e703: config.review.reviewers local-attestation gate ──────────────────

/// The green() git+gh mock's HEAD (== headRefOid, so head_shipped passes). An
/// attestation must carry this exact sha to satisfy the head-pin.
const GREEN_HEAD: &str = "deadbeefdeadbeefdeadbeefdeadbeef00000001";

/// AC3-HP: a `reviewers: [sigma]` gate with NO matching attestation holds the
/// session closed even when the PR is green, CI passes, and HEAD is shipped -
/// the local attestation is required, absence fails closed.
#[test]
fn reviewers_gate_blocks_without_attestation() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nreviewers = [\"sigma\"]\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-rvw-block", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.decision, "block",
        "reviewers gate with no attestation must block: {}",
        d.message
    );
    assert!(d.termination_reason.is_none());
}

/// AC3-HP / AC8-HP: the gate clears once a head-pinned `review_attestation`
/// (reviewer sigma, verdict pass, head_sha == current HEAD) exists.
#[test]
fn reviewers_gate_clears_with_head_pinned_attestation() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nreviewers = [\"sigma\"]\n",
    )
    .unwrap();
    // The attestation lands in the project events log loop-check reads.
    fs::write(
        cwd.join(".fno/events.jsonl"),
        format!(
            "{{\"ts\":\"2026-06-05T00:10:00Z\",\"type\":\"review_attestation\",\"source\":\"target\",\"data\":{{\"reviewer\":\"sigma\",\"head_sha\":\"{GREEN_HEAD}\",\"verdict\":\"pass\"}}}}\n"
        ),
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-rvw-pass", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.decision, "allow",
        "a head-pinned sigma attestation must clear the gate: {}",
        d.message
    );
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));
}

/// The fix (sigma review, silent-failure-hunter MEDIUM): `no_external` is
/// scoped to EXTERNAL GitHub-bot review; it must NOT bypass the LOCAL
/// `reviewers` attestation gate. A session that skips wedged App bots with
/// no_external still owes its configured local sigma pass.
#[test]
fn no_external_still_honors_reviewers_gate() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nreviewers = [\"sigma\"]\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    // no_external: true - but reviewers is set and no attestation exists.
    fs::write(
        &manifest_path,
        "---\nsession_id: sess-rvw-noext\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\nno_external: true\n---\n",
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        settings_path.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.decision, "block",
        "no_external must NOT bypass the local reviewers gate: {}",
        d.message
    );
    assert!(d.termination_reason.is_none());
}

/// AC3-FR: recovery from an accidental empty list - restoring the bot list
/// re-enforces the gate on the next fire with no state migration.
#[test]
fn ac3_fr_restoring_required_bots_reenforces() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-restore", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // gh: green PR, gemini-only review (codex missing).
    let mock = green_gemini_only_reviewed();

    // Fire 1: required_bots [] -> passes without review.
    let empty_settings = cwd.join("empty-config.toml");
    fs::write(
        &empty_settings,
        "[review]\nrequired_bots = []\n",
    )
    .unwrap();
    let (_, d1) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        "--settings",
        empty_settings.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ]);
    assert_eq!(d1.termination_reason.as_deref(), Some("DonePRGreen"));

    // Fire 2: operator restores the list -> gate enforces again immediately.
    let restored_settings = cwd.join("restored-config.toml");
    fs::write(
        &restored_settings,
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();
    let (_, d2) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:35:00Z",
        "--settings",
        restored_settings.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ]);
    assert_eq!(
        d2.decision, "block",
        "restored list must re-enforce: {}",
        d2.message
    );
    assert!(d2.message.contains("chatgpt-codex-connector"));
}

// ── step 2: gh-outage streak freeze (US4) ─────────────────────────────────────

/// AC4-HP + AC4-FR: outage fires neither advance nor reset the consecutive
/// count; after recovery the streak resumes from K and the backstop works.
#[test]
fn ac4_hp_fr_outage_freezes_streak_then_resumes() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");

    // Unattended: N = 3
    fs::write(
        &manifest_path,
        new_manifest("sess-freeze", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let healthy = green_gemini_only_reviewed();
    let outage = MockBins::failing_gh();

    let args_for = |mock: &MockBins| {
        [
            "loop-check".to_string(),
            "--state".to_string(),
            manifest_path.to_str().unwrap().to_string(),
            "--transcript".to_string(),
            transcript_path.to_str().unwrap().to_string(),
            "--cwd".to_string(),
            cwd.to_str().unwrap().to_string(),
            "--now".to_string(),
            "2026-06-05T00:30:00Z".to_string(),
            format!("--gh-bin={}", mock.gh.display()),
            format!("--git-bin={}", mock.git.display()),
            "--events".to_string(),
            events_path.to_str().unwrap().to_string(),
        ]
    };
    let fire_with = |mock: &MockBins| {
        let owned = args_for(mock);
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        fire(&refs)
    };

    // Fires 1-2: healthy, identical fingerprint -> streak 1, 2.
    let (_, d1) = fire_with(&healthy);
    assert_eq!(d1.decision, "block");
    let (_, d2) = fire_with(&healthy);
    assert_eq!(d2.decision, "block");

    // Fires 3-4: OUTAGE. Under pre-step-2 semantics fire 3 would have hit
    // N=3 and terminated NoProgress; the freeze keeps the count at 2.
    let (_, d3) = fire_with(&outage);
    assert_eq!(
        d3.decision, "block",
        "outage fire must block, not terminate"
    );
    assert!(
        d3.termination_reason.is_none(),
        "outage must not trip NoProgress (AC4-HP); got {:?}",
        d3.termination_reason
    );
    let (_, d4) = fire_with(&outage);
    assert!(d4.termination_reason.is_none());

    // AC4-HP: the recorded consecutive count held at 2 across the outage.
    let events = fs::read_to_string(&events_path).unwrap();
    let last_check = events
        .lines()
        .filter(|l| l.contains("\"loop_check\"") && l.contains("sess-freeze"))
        .next_back()
        .expect("loop_check event for fire 4");
    let v: serde_json::Value = serde_json::from_str(last_check).unwrap();
    assert_eq!(
        v.pointer("/data/consecutive_unchanged")
            .and_then(|x| x.as_u64()),
        Some(2),
        "outage fires must hold the count at K=2; event: {last_check}"
    );

    // Fire 5: gh recovers with the SAME fingerprint -> streak resumes from
    // K=2 -> 3 -> backstop trips -> done() runs (codex missing) -> NoProgress.
    let (_, d5) = fire_with(&healthy);
    assert_eq!(
        d5.termination_reason.as_deref(),
        Some("NoProgress"),
        "streak must resume from K after recovery (AC4-FR): {}",
        d5.message
    );
}

/// Locked decision 6 (REVERSES the wedge): backstop tripped + done() gh
/// error -> block-and-retry with the read named, NEVER NoProgress.
#[test]
fn ac4_err_done_read_failure_never_no_progress() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-rev1659", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // Pre-read endpoints (state/checks/reviews) healthy; Read 4 (pulls/)
    // fails -> only the done() path errors.
    let dir = TempDir::new().unwrap();
    let gh = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":12,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000012"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo 'API rate limit exceeded' >&2
  exit 1
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000012""#,
    );

    let args = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];

    // Fires 1-2: quiet healthy blocks (streak 1, 2).
    let (_, d1) = fire(&args);
    assert_eq!(d1.decision, "block");
    let (_, d2) = fire(&args);
    assert_eq!(d2.decision, "block");

    // Fire 3: streak hits N=3 -> backstop trips -> done() runs -> Read 4
    // fails. The wedge terminated NoProgress here; step 2 blocks-and-retries.
    let (_, d3) = fire(&args);
    assert_eq!(
        d3.decision, "block",
        "gh-errored done() must block, not terminate: {}",
        d3.message
    );
    assert!(
        d3.termination_reason.is_none(),
        "REVERSED: no NoProgress on a gh-errored done() read; got {:?}",
        d3.termination_reason
    );
    assert!(
        d3.message.contains("pulls_comments"),
        "block must name the failing read; got: {}",
        d3.message
    );

    let events = fs::read_to_string(&events_path).unwrap();
    assert!(events.contains("loop_check_gh_error"));
    assert!(
        !events.contains("\"termination\""),
        "no termination event during the gh error; events: {events}"
    );

    // AC4-UI: the gh-error event records the failed read AND the stderr tail.
    let err_event = events
        .lines()
        .find(|l| l.contains("loop_check_gh_error"))
        .expect("loop_check_gh_error event");
    let v: serde_json::Value = serde_json::from_str(err_event).unwrap();
    assert_eq!(
        v.pointer("/data/read").and_then(|x| x.as_str()),
        Some("pulls_comments")
    );
    assert!(
        v.pointer("/data/stderr_tail")
            .and_then(|x| x.as_str())
            .map(|s| s.contains("rate limit"))
            .unwrap_or(false),
        "stderr_tail must carry the gh error text; event: {err_event}"
    );
}

/// AC4-EDGE: budget still terminates during a sustained outage (the outage
/// never makes a session immortal).
#[test]
fn ac4_edge_budget_ceiling_during_outage() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // Wall cap 60min, session is 2h old -> Budget, even though gh is down.
    fs::write(
        &manifest_path,
        manifest_with_budget("sess-outbudget", "2026-06-05T00:00:00Z", Some(60), None),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let outage = MockBins::failing_gh();

    let (_, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T02:00:00Z",
        &format!("--gh-bin={}", outage.gh.display()),
        &format!("--git-bin={}", outage.git.display()),
    ]);

    assert_eq!(d.decision, "allow");
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "budget must remain the ceiling during an outage"
    );
}

/// AC6-SAT: the #447 regression round-trip. The exact shape PR #447 had
/// (green CI, codex COMMENTED review, P1 root comment with
/// in_reply_to_id == null) PLUS the reply /check-pr's Step 8a posts
/// (in_reply_to_id set to the finding's id, non-bot login, commit named)
/// terminates DonePRGreen - and the SAME world minus the reply computes
/// reviewed=false. Proves the per-thread writer is load-bearing, not
/// cosmetic.
#[test]
fn ac6_sat_round_trip_reply_is_load_bearing() {
    // The reply row mirrors what `gh api .../comments -F in_reply_to=9001`
    // produces on a subsequent fetch.
    let with_reply = r#"[
  {"id": 9001, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Unpushed-head check missing",
   "path": "crates/fno-agents/src/loopcheck.rs", "line": 1560,
   "created_at": "2026-06-05T01:10:00Z"},
  {"id": 9002, "in_reply_to_id": 9001,
   "user": {"login": "bllshttng"},
   "body": "Fixed in 1a2b3c4d: head_oid now compared against local HEAD.",
   "created_at": "2026-06-05T01:25:00Z"}
]"#;
    let without_reply = r#"[
  {"id": 9001, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Unpushed-head check missing",
   "path": "crates/fno-agents/src/loopcheck.rs", "line": 1560,
   "created_at": "2026-06-05T01:10:00Z"}
]"#;
    let commits = r#"{"commits":[{"committedDate":"2026-06-05T01:30:00Z"}]}"#;

    // Arm 1: reply present -> addressed -> DonePRGreen.
    let tmp1 = findings_cwd("sess-sat-yes");
    let mock1 = findings_mock(with_reply, commits);
    let (_, d1) = fire_findings(tmp1.path(), &mock1);
    assert_eq!(
        d1.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "the /check-pr-shaped reply must satisfy the gate: {}",
        d1.message
    );

    // Arm 2: identical world, reply absent -> reviewed=false -> block.
    let tmp2 = findings_cwd("sess-sat-no");
    let mock2 = findings_mock(without_reply, commits);
    let (_, d2) = fire_findings(tmp2.path(), &mock2);
    assert_eq!(
        d2.decision, "block",
        "without the reply the same world must NOT pass (writer is load-bearing): {}",
        d2.message
    );
    assert!(d2.termination_reason.is_none());
    assert!(
        d2.message.contains("loopcheck.rs:1560"),
        "block names the finding; got: {}",
        d2.message
    );
}

/// sigma-review fix pin: a fire whose lightweight pre-read failed but whose
/// done() reads succeed must keep the CARRIED fingerprint (frozen streak),
/// not rebuild one from the pre-read's stale none|none components.
#[test]
fn prefail_done_success_keeps_carried_fingerprint() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-prefail", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // Healthy mock, but codex missing -> blocks (not done).
    let healthy = green_gemini_only_reviewed();

    // Pre-read-only failure: the fp pre-read queries headRefName WITHOUT
    // headRefOid; done()'s Read 1 includes headRefOid. Fail only the former.
    let dir = TempDir::new().unwrap();
    let gh_prefail = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefOid"; then
  echo '{"state":"OPEN","number":9,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000009"}'
  exit 0
fi
if echo "$*" | grep -q "headRefName"; then
  echo 'connect: network is unreachable' >&2
  exit 1
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000009""#,
    );

    let args_healthy = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", healthy.gh.display()),
        &format!("--git-bin={}", healthy.git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];
    let args_prefail = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:35:00Z",
        &format!("--gh-bin={}", gh_prefail.display()),
        &format!("--git-bin={}", git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];

    // Fires 1-2: healthy blocks with identical fingerprints (streak 1, 2).
    let (_, d1) = fire(&args_healthy);
    assert_eq!(d1.decision, "block");
    let (_, d2) = fire(&args_healthy);
    assert_eq!(d2.decision, "block");

    // Fire 3: pre-read fails, done() succeeds (codex still missing).
    let (_, d3) = fire(&args_prefail);
    assert_eq!(d3.decision, "block");
    assert!(d3.termination_reason.is_none());
    assert_eq!(
        d3.fingerprint, d1.fingerprint,
        "carried fingerprint must survive; a none|none rebuild leaked from the failed pre-read"
    );

    // The frozen count (2) is recorded, not a recount against a phantom fp.
    let events = fs::read_to_string(&events_path).unwrap();
    let last_check = events
        .lines()
        .filter(|l| l.contains("\"loop_check\"") && l.contains("sess-prefail"))
        .next_back()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(last_check).unwrap();
    assert_eq!(
        v.pointer("/data/consecutive_unchanged")
            .and_then(|x| x.as_u64()),
        Some(2),
        "streak must stay frozen at 2; event: {last_check}"
    );
}

/// Concurrency (the #447-motivating composition): quiet clean fires build a
/// streak; a late inline P1 lands; the backstop-tripping fire must RE-BLOCK
/// naming the finding (fingerprint advanced by the finding's timestamp), not
/// terminate NoProgress.
#[test]
fn late_finding_after_clean_fires_reblocks_not_noprogress() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    // Unattended: N = 3.
    fs::write(
        &manifest_path,
        new_manifest("sess-latefind", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    // Quiet session: no promise - the backstop is what trips done().
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // Phase 1 mock: green, codex reviewed, NO findings, but head MISMATCH so
    // done-but-mute cannot terminate DonePRGreen mid-test.
    let dir = TempDir::new().unwrap();
    let mk_gh = |comments_file: &str| {
        format!(
            r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{{"state":"OPEN","number":13,"headRefName":"feat","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000013"}}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{{"name":"ci","state":"SUCCESS","bucket":"pass"}}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  cat "{comments_file}"
  exit 0
fi
if echo "$*" | grep -q "commits"; then
  echo '{{"commits":[]}}'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{{"reviews":[{{"author":{{"login":"chatgpt-codex-connector"}},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}}],"comments":[]}}'
  exit 0
fi
exit 1
"#
        )
    };
    let empty_comments = dir.path().join("empty.json");
    fs::write(&empty_comments, "[]").unwrap();
    let p1_comments = dir.path().join("p1.json");
    fs::write(
        &p1_comments,
        r#"[
  {"id": 500, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Late finding",
   "path": "src/late.rs", "line": 8, "created_at": "2026-06-05T01:45:00Z"}
]"#,
    )
    .unwrap();
    let gh_clean = make_script(
        dir.path(),
        "gh-clean",
        &mk_gh(empty_comments.to_str().unwrap()),
    );
    let gh_p1 = make_script(dir.path(), "gh-p1", &mk_gh(p1_comments.to_str().unwrap()));
    // Local HEAD matches the PR head so the block reason reaches the
    // findings leg (fires 1-2 are quiet and never run done(), so the green
    // world cannot terminate early).
    let git = make_script(
        dir.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000013""#,
    );

    let args_for = |gh: &std::path::Path| {
        [
            "loop-check".to_string(),
            "--state".to_string(),
            manifest_path.to_str().unwrap().to_string(),
            "--transcript".to_string(),
            transcript_path.to_str().unwrap().to_string(),
            "--cwd".to_string(),
            cwd.to_str().unwrap().to_string(),
            "--now".to_string(),
            "2026-06-05T02:00:00Z".to_string(),
            format!("--gh-bin={}", gh.display()),
            format!("--git-bin={}", git.display()),
            "--events".to_string(),
            events_path.to_str().unwrap().to_string(),
        ]
    };

    // Fire 1: clean, streak 1 < MUTE_PROBE_N -> quiet block, done() not run.
    let owned1 = args_for(&gh_clean);
    let refs1: Vec<&str> = owned1.iter().map(|s| s.as_str()).collect();
    let (_, d1) = fire(&refs1);
    assert_eq!(d1.decision, "block");

    // Fire 2: streak 2 hits the mute probe (ab-223d2dae D) -> done() runs and
    // NOW sees the late P1 (created 01:45 > the 01:00 review ts). The
    // advanced fingerprint must convert the would-be termination into a
    // re-block that names the finding - never NoProgress, never DonePRGreen.
    let owned2 = args_for(&gh_p1);
    let refs2: Vec<&str> = owned2.iter().map(|s| s.as_str()).collect();
    let (_, d2) = fire(&refs2);
    assert_eq!(
        d2.decision, "block",
        "late finding must re-block, not terminate: {}",
        d2.message
    );
    assert!(
        d2.termination_reason.is_none(),
        "late finding must not resolve as NoProgress; got {:?}",
        d2.termination_reason
    );
    assert!(
        d2.message.contains("src/late.rs:8"),
        "re-block names the late finding; got: {}",
        d2.message
    );
    assert!(
        d2.fingerprint
            .as_deref()
            .unwrap_or("")
            .ends_with("|2026-06-05T01:45:00Z"),
        "the finding's timestamp must advance the fingerprint; got {:?}",
        d2.fingerprint
    );
}

/// AC2-EDGE: an advisory finding (P2 / unparseable severity) does not block.
#[test]
fn ac2_edge_advisory_finding_does_not_block() {
    let comments = r#"[
  {"id": 100, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "![P2 Badge](https://img.shields.io/badge/P2-yellow?style=flat) Nit",
   "path": "src/x.rs", "line": 7, "created_at": "2026-06-05T01:10:00Z"},
  {"id": 102, "in_reply_to_id": null,
   "user": {"login": "chatgpt-codex-connector[bot]"},
   "body": "no badge at all, just prose",
   "path": "src/y.rs", "line": 9, "created_at": "2026-06-05T01:11:00Z"}
]"#;
    let tmp = findings_cwd("sess-p2");
    let mock = findings_mock(comments, r#"{"commits":[]}"#);

    let (_, d) = fire_findings(tmp.path(), &mock);
    assert_eq!(
        d.decision, "allow",
        "advisory findings must not block: {}",
        d.message
    );
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));
}

// ── AC2: budget cap comment-tail tolerance (ab-610d2ee3) ─────────────────────

/// Build a manifest with raw (hand-crafted) budget lines, bypassing the
/// fixture builder that always writes clean numeric values.
fn manifest_with_raw_budget(session_id: &str, created_at: &str, extra_lines: &str) -> String {
    format!(
        "---\nsession_id: {session_id}\ncreated_at: {created_at}\nattended: true\n{extra_lines}---\n"
    )
}

/// AC2-HP: verbatim production corruption - `budget_cost_cap_usd: 200# Auto-merge inputs`
/// must parse as 200 (high cap, no ledger cost -> NOT a Budget termination).
#[test]
fn ac2_hp_budget_cost_cap_comment_glued_no_space() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // Verbatim production corruption: comment glued with no preceding space
    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2hp",
            "2026-06-05T00:00:00Z",
            "budget_cost_cap_usd: 200# Auto-merge inputs\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    // Cap must parse as 200 (no ledger cost -> no budget trip)
    assert_ne!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "corrupted manifest `budget_cost_cap_usd: 200# Auto-merge inputs` must NOT \
         trip budget (cap parsed as 200, no cost in ledger); got termination_reason={:?}, \
         message={}",
        d.termination_reason,
        d.message
    );
}

/// AC2-EDGE: space-separated comment `budget_cost_cap_usd: 200 # comment` parses as 200.
#[test]
fn ac2_edge_budget_cost_cap_comment_with_space() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2edge-space",
            "2026-06-05T00:00:00Z",
            "budget_cost_cap_usd: 200 # comment\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_ne!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "space-separated comment `budget_cost_cap_usd: 200 # comment` must NOT trip budget; \
         got termination_reason={:?}",
        d.termination_reason
    );
}

/// AC2-EDGE: wall-clock cap with glued comment `budget_wall_clock_cap_minutes: 90# Auto-merge inputs`
/// parses as 90 (cap not exceeded at 30 min elapsed -> NOT Budget).
#[test]
fn ac2_edge_budget_wall_cap_comment_glued() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // 90-minute cap, only 30 min elapsed -> must NOT trip wall-clock budget
    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2edge-wall",
            "2026-06-05T00:00:00Z",
            "budget_wall_clock_cap_minutes: 90# Auto-merge inputs\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z", // 30 min elapsed, cap=90 -> not exceeded
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_ne!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "wall-clock `budget_wall_clock_cap_minutes: 90# Auto-merge inputs` at 30 min elapsed \
         must NOT trip budget (cap=90 not exceeded); got termination_reason={:?}",
        d.termination_reason
    );
}

/// AC2-ERR: `budget_cost_cap_usd: abc` still classifies as Budget (fail closed).
#[test]
fn ac2_err_budget_cost_cap_malformed_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2err",
            "2026-06-05T00:00:00Z",
            "budget_cost_cap_usd: abc\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    // Non-numeric value (even after stripping) must still fail closed as Budget
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "malformed `budget_cost_cap_usd: abc` must fail closed as Budget; \
         got termination_reason={:?}",
        d.termination_reason
    );
}

/// AC2-EDGE degenerate: `budget_cost_cap_usd: # Auto-merge inputs` strips to empty -> fail closed.
#[test]
fn ac2_edge_budget_cost_cap_value_only_comment_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // Value is just a comment with no number before the '#'
    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2edge-empty",
            "2026-06-05T00:00:00Z",
            "budget_cost_cap_usd: # Auto-merge inputs\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    // Strips to empty -> non-numeric -> fail closed as Budget
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "degenerate `budget_cost_cap_usd: # Auto-merge inputs` (value strips to empty) \
         must fail closed as Budget; got termination_reason={:?}",
        d.termination_reason
    );
}

/// AC2-MED value-pinning trip: `budget_wall_clock_cap_minutes: 5# comment` strips to 5
/// and a session 30+ min old genuinely exceeds it -> Budget termination on wall_clock axis.
/// This proves the '#'-strip yields the real numeric value (5), not merely a non-Err result.
///
/// Verification method: reasoning - the strip path splits on '#' and trims, turning
/// "5# comment" into "5", which parses as u64(5). With created_at=T+0 and --now=T+31min,
/// elapsed=31 > cap=5, so the wall-clock detector fires Budget. If the strip were absent
/// (raw value "5# comment" passed to parse), it would fail-closed to Budget but for the
/// wrong reason (parse error). We distinguish by using a cap LOW enough that a working
/// strip triggers the VALUE path: if the result were parse-error fail-closed we'd still
/// get Budget, but we can't distinguish - so we additionally assert the message contains
/// "wall_clock" to confirm the real detector fired, not just a parse-error fallback.
#[test]
fn ac2_med_budget_wall_cap_comment_strip_value_pins_trip() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    // Cap is 5 minutes with a glued comment; session started at T+0; --now is T+31min
    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2med-wall-trip",
            "2026-06-05T00:00:00Z",
            "budget_wall_clock_cap_minutes: 5# comment\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:31:00Z", // 31 min elapsed, cap=5 -> genuinely exceeded
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "`budget_wall_clock_cap_minutes: 5# comment` with 31 min elapsed must trip Budget \
         (strip yields real cap=5, not parse-error fail-closed); got termination_reason={:?}, \
         message={}",
        d.termination_reason,
        d.message
    );
    // Confirm the wall_clock detector fired (not a parse-error fallback)
    assert!(
        d.message.contains("wall_clock"),
        "Budget message must mention wall_clock axis to confirm value-path fired, not parse-error \
         fallback; got message={}",
        d.message
    );
}

/// AC2-LOW degenerate: `budget_wall_clock_cap_minutes: # Auto-merge inputs` strips to empty
/// -> non-numeric -> fail closed as Budget. Mirrors the cost-side degenerate case; closes
/// the asymmetry between the two budget cap arms.
#[test]
fn ac2_low_budget_wall_cap_empty_after_strip_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2low-wall-empty",
            "2026-06-05T00:00:00Z",
            "budget_wall_clock_cap_minutes: # Auto-merge inputs\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "degenerate `budget_wall_clock_cap_minutes: # Auto-merge inputs` (strips to empty) \
         must fail closed as Budget; got termination_reason={:?}",
        d.termination_reason
    );
}

/// AC2-LOW non-'#' junk boundary: `budget_cost_cap_usd: 200x` is not a '#'-tail comment
/// and must fail closed as Budget. Locks that tolerance is '#'-tail ONLY; a future
/// over-broad strip (e.g. stripping all non-numeric chars) would break this test.
#[test]
fn ac2_low_budget_cost_cap_non_hash_junk_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    fs::write(
        &manifest_path,
        manifest_with_raw_budget(
            "sess-ac2low-junk",
            "2026-06-05T00:00:00Z",
            "budget_cost_cap_usd: 200x\n",
        ),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();

    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(
        d.termination_reason.as_deref(),
        Some("Budget"),
        "non-'#' junk `budget_cost_cap_usd: 200x` must fail closed as Budget \
         (tolerance is '#'-tail ONLY); got termination_reason={:?}",
        d.termination_reason
    );
}
