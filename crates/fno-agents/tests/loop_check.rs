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
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
  exit 0
fi
exit 1
"#,
        );
        let git = make_script(
            dir.path(),
            "git",
            r#"case "$*" in
  # A test env has no real repo, so the freshness identity must be
  # UNCOMPUTABLE. Without this the stub answers `git diff --raw` with the
  # same one line at every sha, which compares equal to itself and
  # fabricates a carry out of nothing - the absence-matched-against-
  # absence shape the predicate exists to refuse. Scoped to --raw so
  # `git diff --name-only` (classify_payload) behaves exactly as before.
  *--raw*) exit 1 ;;
  *) echo "deadbeefdeadbeefdeadbeefdeadbeef00000001" ;;
esac"#,
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
            r#"case "$*" in
  # A test env has no real repo, so the freshness identity must be
  # UNCOMPUTABLE. Without this the stub answers `git diff --raw` with the
  # same one line at every sha, which compares equal to itself and
  # fabricates a carry out of nothing - the absence-matched-against-
  # absence shape the predicate exists to refuse. Scoped to --raw so
  # `git diff --name-only` (classify_payload) behaves exactly as before.
  *--raw*) exit 1 ;;
  *) echo "deadbeefdeadbeefdeadbeefdeadbeef00000001" ;;
esac"#,
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
  echo '{"reviews":[{"author":{"login":"codex[bot]"},"state":"APPROVED","submittedAt":"2026-06-05T01:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
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
    // Pin the streak debounce OFF so this suite keeps counting FIRES, which is
    // what every backstop assertion here was written against: these tests drive
    // decide() in-process and fire within the same second, so the production
    // 300s gap would collapse each burst to a single observation and no
    // backstop could ever trip. The gap logic itself is covered by the
    // read_prior_fires unit tests in src/loopcheck.rs, which pass `now` and the
    // gap explicitly and so need no env at all. Same idempotent-set,
    // never-unset discipline as FNO_NUDGE_DISABLED above: every test in this
    // binary sets the identical value, so parallel execution is deterministic.
    std::env::set_var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS", "0");
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
    // Hermeticity, third door: recent_secondary_refusal reads global_events,
    // which defaults to the developer's real ~/.fno/events.jsonl. This very
    // suite runs under a live footnote session whose own stop-hook fires
    // write secondary-refusal rows to that real file all session long, so an
    // unisolated test silently inherits an unrelated stand-down. A case that
    // wants to test the secondary-refusal path passes its own
    // `--global-events`, and this skips.
    if !args.iter().any(|a| a.starts_with("--global-events")) {
        args_owned.push("--global-events".to_string());
        args_owned.push("/nonexistent/global-events.jsonl".to_string());
    }
    // Hermeticity, second door: the author harness came from ambient env
    // markers, so these cases passed in CI (no marker) and failed under
    // `cargo test` run from inside Claude Code, where the marker floors a
    // self-review reviewer the review-gate cases do not expect. A case that
    // wants a real harness passes its own `--author-harness`, and this skips.
    if !args.iter().any(|a| a.starts_with("--author-harness")) {
        args_owned.push("--author-harness".to_string());
        args_owned.push("none".to_string());
    }
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

/// AC7-OBS (x-6231): every emitted loop_check event carries `streak_window_secs`
/// beside `consecutive_unchanged`. Without it a "streak=5" line is unfalsifiable
/// from the log, which is how the fire-counting defect survived 127 terminations.
/// Asserted on EVERY loop_check record so a new emit site cannot quietly omit it.
#[test]
fn streak_window_secs_is_emitted_on_every_loop_check_event() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-window", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();
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

    // Two fires so both the first-fire and the has-priors path emit.
    fire(&args);
    fire(&args);

    let content = fs::read_to_string(&events_path).unwrap();
    let mut seen = 0;
    for line in content.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v.get("type").and_then(|t| t.as_str()) != Some("loop_check") {
            continue;
        }
        seen += 1;
        assert!(
            v.pointer("/data/streak_window_secs")
                .and_then(|w| w.as_i64())
                .is_some(),
            "loop_check event missing streak_window_secs: {line}"
        );
    }
    assert!(
        seen >= 2,
        "expected at least 2 loop_check events, got {seen}"
    );
}

/// x-1680: the resolved Stop-hook block cap is recorded exactly once, on the
/// first fire of a session, so a run ended by the harness override (blocks
/// whose running count meets the cap, then silence) is distinguishable from a
/// budget end (a terminal budget decision).
#[test]
fn loop_check_config_emitted_once_at_first_fire() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");

    fs::write(
        &manifest_path,
        new_manifest("sess-cap", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let mock = MockBins::no_pr();
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

    fire(&args);
    fire(&args);

    let content = fs::read_to_string(&events_path).unwrap();
    let mut config: Vec<serde_json::Value> = Vec::new();
    for line in content.lines() {
        let Ok(v): serde_json::Result<serde_json::Value> = serde_json::from_str(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("loop_check_config") {
            config.push(v);
        }
    }
    assert_eq!(
        config.len(),
        1,
        "loop_check_config must fire exactly once (first fire), got {}",
        config.len()
    );
    let cap = config[0]
        .pointer("/data/block_cap")
        .and_then(|c| c.as_i64())
        .expect("loop_check_config missing block_cap");
    assert!(cap >= 1, "block_cap must be a positive integer, got {cap}");
    let source = config[0]
        .pointer("/data/block_cap_source")
        .and_then(|s| s.as_str())
        .expect("loop_check_config missing block_cap_source");
    assert!(
        matches!(source, "env" | "default"),
        "block_cap_source must be env|default, got {source}"
    );
    // The default branch (env unset) must report the harness default of 9.
    if source == "default" {
        assert_eq!(cap, 9, "default block_cap must be the harness default 9");
    }
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
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"APPROVED","submittedAt":"2026-06-05T02:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
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
    fs::write(
        &settings_path,
        "[ci]\ndeclared_none = true\n[review]\nself_review_required = false\n",
    )
    .unwrap();

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
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
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
    // x-0eaf: the fixture explicitly opts out of the local self-review floor,
    // so the no-review configuration reaches DonePRGreen.
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));
}

/// x-2e20: a hosted workflow revokes declared no-CI before its first check exists.
#[test]
fn detected_workflow_revokes_declared_no_ci() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::create_dir_all(cwd.join(".github/workflows")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &manifest_path,
        new_manifest("sess-new-ci", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();
    fs::write(&settings_path, "[ci]\ndeclared_none = true\n").unwrap();
    fs::write(cwd.join(".github/workflows/ci.yml"), "name: ci\n").unwrap();

    let dir = TempDir::new().unwrap();
    let gh = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":5,"headRefName":"main","headRefOid":"dddddddddddddddddddddddddddddddddddddddd"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then echo '[]'; exit 0; fi
if echo "$*" | grep -q "pulls/"; then echo '[]'; exit 0; fi
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
        r#"echo "dddddddddddddddddddddddddddddddddddddddd""#,
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

    assert_eq!(d.decision, "block");
    assert!(d.message.contains("no CI checks found"));
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
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
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
    let global_dir = tmp.path().join("global_fno");
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
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
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
///
/// x-b167: chatgpt-codex-connector is now nudgeable by default, so a missing-codex
/// block renders the NeedsNudge message (name the bot + the mention it needs)
/// rather than the old passive "has not reviewed" line - the one install whose
/// behavior changes on upgrade is exactly one pinning a login footnote nudges.
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
    // x-b167: the nudgeable-bot block names the mention it needs, not "has not
    // reviewed". (The mock gh has no `pr comment` handler, so the runtime post
    // fails and the bot stays NeedsNudge with the post-by-hand command shown.)
    assert!(
        d.message.contains("has not been asked") && d.message.contains("gh pr comment"),
        "block message must render the NeedsNudge nudge instruction; got: {}",
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
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"COMMENTED","submittedAt":"2026-06-05T00:50:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git1 = make_script(
        dir1.path(),
        "git",
        r#"case "$*" in
  # A test env has no real repo, so the freshness identity must be
  # UNCOMPUTABLE. Without this the stub answers `git diff --raw` with the
  # same one line at every sha, which compares equal to itself and
  # fabricates a carry out of nothing - the absence-matched-against-
  # absence shape the predicate exists to refuse. Scoped to --raw so
  # `git diff --name-only` (classify_payload) behaves exactly as before.
  *--raw*) exit 1 ;;
  *) echo "deadbeefdeadbeefdeadbeefdeadbeef00000001" ;;
esac"#,
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
  echo '{{"reviews":[{{"author":{{"login":"chatgpt-codex-connector"}},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}}}],"comments":[]}}'
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

/// x-f8d4 AC2-HP: an OPEN operator `review_finding` holds the success terminal
/// even with a green + reviewed PR and a promise; an explicit `resolve` clears
/// it and the next fire terminates DonePRGreen. The manifest is never mutated.
#[test]
fn operator_review_finding_blocks_until_resolved() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    // graph_node_id lets the gate scope findings to this node.
    let manifest = format!(
        "{}graph_node_id: x-gate\n",
        new_manifest("sess-finding", "2026-06-05T00:00:00Z", true)
    );
    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, &manifest).unwrap();
    fs::write(cwd.join("transcript.jsonl"), transcript_with_promise()).unwrap();

    // Seed one OPEN finding for x-gate.
    let events = cwd.join(".fno/events.jsonl");
    fs::write(
        &events,
        "{\"ts\":\"t\",\"type\":\"review_finding\",\"source\":\"observer\",\"data\":{\"finding_id\":\"f9\",\"node\":\"x-gate\",\"text\":\"operator says fix the retry\"}}\n",
    )
    .unwrap();

    let mock = MockBins::green();
    let manifest_before = fs::read(&manifest_path).unwrap();

    let (code, d) = fire_findings(cwd, &mock);
    assert_eq!(code, 0);
    assert_eq!(
        d.decision, "block",
        "an open operator finding must hold the gate even on a green PR: {}",
        d.message
    );
    assert!(d.termination_reason.is_none());
    assert!(
        d.message.contains("f9") && d.message.contains("fno annotate resolve f9"),
        "reason must quote the finding id + resolve remedy; got: {}",
        d.message
    );
    assert_eq!(
        manifest_before,
        fs::read(&manifest_path).unwrap(),
        "target-state.md must not be mutated"
    );

    // Resolve it -> the gate clears and the promise terminates DonePRGreen.
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&events).unwrap();
    writeln!(
        f,
        "{{\"ts\":\"t2\",\"type\":\"review_finding_resolved\",\"source\":\"observer\",\"data\":{{\"finding_id\":\"f9\"}}}}"
    )
    .unwrap();
    drop(f);

    let (_, d2) = fire_findings(cwd, &mock);
    assert_eq!(
        d2.decision, "allow",
        "a resolved finding must no longer block: {}",
        d2.message
    );
    assert_eq!(d2.termination_reason.as_deref(), Some("DonePRGreen"));
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
        d.message.contains("wontfix:") && d.message.contains("Reply in-thread"),
        "message must name the remedy; got: {}",
        d.message
    );
    // A no-reply finding is the top-level-comment blind spot this message
    // exists for: it names the missing mechanism, not just the remedy.
    assert!(
        d.message.contains("no in-thread reply"),
        "no-reply message must name the missing mechanism; got: {}",
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
        "[review]\nrequired_bots = []\nself_review_required = false\n",
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
        "declared no-review repo must complete (not block) on PR+CI alone: {}",
        d.message
    );
    // x-0eaf: the fixture explicitly opts out of local self-review, so zero
    // coverage is the configured state rather than a defect.
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
    fs::write(&settings_path, "[review]\nrequired_bots = \"gemini\"\n").unwrap();

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
        "no_external must skip review per-session (not block): {}",
        d.message
    );
    // x-0eaf: no_external skipped the bot read and no local attestation exists,
    // so nothing reviewed -> DoneUnreviewed. The point of this test (no_external
    // does not BLOCK) holds: the session completes (allow).
    assert_eq!(d.termination_reason.as_deref(), Some("DoneUnreviewed"));
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
    fs::write(&settings_path, "[review]\nreviewers = [\"sigma\"]\n").unwrap();

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

    // The message, not just the verdict. Every unit test builds a PrInfo
    // literal, so `read_pr_info` could stop threading the unattested list
    // through (`unattested_reviewers: Vec::new()`) and every one of them would
    // still pass while the reason silently reverted to the generic fallback -
    // the exact failure this node exists to delete. This is the only assertion
    // in the suite that runs the real wiring end to end.
    assert!(
        d.message.contains("reviewers gate unmet"),
        "block reason must name the reviewers gate: {}",
        d.message
    );
    assert!(
        d.message.contains("sigma") && d.message.contains("/fno:review sigma"),
        "block reason must name the reviewer and its invocation: {}",
        d.message
    );
    assert!(
        !d.message.contains("bot reviewer"),
        "block reason must not blame a bot: {}",
        d.message
    );
    assert!(
        !d.message.contains("<watching"),
        "an unmet local gate is work to do, never an idle: {}",
        d.message
    );
}

/// AC3-HP / AC8-HP: the gate clears once a head-pinned `review_attestation`
/// (reviewer sigma, verdict pass, head_sha == current HEAD) exists.
#[test]
fn reviewers_gate_clears_with_head_pinned_attestation() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    let settings_path = cwd.join(".fno/config.toml");
    fs::write(&settings_path, "[review]\nreviewers = [\"sigma\"]\n").unwrap();
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
    fs::write(&settings_path, "[review]\nreviewers = [\"sigma\"]\n").unwrap();

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
        "[review]\nrequired_bots = []\nself_review_required = false\n",
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
    // x-0eaf: the explicit self-review opt-out makes fire 1 a no-review lane;
    // fire 2 tests re-enforcement after a lane is restored.
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
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
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
  echo '{"reviews":[{"author":{"login":"gemini-code-assist[bot]"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}],"comments":[]}'
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
  echo '{{"reviews":[{{"author":{{"login":"chatgpt-codex-connector"}},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z","commit":{{"oid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}}}],"comments":[]}}'
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

// ── done_probes: operational evidence at the ship gate (x-e54c) ───────────────

/// Build a plan doc with an optional `done_probes` frontmatter block.
fn plan_doc(probes: &[&str]) -> String {
    let mut s = String::from("---\ntitle: p\nstatus: ready\n");
    if !probes.is_empty() {
        s.push_str("done_probes:\n");
        for p in probes {
            s.push_str(&format!("  - \"{p}\"\n"));
        }
    }
    s.push_str("---\n\n# plan\n");
    s
}

fn manifest_with_plan(session_id: &str, created_at: &str, plan_path: &Path) -> String {
    format!(
        "---\nsession_id: {session_id}\ncreated_at: {created_at}\nattended: true\nplan_path: {}\n---\n",
        plan_path.display()
    )
}

