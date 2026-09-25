use fno_agents::acp_stdio::{AcpSession, GROK_PROFILE};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn notification_before_response_is_kept_and_response_is_correlated() {
    let dir = tempfile::tempdir().unwrap();
    let session = AcpSession::start(
        &GROK_PROFILE,
        vec![
            "sh".into(),
            "-c".into(),
            concat!(
                "read -r request; ",
                "printf '%s\\n' ",
                "'{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{}}' ",
                "'{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ready\":true}}'"
            )
            .into(),
        ],
        dir.path(),
        None,
    )
    .unwrap();

    let response = session.request("initialize", json!({})).unwrap();

    assert_eq!(response["result"]["ready"], true);
    assert_eq!(session.notifications()[0]["method"], "session/update");
}

#[test]
fn cancellation_handle_is_cloneable_and_thread_safe() {
    fn assert_clone_send_sync<T: Clone + Send + Sync>() {}
    assert_clone_send_sync::<fno_agents::acp_stdio::AcpCancelHandle>();
}

#[test]
fn harness_argv_builders_keep_provider_specific_contracts() {
    use fno_agents::acp_stdio::{dsh_acp_argv, grok_acp_argv, kimi_acp_argv};

    assert_eq!(
        grok_acp_argv(None, None, None),
        vec![
            "grok",
            "agent",
            "-m",
            "grok-4.6",
            "--reasoning-effort",
            "high",
            "stdio"
        ]
    );
    assert_eq!(
        grok_acp_argv(
            Some("grok-4.5"),
            Some("medium"),
            Some(Path::new("/tmp/plugin"))
        ),
        vec![
            "grok",
            "agent",
            "--plugin-dir",
            "/tmp/plugin",
            "-m",
            "grok-4.5",
            "--reasoning-effort",
            "medium",
            "stdio"
        ]
    );
    assert_eq!(kimi_acp_argv(None), vec!["kimi", "acp"]);
    assert_eq!(
        kimi_acp_argv(Some("sonnet")),
        vec!["kimi", "acp", "--model", "sonnet"]
    );
    assert_eq!(dsh_acp_argv(), vec!["dsh", "--profile", "acp"]);
}

#[test]
fn initialize_and_session_new_params_match_acp_contract() {
    use fno_agents::acp_stdio::{initialize_params, session_new_params};

    let dir = tempfile::tempdir().unwrap();
    let grant = dir.path().join("writable");
    assert_eq!(
        initialize_params(),
        json!({
            "protocolVersion": 1,
            "clientInfo": {"name":"fno", "version":"0.1.0"},
            "clientCapabilities": {},
        })
    );
    assert_eq!(
        session_new_params(dir.path(), &[]),
        json!({"cwd":dir.path(), "mcpServers":[]})
    );
    assert_eq!(
        session_new_params(dir.path(), &[grant.clone()]),
        json!({"cwd":dir.path(), "mcpServers":[], "additionalDirectories":[grant]})
    );
}

