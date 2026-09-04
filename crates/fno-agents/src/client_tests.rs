//! The fno-agents client binary's test module. Lives outside client.rs
//! because the file-budget gate makes that file shrink-only.

use super::*;
use fno_agents::{emit_schema_json, state::AgentState, AgentStatus, KNOWN_EVENT_KINDS};
use std::path::Path;

// -----------------------------------------------------------------------
// ab-351427cb: --help parity (top-level verb list + per-verb usage)
// -----------------------------------------------------------------------

#[test]
fn print_help_lists_every_routable_verb() {
    // Mirror of RUST_CLIENT_VERBS (cli/src/fno/agents/rust_runtime.py).
    // The Python parity test guards client.rs<->router; this guards the
    // `--help` display list so a routable verb can't be missing from it.
    let expected = [
        "spawn",
        "ask",
        "list",
        "status",
        "restart",
        "reap",
        "rename",
        "stop",
        "rm",
        "reconcile",
        "drive-authority",
        "trace",
        "ping",
        "resume",
        "adopt",
        "attach",
        "logs",
        "loop-check",
        "loop",
        "finalize",
        "report",
        "wait",
        "subscribe",
        "digest",
        "needs",
    ];
    let listed: std::collections::HashSet<&str> = CLIENT_VERB_USAGE
        .iter()
        .map(|u| u.split_whitespace().next().expect("usage has a verb token"))
        .collect();
    for verb in expected {
        assert!(
            listed.contains(verb),
            "verb {verb} missing from --help (CLIENT_VERB_USAGE)"
        );
    }
    assert_eq!(
        listed.len(),
        expected.len(),
        "CLIENT_VERB_USAGE has an extra or duplicate verb vs RUST_CLIENT_VERBS"
    );
}

#[test]
fn verb_usage_resolves_known_and_rejects_unknown() {
    // Verbs that ab-351427cb added must each resolve a usage line (host/
    // promote retired at G4).
    for verb in [
        "trace",
        "ping",
        "resume",
        "adopt",
        "attach",
        "logs",
        "drive-authority",
        "loop",
    ] {
        assert!(
            verb_usage(verb).is_some(),
            "verb_usage({verb}) should resolve"
        );
    }
    // `loop` and `loop-check` are distinct leading tokens (no prefix collision).
    assert!(verb_usage("loop").unwrap().starts_with("loop run"));
    assert!(verb_usage("loop-check").unwrap().starts_with("loop-check"));
    assert!(verb_usage("definitely-not-a-verb").is_none());
}

#[test]
fn rm_usage_names_the_claude_cascade_and_worktree() {
    let usage = verb_usage("rm").expect("rm usage line");
    assert!(
        usage.contains("claude rm"),
        "rm help must name the cascade verb"
    );
    assert!(usage.contains("short_id"), "rm help must name the join key");
    assert!(
        usage.to_ascii_lowercase().contains("worktree"),
        "rm help must say a removal can remove a worktree"
    );
}

#[test]
fn rm_runs_the_same_daemon_drift_probe_as_list() {
    assert!(warns_on_daemon_drift("list"));
    assert!(warns_on_daemon_drift("rm"));
    assert!(!warns_on_daemon_drift("spawn"));
}

#[test]
fn rm_preserves_internal_audit_context_flags() {
    let (method, params) = build_request(
        "rm",
        &[
            "worker".into(),
            "--audit-actor".into(),
            "post-merge".into(),
            "--audit-reason".into(),
            "pr-merged".into(),
            "--audit-request-id".into(),
            "merge-cleanup-1".into(),
            "--audit-worktree-touched".into(),
            "--audit-reclaimed-bytes".into(),
            "42".into(),
        ],
    )
    .expect("audit flags parse");
    assert_eq!(method, "agent.rm");
    assert_eq!(params["name"], "worker");
    assert_eq!(params["audit_actor"], "post-merge");
    assert_eq!(params["audit_reason"], "pr-merged");
    assert_eq!(params["audit_request_id"], "merge-cleanup-1");
    assert_eq!(params["audit_worktree_touched"], true);
    assert_eq!(params["audit_reclaimed_bytes"], 42);
}

// -----------------------------------------------------------------------
// x-9112: bg/headless provider inference parity with Python
// -----------------------------------------------------------------------

fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(m, _)| *m == k)
            .map(|(_, v)| v.to_string())
    }
}

#[test]
fn maybe_run_spawn_infers_provider_from_single_marker() {
    assert_eq!(
        infer_dispatch_provider(env_of(&[("CLAUDE_CODE_SESSION_ID", "abc")])),
        "claude"
    );
    assert_eq!(
        infer_dispatch_provider(env_of(&[("CODEX_SESSION_ID", "abc")])),
        "codex"
    );
    assert_eq!(
        infer_dispatch_provider(env_of(&[("GEMINI_SESSION_ID", "abc")])),
        "gemini"
    );
    assert_eq!(
        infer_dispatch_provider(env_of(&[("OPENCODE_SESSION_ID", "ses_abc")])),
        "opencode"
    );
}

#[test]
fn maybe_run_spawn_infers_provider_defaults_claude_when_ambiguous() {
    // Zero markers -> builtin default.
    assert_eq!(infer_dispatch_provider(env_of(&[])), "claude");
    // Whitespace-only marker is treated as absent.
    assert_eq!(
        infer_dispatch_provider(env_of(&[("CODEX_SESSION_ID", "   ")])),
        "claude"
    );
    // Markers naming DIFFERENT harnesses are ambiguous -> builtin default.
    assert_eq!(
        infer_dispatch_provider(env_of(&[
            ("CODEX_SESSION_ID", "x"),
            ("GEMINI_SESSION_ID", "y"),
        ])),
        "claude"
    );
    // Two markers for the SAME harness agree (Codex thread id + legacy
    // session id) -> resolves that harness, not ambiguous. Mirrors Python.
    assert_eq!(
        infer_dispatch_provider(env_of(&[
            ("CODEX_THREAD_ID", "t"),
            ("CODEX_SESSION_ID", "s"),
        ])),
        "codex"
    );
}

#[test]
fn infer_dispatch_provider_uses_canonical_spawn_stamp() {
    assert_eq!(
        infer_dispatch_provider(env_of(&[("FNO_HARNESS_NAME", "codex")])),
        "codex"
    );
    assert_eq!(
        infer_dispatch_provider(env_of(&[
            ("FNO_HARNESS_NAME", "claude"),
            ("FNO_HARNESS_SESSION_ID", "claude-session"),
            ("CODEX_THREAD_ID", "codex-session"),
        ])),
        "claude"
    );
}

#[test]
fn harness_marker_table_is_expected() {
    // Guards Rust-internal edits to HARNESS_MARKERS (ordering is load-bearing:
    // it is the priority list). Cross-language parity with Python is enforced
    // by the pytest test_harness_markers_match_client_rs, which reads this
    // const from source rather than a hard-coded mirror.
    assert_eq!(
        HARNESS_MARKERS,
        &[
            ("CODEX_THREAD_ID", "codex"),
            ("CLAUDE_CODE_SESSION_ID", "claude"),
            ("CODEX_SESSION_ID", "codex"),
            ("GEMINI_SESSION_ID", "gemini"),
            ("OPENCODE_SESSION_ID", "opencode"),
        ]
    );
}

#[test]
fn help_request_respects_argv_boundary() {
    // ab-351427cb review (gemini HIGH / codex P2): a `--help` in the verb's
    // own options is a help request; a `--help` after an `--argv`/`--`
    // boundary belongs to the spawned command and must NOT be captured.
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();

    // Verb's own --help / -h -> help request.
    assert!(is_help_request(&s(&["wk", "--help"])));
    assert!(is_help_request(&s(&["--help"])));
    assert!(is_help_request(&s(&["-h"])));

    // --help inside a spawn/host argv payload -> NOT a help request.
    assert!(!is_help_request(&s(&[
        "wk",
        "--harness",
        "codex",
        "--argv",
        "--",
        "tool",
        "--help"
    ])));
    assert!(!is_help_request(&s(&["wk", "--argv", "tool", "--help"])));

    // --help after a bare `--` end-of-options separator -> NOT a help request.
    assert!(!is_help_request(&s(&["wk", "--", "--help"])));

    // No help flag at all.
    assert!(!is_help_request(&s(&["wk", "--harness", "codex"])));
}

// -----------------------------------------------------------------------
// W7: --emit-schema unit tests (struct-drift guard + JSON parse check)
// -----------------------------------------------------------------------

/// AC2-HP: emit_schema_json() must produce valid JSON containing the
/// required top-level keys (envelope, status, event_kinds).
#[test]
fn emit_schema_json_has_required_keys() {
    let schema = emit_schema_json();
    assert!(schema.get("envelope").is_some(), "missing 'envelope' key");
    assert!(schema.get("status").is_some(), "missing 'status' key");
    assert!(
        schema.get("event_kinds").is_some(),
        "missing 'event_kinds' key"
    );
}

/// AC2-HP: The emitted schema must serialize to valid JSON (round-trip check).
#[test]
fn emit_schema_round_trips_as_json() {
    let schema = emit_schema_json();
    let s = serde_json::to_string(&schema).expect("schema must serialize");
    let back: serde_json::Value = serde_json::from_str(&s).expect("re-parse must succeed");
    assert_eq!(schema, back);
}

