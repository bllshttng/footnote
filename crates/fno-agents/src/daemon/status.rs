//! The `agent.status` read: registry counts plus the daemon's own vitals.
//! Moved out of daemon.rs (shrink-only) so the phase-2 op arms pay for their
//! dispatch lines with this move, and so status-side additions land here,
//! never back in the daemon file.

use super::*;

pub(super) async fn handle_status(ctx: &Ctx, req: &Request) -> Response {
    // load_registry does blocking flock I/O; offload it from the async worker
    // thread (Gemini review). The drive-table read below stays async. A read
    // failure is an RPC error: `unwrap_or_default()` here published
    // zero-agent status counts over a broken registry.
    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(reg) => reg,
        Err(e) => return registry_read_failed(req.id, e),
    };
    let mut by_status: Map<String, Value> = Map::new();
    let mut restarting: u64 = 0;
    let mut channels_registered: u64 = 0;
    for e in &registry.entries {
        let key = format!("{:?}", e.status).to_lowercase();
        let n = by_status.get(&key).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        by_status.insert(key, Value::Number(n.into()));
        if e.status == AgentStatus::Restarting {
            restarting += 1;
        }
        if e.mcp_channel_id.is_some() {
            channels_registered += 1;
        }
    }
    Response::ok(
        req.id,
        json!({
            "schema_version": 1,
            "daemon": {
                "state": DaemonState::Serving.as_str(),
                "pid": std::process::id(),
                "uptime_secs": ctx.started_at.elapsed().as_secs(),
                "version": env!("CARGO_PKG_VERSION"),
                // Drift signal, additive. Null when the daemon
                // could not fingerprint itself; a client then reads Unknown.
                "exe_path": ctx
                    .exe_fingerprint
                    .as_ref()
                    .map(|f| f.path.to_string_lossy().into_owned()),
                "exe_mtime": ctx.exe_fingerprint.as_ref().map(|f| f.mtime_nanos),
                "exe_size": ctx.exe_fingerprint.as_ref().map(|f| f.size),
                // The daemon's own process start time, for the `restart`
                // pid-reuse guard.
                "pid_start_time": ctx.pid_start_time,
            },
            "agents": {
                "total": registry.entries.len(),
                "by_status": by_status,
            },
            "restarts": {
                // queue_depth tracks agents currently restarting; the full
                // restart queue + consecutive-failure history is not yet
                // surfaced in the served status (Wave 5), so the max-seen
                // counter reports 0 until that subsystem is wired into Ctx.
                "queue_depth": restarting,
                "consecutive_failures_max_seen": 0,
            },
            "channels": { "registered": channels_registered },
        }),
    )
}
