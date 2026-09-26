//! Session-binding gate for `loop-check --harness <name> --harness-session <id>`.
//!
//! Before this gate, any idle session in a target's directory could reach the
//! completion gate: the OpenCode bridge kept only the boolean "is footnote
//! running here" and passed the idle event's own session id nowhere. The
//! registry already knows which session owns which cwd and node, so the gate
//! resolves the row by `(harness, session_id)` - the sole identity axis since
//! schema v8 - and answers one of three dispositions:
//!
//! - `Owner`: the row's cwd (and, when both name one, its node) agree with
//!   the target at `--state`; the normal engine decides as before.
//! - `Crown`: the row is a crowned session with no node of its own; the king
//!   path answers on its own evidence and never needs a target manifest.
//! - `Refuse`: unreadable registry, absent row, cwd mismatch, or node
//!   mismatch. Each carries its own reason, none is headroom, and the refusal
//!   exits 0 so a plugin reading the JSON never crashes on it.

use super::*;

pub(super) struct Refusal {
    /// The session id the bound row carries; empty when no row exists.
    pub bound: String,
    /// The session id that asked.
    pub asked: String,
    pub reason: String,
}

pub(super) enum Gate {
    Owner,
    Crown,
    Refuse(Refusal),
}

/// The whole identity gate, as a decision-or-proceed. `None` when the caller
/// passed no binding flags (the engine answers exactly as before) or when the
/// asking session is the owner; `Some` carries the refusal or the crown
/// routing, each of which answers instead of the engine.
pub(super) fn gate_output(parsed: &LoopCheckArgs) -> Option<(i32, String)> {
    if parsed.harness.is_none() || parsed.harness_session.is_none() {
        return None;
    }
    match gate(parsed) {
        Gate::Refuse(r) => Some(refusal_output(parsed, r)),
        Gate::Crown => Some(super::king_decide::king_decide(parsed)),
        Gate::Owner => None,
    }
}

/// The typed refusal: exit 0 so a plugin reading the JSON never crashes on
/// it, one loop_check telemetry row, both session ids and the reason on the
/// wire.
fn refusal_output(parsed: &LoopCheckArgs, r: Refusal) -> (i32, String) {
    let project_events = parsed
        .events_path
        .clone()
        .unwrap_or_else(|| crate::paths::events_path(&parsed.cwd));
    let global_events = parsed
        .global_events_path
        .clone()
        .unwrap_or_else(|| project_events.clone());
    super::emit_to_both(
        &project_events,
        &global_events,
        "loop_check",
        serde_json::json!({
            "decision": "refuse",
            "harness": parsed.harness,
            "asked_session": r.asked,
            "bound_session": if r.bound.is_empty() { serde_json::Value::Null } else { serde_json::json!(r.bound) },
            "reason": r.reason,
        }),
    );
    (
        0,
        serde_json::json!({
            "decision": "refuse",
            "termination_reason": null,
            "message": r.reason,
            "asked_session": r.asked,
            "bound_session": if r.bound.is_empty() { serde_json::Value::Null } else { serde_json::json!(r.bound) },
            "reason": r.reason,
        })
        .to_string(),
    )
}

pub(super) fn gate(parsed: &LoopCheckArgs) -> Gate {
    let harness = parsed.harness.as_deref().unwrap_or("");
    let asked = parsed.harness_session.clone().unwrap_or_default();
    let refuse = |reason: String, bound: String| {
        Gate::Refuse(Refusal {
            bound,
            asked: asked.clone(),
            reason,
        })
    };

    // An undeclared home is the unreadable-registry case, not headroom: the
    // gate cannot bind what it cannot read, so it refuses.
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return refuse(
            "registry home undeclared; cannot resolve the session row".into(),
            asked.clone(),
        );
    };
    let registry = match crate::state::load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => return refuse(format!("registry unreadable: {e}"), asked.clone()),
    };
    // The target's owner row: bound by node when the manifest names one,
    // else by cwd among rows with no node of their own. The asking session
    // must BE the owner; anything else is the wrong-session refusal that
    // names both sides.
    let m_node = manifest_node(&parsed.state_path);
    let owner = registry.entries.iter().find(|e| {
        e.harness_name() == harness
            && match (&m_node, &e.node) {
                (Some(mn), Some(en)) => en == mn,
                (None, None) => cwd_matches(&e.cwd, &parsed.cwd),
                _ => false,
            }
    });
    if let Some(owner) = owner {
        let bound = owner.harness_session_id.clone().unwrap_or_default();
        if bound != asked {
            let reason = format!("wrong session: target bound to {bound}, asked {asked}");
            return Gate::Refuse(Refusal {
                bound,
                asked,
                reason,
            });
        }
        // The asking session IS the owner; the crown disposition still gives
        // a crowned owner a path that does not depend on a target manifest.
        if owner.crown_level.is_some() && owner.node.is_none() {
            return Gate::Crown;
        }
        return Gate::Owner;
    }
    // No owner row for this target: fall back to the asking session's own
    // row. An absent row is a refusal, never headroom - unless the manifest
    // itself binds the asking session, which is the pi shape: the extension
    // gates with its own session id against a manifest whose
    // `harness_session_id` names that same session, and the registry never
    // grows a row for it (the worker keeps no registry row keyed by its pi
    // session id).
    let Some(row) = registry.find_by_session(harness, &asked) else {
        if let Some(gate) = manifest_owner(parsed, harness, &asked) {
            return gate;
        }
        return refuse(
            format!("no registry row bound to {harness}/{asked}"),
            String::new(),
        );
    };
    if !cwd_matches(&row.cwd, &parsed.cwd) {
        return refuse(
            format!(
                "cwd mismatch: row claims {}, gate was asked {}",
                row.cwd,
                parsed.cwd.display()
            ),
            row.harness_session_id.clone().unwrap_or_default(),
        );
    }
    if let (Some(row_node), Some(m_node)) = (&row.node, &m_node) {
        if row_node != m_node {
            return refuse(
                format!("node mismatch: row claims {row_node}, manifest binds {m_node}"),
                row.harness_session_id.clone().unwrap_or_default(),
            );
        }
    }
    // A crowned row with no node of its own is a crown, not a target owner:
    // the king path evaluates its own evidence. When the row carries BOTH a
    // crown and a node, the node binding is the tighter contract and wins.
    if row.crown_level.is_some() && row.node.is_none() {
        return Gate::Crown;
    }
    Gate::Owner
}