/// Stage a probe-gated session: returns (cwd tempdir, manifest, transcript, plan).
fn probe_fixture(session_id: &str, probes: &[&str]) -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let plan = cwd.join("plan.md");
    fs::write(&plan, plan_doc(probes)).unwrap();

    let manifest = cwd.join("target-state.md");
    fs::write(
        &manifest,
        manifest_with_plan(session_id, "2026-06-05T00:00:00Z", &plan),
    )
    .unwrap();

    let transcript = cwd.join("transcript.jsonl");
    fs::write(&transcript, transcript_with_promise()).unwrap();

    (tmp, manifest, transcript)
}

fn fire_probe_gate(cwd: &Path, manifest: &Path, transcript: &Path, mock: &MockBins) -> Decision {
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest.to_str().unwrap(),
        "--transcript",
        transcript.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);
    d
}

/// AC1-HP: every other conjunct holds and the declared probe exits 0 ->
/// DonePRGreen, with this fire's probe result recorded in the loop_check event.
#[test]
fn done_probes_ac1_hp_passing_probe_grants_done() {
    let (tmp, manifest, transcript) = probe_fixture("sess-probe-hp", &["exit 0"]);
    let cwd = tmp.path();
    let mock = MockBins::green();

    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(d.decision, "allow");
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "a passing probe must not change the verdict: {}",
        d.message
    );

    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    assert!(
        events.contains("\"done_probes\""),
        "probe evidence must be recorded in the loop_check event"
    );
    assert!(
        events.contains("\"exit 0\":\"pass\""),
        "the probe result for THIS fire must be recorded: {events}"
    );
}

/// AC2-HP: a plan with no `done_probes` field takes the byte-identical path -
/// no probe machinery, verdict unchanged, and no probe evidence in the event.
#[test]
fn done_probes_ac2_hp_absent_field_leaves_gate_unchanged() {
    let (tmp, manifest, transcript) = probe_fixture("sess-probe-none", &[]);
    let cwd = tmp.path();
    let mock = MockBins::green();

    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("DonePRGreen"));

    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    assert!(
        events.contains("\"done_probes\":null"),
        "no declaration must record a null, never a fabricated 0/0: {events}"
    );
}

/// AC1-ERR: a failing probe refuses done, and the reason carries BOTH the
/// verbatim command and the literal exit code.
#[test]
fn done_probes_ac1_err_failing_probe_refuses_done() {
    let (tmp, manifest, transcript) =
        probe_fixture("sess-probe-fail", &["test -f /nonexistent-groom-report"]);
    let cwd = tmp.path();
    let mock = MockBins::green();

    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(
        d.decision, "block",
        "a green PR with a failing probe must NOT be done: {}",
        d.message
    );
    assert_eq!(d.termination_reason, None);
    assert!(
        d.message.contains("test -f /nonexistent-groom-report"),
        "reason must name the verbatim probe: {}",
        d.message
    );
    assert!(
        d.message.contains("exited 1"),
        "reason must carry the literal exit code: {}",
        d.message
    );
}

/// AC2-ERR: a probe whose binary does not exist fails closed as 127, stated
/// explicitly - a generic "probe failed" would leave the agent guessing.
#[test]
fn done_probes_ac2_err_missing_binary_states_127() {
    let (tmp, manifest, transcript) =
        probe_fixture("sess-probe-127", &["fno-no-such-binary-xyz --check"]);
    let cwd = tmp.path();
    let mock = MockBins::green();

    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(d.decision, "block");
    assert!(
        d.message.contains("exited 127"),
        "reason must state exit code 127 explicitly: {}",
        d.message
    );
}

/// AC2-EDGE: probes are the FINAL conjunct - a red-CI PR must never spawn one.
#[test]
fn done_probes_ac2_edge_no_probe_runs_while_ci_is_red() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let sentinel = cwd.join("probe-ran");
    let plan = cwd.join("plan.md");
    fs::write(&plan, plan_doc(&[&format!("touch {}", sentinel.display())])).unwrap();

    let manifest = cwd.join("target-state.md");
    fs::write(
        &manifest,
        manifest_with_plan("sess-probe-ordering", "2026-06-05T00:00:00Z", &plan),
    )
    .unwrap();
    let transcript = cwd.join("transcript.jsonl");
    fs::write(&transcript, transcript_with_promise()).unwrap();

    let mock = MockBins::ci_red();
    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(d.decision, "block");
    assert!(
        !sentinel.exists(),
        "no probe subprocess may run while an earlier conjunct is already false"
    );
}

/// AC1-HP (the clause the substring assertion above cannot pin): the gate must
/// run probes on THIS fire. A prior fire's recorded pass must never satisfy it.
#[test]
fn done_probes_a_prior_fires_pass_never_satisfies_this_fire() {
    let (tmp, manifest, transcript) =
        probe_fixture("sess-probe-cache", &["test -f /nonexistent-groom-report"]);
    let cwd = tmp.path();

    // Seed a prior fire in which the very same probe passed.
    fs::write(
        cwd.join(".fno/events.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "loop_check",
                "data": {
                    "session_id": "sess-probe-cache",
                    "done_probes": {"test -f /nonexistent-groom-report": "pass"}
                }
            })
        ),
    )
    .unwrap();

    let mock = MockBins::green();
    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(
        d.decision, "block",
        "a cached pass from an earlier fire must not grant done: {}",
        d.message
    );
    assert!(d.message.contains("test -f /nonexistent-groom-report"));
}

/// The block path must record probe evidence too - it is the fire most likely
/// to have run probes, and both the grader and the fail-closed history read it.
#[test]
fn done_probes_block_path_records_evidence_in_the_event() {
    let (tmp, manifest, transcript) = probe_fixture("sess-probe-evt", &["exit 3"]);
    let cwd = tmp.path();
    let mock = MockBins::green();

    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);
    assert_eq!(d.decision, "block");

    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap();
    assert!(
        events.contains("\"exit 3\":\"fail:3\""),
        "a failing probe's result must be recorded, not just its reason: {events}"
    );
}

/// A declared field this parser cannot read must refuse, never silently pass.
#[test]
fn done_probes_unparseable_declaration_refuses_done() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let plan = cwd.join("plan.md");
    // A multi-line inline list: declared, but not a shape the gate can read.
    fs::write(
        &plan,
        "---\ntitle: p\ndone_probes: [\n  \"echo a\"\n]\n---\n\n# plan\n",
    )
    .unwrap();
    let manifest = cwd.join("target-state.md");
    fs::write(
        &manifest,
        manifest_with_plan("sess-probe-unparseable", "2026-06-05T00:00:00Z", &plan),
    )
    .unwrap();
    let transcript = cwd.join("transcript.jsonl");
    fs::write(&transcript, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(
        d.decision, "block",
        "an unreadable declaration must not pass as 'no probes': {}",
        d.message
    );
    assert!(
        d.message.contains("undeterminable"),
        "reason must say undeterminable: {}",
        d.message
    );
}

/// AC13-INV (x-a534): a project-local `done_probes` must survive the
/// global-plus-local merge.
///
/// That merge is a hand-written per-field list, so a field omitted from it is
/// read from the GLOBAL file only and the project-local value is silently
/// dropped. For a guardrail key that is a silent bypass: the operator declares
/// a repo-wide probe, sees no error, and the gate never runs it. Driven through
/// the real merge (not `fire`, which pins `--global-settings /nonexistent`)
/// because the parser is not where that bug would live.
#[test]
fn done_probes_ac13_inv_project_local_survives_the_global_merge() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();

    // GLOBAL declares no probes at all - the value the merge would fall back to.
    let global = cwd.join("global.toml");
    fs::write(
        &global,
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();
    // PROJECT-LOCAL declares the guardrail. Flat root, not a `config` table.
    fs::write(
        cwd.join(".fno/config.toml"),
        "done_probes = [\"false\"]\n[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();

    let manifest = cwd.join("target-state.md");
    fs::write(
        &manifest,
        new_manifest("sess-probe-overlay", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    let transcript = cwd.join("transcript.jsonl");
    fs::write(&transcript, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let args: Vec<String> = vec![
        "loop-check".into(),
        "--state".into(),
        manifest.to_str().unwrap().into(),
        "--transcript".into(),
        transcript.to_str().unwrap().into(),
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

    assert_eq!(
        d.decision, "block",
        "a project-local probe list dropped by the merge is a silent guardrail \
         bypass; got: {json_str}"
    );
    assert!(
        d.message.contains("project probe `false`"),
        "the block must name the failing project probe: {}",
        d.message
    );
}

/// AC3-INV (x-a534) end to end: a plan declaring `done_probes: []` must not
/// switch off the repo-wide guardrail. A guard a plan doc can silence is a
/// guard on one of two reachable paths.
#[test]
fn done_probes_ac3_inv_a_plan_cannot_silence_the_project_gate() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    std::env::set_var("FNO_NUDGE_DISABLED", "1");
    fs::write(
        cwd.join(".fno/config.toml"),
        "done_probes = [\"false\"]\n[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();

    let plan = cwd.join("plan.md");
    fs::write(&plan, "---\ntitle: p\ndone_probes: []\n---\n\nbody\n").unwrap();
    let manifest = cwd.join("target-state.md");
    fs::write(
        &manifest,
        manifest_with_plan("sess-probe-silence", "2026-06-05T00:00:00Z", &plan),
    )
    .unwrap();
    let transcript = cwd.join("transcript.jsonl");
    fs::write(&transcript, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let d = fire_probe_gate(cwd, &manifest, &transcript, &mock);

    assert_eq!(
        d.decision, "block",
        "an explicit `done_probes: []` in a plan must not disable the project's \
         guardrail: {}",
        d.message
    );
    assert!(
        d.message.contains("project probe"),
        "the block must name the project source: {}",
        d.message
    );
}

/// AC6-HP (x-a534): a REGISTERED reviewer gates and is satisfied by its own
/// attestation.
///
/// The design's claim is that the Rust side is already name-agnostic, so
/// opening `config.review.reviewers` to a project-registered name needs zero
/// Rust changes. That claim comes from reading `unattested_reviewers_scan`,
/// and a guard verified only by reading is a guard verified on one of N paths.
/// This runs it: a name footnote does not author must both HOLD the gate when
/// unattested and CLEAR it when attested at HEAD.
#[test]
fn reviewers_gate_is_name_agnostic_for_a_registered_reviewer() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    std::env::set_var("FNO_NUDGE_DISABLED", "1");

    // The registry block is Python-side config; Rust reads only the names.
    let settings_path = cwd.join(".fno/config.toml");
    fs::write(
        &settings_path,
        "[review]\nreviewers = [\"my-security-skill\"]\n\n\
         [review.reviewer_registry.my-security-skill]\n\
         kind = \"harness-skill\"\nrequires = \"skill\"\n\
         invocation = \"/my-security-skill\"\nasserts = \"invocation\"\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-rvw-registry", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = MockBins::green();
    let args = |cwd: &Path| -> Vec<String> {
        vec![
            "loop-check".into(),
            "--state".into(),
            manifest_path.to_str().unwrap().into(),
            "--transcript".into(),
            transcript_path.to_str().unwrap().into(),
            "--cwd".into(),
            cwd.to_str().unwrap().into(),
            "--now".into(),
            "2026-06-05T00:30:00Z".into(),
            "--settings".into(),
            settings_path.to_str().unwrap().into(),
            format!("--gh-bin={}", mock.gh.display()),
            format!("--git-bin={}", mock.git.display()),
            "--global-settings".into(),
            "/nonexistent".into(),
        ]
    };

    // Unattested: the gate holds and names the reviewer.
    let (_c, json_str) = fno_agents::loopcheck::run_loop_check_capture(&args(cwd));
    let d: Decision = serde_json::from_str(&json_str).unwrap();
    assert_eq!(
        d.decision, "block",
        "an unattested registered reviewer must hold the gate: {json_str}"
    );
    assert!(
        d.message.contains("my-security-skill"),
        "the block must name the reviewer: {}",
        d.message
    );

    // Attested at HEAD: the gate clears, with no Rust-side allowlist involved.
    fs::write(
        cwd.join(".fno/events.jsonl"),
        format!(
            "{{\"ts\":\"2026-06-05T00:10:00Z\",\"type\":\"review_attestation\",\"source\":\"target\",\"data\":{{\"reviewer\":\"my-security-skill\",\"head_sha\":\"{GREEN_HEAD}\",\"verdict\":\"pass\"}}}}\n"
        ),
    )
    .unwrap();
    let (_c, json_str) = fno_agents::loopcheck::run_loop_check_capture(&args(cwd));
    let d: Decision = serde_json::from_str(&json_str).unwrap();
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "a head-pinned attestation from a registered reviewer must clear the \
         gate without any Rust-side table entry: {json_str}"
    );
}

// ── x-b167 nudge give-up policy, driven end-to-end through decide() ───────────
//
// The helper-level tests in loopcheck.rs pin classification/idle/message; these
// drive the real decision function so the bot_nudges -> post-loop -> backstop
// give-up wiring is exercised, not just its pieces (sigma integration-test gap).
// The mock gh controls the `pr comment` outcome, so no env var is needed to keep
// the suite from posting to a real PR.

/// A gh mock for a PR where the required codex bot has NOT reviewed. `comments`
/// is the JSON array body for `reviews,comments`; `comment_exit` is the exit
/// code of `gh pr comment` (0 = post lands, non-0 = post fails); when `marker`
/// is set, each `pr comment` call appends a line to it so a test can count posts.
fn nudge_mock(comments_json: &str, comment_exit: i32, marker: Option<&Path>) -> MockBins {
    let dir = TempDir::new().unwrap();
    let record = match marker {
        Some(p) => format!("echo x >> '{}'; ", p.display()),
        None => String::new(),
    };
    let body = format!(
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "pr comment"; then {record}exit {comment_exit}; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then echo '[{{"name":"ci","state":"SUCCESS","bucket":"pass"}}]'; exit 0; fi
if echo "$*" | grep -q "pulls/"; then echo '[]'; exit 0; fi
if echo "$*" | grep -q "reviews"; then echo '{{"reviews":[],"comments":{comments_json}}}'; exit 0; fi
exit 1
"#
    );
    let gh = make_script(dir.path(), "gh", &body);
    let git = make_script(
        dir.path(),
        "git",
        r#"case "$*" in
  # A test env has no real repo, so the freshness identity must be
  # UNCOMPUTABLE. Without this the stub answers `git diff --raw` with the
  # same one line at every sha, which compares equal to itself and
  # fabricates a carry out of nothing - the absence-matched-against-
  # absence shape the predicate exists to refuse. Scoped to --raw so
  # `git diff --name-only` (classify_payload) behaves exactly as before.
  *--raw*) exit 1 ;;
  *) echo "deadbeefdeadbeefdeadbeefdeadbeef00000001" ;;
esac"#,
    );
    MockBins { _dir: dir, gh, git }
}

/// AC10-HP: a NeedsNudge bot with a successful runtime post lands exactly one
/// `gh pr comment` and the block message reports the bot as nudged and awaiting.
#[test]
fn nudge_needs_nudge_posts_once_and_reports_awaiting() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let marker = cwd.join("posts.txt");
    fs::write(
        &manifest_path,
        new_manifest("sess-nudge-post", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // No review and no mention -> NeedsNudge -> post succeeds -> Awaiting.
    let mock = nudge_mock("[]", 0, Some(&marker));

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
    assert_eq!(d.decision, "block", "got: {}", d.message);
    assert!(
        d.message.contains("nudged") && d.message.contains("awaiting"),
        "post landed -> message must report nudged + awaiting; got: {}",
        d.message
    );
    let posts = fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        posts.lines().count(),
        1,
        "exactly one gh pr comment must be issued; got: {posts:?}"
    );
}

/// AC11-ERR: a failed runtime post keeps the bot NeedsNudge (message tells the
/// agent to post by hand), leaves the count unchanged, and emits a failure event.
#[test]
fn nudge_failed_post_keeps_needs_nudge_and_emits_event() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-nudge-fail", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // No review, no mention, and `gh pr comment` exits non-zero.
    let mock = nudge_mock("[]", 1, None);

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
        "--events",
        events_path.to_str().unwrap(),
    ]);

    assert_eq!(d.decision, "block", "got: {}", d.message);
    assert!(
        d.message
            .contains("gh pr comment 1 --body \"@codex review\""),
        "a failed post must leave the by-hand command in the reason; got: {}",
        d.message
    );
    let events = fs::read_to_string(&events_path).unwrap_or_default();
    assert!(
        events.contains("loop_check_nudge_post_failed"),
        "a failed post must emit loop_check_nudge_post_failed; events: {events}"
    );
}