/// Bidirectional struct-drift guard for AgentState + PtyStateWire.
///
/// Direction 1 (schema ⊆ struct): every property key in the emitted
/// status schema must exist as a serialized AgentState field. A property
/// added to emit_schema_json() without a corresponding struct field is
/// caught here.
///
/// Direction 2 (struct ⊆ schema): every serialized AgentState field must
/// appear in the emitted status schema properties. A new AgentState field
/// forgotten in emit_schema_json() is caught here.
///
/// The same bidirectional check is applied to the pty sub-object vs the
/// on-disk PtyStateWire flat fields (active, drive_active, drive_session_id,
/// drive_mode, last_heartbeat_at_monotonic_ns).
#[test]
fn emit_schema_status_properties_match_agent_state_fields() {
    use fno_agents::state::PtyState;

    // --- AgentState (pty: None) ---
    let sample = AgentState {
        schema_version: 1,
        short_id: "wkA".into(),
        status: AgentStatus::Ready,
        ready: true,
        last_message_at: Some("2026-01-01T00:00:00Z".into()),
        last_reply: Some("hello".into()),
        restart_count: 2,
        last_restart_at: Some("2026-01-01T00:00:01Z".into()),
        pty: None,
    };
    let serialized = serde_json::to_value(&sample).expect("AgentState must serialize");
    let struct_keys: std::collections::HashSet<String> = serialized
        .as_object()
        .expect("must be object")
        .keys()
        .cloned()
        .collect();

    let schema = emit_schema_json();
    let schema_props = schema["status"]["properties"]
        .as_object()
        .expect("status.properties must be object");
    let schema_keys: std::collections::HashSet<String> = schema_props.keys().cloned().collect();

    // Direction 1: schema_props ⊆ struct_keys
    for key in &schema_keys {
        assert!(
                struct_keys.contains(key.as_str()),
                "emitted status schema has property {key:?} not in serialized AgentState: {struct_keys:?}"
            );
    }

    // Direction 2: struct_keys ⊆ schema_props
    for key in &struct_keys {
        assert!(
            schema_keys.contains(key.as_str()),
            "AgentState field {key:?} not in emitted status schema properties: {schema_keys:?}"
        );
    }

    // --- PtyState / PtyStateWire bidirectional check ---
    // Serialize a PtyState WITH drive active so all optional wire fields
    // (drive_session_id, drive_mode, last_heartbeat_at_monotonic_ns) are
    // present in the output. Using default() (no drive) omits them via
    // skip_serializing_if, which would make Direction 1 trivially pass
    // while hiding that the schema has properties absent from the struct.
    use fno_agents::state::DriveWindow;
    let pty_sample = PtyState {
        active: true,
        drive: Some(DriveWindow {
            session_id: Some("sess-1".into()),
            mode: Some("interactive".into()),
            last_heartbeat_at_monotonic_ns: Some(123_456_789),
        }),
    };
    let pty_json = serde_json::to_value(&pty_sample).expect("PtyState must serialize");
    let pty_struct_keys: std::collections::HashSet<String> = pty_json
        .as_object()
        .expect("pty must be object")
        .keys()
        .cloned()
        .collect();

    // The emitted pty schema is the second branch of the oneOf (type: object).
    let pty_schema_obj = schema["status"]["properties"]["pty"]["oneOf"]
        .as_array()
        .expect("pty oneOf must be array")
        .iter()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("object"))
        .expect("pty oneOf must have an object branch");
    let pty_schema_props = pty_schema_obj["properties"]
        .as_object()
        .expect("pty object branch must have properties");
    let pty_schema_keys: std::collections::HashSet<String> =
        pty_schema_props.keys().cloned().collect();

    // Direction 1: pty schema_props ⊆ pty wire keys
    for key in &pty_schema_keys {
        assert!(
                pty_struct_keys.contains(key.as_str()),
                "emitted pty schema has property {key:?} not in serialized PtyState wire: {pty_struct_keys:?}"
            );
    }

    // Direction 2: pty wire keys ⊆ pty schema_props
    for key in &pty_struct_keys {
        assert!(
            pty_schema_keys.contains(key.as_str()),
            "PtyState wire field {key:?} not in emitted pty schema properties: {pty_schema_keys:?}"
        );
    }
}

/// AC2-HP: KNOWN_EVENT_KINDS must be non-empty and contain the canonical kinds.
#[test]
fn known_event_kinds_are_non_empty_and_contain_canonical() {
    assert!(!KNOWN_EVENT_KINDS.is_empty());
    assert!(KNOWN_EVENT_KINDS.contains(&"agent_spawned"));
    assert!(KNOWN_EVENT_KINDS.contains(&"daemon_started"));
    assert!(KNOWN_EVENT_KINDS.contains(&"event_payload_too_large"));
}

// -----------------------------------------------------------------------
// Task 2.1: format_success per-verb output (stop/rm stdout parity)
// -----------------------------------------------------------------------

/// A codex thread stop names the interrupt outcome; `no-turn` stays silent.
#[test]
fn format_success_stop_names_the_interrupt_outcome() {
    let result = json!({"stopped": true, "backend": "codex-thread", "interrupt": "interrupted"});
    let out = format_success("stop", "t", &result, false, true, false).expect("stop line");
    assert_eq!(out, "stopped: t (turn interrupted)");
    let no_turn = json!({"stopped": true, "backend": "codex-thread", "interrupt": "no-turn"});
    let out = format_success("stop", "t", &no_turn, false, true, false).expect("stop line");
    assert_eq!(out, "stopped: t");
}

/// (x-6678) A refused stop never prints the word "stopped". The daemon
/// answers `stopped: false` over a turn its interrupt never settled, and
/// the old formatter read only `interrupt`, so it printed
/// "stopped: t (turn timeout-turn-still-running)" over a live worker.
#[test]
fn format_success_stop_refused_never_claims_a_stop() {
    let result = json!({
        "stopped": false,
        "backend": "codex-thread",
        "interrupt": "timeout-turn-still-running",
    });
    let out = format_success("stop", "t", &result, false, true, false).expect("stop line");
    assert!(
        !out.contains("stopped"),
        "a refused stop must not report one: {out}"
    );
    assert_eq!(
        out,
        "stop refused: t is still running (timeout-turn-still-running)"
    );
}

/// AC1-HP: stop with short_id in result -> "stopped: <name> (<short_id>)"
#[test]
fn format_success_stop_with_short_id() {
    let result = json!({"stopped": true, "short_id": "fo-1a2b"});
    let out = format_success("stop", "foo", &result, false, true, false);
    assert_eq!(out, Some("stopped: foo (fo-1a2b)".to_string()));
}

/// AC1-HP: stop fallback when short_id absent -> "stopped: <name>"
#[test]
fn format_success_stop_without_short_id() {
    let result = json!({"stopped": true});
    let out = format_success("stop", "foo", &result, false, true, false);
    assert_eq!(out, Some("stopped: foo".to_string()));
}

/// A verified Claude cascade names both surfaces in the receipt.
#[test]
fn format_success_rm() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "claude",
        "harness_removed": true,
        "was_orphaned": false
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false);
    assert_eq!(out, Some("removed: bar-agent (fno + claude)".to_string()));
}

#[test]
fn format_success_rm_warns_when_a_worktree_was_removed() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "claude",
        "harness_removed": true,
        "worktree_touched": true,
        "worktree_outcome": "removed",
        "reclaimed_bytes": 4096
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false)
        .expect("rm renders a receipt");
    assert!(
        out.contains("WARNING"),
        "worktree deletion must be visible: {out}"
    );
    assert!(out.to_ascii_lowercase().contains("worktree"), "{out}");
}

/// A reap that really removed a harness row names the verb that puts it
/// back. Asserted on the literal `fno agents adopt` string, not merely on
/// the receipt being longer: an absence has two explanations.
/// A codex row carries no short_id, so `harness_row_id` degrades to the
/// first eight chars of a time-prefixed id -- which collides across
/// same-window sessions. The hint must name the full id instead, or it
/// points an operator at a sibling session.
#[test]
fn format_success_rm_adopt_hint_prefers_the_full_session_id() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "codex",
        "harness_row_id": "01a02125",
        "harness_session_id": "01a02125-4eb4-7bf1-b74e-d238887eb092",
        "harness_removed": true
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false)
        .expect("rm renders a receipt");
    assert!(
        out.contains("fno agents adopt 01a02125-4eb4-7bf1-b74e-d238887eb092 --cross-project"),
        "{out}"
    );
    assert!(!out.contains("adopt 01a02125\n"), "{out}");
}

#[test]
fn format_success_rm_names_the_adopt_reversal() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "claude",
        "harness_row_id": "0a6e775f",
        "harness_removed": true
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false)
        .expect("rm renders a receipt");
    assert!(
        out.starts_with("removed: bar-agent (fno + claude)"),
        "{out}"
    );
    assert!(
        out.contains("fno agents adopt 0a6e775f --cross-project"),
        "{out}"
    );
    assert!(out.contains("resume handle"), "{out}");
}

#[test]
fn resume_and_adopt_usage_advertise_recovery_flags() {
    let resume = verb_usage("resume").expect("resume usage");
    assert!(resume.contains("--cross-project"), "{resume}");
    assert!(resume.contains("--cwd <existing-checkout>"), "{resume}");
    // The seam appends --account on a wake; the usage line naming it is
    // the parse contract that keeps the wake ladder honest.
    assert!(resume.contains("--account <id>"), "{resume}");

    let adopt = verb_usage("adopt").expect("adopt usage");
    assert!(adopt.contains("--cross-project"), "{adopt}");
}

/// No row id means no handle to name, so the receipt stays exactly as it
/// was rather than printing `fno agents adopt unknown`.
#[test]
fn format_success_rm_omits_the_hint_without_a_row_id() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "claude",
        "harness_removed": true
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false);
    assert_eq!(out, Some("removed: bar-agent (fno + claude)".to_string()));
}

#[test]
fn format_success_rm_never_claims_an_unread_harness_is_gone() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "claude",
        "harness_removed": null,
        "harness_reason": "claude list unreadable"
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false);
    assert_eq!(
        out,
        Some(
            "removed: bar-agent (fno only; claude list unreadable, harness side unverified)"
                .to_string()
        )
    );
}

