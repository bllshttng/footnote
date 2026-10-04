//! `crown-settle`: whether a crowned spawn is granted, transfers, or refuses,
//! reached as payload kind `crown-settle` on the existing `spawn-overlay`
//! verb (law d-fe66560a bars new top-level verbs, hidden ones included).
//!
//! A port of Python's `settle_spawn_crown` (`cli/src/fno/agents/crown.py`)
//! with one new branch: a human caller with `succession` may transfer a
//! crown away from any live holder, the same authority `grant_error` already
//! gives a human to bestow any scope (a human may grant what nobody above it
//! could check). An agent caller may only succeed itself: every live holder
//! of the scope must already be that agent.
//!
//! Rows arrive as plain JSON here, not a typed registry row, so a row's
//! status is matched by string against `announce::TERMINAL_STATUSES`
//! (itself `registry.py::TERMINAL_STATUSES`), the same string-match
//! reasoning `announce.rs` documents for its own terminal check.
//!
//! Occupancy is the exact-scope holders plus ladder-aware rivals: through
//! `loop_king::crown_rivals`, a live crown over overlapping territory
//! declines the spawn before the succession branch, and an agent's own
//! wider crown is its delegation, not a rival.
//!
//! Crown-settle answers twice: before launch against a snapshot, then again
//! under the registry lock with the plan attached. Holder identity is the
//! name and harness session id together because registry names are reclaimable.
//! Rows that both lack a session id still compare by name alone.

use crate::announce::TERMINAL_STATUSES;
use crate::loop_king::{crown_rivals_pub, same_territory, scopes_overlap};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

struct Holder {
    index: usize,
    name: String,
    session: Option<String>,
}

struct Occupancy {
    clear_terminal: Vec<(usize, String)>,
    holders: Vec<Holder>,
    rivals: Vec<(String, String)>,
}

enum Caller {
    Human,
    Agent(String),
}

fn parse_caller(value: Option<&Value>) -> Option<Caller> {
    match value?.get("kind").and_then(Value::as_str)? {
        "human" => Some(Caller::Human),
        "agent" => Some(Caller::Agent(
            value?.get("name").and_then(Value::as_str)?.to_string(),
        )),
        _ => None,
    }
}

/// Decide occupancy for one crowned spawn over `scope`, then apply the
/// crown-name effect (a succeeded succession carries the name with a
/// regnal bump; a fresh grant forgets it). See the module doc for the
/// caller/succession rules and the request/answer shapes.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    if payload.get("plan").is_none() {
        // The plan/preview path reads no store and no home env (existing
        // unit tests run it with no declared agents home).
        let projects = crate::king_board::project_map(&std::env::current_dir().unwrap_or_default());
        return resolve_with_projects(payload, &projects);
    }
    let store = crate::paths::AgentsHome::from_env_opt().map(|h| h.crown_names_json());
    let projects = crate::king_board::project_map(&std::env::current_dir().unwrap_or_default());
    let answer = resolve_with_projects(payload, &projects)?;
    // No declared home (a test) reads as no store, so the plan answer is
    // unchanged; production always declares one.
    if let Some(store) = store.as_ref() {
        apply_name_effect(payload, &answer, store);
    }
    Ok(answer)
}

/// [`resolve`] with an explicit store path, so tests pass a tempdir store
/// instead of setting the agents home env.
pub fn resolve_at(payload: &Value, store: &std::path::Path) -> Result<Value, String> {
    let projects = crate::king_board::project_map(&std::env::current_dir().unwrap_or_default());
    let answer = resolve_with_projects(payload, &projects)?;
    apply_name_effect(payload, &answer, store);
    Ok(answer)
}