#[test]
fn initialize_and_session_list_enforce_each_profile_marker() {
    use fno_agents::acp_stdio::{AcpProfile, DSH_PROFILE, KIMI_PROFILE};

    for profile in [&GROK_PROFILE, &KIMI_PROFILE, &DSH_PROFILE] {
        let dir = tempfile::tempdir().unwrap();
        let agent = profile.agent_name.unwrap_or("Grok");
        let script = format!(
            "read -r initialize; printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":1,\"agentInfo\":{{\"name\":{agent:?},\"version\":\"1\"}}}}}}'; read -r list; printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessions\":[],\"nextCursor\":null}}}}'"
        );
        let profile: &'static AcpProfile = profile;
        let session = AcpSession::start(
            profile,
            vec!["sh".into(), "-c".into(), script],
            dir.path(),
            None,
        )
        .unwrap();

        assert_eq!(session.initialize().unwrap()["protocolVersion"], 1);
        assert!(session.session_list().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}

#[test]
fn permission_refusal_answers_cancelled_and_fails_loud() {
    use fno_agents::acp_stdio::{AcpError, PermissionPolicy};

    let dir = tempfile::tempdir().unwrap();
    let session = AcpSession::start_with_policy(
        &GROK_PROFILE,
        vec!["sh".into(), "-c".into(), concat!(
            "read -r request; ",
            "printf '%s\\n' ",
            "'{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"session/request_permission\",\"params\":{\"toolCall\":{\"title\":\"write probe.txt\"},\"options\":[{\"kind\":\"allow_once\",\"optionId\":\"allow\"}]}}'; ",
            "read -r answer; printf '%s\\n' \"$answer\" >&2; ",
            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"stopReason\":\"end_turn\"}}'"
        ).into()],
        dir.path(),
        None,
        PermissionPolicy::Refuse,
    ).unwrap();

    let error = session.request("session/prompt", json!({})).unwrap_err();
    assert!(matches!(&error, AcpError::PermissionRefused { .. }));
    assert!(error.to_string().contains("write probe.txt"));
    drop(session);
}

#[test]
fn permission_allow_once_selects_the_matching_option_id() {
    use fno_agents::acp_stdio::PermissionPolicy;

    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("answer.json");
    let script = concat!(
        "read -r request; ",
        "printf '%s\\n' ",
        "'{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"session/request_permission\",\"params\":{\"toolCall\":{\"title\":\"write probe.txt\"},\"options\":[{\"kind\":\"allow_once\",\"optionId\":\"allow-7\"}]}}'; ",
        "read -r answer; printf '%s\\n' \"$answer\" > \"$ACP_CAPTURE\"; ",
        "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"stopReason\":\"end_turn\"}}'"
    );
    let env = HashMap::from([(
        "ACP_CAPTURE".to_string(),
        capture.to_string_lossy().into_owned(),
    )]);
    let session = AcpSession::start_with_policy(
        &GROK_PROFILE,
        vec!["sh".into(), "-c".into(), script.into()],
        dir.path(),
        Some(env),
        PermissionPolicy::AllowOnce,
    )
    .unwrap();

    let response = session.request("session/prompt", json!({})).unwrap();

    assert_eq!(response["result"]["stopReason"], "end_turn");
    let answer: serde_json::Value =
        serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow-7");
}

#[test]
fn cancel_handle_sends_session_cancel_and_prompt_returns_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("cancel.json");
    let script = concat!(
        "read -r create; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"sessionId\":\"minted\"}}'; ",
        "read -r prompt; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{}}'; ",
        "read -r cancel; printf '%s\\n' \"$cancel\" > \"$ACP_CAPTURE\"; ",
        "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"stopReason\":\"cancelled\"}}'"
    );
    let env = HashMap::from([(
        "ACP_CAPTURE".to_string(),
        capture.to_string_lossy().into_owned(),
    )]);
    let session = Arc::new(
        AcpSession::start(
            &GROK_PROFILE,
            vec!["sh".into(), "-c".into(), script.into()],
            dir.path(),
            Some(env),
        )
        .unwrap(),
    );
    assert_eq!(
        session
            .session_new(json!({"cwd":"/tmp", "mcpServers":[]}))
            .unwrap(),
        "minted"
    );
    let cancel = session.cancel_handle().unwrap();
    let prompt_session = Arc::clone(&session);
    let prompt_thread = thread::spawn(move || prompt_session.prompt("wait until cancelled"));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !session.notifications().iter().any(|item| {
        item.get("method").and_then(serde_json::Value::as_str) == Some("session/update")
    }) && std::time::Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    let saw_update = session.notifications().iter().any(|item| {
        item.get("method").and_then(serde_json::Value::as_str) == Some("session/update")
    });
    cancel.cancel().unwrap();
    let result = prompt_thread.join().unwrap().unwrap();

    assert!(saw_update);
    assert_eq!(result["stopReason"], "cancelled");
    let message: serde_json::Value =
        serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(message["method"], "session/cancel");
    assert_eq!(message["params"]["sessionId"], "minted");
}

#[test]
fn auth_errors_keep_typed_exit_code_and_profile_refusal() {
    use fno_agents::acp_stdio::AcpError;

    let dir = tempfile::tempdir().unwrap();
    let session = AcpSession::start(
        &GROK_PROFILE,
        vec!["sh".into(), "-c".into(), "exit 0".into()],
        dir.path(),
        None,
    )
    .unwrap();
    let error = session
        .result(
            json!({"error":{"code":-32000,"message":"You are not authenticated"}}),
            "session/new",
        )
        .unwrap_err();

    assert!(matches!(&error, AcpError::AuthRequired { .. }));
    assert_eq!(error.exit_code(), 13);
    assert!(error.to_string().contains("grok login --device-code"));
}

#[test]
fn silent_child_returns_a_bounded_read_timeout() {
    use fno_agents::acp_stdio::{AcpError, AcpProfile};

    static SHORT: AcpProfile = AcpProfile {
        tool: "fake",
        agent_name: None,
        auth_markers: &[],
        auth_refusal: "auth required",
        request_timeout: Duration::from_secs(1),
        list_params_empty: false,
        stderr_settle: Duration::ZERO,
    };
    let dir = tempfile::tempdir().unwrap();
    let session = AcpSession::start(
        &SHORT,
        vec!["sh".into(), "-c".into(), "read -r request; sleep 2".into()],
        dir.path(),
        None,
    )
    .unwrap();
    let started = std::time::Instant::now();

    let error = session.request("initialize", json!({})).unwrap_err();

    assert!(matches!(&error, AcpError::ReadTimeout { .. }));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn writing_after_child_exit_reports_the_exit_code_and_stderr() {
    use fno_agents::acp_stdio::AcpError;

    let dir = tempfile::tempdir().unwrap();
    let session = AcpSession::start(
        &GROK_PROFILE,
        vec![
            "sh".into(),
            "-c".into(),
            "printf 'dead child\\n' >&2; exit 7".into(),
        ],
        dir.path(),
        None,
    )
    .unwrap();
    thread::sleep(Duration::from_millis(50));

    let error = session.request("initialize", json!({})).unwrap_err();

    assert!(matches!(&error, AcpError::BrokenPipe { .. }));
    assert_eq!(error.exit_code(), 7);
    assert!(error.to_string().contains("dead child"));
}

#[test]
fn kimi_auth_refusal_carries_the_stderr_diagnostic() {
    use fno_agents::acp_stdio::{AcpError, KIMI_PROFILE};

    let dir = tempfile::tempdir().unwrap();
    let script = concat!(
        "read -r request; ",
        "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"Authentication required\"}}'; ",
        "printf '%s\\n' 'no provider configured; complete onboarding via /login' >&2; sleep 0.1"
    );
    let session = AcpSession::start(
        &KIMI_PROFILE,
        vec!["sh".into(), "-c".into(), script.into()],
        dir.path(),
        None,
    )
    .unwrap();
    let response = session.request("session/new", json!({})).unwrap();

    let error = session.result(response, "session/new").unwrap_err();
    assert!(matches!(&error, AcpError::AuthRequired { .. }));
    assert_eq!(error.exit_code(), 13);
    assert!(error.to_string().contains("no provider configured"));
}

#[test]
fn dsh_missing_provider_key_maps_to_its_typed_credential_refusal() {
    use fno_agents::acp_stdio::{AcpError, DSH_PROFILE};

    let dir = tempfile::tempdir().unwrap();
    let diagnostic = include_str!("fixtures/dsh-acp-trials.txt")
        .lines()
        .find_map(|line| line.strip_prefix("prompt_without_key.text="))
        .expect("measured DSH authentication diagnostic");
    let frame = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32603, "message": diagnostic},
    })
    .to_string();
    let script = "read -r request; printf '%s\\n' \"$FAKE_RESPONSE\"";
    let env = HashMap::from([("FAKE_RESPONSE".to_string(), frame)]);
    let session = AcpSession::start(
        &DSH_PROFILE,
        vec!["sh".into(), "-c".into(), script.into()],
        dir.path(),
        Some(env),
    )
    .unwrap();
    let response = session.request("session/prompt", json!({})).unwrap();

    let error = session.result(response, "session/prompt").unwrap_err();
    assert!(matches!(&error, AcpError::AuthRequired { .. }));
    assert_eq!(error.exit_code(), 13);
    assert!(error.to_string().contains("DEEPSEEK_API_KEY"));
    assert!(error.to_string().contains("no API key for provider route"));
}

#[test]
fn unknown_server_request_is_answered_and_rejected() {
    use fno_agents::acp_stdio::AcpError;

    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("answer.json");
    let script = concat!(
        "read -r request; ",
        "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"session/unknown\",\"params\":{}}'; ",
        "read -r answer; printf '%s\\n' \"$answer\" > \"$ACP_CAPTURE\""
    );
    let env = HashMap::from([(
        "ACP_CAPTURE".to_string(),
        capture.to_string_lossy().into_owned(),
    )]);
    let session = AcpSession::start(
        &GROK_PROFILE,
        vec!["sh".into(), "-c".into(), script.into()],
        dir.path(),
        Some(env),
    )
    .unwrap();

    let error = session.request("initialize", json!({})).unwrap_err();

    assert!(matches!(&error, AcpError::ServerRequest { .. }));
    let answer: serde_json::Value =
        serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(answer["error"]["code"], -32601);
}
