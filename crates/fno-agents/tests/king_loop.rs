//! The king driver arm, end to end: a crowned session's stop gate reads the
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

// ── the king driver arm ───────────────────────────────────────────────────────
//
// A king has no PR, so none of the target conjuncts above apply. These drive
// the same verb with `--driver king` over a king manifest and a mocked board.

fn king_manifest(dir: &Path, fno_id: &str) -> PathBuf {
    let path = dir.join("king-state.md");
    fs::write(
        &path,
        format!(
            "---\nfno_id: {fno_id}\ncreated_at: 2026-08-18T00:00:00Z\nscope: drain\n\
             harness: claude\nbudget_max_iterations: 40\n---\n"
        ),
    )
    .unwrap();
    path
}

/// A board the fixture serves. The canned payloads died with the subprocess
/// board read (x-25b8: the stop gate reads the collector in process), so a
/// spec now names the graph the fixture writes: the rows of the spec's
/// undispatched queue become planned, ready, unclaimed nodes, and the
/// decision comes out the real pipeline. An unparseable spec is the blind
/// case: the graph source goes dark.
fn king_board_bin(dir: &Path, payload: &str, _exit: i32) -> PathBuf {
    let path = dir.join("board-spec.json");
    fs::write(&path, payload).unwrap();
    path
}

