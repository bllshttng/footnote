//! Build the registry row for a Codex app-server thread.

use std::path::Path;

use crate::daemon::now_rfc3339_like;
use crate::state::RegistryEntry;
use crate::AgentStatus;

/// Build the registry row for a Codex app-server thread. Codex has no fno
/// short id: the full harness session id is both the resume handle and the
/// canonical registry identity.
pub(crate) fn build_codex_thread_entry(
    name: &str,
    cwd: &Path,
    driver: &crate::codex_thread::CodexThread,
    model: Option<&str>,
    effort: Option<&str>,
    yolo: bool,
    node: Option<&str>,
) -> RegistryEntry {
    let cwd_s = cwd.to_string_lossy().into_owned();
    let session_id = driver.thread_id().to_string();
    let (parent_session, parent_harness, parent_cwd) = crate::claims::ambient_parent_edge();
    RegistryEntry {
        node: node.filter(|node| !node.is_empty()).map(str::to_string),
        // v25: the route axes this lane actually used. Codex's ambient auth
        // is the account it positively pinned nothing past, so "default".
        route_provider_id: Some("openai".into()),
        model_name: model.filter(|m| !m.is_empty()).map(str::to_string),
        account_record_id: Some("default".into()),
        // The daemon-hosted codex app-server thread lane.
        substrate: Some("thread".into()),
        name: name.into(),
        short_id: String::new(),
        legacy_provider: String::new(),
        provider: Some("openai".into()),
        model: model.map(str::to_string),
        // The model arrived on the spawn request, so requested is its basis -
        // the field no longer reads unpopulated on this mint.
        model_basis: model.map(|_| "requested".to_string()),
        effort: effort.map(str::to_string),
        // v23 (x-2019): the request beside the effect; verbatim as typed.
        requested_model: model.filter(|m| !m.is_empty()).map(str::to_string),
        requested_provider: None,
        requested_effort: effort.filter(|v| !v.is_empty()).map(str::to_string),
        harness: Some("codex".into()),
        harness_session_id: Some(session_id.clone()),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        // x-d285: non-claude harness; the account axis does not apply.
        launch_account: None,
        related_session_id: None,
        cwd: cwd_s.clone(),
        project_root: cwd_s,
        session_id: None,
        origin: Some("spawn".into()),
        spawn_trigger: None,
        spawned_by_session: parent_session,
        spawned_by_harness: parent_harness,
        spawned_by_cwd: parent_cwd,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: Some(session_id.clone()),
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: Some(crate::state::HOST_MODE_INTERACTIVE.into()),
        status: AgentStatus::Live,
        last_message_at: Some(now_rfc3339_like()),
        created_at: now_rfc3339_like(),
        // No pid, deliberately. A codex thread worker owns NO process: its
        // app-server is the shared daemon, serving every other codex session
        // on the machine too. `pid` is a LIVENESS surface, and writing the
        // daemon's pid here made every thread row carry the same always-alive
        // pid. A stopped row then read `Unmeasured` forever in `derive_liveness`
        // (the status-contradicts-pid tier) and `pid_confirmed_dead` in gc
        // could never corroborate its removal. Ownership is provable from the
        // control socket and `thread/loaded/list`, which is where it belongs.
        pid: None,
        pid_start_time: None,
        keeper_child_pid: None,
        log_path: Some(driver.rollout_path().to_string_lossy().into_owned()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: Some(session_id),
        delivery_policy: None,
        // v19: the launch posture is the resume posture. The doc's old warning
        // that "a registry row records no sandbox posture" died here.
        sandbox_posture: Some(
            if yolo {
                "danger-full-access"
            } else {
                "workspace-write"
            }
            .to_string(),
        ),
        ..Default::default()
    }
}
