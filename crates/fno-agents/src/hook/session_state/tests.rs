//! The session-state contracts, ported from the three shell suites the
//! entry replaces (blocked/report, markers/pin, picker): every case guards
//! the same contract here, against the implementation itself.

use super::*;
use crate::hook::adapter;
use serde_json::json;
use std::path::PathBuf;

/// The env-pinned tests share one process env with every other env-using
/// test in the binary, so they serialize on the crate's shared lock, not a
/// private one (a private lock let these mutations race hook::stop's).
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The wiring a row's session_state audit declares; every call site here
/// tests a supported row, so a missing wiring is the failure.
fn job(name: &str) -> crate::harness_capabilities::HookWiring {
    crate::harness_capabilities::HarnessContract::packaged()
        .unwrap()
        .hook_job(name, "session_state")
        .unwrap()
        .wiring
        .clone()
        .unwrap()
}

fn claude_job() -> crate::harness_capabilities::HookWiring {
    job("claude")
}

fn ev(event: &str, extra: serde_json::Value) -> adapter::HookEvent {
    let mut payload = json!({"session_id": "sess-1", "hook_event_name": event});
    let obj = payload.as_object_mut().unwrap();
    if let serde_json::Value::Object(extra) = extra {
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
    adapter::normalize("claude", event, &payload)
}

// --- the state decision (the blocked/report side) ---

/// T1: a Notification payload with a message decides blocked carrying the
/// reason; the row's `blocked` key names the event.
#[test]
fn notification_blocks_with_the_payload_message() {
    let d = decide(
        &claude_job(),
        &ev(
            "Notification",
            json!({"message": "waiting on permission to run rm"}),
        ),
    )
    .unwrap();
    assert_eq!(d.state, "blocked");
    assert_eq!(d.reason, "waiting on permission to run rm");
}

/// T8/T9/T10 + picker T1/T2: the picker reclassification. A question in the
/// payload names the block; no question falls back to the static word;
/// ExitPlanMode names the plan approval.
#[test]
fn picker_reclassification_names_what_the_session_waits_on() {
    let q = decide(
        &claude_job(),
        &ev("PreToolUse", json!({"tool_name": "AskUserQuestion",
            "tool_input": {"questions": [{"question": "Pick a budget door: hard cap or soft warn?"}]}})),
    )
    .unwrap();
    assert_eq!(q.state, "blocked");
    assert_eq!(q.reason, "Pick a budget door: hard cap or soft warn?");

    let bare = decide(
        &claude_job(),
        &ev("PreToolUse", json!({"tool_name": "AskUserQuestion"})),
    )
    .unwrap();
    assert_eq!(
        (bare.state, bare.reason.as_str()),
        ("blocked", "asking the user")
    );

    let plan = decide(
        &claude_job(),
        &ev("PreToolUse", json!({"tool_name": "ExitPlanMode"})),
    )
    .unwrap();
    assert_eq!(
        (plan.state, plan.reason.as_str()),
        ("blocked", "plan approval requested")
    );
}

/// Picker T3/T4: a plain tool call stays working, and the next PreToolUse
/// after a picker block decides working again (the clear).
#[test]
fn plain_tool_stays_working_and_the_next_call_clears_a_block() {
    let bash = decide(
        &claude_job(),
        &ev("PreToolUse", json!({"tool_name": "Bash"})),
    )
    .unwrap();
    assert_eq!(bash.state, "working");
    assert_eq!(bash.reason, "");
    let after = decide(
        &claude_job(),
        &ev("PreToolUse", json!({"tool_name": "Bash"})),
    )
    .unwrap();
    assert_eq!(after.state, "working");
}

/// T12/T13: the PostModelSwitch word carries the axes and refuses to fly
/// with none; a Stop decides done.
#[test]
fn model_word_needs_an_axis_and_stop_decides_done() {
    let m = decide(
        &claude_job(),
        &ev(
            "PostModelSwitch",
            json!({"to_model": "glm-5.3[1m]", "effort": {"level": "xhigh"}}),
        ),
    )
    .unwrap();
    assert_eq!(m.state, "model");
    let empty = decide(&claude_job(), &ev("PostModelSwitch", json!({})));
    assert_eq!(empty, None);
    let done = decide(&claude_job(), &ev("Stop", json!({}))).unwrap();
    assert_eq!(done.state, "done");
}

/// The row map is the data: claude wires its four events onto Notification;
/// codex wires three and declares no blocked producer (PermissionRequest is
/// unwired until a payload is captured); a goal-leg PreToolUse decides
/// working (a continuation is a user-role row, so UserPromptSubmit never
/// fires - the tool calls are the only signal).
#[test]
fn the_row_map_wires_claude_and_codex() {
    let claude = claude_job();
    assert_eq!(claude.blocked, "Notification");
    for (event, word) in [
        ("UserPromptSubmit", "working"),
        ("PreToolUse", "working"),
        ("Stop", "done"),
        ("PostModelSwitch", "model"),
    ] {
        assert_eq!(claude.events.get(event).map(String::as_str), Some(word));
    }
    let codex = job("codex");
    assert_eq!(codex.blocked, "none");
    assert_eq!(
        codex.events.get("PreToolUse").map(String::as_str),
        Some("working")
    );
    assert_eq!(codex.events.get("Stop").map(String::as_str), Some("done"));
    assert_eq!(codex.events.get("PostModelSwitch"), None);
    let goal = decide(
        &codex,
        &adapter::normalize(
            "codex",
            "PreToolUse",
            &json!({"session_id": "0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3", "tool_name": "Bash"}),
        ),
    )
    .unwrap();
    assert_eq!(goal.state, "working");
    // An unwired event decides nothing.
    assert_eq!(decide(&claude, &ev("PreCompact", json!({}))), None);
}

/// agy joins the row map: PreInvocation is the turn start (working), Stop
/// ends the execution loop (done), and no permission-prompt producer
/// exists (blocked stays "none" - the screen backstop keeps the row). The
/// adapter files the report under agy's camelCase conversationId.
#[test]
fn the_row_map_wires_agy() {
    let agy = job("agy");
    assert_eq!(agy.blocked, "none");
    assert_eq!(
        agy.events.get("PreInvocation").map(String::as_str),
        Some("working")
    );
    assert_eq!(agy.events.get("Stop").map(String::as_str), Some("done"));
    let turn = decide(
        &agy,
        &adapter::normalize(
            "agy",
            "PreInvocation",
            &json!({"conversationId": "conv-agy-1", "invocationNum": 2}),
        ),
    )
    .unwrap();
    assert_eq!(turn.state, "working");
    let done = decide(
        &agy,
        &adapter::normalize(
            "agy",
            "Stop",
            &json!({"conversationId": "conv-agy-1", "terminationReason": "model_stop", "fullyIdle": true}),
        ),
    )
    .unwrap();
    assert_eq!(done.state, "done");
    // An unwired agy event (PostInvocation) decides nothing.
    assert_eq!(
        decide(
            &agy,
            &adapter::normalize(
                "agy",
                "PostInvocation",
                &json!({"conversationId": "conv-agy-1"})
            )
        ),
        None
    );
}
// --- the marker bytes (the OSC 133 lane) ---

/// T4/T5 + mT3/mT4: working opens a block only on a turn-start event, done
/// closes (re-opening under a /target manifest so every loop leg is its own
/// block), and blocked stays silent.
#[test]
fn marker_bytes_open_close_and_reopen() {
    assert!(marker_bytes("working", "PreToolUse", false).is_empty());
    assert_eq!(
        marker_bytes("working", "UserPromptSubmit", false),
        vec![MARKER_C]
    );
    assert!(marker_bytes("blocked", "UserPromptSubmit", false).is_empty());
    assert_eq!(marker_bytes("done", "Stop", false), vec![MARKER_D]);
    assert_eq!(marker_bytes("done", "Stop", true), vec![MARKER_D, MARKER_C]);
    assert!(marker_bytes("model", "PostModelSwitch", false).is_empty());
}

// --- the transition gate (the 120 s record) ---

/// Seed the gate record the way the gate itself resolves it (inside the
/// rendezvous dir), so the seeded line is the one `should_report` reads.
fn seed_record(sid: &str, body: &str) {
    let dir = runtime_pin_dir().expect("rendezvous dir for the seeded record");
    let safe: String = sid
        .chars()
        .map(|c| if c == '/' || c == '.' { '_' } else { c })
        .collect();
    std::fs::write(dir.join(format!("state-{safe}")), body).unwrap();
}

fn runtime_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fno-ss-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// T2/T6/T7: the gate collapses a same state+reason repeat inside the
/// window, never dedups past a genuine reason change, and re-sends after
/// the ceiling.
#[test]
fn the_transition_gate_collapses_repeats_not_reason_changes() {
    let _g = env_lock();
    let rt = runtime_dir("gate");
    std::env::set_var("XDG_RUNTIME_DIR", &rt);
    let d = Decision {
        state: "working",
        reason: String::new(),
        posture: None,
    };
    // No record: send. Then a mark makes the immediate repeat skip.
    assert!(should_report("s", &d));
    mark_reported("s", &d);
    assert!(!should_report("s", &d));
    // A different reason sends even inside the window.
    let blocked = Decision {
        state: "blocked",
        reason: "waiting on permission A".into(),
        posture: None,
    };
    assert!(should_report("s", &blocked));
    // An aged-out record sends again.
    seed_record("s", "working 0 ");
    assert!(should_report("s", &d));
    // A never-marked state keeps retrying: the failed-send contract.
    let d2 = Decision {
        state: "blocked",
        reason: "waiting on permission B".into(),
        posture: None,
    };
    assert!(should_report("s", &d2));
    assert!(should_report("s", &d2));
    std::env::remove_var("XDG_RUNTIME_DIR");
    let _ = std::fs::remove_dir_all(&rt);
}

// --- the marker identity gate (ancestry verdict + v1 first-writer fallback) ---

/// A census row builder for the synthetic ancestry tables the verdict tests
/// install through `set_test_ancestry_table` (cleared with `None`).
fn census_row(pid: u32, ppid: u32, command: &str) -> (u32, crate::census::ProcRow) {
    (
        pid,
        crate::census::ProcRow {
            pid,
            ppid,
            state: 'S',
            elapsed_s: 0,
            cpu_pct: 0.0,
            rss_kb: 0,
            command: command.to_string(),
        },
    )
}

/// Install a synthetic census table (or clear the override with `None`).
fn install_ancestry(rows: &[(u32, u32, &str)]) {
    let table: std::collections::BTreeMap<u32, crate::census::ProcRow> = rows
        .iter()
        .copied()
        .map(|(pid, ppid, cmd)| census_row(pid, ppid, cmd))
        .collect();
    crate::spawn_context::set_test_ancestry_table(Some(table));
}

/// The real ppid of the test binary, so a synthetic table can name the walk
/// start the hook would see.
fn test_ppid() -> u32 {
    (unsafe { libc::getppid() }) as u32
}

/// Host-shaped ancestry: test binary -> zsh -> fno mux -> launchd. The
/// nearest harness ancestor is the test binary row itself.
fn host_ancestry() -> Vec<(u32, u32, &'static str)> {
    let ppid = test_ppid();
    vec![
        (1, 0, "/sbin/launchd"),
        (5, 1, "fno mux serve"),
        (10, 5, "/bin/zsh -il"),
        (ppid, 10, "claude"),
    ]
}

/// Guest-shaped ancestry: the test binary's nearest harness ancestor sits
/// under ANOTHER harness (a Bash tool running `claude -p` inside a pane
/// claude).
fn nested_ancestry() -> Vec<(u32, u32, &'static str)> {
    let ppid = test_ppid();
    vec![
        (1, 0, "/sbin/launchd"),
        (5, 1, "fno mux serve"),
        (10, 5, "/bin/zsh -il"),
        (20, 10, "claude"),
        (30, 20, "bash /some/hook.sh"),
        (ppid, 30, "claude"),
    ]
}

fn set_pane_env(pane: &str, epoch: &str, server: &str, rt: &std::path::Path) {
    std::env::set_var("FNO_PANE", pane);
    std::env::set_var("FNO_PANE_EPOCH", epoch);
    std::env::set_var("FNO_SERVER", server);
    std::env::set_var("XDG_RUNTIME_DIR", rt);
}

fn clear_pane_env() {
    for k in [
        "FNO_PANE",
        "FNO_PANE_EPOCH",
        "FNO_SERVER",
        "FNO_SESSION",
        "XDG_RUNTIME_DIR",
    ] {
        std::env::remove_var(k);
    }
}

/// mT1/mT2/mT13, updated for the ancestry verdict: the pane's harness is a
/// host, a harness nested under the pane's harness is a guest, and
/// FNO_SERVER vs the legacy FNO_SESSION spelling decide the same. The
/// census-unavailable first-writer fallback keeps its own test below.
#[test]
fn the_pane_host_wins_the_pin_and_a_nested_session_stays_silent() {
    let _g = env_lock();
    let rt = runtime_dir("pin");
    set_pane_env("1", "1000", "main", &rt);
    install_ancestry(&host_ancestry());
    assert!(is_pane_host("host-1"));
    install_ancestry(&nested_ancestry());
    assert!(!is_pane_host("nested-2"));
    // The legacy FNO_SESSION spelling decides the same: seed it, drop
    // FNO_SERVER, and the verdict cache written by the first host-1 call
    // still answers.
    std::env::set_var("FNO_SESSION", "main");
    std::env::remove_var("FNO_SERVER");
    install_ancestry(&host_ancestry());
    assert!(is_pane_host("host-1"));
    install_ancestry(&nested_ancestry());
    assert!(!is_pane_host("nested-2"));
    crate::spawn_context::set_test_ancestry_table(None);
    clear_pane_env();
    let _ = std::fs::remove_dir_all(&rt);
}

/// mT6/mT7/mT8/mT11: every broken key degrades to emit (the v1 presence
/// gate) and never latches the host silent: no epoch, a non-numeric pane, an
/// empty pin from a half-failed create, a symlinked rendezvous dir. The
/// link target stays untouched (no hijack), and a traversal FNO_PANE never
/// escapes the dir.
#[test]
fn the_pin_degrades_to_emit_on_every_broken_key() {
    let _g = env_lock();
    let rt = runtime_dir("degrade");
    std::env::set_var("FNO_PANE", "8");
    std::env::set_var("FNO_PANE_EPOCH", "8000");
    std::env::set_var("FNO_SERVER", "main");
    std::env::set_var("XDG_RUNTIME_DIR", &rt);
    // No epoch.
    std::env::remove_var("FNO_PANE_EPOCH");
    assert!(is_pane_host("h"));
    set_pane_env("8", "8000", "main", &rt);
    // Non-numeric pane (a traversal shape).
    std::env::set_var("FNO_PANE", "../../pwn");
    assert!(is_pane_host("h"));
    // (The empty-pin degrade moved to the census-unavailable fallback test:
    // with a census present the verdict cache decides, so an empty v1 pin
    // no longer reaches the latch.)
    // Symlinked rendezvous dir: refused, degrade to emit, target untouched.
    let rt2 = runtime_dir("degrade2");
    std::fs::create_dir_all(rt2.join("hijack-target")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        rt2.join("hijack-target"),
        rt2.join(format!("fno-turn-pins-{}", unsafe { libc::geteuid() })),
    )
    .unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &rt2);
    assert!(is_pane_host("h"));
    assert!(rt2
        .join("hijack-target")
        .read_dir()
        .unwrap()
        .next()
        .is_none());
    clear_pane_env();
}
/// mT9: a path-traversal FNO_SESSION is sanitized so the pin (and the gate
/// record) never escapes the rendezvous dir.
#[test]
fn a_traversal_server_name_stays_in_the_pin_dir() {
    let _g = env_lock();
    let rt = runtime_dir("traversal");
    set_pane_env("9", "9000", "", &rt);
    // An empty census sends the gate to the v1 latch, whose sanitization
    // this test pins; the ancestry path needs a census to answer at all.
    crate::spawn_context::set_test_ancestry_table(Some(std::collections::BTreeMap::new()));
    std::env::remove_var("FNO_SERVER");
    std::env::set_var("FNO_SESSION", "../../escape");
    assert!(is_pane_host("h"));
    let escaped = std::fs::read_dir(&rt)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("escape-")
                || n.contains("-escape")
                || n.contains("fno-turn-pins-") && n.contains("..")
        });
    assert!(!escaped, "a pin escaped the rendezvous dir: {escaped}");
    crate::spawn_context::set_test_ancestry_table(None);
    clear_pane_env();
    let _ = std::fs::remove_dir_all(&rt);
}

