//! The transaction tests, ported from the Python suite that owned these
//! functions before the port (`cli/tests/agents/test_retask.py` and
//! `cli/tests/agents/test_thread_reference.py`). Python test names are kept
//! snake case so a reviewer can pair the suites.

use super::*;
use serde_json::json;
use std::collections::VecDeque;

/// The seam fake the Python tests built from lambdas.
struct Fake {
    frames: VecDeque<String>,
    sends: Vec<(String, bool)>,
    tiers: Vec<(String, String)>,
    restamp: Value,
    renamed: Option<String>,
    rename_calls: usize,
    ready: Option<Value>,
    ready_calls: usize,
    preflight: Option<Value>,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            frames: VecDeque::new(),
            sends: Vec::new(),
            tiers: Vec::new(),
            restamp: json!("new-session"),
            renamed: Some("target-x-bbbb".to_string()),
            rename_calls: 0,
            ready: Some(json!({"matched": true, "rule_id": "idle_prompt", "state": "idle"})),
            ready_calls: 0,
            preflight: None,
        }
    }
}

impl RetaskSeams for Fake {
    fn read_frame(&mut self) -> Result<String, TransportFailure> {
        Ok(self.frames.pop_front().expect("frame queued"))
    }
    fn settle(&mut self) -> Result<(), TransportFailure> {
        Ok(())
    }
    fn send(&mut self, text: &str, submit: bool) -> Result<bool, TransportFailure> {
        self.sends.push((text.to_string(), submit));
        Ok(true)
    }
    fn restamp(&mut self) -> Result<Value, TransportFailure> {
        Ok(self.restamp.clone())
    }
    fn rename(&mut self, _new_name: &str) -> Option<String> {
        self.rename_calls += 1;
        self.renamed.clone()
    }
    fn project_tier(&mut self, model: &str, effort: &str) -> Result<(), String> {
        self.tiers.push((model.to_string(), effort.to_string()));
        Ok(())
    }
    fn ready_frame(&mut self, _frame: &str) -> Option<Value> {
        self.ready_calls += 1;
        self.ready.clone()
    }
    fn source_preflight(&mut self) -> Option<Value> {
        self.preflight.clone()
    }
}

fn row() -> RetaskRow {
    RetaskRow {
        name: "bp-xbdb9-retask".to_string(),
        harness: "codex".to_string(),
        provider: None,
        model: Some("gpt-5.6-sol".to_string()),
        effort: Some("high".to_string()),
        substrate: Some("pane".to_string()),
        status_live: true,
        harness_session_id: Some("old-session".to_string()),
        launch_account: None,
        mux: Some(("main".to_string(), 12)),
        thread_id: None,
    }
}

fn codex_target() -> RetaskTarget {
    RetaskTarget {
        harness: "codex".to_string(),
        provider: None,
        model: Some("gpt-5.6-sol".to_string()),
        effort: Some("high".to_string()),
        substrate: None,
        permission_mode: None,
        route: None,
        account: None,
        verb: "target".to_string(),
    }
}

fn status_frame(model: &str, effort: &str) -> String {
    format!("Model: {model} (reasoning {effort}, summaries auto)")
}

const CODEX_PROMPT: &str = "› Ask Codex to do anything\n";
const TARGET_COMMAND: &str = "$fno:target --no-merge x-bbbb";

fn run(row: &RetaskRow, target: &RetaskTarget, seams: &mut Fake) -> Value {
    let live_mode = None;
    execute_retask(row, target, "x-bbbb", TARGET_COMMAND, seams, live_mode)
        .expect("no transport death in fake")
}

