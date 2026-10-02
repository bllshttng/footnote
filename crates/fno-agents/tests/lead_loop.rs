//! The lead driver arm, end to end: a teamed session's stop gate reads the
//! board through the real in-process collector, decides, and terminates.
//!
//! Split from loop_check.rs: that file is over the line budget and
//! shrink-only, and this family is self-contained - its own fixture (a real
//! graph under a temp home, stub gh/fno-py/fno on PATH), its own spawn
//! helper, and exactly one read still served by a mock (the escalation
//! verb). The board itself is read in process, so the canned-payload mocks
//! are gone; a spec names the graph the fixture writes instead.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms).unwrap();
    fs::rename(&tmp, &path).unwrap();
    path
}

fn event_text(path: &Path) -> String {
    fno_agents::event_store::journal_text(path, &[])
}

// ── the lead driver arm ───────────────────────────────────────────────────────
//
// A lead has no PR, so none of the target conjuncts above apply. These drive
// the same verb with `--driver lead` over a lead manifest and a mocked board.

/// A `created_at` inside every default `span:96h` term, so a fixture that
/// declares no term of its own never trips the Stop-hook term gate: these
/// tests exercise the board/bound machinery downstream of that gate, not the
/// term itself (`lead_term_gate.rs`-equivalent coverage lives in
/// `lead_state.rs`'s and `loopcheck.rs`'s own unit tests).
fn recent_created_at() -> String {
    (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()
}

fn lead_manifest(dir: &Path, fno_id: &str) -> PathBuf {
    lead_manifest_with_budget(dir, fno_id, 40)
}

fn lead_manifest_with_budget(dir: &Path, fno_id: &str, budget: u64) -> PathBuf {
    let path = dir.join("lead-state.md");
    let created_at = recent_created_at();
    fs::write(
        &path,
        format!(
            "---\nfno_id: {fno_id}\ncreated_at: {created_at}\nscope: drain\n\
             harness: claude\nbudget_max_iterations: {budget}\n---\n"
        ),
    )
    .unwrap();
    path
}

fn lead_manifest_with_session(dir: &Path, fno_id: &str, session_id: &str) -> PathBuf {
    let path = lead_manifest(dir, fno_id);
    let content = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        content.replacen(
            "harness: claude",
            &format!("harness: claude\nharness_session_id: {session_id}"),
            1,
        ),
    )
    .unwrap();
    path
}

fn write_stand_down_transcript(cwd: &Path) {
    fs::write(
        cwd.join("transcript.jsonl"),
        r#"{"type":"user","uuid":"turn-stand-down","timestamp":"2026-09-06T21:00:00.000Z","message":{"role":"user","content":"Perhaps our lead has overstayed its welcome"}}
"#,
    )
    .unwrap();
}

/// A board the fixture serves. The canned payloads died with the subprocess
/// board read (x-25b8: the stop gate reads the collector in process), so a
/// spec now names the graph the fixture writes: the rows of the spec's
/// undispatched queue become planned, ready, unclaimed nodes, and the
/// decision comes out the real pipeline. An unparseable spec is the blind
/// case: the graph source goes dark.
fn org_board_bin(dir: &Path, payload: &str, _exit: i32) -> PathBuf {
    let path = dir.join("board-spec.json");
    fs::write(&path, payload).unwrap();
    path
}

/// The spec's undispatched row ids, or None when the spec never parsed.
fn lead_spec_rows(spec: &str) -> Option<Vec<String>> {
    let parsed: serde_json::Value = serde_json::from_str(spec).ok()?;
    let rows = parsed["queues"]
        .as_array()?
        .iter()
        .find(|q| q["name"] == "undispatched")?["rows"]
        .as_array()?
        .clone();
    Some(
        rows.iter()
            .filter_map(|r| r["id"].as_str().map(str::to_string))
            .collect(),
    )
}

/// The drain answer the fixture's fno stub owes: the epic plus every spec row
/// reads undelivered, and a clean spec writes the epic itself done, so the
/// graph and the stub answer the same question. An unparseable spec is the
/// blind case and must refuse, never claim drained.
fn lead_drain_reply(spec: &str) -> String {
    match lead_spec_rows(spec) {
        None => "exit 1".to_string(),
        Some(rows) if rows.is_empty() => {
            "echo '{\"scope\":\"drain\",\"undelivered\":0}'".to_string()
        }
        Some(rows) => format!(
            "echo '{{\"scope\":\"drain\",\"undelivered\":{}}}'",
            rows.len() + 1
        ),
    }
}