/// AC4-CON + AC13-CON: three timed-out mentions with no review -> Unresponsive ->
/// the NoProgress backstop reaps it on the 3rd unattended fire, and the
/// termination message names the bot and the nudge count rather than a bare
/// fingerprint streak. An Unresponsive bot never posts, so no gh comment fires.
#[test]
fn nudge_unresponsive_bot_gives_up_on_backstop() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    // Suppress the give-up OS notification so the test spawns no `fno inbox notify`.
    std::env::set_var("FNO_LOOPCHECK_NO_NOTIFY", "1");

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    // Unattended -> backstop N=3.
    fs::write(
        &manifest_path,
        new_manifest("sess-nudge-giveup", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // Three @codex review mentions, newest at 00:00:00; --now is 60m later, well
    // past the 15m default wait -> Unresponsive at ceiling 3.
    let comments = r#"[
        {"author":{"login":"human-a"},"body":"@codex review","createdAt":"2026-06-04T23:00:00Z"},
        {"author":{"login":"human-b"},"body":"please @codex review","createdAt":"2026-06-04T23:30:00Z"},
        {"author":{"login":"human-c"},"body":"@codex review","createdAt":"2026-06-05T00:00:00Z"}
    ]"#;
    let mock = nudge_mock(comments, 0, None);

    let args = [
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
        "--events",
        events_path.to_str().unwrap(),
    ];

    let (_c1, d1) = fire(&args);
    assert_eq!(d1.decision, "block", "fire 1: {}", d1.message);
    let (_c2, d2) = fire(&args);
    assert_eq!(d2.decision, "block", "fire 2: {}", d2.message);
    let (_c3, d3) = fire(&args);
    assert_eq!(
        d3.decision, "allow",
        "fire 3 must trip the backstop: {}",
        d3.message
    );
    assert_eq!(d3.termination_reason.as_deref(), Some("NoProgress"));
    assert!(
        d3.message.contains("chatgpt-codex-connector") && d3.message.contains("3 nudges"),
        "the give-up termination must name the bot and the nudge count; got: {}",
        d3.message
    );
}

/// Codex P1 (overlay): a project-local `[review.nudge]` override must reach the
/// resolver via the global+local merge. `enabled = false` here opts the repo
/// out, so the missing codex bot is NotNudgeable and NO comment is posted.
#[test]
fn nudge_project_local_disable_is_honored() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    std::env::set_var("FNO_NUDGE_DISABLED", "1");
    fs::write(
        cwd.join(".fno/config.toml"),
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n\
         [review.nudge]\n\"chatgpt-codex-connector\" = { enabled = false }\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let marker = cwd.join("posts.txt");
    fs::write(
        &manifest_path,
        new_manifest("sess-nudge-optout", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let mock = nudge_mock("[]", 0, Some(&marker));

    let (_c, d) = fire(&[
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

    assert_eq!(d.decision, "block", "got: {}", d.message);
    assert!(
        d.message.contains("has not reviewed"),
        "an opted-out bot must render today's passive block, not a nudge; got: {}",
        d.message
    );
    let posts = fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        posts.trim().is_empty(),
        "enabled=false must suppress the post entirely; got: {posts:?}"
    );
}

/// Codex P1 (backstop): a freshly-nudged bot in Awaiting must NOT be reaped by
/// the generic NoProgress backstop before its wait window elapses, even on a
/// harness that cannot idle - the nudge cycle would otherwise be cut short.
#[test]
fn nudge_awaiting_defers_the_backstop() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    // Unattended -> backstop N=3.
    fs::write(
        &manifest_path,
        new_manifest("sess-nudge-await", "2026-06-05T00:00:00Z", false),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // One mention 5m before --now: newest is well inside the 15m wait -> Awaiting.
    let comments = r#"[{"author":{"login":"human"},"body":"@codex review","createdAt":"2026-06-05T00:00:00Z"}]"#;
    let mock = nudge_mock(comments, 0, None);

    let args = [
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:05:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--events",
        events_path.to_str().unwrap(),
    ];

    let (_c1, d1) = fire(&args);
    assert_eq!(d1.decision, "block", "fire 1: {}", d1.message);
    let (_c2, d2) = fire(&args);
    assert_eq!(d2.decision, "block", "fire 2: {}", d2.message);
    let (_c3, d3) = fire(&args);
    // Without the guard this would trip NoProgress on fire 3; the awaiting wait
    // is self-limiting and must keep blocking until it turns Unresponsive.
    assert_eq!(
        d3.decision, "block",
        "an awaiting nudge must defer the backstop: {}",
        d3.message
    );
    assert!(
        d3.termination_reason.is_none(),
        "got: {:?}",
        d3.termination_reason
    );
}

// ── coverage classifier (x-0eaf task 1.1) ────────────────────────────────────

use fno_agents::loopcheck::{
    classify_coverage, coverage_receipt_line, unattested_reviewers_scan, AttestationOrigin,
    AttestationScope, Coverage, CoverageProducer, CoverageReport, CoverageVerdict, Freshness,
};

const COV_HEAD: &str = "abc1234567890abcdef1234567890abcdef1234";

/// A review object as `gh pr view --json reviews` returns one, including the
/// `commit.oid` the review was submitted against. That field is not decoration:
/// coverage reads it to decide whether the verdict still describes HEAD, and a
/// fixture without it exercises the absent-commit path rather than the one the
/// test means to (x-5b99). Defaults to COV_HEAD, so a bot review is fresh
/// unless a test says otherwise.
fn gh_review(author: &str, state: &str) -> serde_json::Value {
    gh_review_at(author, state, COV_HEAD)
}

fn gh_review_at(author: &str, state: &str, oid: &str) -> serde_json::Value {
    serde_json::json!({"author": {"login": author}, "state": state,
                       "submittedAt": "2026-08-05T10:00:00Z", "commit": {"oid": oid}})
}

/// The bare sha equality the freshness predicate replaced, as a resolver. The
/// pre-x-5b99 coverage tests below run against it unchanged: with no carry ever
/// granted, the new code must reproduce the old verdicts exactly.
fn at_head(sha: &str) -> Freshness {
    if !sha.is_empty() && sha == COV_HEAD {
        Freshness::Fresh
    } else {
        Freshness::Stale
    }
}

fn gh_comment(author: &str, body: &str) -> serde_json::Value {
    serde_json::json!({"author": {"login": author}, "body": body, "createdAt": "2026-08-05T10:00:00Z"})
}

fn attestation_line(reviewer: &str, head: &str, verdict: &str) -> String {
    serde_json::json!({
        "type": "review_attestation",
        "data": {"reviewer": reviewer, "head_sha": head, "verdict": verdict}
    })
    .to_string()
}

/// Like `attestation_line` but naming the branch the attestation is about -
/// the scoping field every post-x-e601 producer stamps. A legacy line (no
/// branch) is admitted only on exact head equality, so a test exercising a
/// MOVED head must scope by branch or its fixture silently vanishes.
fn attestation_line_on_branch(reviewer: &str, head: &str, verdict: &str, branch: &str) -> String {
    serde_json::json!({
        "type": "review_attestation",
        "data": {"reviewer": reviewer, "head_sha": head, "verdict": verdict, "branch": branch}
    })
    .to_string()
}

// ── attestation scope (x-e601: which PR an attestation is about) ─────────────
//
// The events journal is shared across every worktree by design, so an
// unscoped scan reads every branch's attestations into every PR's verdict
// list. These tests pin the scoping predicate through BOTH consumers.

/// AC1-HP: an attestation recorded on ANOTHER branch at a DIFFERENT head never
/// reaches this PR's verdict list at all - that is the cherry-pick /
/// duplicate-PR shape where the old global scan read a foreign pass as
/// coverage via CarriedBaseSync (equal shas never carry; they are Fresh before
/// any identity is computed), so this pins that scope, not freshness, is what
/// stops it.
#[test]
fn attestation_scope_foreign_branch_is_absent_not_stale() {
    let foreign_head = "bbbb0000000000000000000000000000000000000";
    let events = attestation_line_on_branch("code-review", foreign_head, "pass", "feature/a");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::CarriedBaseSync,
        "feature/b",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(0));
    assert!(
        rep.verdicts.is_empty(),
        "a foreign-branch attestation must not appear as any verdict: {:?}",
        rep.verdicts
    );
}

/// The spawned-reviewer lane (review-lanes.md): the reviewer's worktree
/// necessarily carries a branch of its own - git refuses two worktrees on one
/// branch - so its exact-HEAD attestation must count for the author's PR
/// despite the branch mismatch. A foreign branch cannot share this head sha
/// without being this commit.
#[test]
fn attestation_scope_reviewer_worktree_branch_at_exact_head_counts() {
    let events = attestation_line_on_branch("code-review", COV_HEAD, "pass", "wt/reviewer-1");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        "feature/a",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.reviewed_sha, COV_HEAD);
}

/// The same-branch counterpart: in scope, counted, and labeled so a receipt
/// can say HOW the pass was scoped.
#[test]
fn attestation_scope_own_branch_counts_and_labels() {
    let events = attestation_line_on_branch("code-review", COV_HEAD, "pass", "feature/a");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        "feature/a",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.reviewed_sha, COV_HEAD);
    assert_eq!(local.scope, Some(AttestationScope::AttestedBranch));
}

/// AC1-EDGE, first half: a legacy line (no branch field) with its head
/// byte-equal to the PR head still counts, so no in-flight gate is wedged by
/// the new field. The verdict carries the legacy label.
#[test]
fn attestation_scope_legacy_exact_head_still_counts() {
    let events = attestation_line("code-review", COV_HEAD, "pass");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        "feature/a",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.scope, Some(AttestationScope::LegacyHeadMatch));
}

/// AC1-EDGE, second half: a legacy line on a DIFFERENT head is not evidence
/// about this PR - absent entirely, never a stale verdict to nudge on.
#[test]
fn attestation_scope_legacy_other_head_is_absent() {
    let old_head = "0000000000000000000000000000000000000000";
    let events = attestation_line("code-review", old_head, "pass");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        "feature/a",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(0));
    assert!(rep.verdicts.is_empty());
}