#[test]
fn format_success_rm_names_a_forced_mux_orphan() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "claude",
        "harness_removed": true,
        "pane_session": "main",
        "pane_id": 24,
        "pane_removed": false,
        "pane_reason": "permission denied"
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false);
    assert_eq!(
        out,
        Some(
            "removed: bar-agent (fno + claude; mux pane main:24 survives: permission denied)"
                .to_string()
        )
    );
}

#[test]
fn format_success_rm_names_an_event_write_failure() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "claude",
        "harness_removed": true,
        "event_written": false,
        "event_reason": "disk full"
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false);
    assert_eq!(
        out,
        Some("removed: bar-agent (fno + claude; event record not written: disk full)".to_string())
    );
}

#[test]
fn format_success_rm_names_a_forced_codex_survivor() {
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "harness": "codex",
        "harness_row_id": "session-1",
        "harness_removed": false,
        "harness_reason": "index is read-only"
    });
    let out = format_success("rm", "bar-agent", &result, false, true, false);
    assert_eq!(
        out,
        Some(
            "removed: bar-agent (fno only; codex row session-1 survives: index is read-only)"
                .to_string()
        )
    );
}

/// AC2-HP: unknown verb returns None (falls back to pretty-print).
/// `spawn` is NOT unknown post-x-3ab8 (it renders a receipt, covered by
/// `format_success_spawn_emits_compact_receipt`); use a truly unhandled verb.
#[test]
fn format_success_unknown_verb_returns_none() {
    let result = json!({"spawned": true});
    assert_eq!(
        format_success("bogus-verb", "worker", &result, false, true, false),
        None
    );
    // list and reconcile now have their own rendering (not None)
    assert_eq!(
        format_success("status", "worker", &result, false, true, false),
        None
    );
}

// ab-1891cdff: `restart` outcome rendering (AC2-HP / AC2-EDGE / AC2-FR)
// -----------------------------------------------------------------------

#[test]
fn render_restart_reports_old_to_new() {
    // AC2-HP: a swap reports `restarted: pid OLD -> NEW` on stdout, exit 0.
    let (out, err, code) = render_restart(&Ok(RestartOutcome {
        old_pid: Some(91627),
        new_pid: 91999,
        forced: false,
        note: None,
    }));
    assert_eq!(out.as_deref(), Some("restarted: pid 91627 -> 91999"));
    assert_eq!(err, None);
    assert_eq!(code, 0);
}

#[test]
fn render_restart_forced_says_killed() {
    // x-3498: a --force swap must read as a KILL, not a drain.
    let (out, err, code) = render_restart(&Ok(RestartOutcome {
        old_pid: Some(91627),
        new_pid: 91999,
        forced: true,
        note: None,
    }));
    assert_eq!(out.as_deref(), Some("forced: killed pid 91627 -> 91999"));
    assert_eq!(err, None);
    assert_eq!(code, 0);
}

#[test]
fn render_restart_note_rides_stderr_at_zero() {
    // --force declining a recycled pid is a report, not a failure.
    let (out, err, code) = render_restart(&Ok(RestartOutcome {
        old_pid: Some(7),
        new_pid: 9,
        forced: false,
        note: Some("--force did not signal pid 7".to_string()),
    }));
    assert_eq!(out.as_deref(), Some("restarted: pid 7 -> 9"));
    assert_eq!(
        err.as_deref(),
        Some("--force did not signal pid 7"),
        "the refusal is heard"
    );
    assert_eq!(code, 0, "the restart itself succeeded");
}

#[test]
fn render_restart_reports_fresh_when_down() {
    // AC2-EDGE: no daemon was running -> started fresh, no error, exit 0.
    let (out, err, code) = render_restart(&Ok(RestartOutcome {
        old_pid: None,
        new_pid: 42,
        forced: false,
        note: None,
    }));
    assert_eq!(
        out.as_deref(),
        Some("daemon was not running; started fresh (pid 42)")
    );
    assert_eq!(err, None);
    assert_eq!(code, 0);
}

#[test]
fn render_restart_failure_is_loud() {
    // AC2-FR: a SIGTERM failure carries a stderr line naming the pid + reason
    // and a nonzero exit; no false "restarted" on stdout.
    let (out, err, code) = render_restart(&Err(RestartError::SigtermFailed {
        pid: 91627,
        reason: "Operation not permitted (os error 1)".to_string(),
    }));
    assert_eq!(out, None);
    let err = err.expect("failure has a stderr line");
    assert!(err.contains("91627"), "names the pid");
    assert!(err.contains("SIGTERM"), "names the failure");
    assert_ne!(code, 0, "failure exits nonzero");

    // A did-not-exit timeout is equally loud and names the pid.
    let (_o, err2, code2) = render_restart(&Err(RestartError::DidNotExit { pid: 5, secs: 5 }));
    assert!(err2.unwrap().contains("did not exit"));
    assert_ne!(code2, 0);
}

// -----------------------------------------------------------------------
// Task 4.1: ask create-on-first-contact + follow-up output parity
// -----------------------------------------------------------------------

/// AC1-HP (create): ask with created=true in result prints "<short_id>\n" only.
#[test]
fn format_success_bg_create_prints_short_id() {
    let result = json!({"created": true, "short_id": "cx-1a2b3c"});
    let out = format_success("ask", "myagent", &result, false, true, false);
    assert_eq!(out, Some("cx-1a2b3c".to_string()));
}

/// AC1-HP (follow-up): ask without created prints the reply verbatim (no added newline).
#[test]
fn format_success_ask_followup_prints_reply_verbatim() {
    let reply = "Here is my answer to your question.";
    let result = json!({"reply": reply, "backend": "pty"});
    let out = format_success("ask", "myagent", &result, false, true, false);
    assert_eq!(out, Some(reply.to_string()));
}

/// AC2-ERR: ask follow-up with empty reply prints empty string (not None).
#[test]
fn format_success_ask_followup_empty_reply() {
    let result = json!({"reply": "", "backend": "pty"});
    let out = format_success("ask", "myagent", &result, false, true, false);
    assert_eq!(out, Some(String::new()));
}

/// The codex-thread bounded-ask receipt: `reply: null` + in_flight prints
/// the in-flight line, never an empty line that reads as an empty answer.
#[test]
fn format_success_ask_in_flight_prints_the_turn_not_nothing() {
    let result = json!({
        "reply": null,
        "backend": "codex-thread",
        "turn_id": "turn-9",
        "status": "in_flight",
    });
    let out = format_success("ask", "myagent", &result, false, true, false)
        .expect("in_flight formats a line");
    assert!(out.contains("turn-9"), "names the turn: {out}");
    assert!(out.contains("in flight"), "names the state: {out}");
}

/// AC3-HP: build_request accepts --from-name, --yolo, --timeout without error.
/// These flags are forwarded to the daemon so `ask` can be called with full
/// Python-parity flag surface without exit 2 (unknown flag).
#[test]
fn ask_accepts_from_name_yolo_timeout_flags() {
    let args = vec![
        "myagent".to_string(),
        "hello there".to_string(),
        "--from-name".to_string(),
        "fno".to_string(),
        "--yolo".to_string(),
        "--timeout".to_string(),
        "30".to_string(),
    ];
    let result = build_request("ask", &args);
    assert!(
        result.is_ok(),
        "build_request must accept --from-name/--yolo/--timeout: {result:?}"
    );
    let (method, params) = result.unwrap();
    assert_eq!(method, "agent.ask");
    assert_eq!(params["name"], "myagent");
    assert_eq!(params["from_name"], "fno");
    assert_eq!(params["yolo"], true);
    assert_eq!(params["timeout"], 30u64);
}

/// Codex P2 (PR #379): with `ask` unconditionally auto-routed, the binary
/// must accept the Click/Typer `--flag=value` equals form for EVERY
/// value-carrying option. Without the normalization, `--cwd=/repo` /
/// `--timeout=30` / `--from-name=bot` would regress to "unknown flag"
/// instead of reaching the dispatch. The harness axis is `--harness`
/// (wire param `provider`); `--provider=...` is a tombstone (AC3).
#[test]
fn ask_accepts_equals_form_for_all_value_flags() {
    let args = vec![
        "myagent".to_string(),
        "hello there".to_string(),
        "--harness=gemini".to_string(),
        "--cwd=/repo".to_string(),
        "--timeout=30".to_string(),
        "--from-name=bot".to_string(),
    ];
    let result = build_request("ask", &args);
    assert!(
        result.is_ok(),
        "equals-form ask flags must parse: {result:?}"
    );
    let (method, params) = result.unwrap();
    assert_eq!(method, "agent.ask");
    assert_eq!(params["name"], "myagent");
    assert_eq!(params["provider"], "gemini");
    assert_eq!(params["cwd"], "/repo");
    assert_eq!(params["timeout"], 30u64);
    assert_eq!(params["from_name"], "bot");
    // A value containing '=' (e.g. a path) splits only on the first '='.
    let (_m, p2) = build_request(
        "ask",
        &["a".to_string(), "m".to_string(), "--cwd=/a=b".to_string()],
    )
    .unwrap();
    assert_eq!(p2["cwd"], "/a=b");
    // AC3 (Rust path, equals form): the retired --provider spelling is a
    // tombstone in both syntaxes - it must NOT silently route.
    let err = build_request(
        "ask",
        &[
            "a".to_string(),
            "m".to_string(),
            "--provider=gemini".to_string(),
        ],
    )
    .unwrap_err();
    assert!(err.contains("split at the axis rename"), "got: {err}");
    assert!(err.contains("--harness/-H"), "got: {err}");
}