/// Write the fixture graph for `board_spec` and pin config + lane at `cwd`.
/// The epic `drain` matches the manifest scope; every spec row becomes one
/// undispatchable planned node.
fn lead_prepare_fixture(cwd: &Path, home: &Path, board_spec: &Path) {
    let fno_dir = cwd.join(".fno");
    fs::create_dir_all(&fno_dir).unwrap();
    let graph = home.join("graph.json");
    let spec = fs::read_to_string(board_spec).unwrap_or_default();
    let ids = lead_spec_rows(&spec);
    if ids.is_none() {
        // Blind: the spec never parsed, so the graph source goes dark and the
        // collector answers with unreadable queues instead of a payload that
        // was never possible to fake here.
        let _ = fs::remove_file(&graph);
    } else {
        let ids = ids.unwrap_or_default();
        // A clean spec writes the epic itself done: the lead's terminations
        // key on the team draining, so a fixture board with no rows must
        // read as a drained team, not an eternally open one.
        let epic = if ids.is_empty() {
            serde_json::json!(
                {"id": "drain", "type": "epic", "status": "done",
                 "completed_at": "2026-08-18T00:00:00Z", "priority": "p1"}
            )
        } else {
            serde_json::json!(
                {"id": "drain", "type": "epic", "status": "ready", "priority": "p1"}
            )
        };
        let nodes: Vec<serde_json::Value> = std::iter::once(epic)
            .chain(ids.into_iter().map(|id| {
                // parent: the manifest scope compiles to the epic plus its
                // descendants, so a workable row is a child of `drain`.
                serde_json::json!({"id": id.clone(), "slug": id.clone(), "title": id.clone(), "type": "feature", "status": "ready",
                                   "priority": "p0", "plan_path": "/plans/p.md",
                                   "parent": "drain"})
            }))
            .collect();
        let mut rows = nodes;
        for row in &mut rows {
            let obj = row.as_object_mut().unwrap();
            obj.entry("slug")
                .or_insert_with(|| serde_json::json!("drain"));
            obj.entry("title")
                .or_insert_with(|| serde_json::json!("drain"));
            obj.entry("priority")
                .or_insert_with(|| serde_json::json!("p1"));
        }
        fno_agents::graph_store::seed_rows(&graph, &rows).unwrap();
    }
    fs::write(
        fno_dir.join("config.toml"),
        format!(
            "state_dir = \"{}\"\n[paths]\noperator_lane = \"{}\"\n\n\
             # The scope queue's project map reads work.workspaces; a machine \
             # with no global config.toml must see the fixture as complete.\n\
             [work.workspaces]\n",
            home.display(),
            home.join("lane.md").display()
        ),
    )
    .unwrap();
    fs::write(home.join("lane.md"), "").unwrap();
    let stubs = home.join("stubs");
    fs::create_dir_all(&stubs).unwrap();
    fs::write(stubs.join("gh"), "#!/bin/sh\necho '[]'\n").unwrap();
    // The batched truth probe shells bare `fno` (claude_ask.rs), so without
    // this stub every fire pays a real installed-CLI cold start and probes the
    // operator's live sessions - slow AND non-hermetic.
    fs::write(stubs.join("fno"), "#!/bin/sh\necho '{}'\n").unwrap();
    let outstanding = if home.join("questions.jsonl").is_file() {
        r#"{"questions":[{"id":"q-k-question","question":"choose","ts":"2026-09-06T00:00:00Z","session_id":"k-question"}]}"#
    } else {
        "{}"
    };
    // The board folds undispatched and ready in-process now, so this stub
    // serves the one queue read that still rides a subprocess: the operator
    // questions. The `*)` arm answers {} for every other verb the board or
    // the truth probe shells.
    fs::write(
        stubs.join("fno-py"),
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *\"inbox outstanding\"*) echo '{outstanding}';;\n  *) echo '{{}}';;\nesac\n"
        ),
    )
    .unwrap();
    // Stage the default mock only when absent: lead_escalate_bin stages its
    // argv-recording version FIRST, and this fixture prep runs after it.
    let mock = home.join("escalate-mock");
    if !mock.is_file() {
        make_script(
            home,
            "escalate-mock",
            &format!(
                "if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"lead\" ] && [ \"$3\" = \"drain\" ]; \
                 then\n\
                 \x20 {}\n\
                 \x20 exit 0\n\
                 fi\n\
                 if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"lead\" ] && [ \"$3\" = \"escalate\" ]; then\n\
                 \x20 echo q-mock\n\
                 \x20 exit 0\n\
                 fi\n\
                 exit 0",
                lead_drain_reply(&spec)
            ),
        );
    };
    #[cfg(unix)]
    for stub in ["gh", "fno-py", "fno"] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(stubs.join(stub), fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn lead_spawn(state: &Path, cwd: &Path, events: &Path, home: &Path) -> (i32, serde_json::Value) {
    lead_spawn_with(state, cwd, events, home, &[])
}

/// `extra` carries per-fire CLI overrides, e.g. a short `--read-timeout-ms`
/// for the wedged-source test. The bound is the whole FIRE's ceiling (the
/// board's budget derives from it minus the serialization reserve), so only
/// a fire that needs a killed read passes one.
fn lead_spawn_with(
    state: &Path,
    cwd: &Path,
    events: &Path,
    home: &Path,
    extra: &[&str],
) -> (i32, serde_json::Value) {
    let stubs = home.join("stubs");
    let real_path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.envs(fno_agents::test_run::self_owner_env());
    cmd.args([
        "loop-check",
        "--driver",
        "lead",
        "--state",
        state.to_str().unwrap(),
        "--transcript",
        cwd.join("transcript.jsonl").to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--events",
        events.to_str().unwrap(),
        "--global-events",
        events.to_str().unwrap(),
    ]);
    // The board is read in process, so the fixture `fno` serves exactly one
    // live read: the escalation verb. Its mock is staged by the fixture and
    // overwritten by tests that need the argv recorder.
    cmd.arg("--fno-bin").arg(home.join("escalate-mock"));
    cmd.args(extra);
    let out = cmd
        .env("FNO_CLAIMS_ROOT", home)
        .env("FNO_HOME", home)
        .env("FNO_AGENTS_HOME", home.join("agents"))
        .env(
            "FNO_OPERATOR_CAPTURE_DIR",
            home.join(".fno/operator-capture"),
        )
        // The board resolves its fno-py shellout FNO_PY-first, ahead of PATH
        // (scrape::fno_py), so a machine with the wheel installed under the uv
        // tools bin runs the REAL CLI against the real machine-wide question
        // index and every fixture stub here never fires. Pin the stub.
        .env("FNO_PY", stubs.join("fno-py"))
        .env("PATH", format!("{}:{}", stubs.display(), real_path))
        .output()
        .unwrap();
    let json = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .unwrap_or(serde_json::Value::Null);
    (out.status.code().unwrap_or(-1), json)
}

fn lead_fire(
    state: &Path,
    cwd: &Path,
    events: &Path,
    board_spec: &Path,
) -> (i32, serde_json::Value) {
    let home = board_spec.parent().unwrap();
    lead_prepare_fixture(cwd, home, board_spec);
    lead_spawn(state, cwd, events, home)
}

fn lead_gate_status_mock(home: &Path, payload: &str) {
    use std::os::unix::fs::PermissionsExt;

    let path = home.join("escalate-mock");
    let script = fs::read_to_string(&path).unwrap();
    let body = script.strip_prefix("#!/bin/sh\n").unwrap_or(&script);
    let gate_status = format!(
        "if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"gate-status\" ]; then\nprintf '%s\\n' '{payload}'\nexit 0\nfi\n"
    );
    fs::write(&path, format!("#!/bin/sh\n{gate_status}{body}")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
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

/// A quiet board whose drain read answers each count in order (the last
/// repeats), plus an escalate argv recorder. A `None` count is the unreadable
/// drain: the stub exits 1, which production records as the i64::MAX
/// sentinel. Pre-staging wins: lead_prepare_fixture stages its default mock
/// only when the file is absent.
fn lead_quiet_drain_bin(dir: &Path, counts: &[Option<i64>], log: &Path) -> PathBuf {
    let spec = org_board_bin(dir, BOARD_CLEAN, 0);
    let counter = dir.join("drain-count");
    let n = counts.len();
    let arms: String = counts
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let label = if i + 1 == n {
                "*"
            } else {
                &(i + 1).to_string()
            };
            match c {
                Some(v) => {
                    format!("  {label}) echo '{{\"scope\":\"drain\",\"undelivered\":{v}}}';;\n")
                }
                None => format!("  {label}) exit 1;;\n"),
            }
        })
        .collect();
    make_script(
        dir,
        "escalate-mock",
        &format!(
            "if [ \"$1\" = agents ] && [ \"$2\" = lead ] && [ \"$3\" = drain ]; then\n\
             n=$(cat {counter} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {counter}\n\
             case $n in\n{arms}esac\n\
             exit 0\nfi\n\
             if [ \"$1\" = agents ] && [ \"$2\" = lead ] && [ \"$3\" = escalate ]; then\n\
             echo \"$*\" >> {log}\n\
             echo q-mock\n\
             exit 0\nfi\n\
             exit 0",
            counter = counter.display(),
            arms = arms,
            log = log.display(),
        ),
    );
    spec
}

#[test]
fn lead_arm_blocks_while_the_board_is_not_empty() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-block");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0, "a non-empty board must block: {d}");
    assert_eq!(d["decision"], "block");
    assert_eq!(d["actionable"], 2);
    let reason = d["reason"].as_str().unwrap();
    assert!(
        reason.contains("undispatched") && reason.contains("x-1234"),
        "the block reason must name the top actionable row: {reason}"
    );
}

