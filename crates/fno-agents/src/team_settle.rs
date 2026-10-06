//! `team-settle`: whether a teamed spawn is granted, transfers, or refuses,
//! reached as payload kind `team-settle` on the existing `spawn-overlay`
//! verb (law d-fe66560a bars new top-level verbs, hidden ones included).
//!
//! A port of Python's `settle_spawn_team` (`cli/src/fno/agents/team.py`)
//! with one new branch: a human caller with `succession` may transfer a
//! team away from any live holder, the same authority `grant_error` already
//! gives a human to grant any scope (a human may grant what nobody above it
//! could check). An agent caller may only succeed itself: every live holder
//! of the scope must already be that agent.
//!
//! Rows arrive as plain JSON here, not a typed registry row. A row's
//! terminal read answers through `row_verdict::finished_json`, the
//! role-vacancy door: the reversible word (Orphaned) re-answers on
//! process evidence, and every other status keeps the legacy
//! `announce::TERMINAL_STATUSES` word list.
//!
//! Occupancy is the exact-scope holders plus ladder-aware rivals: through
//! `loop_lead::team_rivals`, a live team over overlapping territory
//! declines the spawn before the succession branch, and an agent's own
//! wider team is its delegation, not a rival.
//!
//! Team-settle answers twice: before launch against a snapshot, then again
//! under the registry lock with the plan attached. Holder identity is the
//! name and harness session id together because registry names are reclaimable.
//! Rows that both lack a session id still compare by name alone.

use crate::loop_lead::{same_territory, scopes_overlap, team_rivals_pub};
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

/// Decide occupancy for one teamed spawn over `scope`, then apply the
/// team-name effect (a succeeded succession carries the name with a
/// generation bump; a fresh grant forgets it). See the module doc for the
/// caller/succession rules and the request/answer shapes.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    if payload.get("plan").is_none() {
        // The plan/preview path reads no store and no home env (existing
        // unit tests run it with no declared agents home).
        let projects = crate::org_board::project_map(&std::env::current_dir().unwrap_or_default());
        return resolve_with_projects(payload, &projects);
    }
    let store = crate::paths::AgentsHome::from_env_opt().map(|h| h.team_names_json());
    let projects = crate::org_board::project_map(&std::env::current_dir().unwrap_or_default());
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
    let projects = crate::org_board::project_map(&std::env::current_dir().unwrap_or_default());
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
            let result = crate::team_names::carry_succession(store, scope, pending.clone());
            if let Ok(Some(p)) = result.as_ref() {
                crate::succession_txn::announce(scope, p);
                crate::succession_txn::transferred(scope, p);
            }
            result.map(|_| ())
        }
        Some("granted") => {
            let forgotten = crate::team_names::forget(store, scope);
            let successor = payload
                .get("successor")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty());
            // The successor's session rides `successor_identity` (dispatch plumbs the
            // row's own id); the carry is registry-free, so the apply path
            // never waits on the registry lock.
            let session = payload
                .get("successor_identity")
                .and_then(|i| i.get("session_id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match (forgotten, successor) {
                (Ok(()), Some(name)) => {
                    // A fresh grant carries the promoted row's own name, so
                    // the team never lands anonymous.
                    crate::team_names::carry_holder_name(store, session, 2, scope, name).map(|_| ())
                }
                (result, _) => result,
            }
        }
        _ => Ok(()),
    };
    if let Err(e) = effect {
        eprintln!("team-settle: team names: {e}");
    }
}

/// The pending-succession inputs for a succeeded settle: the successor from the
/// payload's `successor` key (the spawned row's name, plumbed by dispatch), the
/// predecessor from the plan's first vacated holder_id. A payload without
/// the successor key (an old caller) carries no pending record and keeps
/// today's shape.
fn succession_pending(payload: &Value) -> Option<crate::team_names::PendingSuccession> {
    let successor = payload
        .get("successor")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let plan = payload.get("plan")?;
    let (name, session) = parse_holder_ids(plan.get("holder_ids"))
        .ok()?
        .into_iter()
        .next()?;
    Some(crate::team_names::PendingSuccession {
        successor_name: successor.to_string(),
        successor_session: payload
            .get("successor_identity")
            .and_then(|i| i.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| successor_session(successor)),
        predecessor_name: name,
        predecessor_session: session,
        ts: crate::daemon::now_rfc3339_like(),
    })
}

