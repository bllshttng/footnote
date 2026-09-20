//! The `agent.convert` transaction: move a live pane session onto the
//! thread lane under its own session id.
//!
//! The daemon owns this rather than the client for the reason every other
//! lifecycle move is daemon-owned: the agent lock serializes it, and the
//! daemon's pid is what a mid-move claim can be pinned to. A client-side
//! conversion would pin its claims to a process that exits seconds later.
//!
//! The classification runs TWICE. Once client-side, so a refusal costs no
//! lock, and again here under the lock against a fresh read, because the
//! world moves between the two: a pane can exit, and a concurrent request
//! can convert the row first. The second answer is the one that acts.

use super::*;
use crate::convert::{ConvertPlan, ConvertRefusal, KeeperSighting, PaneRead};

/// `handle_rename` lives here rather than in daemon.rs for the file budget:
/// daemon.rs is over 5,000 lines and shrink-only, so the dispatch arm this
/// change adds is paid for by moving an existing body out. The behavior is
/// unchanged.
pub(super) fn handle_rename(ctx: &Ctx, req: &Request) -> Response {
    state::rename_response(&ctx.home.registry_json(), req)
}

/// How long to wait for the agent lock before refusing. A conversion is
/// seconds of work, so a wait longer than this means another writer is
/// mid-move on the same row and the caller should hear that rather than
/// queue behind it indefinitely.
const CONVERT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn handle_convert(ctx: &Ctx, req: &Request) -> Response {
    let Some(name) = req.params.get("name").and_then(Value::as_str) else {
        return Response::err(req.id, ErrorCode::InvalidParams, "convert needs a name");
    };
    let dry_run = req
        .params
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // A dry run takes the lock too. It reports what the conversion WOULD do,
    // and a plan read while another writer is mid-move describes a world
    // that no longer exists by the time the operator reads it.
    let _lock = match crate::agent_lock::AgentLock::acquire(&ctx.home, name, CONVERT_LOCK_TIMEOUT) {
        Ok(lock) => lock,
        Err(()) => {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!(
                    "convert {name}: another writer holds the agent lock{}",
                    crate::agent_lock::holder_note(&ctx.home, name)
                ),
            )
        }
    };

    let entries = match state::load_registry(&ctx.home.registry_json()) {
        Ok(registry) => registry.entries,
        Err(error) => {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!("convert {name}: registry read failed: {error}"),
            )
        }
    };
    let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            format!("convert {name}: no such agent"),
        );
    };

    let contract = match crate::harness_capabilities::HarnessContract::packaged()
        .and_then(|contract| contract.conversion(entry.harness_name()))
    {
        Ok(contract) => contract,
        Err(error) => {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!("convert {name}: capability contract unreadable: {error}"),
            )
        }
    };

    let plan = match crate::convert::classify(entry, &contract, &read_panes(entry), &read_keepers())
    {
        Ok(plan) => plan,
        Err(refusal) => return refusal_response(req.id, refusal),
    };

    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({
            "name": plan.name,
            "strategy": plan.strategy,
            "phase": if dry_run { "classified-dry-run" } else { "classified" },
        }),
    );

    if dry_run {
        return Response::ok(req.id, plan_json(&plan, true));
    }

    // The strategies land next; until one is wired, a real conversion
    // refuses with the plan it would have run rather than half-doing it.
    // A refusal that shows the plan is still a receipt the operator can act
    // on, and it can never leave a row mid-move.
    Response::err(
        req.id,
        ErrorCode::Internal,
        format!(
            "convert {}: the {} strategy is not wired yet. The plan is sound - re-run with \
             --dry-run to read it - but no step of it has run, and the row is untouched.",
            plan.name, plan.strategy
        ),
    )
}

fn refusal_response(id: u64, refusal: ConvertRefusal) -> Response {
    match refusal {
        // Idempotent: the caller asked for a state the world is in, so this
        // is a success with a receipt, not an error.
        ConvertRefusal::AlreadyAThread { .. } => Response::ok(
            id,
            json!({"outcome": "already-a-thread", "detail": refusal.message()}),
        ),
        ConvertRefusal::Refused(_) => {
            Response::err(id, ErrorCode::InvalidParams, refusal.message())
        }
    }
}

fn plan_json(plan: &ConvertPlan, dry_run: bool) -> Value {
    let (pane_session, pane_id) = plan.host.pane();
    json!({
        "outcome": if dry_run { "dry-run" } else { "converted" },
        "name": plan.name,
        "harness": plan.harness,
        "strategy": plan.strategy,
        "preserves_id": plan.preserves_id,
        "session_id": plan.session_id,
        "pane": {"session": pane_session, "pane_id": pane_id},
        "child_pid": plan.host.child_pid(),
        "steps": plan.steps,
        "receipt": plan.receipt(),
    })
}

/// Live panes, read through the same listing the pane stop uses. Scoped to
/// the row's recorded mux session when it has one, and unscoped otherwise:
/// a reseated row has no mux ref and is still found by child pid.
fn read_panes(entry: &RegistryEntry) -> Vec<PaneRead> {
    crate::pane_stop::pane_list_via_fno(entry.mux.as_ref().map(|mux| mux.session.as_str()))
        .into_iter()
        .map(|pane| PaneRead {
            session: pane.session,
            pane_id: pane.pane_id,
            child_pid: pane.child_pid,
        })
        .collect()
}

/// The keeper sockets, read through `fno mux pane keeper list --json`. An
/// unreadable listing answers EMPTY, which makes a keeper-lane conversion
/// refuse with its relaunch remedy rather than proceed as though the child
/// had no keeper and stop a process that did not need stopping.
fn read_keepers() -> Vec<KeeperSighting> {
    let Ok(output) = std::process::Command::new("fno")
        .args(["mux", "pane", "keeper", "list", "--json"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(rows) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    KeeperSighting::from_json(&rows)
}