#[test]
fn a_fleet_stop_probe_allows_nowork_and_names_the_incident_owner() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-fleet-stop");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);
    lead_prepare_fixture(cwd, bin_dir.path(), &fno);
    lead_gate_status_mock(
        bin_dir.path(),
        r#"{"verdict":"refused","reason":"fleet-stop","message":"fleet incident stop is active (generation 19, reason: repro)"}"#,
    );

    let (code, decision) = lead_spawn(&state, cwd, &events, bin_dir.path());

    assert_eq!(code, 0, "the fire must return its decision: {decision}");
    assert_eq!(decision["decision"], "allow");
    assert_eq!(decision["termination_reason"], "NoWork");
    assert!(
        decision["reason"]
            .as_str()
            .unwrap()
            .contains("fno agents incident clear --reason"),
        "the message must name the owner: {decision}"
    );
}

#[test]
fn a_gate_status_probe_without_json_keeps_the_lead_blocking() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-broken-gate-probe");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, decision) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0, "the fire must return its decision: {decision}");
    assert_eq!(decision["decision"], "block");
    assert_ne!(decision["termination_reason"], "NoWork");
}

#[test]
fn lead_nowork_is_the_clean_terminal_for_an_empty_board() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-clean");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);

    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow");
    assert_eq!(d["termination_reason"], "NoWork");

    let journal = event_text(&events);
    let row: serde_json::Value = journal
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["type"] == "termination")
        .expect("a termination event must be appended");
    assert_eq!(row["data"]["reason"], "NoWork");
    assert_eq!(
        row["data"]["session_id"], "k-clean",
        "the event must carry the lead session id so the journal reader matches it"
    );
    assert_eq!(row["data"]["driver"], "lead");
}

