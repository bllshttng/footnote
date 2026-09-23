//! The codex thread-spawn lane: request validation, registry insertion and
//! actor start for `agent.spawn` on the thread substrate. Extracted from the
//! config hub's Rust sibling (this file carried 16.7k lines against a 5k
//! budget, shrink-only); the handler and its daemon-private helpers stay put,
//! the lane moves with its body.

use super::{
    codex_thread_on_done, codex_thread_on_status, is_non_terminal, state_error_code,
    thread_spawn_refusal, update_registry_offloaded, Ctx, WARMUP_SEED,
};
use crate::codex_thread_entry::build_codex_thread_entry;
use crate::protocol::{ErrorCode, Request, Response};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) async fn spawn_codex_thread_lane(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    cwd: &Path,
    provider: &str,
    provenance: Option<&crate::spawn_contract::SpawnProvenance>,
) -> Response {
    if provider != "codex" {
        return thread_spawn_refusal(
            ctx,
            req,
            name,
            provider,
            &format!(
                "thread spawn refused: the only attach-with-server destination built drives \
                 the codex app-server; harness {provider} needs its own thread destination \
                 wired before its spawn can be served"
            ),
        );
    }
    let model = req.params.get("model").and_then(Value::as_str);
    // Both spellings, resolved by one reader. Reading `yolo` alone dropped
    // `permission_mode` silently and started bounded, which downgrades the very
    // posture the caller was naming; an unrecognized value is refused here
    // rather than degraded, for the same reason. The posture stays TYPED from
    // here on: both halves ride the start frame, the registry row and the
    // resume, never a bool.
    let posture = match crate::codex_thread::resolve_thread_posture(
        req.params.get("yolo").and_then(Value::as_bool),
        req.params.get("permission_mode").and_then(Value::as_str),
    ) {
        Ok(posture) => posture,
        Err(reason) => return thread_spawn_refusal(ctx, req, name, provider, &reason),
    };
    let effort = req.params.get("effort").and_then(Value::as_str);
    let node = req.params.get("node").and_then(Value::as_str);
    // Hop 2 of the state-root grant. Read the roots from the REQUEST,
    // never from this process's environment. This daemon is long-lived and
    // shared across every thread on the machine, so its own env is not the
    // spawning client's - a `state_dirs_from_env()` call here would read
    // whatever shell started the daemon, which is the exact mistake the next
    // reader of this function will be tempted to make.
    //
    // The same holds for RESOLVING a root rather than reading one. The plan
    // content directory is not missing from this list and does not need
    // `provider::plan_content_dir` called here: the Python spawn seam already
    // computes it for every substrate and publishes it on the env var the
    // client turns into these params. Adding a resolver here would be a second
    // answer to one question, and it would shell out to `fno` per spawn from
    // async code on the daemon every codex worker shares. Pinned by
    // `test_thread_spawn_seam_publishes_the_plan_dir`.
    let state_dirs: Vec<String> = req
        .params
        .get("state_dirs")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|dir| !dir.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Fenced `--` tokens the front door carried here BECAUSE this lane takes
    // them. A token the lane does not carry is a router bug, refused by name
    // rather than dropped: the operator typed it expecting it to land.
    let harness_args: Vec<String> = req
        .params
        .get("harness_args")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let carry = match crate::codex_thread::parse_harness_args(&harness_args) {
        Ok(carry) => carry,
        Err(reason) => return thread_spawn_refusal(ctx, req, name, provider, &reason),
    };
    let mut state_dirs = state_dirs;
    for dir in carry.add_dirs {
        if !state_dirs.iter().any(|existing| existing == &dir) {
            state_dirs.push(dir);
        }
    }
    let seed = req
        .params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // A node-backed target seeded from the CANONICAL checkout is born in the
    // node's worktree, not on canonical main: the same node-keyed ensure the
    // node-seeded spawn door already runs, applied before `thread/start` so
    // the app-server request, the registry row and the birth event carry ONE
    // cwd. A refused ensure refuses the whole spawn - starting on canonical
    // as a fallback would be the exact outcome this resolves away. The ensure
    // shells out (git + the worktree ensure), so it runs on the blocking
    // pool, off the async executor every hosted thread shares.
    let cwd = match resolve_target_cwd(cwd, node, &seed).await {
        Ok(cwd) => cwd,
        Err(reason) => return thread_spawn_refusal(ctx, req, name, provider, &reason),
    };
    let driver = match crate::codex_thread::CodexThread::start_with_state_dirs(
        cwd.clone(),
        model,
        &posture,
        effort,
        &state_dirs,
        Some(&carry.config),
    )
    .await
    {
        Ok(driver) => driver,
        Err(error) => {
            let _ = ctx.emitter.emit(
                "agent_spawn_failed",
                &json!({"name": name, "provider": "codex", "lane": "thread", "reason": error.to_string()}),
            );
            return Response::err(req.id, ErrorCode::SpawnFailed, error.to_string());
        }
    };
    let entry = build_codex_thread_entry(
        name,
        &cwd,
        &driver,
        model,
        effort,
        node,
        req.params.get("account").and_then(Value::as_str),
        &harness_args,
        &req.params,
        provenance,
    );
    let session_id = entry.harness_session_id.clone().unwrap_or_default();
    let inserted = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
        if registry
            .entries
            .iter()
            .any(|existing| existing.name == entry.name)
        {
            return false;
        }
        if registry.entries.iter().any(|existing| {
            existing.harness_name() == "codex"
                && existing.harness_session_id.as_deref() == entry.harness_session_id.as_deref()
                && is_non_terminal(existing.status)
        }) {
            return false;
        }
        registry.entries.push(entry);
        true
    })
    .await;
    match inserted {
        Ok(true) => {}
        Ok(false) => {
            return Response::err(
                req.id,
                ErrorCode::AgentExists,
                format!("agent {name} or Codex thread {session_id} already exists"),
            )
        }
        Err(error) => {
            return Response::err(
                req.id,
                state_error_code(&error),
                format!("registry write: {error}"),
            )
        }
    }
    let handle = Arc::new(driver.into_actor(
        codex_thread_on_done(&ctx.emitter, ctx.home.registry_json(), name),
        codex_thread_on_status(
            &ctx.emitter,
            ctx.home.registry_json(),
            name,
            &session_id,
            1,
            ctx.opts.notify_on_blocked,
            ctx.opts.notify_on_done,
        ),
    ));
    ctx.codex_threads
        .lock()
        .await
        .insert(name.to_string(), Arc::clone(&handle));

    // The seed turn is just the first Submit in the actor's queue: no
    // dedicated task, no lock to steal. Its reply receiver is dropped on
    // purpose (nobody waits); the on-done hook still emits the event, and a
    // first follow-up ask STEERS into the seed turn instead of blocking
    // behind it (the daemon.rs:4077 mutex shape this replaces).
    //
    // A seedless spawn takes WARMUP_SEED rather than no turn at all.
    // `thread/start` records a thread id but writes no rollout, and a harness
    // resolves a session to attach BY that rollout, so a worker with no turn
    // is a worker the operator cannot open: `codex resume` answers "no rollout
    // found for thread id <id>" (measured 2026-08-28, codex-cli 0.149.1).
    // One cheap turn buys attachability from the first second of a worker's
    // life, which is the window in which someone is most likely to look.
    let seed = if seed.trim().is_empty() {
        WARMUP_SEED.to_string()
    } else {
        seed
    };
    {
        let seed_name = name.to_string();
        let submitted = handle.submit(seed).await;
        if submitted.is_err() {
            let _ = ctx.emitter.emit(
                "daemon_recovery_error",
                &json!({"op": "codex_thread_seed", "name": seed_name, "error": "actor gone at seed submit"}),
            );
        }
    }
    // `substrate` and `cwd` are load-bearing: the mux restore receipt parser
    // (crates/fno/src/server.rs parse_spawn_receipts) drops any agent_spawned
    // event without both, which is how a thread worker could lose its only
    // resume fallback before the row is reaped.
    let _ = ctx.emitter.emit(
        "agent_spawned",
        &crate::spawn_edge::birth_event(
            name,
            &crate::codex_thread_entry::thread_lineage(&req.params, provenance),
            json!({
                "provider": "codex",
                "harness": "codex",
                "harness_session_id": session_id,
                "short_id": "",
                "status": "live",
                "lane": "thread",
                "substrate": "thread",
                "cwd": cwd.to_string_lossy(),
                "node": node,
            }),
        ),
    );
    Response::ok(
        req.id,
        json!({
            "short_id": "",
            "harness": "codex",
            "harness_session_id": session_id,
            "session_id": session_id,
            "status": "live",
            "lane": "thread",
        }),
    )
}

