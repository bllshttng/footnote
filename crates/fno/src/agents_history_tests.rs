use super::*;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};
use tempfile::tempdir;

const SID: &str = "d23c3d68-2b1e-448d-af97-7ceee1222a90";
const OTHER_SID: &str = "a41c7f20-2d19-42a8-95de-13cde5012a77";

fn receipt(path: &str, value: Value) -> Receipt {
    Receipt {
        path: PathBuf::from(path),
        value,
    }
}

fn empty_sources() -> Sources {
    Sources {
        registry: Ok(Vec::new()),
        receipts: Ok(Vec::new()),
        graph: Ok(Vec::new()),
        ledger: Ok(Vec::new()),
        events: Ok(Vec::new()),
        repo_slug: None,
    }
}

fn specimen_sources(provider: Option<&str>, requested_model: &str) -> Sources {
    let mut registry = json!({
        "harness_session_id": SID,
        "name": "worker",
        "harness": "claude",
        "origin": "adopted",
        "created_at": "2026-09-23T13:41:05Z",
        "status": "exited",
        "requested_model": requested_model,
        "requested_effort": "xhigh",
        "requested_permission_mode": "bypassPermissions"
    });
    if let Some(provider) = provider {
        registry["provider"] = json!(provider);
        registry["route_settings_path"] = json!("routes/zai.toml");
    }
    Sources {
        registry: Ok(vec![registry]),
        receipts: Ok(Vec::new()),
        graph: Ok(vec![json!({
            "id": "x-3344",
            "title": "Session card fixture",
            "sessions": [
                {
                    "session_id": SID,
                    "phase": "do",
                    "started_at": "2026-09-23T06:30:00Z",
                    "ended_at": "2026-09-23T07:00:00Z",
                    "observed_model": "glm-5.3-flash",
                    "effort": "xhigh",
                    "merge_grant": {"approved": true, "source": "operator"}
                },
                {
                    "session_id": SID,
                    "phase": "ship",
                    "started_at": "2026-09-23T13:00:00Z",
                    "observed_model": "claude-opus-5-5"
                }
            ]
        })]),
        ledger: Ok(Vec::new()),
        events: Ok(vec![
            json!({
                "timestamp": "2026-09-23T00:48:20Z",
                "type": "agent_spawned",
                "data": {
                    "harness_session_id": SID,
                    "name": "worker",
                    "substrate": "thread",
                    "spawned_by": "b7e90e21-1b3a-4ab9-a6d2-3f2bde850aa1"
                }
            }),
            json!({
                "timestamp": "2026-09-23T13:41:09Z",
                "type": "agent_resumed",
                "data": {"harness_session_id": SID}
            }),
            json!({
                "timestamp": "2026-09-23T14:01:04Z",
                "type": "agent_send_started",
                "data": {"harness_session_id": SID, "verb": "target"}
            }),
        ]),
        repo_slug: None,
    }
}

fn transcript_fixture() -> (tempfile::TempDir, TranscriptFacts) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let records = [
        json!({"type":"assistant", "timestamp":"2026-09-23T06:00:00Z", "message":{"model":"glm-5.3-flash"}}),
        json!({"type":"permission-mode", "permissionMode":"bypassPermissions"}),
        json!({"type":"agent-name", "agentName":"history-worker"}),
        json!({"type":"assistant", "timestamp":"2026-09-23T06:36:29Z", "message":{"model":"glm-5.3-flash"}}),
        json!({"type":"assistant", "timestamp":"2026-09-23T14:01:16Z", "message":{"model":"claude-opus-5-5"}}),
        json!({"type":"permission-mode", "permissionMode":"auto"}),
    ];
    fs::write(
        &path,
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    (dir, scan(&path).unwrap())
}