#[test]
fn an_unacked_stand_down_turn_blocks_a_clean_board() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let state = lead_manifest_with_session(cwd, "k-stand-down", "s-stand");
    let events = cwd.join("events.jsonl");
    let fno = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);
    lead_prepare_fixture(cwd, bin_dir.path(), &fno);
    write_stand_down_transcript(cwd);

    let (code, d) = lead_spawn(&state, cwd, &events, bin_dir.path());

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "block", "decision: {d}");
    assert_eq!(d["termination_reason"], serde_json::Value::Null);
    let reason = d["reason"].as_str().unwrap();
    assert!(reason.contains("turn-stand-down"), "reason: {reason}");
    assert!(
        reason.contains("fno inbox operator ack"),
        "reason: {reason}"
    );
}

#[test]
fn an_acked_stand_down_turn_allows_a_clean_board_to_finish() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let state = lead_manifest_with_session(cwd, "k-stand-down-acked", "s-stand");
    let events = cwd.join("events.jsonl");
    let fno = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);
    lead_prepare_fixture(cwd, bin_dir.path(), &fno);
    write_stand_down_transcript(cwd);
    let capture_dir = bin_dir.path().join(".fno/operator-capture");
    fs::create_dir_all(&capture_dir).unwrap();
    fs::write(
        capture_dir.join("s-stand.jsonl"),
        "{\"turn_id\":\"turn-stand-down\",\"outcome\":\"nothing\"}\n",
    )
    .unwrap();

    let (code, d) = lead_spawn(&state, cwd, &events, bin_dir.path());

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow", "decision: {d}");
    assert_eq!(d["termination_reason"], "NoWork");
}

#[test]
fn an_unacked_stand_down_turn_reaches_noprogress_backstop() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let state = lead_manifest_with_session(cwd, "k-stand-down-dry", "s-stand");
    let events = cwd.join("events.jsonl");
    let fno = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);
    lead_prepare_fixture(cwd, bin_dir.path(), &fno);
    write_stand_down_transcript(cwd);
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-stand-down-dry"}),
    );
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-stand-down-dry"}),
    );

    let (code, d) = lead_spawn(&state, cwd, &events, bin_dir.path());

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow", "decision: {d}");
    assert_eq!(d["termination_reason"], "NoProgress");
    assert!(d["reason"].as_str().unwrap().contains("turn-stand-down"));
}

#[test]
fn an_open_question_from_this_lead_allows_a_clean_board_to_wait() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-question");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);
    let questions = bin_dir.path().join("questions.jsonl");
    fs::write(
        questions,
        r#"{"ts":"2026-09-06T00:00:00Z","type":"operator_question","data":{"question_id":"q-k-question","question":"choose","session_id":"k-question"}}
"#,
    )
    .unwrap();

    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow", "decision: {d}");
    assert_eq!(d["termination_reason"], "NoWork");
    assert!(
        d["reason"]
            .as_str()
            .unwrap()
            .starts_with("waiting on the user"),
        "the stop must name the user wait: {d}"
    );
}

#[test]
fn an_unreadable_question_source_blocks_a_clean_board() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-question-unreadable");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let spec = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);
    lead_prepare_fixture(cwd, bin_dir.path(), &spec);
    write_unreadable_questions_stub(bin_dir.path());

    let (code, d) = lead_spawn(&state, cwd, &events, bin_dir.path());

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "block");
    assert_eq!(d["termination_reason"], serde_json::Value::Null);
    assert!(
        d["reason"]
            .as_str()
            .unwrap()
            .contains("outstanding operator questions are unreadable"),
        "the block must name the unreadable question source: {d}"
    );
}

/// The stub every fire of an unreadable-questions test needs. `lead_fire`
/// rewrites the fixture's fno-py on every call, so the override happens once,
/// after the fixture is prepared, and the test spawns directly.
fn write_unreadable_questions_stub(bin_dir: &Path) {
    fs::write(
        bin_dir.join("stubs").join("fno-py"),
        "#!/bin/sh\ncase \"$*\" in\n  *\"inbox outstanding\"*) exit 1;;\n  *\"backlog ready\"*) echo '[]';;\n  *) echo '{}';;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(
        bin_dir.join("stubs").join("fno-py"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[test]
fn lead_arm_allows_silently_when_no_lead_manifest_exists() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, d) = lead_fire(&cwd.join("absent.md"), cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow");
    assert!(
        !events.exists(),
        "a non-lead session must write no lead events"
    );
}

#[test]
fn lead_arm_never_reads_the_target_manifest() {
    // The kill criterion this arm ships under says a diff reaching into the
    // target arm means the second-driver framing was wrong. This asserts the
    // runtime half of that: a target manifest sitting in the same checkout
    // changes nothing about a lead fire.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::write(
        cwd.join(".fno/target-state.md"),
        "---\nsession_id: t-1\n---\n",
    )
    .unwrap();
    let state = lead_manifest(cwd, "k-iso");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);

    let (code, d) = lead_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "NoWork");
}

#[test]
fn lead_arm_honors_the_cancel_sentinel() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    let state = lead_manifest(cwd, "k-cancel");
    fs::write(state.with_extension("cancelled"), "").unwrap();
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, d) = lead_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "Interrupted");
}