/// The one cwd a hosted Codex target thread is born with. A node-backed
/// target whose requested cwd is the repository's canonical checkout is
/// re-homed onto the node's worktree through the existing launch-workdir
/// ensure. Every other shape keeps the requested path: a non-target seed,
/// no node, a cwd that already IS a worktree, and a worktree policy of
/// `never` (where the ensure answers the canonical path itself).
async fn resolve_target_cwd(cwd: &Path, node: Option<&str>, seed: &str) -> Result<PathBuf, String> {
    let Some(node) = node.filter(|node| !node.is_empty()) else {
        return Ok(cwd.to_path_buf());
    };
    let Some((verb, _)) = seed
        .split_whitespace()
        .next()
        .and_then(crate::provider::parse_verb_token)
    else {
        return Ok(cwd.to_path_buf());
    };
    if verb != "target" {
        return Ok(cwd.to_path_buf());
    }
    let requested = cwd.to_path_buf();
    let node_owned = node.to_string();
    let ensured = {
        let node = node_owned.clone();
        tokio::task::spawn_blocking(move || {
            if !crate::launch_workdir::is_canonical_checkout(&requested) {
                return Ok(requested);
            }
            crate::launch_workdir::ensure_node_workdir(&requested, &node, "codex")
        })
        .await
        .map_err(|error| format!("node {node_owned}: launch-workdir join failed: {error}"))?
        .map_err(|reason| format!("node {node_owned}: {reason}"))?
    };
    if !ensured.is_dir() {
        return Err(format!(
            "node {node_owned}: target worktree disappeared before thread start: {}",
            ensured.display()
        ));
    }
    Ok(ensured)
}