/// The fix itself: a second harness typed into the same pane after the
/// first one exited is ALSO a host - the ancestry proof decides, not a pin
/// the first harness happened to win. A harness nested under the pane's
/// harness (a Bash tool running `claude -p` in the same pane) stays a guest.
#[test]
fn a_second_harness_in_the_same_pane_is_also_a_host() {
    let _g = env_lock();
    let rt = runtime_dir("second-harness");
    set_pane_env("3", "3000", "main", &rt);
    install_ancestry(&host_ancestry());
    assert!(is_pane_host("codex-sess"));
    assert!(is_pane_host("claude-sess"));
    install_ancestry(&nested_ancestry());
    assert!(!is_pane_host("nested-sess"));
    crate::spawn_context::set_test_ancestry_table(None);
    clear_pane_env();
    let _ = std::fs::remove_dir_all(&rt);
}

/// The session-keyed verdict cache: once written, the cache answers without
/// touching the census, in both directions.
#[test]
fn the_verdict_cache_answers_without_the_census() {
    let _g = env_lock();
    let rt = runtime_dir("verdict-cache");
    set_pane_env("4", "4000", "main", &rt);
    let dir = runtime_pin_dir().unwrap();
    // A cached guest wins over a host-shaped census.
    install_ancestry(&host_ancestry());
    std::fs::write(dir.join("main-4-4000-cached-a"), "guest\n").unwrap();
    assert!(!is_pane_host("cached-a"));
    // A cached host wins over the REAL census (no injection): the test
    // runner's ancestry proves no harness above it, so only the cache can
    // be answering host here.
    crate::spawn_context::set_test_ancestry_table(None);
    std::fs::write(dir.join("main-4-4000-cached-b"), "host\n").unwrap();
    assert!(is_pane_host("cached-b"));
    clear_pane_env();
    let _ = std::fs::remove_dir_all(&rt);
}