#[test]
fn a_team_with_no_checkin_gets_one_hook_row_per_missed_beat() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-hook");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code1, d1) = lead_fire(&state, cwd, &events, &fno);
    let (code2, d2) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code1, 0, "fire 1: {d1}");
    assert_eq!(code2, 0, "fire 2: {d2}");
    assert_eq!(d1["decision"], "block", "fire 1: {d1}");
    assert_eq!(d2["decision"], "block", "fire 2: {d2}");
    let rows = event_text(&events);
    let checkins: Vec<&str> = rows
        .lines()
        .filter(|l| l.contains("\"lead_checkin\""))
        .collect();
    assert_eq!(checkins.len(), 1, "exactly one mechanical row: {rows}");
    assert!(
        checkins[0].contains("\"source\":\"hook\""),
        "row: {}",
        checkins[0]
    );
    let row: serde_json::Value = serde_json::from_str(checkins[0]).unwrap();
    assert_eq!(row["data"]["scope"], "drain");
    assert!(!row["data"]["change"].as_str().unwrap_or("").is_empty());
}

#[test]
fn a_cancelled_team_writes_no_hook_row() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    let state = lead_manifest(cwd, "k-cancel-hook");
    fs::write(state.with_extension("cancelled"), "").unwrap();
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let (code, d) = lead_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "Interrupted");
    let wrote_checkin = event_text(&events).contains("\"lead_checkin\"");
    assert!(
        !wrote_checkin,
        "a cancelled team writes no lead_checkin row"
    );
}

#[test]
fn lead_arm_blocks_rather_than_certifying_a_board_it_cannot_read() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-blind");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), "not json at all", 1);

    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    // The transport's exit-2 fail-closed path died with the subprocess read:
    // the collector always answers, and blindness degrades into unreadable
    // queues. The contract that survives is the one that matters - a blind
    // board never certifies the lead done.
    assert_eq!(code, 0, "the fire decides on a blind board: {d}");
    assert_eq!(d["decision"], "block", "blind is not clean: {d}");
    assert_ne!(
        d["termination_reason"], "NoWork",
        "a blind board is not a clean terminal: {d}"
    );
}

/// Append one event row to a lead journal.
fn lead_event(events: &Path, event_type: &str, data: serde_json::Value) {
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
    // The defect this replaces: progress keyed ONLY on a lead_action event, and
    // nothing in the repo emitted one, so every lead hit NoProgress on fire 3.
    // A row leaving the board is external truth and needs no producer at all.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-cleared");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    // Two dry fires that recorded both rows...
    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-cleared", "actionable_ids": ids}),
    );
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-cleared", "actionable_ids": ids}),
    );

    // ...then a board with x-1234 gone. That is work the lead did.
    let fno = org_board_bin(bin_dir.path(), BOARD_ONE_CLEARED, 0);
    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0, "clearing a row must keep the loop running: {d}");
    assert_eq!(
        d["decision"], "block",
        "a block at exit 0 must still say so in the JSON: {d}"
    );
    assert_eq!(d["fires"], 1, "the dry-fire counter must have reset");
}

#[test]
fn the_cleared_row_reset_survives_into_the_next_fire() {
    // The defect: `lead_decide` reset a LOCAL `dry` and the journal kept the
    // rows, so the next fire recounted them. The 3-fire tolerance shrank by one
    // per fire and a lead that was demonstrably working still died NoProgress.
    //
    // The sibling test above passes either way, because it asserts only the
    // fire that clears. This one asserts the fire AFTER it, which is where the
    // forgetting showed up.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-durable");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-durable", "actionable_ids": ids}),
    );
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-durable", "actionable_ids": ids}),
    );

    // Fire that clears x-1234.
    let cleared_bin = org_board_bin(bin_dir.path(), BOARD_ONE_CLEARED, 0);
    let (code, d) = lead_fire(&state, cwd, &events, &cleared_bin);
    assert_eq!(code, 0, "the clearing fire must keep running: {d}");
    assert_eq!(d["decision"], "block");

    // The very next fire clears nothing. Pre-fix this read three dry fires and
    // terminated; the lead had just done real work one fire earlier.
    let (code, d) = lead_fire(&state, cwd, &events, &cleared_bin);
    assert_eq!(
        code, 0,
        "a single dry fire after real progress must not end the lead: {d}"
    );
    assert_eq!(d["decision"], "block");
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
    // lead works, so a count that went up can still carry real progress.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-refill");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-refill", "actionable_ids": ids}),
    );
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-refill", "actionable_ids": ids}),
    );

    let fno = org_board_bin(bin_dir.path(), BOARD_REFILLED, 0);
    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0, "a grown board that cleared a row is progress: {d}");
    assert_eq!(d["decision"], "block");
    assert_eq!(d["actionable"], 3);
    assert_eq!(d["fires"], 1);
}

#[test]
fn an_unchanged_board_clears_nothing_and_still_reaches_noprogress() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-same");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();

    let ids = serde_json::json!(["undispatched:x-1234", "undispatched:x-5678"]);
    for _ in 0..2 {
        lead_event(
            &events,
            "lead_loop_check",
            serde_json::json!({"session_id": "k-same", "actionable_ids": ids}),
        );
    }

    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);
    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "NoProgress");
}

#[test]
fn a_fire_records_the_actionable_ids_the_next_fire_compares_against() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-record");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    lead_fire(&state, cwd, &events, &fno);

    let journal = event_text(&events);
    let row: serde_json::Value = journal
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["type"] == "lead_loop_check")
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
fn lead_progress_is_an_action_against_a_target_id_not_seen_before() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-progress");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    // Two dry fires, then a real action: the counter goes back to zero, so the
    // next fire blocks rather than giving up.
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-progress"}),
    );
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-progress"}),
    );
    lead_event(
        &events,
        "lead_action",
        serde_json::json!({"session_id": "k-progress", "kind": "dispatch", "target_id": "x-1234"}),
    );

    let (code, d) = lead_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0, "progress must keep the loop running: {d}");
    assert_eq!(d["decision"], "block");
    assert_eq!(d["fires"], 1, "the dry-fire counter must have reset");
}