/// A PR read that returned no branch fails closed: an attestation at a
/// DIFFERENT head cannot be admitted by branch (the PR's branch is unknown),
/// so it does not count. An exact-head line still counts with an unknown PR
/// branch - a foreign branch cannot share this head sha without being this
/// commit - which is the reviewer-lane case above.
#[test]
fn attestation_scope_unknown_pr_branch_fails_closed() {
    let foreign_head = "bbbb0000000000000000000000000000000000000";
    let events = attestation_line_on_branch("code-review", foreign_head, "pass", "feature/a");
    let rep = classify_coverage(&[], &[], &events, &[], true, None, &at_head, "", COV_HEAD);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

/// AC1-EDGE2: one session attesting PR B after PR A no longer overwrites A's
/// pass. The map key is (reviewer, attester); before scoping, B's line landed
/// in the same key and A's gate read unreviewed with no trace of the deletion.
#[test]
fn attestation_scope_second_pr_does_not_clobber_the_first() {
    let head_a = "aaaa0000000000000000000000000000000000000";
    let head_b = "bbbb0000000000000000000000000000000000000";
    let events = format!(
        "{}\n{}",
        attestation_line_on_branch("code-review", head_a, "pass", "feature/a"),
        attestation_line_on_branch("code-review", head_b, "pass", "feature/b"),
    );
    let fresh_at_a = |sha: &str| {
        if sha == head_a {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &fresh_at_a,
        "feature/a",
        head_a,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(
        local.reviewed_sha, head_a,
        "feature/a must carry ITS OWN pass, not feature/b's head"
    );
}

/// THE false-positive specimen, built: one reviewer, one attester session,
/// its latest pass earned on PR A. PR B's evaluation would carry that sha
/// (the code-diff identity hashes equal, which a cherry-pick produces; a
/// documentation-only tree diff is the other trigger), and the old unscoped
/// scan keyed on (reviewer, attester) read the foreign pass as coverage for
/// a PR nobody reviewed. Scope, not freshness, must stop it.
#[test]
fn attestation_scope_cherry_picked_foreign_pass_never_counts() {
    let head_a = "aaaa0000000000000000000000000000000000000";
    let head_b = "bbbb0000000000000000000000000000000000000";
    let attested = serde_json::json!({
        "type": "review_attestation",
        "data": {"reviewer": "code-review", "head_sha": head_a, "verdict": "pass",
                 "attester_session_id": "reviewer-session-1", "branch": "feature/a"}
    })
    .to_string();
    // The resolver carries EVERY sha: the equal-delta shape the delta
    // predicate exists to grant, which is exactly the exposure.
    let carries_all = |_: &str| Freshness::CarriedBaseSync;
    let rep = classify_coverage(
        &[],
        &[],
        &attested,
        &[],
        true,
        None,
        &carries_all,
        "feature/b",
        head_b,
    );
    assert_eq!(
        rep.coverage,
        Coverage::Covered(0),
        "a carried foreign pass must not count for a PR nobody reviewed"
    );
    assert!(rep.verdicts.is_empty());

    // The same line, same carrying resolver, still counts for the PR it was
    // attested on - the relief x-62a1 exists to grant survives the scoping.
    let rep = classify_coverage(
        &[],
        &[],
        &attested,
        &[],
        true,
        None,
        &carries_all,
        "feature/a",
        head_b,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

/// AC2-HP + the N-reachable-paths rule: BOTH scans over the SAME foreign
/// attestation at a different head (the carry shape the scope predicate
/// exists to stop). The coverage axis (`classify_coverage`) and the
/// `config.review.reviewers` gate (`unattested_reviewers_scan`) must agree
/// that a foreign-branch line is not about this PR; a guard on one of the two
/// paths is decorative.
#[test]
fn attestation_scope_both_scans_agree_on_foreign_line() {
    let foreign_head = "bbbb0000000000000000000000000000000000000";
    let carried = |_: &str| Freshness::CarriedBaseSync;
    let events = attestation_line_on_branch("code-review", foreign_head, "pass", "feature/a");
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("events.jsonl");
    std::fs::write(&p, &events).unwrap();
    let reviewers = vec!["code-review".to_string()];

    let (unattested, _malformed) =
        unattested_reviewers_scan(&p, &reviewers, &carried, "feature/b", COV_HEAD);
    assert_eq!(
        unattested.len(),
        1,
        "the reviewers gate must NOT be satisfied by a foreign branch"
    );

    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &carried,
        "feature/b",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(0));
    assert!(rep.verdicts.is_empty());
}

/// Seed a head-pinned `code-review` pass attestation into `.fno/events.jsonl` so
/// the coverage classifier counts a local review. Used by tests whose PRIMARY
/// intent is not coverage (CI-skip, no_external, empty-config) but which now
/// need review coverage to reach DonePRGreen under the x-0eaf coverage gate.
fn seed_code_review_attestation(cwd: &Path, head_sha: &str) {
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    let events = cwd.join(".fno/events.jsonl");
    let mut existing = fs::read_to_string(&events).unwrap_or_default();
    existing.push_str(&attestation_line("code-review", head_sha, "pass"));
    existing.push('\n');
    fs::write(&events, existing).unwrap();
}

/// AC1-HP: only bot output is the usage-limit refusal -> coverage 0, refused.
#[test]
fn coverage_classify_quota_refusal_is_zero_coverage_refused() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        "You have reached your Codex usage limits for code reviews.",
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(0));
    let bot = rep
        .verdicts
        .iter()
        .find(|v| v.name == "chatgpt-codex-connector")
        .unwrap();
    assert_eq!(bot.producer, CoverageProducer::GithubApp);
    assert_eq!(bot.verdict, CoverageVerdict::Refused);
}

/// AC11-HP: one genuine review, no blocking findings -> coverage 1.
#[test]
fn coverage_classify_real_review_counts() {
    let reviews = vec![gh_review("chatgpt-codex-connector[bot]", "COMMENTED")];
    let rep = classify_coverage(
        &reviews,
        &[],
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

/// Boundary: zero configured reviewers and no evidence -> Covered(0), not an
/// error and not vacuous success.
#[test]
fn coverage_classify_zero_configured_is_covered_zero() {
    let rep = classify_coverage(&[], &[], "", &[], true, None, &at_head, "", COV_HEAD);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

/// AC5-ERR: reviews API failed, no local attestation -> Unknown.
#[test]
fn coverage_classify_github_read_failure_is_unknown_without_local() {
    let rep = classify_coverage(
        &[],
        &[],
        "",
        &["chatgpt-codex-connector".to_string()],
        false,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Unknown);
    assert_eq!(rep.coverage_count(), None);
}

/// Operator ruling + producer-axis correction: a head-pinned local attestation
/// makes coverage Known even when the github read failed. A bot quota outage
/// cannot wedge the autonomous path while a local lane reviewed.
#[test]
fn coverage_classify_local_pass_survives_github_outage() {
    let events = attestation_line("code-review", COV_HEAD, "pass");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &["chatgpt-codex-connector".to_string()],
        false,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.name, "code-review");
}

/// The producer-axis consequence (operator correction): a bot terminal-refused
/// does NOT imply zero coverage, because the local lane is a separate producer
/// over a separate channel with separate limits.
#[test]
fn coverage_classify_bot_refused_plus_local_is_covered() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        "You have reached your Codex usage limits for code reviews.",
    )];
    let events = attestation_line("code-review", COV_HEAD, "pass");
    let rep = classify_coverage(
        &[],
        &comments,
        &events,
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    assert_eq!(
        rep.verdicts
            .iter()
            .filter(|v| v.verdict == CoverageVerdict::Refused)
            .count(),
        1
    );
}

/// AC10-INV: a configured bot with an unrecognized comment (no review object,
/// no usage marker) is absent, never reviewed - keeps gemini's empty
/// usage_markers from rebuilding the bug for the second bot.
#[test]
fn coverage_classify_unrecognized_response_is_absent_never_reviewed() {
    let comments = vec![gh_comment(
        "gemini-code-assist[bot]",
        "something unrecognized",
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["gemini-code-assist".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    let g = rep
        .verdicts
        .iter()
        .find(|v| v.name == "gemini-code-assist")
        .unwrap();
    assert_eq!(g.verdict, CoverageVerdict::Absent);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

// ── clean-pass comments (x-e601: the PR #947 specimen) ───────────────────────
//
// chatgpt-codex-connector submits a formal review ONLY when it has findings;
// on a clean pass it posts a plain issue comment pinning the commit it read.
// Nothing read that comment, so a clean pass could never clear the gate and
// the gate was strictly easier to satisfy with a flawed PR than a clean one.

/// AC4-HP: the specimen comment verbatim (modulo the head being the test's
/// COV_HEAD) earns a Reviewed verdict pinned to that sha.
#[test]
fn clean_pass_comment_counts_as_a_head_pinned_review() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        &format!(
            "Codex Review: Didn't find any major issues. Bravo. Reviewed commit: {}",
            COV_HEAD
        ),
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let v = rep
        .verdicts
        .iter()
        .find(|v| v.name == "chatgpt-codex-connector")
        .unwrap();
    assert_eq!(v.verdict, CoverageVerdict::Reviewed);
    assert_eq!(v.reviewed_sha, COV_HEAD);
}

/// The typographic apostrophe the bot actually posts matches the marker too -
/// one is pinned here so a marker edit that drops either form fails loudly.
#[test]
fn clean_pass_marker_covers_the_typographic_apostrophe() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        &format!(
            "Codex Review: Didn\u{2019}t find any major issues. Reviewed commit: {}.",
            COV_HEAD
        ),
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

/// A re-read supersedes the older clean pass: comments arrive oldest-first,
/// so the selection must rank by freshness, not take the first match - a
/// first-match scan reads the bot's FIRST pass forever and no later re-read
/// can ever clear the gate through this lane.
#[test]
fn clean_pass_comment_reread_supersedes_the_older_one() {
    let old = "0000000000000000000000000000000000000000";
    let comments = vec![
        gh_comment(
            "chatgpt-codex-connector[bot]",
            &format!("Codex Review: Didn't find any major issues. Reviewed commit: {old}"),
        ),
        gh_comment(
            "chatgpt-codex-connector[bot]",
            &format!("Codex Review: Didn't find any major issues. Reviewed commit: {COV_HEAD}"),
        ),
    ];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let v = rep
        .verdicts
        .iter()
        .find(|v| v.name == "chatgpt-codex-connector")
        .unwrap();
    assert_eq!(v.verdict, CoverageVerdict::Reviewed);
    assert_eq!(v.reviewed_sha, COV_HEAD);
}

/// AC4-EDGE: a clean pass naming an older sha whose code no longer matches is
/// stale - recorded, excluded from the count, and nudgeable for a re-read.
#[test]
fn clean_pass_comment_at_an_older_sha_is_stale() {
    let old = "0000000000000000000000000000000000000000";
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        &format!("Codex Review: Didn't find any major issues. Reviewed commit: {old}"),
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(0));
    let v = rep
        .verdicts
        .iter()
        .find(|v| v.name == "chatgpt-codex-connector")
        .unwrap();
    assert_eq!(v.verdict, CoverageVerdict::Stale);
    assert_eq!(v.reviewed_sha, old);
}

/// AC4-EDGE: a clean-pass comment with no `Reviewed commit:` line stays
/// Absent. Nothing is head-pinned, so counting it would be the unpinned-
/// attestation hole the local axis already refuses; do not invent a sha.
#[test]
fn clean_pass_comment_with_no_pinned_sha_stays_absent() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        "Codex Review: Didn't find any major issues. Bravo.",
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(0));
    let v = rep
        .verdicts
        .iter()
        .find(|v| v.name == "chatgpt-codex-connector")
        .unwrap();
    assert_eq!(v.verdict, CoverageVerdict::Absent);
    assert_eq!(v.reviewed_sha, "");
}

/// A non-hex token after the marker is as good as no marker line at all.
#[test]
fn clean_pass_comment_with_a_non_hex_token_stays_absent() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        "Codex Review: Didn't find any major issues. Reviewed commit: tomorrow",
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    let v = rep
        .verdicts
        .iter()
        .find(|v| v.name == "chatgpt-codex-connector")
        .unwrap();
    assert_eq!(v.verdict, CoverageVerdict::Absent);
}

/// Ordering: a usage-limit comment means the bot did not read the code, so a
/// body carrying BOTH markers lands on Refused, never on a clean pass.
#[test]
fn usage_limit_marker_beats_a_clean_pass_marker() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        &format!(
            "You have reached the Codex usage limits for code reviews. \
             (An earlier note said: Didn't find any major issues. \
             Reviewed commit: {}.)",
            COV_HEAD
        ),
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    let v = rep
        .verdicts
        .iter()
        .find(|v| v.name == "chatgpt-codex-connector")
        .unwrap();
    assert_eq!(v.verdict, CoverageVerdict::Refused);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

/// gemini-code-assist has no MEASURED clean-pass shape, so no marker is
/// guessed for it: its unrecognized comment stays absent even though the
/// codex markers exist in the same table.
#[test]
fn no_clean_pass_marker_is_guessed_for_an_unmeasured_bot() {
    let comments = vec![gh_comment(
        "gemini-code-assist[bot]",
        "Didn't find any major issues. Bravo.",
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["gemini-code-assist".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    let g = rep
        .verdicts
        .iter()
        .find(|v| v.name == "gemini-code-assist")
        .unwrap();
    assert_eq!(g.verdict, CoverageVerdict::Absent);
}

/// A config's SHORT name ("codex") resolves the full-login profile, so the
/// clean-pass read does not silently vanish for the one spelling configs
/// actually use. The author login still carries the full form.
#[test]
fn clean_pass_markers_resolve_through_a_config_short_name() {
    let comments = vec![gh_comment(
        "chatgpt-codex-connector[bot]",
        &format!(
            "Codex Review: Didn't find any major issues. Reviewed commit: {}",
            COV_HEAD
        ),
    )];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["codex".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

/// The hedge: a human GitHub APPROVAL is recorded (human_approval: true) but
/// excluded from the count. Lean: exclude; one predicate flip includes it.
#[test]
fn coverage_classify_human_approval_excluded() {
    let reviews = vec![gh_review("jason", "APPROVED")];
    let rep = classify_coverage(&reviews, &[], "", &[], true, None, &at_head, "", COV_HEAD);
    let human = rep.verdicts.iter().find(|v| v.name == "jason").unwrap();
    assert!(human.human_approval);
    assert_eq!(human.verdict, CoverageVerdict::Reviewed);
    assert_eq!(rep.coverage, Coverage::Covered(0));
    assert_eq!(rep.coverage_count(), Some(0));
}

/// Boundary: a reviewer named in two lists is one verdict, not two units.
#[test]
fn coverage_classify_duplicate_login_one_verdict() {
    let reviews = vec![gh_review("chatgpt-codex-connector[bot]", "COMMENTED")];
    let logins = vec![
        "chatgpt-codex-connector".to_string(),
        "chatgpt-codex-connector".to_string(),
    ];
    let rep = classify_coverage(
        &reviews,
        &[],
        "",
        &logins,
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    assert_eq!(
        rep.verdicts
            .iter()
            .filter(|v| v.name == "chatgpt-codex-connector")
            .count(),
        1
    );
}

/// AC7-CON: a later pass at head supersedes an earlier fail (no cached refused).
#[test]
fn coverage_classify_local_fail_then_pass_latest_wins() {
    let events = format!(
        "{}\n{}\n",
        attestation_line("code-review", COV_HEAD, "fail"),
        attestation_line("code-review", COV_HEAD, "pass"),
    );
    let rep = classify_coverage(&[], &[], &events, &[], true, None, &at_head, "", COV_HEAD);
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

/// A later fail at head revokes an earlier pass.
#[test]
fn coverage_classify_local_pass_then_fail_revokes() {
    let events = format!(
        "{}\n{}\n",
        attestation_line("code-review", COV_HEAD, "pass"),
        attestation_line("code-review", COV_HEAD, "fail"),
    );
    let rep = classify_coverage(&[], &[], &events, &[], true, None, &at_head, "", COV_HEAD);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

/// A pass on a prior head does not count after a new commit lands (head-pinning).
#[test]
fn coverage_classify_stale_head_does_not_count() {
    let old_head = "0000000000000000000000000000000000000000";
    let events = attestation_line("code-review", old_head, "pass");
    let rep = classify_coverage(&[], &[], &events, &[], true, None, &at_head, "", COV_HEAD);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

/// The classifier is NAME-AGNOSTIC on the local axis: it counts a head-pinned
/// pass for ANY reviewer name, not only built-ins or configured entries. This
/// pins the load-bearing property that `emit-attestation.sh`'s "reviewer name
/// is NOT an allowlist" comment asserts on the producer side - so a future
/// allowlist added either here or there cannot silently stop `/code-review`
/// (or a future local lane like the codex CLI, attesting as "codex") from
/// counting under `reviewers: []`. A comment in a sibling script is not a guard.
#[test]
fn coverage_classify_local_pass_unconfigured_name_counts() {
    // "codex" here is the LOCAL CLI lane (local_attestation axis), deliberately
    // the same display name as the github_app bot - the producer axis field is
    // what keeps them distinct, and an unconfigured name still counts.
    let events = attestation_line("codex", COV_HEAD, "pass");
    let rep = classify_coverage(&[], &[], &events, &[], true, None, &at_head, "", COV_HEAD);
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.name, "codex");
}

/// x-9ab2 (usage-limit gate): a required review bot that bounces on quota now
/// fails the gate closed UNCONDITIONALLY - `all_required_passed` requires
/// `usage_limited` to be empty, with no attestation exception. So even when a
/// local /code-review attests at HEAD, a bounce terminates DoneAwaitingReview,
/// not DonePRGreen. The local attestation is not wasted: it clears
/// `unattested_reviewers` so `awaiting_review_only` holds, and the case exits
/// cleanly at DoneAwaitingReview instead of wedging to budget death. What is
/// given up is auto-SHIPPING on a bounce, not liveness.
///
/// This test was added by x-0eaf asserting DonePRGreen on the grounds that "a
/// quota-dead bot cannot block the path while a local lane reviewed". x-9ab2
/// supersedes that: when both apply, usage_limited wins (a bounce is an external
/// cause the operator acts on, and the codex connector is permanently
/// quota-exhausted, so a bounce is the steady state, not an anomaly). A flipped
/// assertion under the old doc comment would read as a bug to the next reader;
/// the comment and the rename carry the decision so it is not silently reverted.
#[test]
fn done_awaiting_review_when_required_bot_bounces_despite_local_attestation() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd); // required_bots = [chatgpt-codex-connector]

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-lane", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // gh: green CI; the bot posted ONLY a usage-limit refusal, no review object.
    let dir = TempDir::new().unwrap();
    let gh = make_script(
        dir.path(),
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":7,"headRefName":"main","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}'
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
  echo '{"reviews":[],"comments":[{"author":{"login":"chatgpt-codex-connector[bot]"},"body":"You have reached your Codex usage limits for code reviews.","createdAt":"2026-06-05T01:00:00Z"}]}'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir.path(),
        "git",
        r#"case "$*" in
  # A test env has no real repo, so the freshness identity must be
  # UNCOMPUTABLE. Without this the stub answers `git diff --raw` with the
  # same one line at every sha, which compares equal to itself and
  # fabricates a carry out of nothing - the absence-matched-against-
  # absence shape the predicate exists to refuse. Scoped to --raw so
  # `git diff --name-only` (classify_payload) behaves exactly as before.
  *--raw*) exit 1 ;;
  *) echo "deadbeefdeadbeefdeadbeefdeadbeef00000001" ;;
esac"#,
    );

    // A local /code-review pass attested at the current HEAD.
    seed_code_review_attestation(cwd, "deadbeefdeadbeefdeadbeefdeadbeef00000001");

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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);

    assert_eq!(d.decision, "allow");
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DoneAwaitingReview"),
        "a required-bot quota bounce fails closed even with a local attestation (x-9ab2): {}",
        d.message
    );
}

/// x-0eaf AC12-INV (negative): the coverage path must not read the `attended`
/// manifest field (x-be78: it lies for spawned workers). An attended:false
/// (spawned-worker) session with zero coverage terminates DoneUnreviewed exactly
/// like an attended:true one - the discriminator is coverage, not attendance.
#[test]
fn done_unreviewed_independent_of_attended_field() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd); // required_bots = [chatgpt-codex-connector]

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    // attended: false - the spawn-substrate shape x-be78 showed lies.
    let manifest =
        "---\nsession_id: sess-unatt\ncreated_at: 2026-06-05T00:00:00Z\nattended: false\n---\n";
    fs::write(&manifest_path, manifest).unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // gh: green CI, bot reviewed (so objection-gate passes); but NO local
    // attestation and we want to show attendance does not change the terminal.
    // Use green(): ccc COMMENTED review -> coverage 1 -> DonePRGreen regardless
    // of attended. Then a second case below uses no review -> DoneUnreviewed
    // regardless of attended. This case pins: attended:false + coverage 1 still
    // DonePRGreen (attendance does not downgrade).
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
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "attended:false must not downgrade a reviewed PR: {}",
        d.message
    );
}