#[test]
fn agents_history_card_joins_transcript_stages_and_events() {
    let mut sources = specimen_sources(None, "");
    sources.registry.as_mut().unwrap()[0]
        .as_object_mut()
        .unwrap()
        .remove("requested_model");
    let (_dir, transcript) = transcript_fixture();
    sources.graph.as_mut().unwrap()[0]["status"] = json!("done");
    sources.graph.as_mut().unwrap()[0]["details"] = json!("unused prose");
    sources.graph.as_mut().unwrap()[0]["progress_notes"] = json!([{"body": "unused note"}]);
    let lines = card(SID, &sources, Some(&transcript));
    for row in sources.graph.as_mut().unwrap() {
        row.as_object_mut()
            .unwrap()
            .retain(|key, _| sources::GRAPH_FIELDS.contains(&key.as_str()));
        assert!(row.get("details").is_none() && row.get("progress_notes").is_none());
    }
    assert_eq!(resolve("x-3344", &sources).sessions, [SID]);
    assert_eq!(card(SID, &sources, Some(&transcript)), lines);
    let card = lines.join("\n");

    assert!(card.contains(&format!("session:    {SID}")));
    assert!(card.contains("name:       worker\n"));
    assert!(!card.contains("spawned as worker"));
    assert!(card.contains("node:       x-3344  Session card fixture"));
    assert!(card.contains("harness:    claude"));
    assert!(card.contains("spawn glm-5.3-flash (first transcript turn)"));
    assert!(card.contains("observed claude-opus-5-5 (last turn 2026-09-23T14:01:16Z)"));
    assert!(card.contains("last glm-5.3-flash turn 2026-09-23T06:36:29Z, first claude-opus-5-5 turn 2026-09-23T14:01:16Z"));
    assert!(card.contains("between: adopt 2026-09-23T13:41:05Z, resume 2026-09-23T13:41:09Z, send 2026-09-23T14:01:04Z"));
    assert!(card.contains("stage:      do 2026-09-23T06:30:00Z -> 2026-09-23T07:00:00Z"));
    assert!(card.contains("effort xhigh merge grant approved (operator)"));
    assert!(card.contains("stage:      ship 2026-09-23T13:00:00Z -> open"));
    assert!(card.contains("permission: bypassPermissions -> auto (transcript)"));
    assert!(card.contains("event:      2026-09-23T00:48:20Z spawn worker"));
    assert!(card.contains("event:      2026-09-23T13:41:05Z adopt"));
    assert!(card.contains("event:      2026-09-23T13:41:09Z resume"));
}

#[test]
fn agents_history_unknown_provider_never_prints_a_plain_resume() {
    let sources = specimen_sources(None, "glm-5.3-flash");
    let lines = card(SID, &sources, None).join("\n");

    assert!(lines.contains("provider:   unknown (the registry row records no provider)"));
    assert!(lines.contains("resume:     unknown (the provider is not recorded, so a plain resume could run the account default)"));
    assert!(!lines.contains("claude --resume"));
}

#[test]
fn agents_history_route_tag_compares_normalized_model_but_resumes_exact_request() {
    let sources = specimen_sources(Some("zai"), "glm-5.3-flash[1m]");
    let (_dir, transcript) = transcript_fixture();
    let lines = card(SID, &sources, Some(&transcript)).join("\n");

    assert!(lines.contains("provider:   zai (registry row)"));
    assert!(lines.contains("route:      routes/zai.toml (registry row)"));
    assert!(!lines.contains("spawned asking for"));
    assert!(lines.contains(&format!(
        "resume:     fno agents spawn --resume {SID} -P zai -m 'glm-5.3-flash[1m]'"
    )));
}

#[test]
fn agents_history_routed_resume_quotes_apostrophes_in_model_ids() {
    let sources = specimen_sources(Some("zai"), "model'variant");
    let lines = card(SID, &sources, None).join("\n");

    assert!(lines.contains(&format!(
        "resume:     fno agents spawn --resume {SID} -P zai -m 'model'\"'\"'variant'"
    )));
}

