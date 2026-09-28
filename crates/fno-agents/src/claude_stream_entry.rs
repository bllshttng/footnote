//! Build the registry row and child argv for a claude stream-json adoption
//! lane: birth construction with request-carried lineage, and the resume pin
//! the adopt door owes its session.

use std::path::PathBuf;

use crate::daemon::now_rfc3339_like;
use crate::state::{Lineage, Registry, RegistryEntry};
use crate::AgentStatus;

/// The worker argv for the claude stream-json lane (everything after the worker
/// BINARY path). `parse_stream_args` in bin/worker.rs accepts these flags in any
/// order before `--`; the child argv (normally
/// [`crate::provider::claude_stream_json_resume_argv`]) follows the separator.
/// Pure so the flag wiring is unit-testable without spawning a process.
pub(crate) fn claude_stream_worker_args(
    short_id: &str,
    home: &std::path::Path,
    cwd: &std::path::Path,
    uuid: &str,
    holder: &str,
    child_argv: &[String],
) -> Vec<String> {
    let mut args = vec![
        "--stream".into(),
        "--short-id".into(),
        short_id.into(),
        "--home".into(),
        home.to_string_lossy().into_owned(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--session-uuid".into(),
        uuid.into(),
        "--holder".into(),
        holder.into(),
        "--".into(),
    ];
    args.extend(child_argv.iter().cloned());
    args
}

/// The adopt lane's child argv for `uuid`, or the refusal when the lane cannot
/// carry what the session launched on: a recorded route, a non-default
/// account, or a model no default-endpoint resume serves.
pub(crate) fn stream_child_argv(uuid: &str, registry: &Registry) -> Result<Vec<String>, String> {
    let lookup: crate::resume_pin::RouteProviderOf<'_> =
        &|m| crate::claude_adopt::provider_from_route_settings(m);
    let build = |pin: crate::resume_pin::Pin| {
        let mut argv = crate::provider::claude_stream_json_resume_argv(uuid);
        crate::resume_pin::append_axes(&mut argv, pin.argv_model.as_deref(), pin.effort.as_deref());
        argv
    };
    let row = registry.entries.iter().rev().find(|e| {
        e.harness_name() == "claude"
            && (e.claude_session_uuid.as_deref() == Some(uuid)
                || e.harness_session_id.as_deref() == Some(uuid))
    });
    if let Some(row) = row {
        if row
            .route_settings_path
            .as_deref()
            .is_some_and(|p| !p.is_empty())
        {
            return Err(format!(
                "{} runs on a recorded route that the adopt lane cannot carry; \
                 resume it with fno agents resume {}, which restores the route",
                row.name, row.name
            ));
        }
        if let Some(account) = row
            .launch_account
            .as_deref()
            .filter(|a| !a.is_empty() && *a != "default")
        {
            return Err(format!(
                "{} launched on account {}, which the adopt lane cannot carry; \
                 resume it with fno agents resume {}",
                row.name, account, row.name
            ));
        }
        return crate::resume_pin::resolve(
            Some(crate::resume_pin::RowPins::from_entry(row)),
            crate::claude_drive::find_transcript(uuid).as_deref(),
            false,
            uuid,
            lookup,
        )
        .map(build)
        .map_err(|u| u.text);
    }
    crate::resume_pin::resolve(
        None,
        crate::claude_drive::find_transcript(uuid).as_deref(),
        false,
        uuid,
        lookup,
    )
    .map(build)
    .map_err(|u| u.text)
}

pub(crate) fn build_claude_stream_entry(
    name: &str,
    short_id: &str,
    cwd: &std::path::Path,
    uuid: &str,
    pid: u32,
    pid_start_time: Option<u64>,
    log_path: PathBuf,
    node: Option<&str>,
    spawn_params: &serde_json::Value,
    provenance: Option<&crate::spawn_contract::SpawnProvenance>,
) -> RegistryEntry {
    let cwd_s = cwd.to_string_lossy().into_owned();
    // The daemon's env is scrubbed, so the parent edge rides the spawn
    // REQUEST the client stamped from its own ambient markers. A door-proved
    // origin outranks the raw edge; an edge-less request stamps the reason
    // instead of a silent null.
    let spawned_by = match provenance {
        Some(p) => match crate::spawn_contract::compatibility_parent(&p.origin) {
            (Some(session), Some(harness), Some(cwd)) => {
                Lineage::captured((Some(session), Some(harness), Some(cwd)))
            }
            _ => Lineage::from_request(spawn_params),
        },
        None => Lineage::from_request(spawn_params),
    };
    let (launch_account, launch_account_source) = crate::state::launch_provenance_from_env();
    let mut entry = RegistryEntry {
        // The node this spawn was FOR, from the spawn request - never the
        // daemon's ambient env, which names the daemon-starting session.
        node: node.filter(|v| !v.is_empty()).map(str::to_string),
        // The sibling receipt: the seed named a node the seam could
        // not resolve. Absent when the node resolved or none was named.
        node_reason: spawn_params
            .get("node_reason")
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
        // Stream-json adoption is gated on host_mode plus mode, not on a
        // substrate, and it is not one of the three names - this row's
        // lifecycle belongs to chat/switchboard/ask, so the axis stays
        // unknown rather than forcing a "thread" stamp.
        substrate: None,
        name: name.into(),
        short_id: short_id.into(),
        // Birth marker: the daemon started this PTY worker itself. An absent origin means UNKNOWN,
        // and the watchdog's retire lane never acts on unknown.
        origin: Some("spawn".to_string()),
        legacy_provider: String::new(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        // v23: adoption - the daemon observed no spawn request, so
        // the requested axis stays unknown rather than a guess.
        requested_model: None,
        requested_provider: None,
        requested_effort: None,
        harness: Some("claude".into()),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        // the daemon env is what this claude child inherits, so the
        // three-valued env read is honest (ambient config dir = unknown).
        launch_account: launch_account.clone(),
        launch_account_source,
        related_session_id: None,
        // v25: the vendor route is unobserved on this lane (it may be routed,
        // and `provider` above stays None for the same reason), so it stays
        // unknown here rather than guessing "anthropic". The account record
        // mirrors the launch read - unknown stays unknown.
        route_provider_id: None,
        model_name: None,
        account_record_id: launch_account,
        cwd: cwd_s.clone(),
        project_root: cwd_s,
        session_id: None,
        spawn_trigger: None,
        legacy_claude_short_id: None,
        claude_session_uuid: Some(uuid.into()),
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: Some(crate::state::HOST_MODE_INTERACTIVE.into()),
        status: AgentStatus::Live,
        last_message_at: Some(now_rfc3339_like()),
        created_at: now_rfc3339_like(),
        pid: Some(pid),
        pid_start_time,
        keeper_child_pid: None,
        log_path: Some(log_path.to_string_lossy().into_owned()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
        sandbox_posture: None,
        ..RegistryEntry::new(Some(uuid.into()), spawned_by)
    };
    if let Some(p) = provenance {
        // The door's record rides the row; the parent edge above is the
        // compatibility projection of the same origin.
        entry.spawn_id = Some(crate::spawn_transaction::allocate_spawn_id());
        entry.spawn_provenance = Some(p.clone());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The moved stream-lane worker argv: selector + claim pair present, the
    /// child argv follows `--`, the resume target is the FULL uuid.
    #[test]
    fn claude_stream_worker_args_carry_stream_flags_and_child_argv() {
        let child = crate::provider::claude_stream_json_resume_argv("U-9");
        let args = claude_stream_worker_args(
            "sw9",
            std::path::Path::new("/home/agents"),
            std::path::Path::new("/work"),
            "U-9",
            "stream:sw9",
            &child,
        );
        assert!(args.contains(&"--stream".to_string()));
        assert_eq!(
            args.iter()
                .position(|a| a == "--session-uuid")
                .map(|i| &args[i + 1]),
            Some(&"U-9".to_string())
        );
        assert_eq!(
            args.iter()
                .position(|a| a == "--holder")
                .map(|i| &args[i + 1]),
            Some(&"stream:sw9".to_string())
        );
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("missing -- separator");
        assert_eq!(&args[sep + 1..], child.as_slice());
        assert_eq!(child[0], "claude");
        assert!(child.contains(&"--resume".to_string()) && child.contains(&"U-9".to_string()));
    }

    /// The node receipt is three-state: a request naming a node binds it
    /// (reason absent); one carrying node_reason stamps the receipt with the
    /// node absent; a request naming no node stays silent on both.
    #[test]
    fn node_reason_receipt_is_three_state() {
        let log = std::path::PathBuf::from("/proj/.fno/agents/swR/timeline.jsonl");
        let bound = build_claude_stream_entry(
            "bound",
            "swR",
            std::path::Path::new("/proj"),
            "UUID-BOUND",
            1,
            None,
            log.clone(),
            Some("x-aaaa"),
            &serde_json::json!({"node": "x-aaaa"}),
            None,
        );
        let receipt = build_claude_stream_entry(
            "receipt",
            "swR2",
            std::path::Path::new("/proj"),
            "UUID-RECEIPT",
            1,
            None,
            log.clone(),
            None,
            &serde_json::json!({"node_reason": "x-gone names no readable row (derived from the seed)"}),
            None,
        );
        let silent = build_claude_stream_entry(
            "silent",
            "swR3",
            std::path::Path::new("/proj"),
            "UUID-SILENT",
            1,
            None,
            log,
            None,
            &serde_json::Value::Null,
            None,
        );
        assert_eq!(bound.node.as_deref(), Some("x-aaaa"));
        assert_eq!(bound.node_reason, None);
        assert_eq!(receipt.node, None);
        assert!(receipt
            .node_reason
            .as_deref()
            .unwrap()
            .starts_with("x-gone names no readable row"));
        assert_eq!(silent.node, None);
        assert_eq!(silent.node_reason, None);
        // Serialized rows keep the states distinguishable: skip-if-none drops
        // node_reason when absent and keeps it when stamped.
        let bound_json = serde_json::to_value(&bound).unwrap();
        let receipt_json = serde_json::to_value(&receipt).unwrap();
        let silent_json = serde_json::to_value(&silent).unwrap();
        assert!(bound_json.get("node_reason").is_none());
        assert!(receipt_json.get("node_reason").is_some());
        assert!(silent_json.get("node_reason").is_none());
    }

    fn claude_row(name: &str, uuid: &str) -> RegistryEntry {
        RegistryEntry {
            harness: Some("claude".into()),
            name: name.into(),
            short_id: "sw000".into(),
            claude_session_uuid: Some(uuid.into()),
            host_mode: Some(crate::state::HOST_MODE_INTERACTIVE.into()),
            status: AgentStatus::Exited,
            created_at: "2026-09-21T00:00:00Z".into(),
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            ..Default::default()
        }
    }

    fn hermetic_routes_and_projects(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "cse-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let routes = base.join("routes");
        let projects = base.join("projects").join("-tmp-proj");
        std::fs::create_dir_all(&routes).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        (routes, projects)
    }

    #[test]
    fn routed_row_refuses_naming_the_resume_door() {
        let mut reg = Registry::default();
        reg.entries.push(claude_row("first", "uuid-routed-1"));
        reg.entries[0].route_settings_path = Some("/tmp/x20ac-route.json".into());
        let err = stream_child_argv("uuid-routed-1", &reg).unwrap_err();
        assert!(err.contains("recorded route"), "{err}");
        assert!(err.contains("fno agents resume first"), "{err}");
    }

    #[test]
    fn non_default_account_row_refuses_naming_the_resume_door() {
        let mut reg = Registry::default();
        let mut row = claude_row("acct", "uuid-acct-1");
        row.launch_account = Some("makers".into());
        reg.entries.push(row);
        let err = stream_child_argv("uuid-acct-1", &reg).unwrap_err();
        assert!(err.contains("account makers"), "{err}");
        assert!(err.contains("fno agents resume acct"), "{err}");
    }

    #[test]
    fn unrouted_row_pins_the_recorded_model() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (routes, _projects) = hermetic_routes_and_projects("unrouted-row");
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &routes);
        let mut reg = Registry::default();
        let mut row = claude_row("opus", "uuid-opus-1");
        row.requested_model = Some("claude-opus-5".into());
        reg.entries.push(row);

        let argv = stream_child_argv("uuid-opus-1", &reg).unwrap();
        let base = crate::provider::claude_stream_json_resume_argv("uuid-opus-1");
        assert_eq!(
            argv,
            [base, vec!["--model".into(), "claude-opus-5".into()]].concat()
        );
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(routes.parent().unwrap()).ok();
    }

    #[test]
    fn rowless_glm_transcript_refuses_with_the_route_remedy() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (routes, projects) = hermetic_routes_and_projects("rowless-glm");
        std::fs::write(
            routes.join("zai-glm.json"),
            r#"{"env": {"ANTHROPIC_MODEL": "glm-5.3-flash[1m]", "FNO_ROUTE_PROVIDER": "zai"}}"#,
        )
        .unwrap();
        std::fs::write(
            projects.join("a1b2c3d4-1111-4111-8111-111111111111.jsonl"),
            r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"glm-5.3-flash[1m]","marketingName":null}}}"#,
        )
        .unwrap();
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &routes);
        std::env::set_var(
            crate::claude_drive::PROJECTS_DIR_ENV,
            projects.parent().unwrap(),
        );

        let reg = Registry::default();
        let err = stream_child_argv("a1b2c3d4-1111-4111-8111-111111111111", &reg).unwrap_err();
        assert!(err.contains("-P zai -m 'glm-5.3-flash[1m]'"), "{err}");
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(routes.parent().unwrap()).ok();
    }

    #[test]
    fn rowless_anthropic_transcript_pins_the_birth_model() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (routes, projects) = hermetic_routes_and_projects("rowless-opus");
        std::fs::write(
            projects.join("a1b2c3d4-1111-4222-8222-222222222222.jsonl"),
            r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"claude-opus-5","marketingName":"Opus 5"}}}"#,
        )
        .unwrap();
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &routes);
        std::env::set_var(
            crate::claude_drive::PROJECTS_DIR_ENV,
            projects.parent().unwrap(),
        );

        let reg = Registry::default();
        let argv = stream_child_argv("a1b2c3d4-1111-4222-8222-222222222222", &reg).unwrap();
        assert!(argv.contains(&"--model".to_string()));
        assert!(argv.contains(&"claude-opus-5".to_string()));
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(routes.parent().unwrap()).ok();
    }
}