#[test]
fn payload_rows() {
    let receipt = detect_retask(&row(), &codex_target(), None);
    assert_eq!(receipt.outcome, "retask_ready");

    let target = RetaskTarget {
        model: Some("gpt-5.6-luna".to_string()),
        effort: Some("xhigh".to_string()),
        ..codex_target()
    };
    let receipt = detect_retask(&row(), &target, None);
    assert_eq!(receipt.outcome, "switch_pending");

    let bare_target = RetaskTarget {
        model: None,
        ..codex_target()
    };
    assert_eq!(
        detect_retask(&row(), &bare_target, None),
        DetectOutcome {
            outcome: "spawn_required",
            reason: Some("model_unrouted".into())
        }
    );

    let mut target = codex_target();
    target.provider = Some("codex".to_string());
    let mut openai_row = row();
    openai_row.provider = Some("openai".to_string());
    // The target vendor is unset until a route resolves one, so the row's
    // own provider axis is what the compare answers on.
    let neutral = RetaskTarget {
        provider: None,
        ..target
    };
    let receipt = detect_retask(&openai_row, &neutral, None);
    assert_eq!(receipt.outcome, "retask_ready");

    // One table test per the audit Keep rule: one row per axis value, not
    // one declaration per value.
    let cases: &[(&str, RetaskTarget, Option<&str>)] = &[
        (
            "harness",
            RetaskTarget {
                harness: "claude".to_string(),
                ..codex_target()
            },
            None,
        ),
        (
            "provider",
            RetaskTarget {
                provider: Some("zai".to_string()),
                ..codex_target()
            },
            None,
        ),
        (
            "substrate",
            RetaskTarget {
                substrate: Some("bg".to_string()),
                ..codex_target()
            },
            None,
        ),
        (
            "permission_mode",
            RetaskTarget {
                permission_mode: Some("yolo".to_string()),
                ..codex_target()
            },
            Some("bypassPermissions"),
        ),
        (
            "permission_mode_unobserved",
            RetaskTarget {
                permission_mode: Some("bypassPermissions".to_string()),
                ..codex_target()
            },
            None,
        ),
        (
            "account",
            RetaskTarget {
                account: Some("work".to_string()),
                ..codex_target()
            },
            None,
        ),
    ];
    for (axis, target, observed) in cases {
        assert_eq!(
            detect_retask(&row(), target, *observed),
            DetectOutcome {
                outcome: "spawn_required",
                reason: Some((*axis).to_string()),
            },
            "axis {axis}"
        );
    }

    let target = RetaskTarget {
        harness: "claude".to_string(),
        provider: None,
        permission_mode: Some("bypassPermissions".to_string()),
        account: Some("zai".to_string()),
        ..codex_target()
    };
    let thread_row = RetaskRow {
        harness: "claude".to_string(),
        substrate: Some("thread".to_string()),
        mux: None,
        thread_id: Some("F".to_string()),
        provider: Some("anthropic".to_string()),
        launch_account: Some("zai".to_string()),
        ..row()
    };
    let receipt = detect_retask(&thread_row, &target, Some("bypassPermissions"));
    assert_eq!(receipt.outcome, "retask_ready");
}

#[test]
fn refuse_rows() {
    let no_mux = RetaskRow { mux: None, ..row() };
    let receipt = detect_retask(&no_mux, &codex_target(), None);
    assert_eq!(
        receipt,
        DetectOutcome {
            outcome: "refused",
            reason: Some("worker_has_no_mux_ref".into())
        }
    );

    let stopped = RetaskRow {
        status_live: false,
        ..row()
    };
    assert_eq!(
        detect_retask(&stopped, &codex_target(), None),
        DetectOutcome {
            outcome: "refused",
            reason: Some("worker_not_live".into())
        }
    );

    let no_session = RetaskRow {
        harness_session_id: None,
        ..row()
    };
    assert_eq!(
        detect_retask(&no_session, &codex_target(), None),
        DetectOutcome {
            outcome: "refused",
            reason: Some("worker_has_no_session_id".into())
        }
    );

    let target = RetaskTarget {
        substrate: Some("thread".to_string()),
        ..codex_target()
    };
    let thread_row = RetaskRow {
        substrate: Some("thread".to_string()),
        mux: None,
        thread_id: Some("thread-session".to_string()),
        ..row()
    };
    assert_eq!(
        detect_retask(&thread_row, &target, None).outcome,
        "retask_ready"
    );

    let target = RetaskTarget {
        substrate: Some("thread".to_string()),
        ..codex_target()
    };
    let thread_row = RetaskRow {
        substrate: Some("thread".to_string()),
        mux: None,
        thread_id: None,
        ..row()
    };
    assert_eq!(
        detect_retask(&thread_row, &target, None),
        DetectOutcome {
            outcome: "refused",
            reason: Some("worker_has_no_thread_ref".into())
        }
    );

    let target = RetaskTarget {
        substrate: Some("thread".to_string()),
        ..codex_target()
    };
    let thread_row = RetaskRow {
        substrate: Some("thread".to_string()),
        mux: Some(("main".to_string(), 0)),
        ..row()
    };
    assert_eq!(
        detect_retask(&thread_row, &target, None),
        DetectOutcome {
            outcome: "refused",
            reason: Some("worker_has_no_thread_ref".into())
        }
    );
}