/// The registry commit is the authority: a store error prints one stderr
/// line and never changes the answer.
fn apply_name_effect(payload: &Value, answer: &Value, store: &std::path::Path) {
    if payload.get("plan").is_none() {
        return;
    }
    let Some(scope) = payload.get("scope").and_then(|s| s.as_str()) else {
        return;
    };
    let outcome = answer.get("outcome").and_then(|o| o.as_str());
    let effect = match outcome {
        Some("succeeded") => {
            let pending = succession_pending(payload);
            crate::crown_names::carry_succession(store, scope, pending)
        }
        Some("granted") => crate::crown_names::forget(store, scope),
        _ => Ok(()),
    };
    if let Err(e) = effect {
        eprintln!("crown-settle: crown names: {e}");
    }
}

/// The pending-succession inputs for a succeeded settle: the heir from the
/// payload's `heir` key (the spawned row's name, plumbed by dispatch), the
/// predecessor from the plan's first vacated holder_id. A payload without
/// the heir key (an old caller) carries no pending record and keeps
/// today's shape.
fn succession_pending(payload: &Value) -> Option<crate::crown_names::PendingSuccession> {
    let heir = payload
        .get("heir")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let plan = payload.get("plan")?;
    let (name, session) = parse_holder_ids(plan.get("holder_ids"))
        .ok()?
        .into_iter()
        .next()?;
    Some(crate::crown_names::PendingSuccession {
        heir_name: heir.to_string(),
        heir_session: heir_session(heir),
        predecessor_name: name,
        predecessor_session: session,
        ts: crate::daemon::now_rfc3339_like(),
    })
}

/// The heir row's session id at settle time, so the succession revert's
/// join keys on identity. A row not yet in the registry (a settle racing
/// the spawn row's write, or a test with no declared home) carries no
/// session; the revert then falls back to the name join as before.
fn heir_session(heir: &str) -> Option<String> {
    let home = crate::paths::AgentsHome::from_env_opt()?;
    let registry = crate::state::try_load_registry(&home.registry_json()).ok()??;
    match crate::loop_reign::live_name_join(&registry.entries, heir) {
        crate::loop_reign::NameJoin::One(row) => row.harness_session_id.clone(),
        _ => None,
    }
}

fn resolve_with_projects(
    payload: &Value,
    projects: &Result<HashMap<String, String>, String>,
) -> Result<Value, String> {
    if payload.get("plan").is_some() {
        return apply_with_projects(payload, projects);
    }
    plan_with_projects(payload, projects)
}