#[test]
fn lead_noprogress_ends_a_board_that_refuses_to_shrink() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-stuck");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-stuck"}),
    );
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-stuck"}),
    );

    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(code, 0);
    assert_eq!(d["termination_reason"], "NoProgress");
    let reason = d["reason"].as_str().unwrap();
    assert!(
        reason.contains('2') && reason.contains("actionable"),
        "the terminal must name what stayed unshrunk: {reason}"
    );
}

#[test]
fn a_repeated_lead_action_is_not_progress() {
    // The specific way this loop would fail to converge, and it passes every
    // naive test: `stalled_holder` rows survive the one action a lead has for
    // them, so re-waking the same node forever would reset the counter forever.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-repeat");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    let wake = serde_json::json!({"session_id": "k-repeat", "kind": "wake", "target_id": "x-1234"});
    lead_event(&events, "lead_action", wake.clone());
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-repeat"}),
    );
    lead_event(&events, "lead_action", wake.clone());
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-repeat"}),
    );

    let (code, d) = lead_fire(&state, cwd, &events, &fno);

    assert_eq!(
        code, 0,
        "a repeated action must not hold the loop open: {d}"
    );
    assert_eq!(d["termination_reason"], "NoProgress");
}

#[test]
fn another_leads_events_do_not_move_this_leads_counter() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-mine");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);

    for _ in 0..5 {
        lead_event(
            &events,
            "lead_loop_check",
            serde_json::json!({"session_id": "k-other"}),
        );
    }

    let (code, d) = lead_fire(&state, cwd, &events, &fno);
    assert_eq!(code, 0, "a sibling lead's fires are not mine: {d}");
    assert_eq!(d["decision"], "block");
    assert_eq!(d["fires"], 1);
}

#[test]
fn an_empty_board_wins_over_a_dry_fire_streak() {
    // NoWork is the clean terminal and must not be pre-empted by NoProgress:
    // a lead that drained its board on the third fire finished, it did not stall.
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-drained");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let fno = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);

    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-drained"}),
    );
    lead_event(
        &events,
        "lead_loop_check",
        serde_json::json!({"session_id": "k-drained"}),
    );

    let (code, d) = lead_fire(&state, cwd, &events, &fno);
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
        err.contains("emperor") && err.contains("lead"),
        "got: {err}"
    );
}

/// A mock `fno` that answers `inbox board --json` and LOGS every
/// `agents lead escalate`
/// argv, so a test can read back which paths escalated and over what.
fn lead_escalate_bin(dir: &Path, payload: &str, log: &Path) -> PathBuf {
    // The board half of this mock is dead (the board is read in process).
    // What remains: the SPEC the fixture pipeline reads, and the escalate
    // argv recorder this returns implicitly through lead_spawn's fixed-name
    // --fno-bin wiring. Returns the SPEC path, which is what lead_fire takes.
    fs::write(dir.join("board-spec.json"), payload).unwrap();
    make_script(
        dir,
        "escalate-mock",
        &format!(
            "if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"lead\" ] && [ \"$3\" = \"drain\" ]; then\n\
             \x20 {}\n\
             \x20 exit 0\n\
             fi\n\
             if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"lead\" ] && [ \"$3\" = \"escalate\" ]; then\n\
             \x20 echo \"$*\" >> {log}\n\
             \x20 echo q-mock\n\
             \x20 exit 0\n\
             fi\n\
             exit 0",
            lead_drain_reply(payload),
            log = log.display()
        ),
    );
    dir.join("board-spec.json")
}

/// Plan verification 7, first half: EVERY NoProgress terminal escalates.
///
/// `lead_decide` reaches NoProgress three ways: a board it could not read for
/// the whole dry-fire run, a board whose rows nothing cleared, and a quiet
/// board whose scope still holds undelivered nodes. All route
/// through the shared `terminate` closure, so both escalate and a terminal
/// added later is covered without anyone remembering to wire it. This drives
/// both rather than asserting the helper they share, which would pin the
/// function and not the destination.
///
/// The second half, that repeated calls over one stalled set yield exactly ONE
/// operator question, is `test_lead_escalate.py`: the dedupe lives in the verb,
/// and a mock `fno` here records no questions to count. Neither test alone is
/// the verification; the seam between them is the `--stalled` argument.
#[test]
fn every_lead_noprogress_terminal_escalates() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let log = cwd.join("escalations.log");
    let state = lead_manifest(cwd, "k-escalate");
    let events = cwd.join("events.jsonl");
    let fno = lead_escalate_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, &log);

    // Terminal 1: a readable board whose rows nothing clears.
    let mut last = (0, serde_json::Value::Null);
    for _ in 0..3 {
        last = lead_fire(&state, cwd, &events, &fno);
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
    // told nothing is the failure this verb exists to prevent, and a lead that
    // cannot see its board is the case most worth telling them about.
    let blind_tmp = TempDir::new().unwrap();
    let blind_cwd = blind_tmp.path();
    let blind_log = blind_cwd.join("escalations.log");
    let blind_state = lead_manifest(blind_cwd, "k-blind");
    let blind_events = blind_cwd.join("events.jsonl");
    let blind_bin_dir = TempDir::new().unwrap();
    let blind_bin = lead_escalate_bin(blind_bin_dir.path(), "not json at all", &blind_log);

    let mut blind = (0, serde_json::Value::Null);
    for _ in 0..3 {
        blind = lead_fire(&blind_state, blind_cwd, &blind_events, &blind_bin);
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
        blind_calls[0].contains("--stalled reading:board-unreadable"),
        "the blind terminal names the reading it measured (x-ff27), got: {}",
        blind_calls[0]
    );
}