/// A bound ask gets the continuation in the asking harness's own command
/// form. pi registers skills as `/skill:<name>` and has no `/target` of its
/// own, and the word the target skill resumes with is `resume`, never
/// `--resume`, so pi's continuation renders through the capability row from
/// the skill-correct base. Every other harness keeps the legacy literal
/// byte-for-byte: the opencode bridge sends the gate's string verbatim and
/// its contract is pinned.
pub(super) fn render_continuation_for_harness(
    args: &[String],
    result: (i32, String),
) -> (i32, String) {
    let Ok(parsed) = super::parse_args(args) else {
        return result;
    };
    if parsed.harness.as_deref() != Some("pi") {
        return result;
    }
    let (code, out) = result;
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&out) else {
        return (code, out);
    };
    if value.get("continuation").and_then(Value::as_str) == Some("/target --resume") {
        value["continuation"] =
            serde_json::Value::String(crate::provider::render_verb_seed("/target resume", "pi"));
        return (code, value.to_string());
    }
    (code, out)
}

fn cwd_matches(row_cwd: &str, asked: &Path) -> bool {
    let row = Path::new(row_cwd);
    if row == asked {
        return true;
    }
    // Representation differs (symlink, trailing slash, /private prefix on
    // macOS); fall through to the resolved paths when both resolve.
    match (std::fs::canonicalize(row), std::fs::canonicalize(asked)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The harness-agnostic owner arm beside the registry row: the manifest at
/// `--state` may bind the asking session itself. This is the pi shape - the
/// extension gates with its own session id against a manifest whose
/// `harness_session_id` names that same session, and no registry row is
/// ever keyed by a pi session id. All three facts must agree: the row's
/// harness, the session id (under the same comparison a registry row's id
/// gets), and the checkout the manifest lives in versus `--cwd`. Anything
/// else keeps the typed refusal, enriched with the manifest's own bound
/// pair so the refusal names both sides.
fn manifest_owner(parsed: &LoopCheckArgs, harness: &str, asked: &str) -> Option<Gate> {
    let content = std::fs::read_to_string(&parsed.state_path).ok()?;
    let m_harness = scan_manifest_field(&content, "harness")?;
    let m_session = scan_manifest_field(&content, "harness_session_id")?;
    // The checkout the manifest records is authoritative: a space-resolved
    // manifest lives under the state root, never inside the checkout it
    // binds. A legacy manifest that names no owner_cwd falls back to its own
    // directory (the in-repo layout, where the manifest sat in the checkout).
    let checkout = scan_manifest_field(&content, "owner_cwd").or_else(|| {
        parsed
            .state_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
    });
    let Some(checkout) = checkout else {
        return None;
    };
    if m_harness != harness
        || !crate::claims::same_session_id(&m_session, asked)
        || !cwd_matches(&checkout, &parsed.cwd)
    {
        let reason = format!(
            "wrong session: manifest bound to {m_harness}/{m_session}, asked {harness}/{asked}"
        );
        return Some(Gate::Refuse(Refusal {
            bound: m_session,
            asked: asked.to_string(),
            reason,
        }));
    }
    // The manifest-owner disposition is observable the same way a refusal
    // is: one loop_check row, so an audit can tell a manifest-bound owner
    // from a registry-row owner.
    let project_events = parsed
        .events_path
        .clone()
        .unwrap_or_else(|| crate::paths::events_path(&parsed.cwd));
    let global_events = parsed
        .global_events_path
        .clone()
        .unwrap_or_else(|| project_events.clone());
    super::emit_to_both(
        &project_events,
        &global_events,
        "loop_check",
        serde_json::json!({
            "decision": "owner",
            "source": "manifest",
            "harness": harness,
            "asked_session": asked,
        }),
    );
    Some(Gate::Owner)
}

/// The node the manifest binds: an explicit `node:` line, else `input:` when
/// it is node-shaped (a plan path or free text binds no node). Absence is
/// unknown, never a mismatch.
fn manifest_node(state_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(state_path).ok()?;
    if let Some(node) = scan_manifest_field(&content, "node") {
        return Some(node);
    }
    let input = scan_manifest_field(&content, "input")?;
    if is_node_shaped(&input) {
        Some(input)
    } else {
        None
    }
}

/// `<prefix>-<hex>` backlog node shape: a lowercase prefix, a dash,
/// at least four hex characters. Deliberately shape-only: resolution stays
/// the backlog's job.
fn is_node_shaped(value: &str) -> bool {
    let Some((prefix, hex)) = value.split_once('-') else {
        return false;
    };
    !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_lowercase())
        && hex.len() >= 4
        && !hex.is_empty()
        && hex.chars().all(|c| c.is_ascii_hexdigit())
}