/// Census unavailable (an empty table): the ancestry answer would be a
/// guess, so the v1 first-writer latch decides, the empty-pin degrade
/// stands, and no verdict file is cached on this path.
#[test]
fn census_unavailable_falls_back_to_the_first_writer_latch() {
    let _g = env_lock();
    let rt = runtime_dir("no-census");
    set_pane_env("6", "6000", "main", &rt);
    crate::spawn_context::set_test_ancestry_table(Some(std::collections::BTreeMap::new()));
    // The v1 latch: first writer emits, a different session in the epoch
    // stays silent.
    assert!(is_pane_host("host-1"));
    assert!(!is_pane_host("nested-2"));
    // No verdict file is written on the fallback path.
    let dir = runtime_pin_dir().unwrap();
    assert!(!dir.join("main-6-6000-host-1").exists());
    // The v1 degrade stands: an empty pin from a half-failed create emits.
    std::fs::write(dir.join("main-6-6000"), "").unwrap();
    assert!(is_pane_host("h"));
    crate::spawn_context::set_test_ancestry_table(None);
    clear_pane_env();
    let _ = std::fs::remove_dir_all(&rt);
}

/// mT10: a malformed payload still emits through the full process() fire --
/// the marker lane is independent of the parse (presence-gate degrade),
/// only the report is skipped.
#[test]
fn a_malformed_payload_still_emits_markers() {
    let _g = env_lock();
    let rt = runtime_dir("malformed");
    let sink = rt.join("sink");
    set_pane_env("10", "10000", "main", &rt);
    std::env::set_var("FNO_TURN_MARKER_TTY", &sink);
    process("claude", "UserPromptSubmit", &json!({}));
    let out = std::fs::read(&sink).unwrap_or_default();
    assert!(out
        .windows(MARKER_C.len())
        .any(|w| w == MARKER_C.as_bytes()));
    clear_pane_env();
    let _ = std::fs::remove_dir_all(&rt);
}