#[test]
fn agents_history_spawn_model_fallback_reports_first_turn_mismatch() {
    let mut sources = specimen_sources(None, "");
    sources.registry.as_mut().unwrap()[0]["model"] = json!("glm-5.3-flash");
    let dir = tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    fs::write(
        &path,
        json!({
            "type": "assistant",
            "timestamp": "2026-09-23T14:01:16Z",
            "message": {"model": "claude-opus-5-5"}
        })
        .to_string(),
    )
    .unwrap();
    let transcript = scan(&path).unwrap();

    let lines = card(SID, &sources, Some(&transcript)).join("\n");
    assert!(lines.contains("spawn glm-5.3-flash (registry model)"));
    assert!(lines.contains(
        "spawned asking for glm-5.3-flash, first turn answered as claude-opus-5-5 at 2026-09-23T14:01:16Z"
    ));
}

#[test]
fn agents_history_receipts_resolve_short_and_row_names_and_render_verbatim_resume() {
    let mut sources = empty_sources();
    sources.receipts = Ok(vec![receipt(
        "receipts/first.json",
        json!({
            "harness_session_id": SID,
            "short_id": "d23c3d68",
            "row_name": "worker",
            "reaped_at": "2026-09-23T15:00:00Z",
            "resume": "custom resume --exact"
        }),
    )]);

    for handle in ["d23c3d68", "worker", SID] {
        let resolved = resolve(handle, &sources);
        assert_eq!(resolved.sessions, vec![SID.to_string()]);
    }
    let rendered = card(SID, &sources, None).join("\n");
    assert!(rendered.contains("receipt:    receipts/first.json, reaped 2026-09-23T15:00:00Z"));
    assert!(rendered.contains("resume:     custom resume --exact"));
}

#[test]
fn agents_history_session_handle_resolves_ledger_entry() {
    let mut sources = empty_sources();
    sources.ledger = Ok(vec![json!({
        "graph_node_id": "x-3344",
        "session_id": SID,
        "pr_number": 44,
        "status": "done"
    })]);

    let resolved = resolve(SID, &sources);
    assert_eq!(resolved.sessions, vec![SID.to_string()]);
    let rendered = card(SID, &sources, None).join("\n");
    assert!(rendered.contains("ledger:     x-3344 #44 done"));
}

#[test]
fn agents_history_empty_handle_never_matches_blank_receipt_fields() {
    let mut sources = empty_sources();
    sources.receipts = Ok(vec![receipt(
        "receipts/blank.json",
        json!({
            "harness_session_id": "",
            "short_id": "",
            "row_name": "",
            "resume": "must not be selected"
        }),
    )]);

    assert!(resolve("", &sources).sessions.is_empty());
    let lines = card(SID, &sources, None).join("\n");
    assert!(lines.contains("harness_session_id, short_id, row_name"));
    assert!(!lines.contains("must not be selected"));
}

#[test]
fn agents_history_node_resolves_receipt_enrichment_when_ledger_is_unreadable() {
    let mut sources = empty_sources();
    sources.ledger = Err("bad ledger JSON".into());
    sources.receipts = Ok(vec![receipt(
        "receipts/reaped.json",
        json!({
            "harness_session_id": SID,
            "reaped_at": "2026-09-23T15:00:00Z",
            "resume": "custom resume --exact",
            "ledger": {"graph_node_id": "x-9f2e"}
        }),
    )]);

    let resolved = resolve("x-9f2e", &sources);
    assert_eq!(resolved.sessions, vec![SID.to_string()]);
    let lines = card(SID, &sources, None).join("\n");
    assert!(lines.contains("receipts/reaped.json"));
    assert!(lines.contains("resume:     custom resume --exact"));
}

