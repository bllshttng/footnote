//! One module answers "who owns this" and "who is this session"
//! (docs/architecture/notice-routing.md).
//!
//! The ladder: a node's lead (rung 0, `territory::node_owners`), that
//! lead's higher-up (rung 1, the level-minus-one rule moved out of
//! `lead_wake`), the least-loaded live lead (rung 2, beat inside two
//! hours, fewest held nodes plus open notices, tie to the newest beat),
//! then the user - the caller files the question page. A rung whose
//! holder session equals the asker is skipped, so a worker never mails
//! itself. An unreadable registry is `Err` from [`world`], never an
//! empty team list.
//!
//! x-1d4c's help router calls the same `Ask`/`resolve` shape: whichever
//! node lands first owns this file.

use crate::state::RegistryEntry;
use crate::territory::Team;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// A live lead beat this recently to be eligible at the least-loaded rung.
pub(crate) const LEAST_LOADED_BEAT_WINDOW_SECS: i64 = 7_200;

/// The role a session reads as for notice audience.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Worker,
    Lead,
    User,
}

impl Role {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Role::Worker => "worker",
            Role::Lead => "lead",
            Role::User => "user",
        }
    }

    pub(crate) fn parse(word: &str) -> Role {
        match word.trim().to_lowercase().as_str() {
            "worker" => Role::Worker,
            "lead" => Role::Lead,
            _ => Role::User,
        }
    }
}

/// Where the ladder starts, and the rung a resolved owner sat on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rung {
    NodeLead,
    HigherUp,
    LeastLoaded,
    User,
}

/// The ask: which node, who is asking, where to start. `start` also names
/// the skip-self rung x-1d4c's question ladder enters at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ask<'a> {
    pub(crate) node: Option<&'a str>,
    pub(crate) from_session: Option<&'a str>,
    pub(crate) start: Rung,
}

/// Who answers, at which rung. `User` carries no scope: the caller files
/// the question page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Owner {
    pub(crate) rung: Rung,
    pub(crate) scope: Option<String>,
    pub(crate) holder: Option<String>,
    pub(crate) session: Option<String>,
}

/// The world the ladder resolves against, read once per pass: live teams,
/// graph entries, the project map, each lead's last beat, and each lead's
/// load (live held claims plus open notices assigned to the scope). Tests
/// build it directly; production reads it with [`world`].
pub(crate) struct World {
    pub(crate) teams: Vec<Team>,
    pub(crate) entries: Vec<Value>,
    pub(crate) projects: Result<HashMap<String, String>, String>,
    pub(crate) beats: BTreeMap<String, Option<i64>>,
    pub(crate) load: BTreeMap<String, u32>,
    pub(crate) now: i64,
}

/// Read the world from disk. A registry or graph read fault is `Err` -
/// the ladder never resolves out of a half-readable world.
pub(crate) fn world(home: &crate::paths::AgentsHome, config_cwd: &Path) -> Result<World, String> {
    let registry = home.registry_json();
    let teams =
        crate::territory::live_teams(&registry).map_err(|e| format!("owner_ladder: {}", e.0))?;
    let entries = crate::territory::graph_entries(config_cwd)
        .map_err(|e| format!("owner_ladder: {}", e.0))?;
    let projects = Ok(crate::territory::workspace_paths(config_cwd));
    let journals = crate::tick_ledger::journals(home);
    let now = now_epoch();
    let claimed = crate::territory::live_node_claims().unwrap_or_default();
    let mut beats: BTreeMap<String, Option<i64>> = BTreeMap::new();
    let mut load: BTreeMap<String, u32> = BTreeMap::new();
    for team in &teams {
        let ts = crate::lead_history::previous_beat(
            &journals,
            &team.scope,
            team.holder_session.as_deref(),
            false,
        )
        .ok()
        .flatten()
        .and_then(|row| row.get("ts").and_then(Value::as_str).and_then(parse_ts));
        beats.insert(team.scope.clone(), ts);
        let held = crate::territory::compile_territory(&team.scope, &entries, &projects)
            .map(|(_, ids)| ids)
            .unwrap_or_default();
        let claims = crate::territory::live_held_in(&held, &claimed) as u32;
        let notices = crate::notice_route::open_notice_count(&team.scope);
        load.insert(team.scope.clone(), claims + notices);
    }
    Ok(World {
        teams,
        entries,
        projects,
        beats,
        load,
        now,
    })
}