/// codex P2 (PR #73): `--model` must reach the request, else
/// `spawn --harness agy --once --model <name>` fails with "unknown flag"
/// before dispatch_agy_once sees it. Both space- and equals-form parse.
#[test]
fn spawn_forwards_model_flag() {
    let (_m, space) = build_request(
        "spawn",
        &[
            "wk".to_string(),
            "--harness".to_string(),
            "agy".to_string(),
            "--once".to_string(),
            "--model".to_string(),
            "Gemini 3.5 Flash (High)".to_string(),
        ],
    )
    .expect("--model must parse");
    assert_eq!(space["model"], "Gemini 3.5 Flash (High)");
    let (_m2, eq) = build_request("spawn", &["wk".to_string(), "--model=pro".to_string()])
        .expect("--model= must parse");
    assert_eq!(eq["model"], "pro");
}

#[test]
fn spawn_accepts_squad_placement_aliases() {
    let (_method, short) = build_request(
        "spawn",
        &[
            "reviewer".to_string(),
            "-s".to_string(),
            "reviews".to_string(),
            "-x".to_string(),
            "right".to_string(),
        ],
    )
    .expect("mobile placement aliases must parse");
    assert_eq!(short["squad"], "reviews");
    assert_eq!(short["split"], "right");

    let (_method, long) = build_request(
        "spawn",
        &[
            "reviewer".to_string(),
            "--squad=reviews".to_string(),
            "--split=right".to_string(),
        ],
    )
    .expect("long placement options must parse");
    assert_eq!(short, long);
}

#[test]
fn spawn_placement_is_pane_only() {
    // The thread lane (bg on the wire) carries the placement flags only
    // when --portal names the pane the thread hosts: without it the
    // placement has nothing to place, and headless never hosts a session.
    let params = serde_json::json!({"squad": "reviews", "split": "Right"});
    assert_eq!(
        validate_spawn_placement(&params, "bg"),
        Err("--workspace/-s, --split/-x, and --tab on --substrate \
             thread need --portal N: a thread hosts no pane until a portal \
             opens one, so the placement has nothing to place"
            .to_string())
    );
    assert_eq!(
        validate_spawn_placement(&serde_json::json!({"portal": 1u8}), "pane"),
        Err(
            "--portal applies only to --substrate thread; a pane hosts its \
             own geometry and headless hosts no session at all"
                .to_string()
        )
    );
    assert!(
        validate_spawn_placement(
            &serde_json::json!({"portal": 1u8, "split": "right", "tab": "3"}),
            "bg"
        )
        .is_ok(),
        "portal + placement on the thread lane is the legal combination"
    );
}

/// x-dfa4: `--permission-mode` parses in both space and equals form so the
/// pane re-exec (raw args) and the bg/headless reader (maybe_run_spawn) both
/// see it; an unknown-flag rejection would otherwise block the pane path.
#[test]
fn spawn_forwards_permission_mode_flag() {
    let (_m, space) = build_request(
        "spawn",
        &[
            "wk".to_string(),
            "--harness".to_string(),
            "claude".to_string(),
            "--substrate".to_string(),
            "bg".to_string(),
            "--permission-mode".to_string(),
            "acceptEdits".to_string(),
        ],
    )
    .expect("--permission-mode must parse");
    assert_eq!(space["permission_mode"], "acceptEdits");
    let (_m2, eq) = build_request(
        "spawn",
        &["wk".to_string(), "--permission-mode=plan".to_string()],
    )
    .expect("--permission-mode= must parse");
    assert_eq!(eq["permission_mode"], "plan");
}

// x-d012: --account parses into params (space + equals form) so the spawn
// arm can route an account spawn to the Python resolver instead of erroring
// as an unknown flag.
#[test]
fn spawn_forwards_account_flag() {
    let (_m, space) = build_request(
        "spawn",
        &[
            "wk".to_string(),
            "--account".to_string(),
            "readyrule".to_string(),
        ],
    )
    .expect("--account must parse");
    assert_eq!(space["account"], "readyrule");
    let (_m2, eq) = build_request("spawn", &["wk".to_string(), "--account=makers".to_string()])
        .expect("--account= must parse");
    assert_eq!(eq["account"], "makers");
}

// x-b6e2 (US1): the Tier-3 passthrough flags land in params under their
// snake_case keys, in both space and equals form.
#[test]
fn spawn_forwards_tier3_flags() {
    let (_m, p) = build_request(
        "spawn",
        &[
            "wk".to_string(),
            "--add-dir".to_string(),
            "/work".to_string(),
            "--agent".to_string(),
            "reviewer".to_string(),
            "--tools".to_string(),
            "Read,Edit".to_string(),
            "--deny-tools".to_string(),
            "Bash".to_string(),
        ],
    )
    .expect("tier-3 flags must parse");
    assert_eq!(p["add_dir"], "/work");
    assert_eq!(p["agent"], "reviewer");
    assert_eq!(p["tools"], "Read,Edit");
    assert_eq!(p["deny_tools"], "Bash");
    // Equals form (VALUE_FLAGS normalization) is equivalent.
    let (_m2, eq) = build_request("spawn", &["wk".to_string(), "--add-dir=/extra".to_string()])
        .expect("--add-dir= must parse");
    assert_eq!(eq["add_dir"], "/extra");
}

#[test]
fn spawn_forwards_effort_flag() {
    let (_method, params) = build_request(
        "spawn",
        &[
            "wk".to_string(),
            "--harness".to_string(),
            "codex".to_string(),
            "--substrate".to_string(),
            "headless".to_string(),
            "--effort".to_string(),
            "high".to_string(),
        ],
    )
    .expect("--effort must parse");
    assert_eq!(params["effort"], "high");
}

/// ab-3ff64151 AC1 (Rust-path parity) + x-bab1 AC6: `agents ask` accepts the
/// surviving phone shorts `-c`/`-t` and the global `-Y`, with the harness axis
/// short `-H` (renamed from `-p`). `-p` was the provider short; off spawn it
/// is now a loud tombstone, never silently bound to a harness.
#[test]
fn ask_accepts_phone_short_flags() {
    // -H/-c/-t/-Y build the byte-identical request the long flags would.
    let short = build_request(
        "ask",
        &[
            "myagent".to_string(),
            "hi".to_string(),
            "-H".to_string(),
            "claude".to_string(),
            "-c".to_string(),
            "/repo".to_string(),
            "-t".to_string(),
            "30".to_string(),
            "-Y".to_string(),
        ],
    )
    .expect("short flags must parse on the Rust ask path");
    let long = build_request(
        "ask",
        &[
            "myagent".to_string(),
            "hi".to_string(),
            "--harness".to_string(),
            "claude".to_string(),
            "--cwd".to_string(),
            "/repo".to_string(),
            "--timeout".to_string(),
            "30".to_string(),
            "--yolo".to_string(),
        ],
    )
    .expect("long flags must parse");
    assert_eq!(
        short, long,
        "short flags must build the same request as long flags"
    );
    let (method, params) = short;
    assert_eq!(method, "agent.ask");
    assert_eq!(params["name"], "myagent");
    assert_eq!(params["provider"], "claude");
    assert_eq!(params["cwd"], "/repo");
    assert_eq!(params["timeout"], 30u64);
    assert_eq!(params["yolo"], true);
    // AC6 (Rust path): -p is no longer the provider short; it is a loud
    // tombstone (the one-shot short is spawn-only), never a silent harness.
    let err = build_request(
        "ask",
        &[
            "myagent".to_string(),
            "hi".to_string(),
            "-p".to_string(),
            "claude".to_string(),
        ],
    )
    .unwrap_err();
    assert!(err.contains("-p is not valid here"), "got: {err}");
    assert!(err.contains("--harness/-H"), "got: {err}");
}

/// ab-3ff64151 AC2 (Rust-path parity): the global-register boolean shorts
/// the client recognizes (`-A` --all, `-F` --force) parse identically to the
/// long forms on the verbs that use them. `-J` --json is client-side (not a
/// build_request param) and is covered by the json-detection path.
#[test]
fn global_register_boolean_shorts_parse() {
    let (_m, all_params) = build_request("list", &["-A".to_string()]).expect("-A must parse");
    assert_eq!(all_params["all"], true);
    let (_m, force_params) =
        build_request("rm", &["myagent".to_string(), "-F".to_string()]).expect("-F must parse");
    assert_eq!(force_params["force"], true);
}

/// x-c5cc: the spawn-gate flags parse on the spawn verb (--force already
/// shared with stop/rm; --no-wait is gate-only).
#[test]
fn spawn_gate_flags_parse() {
    let args = vec![
        "w1".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
        "--substrate".to_string(),
        "bg".to_string(),
        "--force".to_string(),
        "--no-wait".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).expect("gate flags must parse");
    assert_eq!(params["force"], true);
    assert_eq!(params["no_wait"], true);
}

/// Both gate constructions (the daemon-bound codex-thread gate and the
/// shared one) read their flags through `gate_flags_from_params`: a
/// hardcoded `GateFlags { force: false, .. }` refused a `--force` spawn at
/// capacity and made `--no-wait` queue for a slot.
#[test]
fn gate_flags_read_from_params_for_both_gate_constructions() {
    let forced = gate_flags_from_params(&serde_json::json!({"force": true, "no_wait": true}));
    assert!(forced.force);
    assert!(forced.no_wait);
    let defaults = gate_flags_from_params(&serde_json::json!({}));
    assert!(!defaults.force);
    assert!(!defaults.no_wait);
}

#[test]
fn rust_owned_substrates_append_spawn_payload_brevity_but_python_pane_does_not() {
    let original = "$fno:target --no-merge x-1234";
    for substrate in ["bg", "headless"] {
        let enriched = effective_spawn_message(original, substrate);
        assert!(enriched.starts_with(&format!("{original}\n\n")));
        assert_eq!(enriched.matches("<fno_relay_compression>").count(), 1);
    }
    assert_eq!(effective_spawn_message(original, "pane"), original);
}