// --- the stop gate's block report ---

/// A blocked stop reports working for the session and marks the gate record,
/// so the leg's first PreToolUse working fire collapses as the duplicate it
/// is. A footnote-harness session stays out of the row's way (the shim's one
/// policy line), and an empty id reports nothing.
#[test]
fn the_stop_gate_block_report_sends_working_and_marks_the_record() {
    let _g = env_lock();
    let rt = runtime_dir("stopgate-report");
    let home = rt.join("agents-home");
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &rt);
    // No daemon socket under this root: the send answers DaemonNotRunning,
    // which the fire-and-forget contract treats as delivered and marks.
    let saved_agents_home = std::env::var_os("FNO_AGENTS_HOME");
    std::env::set_var("FNO_AGENTS_HOME", &home);
    let saved_harness_name = std::env::var_os("FNO_HARNESS_NAME");
    std::env::remove_var("FNO_HARNESS_NAME");
    report_working("sess-blocked");
    let record =
        std::fs::read_to_string(runtime_pin_dir().unwrap().join("state-sess-blocked")).unwrap();
    assert!(record.starts_with("working "), "{record}");
    std::env::set_var("FNO_HARNESS_NAME", "footnote");
    report_working("sess-footnote");
    assert!(!runtime_pin_dir()
        .unwrap()
        .join("state-sess-footnote")
        .exists());
    report_working("");
    match saved_agents_home {
        Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
        None => std::env::remove_var("FNO_AGENTS_HOME"),
    }
    match saved_harness_name {
        Some(v) => std::env::set_var("FNO_HARNESS_NAME", v),
        None => std::env::remove_var("FNO_HARNESS_NAME"),
    }
    std::env::remove_var("XDG_RUNTIME_DIR");
    let _ = std::fs::remove_dir_all(&rt);
}