/// x-0eaf AC2-CON: with auto_merge approved (autonomous consent) and zero
/// coverage, the terminal is DoneUnreviewed, not DonePRGreen - so finalize will
/// not arm auto-merge and the autonomous path refuses to merge unreviewed code.
/// The discriminator is coverage; auto_merge consent does not override it.
#[test]
fn ac2_con_auto_merge_approved_zero_coverage_refuses() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    let settings = cwd.join(".fno/config.toml");
    fs::write(
        &settings,
        "[review]\nrequired_bots = []\nself_review_required = false\n",
    )
    .unwrap();

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let manifest = "---\nsession_id: sess-auto\ncreated_at: 2026-06-05T00:00:00Z\nattended: false\nauto_merge_approved: true\n---\n";
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
        "--settings",
        settings.to_str().unwrap(),
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(d.decision, "allow");
    // x-0eaf boundary: the explicit no-review configuration reaches
    // DonePRGreen even with auto_merge consent.
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "no-lane config + auto_merge: {}",
        d.message
    );
}

// ── coverage receipt line (x-0eaf task 3.1) ──────────────────────────────────

#[test]
fn coverage_receipt_covered_names_reviewers() {
    // gh_review, not a hand-rolled object: a review with no `commit.oid`
    // exercises the absent-commit path (fail closed) rather than the reviewed
    // one this test is about.
    let rep = classify_coverage(
        &[gh_review("chatgpt-codex-connector", "COMMENTED")],
        &[],
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    let line = coverage_receipt_line(&rep, None);
    assert!(line.starts_with("review coverage: 1 reviewed ("), "{line}");
    assert!(line.contains("chatgpt-codex-connector"), "{line}");
}

#[test]
fn coverage_receipt_zero_names_refused_and_absent() {
    let comments = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector[bot]"},
        "body": "You have reached your Codex usage limits for code reviews."
    })];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &[
            "chatgpt-codex-connector".to_string(),
            "gemini-code-assist".to_string(),
        ],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    let line = coverage_receipt_line(&rep, None);
    assert!(line.contains("0 reviewed"), "{line}");
    assert!(line.contains("1 refused"), "{line}");
    assert!(line.contains("chatgpt-codex-connector"), "{line}");
    // Names the missing evidence. "Nothing reviewed this diff" stated the count
    // a second time and no cause, which is the vacuum a reader fills with
    // attestation_origin - so the line must not mention origin either.
    assert!(line.contains("No head-pinned pass attestation"), "{line}");
    assert!(!line.contains("origin"), "{line}");
    // gemini-code-assist is configured and silent, so it is NAMED as the thing
    // being waited on - not merely counted, and not conflated with the refused
    // reviewer, which is the only other name on the line.
    assert!(line.contains("waiting on gemini-code-assist"), "{line}");
    // Never prescribes the local verb while someone is outstanding. This line
    // cannot tell required from optional, and `fno do pr merge` reads coverage
    // alone, so a worker that self-attests past a required bot lands the PR
    // before its blocking finding posts.
    assert!(!line.contains("review verb"), "{line}");
    // Not a bare wait either: an optional App that is never installed sits
    // absent forever, so a move that is safe either way is always named.
    assert!(line.contains("check config.review"), "{line}");
    // Names appear once. The earlier form printed them in a count parenthetical
    // AND in the tail, and rendered "0 absent ()" when there were none.
    assert_eq!(line.matches("gemini-code-assist").count(), 1, "{line}");
    assert!(!line.contains("()"), "{line}");
}

/// A refusal is not an absence. The only configured reviewer hit its quota and
/// declined, so nothing is still coming and a local attestation IS the move -
/// the line prescribes the verb here, unlike the absent case above.
#[test]
fn coverage_receipt_zero_prescribes_the_verb_when_the_only_reviewer_refused() {
    let comments = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector[bot]"},
        "body": "You have reached your Codex usage limits for code reviews."
    })];
    let rep = classify_coverage(
        &[],
        &comments,
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &at_head,
        "",
        COV_HEAD,
    );
    let line = coverage_receipt_line(&rep, None);
    assert!(line.contains("1 refused"), "{line}");
    assert!(line.contains("0 absent"), "{line}");
    assert!(line.contains("run the review verb at HEAD"), "{line}");
}

#[test]
fn coverage_receipt_unknown_says_unknown() {
    let rep = CoverageReport {
        coverage: Coverage::Unknown,
        verdicts: vec![],
    };
    let line = coverage_receipt_line(&rep, None);
    assert!(
        line.contains("unknown"),
        "unknown must say unknown, not a number: {line}"
    );
    assert!(
        !line.contains("reviewed:"),
        "unknown must not present a count: {line}"
    );
}

// ── attestation origin (producer records the emitting process) ───────────────

/// Like `attestation_line` but stamps the harness session that emitted the
/// attestation - the field the authorship join keys on.
fn attestation_line_attested(reviewer: &str, head: &str, verdict: &str, attester: &str) -> String {
    serde_json::json!({
        "type": "review_attestation",
        "data": {"reviewer": reviewer, "head_sha": head, "verdict": verdict, "attester_session_id": attester}
    })
    .to_string()
}

const AUTHOR: &str = "author-session-aaaa";
const OTHER: &str = "reviewer-session-bbbb";

/// attester == authoring session -> SelfAttested, and the count is UNCHANGED.
/// This is the test that turns red the day someone adds
/// `&& origin != SelfAttested` to coverage_count: such a flip would drop every
/// self-attested review from the count, flipping every open PR to DoneUnreviewed
/// in one deploy. The count must not move with the origin.
#[test]
fn coverage_origin_self_attested_does_not_change_count() {
    let events = attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR);
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.attestation_origin, AttestationOrigin::SelfAttested);
    assert_eq!(rep.coverage, Coverage::Covered(1));
    assert_eq!(rep.coverage_count(), Some(1));
}

/// attester != authoring session -> OtherSession, count UNCHANGED. The middle
/// state is deliberately NOT "independent": a different session may be a
/// self-handoff successor or a shared-worktree sibling, still not independent.
#[test]
fn coverage_origin_other_session_is_not_independent_count_unchanged() {
    let events = attestation_line_attested("code-review", COV_HEAD, "pass", OTHER);
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.attestation_origin, AttestationOrigin::OtherSession);
    assert_eq!(rep.coverage, Coverage::Covered(1));
    assert_eq!(rep.coverage_count(), Some(1));
}

/// An attestation with no attester_session_id (the entire pre-landed backlog,
/// or a session with no env marker) -> Unknown, count UNCHANGED.
#[test]
fn coverage_origin_unknown_when_attester_empty_count_unchanged() {
    // attestation_line omits attester_session_id entirely, modeling the backlog.
    let events = attestation_line("code-review", COV_HEAD, "pass");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.attestation_origin, AttestationOrigin::Unknown);
    assert_eq!(rep.coverage_count(), Some(1));
}

/// No authoring session known (no manifest / unparseable) -> every local
/// verdict Unknown, failing open so the verdict set is byte-identical to the
/// pre-change behavior on unknown authorship. A present attester with an absent
/// author is Unknown, not Other: the comparison is never guessed.
#[test]
fn coverage_origin_author_unknown_is_unknown_fail_open() {
    let events = format!(
        "{}\n",
        attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR)
    );
    let rep = classify_coverage(&[], &[], &events, &[], true, None, &at_head, "", COV_HEAD);
    let local = rep
        .verdicts
        .iter()
        .find(|v| v.producer == CoverageProducer::LocalAttestation)
        .unwrap();
    assert_eq!(local.attestation_origin, AttestationOrigin::Unknown);
    assert_eq!(rep.coverage_count(), Some(1));
}

// ── pair-key: (reviewer, attester_session_id) ────────────────────────────────

/// AC1-HP: two passes at the same head under the same reviewer label but from
/// DIFFERENT attester sessions join instead of replacing. Before the pair key
/// the second `insert` overwrote the first and a peer-reviewed PR read
/// `1 reviewed` while deleting its own control. Now it reads two verdicts - one
/// `SelfAttested` (the author) and one `OtherSession` (the spawned peer) - and
/// coverage_count is 2. This is the load-bearing test for the spawned-reviewer
/// lane: it is what proves a non-author attestation is both producible AND
/// countable, not silently collapsed.
#[test]
fn coverage_two_sessions_same_reviewer_join_not_replace() {
    let events = format!(
        "{}\n{}\n",
        attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR),
        attestation_line_attested("code-review", COV_HEAD, "pass", OTHER),
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let locals: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(
        locals.len(),
        2,
        "two sessions must produce two verdicts, not one"
    );
    assert_eq!(
        locals
            .iter()
            .filter(|v| v.attestation_origin == AttestationOrigin::SelfAttested)
            .count(),
        1
    );
    assert_eq!(
        locals
            .iter()
            .filter(|v| v.attestation_origin == AttestationOrigin::OtherSession)
            .count(),
        1
    );
    assert_eq!(rep.coverage, Coverage::Covered(2));
    assert_eq!(rep.coverage_count(), Some(2));
}

/// AC2: two passes at the same head, same reviewer, SAME attester session
/// collapse to one entry - the dedup the name key was originally built for (a
/// same-session re-run must not double-count). Last-writer-wins within the pair,
/// so the later verdict is the survivor.
#[test]
fn coverage_same_session_same_reviewer_dedups_to_one() {
    let events = format!(
        "{}\n{}\n",
        attestation_line_attested("code-review", COV_HEAD, "fail", AUTHOR),
        attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR),
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let locals: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(locals.len(), 1);
    assert_eq!(rep.coverage_count(), Some(1));
}

/// AC3: a legacy corpus with no `attester_session_id` (the whole pre-landed
/// backlog) keys as `(name, None)` and is byte-identical to the old name-keyed
/// output. A same-name re-run collapses (latest verdict wins), distinct names
/// stay distinct, and every origin is Unknown. The pair key must not change the
/// shape of a corpus that predates the field.
#[test]
fn coverage_legacy_no_attester_byte_identical_to_name_key() {
    let events = format!(
        "{}\n{}\n{}\n",
        // same reviewer, no attester: re-run collapses, latest (pass) wins
        attestation_line("code-review", COV_HEAD, "fail"),
        attestation_line("code-review", COV_HEAD, "pass"),
        // a distinct reviewer with no attester: its own entry
        attestation_line("codex", COV_HEAD, "pass"),
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let locals: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(locals.len(), 2);
    assert!(locals
        .iter()
        .all(|v| v.attestation_origin == AttestationOrigin::Unknown));
    assert_eq!(rep.coverage_count(), Some(2));
}

/// AC4-ERR: an author pass at head plus a later peer FAIL at the same head from
/// a different attester does NOT revoke the author's pass. Under the pair key
/// the peer's fail touches only the peer's own `(name, peer)` slot, which never
/// held a pass; the author's `(name, author)` pass still counts. Coverage counts
/// reviews performed, not approvals granted - the hold on a bad peer review
/// lives on `open_review_findings` and on `unattested_reviewers_scan`'s name key
/// (unchanged), which is the deliberate divergence this design calls for.
#[test]
fn coverage_peer_fail_does_not_revoke_author_pass() {
    let events = format!(
        "{}\n{}\n",
        attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR),
        attestation_line_attested("code-review", COV_HEAD, "fail", OTHER),
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let locals: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(locals.len(), 1, "only the author's pass survives");
    assert_eq!(
        locals[0].attestation_origin,
        AttestationOrigin::SelfAttested
    );
    assert_eq!(rep.coverage_count(), Some(1));
}

/// The receipt names all three buckets so a reader learns the vocabulary even
/// when two are zero.
#[test]
fn coverage_receipt_names_origin_buckets() {
    let events = attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR);
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    let line = coverage_receipt_line(&rep, None);
    assert!(line.contains("self 1"), "{line}");
    assert!(line.contains("other 0"), "{line}");
    assert!(line.contains("unknown 0"), "{line}");
}