/// The ladder. Pure over the world; the authority-router decision write is
/// the caller's choice via [`record_authority_pick`].
pub(crate) fn resolve(ask: &Ask, w: &World) -> Owner {
    match ask.start {
        Rung::User => user(),
        Rung::LeastLoaded => least_loaded(ask, w),
        Rung::NodeLead => node_chain(ask, w),
        Rung::HigherUp => match ask.node.and_then(|n| node_lead(n, w)) {
            Some(lead) => match rung_up(&w.teams, lead)
                .into_iter()
                .find(|t| !is_self(t, ask))
            {
                Some(up) => owner_of(Rung::HigherUp, up),
                None => least_loaded(ask, w),
            },
            None => least_loaded(ask, w),
        },
    }
}

/// NodeLead with the skip-self climb: the node's lead, else its higher-up,
/// else the least-loaded live lead, else the user.
fn node_chain(ask: &Ask, w: &World) -> Owner {
    let lead = ask.node.and_then(|n| node_lead(n, w));
    if let Some(team) = lead {
        if !is_self(team, ask) {
            return owner_of(Rung::NodeLead, team);
        }
    }
    let up = lead.and_then(|t| rung_up(&w.teams, t).into_iter().find(|t| !is_self(t, ask)));
    match up {
        Some(up) => owner_of(Rung::HigherUp, up),
        None => least_loaded(ask, w),
    }
}

/// Rung 2: the live lead with a beat inside the window and the smallest
/// load (held claims plus open notices); a tie goes to the newest beat.
/// No eligible lead reads as the user rung (AC4).
fn least_loaded(ask: &Ask, w: &World) -> Owner {
    let mut best: Option<(&Team, i64, u32)> = None;
    for team in &w.teams {
        if is_self(team, ask) {
            continue;
        }
        let beat = match w.beats.get(&team.scope) {
            Some(Some(ts)) if beat_in_window(w.now, *ts) => *ts,
            _ => continue,
        };
        let load = w.load.get(&team.scope).copied().unwrap_or(0);
        let better = match best {
            None => true,
            Some((_, b_beat, b_load)) => load < b_load || (load == b_load && beat > b_beat),
        };
        if better {
            best = Some((team, beat, load));
        }
    }
    match best {
        Some((team, _, _)) => owner_of(Rung::LeastLoaded, team),
        None => user(),
    }
}

/// Within [`LEAST_LOADED_BEAT_WINDOW_SECS`], and never in the future.
fn beat_in_window(now: i64, beat: i64) -> bool {
    now >= beat && now - beat <= LEAST_LOADED_BEAT_WINDOW_SECS
}

/// The node's lead: the deepest live team whose compiled territory holds it.
fn node_lead<'a>(node: &str, w: &'a World) -> Option<&'a Team> {
    let (owners, _) = crate::territory::node_owners(&w.teams, &w.entries, &w.projects);
    let scope = owners.get(node)?;
    w.teams.iter().find(|t| t.scope == *scope)
}

/// The rung-up rule, moved out of `lead_wake::plan_wakes`: the live teams
/// exactly one level above the lead. Empty means the operator lane.
pub(crate) fn rung_up<'a>(teams: &'a [Team], lead: &Team) -> Vec<&'a Team> {
    let level = lead.level as i64;
    teams
        .iter()
        .filter(|t| t.level as i64 + 1 == level)
        .collect()
}

fn owner_of(rung: Rung, team: &Team) -> Owner {
    Owner {
        rung,
        scope: Some(team.scope.clone()),
        holder: Some(team.holder.clone()),
        session: team.holder_session.clone(),
    }
}

fn user() -> Owner {
    Owner {
        rung: Rung::User,
        scope: None,
        holder: None,
        session: None,
    }
}

fn is_self(team: &Team, ask: &Ask) -> bool {
    match (ask.from_session, team.holder_session.as_deref()) {
        (Some(from), Some(holder)) => !from.trim().is_empty() && from.trim() == holder.trim(),
        _ => false,
    }
}