/// Write the fixture graph for `board_spec` and pin config + lane at `cwd`.
/// The epic `drain` matches the manifest scope; every spec row becomes one
/// undispatchable planned node.
fn king_prepare_fixture(cwd: &Path, home: &Path, board_spec: &Path) {
    let fno_dir = cwd.join(".fno");
    fs::create_dir_all(&fno_dir).unwrap();
    let graph = cwd.join("graph.json");
    let spec = fs::read_to_string(board_spec).unwrap_or_default();
    let parsed: Option<serde_json::Value> = serde_json::from_str(&spec).ok();
    let ids: Vec<String> = parsed
        .as_ref()
        .and_then(|v| {
            let rows = v["queues"]
                .as_array()?
                .iter()
                .find(|q| q["name"] == "undispatched")?["rows"]
                .as_array()
                .cloned()?;
            Some(
                rows.iter()
                    .filter_map(|r| r["id"].as_str().map(str::to_string))
                    .collect(),
            )
        })
        .unwrap_or_default();
    if parsed.is_none() {
        // Blind: the spec never parsed, so the graph source goes dark and the
        // collector answers with unreadable queues instead of a payload that
        // was never possible to fake here.
        let _ = fs::remove_file(&graph);
    } else {
        let nodes: Vec<serde_json::Value> = std::iter::once(serde_json::json!(
            {"id": "drain", "type": "epic", "status": "ready", "priority": "p1"}
        ))
        .chain(ids.into_iter().map(|id| {
            // parent: the manifest scope compiles to the epic plus its
            // descendants, so a workable row is a child of `drain`.
            serde_json::json!({"id": id, "type": "feature", "status": "ready",
                               "priority": "p0", "plan_path": "/plans/p.md",
                               "parent": "drain"})
        }))
        .collect();
        fs::write(
            &graph,
            serde_json::to_string(&serde_json::json!({ "entries": nodes })).unwrap(),
        )
        .unwrap();
    }
    fs::write(
        fno_dir.join("config.toml"),
        format!(
            "[paths]\ngraph_json = \"{}\"\noperator_lane = \"{}\"\n",
            graph.display(),
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
    fs::write(
        stubs.join("fno-py"),
        "#!/bin/sh\ncase \"$*\" in\n  *\"backlog ready\"*) echo '[]';;\n  *) echo '{}';;\nesac\n",
    )
    .unwrap();
    // Stage the default mock only when absent: king_escalate_bin stages its
    // argv-recording version FIRST, and this fixture prep runs after it.
    let mock = home.join("escalate-mock");
    if !mock.is_file() {
        make_script(
            home,
            "escalate-mock",
            "if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"king\" ] && [ \"$3\" = \"escalate\" ]; then\n\
             \x20 echo q-mock\n\
             \x20 exit 0\n\
             fi\n\
             exit 0",
        );
    };
    #[cfg(unix)]
    for stub in ["gh", "fno-py", "fno"] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(stubs.join(stub), fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn king_spawn(state: &Path, cwd: &Path, events: &Path, home: &Path) -> (i32, serde_json::Value) {
    king_spawn_with(state, cwd, events, home, &[])
}

/// `extra` carries per-fire CLI overrides, e.g. a short `--read-timeout-ms`
/// for the wedged-source test. The bound is the whole FIRE's ceiling (the
/// board's budget derives from it minus the serialization reserve), so only
/// a fire that needs a killed read passes one.
fn king_spawn_with(
    state: &Path,
    cwd: &Path,
    events: &Path,
    home: &Path,
    extra: &[&str],
) -> (i32, serde_json::Value) {
    let stubs = home.join("stubs");
    let real_path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.args([
        "loop-check",
        "--driver",
        "king",
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
        .env("PATH", format!("{}:{}", stubs.display(), real_path))
        .output()
        .unwrap();
    let json = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .unwrap_or(serde_json::Value::Null);
    (out.status.code().unwrap_or(-1), json)
}

fn king_fire(
    state: &Path,
    cwd: &Path,
    events: &Path,
    board_spec: &Path,
) -> (i32, serde_json::Value) {
    let home = board_spec.parent().unwrap();
    king_prepare_fixture(cwd, home, board_spec);
    king_spawn(state, cwd, events, home)
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
    fs::write(state.with_extension("cancelled"), "").unwrap();
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

    // The transport's exit-2 fail-closed path died with the subprocess read:
    // the collector always answers, and blindness degrades into unreadable
    // queues. The contract that survives is the one that matters - a blind
    // board never certifies the king done.
    assert_eq!(code, 0, "the fire decides on a blind board: {d}");
    assert_eq!(d["decision"], "block", "blind is not clean: {d}");
    assert_ne!(
        d["termination_reason"], "NoWork",
        "a blind board is not a clean terminal: {d}"
    );
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

    assert_eq!(code, 0, "clearing a row must keep the loop running: {d}");
    assert_eq!(
        d["decision"], "block",
        "a block at exit 0 must still say so in the JSON: {d}"
    );
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
    assert_eq!(code, 0, "the clearing fire must keep running: {d}");
    assert_eq!(d["decision"], "block");

    // The very next fire clears nothing. Pre-fix this read three dry fires and
    // terminated; the king had just done real work one fire earlier.
    let (code, d) = king_fire(&state, cwd, &events, &cleared_bin);
    assert_eq!(
        code, 0,
        "a single dry fire after real progress must not end the king: {d}"
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

    assert_eq!(code, 0, "a grown board that cleared a row is progress: {d}");
    assert_eq!(d["decision"], "block");
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
    assert_eq!(code, 0, "progress must keep the loop running: {d}");
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
    assert_eq!(code, 0, "a sibling king's fires are not mine: {d}");
    assert_eq!(d["decision"], "block");
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
    // The board half of this mock is dead (the board is read in process).
    // What remains: the SPEC the fixture pipeline reads, and the escalate
    // argv recorder this returns implicitly through king_spawn's fixed-name
    // --fno-bin wiring. Returns the SPEC path, which is what king_fire takes.
    fs::write(dir.join("board-spec.json"), payload).unwrap();
    make_script(
        dir,
        "escalate-mock",
        &format!(
            "if [ \"$1\" = \"agents\" ] && [ \"$2\" = \"king\" ] && [ \"$3\" = \"escalate\" ]; then\n\
             \x20 echo \"$*\" >> {log}\n\
             \x20 echo q-mock\n\
             \x20 exit 0\n\
             fi\n\
             exit 0",
            log = log.display()
        ),
    );
    dir.join("board-spec.json")
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
                last.0, 0,
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

/// killed read.
#[test]
fn external_read_timeout_king_board_blocks_named() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    let state = king_manifest(cwd, "k-wedge");
    let events = cwd.join("events.jsonl");
    let bin_dir = TempDir::new().unwrap();
    let spec = king_board_bin(bin_dir.path(), BOARD_TWO_ACTIONABLE, 0);
    king_prepare_fixture(cwd, bin_dir.path(), &spec);

    // Exactly the `backlog ready` read never answers; every other read of the
    // same binary answers clean, so the timeout is attributable to ONE slice.
    let stubs = bin_dir.path().join("stubs");
    fs::write(
        stubs.join("fno-py"),
        "#!/bin/sh\ncase \"$*\" in\n  *\"backlog ready\"*) exec sleep 30;;\n  *) echo '{}';;\nesac\n",
    )
    .unwrap();

    let started = std::time::Instant::now();
    let (code, json) = king_spawn_with(
        &state,
        cwd,
        &events,
        bin_dir.path(),
        &["--read-timeout-ms", "6000"],
    );
    let elapsed = started.elapsed();
    let d = json;
    assert_eq!(d["decision"], "block", "{d}");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "a wedged source must die at its slice, not hang the fire: {elapsed:?}"
    );
    assert_eq!(
        code, 0,
        "a block keeps the king running like any non-empty board: {code}"
    );

    // The killed read is named where the payload carries it.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fno-agents"))
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
    let ready_err = board["sources"]["ready"]["error"].as_str().unwrap_or("");
    assert!(
        ready_err.contains("timed out after"),
        "the killed source is named in the payload: {ready_err}"
    );
}