fn occupancy(
    rows: &[Value],
    scope: &str,
    caller: &Caller,
    exclude_name: Option<&str>,
    projects: &Result<HashMap<String, String>, String>,
) -> Occupancy {
    let empty_map: HashMap<String, String> = HashMap::new();
    let map = projects.as_ref().unwrap_or(&empty_map);
    let mut clear_terminal = Vec::new();
    let mut holders = Vec::new();
    let mut rivals = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let name = row.get("name").and_then(Value::as_str).unwrap_or("");
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        let row_scope = row.get("crown_scope").and_then(Value::as_str);
        if row_scope == Some(scope) && TERMINAL_STATUSES.contains(&status) {
            clear_terminal.push((index, name.to_string()));
            continue;
        }
        if TERMINAL_STATUSES.contains(&status) || Some(name) == exclude_name {
            continue;
        }
        if row_scope == Some(scope) {
            holders.push(Holder {
                index,
                name: name.to_string(),
                session: row
                    .get("harness_session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        let Some(held) = row_scope else {
            continue;
        };
        if held.trim().is_empty() || held == scope {
            continue;
        }
        // An agent delegating one member of its own crown does not rival
        // itself; grant_error verified strict containment before crown-settle
        // runs, and promote_existing_session skips the grantor the same way.
        if let Caller::Agent(me) = caller {
            if name == me && !same_territory(held, scope, map) {
                continue;
            }
        }
        let rival = if projects.is_ok() {
            crown_rivals_pub(held, None, scope, None, map)
        } else {
            scopes_overlap(held, scope, &empty_map)
        };
        if rival {
            rivals.push((name.to_string(), held.to_string()));
        }
    }
    holders.sort_by(|a, b| (&a.name, &a.session).cmp(&(&b.name, &b.session)));
    clear_terminal.sort_by_key(|(index, _)| *index);
    rivals.sort();
    Occupancy {
        clear_terminal,
        holders,
        rivals,
    }
}

fn holder_names(holders: &[Holder]) -> Vec<String> {
    holders.iter().map(|holder| holder.name.clone()).collect()
}

fn holder_ids(holders: &[Holder]) -> Vec<Value> {
    holders
        .iter()
        .map(|holder| {
            json!({
                "name": holder.name,
                "harness_session_id": holder.session,
            })
        })
        .collect()
}

fn plan_answer(
    outcome: &str,
    occupancy: Occupancy,
    vacate: Vec<String>,
    refusal: Value,
    caller: Value,
) -> Value {
    let holders = holder_names(&occupancy.holders);
    let holder_ids = holder_ids(&occupancy.holders);
    let clear_terminal = occupancy
        .clear_terminal
        .iter()
        .map(|(_, name)| name.clone())
        .collect::<Vec<_>>();
    json!({
        "outcome": outcome,
        "clear_terminal": clear_terminal,
        "holders": holders,
        "holder_ids": holder_ids,
        "caller": caller,
        "vacate": vacate,
        "rivals": occupancy.rivals.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(),
        "refusal": refusal,
    })
}

fn plan_with_projects(
    payload: &Value,
    projects: &Result<HashMap<String, String>, String>,
) -> Result<Value, String> {
    let scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "crown-settle: payload needs a non-empty scope".to_string())?;
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "crown-settle: payload needs a rows array".to_string())?;
    let caller = parse_caller(payload.get("caller"))
        .ok_or_else(|| "crown-settle: payload needs a caller of kind human or agent".to_string())?;
    let succession = payload
        .get("succession")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let exclude_name = payload.get("exclude_name").and_then(Value::as_str);
    let occupancy = occupancy(rows, scope, &caller, exclude_name, projects);
    let caller_value = payload.get("caller").cloned().unwrap_or(Value::Null);
    let holders = holder_names(&occupancy.holders);

    if let Some((first, _)) = occupancy.rivals.first() {
        let listed = occupancy
            .rivals
            .iter()
            .map(|(n, h)| format!("{n} (holding {h:?})"))
            .collect::<Vec<_>>()
            .join(", ");
        let refusal = format!(
            "scope {scope:?} overlaps territory held by live row(s) {listed}. Two live \
             crowns would rule the same members, so this spawn refuses before launch. \
             --succeed hands down only an identical crown, never part of a wider or \
             overlapping one. Re-scope the holder (fno agents org promote {first} --scope \
             <other territory>), run fno agents reconcile if it looks dead, or fno \
             agents stop {first}, then retry."
        );
        return Ok(plan_answer(
            "declined",
            occupancy,
            Vec::new(),
            json!(refusal),
            caller_value,
        ));
    }

    if holders.is_empty() {
        return Ok(plan_answer(
            "granted",
            occupancy,
            Vec::new(),
            Value::Null,
            caller_value,
        ));
    }

    if succession {
        match &caller {
            Caller::Agent(name) if holders.iter().all(|h| h == name) => {
                return Ok(plan_answer(
                    "succeeded",
                    occupancy,
                    vec![name.clone()],
                    Value::Null,
                    caller_value,
                ));
            }
            Caller::Human => {
                return Ok(plan_answer(
                    "succeeded",
                    occupancy,
                    holders,
                    Value::Null,
                    caller_value,
                ));
            }
            Caller::Agent(_) => {}
        }
    }

    let refusal = match &caller {
        Caller::Human => format!(
            "scope {scope:?} is held by live row(s) {holders:?}. This spawn would launch \
             an heir with no crown, so it refuses. Re-run with --succeed to transfer the \
             crown to the new session, or choose a scope nobody holds."
        ),
        Caller::Agent(_) => format!(
            "scope {scope:?} is held by live row(s) {holders:?}, not by this session, so \
             this session cannot hand it down. Only the holder (spawn --crown --succeed \
             from its own session) or an attended shell (spawn --crown --succeed) can \
             transfer it."
        ),
    };

    Ok(plan_answer(
        "declined",
        occupancy,
        Vec::new(),
        json!(refusal),
        caller_value,
    ))
}