#[test]
fn exec_rows() {
    let mut seams = Fake {
        frames: VecDeque::from(vec![
            CODEX_PROMPT.to_string(),
            status_frame("gpt-5.6-sol", "high"),
        ]),
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(
        receipt,
        json!({
            "status": "retasked",
            "cleared": true,
            "session_restamped": true,
            "switch": "skipped_same_tier",
            "switch_verified": true,
            "target_submit_confirmed": true,
            "registry_name": "target-x-bbbb",
            "source_session_id": "old-session",
            "current_session_id": "new-session",
            "transition": "succession",
            "registry_rows": 1,
            "lineage_recorded": true,
        })
    );
    let texts: Vec<&str> = seams.sends.iter().map(|(text, _)| text.as_str()).collect();
    assert_eq!(texts, ["/clear", "/status", TARGET_COMMAND]);
    assert_eq!(
        seams.tiers,
        [("gpt-5.6-sol".to_string(), "high".to_string())]
    );

    let mut seams = Fake {
        preflight: Some(json!({
            "status": "refused",
            "reason": "source_pr_not_green",
            "pr": 1168,
            "head": "source-head",
            "verdict": "red",
            "blockers": {"failing": 4},
        })),
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("source_pr_not_green"));
    assert_eq!(receipt["pr"], json!(1168));
    assert!(seams.sends.is_empty());

    let mut seams = Fake {
        frames: VecDeque::from(vec![
            CODEX_PROMPT.to_string(),
            status_frame("gpt-5.6-sol", "high"),
        ]),
        restamp: json!({
            "classification": "succession",
            "predecessor_session_id": "old-session",
            "current_session_id": "new-session",
            "registry_rows": 1,
            "lineage_recorded": true,
        }),
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["transition"], json!("succession"));
    assert_eq!(receipt["source_session_id"], json!("old-session"));
    assert_eq!(receipt["current_session_id"], json!("new-session"));
    assert_eq!(receipt["registry_rows"], json!(1));
    assert_eq!(receipt["lineage_recorded"], json!(true));
    assert_eq!(receipt["target_submit_confirmed"], json!(true));

    let restamp = json!({
        "classification": "branch",
        "reason": "session_transition_not_succession",
        "predecessor_session_id": "old-session",
        "current_session_id": "new-session",
    });
    let mut seams = Fake {
        frames: VecDeque::from([CODEX_PROMPT.to_string()]),
        restamp,
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["cleared"], json!(true));
    assert_eq!(receipt["target_submit_confirmed"], json!(false));
    assert_eq!(seams.sends, [("/clear".to_string(), true)]);
    assert_eq!(
        seams.rename_calls, 0,
        "rename must wait for succession proof"
    );

    let restamp = json!({
        "classification": "succession",
        "predecessor_session_id": "other-session",
        "current_session_id": "new-session",
        "registry_rows": 1,
        "lineage_recorded": true,
    });
    let mut seams = Fake {
        frames: VecDeque::from([CODEX_PROMPT.to_string()]),
        restamp,
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["cleared"], json!(true));
    assert_eq!(seams.sends, [("/clear".to_string(), true)]);

    let restamp = json!({
        "classification": "succession",
        "predecessor_session_id": "old-session",
        "current_session_id": "new-session",
        "registry_rows": 2,
        "lineage_recorded": true,
    });
    let mut seams = Fake {
        frames: VecDeque::from([CODEX_PROMPT.to_string()]),
        restamp,
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("successor_row_count_invalid"));
    assert_eq!(seams.sends, [("/clear".to_string(), true)]);

    let mut seams = Fake {
        frames: VecDeque::from(["painted but busy".to_string()]),
        ready: Some(json!({"matched": true, "rule_id": "working", "state": "working"})),
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["reason"], json!("pane_not_idle"));
    assert_eq!(receipt["cleared"], json!(false));
    assert_eq!(receipt["target_submit_confirmed"], json!(false));

    let mut seams = Fake {
        frames: VecDeque::from(["painted but no known manifest rule".to_string()]),
        ready: Some(json!({"matched": false})),
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["reason"], json!("pane_state_unobserved"));
    assert_eq!(receipt["cleared"], json!(false));
    assert!(seams.sends.is_empty());

    let mut seams = Fake {
        frames: VecDeque::from(["readable pane frame".to_string()]),
        ready: None,
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["reason"], json!("pane_state_unobserved"));
    assert_eq!(receipt["cleared"], json!(false));

    let mut seams = Fake {
        frames: VecDeque::from([String::new()]),
        ..Fake::default()
    };
    let receipt = run(&row(), &codex_target(), &mut seams);

    assert_eq!(receipt["reason"], json!("pane_frame_unreadable"));
    assert_eq!(receipt["cleared"], json!(false));
    assert_eq!(seams.ready_calls, 0, "empty frame must not be evaluated");
}

#[test]
fn menu_rows() {
    let target = RetaskTarget {
        model: Some("gpt-5.6-luna".to_string()),
        effort: Some("xhigh".to_string()),
        ..codex_target()
    };
    let mut seams = Fake {
        frames: VecDeque::from(vec![
            CODEX_PROMPT.to_string(),
            status_frame("gpt-5.6-sol", "high"),
            "Select Model and Effort\n› 1. gpt-5.6-sol (current)\n  3. gpt-5.6-luna\n".to_string(),
            "Select Model and Effort\n  1. gpt-5.6-sol\n› 3. gpt-5.6-luna (current)\n".to_string(),
            "Select Reasoning Level for gpt-5.6-luna\n› 2. Medium (default)\n  4. Extra high\n"
                .to_string(),
            "Select Reasoning Level for gpt-5.6-luna\n  2. Medium (default)\n› 4. Extra high\n"
                .to_string(),
            status_frame("gpt-5.6-luna", "xhigh"),
        ]),
        ..Fake::default()
    };
    let receipt = run(&row(), &target, &mut seams);

    assert_eq!(receipt["status"], json!("retasked"));
    assert_eq!(receipt["switch"], json!("switched"));
    assert_eq!(receipt["switch_verified"], json!(true));
    assert_eq!(receipt["target_submit_confirmed"], json!(true));
    assert_eq!(seams.sends[0], ("/clear".to_string(), true));
    assert_eq!(
        seams.sends.last(),
        Some(&(TARGET_COMMAND.to_string(), true))
    );
    assert!(seams.sends.contains(&("/model".to_string(), true)));
    assert!(seams.sends.contains(&(String::new(), true)));
    assert!(seams
        .sends
        .iter()
        .any(|(text, submit)| text == "\x1b[B" && !submit));
    assert_eq!(
        seams.tiers,
        [
            ("gpt-5.6-sol".to_string(), "high".to_string()),
            ("gpt-5.6-luna".to_string(), "xhigh".to_string()),
        ]
    );

    let target = RetaskTarget {
        harness: "claude".to_string(),
        model: Some("new-model".to_string()),
        effort: Some("xhigh".to_string()),
        ..codex_target()
    };
    let claude_row = RetaskRow {
        harness: "claude".to_string(),
        model: Some("old-model".to_string()),
        ..row()
    };
    let mut seams = Fake {
        frames: VecDeque::from(vec![
            "ready".to_string(),
            status_frame("old-model", "high"),
            status_frame("new-model", "xhigh"),
        ]),
        ready: Some(json!({"matched": true, "rule_id": "live_prompt_box", "state": "idle"})),
        ..Fake::default()
    };
    let receipt = run(&claude_row, &target, &mut seams);

    assert_eq!(receipt["status"], json!("retasked"));
    assert!(seams
        .sends
        .contains(&("/model new-model".to_string(), true)));
    assert!(seams.sends.contains(&("/effort xhigh".to_string(), true)));
    let model_sends: Vec<&(String, bool)> = seams
        .sends
        .iter()
        .filter(|(text, _)| text.starts_with("/model"))
        .collect();
    assert_eq!(model_sends.len(), 1);

    let bare_target = RetaskTarget {
        model: None,
        effort: None,
        ..codex_target()
    };
    let bare_row = RetaskRow {
        model: None,
        effort: None,
        ..row()
    };
    let mut seams = Fake {
        frames: VecDeque::from(vec![
            CODEX_PROMPT.to_string(),
            status_frame("gpt-5.6-sol", "high"),
        ]),
        ..Fake::default()
    };
    let receipt = run(&bare_row, &bare_target, &mut seams);

    assert_eq!(receipt["status"], json!("retasked"));
    assert_eq!(receipt["switch"], json!("skipped_same_tier"));

    let bare_target = RetaskTarget {
        model: None,
        ..codex_target()
    };
    let mut seams = Fake::default();
    let receipt = run(&row(), &bare_target, &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("model_unrouted"));
    assert_eq!(receipt["cleared"], json!(false));
    assert!(seams.sends.is_empty());

    let frame = "› 1. gpt-5.6-sol-mini\n  3. gpt-5.6-sol\n";
    assert_eq!(menu_delta(frame, "gpt-5.6-sol"), Some(2));
    assert_eq!(menu_delta(frame, "sol"), Some(2));
    assert_eq!(menu_delta(frame, "luna"), None);

    let target = RetaskTarget {
        model: Some("gpt-5.6-luna".to_string()),
        effort: Some("xhigh".to_string()),
        ..codex_target()
    };
    let mut seams = Fake {
        frames: VecDeque::from(vec![
            CODEX_PROMPT.to_string(),
            status_frame("gpt-5.6-sol", "high"),
            "Select Model and Effort\n› 1. gpt-5.6-sol (current)\n".to_string(),
        ]),
        ..Fake::default()
    };
    let receipt = run(&row(), &target, &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("model_row_missing"));
    assert_eq!(receipt["target_submit_confirmed"], json!(false));

    let target = RetaskTarget {
        harness: "gemini".to_string(),
        model: None,
        effort: None,
        ..codex_target()
    };
    let gemini_row = RetaskRow {
        harness: "gemini".to_string(),
        model: None,
        effort: None,
        ..row()
    };
    let mut seams = Fake {
        frames: VecDeque::from(["ready".to_string()]),
        ..Fake::default()
    };
    let receipt = run(&gemini_row, &target, &mut seams);

    assert_eq!(receipt["reason"], json!("unsupported_switch_strategy"));
    assert_eq!(receipt["cleared"], json!(false));
}

#[test]
fn transition_receipt_bare_successor_string_is_a_minimal_succession() {
    let receipt = transition_receipt(&json!("new-session"), "old-session").unwrap();
    assert_eq!(
        receipt,
        json!({
            "classification": "succession",
            "predecessor_session_id": "old-session",
            "current_session_id": "new-session",
            "registry_rows": 1,
            "lineage_recorded": true,
        })
    );
    assert!(transition_receipt(&json!("old-session"), "old-session").is_none());
    assert!(transition_receipt(&json!(7), "old-session").is_none());
}