/// AC4-HP: spawn with provider and no --argv succeeds (uses provider-derived argv).
#[test]
fn spawn_without_argv_with_known_provider_succeeds() {
    // After Task 4.1, spawn with a known --provider and no --argv should
    // build the request without error (the daemon resolves argv from the provider).
    let args = vec![
        "myagent".to_string(),
        "--harness".to_string(),
        "codex".to_string(),
    ];
    let result = build_request("spawn", &args);
    assert!(
        result.is_ok(),
        "spawn with --provider (no --argv) must not error: {result:?}"
    );
    let (method, params) = result.unwrap();
    assert_eq!(method, "agent.spawn");
    assert_eq!(params["name"], "myagent");
    assert_eq!(params["provider"], "codex");
    // argv must be absent from params so the daemon knows to use provider-derived argv.
    assert!(
        params.get("argv").is_none(),
        "argv must be absent when using provider-derived argv"
    );
}

fn argv_of(params: &Value) -> Vec<String> {
    params["argv"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn spawn_strips_leading_double_dash_from_argv() {
    // Documented syntax: `spawn worker --argv -- sleep 60`. The `--`
    // separator must not become argv[0] (Codex P1).
    let args = vec![
        "worker".to_string(),
        "--argv".to_string(),
        "--".to_string(),
        "sleep".to_string(),
        "60".to_string(),
    ];
    let (method, params) = build_request("spawn", &args).unwrap();
    assert_eq!(method, "agent.spawn");
    assert_eq!(argv_of(&params), vec!["sleep", "60"]);
    assert_eq!(params["name"], "worker");
}

#[test]
fn spawn_argv_without_separator_is_unchanged() {
    let args = vec![
        "worker".to_string(),
        "--argv".to_string(),
        "codex".to_string(),
        "exec".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert_eq!(argv_of(&params), vec!["codex", "exec"]);
}

/// Sigma-review (PR #379): the equals-form normalization must NOT touch the
/// `--argv` payload. A downstream tool's `--timeout=5` in the provider
/// command line must survive verbatim, not get split into `--timeout 5`.
#[test]
fn spawn_argv_payload_equals_form_survives_normalization() {
    let args = vec![
        "worker".to_string(),
        "--argv".to_string(),
        "--".to_string(),
        "mytool".to_string(),
        "--timeout=5".to_string(),
        "--cwd=/x".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert_eq!(argv_of(&params), vec!["mytool", "--timeout=5", "--cwd=/x"]);
    // The verb's own --timeout/--cwd are NOT set from the payload tokens.
    assert!(params.get("timeout").is_none());
    assert!(params.get("cwd").is_none());
}

#[test]
fn mint_session_uuid_is_well_formed_v4() {
    let u = mint_session_uuid();
    let parts: Vec<&str> = u.split('-').collect();
    assert_eq!(parts.len(), 5, "uuid has five dash-separated groups: {u}");
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12],
        "uuid group widths: {u}"
    );
    assert!(
        u.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
        "uuid is hex + dashes: {u}"
    );
    // version nibble (group 3, first char) is '4'; variant nibble (group 4,
    // first char) is one of 8/9/a/b.
    assert_eq!(parts[2].chars().next().unwrap(), '4', "v4 version: {u}");
    assert!(
        matches!(parts[3].chars().next().unwrap(), '8' | '9' | 'a' | 'b'),
        "rfc-4122 variant: {u}"
    );
    assert_ne!(mint_session_uuid(), u, "two mints differ");
}

// -----------------------------------------------------------------------
// spawn defaults to an owned interactive pane (x-3ab8)
// -----------------------------------------------------------------------

#[test]
fn spawn_defaults_interactive_for_pty_providers() {
    // AC1-HP: spawn --provider <pty> (no --once) -> host_mode=interactive.
    // codex/gemini/agy never mint a session id (claude-only).
    for provider in ["codex", "gemini", "agy"] {
        let args = vec![
            "wk".to_string(),
            "--harness".to_string(),
            provider.to_string(),
        ];
        let (method, params) = build_request("spawn", &args).unwrap();
        assert_eq!(method, "agent.spawn");
        assert_eq!(
            params["host_mode"], "interactive",
            "{provider} default-interactive"
        );
        assert!(params.get("session_id").is_none(), "{provider} never mints");
    }
}

#[test]
fn spawn_claude_default_is_pty_lane_with_minted_session() {
    // claude default -> PTY lane (mode=interactive) + a minted session id.
    let args = vec![
        "wk".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert_eq!(params["host_mode"], "interactive");
    assert_eq!(params["mode"], "interactive");
    let sid = params["session_id"].as_str().expect("minted session_id");
    assert_eq!(sid.split('-').count(), 5, "minted a uuid: {sid}");
}

#[test]
fn spawn_once_is_headless_byte_unchanged() {
    // AC1-EDGE: --once is the back-compat alias for --substrate headless ->
    // no host_mode, no mint, for EVERY provider; substrate=headless.
    for provider in ["claude", "codex", "gemini", "agy"] {
        let args = vec![
            "wk".to_string(),
            "--harness".to_string(),
            provider.to_string(),
            "--once".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(
            params.get("substrate").and_then(|v| v.as_str()),
            Some("headless"),
            "{provider} --once aliases to substrate=headless"
        );
        assert!(
            params.get("host_mode").is_none(),
            "{provider} --once: no host_mode"
        );
        assert!(
            params.get("session_id").is_none(),
            "{provider} --once: no mint"
        );
    }
}

#[test]
fn spawn_once_after_named_message_stays_headless() {
    let args = vec![
        "--name".to_string(),
        "parity-agent".to_string(),
        "hi".to_string(),
        "--harness".to_string(),
        "codex".to_string(),
        "--once".to_string(),
    ];

    let (method, params) = build_request("spawn", &args).unwrap();

    assert_eq!(method, "agent.spawn");
    assert_eq!(params["name"], "parity-agent");
    assert_eq!(params["message"], "hi");
    assert_eq!(params["provider"], "codex");
    assert_eq!(params["substrate"], "headless");
    assert!(params.get("host_mode").is_none());
}

#[test]
fn spawn_substrate_pane_is_default_and_interactive() {
    // AC1-UI: no --substrate -> pane -> interactive defaults applied (the
    // x-3ab8 owned-PTY behavior is the strictly-additive default).
    let args = vec![
        "wk".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert!(
        params.get("substrate").is_none(),
        "no substrate key when omitted"
    );
    assert_eq!(params["host_mode"], "interactive");
    // Explicit --substrate pane is identical (interactive defaults applied).
    let args = vec![
        "wk".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
        "--substrate".to_string(),
        "pane".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert_eq!(params["substrate"], "pane");
    assert_eq!(params["host_mode"], "interactive");
}

#[test]
fn spawn_substrate_bg_and_headless_suppress_interactive() {
    // thread (with deprecated bg alias) + headless are client-side lanes:
    // no host_mode, no mint.
    for sub in ["thread", "bg", "headless"] {
        let args = vec![
            "wk".to_string(),
            "--harness".to_string(),
            "claude".to_string(),
            "--substrate".to_string(),
            sub.to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        let expected = if sub == "bg" { "thread" } else { sub };
        assert_eq!(params["substrate"], expected);
        assert!(params.get("host_mode").is_none(), "{sub}: no host_mode");
        assert!(params.get("session_id").is_none(), "{sub}: no mint");
    }
}

#[test]
fn spawn_substrate_rejects_unknown_value() {
    let args = vec![
        "wk".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
        "--substrate".to_string(),
        "detached".to_string(),
    ];
    let err = build_request("spawn", &args).unwrap_err();
    assert!(err.contains("--substrate must be one of"), "got: {err}");
}

#[test]
fn spawn_explicit_substrate_wins_over_once_alias() {
    // --substrate set explicitly is not clobbered by a trailing --once.
    let args = vec![
        "wk".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
        "--substrate".to_string(),
        "bg".to_string(),
        "--once".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert_eq!(params["substrate"], "thread");
}

#[test]
fn spawn_headless_flag_aliases_to_substrate_headless() {
    // x-c772: --headless is the front for --substrate headless (identical to
    // --once), for every provider. `-H` was reassigned to --harness (x-6de8).
    for flag in ["--headless", "--once", "-o"] {
        for provider in ["claude", "codex", "gemini", "agy"] {
            let args = vec![
                "wk".to_string(),
                "--harness".to_string(),
                provider.to_string(),
                flag.to_string(),
            ];
            let (_m, params) = build_request("spawn", &args).unwrap();
            assert_eq!(
                params.get("substrate").and_then(|v| v.as_str()),
                Some("headless"),
                "{provider} {flag} aliases to substrate=headless"
            );
            assert!(params.get("host_mode").is_none(), "{flag}: no host_mode");
        }
    }
}

#[test]
fn spawn_harness_flag_sets_provider() {
    // x-6de8: --harness/-H is the CLI-binary axis. -H takes a VALUE (harness
    // name) rather than meaning headless.
    for flag in ["--harness", "-H"] {
        let args = vec!["wk".to_string(), flag.to_string(), "codex".to_string()];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(
            params.get("provider").and_then(|v| v.as_str()),
            Some("codex"),
            "{flag} sets provider"
        );
        // -H carries a value now, so it must NOT default the substrate to headless.
        assert!(
            params.get("substrate").is_none(),
            "{flag}: no headless substrate side effect"
        );
    }
}

#[test]
fn spawn_p_short_is_headless_not_provider() {
    // x-6de8: -p mirrors the harnesses' own one-shot short. It takes NO value,
    // so a stray `-p codex` must leave `codex` a positional rather than
    // silently selecting a harness.
    let args = vec!["wk".to_string(), "-p".to_string()];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert_eq!(
        params.get("substrate").and_then(|v| v.as_str()),
        Some("headless")
    );
    assert!(
        params.get("provider").is_none(),
        "-p must not set a harness"
    );

    // Off `spawn`, -p is no longer the provider short (the harness axis is
    // --harness/-H); it is a loud tombstone, never silently bound to a harness.
    let ask = vec![
        "wk".to_string(),
        "hi".to_string(),
        "-p".to_string(),
        "codex".to_string(),
    ];
    let err = build_request("ask", &ask).unwrap_err();
    assert!(err.contains("-p is not valid here"), "got: {err}");
    assert!(err.contains("--harness/-H"), "got: {err}");
}

#[test]
fn spawn_harness_name_on_the_provider_axis_is_rejected() {
    // x-6de8: --provider is the model-VENDOR axis. This lane never re-execs
    // Python cmd_spawn, so a harness name typed there must be refused BY NAME
    // here too, or it reaches the daemon as a vendor it cannot resolve.
    for h in ["claude", "codex", "gemini", "opencode", "agy"] {
        let args = vec!["wk".to_string(), "--provider".to_string(), h.to_string()];
        let err = build_request("spawn", &args).unwrap_err();
        assert!(
            err.contains(&format!("{h} is a harness, not a provider")),
            "got: {err}"
        );
        assert!(err.contains(&format!("use --harness {h}")), "got: {err}");
    }

    // A real vendor names the routed lane, which only the fno CLI materializes.
    let args = vec![
        "wk".to_string(),
        "--provider".to_string(),
        "zai".to_string(),
    ];
    let err = build_request("spawn", &args).unwrap_err();
    assert!(err.contains("names a model vendor"), "got: {err}");

    // Off `spawn`, --provider is the retired harness-axis spelling: a
    // tombstone (the CLI binary is --harness/-H), never silently routed.
    let ask = vec![
        "wk".to_string(),
        "hi".to_string(),
        "--provider".to_string(),
        "codex".to_string(),
    ];
    let err = build_request("ask", &ask).unwrap_err();
    assert!(err.contains("split at the axis rename"), "got: {err}");
    assert!(err.contains("--harness/-H"), "got: {err}");
}

#[test]
fn spawn_model_short_m_parses_like_long() {
    // x-c772: -m is the mobile short for --model.
    for flag in ["--model", "-m"] {
        let args = vec![
            "wk".to_string(),
            "--harness".to_string(),
            "claude".to_string(),
            "--substrate".to_string(),
            "bg".to_string(),
            flag.to_string(),
            "opus".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(
            params.get("model").and_then(|v| v.as_str()),
            Some("opus"),
            "{flag} sets model"
        );
    }
}

#[test]
fn spawn_explicit_substrate_wins_over_headless_flag() {
    // An explicit --substrate is not clobbered by a trailing --headless.
    let args = vec![
        "wk".to_string(),
        "--harness".to_string(),
        "claude".to_string(),
        "--substrate".to_string(),
        "bg".to_string(),
        "--headless".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert_eq!(params["substrate"], "thread");
}

#[test]
fn spawn_unknown_provider_does_not_force_interactive() {
    // AC1-EDGE (Boundaries): an unknown provider keeps today's behavior; the
    // daemon's provider_for_pty errors on it as before, so we must NOT force
    // host_mode (which would change the error surface). goose is the
    // canonical unhosted CLI (opencode joined the roster at x-51f6).
    let args = vec![
        "wk".to_string(),
        "--harness".to_string(),
        "goose".to_string(),
    ];
    let (_m, params) = build_request("spawn", &args).unwrap();
    assert!(
        params.get("host_mode").is_none(),
        "unknown provider: interactive not forced"
    );
}

#[test]
fn format_success_spawn_emits_compact_receipt() {
    // x-3ab8: a daemon-routed spawn must emit the one-line JSON receipt that
    // advance.py / dispatch-node.sh parse for short_id (line-by-line
    // json.loads needs it compact, not pretty-printed).
    let result = json!({
        "short_id": "ab12cd34",
        "harness": "claude",
        "status": "live",
        "extra": "ignored"
    });
    let line = format_success("spawn", "wk", &result, false, false, false).unwrap();
    assert!(!line.contains('\n'), "receipt must be one line: {line}");
    let parsed: Value = serde_json::from_str(&line).expect("valid JSON receipt");
    assert_eq!(parsed["name"], "wk");
    assert_eq!(parsed["short_id"], "ab12cd34");
    assert_eq!(parsed["harness"], "claude");
    assert_eq!(parsed["status"], "live");
}

// -----------------------------------------------------------------------
// cwd forwarding (fix/agents-host-cwd): the daemon is a shared long-lived
// process whose own current_dir is frozen to wherever it was first started,
// so the client must stamp the caller's cwd into daemon-bound spawn/ask
// requests. Without this, a spawn from project A opens the provider in the
// daemon's home project B.
// -----------------------------------------------------------------------

#[test]
fn ensure_request_cwd_stamps_caller_dir_for_spawn() {
    let mut params = json!({"name": "w", "provider": "codex"});
    ensure_request_cwd("agent.spawn", &mut params, Path::new("/work/proj"));
    assert_eq!(params["cwd"], "/work/proj");
}

#[test]
fn ensure_request_cwd_explicit_cwd_wins() {
    // An explicit --cwd (already in params) must never be overwritten.
    let mut params = json!({"name": "w", "provider": "codex", "cwd": "/explicit"});
    ensure_request_cwd("agent.spawn", &mut params, Path::new("/work/proj"));
    assert_eq!(params["cwd"], "/explicit");
}

#[test]
fn ensure_request_cwd_covers_ask_first_contact() {
    // gemini `ask` falls through to the daemon's auto-spawn path, which has
    // the same cwd fallback; the client must forward cwd for agent.ask too.
    let mut params = json!({"name": "g", "provider": "gemini"});
    ensure_request_cwd("agent.ask", &mut params, Path::new("/work/proj"));
    assert_eq!(params["cwd"], "/work/proj");
}

#[test]
fn ensure_request_cwd_skips_non_spawn_methods() {
    // list/stop/rm carry no worker launch; leave params untouched so a
    // `--cwd` *filter* on list is the only thing that sets cwd there.
    let mut params = json!({"status": "live"});
    ensure_request_cwd("agent.list", &mut params, Path::new("/work/proj"));
    assert!(params.get("cwd").is_none());
}

// -----------------------------------------------------------------------
// x-85fe: canonical-by-default cwd precedence (inverts ab-77b691dc)
//
// effective_worker_cwd encodes: --cwd > --here (caller) > default canonical
// (unresolved canonical -> caller, the safe side). --fresh is an accepted
// no-op alias. It is pure so the precedence is provable without git.
// -----------------------------------------------------------------------

fn pb(s: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(s)
}

#[test]
fn effective_cwd_default_resolves_canonical() {
    // No flags: the inverted default lands on canonical (AC1-HP).
    let got = effective_worker_cwd(None, false, false, Some(pb("/canon")), pb("/wt"));
    assert_eq!(got, pb("/canon"));
}

#[test]
fn effective_cwd_fresh_is_noop_alias() {
    // --fresh is an accepted no-op alias: identical to passing nothing, the
    // default already being canonical (AC2-EDGE).
    let with_fresh = effective_worker_cwd(None, true, false, Some(pb("/canon")), pb("/wt"));
    let without = effective_worker_cwd(None, false, false, Some(pb("/canon")), pb("/wt"));
    assert_eq!(with_fresh, without);
    assert_eq!(with_fresh, pb("/canon"));
}

#[test]
fn effective_cwd_here_keeps_caller() {
    // --here is the explicit opt-in to stay in the caller's worktree (AC2-HP).
    let got = effective_worker_cwd(None, false, true, Some(pb("/canon")), pb("/wt"));
    assert_eq!(got, pb("/wt"));
}

#[test]
fn effective_cwd_unresolved_canonical_falls_back_to_caller() {
    // Ambiguous / git-missing canonical resolution -> caller cwd, the safe
    // side (AC1-ERR; Failure Modes > Boundaries: never guess canonical).
    let got = effective_worker_cwd(None, false, false, None, pb("/wt"));
    assert_eq!(got, pb("/wt"));
}

#[test]
fn effective_cwd_explicit_cwd_wins_over_everything() {
    // --cwd is the highest-priority cwd source and wins over --here/--fresh
    // (AC2-ERR; Failure Modes > Invariants).
    let got = effective_worker_cwd(
        Some(pb("/explicit")),
        true,
        true,
        Some(pb("/canon")),
        pb("/wt"),
    );
    assert_eq!(got, pb("/explicit"));
}

#[test]
fn build_request_parses_fresh_and_here_flags() {
    // --fresh / --here / --in-place are plumbed into params for spawn/ask.
    let (_m, p) = build_request(
        "spawn",
        &[
            "w".into(),
            "--harness".into(),
            "claude".into(),
            "--fresh".into(),
        ],
    )
    .unwrap();
    assert_eq!(p["fresh"], Value::Bool(true));
    assert!(p.get("here").is_none());

    let (_m, p) = build_request("ask", &["w".into(), "hi".into(), "--here".into()]).unwrap();
    assert_eq!(p["here"], Value::Bool(true));

    let (_m, p) = build_request("ask", &["w".into(), "hi".into(), "--in-place".into()]).unwrap();
    assert_eq!(p["here"], Value::Bool(true));
}

// -----------------------------------------------------------------------
// (x-9b60) The portal placement trio on the Rust spawn path
// -----------------------------------------------------------------------

/// AC6-HP/AC7-ERR: the runtime that runs accepts the flags the help
/// advertises, and an out-of-range --portal refuses with the range, never
/// the catch-all "unknown flag".
#[test]
fn spawn_placement_flags_are_parsed_and_bounded() {
    let (method, p) = build_request(
        "spawn",
        &[
            "w".into(),
            "--substrate".into(),
            "thread".into(),
            "--portal".into(),
            "1".into(),
            "--tab".into(),
            "3".into(),
            "--at".into(),
            "7".into(),
            "--split".into(),
            "Right".into(),
        ],
    )
    .unwrap();
    assert_eq!(method, "agent.spawn");
    assert_eq!(p["portal"], Value::from(1u8));
    assert_eq!(p["tab"], Value::String("3".into()));
    assert_eq!(p["at"], Value::String("7".into()));
    assert_eq!(p["split"], Value::String("Right".into()));

    for bad in ["999", "abc", "-1", ""] {
        let err = build_request(
            "spawn",
            &["w".into(), "--portal".into(), bad.to_string().into()],
        )
        .unwrap_err();
        assert!(
            err.contains("0-255"),
            "the refusal names the range, never the catch-all: {err}"
        );
        assert!(!err.contains("unknown flag"), "{err}");
    }
}

/// The equals form parses the same way: a routed `--portal=1` must not
/// regress to "unknown flag" (the PR 379/371 regression class).
#[test]
fn spawn_portal_equals_form_parses() {
    let (_m, p) = build_request(
        "spawn",
        &["w".into(), "--portal=1".into(), "--tab=2".into()],
    )
    .unwrap();
    assert_eq!(p["portal"], Value::from(1u8));
    assert_eq!(p["tab"], Value::String("2".into()));
}

// (canonical_repo_root unit tests live in src/paths.rs, where the shared
// resolver now lives -- ab-77b691dc.)

// -----------------------------------------------------------------------
// Task 3.1: list/reconcile JSON parity + flag parsing
// -----------------------------------------------------------------------

/// AC1-HP: list --status is parsed into daemon params (not rejected as unknown)
#[test]
fn list_status_flag_is_parsed() {
    let args = vec!["--status".to_string(), "live".to_string()];
    let (method, params) = build_request("list", &args).unwrap();
    assert_eq!(method, "agent.list");
    assert_eq!(params["status"], Value::String("live".to_string()));
}

#[test]
fn list_progress_flag_is_parsed() {
    let args = vec!["--progress".to_string(), "parked".to_string()];
    let (method, params) = build_request("list", &args).unwrap();
    assert_eq!(method, "agent.list");
    assert_eq!(params["progress"], Value::String("parked".to_string()));
}

/// AC1-HP: list --cwd and --provider are forwarded to daemon params
#[test]
fn list_filter_flags_are_forwarded() {
    let args = vec![
        "--cwd".to_string(),
        "/tmp/myproject".to_string(),
        "--harness".to_string(),
        "codex".to_string(),
    ];
    let (_method, params) = build_request("list", &args).unwrap();
    assert_eq!(params["cwd"], Value::String("/tmp/myproject".to_string()));
    assert_eq!(params["provider"], Value::String("codex".to_string()));
}

/// A non-live filter reaches the discovered rows instead of discarding the
/// whole lane.
///
/// The lane used to return empty for any filter other than `live`, which
/// encoded "a discovered session is live by definition". The shared
/// reachability verdict retired that: a discovered row whose process is
/// provably gone comes back `orphaned`, and dropping the lane made a filter
/// for orphaned rows answer differently through Rust than through Python
/// for the same registry.
#[test]
fn discovered_rows_are_filtered_by_their_own_verdict() {
    let row = |name: &str, verdict: &str| json!({"name": name, "status": verdict});
    let all = vec![
        row("live-one", "live"),
        row("gone-one", "orphaned"),
        row("quiet-one", "unknown"),
    ];

    let mut orphaned = all.clone();
    retain_discovered_by_status(&mut orphaned, Some("orphaned"));
    assert_eq!(orphaned.len(), 1, "an orphaned discovered row must survive");
    assert_eq!(orphaned[0]["name"], "gone-one");

    let mut live = all.clone();
    retain_discovered_by_status(&mut live, Some("live"));
    assert_eq!(live.len(), 1);
    assert_eq!(live[0]["name"], "live-one");

    // No filter keeps every row: the caller asked no question.
    let mut unfiltered = all.clone();
    retain_discovered_by_status(&mut unfiltered, None);
    assert_eq!(unfiltered.len(), 3);
}

#[test]
fn discovered_rows_are_filtered_by_progress_independently() {
    let mut rows = vec![
        json!({"name": "active", "status": "live", "progress": "advancing"}),
        json!({"name": "wedged", "status": "live", "progress": "unknown"}),
        json!({"name": "done", "status": "unknown", "progress": "parked"}),
    ];
    retain_discovered_by_progress(&mut rows, Some("unknown"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "wedged");
    assert_eq!(rows[0]["status"], "live");
}

/// AC1-HP: --json is NOT forwarded to daemon params (it is a client-side rendering flag)
#[test]
fn list_json_flag_is_not_forwarded_to_daemon() {
    // --json must be captured by build_request as a recognized flag
    // but NOT appear in the daemon params object.
    // build_request itself should not error on --json.
    let args = vec!["--json".to_string()];
    let result = build_request("list", &args);
    // Must succeed (not return Err "unknown flag: --json")
    assert!(result.is_ok(), "build_request must accept --json for list");
    let (_method, params) = result.unwrap();
    // --json must NOT be forwarded to the daemon
    assert!(
        params.get("json").is_none(),
        "--json must not appear in daemon params"
    );
}

/// AC2-HP: render_list_json produces the Python-matching shape with correct keys
#[test]
fn render_list_json_shape_matches_python_contract() {
    // Simulate the full daemon RPC result so envelope metadata cannot be
    // reconstructed independently by the outward client renderer.
    let result = json!({
        "agents": [{
            "name": "worker-a",
            "harness": "claude",
            "observed_model": {"kind": "observed", "model": "glm-5.2", "samples": 300},
            "short_id": "cl-abc123",
            "session_id": "cl-abc123",
            "cwd": "/home/user/project",
            "created_at": "2026-05-25T00:00:00Z",
            "last_message_at": "2026-05-25T01:00:00Z",
            "status": "live",
            "live_status": null,
            "pid": 4242,
            "last_reconciled_at": "2026-05-25T00:30:00Z",
            "log_path": null,
        }],
        "fields_omitted": ["model", "model_basis"],
        "filters_applied": {"cwd": null, "provider": null, "status": null},
    });
    let output = format_success("list", "", &result, true, false, false)
        .expect("list has an outward renderer");

    let parsed: Value = serde_json::from_str(&output).expect("must be valid JSON");
    // Top-level keys must match Python's render_json shape
    assert!(parsed.get("agents").is_some(), "missing 'agents' key");
    assert!(parsed.get("count").is_some(), "missing 'count' key");
    assert!(
        parsed.get("filters_applied").is_some(),
        "missing 'filters_applied' key"
    );
    assert!(
        parsed.get("schema_version").is_some(),
        "missing 'schema_version' key"
    );
    // ab-098967b4: discovered lane keys are additive; schema bumped to 2.
    assert!(
        parsed.get("discovered_sessions").is_some(),
        "missing 'discovered_sessions' key"
    );
    assert_eq!(parsed["discovered_count"], 0);
    assert_eq!(parsed["schema_version"], 6);
    assert_eq!(parsed["count"], 1);
    assert_eq!(
        parsed["fields_omitted"], result["fields_omitted"],
        "outward envelope must preserve the daemon's exact sorted omission contract"
    );

    // NOT the key-set guard, despite appearances: this row is hand-built in
    // this test, so the list below only asserts against its own input. The
    // real projection lives in the daemon and is pinned to
    // schemas/agents-list-row.json by daemon.rs's
    // `list_row_key_set_matches_shared_contract`. Adding a key here proves
    // nothing about what `fno agents list` emits.
    //
    // The client passes rows through verbatim: the 10 Python parity keys
    // (incl. live_status, retained for back-compat -- AC4-FR) plus the
    // additive Architecture C keys pid + last_reconciled_at (AC4-HP) survive.
    let row = &parsed["agents"][0];
    for key in &[
        "name",
        "harness",
        "observed_model",
        "short_id",
        "session_id",
        "cwd",
        "created_at",
        "last_message_at",
        "status",
        "live_status",
        "log_path",
        "pid",
        "last_reconciled_at",
    ] {
        assert!(row.get(*key).is_some(), "row missing key: {key}");
    }
    assert_eq!(row["pid"], 4242, "pid passes through");
    assert!(row["live_status"].is_null(), "live_status retained as null");
}

#[test]
fn empty_effort_is_rejected_but_opencode_values_are_passed_through() {
    assert_eq!(
        validate_effort_for_spawn("claude", "headless", Some("")),
        Err("--effort must be non-empty".to_string())
    );
    assert_eq!(
        validate_effort_for_spawn("codex", "bg", Some("")),
        Err("--effort must be non-empty".to_string())
    );
    assert!(validate_effort_for_spawn("opencode", "headless", Some("provider-value")).is_ok());
}

/// The effort deny set must be the same in both spelling maps: the Python
/// lane (`effort_tokens`, `--substrate pane`) allows agy, so the Rust lane
/// refusing it made one harness two-valued by substrate. agy carries its
/// own `--effort (low|medium|high)`; gemini genuinely has no surface. The
/// FLAG_OWNERS row for `--effort` names both maps.
#[test]
fn agy_effort_is_accepted_and_gemini_is_still_refused() {
    assert!(validate_effort_for_spawn("agy", "headless", Some("high")).is_ok());
    assert!(validate_effort_for_spawn("agy", "bg", Some("low")).is_ok());
    assert!(validate_effort_for_spawn("agy", "pane", Some("nonsense-still-forwarded")).is_ok());
    assert!(validate_effort_for_spawn("gemini", "headless", Some("high")).is_err());
}

/// ab-098967b4: render_list_json folds in the discovered lane (additive
/// keys, schema 2); render_list_table appends a distinct DISCOVERED section.
#[test]
fn render_list_with_discovered_lane() {
    let agents = json!([]);
    let filters = json!({"cwd": null, "provider": null, "status": null});
    let discovered = vec![json!({
        "handle": "fno-aaaa1111",
        "address": "aaaa1111",
        "short_id": "aaaa1111",
        "session_id": "uuid-1",
        "pid": 4242,
        "cwd": "/Users/x/code/proj",
        "project": "fno",
        "status": "busy",
        "agent": "claude",
    })];
    let out = render_list_json(&agents, &filters, &json!(["model"]), &discovered);
    let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(parsed["discovered_count"], 1);
    assert_eq!(parsed["discovered_sessions"][0]["handle"], "fno-aaaa1111");
    assert_eq!(parsed["schema_version"], 6);

    let table = render_list_table(&agents, &discovered);
    assert!(table.contains("DISCOVERED LIVE SESSIONS (1, host-local)"));
    // ADDRESS leads and the alias is demoted to LABEL. The alias led this
    // table for its whole life, so it was the leftmost thing a reader
    // copied, and `<project>-<short8>` is not a mailbox.
    // Anchored on the banner: the registry header above now carries
    // ADDRESS too, so a bare `contains` search finds the wrong line.
    let lines: Vec<&str> = table.lines().collect();
    let banner = lines
        .iter()
        .position(|l| l.contains("DISCOVERED LIVE"))
        .expect("banner present");
    let header = lines[banner + 1];
    assert!(
        header.find("ADDRESS") < header.find("LABEL"),
        "address must lead the discovered lane, got: {header:?}"
    );
    assert!(!table.contains("HANDLE"), "HANDLE was renamed to LABEL");
    assert!(table.contains("aaaa1111"));
    assert!(table.contains("fno-aaaa1111"));
    assert!(table.contains("busy"));
}

/// The registry table is the surface nearly every reader sees: `list`
/// auto-routes here whenever an installed binary is present. An address
/// column that existed only on the Python table would leave that reader
/// copying NAME, whose durable write queues under a key no drain reads --
/// the exact stranded-mail failure the column exists to end.
#[test]
fn render_list_table_carries_the_mailbox_address() {
    let agents = json!([
        {
            "name": "pane-worker",
            "address": "e6f78b98",
            "harness": "claude",
            "short_id": null,
            "session_id": null,
            "cwd": "/home/user/project",
            "created_at": "2026-05-25T00:00:00Z",
            "last_message_at": null,
            // The liveness key is omitted on purpose. The assertions below
            // never read it, and `check-plan-rung-authority` ratchets an
            // identifier count over production Rust files with this
            // module's inline tests included, so an unused fixture key
            // would move a baseline that polices plan-frontmatter
            // classification and has nothing to do with this table.
            "live_status": null,
            "pid": null,
            "last_reconciled_at": null,
            "log_path": null,
        }
    ]);

    let table = render_list_table(&agents, &[]);
    let lines: Vec<&str> = table.lines().collect();

    assert!(
        lines[0].find("NAME") < lines[0].find("ADDRESS"),
        "ADDRESS sits second, mirroring the Python renderer: {:?}",
        lines[0]
    );
    // By value, not by header presence: a column that is always `-` is the
    // same lie in a different shape. This row is the shape the column
    // exists for -- a pane worker whose only other identifier was its name.
    assert!(
        lines[1].contains("e6f78b98"),
        "address value must reach the row: {:?}",
        lines[1]
    );
}

/// AC5-UI: render_list_table drops LIVE and adds CHECKED + PID; AC2-UI: a
/// never-reconciled row renders `never`; AC4: a PTY pid is shown, an ask
/// row's null pid renders `-`.
#[test]
fn render_list_table_has_checked_and_pid_columns_not_live() {
    let agents = json!([
        {
            "name": "pty-worker",
            "harness": "codex",
            "short_id": "wk1",
            "session_id": null,
            "cwd": "/home/user/project",
            "created_at": "2026-05-25T00:00:00Z",
            "last_message_at": null,
            "status": "live",
            "live_status": null,
            "pid": 4242,
            "last_reconciled_at": "2026-05-25T00:00:00Z",
            "log_path": null,
        },
        {
            "name": "ask-row",
            "harness": "claude",
            "short_id": null,
            "session_id": "cl-xyz",
            "cwd": "/home/user/other",
            "created_at": "2026-05-25T00:00:00Z",
            "last_message_at": null,
            "status": "exited",
            "live_status": null,
            "pid": null,
            "last_reconciled_at": null,
            "log_path": null,
        }
    ]);
    let table = render_list_table(&agents, &[]);
    let lines: Vec<&str> = table.lines().collect();
    // AC5-UI: header shows STATUS + CHECKED + PID, and LIVE is gone.
    assert!(
        lines[0].contains("NAME")
            && lines[0].contains("HARNESS")
            && lines[0].contains("STATUS")
            && lines[0].contains("CHECKED")
            && lines[0].contains("PID")
            && lines[0].contains("CWD"),
        "header must contain the new column set, got: {:?}",
        lines[0]
    );
    assert!(
        !lines[0].contains("LIVE"),
        "LIVE column must be removed, got: {:?}",
        lines[0]
    );
    // header + 2 data rows
    assert!(lines.len() >= 3, "got {} lines", lines.len());
    // PTY row shows its worker pid (AC4-HP at the table surface).
    let pty_line = lines.iter().find(|l| l.contains("pty-worker")).unwrap();
    assert!(pty_line.contains("4242"), "PTY pid in table: {pty_line}");
    // Never-reconciled ask row renders `never` (AC2-UI), not `0s`/blank.
    let ask_line = lines.iter().find(|l| l.contains("ask-row")).unwrap();
    assert!(
        ask_line.contains("never"),
        "unprobed row shows never: {ask_line}"
    );
}

/// EVENT AGE renders the transcript's newest-activity age and LAST
/// MESSAGE the flattened last-turn text. The registry timestamp this column
/// was wired to for its whole life was null on many rows while the worker
/// was mid-sentence, so a "last message" column that never showed a
/// message. Absent readings render `unknown` / `-`, never a fresh age: an
/// unread transcript is not health.
///
/// The state fixture key is omitted on purpose (see the address test
/// above): the identifier ratchet in check-plan-rung-authority counts over
/// inline tests too, and the assertions below never read it.
#[test]
fn render_list_table_has_event_age_and_last_message_columns() {
    let fresh = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
    let agents = json!([
        {
            "name": "live-worker",
            "address": "aaaa1111",
            "harness": "claude",
            "short_id": null,
            "session_id": null,
            "cwd": "/home/user/project",
            "created_at": "2026-05-25T00:00:00Z",
            "last_message_at": null,
            "live_status": null,
            "pid": 99,
            "last_reconciled_at": "2026-05-25T00:00:00Z",
            "last_event_at": fresh,
            "last_message": "[tool_use: Bash] on the pytest run",
            "log_path": null
        },
        {
            "name": "gone-worker",
            "address": "bbbb2222",
            "harness": "claude",
            "short_id": null,
            "session_id": null,
            "cwd": "/home/user/other",
            "created_at": "2026-05-25T00:00:00Z",
            "last_message_at": null,
            "live_status": null,
            "pid": null,
            "last_reconciled_at": null,
            "log_path": null
        }
    ]);
    let table = render_list_table(&agents, &[]);
    let lines: Vec<&str> = table.lines().collect();

    assert!(
        lines[0].contains("EVENT AGE") && lines[0].contains("LAST MESSAGE"),
        "header must carry both new columns, got: {:?}",
        lines[0]
    );
    let live = lines.iter().find(|l| l.contains("live-worker")).unwrap();
    assert!(live.contains("5s"), "fresh transcript age renders: {live}");
    assert!(
        live.contains("[tool_use: Bash] on the pytest run"),
        "last-turn text renders: {live}"
    );
    // No probe answer: an absent reading, rendered as unread - never `0s`,
    // which would claim the transcript moved seconds ago.
    let gone = lines.iter().find(|l| l.contains("gone-worker")).unwrap();
    assert!(
        gone.contains("unknown"),
        "absent stamp reads unknown: {gone}"
    );
    assert!(
        !gone.contains("0s"),
        "absent stamp never reads fresh: {gone}"
    );
}

#[test]
fn format_age_secs_compact_units() {
    // AC2-EDGE: compact single-unit ages across the threshold boundaries.
    assert_eq!(format_age_secs(3), "3s");
    assert_eq!(format_age_secs(59), "59s");
    assert_eq!(format_age_secs(240), "4m");
    assert_eq!(format_age_secs(3599), "59m");
    assert_eq!(format_age_secs(64800), "18h"); // 18 * 3600
    assert_eq!(format_age_secs(86400), "1d");
    // Clock skew (future timestamp) clamps to 0s, never negative.
    assert_eq!(format_age_secs(-5), "0s");
}

#[test]
fn render_checked_handles_never_and_unparseable() {
    let now = chrono::Utc::now();
    // AC2-UI: never reconciled.
    assert_eq!(render_checked(None, now), "never");
    // A parseable recent timestamp yields a small age (seconds bucket).
    let recent = (now - chrono::Duration::seconds(5)).to_rfc3339();
    assert_eq!(render_checked(Some(&recent), now), "5s");
    // An unparseable stored value is explicit `?`, never blank or a panic.
    assert_eq!(render_checked(Some("not-a-timestamp"), now), "?");
}

/// AC4-HP: render_reconcile_json produces the Python-matching key set
#[test]
fn render_reconcile_json_shape_matches_python_contract() {
    let daemon_result = json!({
        "scanned": 3,
        "orphaned": [{"name": "gone-agent", "provider": "claude", "id": "cl-123"}],
        "recovered": [],
        "skipped": [],
        "errors": [],
    });
    let output = render_reconcile_json(&daemon_result);
    let parsed: Value = serde_json::from_str(&output).expect("must be valid JSON");
    // Must have exactly the Python ReconcileResult keys
    for key in &["scanned", "orphaned", "recovered", "skipped", "errors"] {
        assert!(
            parsed.get(*key).is_some(),
            "reconcile JSON missing key: {key}"
        );
    }
    assert_eq!(parsed["scanned"], 3);
}
