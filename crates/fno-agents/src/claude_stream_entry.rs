//! Build the registry row for a claude stream-json adoption lane: birth
//! construction with request-carried lineage.

use std::path::PathBuf;

use crate::daemon::now_rfc3339_like;
use crate::state::{Lineage, RegistryEntry};
use crate::AgentStatus;

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
    use super::build_claude_stream_entry;

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
}