/// The origin tally folds EVERY reviewed (non-human) verdict by its
/// attestation_origin, so the three buckets sum to the reviewed count. A GitHub
/// App review has no session to compare and reads `unknown`; it is named in the
/// reviewed list, so "unknown" there is its origin, not its verdict.
#[test]
fn coverage_receipt_origin_tally_sums_to_reviewed_count() {
    let reviews = vec![gh_review("chatgpt-codex-connector[bot]", "COMMENTED")];
    let events = attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR);
    let rep = classify_coverage(
        &reviews,
        &[],
        &events,
        &["chatgpt-codex-connector".to_string()],
        true,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(2));
    let line = coverage_receipt_line(&rep, None);
    assert!(line.contains("self 1"), "{line}");
    assert!(line.contains("other 0"), "{line}");
    assert!(line.contains("unknown 1"), "{line}");
    assert!(line.contains("2 reviewed"), "{line}");
}

/// The receipt states that the tally is not a subtraction. Two workers read
/// `self 1` beside a review count as "one was dropped" and refused to merge
/// green, unblocked PRs; a bare tally next to a number invites arithmetic.
/// "all counted" is a positive claim rather than a disclaimer - a denial
/// ("not a gate") answers the question by raising it.
///
/// The claim is checked, not asserted: the three buckets must sum to the
/// reviewed count, so the line cannot say "all counted" while dropping one.
#[test]
fn coverage_receipt_states_the_tally_is_not_a_subtraction() {
    let events = attestation_line_attested("code-review", COV_HEAD, "pass", AUTHOR);
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        false,
        Some(AUTHOR),
        &at_head,
        "",
        COV_HEAD,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let line = coverage_receipt_line(&rep, None);

    // The affirmation, and the sole self-attested verdict it is affirming.
    // Scoped to ORIGINS: `n` does drop human approvals, so a bare "all counted"
    // would be false on a human-approved PR and re-open the very question the
    // line exists to close.
    assert!(line.contains("all origins counted"), "{line}");
    assert!(line.contains("1 reviewed"), "{line}");
    assert!(line.contains("self 1"), "{line}");

    // The affirmation is true: buckets sum to the reviewed count.
    let counted = rep.coverage_count().expect("covered");
    let (s, o, u) = (
        line_bucket(&line, "self "),
        line_bucket(&line, "other "),
        line_bucket(&line, "unknown "),
    );
    assert_eq!(s + o + u, counted, "{line}");

    // And no denial: naming a gate to rule it out is what teaches a reader a
    // gate exists.
    assert!(!line.contains("not a gate"), "{line}");
}

/// Pull `<label><n>` out of the receipt line for the sum check above.
fn line_bucket(line: &str, label: &str) -> usize {
    let tail = line
        .split(label)
        .nth(1)
        .unwrap_or_else(|| panic!("no {label:?} in {line}"));
    tail.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("no digits after {label:?} in {line}"))
}

// ── GraphQL quota floor + exhaustion naming ───────────────────────────────

/// A gh mock that answers ONLY `api rate_limit` (GraphQL bucket per `remaining`
/// in the script body) and `--version`, fails everything else, and logs every
/// invocation to `calls.log` so a test can prove which reads a fire spent.
fn quota_gh(dir: &Path, remaining: i64, green_pr_view: bool) -> PathBuf {
    let reset = chrono::Utc::now().timestamp() + 40 * 60;
    let pr_view_body = if green_pr_view {
        r#"if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}'
  exit 0
fi"#
    } else {
        "true"
    };
    make_script(
        dir,
        "gh",
        &format!(
            r#"echo "$*" >> "{calls}"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if [ "$1" = api ] && [ "$2" = rate_limit ]; then
  echo '{{"resources":{{"graphql":{{"remaining":{remaining},"reset":{reset}}}}}}}'
  exit 0
fi
{pr_view_body}
exit 1"#,
            calls = dir.join("calls.log").display(),
            remaining = remaining,
            reset = reset,
            pr_view_body = pr_view_body,
        ),
    )
}

/// Item 4: below the floor and carrying no promise intent, a fire spends NO
/// GraphQL at all - the stand-down must precede every `gh pr view`.
#[test]
fn floor_stand_down_spends_no_graphql() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-floor", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    let gh = quota_gh(cwd, 50, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(d.message.contains("standing down"), "got: {}", d.message);
    assert!(d.message.contains("floor"), "got: {}", d.message);
    let calls = fs::read_to_string(cwd.join("calls.log")).unwrap();
    assert!(calls.contains("api rate_limit"), "probe must run: {calls}");
    assert!(
        !calls.contains("pr view"),
        "no GraphQL spend below floor: {calls}"
    );
    // A stand-down fire verifies no PR state, so it has no real fingerprint.
    // It must NOT be recorded as a "loop_check" event: read_prior_fires scans
    // for that exact type and treats a missing fingerprint as an empty
    // string, which never matches current_fp and truncates the reverse-scan
    // the instant it hits this row - one stand-down silently breaks the
    // consecutive-unchanged streak for every earlier fire in the session.
    let events = fs::read_to_string(cwd.join(".fno/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("\"type\":\"loop_check_graphql_standdown\"")
            || events.contains("\"type\": \"loop_check_graphql_standdown\""),
        "stand-down must emit its own event type, not loop_check: {events}"
    );
    for line in events.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v.get("type").and_then(|t| t.as_str()) == Some("loop_check") {
            panic!("a stand-down fire must never be recorded as loop_check (breaks the fingerprint streak scan): {line}");
        }
    }
}

/// The primary floor is blind to GitHub's secondary (burst/concurrency)
/// limit by construction: a live specimen showed core 4922/5000 and graphql
/// 1392/5000 - both healthy - while a call was refused anyway. A fire must
/// also stand down on an OBSERVED secondary refusal in the last 5 minutes,
/// never only on advertised quota headroom.
#[test]
fn floor_stands_down_on_a_recent_secondary_refusal_even_with_healthy_quota() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let global_dir = tmp.path().join("global_fno");
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::create_dir_all(&global_dir).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    let global_events_path = global_dir.join("events.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-2ndary", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    // A prior fire's forensic trail: a secondary-limit refusal 90s ago, well
    // inside the 5-minute window, on the MACHINE-WIDE log (`emit_to_both`'s
    // destination for every session's rows, this session's included).
    fs::write(
        &global_events_path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "loop_check_gh_error",
                "ts": "2026-06-05T00:28:30Z",
                "data": {
                    "session_id": "sess-2ndary",
                    "read": "pulls_comments",
                    "stderr_tail": "HTTP 403: You have exceeded a secondary rate limit"
                }
            })
        ),
    )
    .unwrap();

    // 4922 remaining, well above GRAPHQL_FLOOR (200): the primary floor alone
    // would NOT trigger here. Only the observed refusal should.
    let gh = quota_gh(cwd, 4922, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
        "--events",
        events_path.to_str().unwrap(),
        "--global-events",
        global_events_path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(d.message.contains("standing down"), "got: {}", d.message);
    assert!(
        d.message.contains("secondary"),
        "must name the secondary limit, not just the primary floor: {}",
        d.message
    );
    let calls = fs::read_to_string(cwd.join("calls.log")).unwrap();
    assert!(
        !calls.contains("pr view"),
        "no GraphQL spend on a secondary-refusal stand-down: {calls}"
    );
}

/// The fleet-wide half of the same guard: the secondary limit is per-USER,
/// so a refusal recorded by ANOTHER session on this machine must stand THIS
/// session down too, not just one that refused itself. 29 live workers were
/// on this machine the night the guard was designed; a per-session refusal
/// record would have let 28 of them keep sending against a budget the 29th
/// had already proven refused.
#[test]
fn floor_stands_down_on_another_sessions_secondary_refusal() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let global_dir = tmp.path().join("global_fno");
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::create_dir_all(&global_dir).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let events_path = cwd.join(".fno/events.jsonl");
    let global_events_path = global_dir.join("events.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-quiet", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    // A DIFFERENT session's refusal, in the shared machine-wide log. This
    // session's own project-local log has no such row.
    fs::write(
        &global_events_path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "loop_check_gh_error",
                "ts": "2026-06-05T00:28:30Z",
                "data": {
                    "session_id": "sess-loud",
                    "read": "pulls_comments",
                    "stderr_tail": "HTTP 403: You have exceeded a secondary rate limit"
                }
            })
        ),
    )
    .unwrap();

    let gh = quota_gh(cwd, 4922, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
        "--events",
        events_path.to_str().unwrap(),
        "--global-events",
        global_events_path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(
        d.message.contains("secondary"),
        "another session's refusal must stand this one down too: {}",
        d.message
    );
}

/// Item 4's other half: the floor belongs to the merge guard, so a
/// promise-intent fire proceeds below it.
#[test]
fn floor_never_blocks_a_promise() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-floor2", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // Exhausted (0 remaining) AND green: the promise must still be evaluated.
    let gh = quota_gh(cwd, 0, true);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);
    assert!(!d.message.contains("standing down"), "got: {}", d.message);
    let calls = fs::read_to_string(cwd.join("calls.log")).unwrap();
    assert!(calls.contains("pr view"), "promise reads proceed: {calls}");
    assert_eq!(code, 0);
}

/// The floor's other exemption: honoring a cancel spends no GraphQL, so an
/// aborted fire below the floor must still terminate - blocking it would trap
/// a cancelled session behind the reserve for a whole reset window.
#[test]
fn floor_never_blocks_an_abort() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-floor-abort", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_aborted()).unwrap();

    let gh = quota_gh(cwd, 50, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);
    assert_eq!(code, 0);
    assert!(!d.message.contains("standing down"), "got: {}", d.message);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Aborted"));
}

/// Item 2: an exhausted quota turns the bare read failure into the exhaustion
/// name with a reset horizon, and drops the "retrying next fire" advice.
#[test]
fn exhaustion_named_on_read_failure() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-exh", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // remaining 0 with a FAILING pr view: the done() read errors and the
    // reason must name the bucket, not just "retrying next fire".
    let gh = quota_gh(cwd, 0, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(
        d.message.contains("GraphQL quota exhausted"),
        "got: {}",
        d.message
    );
    assert!(d.message.contains("resets in ~"), "got: {}", d.message);
    assert!(d.message.contains("fno do pr status"), "got: {}", d.message);
    assert!(
        !d.message.contains("retrying next fire"),
        "exhaustion must not be advised to retry: {}",
        d.message
    );
}

/// A secondary (burst/concurrency) refusal must NOT be named as primary
/// quota exhaustion, even when the read that failed IS a GraphQL read: the
/// two failure modes clear on completely different timescales (seconds vs
/// up to an hour), and misnaming one as the other sends the caller to wait
/// for a reset that was never the actual limiter.
#[test]
fn secondary_refusal_is_named_distinctly_from_primary_exhaustion() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-2nderr", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    // rate_limit reports HEALTHY (both buckets), but the actual pr_view call
    // fails with a secondary-limit stderr - the live specimen's exact shape.
    let calls_log = cwd.join("calls.log");
    let gh = make_script(
        cwd,
        "gh",
        &format!(
            r#"echo "$*" >> "{calls}"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if [ "$1" = api ] && [ "$2" = rate_limit ]; then
  echo '{{"resources":{{"core":{{"remaining":4922,"reset":9999999999}},"graphql":{{"remaining":1392,"reset":9999999999}}}}}}'
  exit 0
fi
if [ "$1" = pr ] && [ "$2" = view ]; then
  echo "HTTP 403: You have exceeded a secondary rate limit" >&2
  exit 1
fi
exit 1"#,
            calls = calls_log.display(),
        ),
    );
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(
        d.message.contains("SECONDARY"),
        "must name the secondary limit: {}",
        d.message
    );
    assert!(
        !d.message.contains("GraphQL quota exhausted"),
        "must not misname a secondary refusal as primary exhaustion: {}",
        d.message
    );
    assert!(
        !d.message.contains("resets in ~"),
        "must not point at the primary reset horizon for a secondary refusal: {}",
        d.message
    );
}

/// The floor's watching exemption must NEVER idle without a lease: a watching
/// fire below floor whose claim cannot be renewed falls through to the
/// stand-down block (and never crashes on the exemption path itself).
#[test]
fn floor_watching_fire_without_lease_still_blocks() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-floorw", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    // A newest-entry <watching> tag (no claim fields in the manifest, so the
    // lease renewal cannot succeed).
    let msg = serde_json::json!({
        "message": {
            "role": "assistant",
            "content": "<watching reason=\"ci\" pr=\"1\" timeout=\"30m\">"
        }
    });
    fs::write(
        &transcript_path,
        serde_json::to_string(&msg).unwrap() + "\n",
    )
    .unwrap();

    let gh = quota_gh(cwd, 50, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(d.message.contains("standing down"), "got: {}", d.message);
    assert!(
        !d.message.contains("watching under GraphQL stand-down"),
        "an unleased watching fire must not idle: {}",
        d.message
    );
}

// ── coverage commit-status publish (x-6352) ─────────────────────────────────
//
// AC8: both emitters of the review_coverage row (the stop-hook decide path and
// the standalone verb) also publish the commit status, and the load-bearing
// assert is the POSTED TARGET SHA + CONTEXT - never merely that a helper ran.
// A status on the wrong sha or under a mistyped context is a green marker on
// nothing, which is worse than no marker: the ruleset reads one exact name on
// one exact commit.

/// The PR head the green mock reports; the git mock echoes the same sha as
/// local HEAD, so the emitted row is pinned to the head it publishes on.
const PUB_HEAD: &str = "deadbeefdeadbeefdeadbeefdeadbeef00000001";

/// green()-shaped gh mock that additionally tees every `gh api` invocation to
/// `record`, so a test can assert on what was POSTED, not just that gh ran.
fn recording_green_gh(dir: &Path, record: &Path, reviewed: bool) -> PathBuf {
    let reviews = if reviewed {
        format!(
            r#"{{"reviews":[{{"author":{{"login":"chatgpt-codex-connector"}},"state":"COMMENTED","submittedAt":"2026-08-14T01:00:00Z","commit":{{"oid":"{PUB_HEAD}"}}}}],"comments":[]}}"#
        )
    } else {
        r#"{"reviews":[],"comments":[]}"#.to_string()
    };
    let record_s = record.display().to_string();
    make_script(
        dir,
        "gh",
        &format!(
            r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"{PUB_HEAD}","mergeable":"MERGEABLE","baseRefName":"main"}}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{{"name":"ci","state":"SUCCESS","bucket":"pass"}}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then echo '[]'; exit 0; fi
if echo "$*" | grep -q "reviews"; then echo '{reviews}'; exit 0; fi
if echo "$*" | grep -q -- "--json labels"; then echo 'false'; exit 0; fi
if echo "$*" | grep -q "^api"; then echo "$*" >> "{record_s}"; echo '{{}}'; exit 0; fi
exit 1
"#,
        ),
    )
}