/// The session's role, pure over the rows the caller read: Lead when a
/// live team's holder session matches, else Worker when the registry row
/// carries a node, else User (AC5).
pub(crate) fn role(session_id: &str, rows: &[RegistryEntry], teams: &[Team]) -> Role {
    if session_id.trim().is_empty() {
        return Role::User;
    }
    if teams
        .iter()
        .any(|t| t.holder_session.as_deref() == Some(session_id))
    {
        return Role::Lead;
    }
    if crate::lead_state::find_by_session(rows, session_id, None)
        .is_some_and(|r| r.node.as_deref().is_some_and(|n| !n.trim().is_empty()))
    {
        return Role::Worker;
    }
    Role::User
}

/// The role read hooks take (`--role` and `FNO_SESSION_ROLE`): rows and
/// teams read here, and any unreadable source reads as User - today's
/// behavior, never a refusal.
pub(crate) fn role_now(
    session_id: &str,
    home: &crate::paths::AgentsHome,
    _config_cwd: &Path,
) -> Role {
    if session_id.trim().is_empty() {
        return Role::User;
    }
    let registry = home.registry_json();
    let rows = match crate::state::load_registry(&registry) {
        Ok(r) => r.entries,
        Err(_) => return Role::User,
    };
    let teams = crate::territory::live_teams(&registry).unwrap_or_default();
    role(session_id, &rows, &teams)
}

/// The least-loaded pick is a ruling, so it records one `authority-router`
/// decision naming the scope and the load numbers (AC3). Best-effort: a
/// failed record costs only the audit trail, never the routing.
pub(crate) fn record_authority_pick(node: Option<&str>, owner: &Owner, load_line: &str) -> bool {
    if owner.rung != Rung::LeastLoaded {
        return false;
    }
    let Some(scope) = owner.scope.as_deref() else {
        return false;
    };
    let mut decision = format!("least-loaded lead {scope} takes the ask");
    if let Some(n) = node {
        decision.push_str(" for node ");
        decision.push_str(n);
    }
    let mut cmd = crate::loop_dispatch::fno_cmd("fno");
    cmd.args([
        "inbox",
        "decide",
        "authority-router",
        &decision,
        "--rationale",
        load_line,
    ]);
    let out = crate::bounded_cmd::output_with_timeout_result(cmd, 20);
    out.map(|o| o.status.success()).unwrap_or(false)
}