#[test]
fn agents_history_live_row_suppresses_stale_receipt_and_reused_names_sort_newest_first() {
    let mut sources = empty_sources();
    sources.registry = Ok(vec![json!({
        "harness_session_id": SID, "name": "worker", "harness": "claude", "status": "working"
    })]);
    sources.receipts = Ok(vec![
        receipt(
            "receipts/old.json",
            json!({
                "harness_session_id": OTHER_SID, "row_name": "worker", "reaped_at": "2026-09-22T00:00:00Z"
            }),
        ),
        receipt(
            "receipts/new.json",
            json!({
                "harness_session_id": "f0859112-6df4-4103-8451-99dd04b30d62", "row_name": "worker", "reaped_at": "2026-09-23T00:00:00Z"
            }),
        ),
        receipt(
            "receipts/stale.json",
            json!({
                "harness_session_id": SID, "row_name": "worker", "reaped_at": "2026-09-24T00:00:00Z"
            }),
        ),
    ]);

    let resolved = resolve("worker", &sources);
    assert_eq!(resolved.sessions, vec![SID.to_string()]);
    let live_card = card(SID, &sources, None).join("\n");
    assert!(!live_card.contains("receipts/stale.json"));
    assert!(live_card.contains("receipt:    not recorded (row is live; reap receipt suppressed)"));

    // The row's fno handle (the head of its fno_id) also finds the row.
    sources.registry = Ok(vec![json!({
        "harness_session_id": SID, "name": "worker", "harness": "claude", "status": "working",
        "fno_id": "9c1d2e3f-2222-4222-8222-222222222223"
    })]);
    let resolved = resolve("9c1d2e3f", &sources);
    assert_eq!(resolved.sessions, vec![SID.to_string()]);
    sources.registry = Ok(Vec::new());
    let resolved = resolve("worker", &sources);
    assert_eq!(resolved.sessions.len(), 3);
    let cards = resolved
        .sessions
        .iter()
        .map(|sid| card(sid, &sources, None).join("\n"))
        .collect::<Vec<_>>();
    assert!(cards[0].contains("receipts/stale.json"));
    assert!(cards[1].contains("receipts/new.json"));
    assert!(cards[2].contains("receipts/old.json"));
}

#[test]
fn agents_history_miss_names_every_source_and_returns_one() {
    let dir = tempdir().unwrap();
    let args = vec![
        OsString::from("never-seen"),
        OsString::from("--graph"),
        dir.path().join("unavailable-graph-store").into_os_string(),
        OsString::from("--ledger"),
        dir.path().join("ledger.json").into_os_string(),
        OsString::from("--events"),
        dir.path().join("events.jsonl").into_os_string(),
        OsString::from("--agents-home"),
        dir.path().join("agents").into_os_string(),
    ];
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    assert_eq!(run_to(&args, &mut stdout, &mut stderr), 1);
    let stderr = String::from_utf8(stderr).unwrap();
    for source in ["registry", "reap receipts", "graph", "ledger", "event log"] {
        assert!(stderr.contains(source), "missing {source} in {stderr}");
    }
    assert!(stderr.contains("harness_session_id, short_id, row_name"));
}

#[test]
fn agents_history_node_resolution_joins_graph_and_ledger_and_keeps_ledger_only_rows() {
    let mut sources = empty_sources();
    sources.graph = Ok(vec![json!({
        "id": "x-3344", "title": "Node title", "pr_number": 44,
        "sessions": [{"session_id": SID, "phase": "do"}]
    })]);
    sources.ledger = Ok(vec![
        json!({"graph_node_id":"x-3344", "session_id": SID, "pr_number":44, "status":"done"}),
        json!({"graph_node_id":"x-3344", "pr_number":44, "status":"done", "completed_at":"2026-09-23T12:00:00Z"}),
    ]);

    let resolved = resolve("x-3344", &sources);
    assert_eq!(resolved.sessions, vec![SID.to_string()]);
    assert_eq!(resolved.ledger_only.len(), 1);
    let card = card(SID, &sources, None).join("\n");
    assert!(card.contains("ledger:     x-3344 #44 done"));
    assert!(card.contains("node:       x-3344  Node title"));
}