/// The successor row's session id at settle time, so the succession revert's
/// join keys on identity. A row not yet in the registry (a settle racing
/// the spawn row's write, or a test with no declared home) carries no
/// session; the revert then falls back to the name join as before.
fn successor_session(successor: &str) -> Option<String> {
    let home = crate::paths::AgentsHome::from_env_opt()?;
    let registry = crate::state::try_load_registry(&home.registry_json()).ok()??;
    match crate::lead_state::live_name_join(&registry.entries, successor) {
        crate::lead_state::NameJoin::One(row) => row.harness_session_id.clone(),
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
        let row_scope = row.get("role_scope").and_then(Value::as_str);
        if row_scope == Some(scope) && crate::row_verdict::finished_json(row) {
            clear_terminal.push((index, name.to_string()));
            continue;
        }
        if crate::row_verdict::finished_json(row) || Some(name) == exclude_name {
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
        // An agent delegating one member of its own team does not rival
        // itself; grant_error verified strict containment before team-settle
        // runs, and promote_existing_session skips the grantor the same way.
        if let Caller::Agent(me) = caller {
            if name == me && !same_territory(held, scope, map) {
                continue;
            }
        }
        let rival = if projects.is_ok() {
            team_rivals_pub(held, None, scope, None, map)
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
        .ok_or_else(|| "team-settle: payload needs a non-empty scope".to_string())?;
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "team-settle: payload needs a rows array".to_string())?;
    let caller = parse_caller(payload.get("caller"))
        .ok_or_else(|| "team-settle: payload needs a caller of kind human or agent".to_string())?;
    let succession = payload
        .get("succession")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let exclude_name = payload.get("exclude_name").and_then(Value::as_str);
    let occupancy = occupancy(rows, scope, &caller, exclude_name, projects);
    let caller_value = payload.get("caller").cloned().unwrap_or(Value::Null);
    let holders = holder_names(&occupancy.holders);
    let proposed_name = payload
        .get("proposed_name")
        .and_then(Value::as_str)
        .unwrap_or("");

    if succession
        && (!crate::team_names::valid_person_name(proposed_name)
            || rows.iter().any(|row| {
                !crate::row_verdict::finished_json(row)
                    && row.get("role_scope").and_then(Value::as_str).is_some()
                    && row
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.eq_ignore_ascii_case(proposed_name))
            }))
    {
        return Ok(plan_answer("declined", occupancy, Vec::new(),
            json!("--hand-off requires --name <Name>: the outgoing lead chooses a fresh person name, distinct from every live lead"), caller_value));
    }

    if let Some((first, _)) = occupancy.rivals.first() {
        let listed = occupancy
            .rivals
            .iter()
            .map(|(n, h)| format!("{n} (holding {h:?})"))
            .collect::<Vec<_>>()
            .join(", ");
        let refusal = format!(
            "scope {scope:?} overlaps territory held by live row(s) {listed}. Two live \
             teams would rule the same members, so this spawn refuses before launch. \
             --hand-off hands down only an identical team, never part of a wider or \
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
             a successor with no team, so it refuses. Re-run with --hand-off to transfer the \
             team to the new session, or choose a scope nobody holds."
        ),
        Caller::Agent(_) => format!(
            "scope {scope:?} is held by live row(s) {holders:?}, not by this session, so \
             this session cannot hand it down. Only the holder (spawn --promote \
             --hand-off from its own session) or an attended shell (spawn --promote \
             --hand-off) can transfer it."
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
        .ok_or_else(|| "team-settle: plan needs holder_ids array".to_string())?;
    let mut parsed = Vec::with_capacity(ids.len());
    for id in ids {
        let name = id
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "team-settle: holder_ids entries need a name".to_string())?;
        let session = match id.get("harness_session_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(session)) => Some(session.clone()),
            _ => return Err("team-settle: holder_ids session must be a string or null".to_string()),
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
        .ok_or_else(|| "team-settle: payload needs a non-empty scope".to_string())?;
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "team-settle: payload needs a rows array".to_string())?;
    let plan = payload
        .get("plan")
        .and_then(Value::as_object)
        .ok_or_else(|| "team-settle: plan must be an object".to_string())?;
    let caller_value = plan.get("caller");
    let caller = parse_caller(caller_value)
        .ok_or_else(|| "team-settle: plan needs caller of kind human or agent".to_string())?;
    let expected = parse_holder_ids(plan.get("holder_ids"))?;
    let outcome = plan
        .get("outcome")
        .and_then(Value::as_str)
        .filter(|outcome| matches!(*outcome, "granted" | "succeeded" | "declined"))
        .ok_or_else(|| "team-settle: plan needs valid outcome".to_string())?;
    let successor = payload
        .get("successor")
        .and_then(Value::as_str)
        .unwrap_or("");
    if outcome == "succeeded"
        && (!crate::team_names::valid_person_name(successor)
            || expected
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(successor)))
    {
        return Ok(
            json!({"outcome":"declined", "clear_terminal_rows":[], "vacate_rows":[],
            "reown_rows":[], "reown_owner":null, "refusal":"--hand-off requires a fresh person name"}),
        );
    }
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
        .ok_or_else(|| "team-settle: plan needs vacate array".to_string())?
        .iter()
        .map(|name| {
            name.as_str()
                .ok_or_else(|| "team-settle: vacate entries need names".to_string())
        })
        .collect::<Result<HashSet<_>, _>>()?;
    if outcome == "succeeded"
        && occupancy
            .holders
            .iter()
            .any(|holder| !planned_vacate.contains(holder.name.as_str()))
    {
        return Err("team-settle: plan vacate omits a holder".to_string());
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
    // Succession re-homes the org: every live child whose CURRENT owner (the
    // provenance owner) names a vacated holder's session follows the team to
    // the successor. The birth edge itself stays history - ownership is what the
    // sideline reads.
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
                if crate::row_verdict::finished_json(row) {
                    return false;
                }
                if Some(row.get("name").and_then(Value::as_str).unwrap_or("")) == exclude_name {
                    return false;
                }
                // Reown only rows that already carry provenance. Falling back
                // to the birth edge selected provenance-less rows (adopt,
                // pre-v33), and the applier then forked an origin-less block
                // onto them that the typed reader rejects row-wide.
                let owner = row
                    .get("spawn_provenance")
                    .and_then(|p| p.get("owner"))
                    .and_then(|o| o.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_ascii_lowercase);
                owner.is_some_and(|o| vacated_sessions.contains(&o))
            })
            .map(|(index, _)| index)
            .collect()
    };
    // The owner block the applier stamps onto each reowned child, composed
    // here from the successor's own identity so the one session-id rule (the
    // payload's `successor_identity`, read like every other field) writes the
    // block and Python only assigns it. Blank session id answers null: an
    // unaddressable successor re-creates the orphan the reown exists to prevent.
    let identity = payload.get("successor_identity");
    let successor_session = identity
        .and_then(|i| i.get("session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let reown_owner = successor_session.map(|session_id| {
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

    #[test]
    fn handoff_requires_an_explicit_new_person_name_before_any_transfer() {
        let tmp = tempfile::tempdir().unwrap();
        let mut payload = json!({"scope":"scope", "succession":true,
            "caller":{"kind":"agent", "name":"Jordan"},
            "rows":[{"name":"Jordan", "status":"live", "role_scope":"scope",
                "harness_session_id":"old-session"}]});
        for candidate in [None, Some("warm-viper"), Some("Jordan")] {
            payload["proposed_name"] = candidate.map(Value::from).unwrap_or(Value::Null);
            let refused = resolve_at(&payload, &team_store(tmp.path())).unwrap();
            assert_eq!(refused["outcome"], "declined");
            assert!(refused["refusal"].as_str().unwrap().contains("--name"));
            assert!(!team_store(tmp.path()).exists());
        }
        payload["proposed_name"] = json!("Avery");
        let plan = resolve_at(&payload, &team_store(tmp.path())).unwrap();
        assert_eq!(plan["outcome"], "succeeded");
        payload["plan"] = plan;
        payload["successor"] = json!("Avery");
        payload["successor_identity"] = json!({"session_id":"new-session"});
        let applied = resolve_at(&payload, &team_store(tmp.path())).unwrap();
        assert_eq!(applied["outcome"], "succeeded");
        let store = crate::team_names::snapshot(&team_store(tmp.path())).unwrap();
        assert_eq!(store["teams"]["scope"]["name"], "Avery");
        assert_eq!(
            store["teams"]["scope"]["pending_succession"]["successor_name"],
            "Avery"
        );
        assert_eq!(
            store["teams"]["scope"]["pending_succession"]["successor_session"],
            "new-session"
        );
    }

    fn row(name: &str, scope: &str, status: &str) -> Value {
        json!({"name": name, "role_scope": scope, "status": status})
    }

    fn team_registry(tmp: &std::path::Path, agents: Value) -> std::path::PathBuf {
        let reg = tmp.join("registry.json");
        let doc = json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": agents,
        });
        std::fs::write(&reg, doc.to_string()).unwrap();
        reg
    }

    fn team_store(tmp: &std::path::Path) -> std::path::PathBuf {
        tmp.join("team_names.json")
    }

    fn named_record_fixture(tmp: &std::path::Path) {
        std::fs::write(
            team_store(tmp),
            serde_json::to_string(&json!({
                "version": 1,
                "teams": {"x-aaaa": {
                    "name": "barnaby", "generation": 1,
                    "holder_session": "sess-old",
                    "nodes": [], "updated_at": "2026-09-23T20:00:00Z"}},
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn agents_with_succession_rows() -> Value {
        json!([
            {"name": "lead-old", "status": "live", "role_scope": "x-aaaa",
             "role_level": 2, "cwd": "/repo", "harness": "claude",
             "harness_session_id": "sess-old",
             "created_at": "2026-09-23T20:00:00Z"},
            {"name": "Avery", "status": "live", "role_scope": "x-aaaa",
             "role_level": 2, "cwd": "/repo", "harness": "claude",
             "harness_session_id": "sess-new",
             "created_at": "2026-09-23T20:00:00Z"}
        ])
    }

    #[test]
    fn grant_rows() {
        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"}, "rows": [],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["vacate"], json!([]));
        assert!(out["refusal"].is_null());

        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": true, "proposed_name": "Taylor",
            "caller": {"kind": "agent", "name": "lead-a"},
            "rows": [row("lead-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "succeeded");
        assert_eq!(out["vacate"], json!(["lead-a"]));

        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": true, "proposed_name": "Taylor",
            "caller": {"kind": "human"},
            "rows": [row("lead-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "succeeded");
        assert_eq!(out["vacate"], json!(["lead-a"]));

        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"},
            "rows": [row("dead-lead", "fno", "exited")],
        }))
        .unwrap();
        assert_eq!(out["clear_terminal"], json!(["dead-lead"]));
        assert_eq!(out["outcome"], "granted");

        // The reversible word never hands a role away: an Orphaned row
        // with a live pid stays a holder, and the spawn declines naming it.
        let quiet = json!({
            "name": "quiet-lead", "role_scope": "fno", "status": "orphaned",
            "pid": std::process::id(),
            "created_at": "2026-10-01T00:00:00Z",
        });
        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"},
            "rows": [quiet],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["holders"], json!(["quiet-lead"]));
        assert!(out["refusal"].as_str().unwrap().contains("quiet-lead"));

        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"},
            "rows": [row("lead-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("lead-a"));
        assert!(refusal.contains("--hand-off"));

        // holders = ["lead-a", "lead-b"]; the caller matches one but not all,
        // so succession must fall through to the ordinary decline rather
        // than succeeding a partial match. The refusal names the holder and
        // the --succeed flag, the same fall-through arm a scope the caller
        // does not hold at all takes.
        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": true, "proposed_name": "Taylor",
            "caller": {"kind": "agent", "name": "lead-a"},
            "rows": [row("lead-a", "fno", "busy"), row("lead-b", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate"], json!([]));
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("lead-a"));
        assert!(refusal.contains("--hand-off"));
    }

    #[test]
    fn revive_rows() {
        let out = resolve(&json!({
            "kind": "role-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"}, "exclude_name": "successor",
            "rows": [row("successor", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["holders"], json!([]));
        assert_eq!(out["outcome"], "granted");

        assert!(resolve(&json!({
            "kind": "role-settle", "caller": {"kind": "human"}, "rows": [],
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
                "kind": "role-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("lead-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["lead-a"]));
        assert_eq!(out["vacate"], json!([]));
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("lead-a"));
        assert!(refusal.contains("epic-a,epic-b"));

        let rows = [row("lead-a", "epic-a", "busy")];
        for caller in [
            json!({"kind": "human"}),
            json!({"kind": "agent", "name": "other"}),
        ] {
            let out = settle(
                json!({
                    "kind": "role-settle", "scope": "epic-a,epic-b", "succession": false,
                    "caller": caller, "rows": rows,
                }),
                Ok(HashMap::new()),
            );
            assert_eq!(out["outcome"], "declined");
            assert_eq!(out["rivals"], json!(["lead-a"]));
        }

        let out = settle(
            json!({
                "kind": "role-settle", "scope": "epic-a", "succession": true, "proposed_name": "Taylor",
                "caller": {"kind": "human"},
                "rows": [row("lead-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate"], json!([]));
        assert!(out["refusal"]
            .as_str()
            .unwrap()
            .contains("--hand-off hands down only an identical team"));

        let out = settle(
            json!({
                "kind": "role-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "agent", "name": "lead-a"},
                "rows": [row("lead-a", "epic-a,epic-b", "busy")],
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
                "kind": "role-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("lead-p", "alpha,beta", "busy")],
            }),
            projects,
        );
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["rivals"], json!([]));

        let out = settle(
            json!({
                "kind": "role-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("lead-p", "alpha,beta", "busy")],
            }),
            Err("no work.workspaces in any candidate config.toml".to_string()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["lead-p"]));

        let projects = Ok(HashMap::from([
            ("alpha".to_string(), "alpha".to_string()),
            ("a".to_string(), "alpha".to_string()),
        ]));
        let out = settle(
            json!({
                "kind": "role-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("lead-a", "a", "busy")],
            }),
            projects,
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["lead-a"]));

        let out = settle(
            json!({
                "kind": "role-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("dead-lead", "epic-a,epic-b", "exited")],
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
        let mut holder = row("lead-a", "epic-a", "busy");
        holder["harness_session_id"] = json!("sess-a");
        let out = resolve(&json!({
            "kind": "role-settle", "scope": "epic-a", "successor": "Taylor", "succession": true, "proposed_name": "Taylor",
            "caller": caller, "rows": [holder],
        }))
        .unwrap();
        assert_eq!(out["holders"], json!(["lead-a"]));
        assert_eq!(
            out["holder_ids"],
            json!([{
                "name": "lead-a", "harness_session_id": "sess-a",
            }])
        );
        assert_eq!(out["caller"], json!({"kind": "human"}));

        // Succession re-homes the org. A live child whose CURRENT
        // owner (the provenance owner) names a vacated holder's session
        // follows the team; a child already owned by another session stays
        // put; a provenance-less child (birth edge only) is never selected,
        // and a terminal row never moves.
        let out = resolve(&json!({
            "kind": "role-settle", "scope": "epic-a", "successor": "Taylor",
            "successor_identity": {"harness": "codex", "session_id": "sess-successor", "cwd": "/w"},
            "plan": {
                "caller": {"kind": "human"},
                "holder_ids": [{"name": "lead-a", "harness_session_id": "sess-a"}],
                "outcome": "succeeded", "vacate": ["lead-a"],
            },
            "rows": [
                {"name": "lead-a", "role_scope": "epic-a", "status": "busy",
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
        assert_eq!(out["reown_rows"], json!([1]));
        assert_eq!(out["reown_owner"]["session_id"], "sess-successor");
        assert_eq!(out["reown_owner"]["kind"], "session");

        let out = resolve(&json!({
            "kind": "role-settle", "scope": "epic-a", "successor": "Taylor",
            "plan": {
                "caller": {"kind": "human"},
                "holder_ids": [{"name": "lead-a", "harness_session_id": "sess-a"}],
                "outcome": "succeeded", "vacate": ["lead-a"],
            },
            "rows": [{"name": "lead-a", "role_scope": "epic-a", "status": "busy",
                      "harness_session_id": "sess-b"}],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate_rows"], json!([]));

        let out = resolve(&json!({
            "kind": "role-settle", "scope": "epic-a", "successor": "Taylor",
            "plan": {
                "caller": {"kind": "human"},
                "holder_ids": [{"name": "lead-a", "harness_session_id": "sess-a"}],
                "outcome": "succeeded", "vacate": ["lead-a"],
            },
            "rows": [
                {"name": "other", "role_scope": null, "status": "busy"},
                {"name": "dead-lead", "role_scope": "epic-a", "status": "exited"},
            ],
        }))
        .unwrap();
        assert_eq!(out["clear_terminal_rows"], json!([1]));
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["vacate_rows"], json!([]));

        let out = resolve_with_projects(
            &json!({
                "kind": "role-settle", "scope": "epic-a", "successor": "Taylor",
                "plan": {
                    "caller": {"kind": "human"}, "holder_ids": [],
                    "outcome": "granted", "vacate": [],
                },
                "rows": [{"name": "lead-b", "role_scope": "epic-a,epic-b",
                          "status": "busy", "harness_session_id": "sess-b"}],
            }),
            &Ok(HashMap::new()),
        )
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate_rows"], json!([]));

        let rows = json!([{
            "name": "lead-a", "role_scope": "epic-a", "status": "busy",
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
                    "holder_ids": [{"name": "lead-a", "harness_session_id": "sess-a"}],
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
                    "holder_ids": [{"name": "lead-a", "harness_session_id": "sess-a"}],
                    "outcome": "succeeded", "vacate": [],
                }),
                "vacate",
            ),
        ] {
            let error = resolve(&json!({
                "kind": "role-settle", "scope": "epic-a", "successor": "Taylor", "rows": rows,
                "plan": plan,
            }))
            .unwrap_err();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn record_rows() {
        // The transaction receipts land under a pinned nested home for the
        // successor settle; the lock keeps the env mutation off parallel tests.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let bus_home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(bus_home.path().join("home")).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", bus_home.path().join("home"));
        // The fleet announce needs one live recipient in the home registry.
        std::fs::write(
            bus_home.path().join("home/registry.json"),
            serde_json::to_string(&json!({
                "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                "agents": [{
                    "name": "Avery", "status": "live", "cwd": "/repo", "log_path": "/repo/lead-successor.log",
                    "harness": "claude", "harness_session_id": "sess-new",
                    "created_at": "2026-10-04T00:00:00Z",
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let _reg = team_registry(tmp.path(), agents_with_succession_rows());
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "role-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "agent", "name": "Avery"},
                    "holder_ids": [{"name": "lead-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["lead-old"],
                },
                "rows": [{
                    "name": "lead-old", "role_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &team_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "declined");
        let store = std::fs::read_to_string(team_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        assert_eq!(doc["teams"]["x-aaaa"]["generation"], json!(1));
        assert_eq!(doc["teams"]["x-aaaa"]["holder_session"], json!("sess-old"));

        // A payload carrying the successor key pends the succession with the
        // predecessor identity from the plan's holder_ids.
        let answer = resolve_at(
            &json!({
                "kind": "role-settle", "scope": "x-aaaa", "successor": "Avery",
                "plan": {
                    "caller": {"kind": "agent", "name": "Avery"},
                    "holder_ids": [{"name": "lead-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["lead-old"],
                },
                "rows": [{
                    "name": "lead-old", "role_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &team_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "succeeded");
        let store = std::fs::read_to_string(team_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        let pending = &doc["teams"]["x-aaaa"]["pending_succession"];
        assert_eq!(pending["successor_name"], json!("Avery"));
        assert_eq!(pending["predecessor_name"], json!("lead-old"));
        assert_eq!(pending["predecessor_session"], json!("sess-old"));
        assert!(pending["ts"].is_string());
        // AC2-HP: exactly one announce row and one transfer receipt for the
        // succeeded settle. The store commit is the write boundary: query
        // the store; the raw journal bytes are only a fallback.
        let journal = crate::paths::AgentsHome::from_env().events_jsonl();
        let raw = match crate::event_store::query_events(&journal, &Default::default()) {
            Ok(rows) => rows
                .iter()
                .map(|r| r.line.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => std::fs::read_to_string(&journal).unwrap_or_default(),
        };
        assert_eq!(
            raw.matches("team_succession_transferred").count(),
            1,
            "one transfer receipt: {raw}"
        );
        assert_eq!(
            raw.matches("team_succession_announced").count(),
            1,
            "one announce receipt: {raw}"
        );
        let bus =
            std::fs::read_to_string(bus_home.path().join("bus/messages.jsonl")).unwrap_or_default();
        assert_eq!(
            bus.matches("succession: x-aaaa").count(),
            1,
            "one fleet announcement: {bus}"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        // Without the successor key (an old caller) today's shape holds: no
        // pending record is written.
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "role-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "agent", "name": "Avery"},
                    "holder_ids": [{"name": "lead-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["lead-old"],
                },
                "rows": [{
                    "name": "lead-old", "role_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &team_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "declined");
        let store = std::fs::read_to_string(team_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        assert!(doc["teams"]["x-aaaa"].get("pending_succession").is_none());

        let tmp = tempfile::TempDir::new().unwrap();
        let _reg = team_registry(tmp.path(), json!([{"name": "w", "status": "exited"}]));
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "role-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "human"},
                    "holder_ids": [],
                    "outcome": "granted", "vacate": [],
                },
                "rows": [],
            }),
            &team_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "granted");
        let store = std::fs::read_to_string(team_store(tmp.path())).unwrap();
        let doc: Value = serde_json::from_str(&store).unwrap();
        assert!(doc["teams"].get("x-aaaa").is_none(), "{store}");

        let tmp = tempfile::TempDir::new().unwrap();
        // A fresh grant carries the successor's own people-shaped row name: the
        // stale record from the dead predecessor team is forgotten, then the
        // new team takes the row's name bound to the successor session the payload
        // plumbs. No registry read anywhere: the carry keys on the payload's
        // successor_identity.
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "role-settle", "scope": "x-aaaa", "successor": "kestrel",
                "successor_identity": {"harness": "claude", "session_id": "sess-new",
                                   "cwd": "/repo"},
                "plan": {
                    "caller": {"kind": "human"},
                    "holder_ids": [],
                    "outcome": "granted", "vacate": [],
                },
                "rows": [],
            }),
            &team_store(tmp.path()),
        )
        .unwrap();
        assert_eq!(answer["outcome"], "granted");
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(team_store(tmp.path())).unwrap())
                .unwrap();
        let person = doc["teams"]["x-aaaa"]["name"].as_str().unwrap();
        assert!(person.chars().all(char::is_alphabetic));
        assert_eq!(person, "kestrel");
        assert_ne!(person, "barnaby");
        assert_eq!(doc["teams"]["x-aaaa"]["holder_session"], json!("sess-new"));

        let tmp = tempfile::TempDir::new().unwrap();
        // A file where the store's parent dir would be: the write fails.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let store = blocker.join("team_names.json");
        named_record_fixture(tmp.path());
        let answer = resolve_at(
            &json!({
                "kind": "role-settle", "scope": "x-aaaa",
                "plan": {
                    "caller": {"kind": "agent", "name": "Avery"},
                    "holder_ids": [{"name": "lead-old", "harness_session_id": "sess-old"}],
                    "outcome": "succeeded", "vacate": ["lead-old"],
                },
                "rows": [{
                    "name": "lead-old", "role_scope": "x-aaaa", "status": "busy",
                    "harness_session_id": "sess-old",
                }],
            }),
            &store,
        )
        .unwrap();
        assert_eq!(answer["outcome"], "declined");
    }
}