fn fallback_gh(
    dir: &Path,
    record: &Path,
    reviewed: bool,
    label_mode: &str,
    status_description: &str,
) -> PathBuf {
    let reviews = if reviewed {
        format!(
            r#"{{"reviews":[{{"author":{{"login":"chatgpt-codex-connector"}},"state":"COMMENTED","submittedAt":"2026-08-14T01:00:00Z","commit":{{"oid":"{PUB_HEAD}"}}}}],"comments":[]}}"#
        )
    } else {
        r#"{"reviews":[],"comments":[]}"#.to_string()
    };
    let record_s = record.display().to_string();
    let attempts = dir.join("label-attempts");
    make_script(
        dir,
        "gh-fallback",
        &format!(
            r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"{PUB_HEAD}","mergeable":"MERGEABLE","baseRefName":"main"}}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{{"name":"ci","state":"SUCCESS","bucket":"pass"}}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then echo '[]'; exit 0; fi
if echo "$*" | grep -q "reviews"; then echo '{reviews}'; exit 0; fi
if echo "$*" | grep -q -- "--json labels"; then
  n=0
  if [ -f "{attempts}" ]; then n=$(cat "{attempts}"); fi
  n=$((n + 1))
  echo "$n" > "{attempts}"
  echo "label-read-$n" >> "{record_s}"
  if [ "{label_mode}" = "second-true" ] && [ "$n" -ge 2 ]; then
    echo true
    exit 0
  fi
  exit 1
fi
if echo "$*" | grep -q "/commits/.*/status"; then
  echo "status-description-read" >> "{record_s}"
  echo '{status_description}'
  exit 0
fi
if echo "$*" | grep -q "^api"; then echo "$*" >> "{record_s}"; echo '{{}}'; exit 0; fi
exit 1
"#,
            attempts = attempts.display(),
        ),
    )
}

fn pub_git(dir: &Path) -> PathBuf {
    make_script(
        dir,
        "git",
        r#"case "$*" in
  *--raw*) exit 1 ;;
  *) echo "deadbeefdeadbeefdeadbeefdeadbeef00000001" ;;
esac"#,
    )
}

fn pub_fixture(cwd: &Path, review_config: &str) {
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::write(cwd.join(".fno/config.toml"), review_config).unwrap();
}

/// Run the standalone verb against `cwd`, returning its (exit code, stdout).
fn fire_verb(cwd: &Path, gh: &Path, git: &Path, project: &Path, global: &Path) -> (i32, String) {
    fno_agents::loopcheck::run_review_coverage_capture(&[
        "review-coverage".to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        "--events".to_string(),
        project.display().to_string(),
        "--global-events".to_string(),
        global.display().to_string(),
        format!("--gh-bin={}", gh.display()),
        format!("--git-bin={}", git.display()),
        "--global-settings".to_string(),
        "/nonexistent/global-settings.yaml".to_string(),
        "--author-harness".to_string(),
        "none".to_string(),
    ])
}

fn status_posts(record: &Path) -> Vec<String> {
    fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("/statuses/"))
        .map(|l| l.to_string())
        .collect()
}

const BOT_LANE: &str = "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n";

/// Covered fixture: BOTH emitters post one status each, to the same PR head
/// sha, under the one context string, green because the review is at HEAD.
#[test]
fn coverage_status_publish_fires_from_both_emitters_to_the_pr_head() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("proj");
    pub_fixture(&cwd, BOT_LANE);
    let project = cwd.join(".fno/events.jsonl");
    let global = tmp.path().join("global.jsonl");
    let manifest = cwd.join("target-state.md");
    fs::write(
        &manifest,
        new_manifest("sess-pub", "2026-08-14T00:00:00Z", true),
    )
    .unwrap();
    fs::write(cwd.join("transcript.jsonl"), transcript_with_promise()).unwrap();
    let record = tmp.path().join("gh-api-record");
    let bins = TempDir::new().unwrap();
    let gh = recording_green_gh(bins.path(), &record, true);
    let git = pub_git(bins.path());
    std::env::set_var("FNO_NUDGE_DISABLED", "1");
    std::env::set_var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS", "0");

    // Emitter 1: the stop-hook path (decide -> run_done).
    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest.to_str().unwrap(),
        "--transcript",
        cwd.join("transcript.jsonl").to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-08-14T00:30:00Z",
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
        "--events",
        project.to_str().unwrap(),
        "--global-events",
        global.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "loop-check must allow: {:?}", d);

    // Emitter 2: the standalone verb.
    let (vcode, vjson) = fire_verb(&cwd, &gh, &git, &project, &global);
    assert_eq!(vcode, 0, "verb must emit a row: {vjson}");

    let posts = status_posts(&record);
    assert_eq!(posts.len(), 2, "one POST per emitter, got: {posts:?}");
    for post in &posts {
        assert!(
            post.contains(&format!("statuses/{PUB_HEAD}")),
            "posted to the wrong sha: {post}"
        );
        assert!(
            post.contains("context=fno/review-coverage"),
            "posted under the wrong context: {post}"
        );
        assert!(
            post.contains("state=success")
                && post.contains(&format!("reviewed at {}", &PUB_HEAD[..8])),
            "covered fixture must post a success naming count+sha: {post}"
        );
    }
}

/// Unreviewed fixture: both the row and the status say failure. The status is
/// the refusal a human reads on the GitHub side, so it must never say green
/// for a head nothing reviewed.
#[test]
fn coverage_status_publish_posts_failure_when_unreviewed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("proj");
    pub_fixture(&cwd, BOT_LANE);
    let project = cwd.join(".fno/events.jsonl");
    let global = tmp.path().join("global.jsonl");
    let record = tmp.path().join("gh-api-record");
    let bins = TempDir::new().unwrap();
    let gh = recording_green_gh(bins.path(), &record, false);
    let git = pub_git(bins.path());

    let (vcode, vjson) = fire_verb(&cwd, &gh, &git, &project, &global);
    assert_eq!(vcode, 0, "verb must emit a row: {vjson}");

    let posts = status_posts(&record);
    assert_eq!(posts.len(), 1, "verb arm posts exactly once: {posts:?}");
    assert!(posts[0].contains("state=failure"), "got: {}", posts[0]);
    assert!(posts[0].contains("context=fno/review-coverage"));
}

/// A configured local `code-review` reviewer with no head-pinned local pass:
/// the status stays failure even though a GitHub review exists, mirroring the
/// merge gate's conjunction (the row alone is not enough).
#[test]
fn coverage_status_publish_requires_local_code_review_pass_when_configured() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("proj");
    pub_fixture(&cwd, "[review]\nreviewers = [\"code-review\"]\n");
    let project = cwd.join(".fno/events.jsonl");
    let global = tmp.path().join("global.jsonl");
    let record = tmp.path().join("gh-api-record");
    let bins = TempDir::new().unwrap();
    // A passing GitHub review exists, but no local attestation does.
    let gh = recording_green_gh(bins.path(), &record, true);
    let git = pub_git(bins.path());

    let (vcode, vjson) = fire_verb(&cwd, &gh, &git, &project, &global);
    assert_eq!(vcode, 0, "verb must emit a row: {vjson}");

    let posts = status_posts(&record);
    assert_eq!(posts.len(), 1, "verb arm posts exactly once: {posts:?}");
    assert!(
        posts[0].contains("state=failure"),
        "a bot review must not satisfy the local code-review lane: {}",
        posts[0]
    );
}

#[test]
fn coverage_status_protects_an_override_after_three_refused_label_reads() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("proj");
    pub_fixture(&cwd, BOT_LANE);
    let project = cwd.join(".fno/events.jsonl");
    let global = tmp.path().join("global.jsonl");
    let record = tmp.path().join("gh-api-record");
    let bins = TempDir::new().unwrap();
    let gh = fallback_gh(
        bins.path(),
        &record,
        false,
        "always-fail",
        "coverage-override label applied by jane",
    );
    let git = pub_git(bins.path());

    let (code, json) = fire_verb(&cwd, &gh, &git, &project, &global);

    assert_eq!(code, 0, "{json}");
    let recorded = fs::read_to_string(&record).unwrap();
    assert_eq!(recorded.matches("label-read-").count(), 3, "{recorded}");
    assert_eq!(recorded.matches("status-description-read").count(), 1);
    assert!(status_posts(&record).is_empty(), "{recorded}");
}

#[test]
fn coverage_status_posts_covered_after_refused_label_reads_find_no_override() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("proj");
    pub_fixture(&cwd, BOT_LANE);
    let project = cwd.join(".fno/events.jsonl");
    let global = tmp.path().join("global.jsonl");
    let record = tmp.path().join("gh-api-record");
    let bins = TempDir::new().unwrap();
    let gh = fallback_gh(
        bins.path(),
        &record,
        true,
        "always-fail",
        "no covered review at deadbeef",
    );
    let git = pub_git(bins.path());

    let (code, json) = fire_verb(&cwd, &gh, &git, &project, &global);

    assert_eq!(code, 0, "{json}");
    let recorded = fs::read_to_string(&record).unwrap();
    assert_eq!(recorded.matches("label-read-").count(), 3, "{recorded}");
    let posts = status_posts(&record);
    assert_eq!(posts.len(), 1, "{recorded}");
    assert!(posts[0].contains("state=success"), "{}", posts[0]);
    assert!(posts[0].contains("covered: 1 reviewed at"), "{}", posts[0]);
}

#[test]
fn coverage_status_posts_failure_after_refused_label_reads_find_no_status() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("proj");
    pub_fixture(&cwd, BOT_LANE);
    let project = cwd.join(".fno/events.jsonl");
    let global = tmp.path().join("global.jsonl");
    let record = tmp.path().join("gh-api-record");
    let bins = TempDir::new().unwrap();
    let gh = fallback_gh(bins.path(), &record, false, "always-fail", "");
    let git = pub_git(bins.path());

    let (code, json) = fire_verb(&cwd, &gh, &git, &project, &global);

    assert_eq!(code, 0, "{json}");
    let recorded = fs::read_to_string(&record).unwrap();
    assert_eq!(recorded.matches("label-read-").count(), 3, "{recorded}");
    let posts = status_posts(&record);
    assert_eq!(posts.len(), 1, "{recorded}");
    assert!(posts[0].contains("state=failure"), "{}", posts[0]);
}

#[test]
fn coverage_status_retries_a_label_read_then_honors_the_override() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("proj");
    pub_fixture(&cwd, BOT_LANE);
    let project = cwd.join(".fno/events.jsonl");
    let global = tmp.path().join("global.jsonl");
    let record = tmp.path().join("gh-api-record");
    let bins = TempDir::new().unwrap();
    let gh = fallback_gh(bins.path(), &record, false, "second-true", "unused");
    let git = pub_git(bins.path());

    let (code, json) = fire_verb(&cwd, &gh, &git, &project, &global);

    assert_eq!(code, 0, "{json}");
    let recorded = fs::read_to_string(&record).unwrap();
    assert_eq!(recorded.matches("label-read-").count(), 2, "{recorded}");
    assert!(!recorded.contains("status-description-read"), "{recorded}");
    let posts = status_posts(&record);
    assert_eq!(posts.len(), 1, "{recorded}");
    assert!(posts[0].contains("state=success"), "{}", posts[0]);
    assert!(posts[0].contains("coverage-override"), "{}", posts[0]);
}

// ── the king driver arm ───────────────────────────────────────────────────────
//
// A king has no PR, so none of the target conjuncts above apply. These drive
// the same verb with `--driver king` over a king manifest and a mocked board.

fn king_manifest(dir: &Path, fno_id: &str) -> PathBuf {
    let path = dir.join("king-state.md");
    fs::write(
        &path,
        format!(
            "---\nfno_id: {fno_id}\ncreated_at: 2026-08-18T00:00:00Z\nscope: board drain\n\
             harness: claude\nbudget_max_iterations: 40\n---\n"
        ),
    )
    .unwrap();
    path
}

/// A mock `fno` whose `king board --json` prints `payload`.
fn king_board_bin(dir: &Path, payload: &str, exit: i32) -> PathBuf {
    make_script(
        dir,
        "fno-king-mock",
        &format!("cat <<'JSON'\n{payload}\nJSON\nexit {exit}"),
    )
}

fn king_fire(state: &Path, cwd: &Path, events: &Path, fno_bin: &Path) -> (i32, serde_json::Value) {
    let (code, json) = fno_agents::loopcheck::run_loop_check_capture(&[
        "loop-check".to_string(),
        "--driver".to_string(),
        "king".to_string(),
        "--state".to_string(),
        state.to_str().unwrap().to_string(),
        "--transcript".to_string(),
        cwd.join("transcript.jsonl").to_str().unwrap().to_string(),
        "--cwd".to_string(),
        cwd.to_str().unwrap().to_string(),
        "--events".to_string(),
        events.to_str().unwrap().to_string(),
        "--global-events".to_string(),
        events.to_str().unwrap().to_string(),
        "--fno-bin".to_string(),
        fno_bin.to_str().unwrap().to_string(),
    ]);
    (code, serde_json::from_str(&json).unwrap())
}

const BOARD_TWO_ACTIONABLE: &str = r#"{
  "actionable": 2, "unreadable": 0,
  "queues": [
    {"name":"undispatched","status":"ok","actionable":true,"count":2,
     "rows":[{"id":"x-1234"},{"id":"x-5678"}],"error":"","truncated":0,"note":"","source":"s"}
  ]
}"#;

/// The same board with one row cleared, which is the progress signal.
const BOARD_ONE_CLEARED: &str = r#"{
  "actionable": 1, "unreadable": 0,
  "queues": [
    {"name":"undispatched","status":"ok","actionable":true,"count":1,
     "rows":[{"id":"x-5678"}],"error":"","truncated":0,"note":"","source":"s"}
  ]
}"#;

/// A row cleared while the board GREW. Progress, because progress is a row
/// leaving, never board size.
const BOARD_REFILLED: &str = r#"{
  "actionable": 3, "unreadable": 0,
  "queues": [
    {"name":"undispatched","status":"ok","actionable":true,"count":3,
     "rows":[{"id":"x-5678"},{"id":"x-9999"},{"id":"x-aaaa"}],
     "error":"","truncated":0,"note":"","source":"s"}
  ]
}"#;

const BOARD_CLEAN: &str = r#"{
  "actionable": 0, "unreadable": 0,
  "queues": [
    {"name":"undispatched","status":"ok","actionable":true,"count":0,
     "rows":[],"error":"","truncated":0,"note":"","source":"s"}
  ]
}"#;

#[test]
fn king_arm_blocks_while_the_board_is_not_empty() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-block");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 2, "a non-empty board must block: {d}");
    assert_eq!(d["decision"], "block");
    assert_eq!(d["actionable"], 2);
    let reason = d["reason"].as_str().unwrap();
    assert!(
        reason.contains("undispatched") && reason.contains("x-1234"),
        "the block reason must name the top actionable row: {reason}"
    );
}

#[test]
fn king_nowork_is_the_clean_terminal_for_an_empty_board() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-clean");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_CLEAN, 0);

    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow");
    assert_eq!(d["termination_reason"], "NoWork");

    let journal = fs::read_to_string(&events).unwrap();
    let row: serde_json::Value = journal
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["type"] == "termination")
        .expect("a termination event must be appended");
    assert_eq!(row["data"]["reason"], "NoWork");
    assert_eq!(
        row["data"]["session_id"], "k-clean",
        "the event must carry the king session id so the journal reader matches it"
    );
    assert_eq!(row["data"]["driver"], "king");
}