#[test]
fn agents_history_resolves_dashless_node_ids_in_either_spelling() {
    for (stored, query) in [("x3344", "x-3344"), ("x-3344", "x3344")] {
        let mut sources = empty_sources();
        sources.graph = Ok(vec![json!({
            "id": stored,
            "title": "Node title",
            "sessions": [{"session_id": SID, "phase": "do"}]
        })]);

        assert_eq!(resolve(query, &sources).sessions, vec![SID.to_string()]);
    }

    let mut sources = empty_sources();
    sources.graph = Ok(vec![
        json!({"id": "x-3344", "sessions": [{"session_id": SID}]}),
        json!({"id": "x3344", "sessions": [{"session_id": OTHER_SID}]}),
    ]);
    assert_eq!(resolve("x-3344", &sources).sessions, vec![SID.to_string()]);
}

#[test]
fn agents_history_corrupt_session_elements_do_not_hide_valid_siblings() {
    let mut sources = empty_sources();
    sources.graph = Ok(vec![json!({
        "id": "x-3344",
        "title": "Node title",
        "sessions": [{"bad": 1}, {"session_id": SID, "phase": "do"}]
    })]);
    sources.ledger = Ok(vec![json!({
        "graph_node_id": "x-3344",
        "sessions": [{"bad": 1}, SID],
        "status": "done"
    })]);

    let resolved = resolve("x-3344", &sources);
    assert_eq!(resolved.sessions, vec![SID.to_string()]);
    let rendered = card(SID, &sources, None).join("\n");
    assert!(rendered.contains(&format!("session:    {SID}")));
    assert!(rendered.contains("node:       x-3344  Node title"));
}

#[test]
fn agents_history_legacy_node_and_unrecoverable_rows_remain_distinct() {
    let mut sources = empty_sources();
    sources.ledger = Ok(vec![
        json!({"node":"x-3344", "pr_number":44, "status":"done"}),
        json!({"graph_node_id":"x-3344", "node_id_unrecoverable":true, "status":"done"}),
    ]);

    let resolved = resolve("x-3344", &sources);
    assert!(resolved.sessions.is_empty());
    assert_eq!(resolved.ledger_only.len(), 2);
    assert_eq!(resolved.ledger_only[0]["node"], "x-3344");
    assert_eq!(resolved.ledger_only[1]["node_id_unrecoverable"], true);
}