/// The ceiling `--max-iterations` advertises must actually bind.
///
/// It was parsed into the manifest and read by nothing, so the help string
/// promised a bound that did not exist. A help string that lies is worse than
/// a missing flag: someone sets it, believes the lead is bounded, and walks
/// away.
///
/// Progress is deliberately irrelevant here. A lead clearing a row every fire
/// never trips the dry-fire counter, so without this it runs forever. That is
/// the case the flag exists for.
#[test]
fn the_manifest_iteration_ceiling_stops_a_lead_that_is_still_working() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let events = cwd.join("events.jsonl");

    // A manifest with a ceiling of 3 rather than the default 40.
    let state = cwd.join("lead-state.md");
    fs::write(
        &state,
        format!(
            "---\nfno_id: k-budget\ncreated_at: {}\nscope: drain\n\
             harness: claude\nbudget_max_iterations: 3\n---\n",
            recent_created_at()
        ),
    )
    .unwrap();

    // Every fire clears a row, so the dry-fire counter never trips.
    let boards = [BOARD_TWO_ACTIONABLE, BOARD_ONE_CLEARED, BOARD_REFILLED];
    let mut last = (0, serde_json::Value::Null);
    for (i, payload) in boards.iter().enumerate() {
        let board_home = TempDir::new().unwrap();
        let fno = org_board_bin(board_home.path(), payload, 0);
        last = lead_fire(&state, cwd, &events, &fno);
        if i < boards.len() - 1 {
            assert_eq!(
                last.0, 0,
                "fire {i} must keep the lead running: {:?}",
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
        "a lead that cleared a row every fire did not stall"
    );
}

/// A terminal that is NOT NoProgress never escalates. NoWork is the lead's
/// clean exit; asking the operator about a board it just emptied would train
/// them to ignore the queue this feature depends on.
#[test]
fn a_clean_lead_terminal_does_not_ask_the_operator_anything() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let log = cwd.join("escalations.log");
    let state = lead_manifest(cwd, "k-clean");
    let events = cwd.join("events.jsonl");
    let fno = lead_escalate_bin(bin_dir.path(), BOARD_CLEAN, &log);

    let (_, json) = lead_fire(&state, cwd, &events, &fno);
    assert_eq!(json["termination_reason"], "NoWork");
    assert!(
        !log.exists(),
        "a NoWork terminal must not escalate: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
}

/// Exactly the `fno inbox outstanding` read never answers; every other read
/// of the same binary answers clean, so the timeout is attributable to ONE
/// slice. (The ready selection and the undispatched fold answer in-process
/// now; a wedged source must be one that still rides a subprocess.)
#[test]
fn external_read_timeout_dies_at_its_slice_and_is_named() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-wedge");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let spec = org_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);
    lead_prepare_fixture(cwd, bin_dir.path(), &spec);

    let stubs = bin_dir.path().join("stubs");
    fs::write(
        stubs.join("fno-py"),
        "#!/bin/sh\ncase \"$*\" in\n  *\"inbox outstanding\"*) exec sleep 30;;\n  *) echo '{}';;\nesac\n",
    )
    .unwrap();

    let started = std::time::Instant::now();
    let (code, json) = lead_spawn_with(
        &state,
        cwd,
        &events,
        bin_dir.path(),
        &["--read-timeout-ms", "6000"],
    );
    let elapsed = started.elapsed();
    let d = json;
    // A slice kill is the board's own choice, not evidence: it never blocks
    // by itself. The rows the board did read still decide, and the kill is
    // named in the message instead of hiding behind them (x-1867).
    assert_eq!(d["decision"], "block", "{d}");
    assert!(d["termination_reason"].is_null(), "{d}");
    let message = d["reason"].as_str().unwrap_or_default();
    assert!(
        message.contains("not read: ") && message.contains("killed at its"),
        "the kill is named in the block message: {d}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "a wedged source must die at its slice, not hang the fire: {elapsed:?}"
    );
    assert_eq!(
        code, 0,
        "a decided beat keeps the exit clean like any quiet beat: {code}"
    );

    // The killed read is named where the payload carries it.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .envs(fno_agents::test_run::self_owner_env())
        .args([
            "board",
            "--json",
            "--budget-ms",
            "5000",
            "--state",
            state.to_str().unwrap(),
        ])
        .env("FNO_CLAIMS_ROOT", bin_dir.path())
        .env("FNO_HOME", bin_dir.path())
        .env("FNO_PY", stubs.join("fno-py"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                stubs.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .unwrap();
    let board: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let outstanding_err = board["sources"]["outstanding"]["error"]
        .as_str()
        .unwrap_or("");
    assert!(
        outstanding_err.contains("killed at its")
            && outstanding_err.contains("slice of the board budget"),
        "the killed source is named in the payload as a budget kill: {outstanding_err} :: sources={}",
        board["sources"]
    );
}

// ── the quiet-board bound (x-1959's defect) ──────────────────────────────────
//
// A board reading zero actionable while scope nodes sit undelivered used to
// return above the manifest ceiling and the dry-fire backstop: blocking was
// unbounded and the parked state was never recorded. These drive the bound.

/// AC1: a quiet board with undelivered scope waits for CI or a worker.
#[test]
fn a_quiet_board_with_undelivered_scope_terminates_nowork_while_waiting() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let log = cwd.join("escalations.log");
    let state = lead_manifest(cwd, "k-quiet");
    let events = cwd.join("events.jsonl");
    let spec = lead_quiet_drain_bin(bin_dir.path(), &[Some(3)], &log);

    let last = lead_fire(&state, cwd, &events, &spec);

    assert_eq!(last.0, 0, "{:?}", last.1);
    assert_eq!(last.1["decision"], "allow", "{:?}", last.1);
    assert_eq!(last.1["termination_reason"], "NoWork");
    assert!(last.1["reason"]
        .as_str()
        .unwrap()
        .starts_with("waiting on CI or a worker"));
    let logged = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !logged.contains("--reason NoProgress"),
        "a legal wait must not escalate: {logged}"
    );
}