fn parse_holder_ids(value: Option<&Value>) -> Result<Vec<(String, Option<String>)>, String> {
    let ids = value
        .and_then(Value::as_array)
        .ok_or_else(|| "crown-settle: plan needs holder_ids array".to_string())?;
    let mut parsed = Vec::with_capacity(ids.len());
    for id in ids {
        let name = id
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "crown-settle: holder_ids entries need a name".to_string())?;
        let session = match id.get("harness_session_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(session)) => Some(session.clone()),
            _ => {
                return Err("crown-settle: holder_ids session must be a string or null".to_string())
            }
        };
        parsed.push((name.to_string(), session));
    }
    parsed.sort();
    Ok(parsed)
}

fn apply_with_projects(
    payload: &Value,
    projects: &Result<HashMap<String, String>, String>,
) -> Result<Value, String> {
    let scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .filter(|scope| !scope.is_empty())
        .ok_or_else(|| "crown-settle: payload needs a non-empty scope".to_string())?;
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "crown-settle: payload needs a rows array".to_string())?;
    let plan = payload
        .get("plan")
        .and_then(Value::as_object)
        .ok_or_else(|| "crown-settle: plan must be an object".to_string())?;
    let caller_value = plan.get("caller");
    let caller = parse_caller(caller_value)
        .ok_or_else(|| "crown-settle: plan needs caller of kind human or agent".to_string())?;
    let expected = parse_holder_ids(plan.get("holder_ids"))?;
    let outcome = plan
        .get("outcome")
        .and_then(Value::as_str)
        .filter(|outcome| matches!(*outcome, "granted" | "succeeded" | "declined"))
        .ok_or_else(|| "crown-settle: plan needs valid outcome".to_string())?;
    let exclude_name = payload.get("exclude_name").and_then(Value::as_str);
    let occupancy = occupancy(rows, scope, &caller, exclude_name, projects);
    let clear_terminal_rows = occupancy
        .clear_terminal
        .iter()
        .map(|(index, _)| *index)
        .collect::<Vec<_>>();
    let current = occupancy
        .holders
        .iter()
        .map(|holder| (holder.name.clone(), holder.session.clone()))
        .collect::<Vec<_>>();
    let planned_vacate = plan
        .get("vacate")
        .and_then(Value::as_array)
        .ok_or_else(|| "crown-settle: plan needs vacate array".to_string())?
        .iter()
        .map(|name| {
            name.as_str()
                .ok_or_else(|| "crown-settle: vacate entries need names".to_string())
        })
        .collect::<Result<HashSet<_>, _>>()?;
    if outcome == "succeeded"
        && occupancy
            .holders
            .iter()
            .any(|holder| !planned_vacate.contains(holder.name.as_str()))
    {
        return Err("crown-settle: plan vacate omits a holder".to_string());
    }
    let (outcome, vacate_rows) = if !occupancy.rivals.is_empty() {
        ("declined", Vec::new())
    } else if current == expected {
        let vacate_rows = if outcome == "succeeded" {
            occupancy
                .holders
                .iter()
                .filter(|holder| planned_vacate.contains(holder.name.as_str()))
                .map(|holder| holder.index)
                .collect()
        } else {
            Vec::new()
        };
        (outcome, vacate_rows)
    } else if !occupancy.holders.is_empty() {
        ("declined", Vec::new())
    } else {
        ("granted", Vec::new())
    };
    // Succession re-homes the court: every live child whose CURRENT owner
    // (the provenance owner, else the birth edge on a pre-v33 row) names a
    // vacated holder's session follows the crown to the heir. The birth edge
    // itself stays history - ownership is what the sideline reads.
    let vacated_sessions: HashSet<String> = if outcome == "succeeded" {
        occupancy
            .holders
            .iter()
            .filter(|holder| planned_vacate.contains(holder.name.as_str()))
            .filter_map(|holder| holder.session.as_deref().map(str::to_ascii_lowercase))
            .collect()
    } else {
        HashSet::new()
    };
    let reown_rows: Vec<usize> = if vacated_sessions.is_empty() {
        Vec::new()
    } else {
        rows.iter()
            .enumerate()
            .filter(|(_, row)| {
                let status = row.get("status").and_then(Value::as_str).unwrap_or("");
                if TERMINAL_STATUSES.contains(&status) {
                    return false;
                }
                if Some(row.get("name").and_then(Value::as_str).unwrap_or("")) == exclude_name {
                    return false;
                }
                let owner = row
                    .get("spawn_provenance")
                    .and_then(|p| p.get("owner"))
                    .and_then(|o| o.get("session_id"))
                    .and_then(Value::as_str)
                    .or_else(|| row.get("spawned_by_session").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_ascii_lowercase);
                owner.is_some_and(|o| vacated_sessions.contains(&o))
            })
            .map(|(index, _)| index)
            .collect()
    };
    // The owner block the applier stamps onto each reowned child, composed
    // here from the heir's own identity so the one session-id rule (the
    // payload's `heir_identity`, read like every other field) writes the
    // block and Python only assigns it. Blank session id answers null: an
    // unaddressable heir re-creates the orphan the reown exists to prevent.
    let identity = payload.get("heir_identity");
    let heir_session = identity
        .and_then(|i| i.get("session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let reown_owner = heir_session.map(|session_id| {
        json!({
            "kind": "session",
            "harness": identity.and_then(|i| i.get("harness")).and_then(Value::as_str),
            "session_id": session_id,
            "cwd": identity.and_then(|i| i.get("cwd")).and_then(Value::as_str),
        })
    });
    Ok(json!({
        "outcome": outcome,
        "clear_terminal_rows": clear_terminal_rows,
        "vacate_rows": vacate_rows,
        "reown_rows": reown_rows,
        "reown_owner": reown_owner,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn row(name: &str, scope: &str, status: &str) -> Value {
        json!({"name": name, "crown_scope": scope, "status": status})
    }

    fn crown_registry(tmp: &std::path::Path, agents: Value) -> std::path::PathBuf {
        let reg = tmp.join("registry.json");
        let doc = json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": agents,
        });
        std::fs::write(&reg, doc.to_string()).unwrap();
        reg
    }

    fn crown_store(tmp: &std::path::Path) -> std::path::PathBuf {
        tmp.join("crown_names.json")
    }

    fn named_record_fixture(tmp: &std::path::Path) {
        std::fs::write(
            crown_store(tmp),
            serde_json::to_string(&json!({
                "version": 1,
                "crowns": {"x-aaaa": {
                    "name": "barnaby", "regnal": 1,
                    "holder_session": "sess-old",
                    "nodes": [], "updated_at": "2026-09-23T20:00:00Z"}},
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn agents_with_succession_rows() -> Value {
        json!([
            {"name": "king-old", "status": "live", "crown_scope": "x-aaaa",
             "crown_level": 2, "cwd": "/repo", "harness": "claude",
             "harness_session_id": "sess-old",
             "created_at": "2026-09-23T20:00:00Z"},
            {"name": "king-heir", "status": "live", "crown_scope": "x-aaaa",
             "crown_level": 2, "cwd": "/repo", "harness": "claude",
             "harness_session_id": "sess-new",
             "created_at": "2026-09-23T20:00:00Z"}
        ])
    }

    #[test]
    fn grant_rows() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"}, "rows": [],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["vacate"], json!([]));
        assert!(out["refusal"].is_null());

        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": true,
            "caller": {"kind": "agent", "name": "king-a"},
            "rows": [row("king-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "succeeded");
        assert_eq!(out["vacate"], json!(["king-a"]));

        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": true,
            "caller": {"kind": "human"},
            "rows": [row("king-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "succeeded");
        assert_eq!(out["vacate"], json!(["king-a"]));

        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"},
            "rows": [row("dead-king", "fno", "exited")],
        }))
        .unwrap();
        assert_eq!(out["clear_terminal"], json!(["dead-king"]));
        assert_eq!(out["outcome"], "granted");

        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"},
            "rows": [row("king-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("king-a"));
        assert!(refusal.contains("--succeed"));

        // holders = ["king-a", "king-b"]; the caller matches one but not all,
        // so succession must fall through to the ordinary decline rather
        // than succeeding a partial match. The refusal names the holder and
        // the --succeed flag, the same fall-through arm a scope the caller
        // does not hold at all takes.
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": true,
            "caller": {"kind": "agent", "name": "king-a"},
            "rows": [row("king-a", "fno", "busy"), row("king-b", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate"], json!([]));
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("king-a"));
        assert!(refusal.contains("--succeed"));
    }

    #[test]
    fn revive_rows() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"}, "exclude_name": "heir",
            "rows": [row("heir", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["holders"], json!([]));
        assert_eq!(out["outcome"], "granted");

        assert!(resolve(&json!({
            "kind": "crown-settle", "caller": {"kind": "human"}, "rows": [],
        }))
        .is_err());
    }

    // -- rivals: occupancy beyond the exact scope string, map injected --

    fn settle(payload: Value, projects: Result<HashMap<String, String>, String>) -> Value {
        resolve_with_projects(&payload, &projects).unwrap()
    }

    #[test]
    fn rival_rows() {
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["king-a"]));
        assert_eq!(out["vacate"], json!([]));
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("king-a"));
        assert!(refusal.contains("epic-a,epic-b"));

        let rows = [row("king-a", "epic-a", "busy")];
        for caller in [
            json!({"kind": "human"}),
            json!({"kind": "agent", "name": "other"}),
        ] {
            let out = settle(
                json!({
                    "kind": "crown-settle", "scope": "epic-a,epic-b", "succession": false,
                    "caller": caller, "rows": rows,
                }),
                Ok(HashMap::new()),
            );
            assert_eq!(out["outcome"], "declined");
            assert_eq!(out["rivals"], json!(["king-a"]));
        }

        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": true,
                "caller": {"kind": "human"},
                "rows": [row("king-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate"], json!([]));
        assert!(out["refusal"]
            .as_str()
            .unwrap()
            .contains("--succeed hands down only an identical crown"));

        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "agent", "name": "king-a"},
                "rows": [row("king-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["rivals"], json!([]));

        let projects = Ok(HashMap::from([
            ("alpha".to_string(), "alpha".to_string()),
            ("beta".to_string(), "beta".to_string()),
        ]));
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-p", "alpha,beta", "busy")],
            }),
            projects,
        );
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["rivals"], json!([]));

        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-p", "alpha,beta", "busy")],
            }),
            Err("no work.workspaces in any candidate config.toml".to_string()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["king-p"]));

        let projects = Ok(HashMap::from([
            ("alpha".to_string(), "alpha".to_string()),
            ("a".to_string(), "alpha".to_string()),
        ]));
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-a", "a", "busy")],
            }),
            projects,
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["king-a"]));

        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("dead-king", "epic-a,epic-b", "exited")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["rivals"], json!([]));
        assert_eq!(out["clear_terminal"], json!([]));
    }

    #[test]
    fn plan_rows() {
        let caller = json!({"kind": "human"});
        let mut holder = row("king-a", "epic-a", "busy");
        holder["harness_session_id"] = json!("sess-a");
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "epic-a", "succession": true,
            "caller": caller, "rows": [holder],
        }))
        .unwrap();
        assert_eq!(out["holders"], json!(["king-a"]));
        assert_eq!(
            out["holder_ids"],
            json!([{
                "name": "king-a", "harness_session_id": "sess-a",
            }])
        );
        assert_eq!(out["caller"], json!({"kind": "human"}));

        // Succession re-homes the court. A live child whose CURRENT
        // owner (the provenance owner, else the birth edge on a pre-v33 row)
        // names a vacated holder's session follows the crown; a child already
        // owned by another session stays put; a terminal row never moves.
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "epic-a",
            "heir_identity": {"harness": "codex", "session_id": "sess-heir", "cwd": "/w"},
            "plan": {
                "caller": {"kind": "human"},
                "holder_ids": [{"name": "king-a", "harness_session_id": "sess-a"}],
                "outcome": "succeeded", "vacate": ["king-a"],
            },
            "rows": [
                {"name": "king-a", "crown_scope": "epic-a", "status": "busy",
                 "harness_session_id": "sess-a"},
                {"name": "w5", "status": "busy",
                 "spawned_by_session": "sess-a",
                 "spawn_provenance": {"owner": {"kind": "session",
                                                "session_id": "sess-a"}}},
                {"name": "elsewhere", "status": "busy",
                 "spawned_by_session": "sess-a",
                 "spawn_provenance": {"owner": {"kind": "session",
                                                "session_id": "sess-other"}}},
                {"name": "legacy", "status": "busy",
                 "spawned_by_session": "sess-a"},
                {"name": "corpse", "status": "exited",
                 "spawned_by_session": "sess-a"}
            ],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "succeeded");
        assert_eq!(out["vacate_rows"], json!([0]));
        assert_eq!(out["reown_rows"], json!([1, 3]));
        assert_eq!(out["reown_owner"]["session_id"], "sess-heir");
        assert_eq!(out["reown_owner"]["kind"], "session");

        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "epic-a",
            "plan": {
                "caller": {"kind": "human"},
                "holder_ids": [{"name": "king-a", "harness_session_id": "sess-a"}],
                "outcome": "succeeded", "vacate": ["king-a"],
            },
            "rows": [{"name": "king-a", "crown_scope": "epic-a", "status": "busy",
                      "harness_session_id": "sess-b"}],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate_rows"], json!([]));

        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "epic-a",
            "plan": {
                "caller": {"kind": "human"},
                "holder_ids": [{"name": "king-a", "harness_session_id": "sess-a"}],
                "outcome": "succeeded", "vacate": ["king-a"],
            },
            "rows": [
                {"name": "other", "crown_scope": null, "status": "busy"},
                {"name": "dead-king", "crown_scope": "epic-a", "status": "exited"},
            ],
        }))
        .unwrap();
        assert_eq!(out["clear_terminal_rows"], json!([1]));
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["vacate_rows"], json!([]));

        let out = resolve_with_projects(
            &json!({
                "kind": "crown-settle", "scope": "epic-a",
                "plan": {
                    "caller": {"kind": "human"}, "holder_ids": [],
                    "outcome": "granted", "vacate": [],
                },
                "rows": [{"name": "king-b", "crown_scope": "epic-a,epic-b",
                          "status": "busy", "harness_session_id": "sess-b"}],
            }),
            &Ok(HashMap::new()),
        )
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate_rows"], json!([]));

        let rows = json!([{
            "name": "king-a", "crown_scope": "epic-a", "status": "busy",
            "harness_session_id": "sess-a",
        }]);
        for (plan, key) in [
            (json!({"holder_ids": [], "outcome": "granted"}), "caller"),
            (
                json!({"caller": {"kind": "unknown"}, "holder_ids": [], "outcome": "granted"}),
                "caller",
            ),
            (
                json!({"caller": {"kind": "human"}, "outcome": "granted"}),
                "holder_ids",
            ),
            (
                json!({"caller": {"kind": "human"}, "holder_ids": "bad", "outcome": "granted"}),
                "holder_ids",
            ),
            (
                json!({"caller": {"kind": "human"}, "holder_ids": []}),
                "outcome",
            ),
            (
                json!({"caller": {"kind": "human"}, "holder_ids": [], "outcome": "unknown"}),
                "outcome",
            ),
            (
                json!({
                    "caller": {"kind": "human"},
                    "holder_ids": [{"name": "king-a", "harness_session_id": "sess-a"}],
                    "outcome": "succeeded",
                }),
                "vacate",
            ),
            (
                json!({"caller": {"kind": "human"}, "holder_ids": [],
                       "outcome": "granted", "vacate": "bad"}),
                "vacate",
            ),
            (
                json!({
                    "caller": {"kind": "human"},
                    "holder_ids": [{"name": "king-a", "harness_session_id": "sess-a"}],
                    "outcome": "succeeded", "vacate": [],
                }),
                "vacate",
            ),
        ] {
            let error = resolve(&json!({
                "kind": "crown-settle", "scope": "epic-a", "rows": rows,
                "plan": plan,
            }))
            .unwrap_err();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn record_rows() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _reg = crown_registry(tmp.path(), agents_with_succession_rows());
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "crown-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "agent", "name": "king-heir"},
                    "holder_ids": [{"name": "king-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["king-old"],
                },
                "rows": [{
                    "name": "king-old", "crown_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &crown_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "succeeded");
        let store = std::fs::read_to_string(crown_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        assert_eq!(doc["crowns"]["x-aaaa"]["regnal"], json!(2));
        assert_eq!(doc["crowns"]["x-aaaa"]["holder_session"], json!(null));

        // A payload carrying the heir key pends the succession with the
        // predecessor identity from the plan's holder_ids.
        let answer = resolve_at(
            &json!({
                "kind": "crown-settle", "scope": "x-aaaa", "heir": "king-heir",
                "plan": {
                    "caller": {"kind": "agent", "name": "king-heir"},
                    "holder_ids": [{"name": "king-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["king-old"],
                },
                "rows": [{
                    "name": "king-old", "crown_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &crown_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "succeeded");
        let store = std::fs::read_to_string(crown_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        let pending = &doc["crowns"]["x-aaaa"]["pending_succession"];
        assert_eq!(pending["heir_name"], json!("king-heir"));
        assert_eq!(pending["predecessor_name"], json!("king-old"));
        assert_eq!(pending["predecessor_session"], json!("sess-old"));
        assert!(pending["ts"].is_string());
        // Without the heir key (an old caller) today's shape holds: no
        // pending record is written.
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "crown-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "agent", "name": "king-heir"},
                    "holder_ids": [{"name": "king-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["king-old"],
                },
                "rows": [{
                    "name": "king-old", "crown_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &crown_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "succeeded");
        let store = std::fs::read_to_string(crown_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        assert!(doc["crowns"]["x-aaaa"].get("pending_succession").is_none());

        let tmp = tempfile::TempDir::new().unwrap();
        let _reg = crown_registry(tmp.path(), json!([{"name": "w", "status": "exited"}]));
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "crown-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "human"},
                    "holder_ids": [],
                    "outcome": "granted", "vacate": [],
                },
                "rows": [],
            }),
            &crown_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "granted");
        let store = std::fs::read_to_string(crown_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        assert!(doc["crowns"].get("x-aaaa").is_none(), "{store}");

        let tmp = tempfile::TempDir::new().unwrap();
        // A file where the store's parent dir would be: the write fails.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let store = blocker.join("crown_names.json");
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "crown-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "agent", "name": "king-heir"},
                    "holder_ids": [{"name": "king-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["king-old"],
                },
                "rows": [{
                    "name": "king-old", "crown_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &store,
        )
        .unwrap();
        assert_eq!(answer["outcome"], "succeeded");
    }
}