#[test]
fn king_arm_allows_silently_when_no_king_manifest_exists() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, d) = king_fire(&cwd.join("absent.md"), cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow");
    assert!(
        !events.exists(),
        "a non-king session must write no king events"
    );
}

#[test]
fn king_arm_never_reads_the_target_manifest() {
    // The kill criterion this arm ships under says a diff reaching into the
    // target arm means the second-driver framing was wrong. This asserts the
    // runtime half of that: a target manifest sitting in the same checkout
    // changes nothing about a king fire.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::write(
        cwd.join(".fno/target-state.md"),
        "---\nsession_id: t-1\n---\n",
    )
    .unwrap();
    let state = king_manifest(cwd, "k-iso");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_CLEAN, 0);

    let (code, d) = king_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "NoWork");
}

#[test]
fn king_arm_honors_the_cancel_sentinel() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    let state = king_manifest(cwd, "k-cancel");
    fs::write(cwd.join(".fno/.target-cancelled"), "").unwrap();
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, d) = king_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "Interrupted");
}

#[test]
fn king_arm_blocks_rather_than_certifying_a_board_it_cannot_read() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-blind");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), "not json at all", 1);

    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 2, "blind is not clean: {d}");
    assert!(d["reason"].as_str().unwrap().contains("unreadable"));
}

/// Append one event row to a king journal.
fn king_event(events: &Path, event_type: &str, data: serde_json::Value) {
    use std::io::Write;
    let row = serde_json::json!({
        "ts": "2026-08-18T00:00:00Z",
        "type": event_type,
        "source": "hook",
        "data": data,
    });
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events)
        .unwrap();
    writeln!(f, "{row}").unwrap();
}

#[test]
fn a_cleared_row_is_progress_and_needs_no_event_producer() {
    // The defect this replaces: progress keyed ONLY on a king_action event, and
    // nothing in the repo emitted one, so every king hit NoProgress on fire 3.
    // A row leaving the board is external truth and needs no producer at all.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-cleared");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    // Two dry fires that recorded both rows...
    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-cleared", "actionable_ids": ids}),
    );
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-cleared", "actionable_ids": ids}),
    );

    // ...then a board with x-1234 gone. That is work the king did.
    let fno = king_board_bin(bin_dir.path(), BOARD_ONE_CLEARED, 0);
    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 2, "clearing a row must keep the loop running: {d}");
    assert_eq!(d["fires"], 1, "the dry-fire counter must have reset");
}

#[test]
fn the_cleared_row_reset_survives_into_the_next_fire() {
    // The defect: `king_decide` reset a LOCAL `dry` and the journal kept the
    // rows, so the next fire recounted them. The 3-fire tolerance shrank by one
    // per fire and a king that was demonstrably working still died NoProgress.
    //
    // The sibling test above passes either way, because it asserts only the
    // fire that clears. This one asserts the fire AFTER it, which is where the
    // forgetting showed up.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-durable");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-durable", "actionable_ids": ids}),
    );
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-durable", "actionable_ids": ids}),
    );

    // Fire that clears x-1234.
    let cleared_bin = king_board_bin(bin_dir.path(), BOARD_ONE_CLEARED, 0);
    let (code, d) = king_fire(&state, cwd, &events, &cleared_bin);
    assert_eq!(code, 2, "the clearing fire must keep running: {d}");

    // The very next fire clears nothing. Pre-fix this read three dry fires and
    // terminated; the king had just done real work one fire earlier.
    let (code, d) = king_fire(&state, cwd, &events, &cleared_bin);
    assert_eq!(
        code, 2,
        "a single dry fire after real progress must not end the king: {d}"
    );
    assert_eq!(
        d["termination_reason"],
        serde_json::Value::Null,
        "no terminal one fire after a cleared row: {d}"
    );
    assert_eq!(
        d["fires"], 1,
        "the counter restarts from the clear, not from 0 fires ago"
    );
}

#[test]
fn a_row_cleared_while_the_board_grew_is_still_progress() {
    // Progress is a row LEAVING, never board size. The board refills while the
    // king works, so a count that went up can still carry real progress.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-refill");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-refill", "actionable_ids": ids}),
    );
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-refill", "actionable_ids": ids}),
    );

    let fno = king_board_bin(bin_dir.path(), BOARD_REFILLED, 0);
    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 2, "a grown board that cleared a row is progress: {d}");
    assert_eq!(d["actionable"], 3);
    assert_eq!(d["fires"], 1);
}

#[test]
fn an_unchanged_board_clears_nothing_and_still_reaches_noprogress() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-same");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    for _ in 0..2 {
        king_event(
            &events,
            "king_loop_check",
            serde_json::json!({"session_id": "k-same", "actionable_ids": ids}),
        );
    }

    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);
    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "NoProgress");
}

#[test]
fn a_fire_records_the_actionable_ids_the_next_fire_compares_against() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-record");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    king_fire(&state, cwd, &events, &fno);

    let journal = fs::read_to_string(&events).unwrap();
    let row: serde_json::Value = journal
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["type"] == "king_loop_check")
        .expect("a blocking fire must record its board");
    let ids = row["data"]["actionable_ids"].as_array().unwrap();
    assert_eq!(
        ids.len(),
        2,
        "without these the next fire cannot see a clear"
    );
    assert_eq!(ids[0], "undispatched:x-1234");
}

#[test]
fn king_progress_is_an_action_against_a_target_id_not_seen_before() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-progress");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    // Two dry fires, then a real action: the counter goes back to zero, so the
    // next fire blocks rather than giving up.
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-progress"}),
    );
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-progress"}),
    );
    king_event(
        &events,
        "king_action",
        serde_json::json!({"session_id": "k-progress", "kind": "dispatch", "target_id": "x-1234"}),
    );

    let (code, d) = king_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 2, "progress must keep the loop running: {d}");
    assert_eq!(d["decision"], "block");
    assert_eq!(d["fires"], 1, "the dry-fire counter must have reset");
}

#[test]
fn king_noprogress_ends_a_board_that_refuses_to_shrink() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-stuck");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-stuck"}),
    );
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-stuck"}),
    );

    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "NoProgress");
    let reason = d["reason"].as_str().unwrap();
    assert!(
        reason.contains('2') && reason.contains("actionable"),
        "the terminal must name what stayed unshrunk: {reason}"
    );
}

#[test]
fn a_repeated_king_action_is_not_progress() {
    // The specific way this loop would fail to converge, and it passes every
    // naive test: `stalled_holder` rows survive the one action a king has for
    // them, so re-waking the same node forever would reset the counter forever.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-repeat");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let wake = serde_json::json!({"session_id": "k-repeat", "kind": "wake", "target_id": "x-1234"});
    king_event(&events, "king_action", wake.clone());
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-repeat"}),
    );
    king_event(&events, "king_action", wake.clone());
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-repeat"}),
    );

    let (code, d) = king_fire(&state, cwd, &events, &fno);

    assert_eq!(
        code, 0,
        "a repeated action must not hold the loop open: {d}"
    );
    assert_eq!(d["termination_reason"], "NoProgress");
}

#[test]
fn another_kings_events_do_not_move_this_kings_counter() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-mine");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    for _ in 0..5 {
        king_event(
            &events,
            "king_loop_check",
            serde_json::json!({"session_id": "k-other"}),
        );
    }

    let (code, d) = king_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 2, "a sibling king's fires are not mine: {d}");
    assert_eq!(d["fires"], 1);
}

#[test]
fn an_empty_board_wins_over_a_dry_fire_streak() {
    // NoWork is the clean terminal and must not be pre-empted by NoProgress:
    // a king that drained its board on the third fire finished, it did not stall.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-drained");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = king_board_bin(bin_dir.path(), BOARD_CLEAN, 0);

    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-drained"}),
    );
    king_event(
        &events,
        "king_loop_check",
        serde_json::json!({"session_id": "k-drained"}),
    );

    let (code, d) = king_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "NoWork");
}

#[test]
fn an_unknown_driver_is_refused_rather_than_run_against_the_wrong_gate() {
    let (code, json) = fno_agents::loopcheck::run_loop_check_capture(&[
        "loop-check".to_string(),
        "--driver".to_string(),
        "emperor".to_string(),
        "--state".to_string(),
        "/nonexistent".to_string(),
        "--transcript".to_string(),
        "/nonexistent".to_string(),
        "--cwd".to_string(),
        "/tmp".to_string(),
    ]);
    assert_eq!(code, 2);
    let d: serde_json::Value = serde_json::from_str(&json).unwrap();
    let err = d["error"].as_str().unwrap();
    assert!(
        err.contains("emperor") && err.contains("king"),
        "got: {err}"
    );
}

/// A mock `fno` that answers `inbox board --json` and LOGS every
/// `agents king escalate`
/// argv, so a test can read back which paths escalated and over what.
fn king_escalate_bin(dir: &Path, payload: &str, log: &Path) -> PathBuf {
    make_script(
        dir,
        "fno-king-escalate-mock",
        &format!(
            "if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"king\" ] && [ \"$3\" = \"escalate\" ]; then\n\
             \x20 echo \"$*\" >> {log}\n\
             \x20 echo q-mock\n\
             \x20 exit 0\n\
             fi\n\
             cat <<'JSON'\n{payload}\nJSON",
            log = log.display()
        ),
    )
}

/// Plan verification 7, first half: EVERY NoProgress terminal escalates.
///
/// `king_decide` reaches NoProgress two ways: a board it could not read for
/// the whole dry-fire run, and a board whose rows nothing cleared. Both route
/// through the shared `terminate` closure, so both escalate and a terminal
/// added later is covered without anyone remembering to wire it. This drives
/// both rather than asserting the helper they share, which would pin the
/// function and not the destination.
///
/// The second half, that repeated calls over one stalled set yield exactly ONE
/// operator question, is `test_king_escalate.py`: the dedupe lives in the verb,
/// and a mock `fno` here records no questions to count. Neither test alone is
/// the verification; the seam between them is the `--stalled` argument.
#[test]
fn every_king_noprogress_terminal_escalates() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let log = cwd.join("escalations.log");
    let state = king_manifest(cwd, "k-escalate");
    let events = cwd.join("events.jsonl");
    let fno = king_escalate_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, &log);

    // Terminal 1: a readable board whose rows nothing clears.
    let mut last = (0, serde_json::Value::Null);
    for _ in 0..3 {
        last = king_fire(&state, cwd, &events, &fno);
    }
    assert_eq!(
        last.1["termination_reason"], "NoProgress",
        "three dry fires must reach the NoProgress terminal: {:?}",
        last.1
    );

    let logged = fs::read_to_string(&log).unwrap_or_default();
    let calls: Vec<&str> = logged.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        calls.len(),
        1,
        "the unshrunk-board terminal escalates: {logged}"
    );
    assert!(
        calls[0].contains("--stalled undispatched:x-1234,undispatched:x-5678"),
        "it escalates over the board's actionable rows, QUEUE-QUALIFIED \
         (the same node in two queues is two rows), got: {}",
        calls[0]
    );

    // Terminal 2: a board that never answers. There are no ids to name, so the
    // escalation carries an EMPTY set rather than being skipped. An operator
    // told nothing is the failure this verb exists to prevent, and a king that
    // cannot see its board is the case most worth telling them about.
    let blind_tmp = TempDir::new().unwrap();
    let blind_cwd = blind_tmp.path();
    let blind_log = blind_cwd.join("escalations.log");
    let blind_state = king_manifest(blind_cwd, "k-blind");
    let blind_events = blind_cwd.join("events.jsonl");
    let blind_bin = king_escalate_bin(bin_dir.path(), "not json at all", &blind_log);

    let mut blind = (0, serde_json::Value::Null);
    for _ in 0..3 {
        blind = king_fire(&blind_state, blind_cwd, &blind_events, &blind_bin);
    }
    assert_eq!(
        blind.1["termination_reason"], "NoProgress",
        "an unreadable board must terminate rather than block forever: {:?}",
        blind.1
    );
    let blind_logged = fs::read_to_string(&blind_log).unwrap_or_default();
    let blind_calls: Vec<&str> = blind_logged
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        blind_calls.len(),
        1,
        "the unreadable-board terminal escalates too: {blind_logged}"
    );
    assert!(
        blind_calls[0].contains("--stalled  --reason NoProgress")
            || blind_calls[0].contains("--stalled --reason"),
        "with no rows to name it still escalates, got: {}",
        blind_calls[0]
    );
}

/// The ceiling `--max-iterations` advertises must actually bind.
///
/// It was parsed into the manifest and read by nothing, so the help string
/// promised a bound that did not exist. A help string that lies is worse than
/// a missing flag: someone sets it, believes the king is bounded, and walks
/// away.
///
/// Progress is deliberately irrelevant here. A king clearing a row every fire
/// never trips the dry-fire counter, so without this it runs forever. That is
/// the case the flag exists for.
#[test]
fn the_manifest_iteration_ceiling_stops_a_king_that_is_still_working() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let events = cwd.join("events.jsonl");

    // A manifest with a ceiling of 3 rather than the default 40.
    let state = cwd.join("king-state.md");
    fs::write(
        &state,
        "---\nfno_id: k-budget\ncreated_at: 2026-08-18T00:00:00Z\nscope: drain\n\
         harness: claude\nbudget_max_iterations: 3\n---\n",
    )
    .unwrap();

    // Every fire clears a row, so the dry-fire counter never trips.
    let boards = [BOARD_TWO_ACTIONABLE, BOARD_ONE_CLEARED, BOARD_REFILLED];
    let mut last = (0, serde_json::Value::Null);
    for (i, payload) in boards.iter().enumerate() {
        let fno = king_board_bin(bin_dir.path(), payload, 0);
        last = king_fire(&state, cwd, &events, &fno);
        if i < boards.len() - 1 {
            assert_eq!(
                last.0, 2,
                "fire {i} must keep the king running: {:?}",
                last.1
            );
        }
    }

    assert_eq!(
        last.1["termination_reason"], "Budget",
        "the third fire reaches the manifest ceiling: {:?}",
        last.1
    );
    assert_ne!(
        last.1["termination_reason"], "NoProgress",
        "a king that cleared a row every fire did not stall"
    );
}

/// A terminal that is NOT NoProgress never escalates. NoWork is the king's
/// clean exit; asking the operator about a board it just emptied would train
/// them to ignore the queue this feature depends on.
#[test]
fn a_clean_king_terminal_does_not_ask_the_operator_anything() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let log = cwd.join("escalations.log");
    let state = king_manifest(cwd, "k-clean");
    let events = cwd.join("events.jsonl");
    let fno = king_escalate_bin(bin_dir.path(), BOARD_CLEAN, &log);

    let (_, json) = king_fire(&state, cwd, &events, &fno);
    assert_eq!(json["termination_reason"], "NoWork");
    assert!(
        !log.exists(),
        "a NoWork terminal must not escalate: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
}
