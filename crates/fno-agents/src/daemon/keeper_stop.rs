//! The keeper-thread arm of the stop verb: `stop_keeper_row`, moved out of
//! `daemon.rs` for the file budget, beside `claude_stop`. A lane-B keeper
//! row's lifecycle lives on its own socket (see `keeper_thread_sock`), and a
//! cursor-agent row adds one more leg: Cursor's detached worker-server is the
//! TUI's child, so it is captured while the row's pid still proves ownership
//! and reaped after the teardown - the order the deleted Python twin held.
//! A census failure degrades to an empty capture (the kill does not depend
//! on it), while a reap failure refuses the terminal stamp, because
//! surviving servers are exactly what the reaper exists to prevent.

use serde_json::json;
use std::path::Path;

use super::blocking_bound::off_executor;
use super::{
    now_rfc3339_like, state_error_code, stop_keeper_confirmed, update_registry_offloaded, Ctx,
    RegistryEntry, Request, Response,
};
use crate::protocol::ErrorCode;
use crate::AgentStatus;

pub(super) async fn stop_keeper_row(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    entry: &RegistryEntry,
    sock: &Path,
) -> Response {
    let cursor = entry.harness_name() == "cursor-agent";
    let worker_servers = if cursor {
        match off_executor(|| {
            crate::cursor_agent::capture_detached_worker_servers(entry.pid, entry.pid_start_time)
        }) {
            Ok(handles) => handles,
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };
    if !stop_keeper_confirmed(sock).await {
        let _ = ctx.emitter.emit(
            "agent_stop_refused",
            &json!({"name": name, "backend": "keeper-thread"}),
        );
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("agent {name}: keeper did not confirm shutdown; it may still be running"),
        );
    }
    let reaped = if cursor {
        match off_executor(|| crate::cursor_agent::reap_detached_worker_servers(&worker_servers)) {
            Ok(count) => Some(count),
            Err(reason) => {
                return Response::err(
                    req.id,
                    ErrorCode::Internal,
                    format!(
                        "agent {name}: keeper stopped but cursor-agent worker-server reap failed: {reason}"
                    ),
                );
            }
        }
    } else {
        None
    };
    let stop_name = name.to_string();
    let wake_home = ctx.home.clone();
    if let Err(error) = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
        if let Some(entry) = registry.find_mut(&stop_name) {
            entry.status = AgentStatus::Exited;
            entry.exited_at = Some(now_rfc3339_like());
            crate::state::record_stop(entry, "stop-verb", Some("keeper-thread".into()));
            // best-effort: a failed stamp costs a later wake its name,
            // never the stop.
            crate::wake_name::stamp_from_row(&wake_home, entry);
        }
    })
    .await
    {
        return Response::err(
            req.id,
            state_error_code(&error),
            format!("keeper thread {name} stopped but registry write failed: {error}"),
        );
    }
    let mut event = json!({"name": name, "backend": "keeper-thread"});
    let mut payload = json!({"stopped": true, "backend": "keeper-thread"});
    if let Some(count) = reaped {
        event["worker_servers_reaped"] = json!(count);
        payload["worker_servers_reaped"] = json!(count);
    }
    let _ = ctx.emitter.emit("agent_stopped", &event);
    Response::ok(req.id, payload)
}