fn parse_ts(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.timestamp())
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail_hold::tests::registry_row;

    fn team(scope: &str, level: u8, holder: &str, sid: Option<&str>) -> Team {
        Team {
            scope: scope.to_string(),
            level,
            holder: holder.to_string(),
            holder_session: sid.map(str::to_string),
        }
    }

    fn entries_fixture() -> Vec<Value> {
        vec![
            serde_json::json!({"id": "n-1", "project": "proj-a"}),
            serde_json::json!({"id": "x-epic", "type": "epic"}),
            serde_json::json!({"id": "n-2", "parent": "x-epic"}),
        ]
    }

    fn world_now(
        teams: Vec<Team>,
        now: i64,
        beats_of: Vec<(&str, i64)>,
        loads: Vec<(&str, u32)>,
    ) -> World {
        let mut beats = BTreeMap::new();
        for t in &teams {
            beats.insert(t.scope.clone(), None);
        }
        for (scope, ts) in beats_of {
            beats.insert(scope.to_string(), Some(ts));
        }
        let mut load = BTreeMap::new();
        for (scope, n) in loads {
            load.insert(scope.to_string(), n);
        }
        let mut projects = HashMap::new();
        projects.insert("proj-a".to_string(), "proj-a".to_string());
        World {
            teams,
            entries: entries_fixture(),
            projects: Ok(projects),
            beats,
            load,
            now,
        }
    }

    /// AC1-AC5 in one pass over one fixture world: NodeLead hit, the
    /// skip-self climb to HigherUp, the least-loaded pick among two fresh
    /// leads, the user rung when every beat is stale, and the role read.
    /// The authority-record guard is asserted without its shellout.
    #[test]
    fn ac1_to_ac5_ladder_rungs_role_and_record_guard() {
        let now = 1_000_000_i64;
        let fresh = now - 60;
        let teams = vec![
            team("proj-top", 2, "lead-top", Some("sid-top")),
            team("x-epic", 1, "lead-mid", Some("sid-mid")),
            team("proj-a", 0, "lead-a", Some("sid-a")),
            team("proj-b", 0, "lead-b", Some("sid-b")),
        ];
        let loads = vec![("proj-top", 2), ("x-epic", 5), ("proj-a", 3), ("proj-b", 1)];
        let beats = vec![
            ("proj-top", fresh),
            ("x-epic", fresh),
            ("proj-a", fresh),
            ("proj-b", fresh),
        ];

        // AC1: the node's lead at NodeLead.
        let w = world_now(teams.clone(), now, beats.clone(), loads.clone());
        let o = resolve(
            &Ask {
                node: Some("n-2"),
                from_session: None,
                start: Rung::NodeLead,
            },
            &w,
        );
        assert_eq!(o.rung, Rung::NodeLead);
        assert_eq!(o.scope.as_deref(), Some("x-epic"));
        assert_eq!(o.holder.as_deref(), Some("lead-mid"));

        // AC2: the node lead's own session skips to HigherUp.
        let o = resolve(
            &Ask {
                node: Some("n-2"),
                from_session: Some("sid-mid"),
                start: Rung::NodeLead,
            },
            &w,
        );
        assert_eq!(o.rung, Rung::HigherUp);
        assert_eq!(o.scope.as_deref(), Some("proj-top"));
        assert_eq!(o.holder.as_deref(), Some("lead-top"));

        // AC3: no team holds the node; the load-1 lead wins at LeastLoaded.
        let o = resolve(
            &Ask {
                node: Some("n-9"),
                from_session: None,
                start: Rung::NodeLead,
            },
            &w,
        );
        assert_eq!(o.rung, Rung::LeastLoaded);
        assert_eq!(o.scope.as_deref(), Some("proj-b"));
        assert_eq!(o.holder.as_deref(), Some("lead-b"));

        // AC4: every beat stale past the window reads as the user rung.
        let stale = now - LEAST_LOADED_BEAT_WINDOW_SECS - 1;
        let w = world_now(
            teams,
            now,
            vec![
                ("proj-top", stale),
                ("x-epic", stale),
                ("proj-a", stale),
                ("proj-b", stale),
            ],
            loads,
        );
        let o = resolve(
            &Ask {
                node: Some("n-9"),
                from_session: None,
                start: Rung::NodeLead,
            },
            &w,
        );
        assert_eq!(o.rung, Rung::User);
        assert_eq!(o.scope, None);

        // The record guard: only a LeastLoaded pick records, and it does so
        // without a shellout when the guard already refuses.
        assert!(!record_authority_pick(Some("n-9"), &o, "loads"));
        let node_owner = Owner {
            rung: Rung::NodeLead,
            scope: Some("x-epic".to_string()),
            holder: Some("lead-mid".to_string()),
            session: None,
        };
        assert!(!record_authority_pick(Some("n-2"), &node_owner, "loads"));

        // AC5: the role read. Lead beats Worker beats User; a registry row
        // with a node is a Worker; an unknown or empty session is User.
        let rows: Vec<RegistryEntry> = vec![
            {
                let mut r = registry_row("worker-9", "sid-w");
                r["node"] = serde_json::json!("n-1");
                serde_json::from_value(r).unwrap()
            },
            {
                let r = registry_row("worker-8", "sid-n");
                serde_json::from_value(r).unwrap()
            },
        ];
        let live = vec![team("x-epic", 1, "lead-mid", Some("sid-mid"))];
        assert_eq!(role("sid-mid", &rows, &live), Role::Lead);
        assert_eq!(role("sid-w", &rows, &live), Role::Worker);
        assert_eq!(role("sid-n", &rows, &live), Role::User);
        assert_eq!(role("sid-x", &rows, &live), Role::User);
        assert_eq!(role("", &rows, &live), Role::User);
        assert_eq!(Role::parse("worker"), Role::Worker);
        assert_eq!(Role::parse("LEAD"), Role::Lead);
        assert_eq!(Role::parse("nonsense"), Role::User);
        assert_eq!(Role::Worker.word(), "worker");
        assert_eq!(Role::Lead.word(), "lead");
        assert_eq!(Role::User.word(), "user");
    }
}