/// AC2: the undelivered journal row is written before the legal wait terminal.
#[test]
fn a_quiet_board_records_undelivered_before_nowork() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let log = cwd.join("escalations.log");
    let state = lead_manifest_with_budget(cwd, "k-budget", 3);
    let events = cwd.join("events.jsonl");
    let spec = lead_quiet_drain_bin(bin_dir.path(), &[Some(3)], &log);

    let last = lead_fire(&state, cwd, &events, &spec);

    assert_eq!(last.1["decision"], "allow", "{:?}", last.1);
    assert_eq!(last.1["termination_reason"], "NoWork");
    assert!(
        event_text(&events).contains("\"undelivered\":3"),
        "the wait must retain its undelivered journal row"
    );
}

/// AC4: an unreadable drain is the i64::MAX sentinel and never a baseline;
/// a later real count is a legal wait, not progress against the sentinel.
#[test]
fn an_unreadable_drain_is_never_a_progress_baseline() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let bin_dir = TempDir::new().unwrap();
    let log = cwd.join("escalations.log");
    let state = lead_manifest(cwd, "k-blind-drain");
    let events = cwd.join("events.jsonl");
    let spec = lead_quiet_drain_bin(bin_dir.path(), &[None, None, Some(5)], &log);

    let fires: Vec<_> = (0..3)
        .map(|_| lead_fire(&state, cwd, &events, &spec))
        .collect();

    assert_eq!(fires[1].1["decision"], "block", "{:?}", fires[1].1);
    // Had the sentinel been recorded as a baseline, 5 < i64::MAX would read
    // as progress. The later successful read is a legal wait instead.
    assert_eq!(
        fires[2].1["termination_reason"], "NoWork",
        "{:?}",
        fires[2].1
    );
    assert!(fires[2].1["reason"]
        .as_str()
        .unwrap()
        .starts_with("waiting on CI or a worker"));
}

/// AC5: the unreadable-questions block is bounded, and each blocking fire
/// emitted its journal row so the counters advanced to the bound.
#[test]
fn an_unreadable_question_source_is_bounded_not_eternal() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-questions-bounded");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let spec = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);
    lead_prepare_fixture(cwd, bin_dir.path(), &spec);
    write_unreadable_questions_stub(bin_dir.path());

    let mut last = (0, serde_json::Value::Null);
    for _ in 0..3 {
        last = lead_spawn(&state, cwd, &events, bin_dir.path());
    }

    assert_eq!(last.1["decision"], "allow", "{:?}", last.1);
    assert_eq!(last.1["termination_reason"], "NoProgress");
    let journal = event_text(&events);
    let rows = journal
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["type"] == "lead_loop_check")
        .filter(|v| v["data"]["session_id"] == "k-questions-bounded")
        .count();
    assert!(
        rows >= 2,
        "every blocking fire must emit a journal row: {rows} rows in {journal}"
    );
}

/// AC6: an open operator question is a legal wait and the terminal is durable.
#[test]
fn an_open_operator_question_records_one_nowork_terminal() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = lead_manifest(cwd, "k-question");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let spec = org_board_bin(bin_dir.path(), BOARD_CLEAN, 0);
    let questions = bin_dir.path().join("questions.jsonl");
    fs::write(
        &questions,
        r#"{"ts":"2026-09-06T00:00:00Z","type":"operator_question","data":{"question_id":"q-k-open-forever","question":"choose","session_id":"k-question"}}
"#,
    )
    .unwrap();

    let (code, d) = lead_fire(&state, cwd, &events, &spec);
    assert_eq!(code, 0);
    assert_eq!(d["decision"], "allow", "fire: {:?}", d);
    assert_eq!(d["termination_reason"], "NoWork", "fire: {:?}", d);
    assert!(d["reason"]
        .as_str()
        .unwrap()
        .starts_with("waiting on the user"));

    let (repeat_code, repeat) = lead_fire(&state, cwd, &events, &spec);
    assert_eq!(repeat_code, 0);
    assert_eq!(repeat["decision"], "allow", "repeat: {:?}", repeat);
    assert!(repeat["reason"]
        .as_str()
        .unwrap()
        .contains("already terminal"));
    let terminals = event_text(&events)
        .lines()
        .filter(|line| line.contains("\"type\":\"termination\""))
        .count();
    assert_eq!(terminals, 1, "a re-wake must not journal a second terminal");
}