#[test]
fn agents_history_node_prints_ledger_only_row_and_missing_session_reason() {
    let dir = tempdir().unwrap();
    let ledger = dir.path().join("ledger.json");
    let agents_home = dir.path().join("agents");
    fs::create_dir_all(&agents_home).unwrap();
    fs::write(
        &ledger,
        json!({
            "entries": [
                {
                    "graph_node_id": "x-3344",
                    "pr_number": 44,
                    "status": "done",
                    "completed": "2026-09-23T12:00:00Z"
                },
                {
                    "graph_node_id": "x-9f2e",
                    "pr_number": 45,
                    "status": "done",
                    "sessions": [LEDGER_SESSION_UNRESOLVED]
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let run = |arg: &str| {
        let args = vec![
            OsString::from(arg),
            OsString::from("--graph"),
            dir.path().join("graph.db").into_os_string(),
            OsString::from("--ledger"),
            ledger.clone().into_os_string(),
            OsString::from("--events"),
            dir.path().join("events.jsonl").into_os_string(),
            OsString::from("--agents-home"),
            agents_home.clone().into_os_string(),
        ];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_to(&args, &mut stdout, &mut stderr);
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    };

    let (code, stdout, _) = run("x-3344");
    assert_eq!(code, 0);
    assert!(stdout.contains("ledger:     x-3344 #44 done"));
    assert!(stdout.contains(
        "session: not recorded (ledger uuid coverage is write-path only; this row predates it)"
    ));

    let (code, stdout, _) = run("x-9f2e");
    assert_eq!(code, 0);
    assert!(stdout.contains("ledger:     x-9f2e #45 done"));
    assert!(stdout.contains("session: no resume handle was recorded for this run"));
    assert!(!stdout.contains("session: not recorded (ledger uuid coverage"));
}

#[test]
fn agents_history_pr_resolution_filters_repo_slug_and_keeps_both_when_unresolved() {
    let mut sources = empty_sources();
    sources.graph = Ok(vec![
        json!({"id":"x-3344", "pr_number":44, "pr_url":"https://github.com/acme/one/pull/44", "sessions":[{"session_id":SID}]}),
        json!({"id":"x-9f2e", "pr_number":44, "pr_url":"https://github.com/acme/two/pull/44", "sessions":[{"session_id":OTHER_SID}]}),
    ]);
    sources.ledger = Ok(vec![
        json!({
            "graph_node_id": "x-3344",
            "pr_number": 44,
            "pr_url": "https://github.com/acme/one/pull/44",
            "sessions": [SID],
            "status": "done"
        }),
        json!({
            "graph_node_id": "x-9f2e",
            "pr_number": 44,
            "pr_url": "https://github.com/acme/two/pull/44",
            "sessions": [OTHER_SID],
            "status": "done"
        }),
    ]);
    sources.repo_slug = Some("acme/one".to_string());

    assert_eq!(resolve("44", &sources).sessions, vec![SID.to_string()]);
    sources.repo_slug = None;
    let resolved = resolve("#44", &sources);
    assert_eq!(resolved.sessions.len(), 2);
    assert!(resolved.repo_slug_unresolved);

    sources.ledger = Ok(Vec::new());
    for row in sources.graph.as_mut().unwrap() {
        row["status"] = json!("done");
        row["details"] = json!("unused prose");
        row.as_object_mut()
            .unwrap()
            .retain(|key, _| sources::GRAPH_FIELDS.contains(&key.as_str()));
    }
    sources.repo_slug = Some("acme/one".to_string());
    assert_eq!(resolve("44", &sources).sessions, vec![SID.to_string()]);
    sources.repo_slug = None;
    assert_eq!(resolve("#44", &sources).sessions.len(), 2);
}

#[test]
fn agents_history_pr_lookup_prints_warning_when_repo_slug_is_missing() {
    let dir = tempdir().unwrap();
    let agents_home = dir.path().join("agents");
    fs::create_dir_all(&agents_home).unwrap();
    let ledger = dir.path().join("ledger.json");
    fs::write(
        &ledger,
        json!({
            "entries": [
                {
                    "graph_node_id": "x-3344",
                    "pr_number": 44,
                    "pr_url": "https://github.com/acme/one/pull/44",
                    "status": "done"
                },
                {
                    "graph_node_id": "x-9f2e",
                    "pr_number": 44,
                    "pr_url": "https://github.com/acme/two/pull/44",
                    "status": "done"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let args = vec![
        OsString::from("44"),
        OsString::from("--graph"),
        dir.path().join("graph.db").into_os_string(),
        OsString::from("--ledger"),
        ledger.into_os_string(),
        OsString::from("--events"),
        dir.path().join("events.jsonl").into_os_string(),
        OsString::from("--agents-home"),
        agents_home.into_os_string(),
    ];
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    assert_eq!(run_to(&args, &mut stdout, &mut stderr), 0);
    let stdout = String::from_utf8(stdout).unwrap();
    assert_eq!(
        stdout
            .matches("repo slug unresolved; PR numbers collide across repos")
            .count(),
        1
    );
    assert!(stdout.contains("ledger:     x-3344 #44 done"));
    assert!(stdout.contains("ledger:     x-9f2e #44 done"));
}