// --- the codex fixtures (the captured payload shapes) ---

fn fixture(name: &str) -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hook_payloads")
        .join(name);
    serde_json::from_str(
        &std::fs::read_to_string(path).unwrap_or_else(|e| panic!("fixture {name}: {e}")),
    )
    .unwrap()
}

/// Every fixture normalizes to a carry-the-session-id event; the goal-leg
/// PreToolUse (a goal continuation is a user-role row, so UserPromptSubmit
/// never fires) decides working like any other tool call.
#[test]
fn the_codex_fixtures_normalize_to_working_events() {
    let codex = job("codex");
    for (name, event, tool) in [
        ("codex-user-prompt-submit.json", "UserPromptSubmit", ""),
        ("codex-pre-tool-use.json", "PreToolUse", "Bash"),
        ("codex-goal-pre-tool-use.json", "PreToolUse", "Bash"),
    ] {
        let payload = fixture(name);
        let e = adapter::normalize("codex", event, &payload);
        assert_eq!(e.session_id, "0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3");
        assert_eq!(e.tool, tool, "{name}");
        assert_eq!(decide(&codex, &e).unwrap().state, "working", "{name}");
    }
}

/// AC9: the codex Stop fixture with a rollout carrying a turn_context row
/// reads the observed axes: state done, posture workspace-write:never, and
/// the payload-lacking model/effort filled from the rollout.
#[test]
fn the_codex_stop_fixture_reads_posture_from_the_rollout() {
    let rt = runtime_dir("rollout");
    let uuid = "0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3";
    let sessions = rt.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("rollout-2026-10-01T12-00-00-{uuid}.jsonl")),
        concat!(
            r#"{"type":"session_meta","payload":{"id":"0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"cwd":"/x","originator":"codex_cli_rs","model":"gpt-6-luna","effort":"high","summary":"r"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"cwd":"/x","originator":"codex_cli_rs","model":"gpt-6-luna","effort":"high","summary":"r","approval_policy":"never","sandbox_policy":{"mode":"workspace-write","writable_roots":["/x"]}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let stop = fixture("codex-stop.json");
    let e = crate::hook::adapter::codex::normalize_with_root("Stop", &stop, Some(&sessions));
    assert_eq!(e.session_id, uuid);
    assert_eq!(e.posture.as_deref(), Some("workspace-write:never"));
    assert_eq!(e.model, "gpt-6-luna");
    assert_eq!(e.effort, "high");
    // And the decision is done, as the map wired it.
    let d = decide(&job("codex"), &e).unwrap();
    assert_eq!(d.state, "done");
    let _ = std::fs::remove_dir_all(&rt);
}
