//! `lead-history`: the team-scope `lead_checkin` readback behind
//! `fno agents org history`.
//!
//! Python resolves the caller's team scope (harness identity and registry
//! rows are Python-owned), passes every journal `paths.event_journals`
//! returns, and relays here. The paths reduce to unique live journals (the
//! `.ephemeral` siblings never carry durable rows, and a `.1` generation is
//! the same journal its live path names); each live journal's `events.db`
//! store is synced FIRST (`event_store::sync`, which ingests the rotated
//! generation and then the live file), and the read is an indexed
//! `(scope, type, ts_ms)` select instead of a scan of every row ever
//! journaled. Selection is EXACT `data.scope` equality via the store's
//! `scope` column: rows are written through the team canonicalization, so
//! a second normalizer here could only disagree with it; a stored row whose
//! `data.scope` was not canonical carries `scope IS NULL` and reaches the
//! legacy classifier through the same query. Legacy rows (the refused
//! `role_scope`/`team`/`result` aliases, or a missing canonical key)
//! stay byte-preserved evidence: counted in `rejected`, listed in
//! `rejected_legacy` by store and `ts`, because a line number does not
//! survive rotation. The output is read-back, never a generated summary; a
//! zero-match answer still names every store and its counts, so an empty
//! history is a measurement, not an absence. A store that cannot be opened
//! is an error (rc 1), never an empty history.
//!
//! `lead-history --verdict` reads the same journals as a tenure verdict: the bounds
//! declared on the team manifest (iterations, respawns, compactions, the
//! recorded block cap) plus the inherited-scope delivery trend, judged as
//! ONE set. Each bound alone looked correctly configured while a lead sat
//! outside all of them; the verdict exists so something reads the set, and
//! so an absent bound never prints as a satisfied one. Its scan is still
//! the direct file walk the store syncs from; one indexed reader for both
//! is the follow-up port.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(crate) const LEAD_CHECKIN: &str = "lead_checkin";
pub(crate) const FORBIDDEN_ALIASES: [&str; 3] = ["role", "role_scope", "result"];

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// One stored row's verdict: matched, rejected (with optional legacy
/// evidence when one of its scope spellings names the requested team).
/// Shared by scope-stamped and NULL-scope result sets, so the canonical and
/// legacy tests apply identically to both. A `None` scope matches every
/// canonical row and attributes no legacy row.
fn classify(event: &Value, scope: Option<&str>) -> (bool, bool, Option<Value>) {
    let Some(data) = event.get("data").and_then(|d| d.as_object()) else {
        // A lead_checkin without an object payload is legacy evidence
        // too; it names no scope, so it counts but attributes nowhere.
        return (false, true, None);
    };
    let data = Value::Object(data.clone());
    let aliases: Vec<&str> = FORBIDDEN_ALIASES
        .iter()
        .filter(|k| data.get(**k).is_some())
        .copied()
        .collect();
    let row_scope = s_str(&data, "scope").unwrap_or("");
    let canonical = !row_scope.is_empty() && data.get("change").is_some() && aliases.is_empty();
    if canonical {
        return (scope.is_none_or(|wanted| row_scope == wanted), false, None);
    }
    let names_this_team = scope.is_some_and(|wanted| {
        row_scope == wanted || aliases.iter().any(|k| s_str(&data, k) == Some(wanted))
    });
    let legacy = names_this_team.then(|| {
        let missing: Vec<&str> = ["scope", "change"]
            .iter()
            .filter(|k| data.get(**k).is_none())
            .copied()
            .collect();
        json!({
            "forbidden": aliases,
            "missing": missing,
        })
    });
    (false, true, legacy)
}

/// The stored lead rows for one store. With a scope: exact-scope rows,
/// then NULL-scope rows (non-canonical scope spellings). With none: every
/// lead row whatever its scope spelling. Oldest first within each set.
fn lead_rows(store: &Connection, scope: Option<&str>) -> Result<Vec<String>, String> {
    let mut rows: Vec<String> = Vec::new();
    let read = |stmt: &mut rusqlite::Statement,
                args: &[&dyn rusqlite::ToSql],
                rows: &mut Vec<String>|
     -> Result<(), String> {
        let found = stmt
            .query_map(args, |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows.extend(found);
        Ok(())
    };
    let Some(scope) = scope else {
        let mut all = store
            .prepare("SELECT line FROM events WHERE type = ?1 ORDER BY ts_ms")
            .map_err(|e| e.to_string())?;
        read(&mut all, &[&LEAD_CHECKIN], &mut rows)?;
        return Ok(rows);
    };
    let mut scoped = store
        .prepare("SELECT line FROM events WHERE scope = ?1 AND type = ?2 ORDER BY ts_ms")
        .map_err(|e| e.to_string())?;
    read(&mut scoped, &[&scope, &LEAD_CHECKIN], &mut rows)?;
    let mut legacy = store
        .prepare("SELECT line FROM events WHERE scope IS NULL AND type = ?1 ORDER BY ts_ms")
        .map_err(|e| e.to_string())?;
    read(&mut legacy, &[&LEAD_CHECKIN], &mut rows)?;
    Ok(rows)
}

pub(crate) fn scan_scopes(events_paths: &[PathBuf], scope: Option<&str>) -> Result<Value, String> {
    // Generations and mirrors collapse here: one live journal, one store.
    let mut lives: Vec<PathBuf> = Vec::new();
    for path in events_paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(crate::events::EPHEMERAL_SUFFIX) {
            continue;
        }
        let live = crate::event_store::live_journal(path);
        if !lives.contains(&live) {
            lives.push(live);
        }
    }
    let mut payload = json!({
        "scope": scope,
        "journals": Vec::<Value>::new(),
        "scanned": 0,
        "matched": 0,
        "events": Vec::<Value>::new(),
        "rejected": 0,
        "rejected_legacy": Vec::<Value>::new(),
        "duplicates": 0,
        "ingested": 0,
    });
    let mut events: Vec<Value> = Vec::new();
    let mut rejected_legacy: Vec<Value> = Vec::new();
    let mut journals: Vec<Value> = Vec::new();
    // The loop runtime mirrors rows across journals, so one check-in can sit
    // in two files. The history lists a check-in once; the collapsed copies
    // are counted, not silently dropped.
    let mut seen: HashSet<String> = HashSet::new();
    let mut duplicates: u64 = 0;
    for live in &lives {
        let receipt = crate::event_store::import_all(live)?;
        let store = crate::event_store::open_read(&crate::event_store::store_path(live))?;
        let rows = lead_rows(&store, scope)?;
        let mut scanned = 0u64;
        let mut matched = 0u64;
        let mut rejected = 0u64;
        for line in rows {
            scanned += 1;
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let (hit, rejected_row, legacy) = classify(&event, scope);
            let ts_val = event.get("ts").cloned().unwrap_or(Value::Null);
            if hit {
                matched += 1;
                let key = serde_json::to_string(&event).unwrap_or_default();
                if seen.insert(key) {
                    events.push(event);
                } else {
                    duplicates += 1;
                }
            }
            if rejected_row {
                rejected += 1;
                if let Some(mut detail) = legacy {
                    detail["file"] = json!(receipt.store.display().to_string());
                    detail["ts"] = ts_val;
                    rejected_legacy.push(detail);
                }
            }
        }
        journals.push(json!({
            "path": live.display().to_string(),
            "store": receipt.store.display().to_string(),
            "ingested": receipt.ingested,
            "corrupt": receipt.corrupt,
            "scanned": scanned,
            "matched": matched,
            "rejected": rejected,
            "coverage": coverage_json(&crate::event_store::coverage(
                live,
                None,
                &[LEAD_CHECKIN.to_string()],
            )),
        }));
        payload["scanned"] = json!(payload["scanned"].as_u64().unwrap_or(0) + scanned);
        payload["rejected"] = json!(payload["rejected"].as_u64().unwrap_or(0) + rejected);
        payload["ingested"] = json!(payload["ingested"].as_u64().unwrap_or(0) + receipt.ingested);
    }
    // Across rotations one lead spans several files, so file order is no
    // longer display order; the envelope's own ts is.
    events.sort_by(|a, b| {
        s_str(b, "ts")
            .unwrap_or("")
            .cmp(s_str(a, "ts").unwrap_or(""))
    });
    payload["events"] = Value::Array(events);
    // The store returns the scoped set before the NULL-scope set; evidence
    // reads chronologically, so the legacy rows sort by their own ts.
    rejected_legacy.sort_by(|a, b| {
        s_str(a, "ts")
            .unwrap_or("")
            .cmp(s_str(b, "ts").unwrap_or(""))
    });
    payload["rejected_legacy"] = Value::Array(rejected_legacy);
    payload["journals"] = Value::Array(journals);
    payload["duplicates"] = json!(duplicates);
    payload["matched"] = json!(payload["events"].as_array().map(|a| a.len()).unwrap_or(0));
    payload["complete_since"] = complete_since_json(&payload["journals"]);
    Ok(payload)
}

/// The serialized store coverage receipt a history payload embeds.
fn coverage_json(cov: &crate::event_store::Coverage) -> Value {
    json!({
        "status": cov.status,
        "complete_since": cov.complete_since_ms.map(ms_to_rfc3339),
        "observed_first": cov.observed_first_ms.map(ms_to_rfc3339),
        "observed_last": cov.observed_last_ms.map(ms_to_rfc3339),
        "reason": cov.reason,
    })
}

fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| ms.to_string())
}

/// The latest proven start across a payload's journals; `null` when any
/// store is unknown or unreadable, so a partial read never claims a horizon.
fn complete_since_json(journals: &Value) -> Value {
    let receipts = journals.as_array().cloned().unwrap_or_default();
    let any_unproven = receipts.iter().any(|j| {
        j["coverage"]["status"]
            .as_str()
            .is_none_or(|s| s != "complete" && s != "partial")
    });
    if any_unproven {
        return Value::Null;
    }
    receipts
        .iter()
        .filter_map(|j| j["coverage"]["complete_since"].as_str())
        .max()
        .map(|s| json!(s))
        .unwrap_or(Value::Null)
}

pub(crate) fn scan(events_paths: &[PathBuf], scope: &str) -> Result<Value, String> {
    scan_scopes(events_paths, Some(scope))
}

/// The stop hook's half of the lead record: when this scope's newest
/// check-in is older than two check-in intervals, journal one row from what
/// the previous fire measured. It never decides anything.
pub(crate) fn hook_beat(
    events_path: &Path,
    cwd: &Path,
    scope: &str,
    session_id: &str,
    holder_session: Option<&str>,
    history: &crate::loop_lead::LeadFireHistory,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if scope.is_empty() {
        return false;
    }
    let payload = match crate::lead_history::previous_beat(
        &[events_path.to_path_buf()],
        scope,
        holder_session,
        false,
    ) {
        Ok(p) => p,
        // A blind due check must never write: a scan that cannot read the
        // journal is no evidence a beat was missed.
        Err(e) => {
            eprintln!("lead-checkin: WARNING: hook beat skipped: {e}");
            return false;
        }
    };
    let newest = payload
        .as_ref()
        .and_then(|r| r.get("ts"))
        .and_then(Value::as_str);
    let due = newest
        .and_then(|t| t.parse::<chrono::DateTime<chrono::Utc>>().ok())
        .map(|ts| {
            now - ts
                >= chrono::Duration::seconds(
                    2 * crate::lead_verdict_inputs::checkin_interval_secs(cwd),
                )
        })
        .unwrap_or(true);
    if !due {
        return false;
    }
    // ponytail: two stops inside one second can both see the beat due and
    // write two rows; a cross-process lock costs more than a doubled row.
    let since = newest.unwrap_or("on record");
    let undelivered = history
        .last_undelivered
        .map(|u| u.to_string())
        .unwrap_or_else(|| "unread".into());
    let data = json!({
        "scope": scope,
        "session_id": session_id,
        "holder_session": holder_session,
        "fires": history.total,
        "dry": history.dry,
        "last_actionable": history.last_ids.len(),
        "last_undelivered": history.last_undelivered,
        "change": format!(
            "missed beat: no check-in since {since}; last fire actionable {}, undelivered {undelivered}, dry {} of {} fires",
            history.last_ids.len(),
            history.dry,
            history.total
        ),
    });
    crate::lead_checkin::emit_row(events_path, "hook", data.as_object().unwrap())
}

/// The previous beat for this holder. With a session, the walk takes
/// `scan_scopes(.., None)` newest-first across every scope and returns the
/// first row whose `data.holder_session` names it (`source` restricted to
/// `loop` when `loop_only`); without a session, or when none match, it
/// returns today's result: the first such row of `scan(.., scope)`. Only
/// the verb's own rows carry NUMERIC_DIFF_KEYS, so the diff baseline is
/// the newest `loop` row; a hook row or a hand row must never baseline
/// the diff.
pub(crate) fn previous_beat(
    events_paths: &[PathBuf],
    scope: &str,
    holder_session: Option<&str>,
    loop_only: bool,
) -> Result<Option<Value>, String> {
    if let Some(session) = holder_session.filter(|s| !s.trim().is_empty()) {
        let payload = scan_scopes(events_paths, None)?;
        let found = payload["events"].as_array().and_then(|events| {
            events.iter().find(|r| {
                r.get("data")
                    .and_then(|d| d.get("holder_session"))
                    .and_then(Value::as_str)
                    == Some(session)
                    && (!loop_only || s_str(r, "source") == Some("loop"))
            })
        });
        // No row of this holder yet (or pre-holder rows only): the scope's
        // own newest row is the baseline, exactly the pre-holder read.
        if let Some(row) = found {
            return Ok(Some(row.clone()));
        }
    }
    let payload = scan(events_paths, scope)?;
    Ok(payload["events"]
        .as_array()
        .and_then(|e| {
            e.iter()
                .find(|r| !loop_only || s_str(r, "source") == Some("loop"))
        })
        .cloned())
}

fn render(payload: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    for event in payload["events"].as_array().unwrap() {
        let data = event.get("data").cloned().unwrap_or_else(|| json!({}));
        lines.push(format!(
            "{}  {}",
            s_str(event, "ts").unwrap_or(""),
            s_str(&data, "scope").unwrap_or("")
        ));
        lines.push(format!(
            "  change: {}",
            s_str(&data, "change").unwrap_or("")
        ));
        if let Some(rest) = data.as_object() {
            let rest: serde_json::Map<String, Value> = rest
                .iter()
                .filter(|(k, _)| k.as_str() != "scope" && k.as_str() != "change")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !rest.is_empty() {
                lines.push(format!(
                    "  evidence: {}",
                    serde_json::to_string(&rest).unwrap_or_default()
                ));
            }
        }
    }
    lines.push(format!(
        "history: {} canonical check-in(s) for {}, read {} lead row(s) from {} store(s), {} row(s) ingested, {} legacy-invalid lead row(s)",
        payload["matched"], payload["scope"], payload["scanned"],
        payload["journals"].as_array().map(|a| a.len()).unwrap_or(0),
        payload["ingested"],
        payload["rejected"]
    ));
    if let Some(since) = payload["complete_since"].as_str() {
        lines.push(format!("complete since {since}; earlier unknown"));
    } else {
        lines.push(
            "coverage unknown: a store predates the coverage epoch or cannot be read".to_string(),
        );
    }
    if payload["duplicates"].as_u64().unwrap_or(0) > 0 {
        lines.push(format!(
            "  ({} duplicate mirror row(s) collapsed)",
            payload["duplicates"]
        ));
    }
    for journal in payload["journals"].as_array().unwrap() {
        lines.push(format!(
            "  {}: store {}, {} row(s) ingested, {} lead row(s) read, {} matched",
            journal["path"],
            journal["store"],
            journal["ingested"],
            journal["scanned"],
            journal["matched"]
        ));
    }
    for entry in payload["rejected_legacy"].as_array().unwrap() {
        lines.push(format!(
            "  rejected legacy row in {} at {}: forbidden={} missing={}",
            entry["file"],
            entry["ts"].as_str().unwrap_or(""),
            serde_json::to_string(&entry["forbidden"]).unwrap_or_default(),
            serde_json::to_string(&entry["missing"]).unwrap_or_default(),
        ));
    }
    lines.join("\n")
}

/// `lead-history [--scope SCOPE] --events-path PATH [--events-path PATH ...] [--json|-J]`
///
/// With no `--scope`, the caller's team scope is resolved natively from the
/// registry (the retired Python `resolve_scope`).
///
/// rc 0 read (any match count), 1 a store that cannot be opened or synced
/// (the message names the store path), 2 usage failure.
pub fn run_lead_history(args: &[String]) -> i32 {
    let mut scope = String::new();
    let mut events_paths: Vec<PathBuf> = Vec::new();
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" if i + 1 < args.len() => {
                scope = args[i + 1].clone();
                i += 2;
            }
            "--events-path" if i + 1 < args.len() => {
                events_paths.push(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--json" | "-J" => {
                as_json = true;
                i += 1;
            }
            other => {
                eprintln!("fno-agents lead-history: unknown flag {other}");
                eprintln!(
                    "fno-agents lead-history: [--scope SCOPE] --events-path PATH \
                     [--events-path PATH ...] [--json|-J]"
                );
                return 2;
            }
        }
    }
    if scope.is_empty() {
        let registry_path = crate::paths::AgentsHome::from_env().registry_json();
        match crate::lead_verdict_inputs::resolve_scope(None, &registry_path) {
            Ok(resolved) => scope = resolved,
            Err(msg) => {
                eprintln!("lead: {msg}");
                return 2;
            }
        }
    }
    if events_paths.is_empty() {
        eprintln!("fno-agents lead-history: --events-path is required");
        return 2;
    }
    match scan(&events_paths, &scope) {
        Ok(payload) => {
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                println!("{}", render(&payload));
            }
            0
        }
        Err(msg) => {
            eprintln!("fno-agents lead-history: {msg}");
            1
        }
    }
}

// ---- the --verdict mode: the tenure bounds read as one set ----

const LEAD_LOOP_CHECK: &str = "lead_loop_check";
const TERMINATION: &str = "termination";
const CONTEXT_SNAPSHOT: &str = "context_snapshot";
const LOOP_CHECK_CONFIG: &str = "loop_check_config";
const LEAD_CONTEXT_NUDGE: &str = "lead_context_nudge";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BoundState {
    Exceeded,
    Within,
    /// The count is a lower bound: the team predates the store's coverage
    /// epoch, so an under-ceiling value is unproven, never a quiet within.
    Unmeasured,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    Converging,
    Stalled,
    Degraded,
    Unknown,
}

#[derive(Debug, PartialEq)]
pub(crate) struct BlockCapReading {
    value: u64,
    source: String,
}

/// Everything the verdict reads, gathered. The manifest numbers ride here
/// too so `verdict` stays pure: tests need no journal and no manifest file.
#[derive(Debug, Default)]
pub(crate) struct VerdictReadings {
    pub fires: u64,
    /// The last `lead_loop_check` row's `actionable`, newest by ts. `None`
    /// when the lead never fired or the last row named no board at all -
    /// a blind board must not read as a quiet one.
    pub last_actionable: Option<i64>,
    pub max_iterations: u64,
    pub respawn_count: u64,
    pub respawn_ceiling: u64,
    pub compactions: u64,
    /// `None` when no `--compaction-ceiling` was passed: an unset ceiling is
    /// an absence, and the verdict names it absent rather than satisfied.
    pub compaction_ceiling: Option<u64>,
    pub block_cap: Option<BlockCapReading>,
    /// Compaction count is only measurable against the manifest's harness
    /// session; without one the bound is unmeasurable, not zero.
    pub compactions_measurable: bool,
    pub checkins: u64,
    pub checkins_expected: bool,
    pub checkins_stale: bool,
    pub last_checkin_epoch: Option<i64>,
    /// `lead_context_nudge` rows for this scope. Context pressure is a
    /// reading the payload carries; the verdict itself keys on none of it.
    pub nudges: u64,
    /// Termination rows for this fno_id with `driver: lead`, counted by reason.
    pub terminations: Vec<(String, u64)>,
    pub inherited_undelivered: u64,
    pub inherited_closed_in_window: u64,
    /// True when the team began before the stores' coverage epoch: every
    /// count read is a lower bound, and an under-ceiling bound reads
    /// unmeasured instead of within.
    pub lifetime_partial: bool,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct BoundRow {
    name: &'static str,
    /// `None` when the bound is absent or its value unmeasurable.
    value: Option<u64>,
    ceiling: Option<u64>,
    state: BoundState,
}

fn bound_row(name: &'static str, value: Option<u64>, ceiling: Option<u64>) -> BoundRow {
    match (value, ceiling) {
        (Some(v), Some(c)) if c > 0 => BoundRow {
            name,
            value: Some(v),
            ceiling: Some(c),
            state: if v > c {
                BoundState::Exceeded
            } else {
                BoundState::Within
            },
        },
        _ => BoundRow {
            name,
            value,
            ceiling,
            state: BoundState::Absent,
        },
    }
}

/// True when an event timestamp falls at or after the team's start. Both
/// spellings of UTC (Z and +00:00) must compare equal, so the Z form is
/// normalized before the string compare; the fractional part is dropped so a
/// manifest stamp without one and an event stamp with one order as the same
/// second instead of the fraction sorting before its own second. An
/// unreadable team start reads as in-tenure, the conservative direction for
/// an alarm.
fn in_tenure(ts: &str, team_start: &str) -> bool {
    if team_start.is_empty() {
        return true;
    }
    let norm = |s: &str| {
        s.strip_suffix('Z')
            .unwrap_or(s)
            .split('.')
            .next()
            .unwrap_or(s)
            .to_string()
    };
    norm(ts) >= norm(team_start)
}

/// The one decision, pure so tests need no journal.
///
/// Degraded: any declared bound exceeded. Unknown: the team is old enough to
/// owe a recent check-in but none is recent and readable. Stalled: nothing
/// degraded, the
/// last fire read a quiet board, and the scope the lead INHERITED shows no
/// closure in the window. Filed nodes are deliberately excluded from the
/// stalled test: a lead that files real work into its own scope raises the
/// raw undelivered count by working well, and filing must never read as
/// divergence. Converging: everything else.
pub(crate) fn verdict(r: &VerdictReadings) -> (Verdict, Vec<BoundRow>) {
    // The iteration bound reads the stopping semantics, not a raw count:
    // bound_breached terminates on `total + 1 >= max_iterations` BEFORE the
    // breaching fire is appended, so a spent ceiling can sit at ceiling - 1
    // recorded fires and must still read exceeded.
    let iterations_exceeded = r.max_iterations > 0 && r.fires + 1 >= r.max_iterations;
    let bounds = vec![
        BoundRow {
            name: "iterations",
            value: Some(r.fires),
            ceiling: (r.max_iterations > 0).then_some(r.max_iterations),
            state: match (r.max_iterations > 0, iterations_exceeded) {
                (true, true) => BoundState::Exceeded,
                (true, false) => BoundState::Within,
                (false, _) => BoundState::Absent,
            },
        },
        bound_row(
            "respawns",
            Some(r.respawn_count),
            (r.respawn_ceiling > 0).then_some(r.respawn_ceiling),
        ),
        bound_row(
            "compactions",
            r.compactions_measurable.then_some(r.compactions),
            r.compaction_ceiling.filter(|c| *c > 0),
        ),
        // The block cap is a recording, not a ceiling: the bound is whether
        // the cap is KNOWN. Absent with no loop_check_config row, within
        // with the recorded value.
        BoundRow {
            name: "block_cap",
            value: r.block_cap.as_ref().map(|b| b.value),
            ceiling: None,
            state: if r.block_cap.is_some() {
                BoundState::Within
            } else {
                BoundState::Absent
            },
        },
    ];
    // Under partial lifetime the counts are lower bounds: a bound under its
    // ceiling is unproven, never quiet. Exceeded stays proven (a lower bound
    // over a ceiling is proof enough); the block cap is a recording, not a
    // count over history, so coverage does not touch it.
    let bounds = if r.lifetime_partial {
        bounds
            .into_iter()
            .map(|mut b| {
                if b.name != "block_cap" && b.state == BoundState::Within {
                    b.state = BoundState::Unmeasured;
                }
                b
            })
            .collect()
    } else {
        bounds
    };
    let degraded = bounds.iter().any(|b| b.state == BoundState::Exceeded);
    let v = if degraded {
        Verdict::Degraded
    } else if r.checkins_stale {
        Verdict::Unknown
    } else if r.last_actionable == Some(0)
        && r.inherited_undelivered > 0
        && r.inherited_closed_in_window == 0
    {
        Verdict::Stalled
    } else {
        Verdict::Converging
    };
    (v, bounds)
}

/// The six verdict row kinds `scan_readings` keeps, one vocabulary for the
/// store query.
const READING_TYPES: &[&str] = &[
    LEAD_LOOP_CHECK,
    TERMINATION,
    LEAD_CHECKIN,
    CONTEXT_SNAPSHOT,
    LOOP_CHECK_CONFIG,
    LEAD_CONTEXT_NUDGE,
];

/// One journal walk over the verdict row kinds, with the same mirror
/// dedupe `scan` applies. `fno_id` keys the loop rows, `harness_session_id`
/// the compaction snapshots, `scope` the context nudges. `team_start`
/// bounds the compaction count to THIS lead: a harness session that
/// compacted before it was teamed must not hand the new team a spent
/// bound.
fn scan_readings(
    events_paths: &[PathBuf],
    fno_id: &str,
    harness_session_id: &str,
    scope: &str,
    team_start: &str,
) -> Result<(VerdictReadings, u64, u64, u64, Vec<(String, u64, u64)>), String> {
    let mut r = VerdictReadings::default();
    let mut scanned: u64 = 0;
    let mut duplicates: u64 = 0;
    // Malformed rows skipped, not fatal: one torn write must not kill the
    // verdict read. Counted per journal, warned once per journal, reported
    // in the payload.
    let mut skipped: u64 = 0;
    let mut journals: Vec<(String, u64, u64)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // (ts, actionable) pairs for the newest-board read; ts sorted at the end.
    // Every fire for this team is recorded, even one that names no board,
    // so the LAST row answers and an older quiet row never speaks for it.
    let mut fires_ts: Vec<(String, Option<i64>)> = Vec::new();
    let mut terminations: Vec<String> = Vec::new();
    for path in events_paths {
        let content = crate::event_store::journal_text_checked(
            path,
            &crate::event_store::EventQuery::of_types(READING_TYPES),
        )
        .map_err(|_| format!("{}: unreadable journal", path.display()))?;
        let mut file_scanned: u64 = 0;
        let mut file_skipped: u64 = 0;
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            // A torn write can leave a malformed line in a healthy journal;
            // skipping one must not kill the whole verdict read.
            let event: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    file_skipped += 1;
                    continue;
                }
            };
            if !event.is_object() {
                file_skipped += 1;
                continue;
            }
            file_scanned += 1;
            let kind = crate::event_store::event_type_alias(s_str(&event, "type").unwrap_or(""));
            if !matches!(
                kind,
                LEAD_LOOP_CHECK
                    | TERMINATION
                    | LEAD_CHECKIN
                    | CONTEXT_SNAPSHOT
                    | LOOP_CHECK_CONFIG
                    | LEAD_CONTEXT_NUDGE
            ) {
                continue;
            }
            let key = serde_json::to_string(&event).unwrap_or_default();
            if !seen.insert(key) {
                duplicates += 1;
                continue;
            }
            let data = event.get("data").cloned().unwrap_or_else(|| json!({}));
            let row_session = s_str(&data, "session_id").unwrap_or("");
            match kind {
                LEAD_LOOP_CHECK if row_session == fno_id => {
                    r.fires += 1;
                    if let Some(ts) = s_str(&event, "ts") {
                        fires_ts.push((
                            ts.to_string(),
                            data.get("actionable").and_then(|x| x.as_i64()),
                        ));
                    }
                }
                TERMINATION if row_session == fno_id && s_str(&data, "driver") == Some("lead") => {
                    terminations.push(s_str(&data, "reason").unwrap_or("unknown").to_string());
                }
                LEAD_CHECKIN => {
                    let (canonical, _, _) = classify(&event, Some(scope));
                    if canonical
                        && s_str(&event, "source") == Some("loop")
                        && in_tenure(s_str(&event, "ts").unwrap_or(""), team_start)
                    {
                        r.checkins += 1;
                        if let Some(epoch) = s_str(&event, "ts")
                            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                            .map(|ts| ts.timestamp())
                        {
                            r.last_checkin_epoch =
                                Some(r.last_checkin_epoch.map_or(epoch, |last| last.max(epoch)));
                        }
                    }
                }
                CONTEXT_SNAPSHOT
                    if !harness_session_id.is_empty()
                        && row_session == harness_session_id
                        && s_str(&data, "entry_state") == Some("post_compact")
                        && in_tenure(s_str(&event, "ts").unwrap_or(""), team_start) =>
                {
                    r.compactions += 1;
                }
                LOOP_CHECK_CONFIG
                    if row_session == fno_id
                        || (!harness_session_id.is_empty()
                            && row_session == harness_session_id) =>
                {
                    if let Some(v) = data.get("block_cap").and_then(|x| x.as_u64()) {
                        r.block_cap = Some(BlockCapReading {
                            value: v,
                            source: s_str(&data, "block_cap_source").unwrap_or("").to_string(),
                        });
                    }
                }
                LEAD_CONTEXT_NUDGE if s_str(&data, "role_scope") == Some(scope) => {
                    r.nudges += 1;
                }
                _ => {}
            }
        }
        if file_skipped > 0 {
            eprintln!(
                "fno-agents lead-history --verdict: warning: {}: skipped {file_skipped} malformed JSON line(s)",
                path.display()
            );
        }
        journals.push((path.display().to_string(), file_scanned, file_skipped));
        scanned += file_scanned;
        skipped += file_skipped;
    }
    fires_ts.sort_by(|a, b| a.0.cmp(&b.0));
    r.last_actionable = fires_ts.last().and_then(|(_, a)| *a);
    let mut by_reason: Vec<(String, u64)> = Vec::new();
    for reason in &terminations {
        if let Some(row) = by_reason.iter_mut().find(|(k, _)| k == reason) {
            row.1 += 1;
        } else {
            by_reason.push((reason.clone(), 1));
        }
    }
    by_reason.sort();
    r.terminations = by_reason;
    Ok((r, scanned, duplicates, skipped, journals))
}

/// `lead-history --verdict [--scope SCOPE] [--manifest PATH] --cwd DIR
/// --events-path PATH [--events-path ...] [--json]`
///
/// rc 0 verdict read (any verdict), 1 refused or unreadable input (the
/// message names the failed reading), 2 usage failure. The read assembles
/// its own inputs: team, manifest, config, graph scope, window,
/// and delivery split come from `lead_verdict_inputs`; accepting
/// precomputed facts on the argv would keep the split owner this port
/// removes. `--verdict` selects this mode of the lead-history action (law
/// d-fe66560a: an argument of an existing action, never a new action).
pub fn run_lead_verdict(args: &[String]) -> i32 {
    let mut scope: Option<String> = None;
    let mut manifest_path: Option<PathBuf> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut events_paths: Vec<PathBuf> = Vec::new();
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--verdict" => {
                i += 1;
            }
            "--scope" if i + 1 < args.len() => {
                scope = Some(args[i + 1].clone());
                i += 2;
            }
            "--manifest" if i + 1 < args.len() => {
                manifest_path = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--events-path" if i + 1 < args.len() => {
                events_paths.push(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--cwd" if i + 1 < args.len() => {
                cwd = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--json" | "-J" => {
                as_json = true;
                i += 1;
            }
            other => {
                eprintln!("fno-agents lead-history --verdict: unknown flag {other}");
                eprintln!(
                    "fno-agents lead-history --verdict: [--scope SCOPE] [--manifest PATH] \
                     --cwd DIR --events-path PATH [--events-path ...] [--json]"
                );
                return 2;
            }
        }
    }
    let Some(cwd) = cwd else {
        eprintln!("fno-agents lead-history --verdict: --cwd and --events-path are required");
        return 2;
    };
    if events_paths.is_empty() {
        eprintln!("fno-agents lead-history --verdict: --cwd and --events-path are required");
        return 2;
    }
    let registry = crate::paths::AgentsHome::from_env().registry_json();
    let inputs = match crate::lead_verdict_inputs::resolve_verdict_inputs(
        &cwd,
        scope.as_deref(),
        manifest_path.as_deref(),
        &registry,
        || chrono::Utc::now(),
    ) {
        Ok(inputs) => inputs,
        Err(msg) => {
            eprintln!("fno-agents lead-history --verdict: {msg}");
            return 1;
        }
    };
    let manifest_path = inputs.manifest_path.clone();
    let harness = inputs.harness.clone();
    let now = inputs.now;
    let checkin_interval_secs = inputs.checkin_interval_secs;
    let team_age_secs = inputs.team_age_secs;
    let manifest = inputs.manifest;
    // The lead view: a re-scope carries the lead start and the declared
    // term through the clock on the team name record. The payload's
    // `manifest` block below keeps the raw arm.
    let lead = crate::team_names::lead_view(&manifest);
    let harness_session_id = manifest.harness_session_id.clone().unwrap_or_default();
    let team_lineage = TeamLineage {
        inherited: inputs.team_inherited,
        from_session: inputs.team_from_session.clone(),
    };
    let team_start = lead.created_at.clone().unwrap_or_default();
    let team_start_ms: Option<i64> = chrono::DateTime::parse_from_rfc3339(&team_start)
        .ok()
        .map(|dt| dt.timestamp_millis());
    let (mut readings, scanned, duplicates, skipped, journals) = match scan_readings(
        &events_paths,
        &manifest.fno_id,
        &harness_session_id,
        &inputs.scope,
        &team_start,
    ) {
        Ok(x) => x,
        Err(msg) => {
            eprintln!("fno-agents lead-history --verdict: {msg}");
            return 1;
        }
    };
    // Coverage across the verdict's stores: the row kinds the verdict
    // counts. A store with no epoch (or an unreadable one) leaves the
    // horizon unproven, which reads as partial lifetime - the conservative
    // direction for an alarm.
    let cov_types: Vec<String> = READING_TYPES.iter().map(|s| s.to_string()).collect();
    let mut complete_since_ms: Option<i64> = None;
    let mut cov_status = "complete";
    for path in &events_paths {
        let cov = crate::event_store::coverage(path, None, &cov_types);
        cov_status = cov.status;
        if cov.status != "complete" && cov.status != "partial" {
            complete_since_ms = None;
            break;
        }
        complete_since_ms = Some(match complete_since_ms {
            Some(cur) => cur.max(cov.complete_since_ms.unwrap_or(0)),
            None => cov.complete_since_ms.unwrap_or(0),
        });
    }
    let lifetime_partial = match (complete_since_ms, team_start_ms) {
        (Some(proven), Some(started)) => started < proven,
        _ => true,
    };
    readings.lifetime_partial = lifetime_partial;
    readings.max_iterations = manifest.max_iterations;
    readings.respawn_count = manifest.respawn_count;
    readings.respawn_ceiling = manifest.respawn_ceiling;
    readings.compaction_ceiling = Some(inputs.compaction_ceiling);
    readings.checkins_expected =
        matches!(manifest.shape.as_str(), "org" | "team") && team_age_secs > checkin_interval_secs;
    readings.checkins_stale = checkins_stale(
        &readings,
        now.timestamp(),
        checkin_interval_secs,
        team_age_secs,
    );
    let (compactions, compactions_source, compactions_error) =
        compaction_reading(&lead, &harness, readings.compactions);
    readings.compactions = compactions;
    let term_transcript = if harness == "claude"
        && !harness_session_id.is_empty()
        && lead
            .term
            .as_deref()
            .is_some_and(|s| s.trim().starts_with("compactions:"))
    {
        crate::claude_drive::find_transcript_in(
            &crate::claude_drive::claude_projects_dir(),
            &harness_session_id,
        )
    } else {
        None
    };
    let term_reading = crate::lead_term::reading(&lead, now, term_transcript.as_deref());
    let hygiene_transcript = hygiene_transcript_for_holder(&harness, &harness_session_id);
    let hygiene = hygiene_reading(
        &harness,
        &harness_session_id,
        &manifest.shape,
        hygiene_transcript.as_deref(),
        &team_lineage,
    );
    readings.compactions_measurable = !harness_session_id.is_empty();
    readings.inherited_undelivered = inputs.inherited_undelivered;
    readings.inherited_closed_in_window = inputs.inherited_closed_in_window;
    let (v, bounds) = verdict(&readings);
    let summary = bound_summary(v, &bounds);
    if let Some(note) = &inputs.harness_disagreement {
        eprintln!("lead-history --verdict: harness disagreement: {note}");
    }
    let payload = json!({
        "scope": inputs.scope,
        "verdict": v,
        "summary": summary,
        "bounds": bounds,
        "lifetime": if lifetime_partial { "partial" } else { "measured" },
        "harness": inputs.harness,
        "harness_source": inputs.harness_source,
        "harness_disagreement": inputs.harness_disagreement,
        "coverage": json!({
            "status": cov_status,
            "complete_since": complete_since_ms.map(ms_to_rfc3339),
        }),
        "inherited_undelivered": inputs.inherited_undelivered,
        "filed_undelivered": inputs.filed_undelivered,
        "inherited_closed_in_window": inputs.inherited_closed_in_window,
        "generation_start": inputs.generation_start,
        "generation_start_source": inputs.generation_start_source,
        "hygiene": hygiene,
        "window": inputs.window,
        "fires": readings.fires,
        "last_actionable": readings.last_actionable,
        "compactions": readings.compactions,
        "compactions_source": compactions_source,
        "compactions_error": compactions_error,
        "checkins": readings.checkins,
        "checkins_expected": readings.checkins_expected,
        "checkins_stale": readings.checkins_stale,
        "nudges": readings.nudges,
        "terminations": readings.terminations.iter().map(|(reason, n)| json!({
            "reason": reason,
            "count": n,
        })).collect::<Vec<_>>(),
        "block_cap": readings.block_cap.as_ref().map(|b| json!({
            "value": b.value,
            "source": b.source,
        })),
        "lead_started_at": lead.created_at,
        "term": term_reading_payload(&term_reading),
        "manifest": {
            "path": manifest_path.display().to_string(),
            "fno_id": manifest.fno_id,
            "created_at": manifest.created_at,
            "max_iterations": manifest.max_iterations,
            "respawn_count": manifest.respawn_count,
            "respawn_ceiling": manifest.respawn_ceiling,
        },
        "scanned": scanned,
        "duplicates": duplicates,
        "skipped": skipped,
        "journals": journals.iter().map(|(p, n, s)| json!({
            "path": p,
            "scanned": n,
            "skipped": s,
        })).collect::<Vec<_>>(),
    });
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!("{}", render_verdict(&payload));
    }
    0
}

/// The one-line verdict the escalation question embeds: the verdict word,
/// then the exceeded bounds with their numbers, then the absent names. The
/// first word is always the verdict name.
pub(crate) fn bound_summary(v: Verdict, bounds: &[BoundRow]) -> String {
    let name = format!("{v:?}").to_lowercase();
    let exceeded: Vec<String> = bounds
        .iter()
        .filter(|b| b.state == BoundState::Exceeded)
        .map(|b| match (b.value, b.ceiling) {
            (Some(v), Some(c)) => format!("{} {} of {}", b.name, v, c),
            (Some(v), None) => format!("{} {}", b.name, v),
            _ => format!("{} ?", b.name),
        })
        .collect();
    let absent: Vec<&str> = bounds
        .iter()
        .filter(|b| b.state == BoundState::Absent)
        .map(|b| b.name)
        .collect();
    let unmeasured: Vec<&str> = bounds
        .iter()
        .filter(|b| b.state == BoundState::Unmeasured)
        .map(|b| b.name)
        .collect();
    let mut parts: Vec<String> = Vec::new();
    if !exceeded.is_empty() {
        parts.push(format!("exceeded: {}", exceeded.join(", ")));
    }
    if !unmeasured.is_empty() {
        parts.push(format!("unmeasured: {}", unmeasured.join(", ")));
    }
    if !absent.is_empty() {
        parts.push(format!("absent: {}", absent.join(", ")));
    }
    if parts.is_empty() {
        name
    } else {
        format!("{} ({})", name, parts.join("; "))
    }
}

#[derive(Debug, Clone, Default)]
struct TeamLineage {
    inherited: Option<bool>,
    from_session: Option<String>,
}

pub(crate) fn hygiene_transcript_for_holder(harness: &str, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    match harness {
        "claude" => crate::claude_drive::find_transcript_in(
            &crate::claude_drive::claude_projects_dir(),
            session_id,
        ),
        "codex" => {
            let sessions = crate::codex_store::codex_home()?.join("sessions");
            crate::daemon::index_tree(&sessions, 0)
                .ok()?
                .into_iter()
                .find_map(|(name, path)| {
                    crate::codex_store::codex_rollout_matches(&name, session_id).then_some(path)
                })
        }
        _ => None,
    }
}

fn hygiene_reading(
    harness: &str,
    session_id: &str,
    shape: &str,
    transcript: Option<&Path>,
    team: &TeamLineage,
) -> Value {
    let team = json!({
        "inherited": team.inherited,
        "from_session": team.from_session,
    });
    let Some(transcript) = transcript else {
        return json!({
            "state": "unmeasurable",
            "reason": format!("transcript not found for {harness} session {session_id}"),
            "transcript": null,
            "applicable": 0,
            "declared": 5,
            "violations": [],
            "role": team,
        });
    };
    let entries = match crate::lead_hygiene::entries_from_transcript(harness, transcript) {
        Ok(entries) => entries,
        Err(error) => {
            return json!({
                "state": "unmeasurable",
                "reason": error,
                "transcript": transcript.display().to_string(),
                "applicable": 0,
                "declared": 5,
                "violations": [],
                "role": team,
            });
        }
    };
    let checks = crate::lead_hygiene::run_checks(&entries, Some(shape));
    let applicable = checks.iter().filter(|check| check.applicable).count();
    let violations = checks
        .iter()
        .filter(|check| check.status == "violation")
        .map(|check| {
            json!({
                "check": check.check,
                "index": check.index,
                "detail": check.detail,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "state": "measured",
        "reason": null,
        "transcript": transcript.display().to_string(),
        "applicable": applicable,
        "declared": checks.len(),
        "violations": violations,
        "role": team,
    })
}

fn render_hygiene_line(hygiene: &Value) -> String {
    if hygiene["state"].as_str() != Some("measured") {
        return format!(
            "hygiene: unmeasurable ({})",
            hygiene["reason"].as_str().unwrap_or("reading unavailable")
        );
    }
    let violations = hygiene["violations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let count = violations.len();
    let noun = if count == 1 {
        "violation"
    } else {
        "violations"
    };
    let mut line = format!(
        "hygiene: {} of {} checks applicable, {count} {noun}",
        hygiene["applicable"].as_u64().unwrap_or(0),
        hygiene["declared"].as_u64().unwrap_or(5)
    );
    if !violations.is_empty() {
        let details = violations
            .iter()
            .map(|violation| {
                let check = violation["check"].as_str().unwrap_or("unknown check");
                match violation["index"].as_u64() {
                    Some(index) => format!("{check} at call {index}"),
                    None => check.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(" ({details})"));
    }
    match hygiene["role"]["inherited"].as_bool() {
        Some(true) => {
            if let Some(session) = hygiene["role"]["from_session"].as_str() {
                line.push_str(&format!("; team inherited from {session}"));
            } else {
                line.push_str("; team inherited");
            }
        }
        Some(false) => line.push_str("; team not inherited"),
        None => {}
    }
    line
}

fn render_verdict(payload: &Value) -> String {
    let mut lines = vec![format!(
        "verdict: {}",
        payload["verdict"].as_str().unwrap_or("")
    )];
    lines.push(format!(
        "manifest: {}",
        payload["manifest"]["path"].as_str().unwrap_or("")
    ));
    for b in payload["bounds"].as_array().unwrap() {
        let state = b["state"].as_str().unwrap_or("");
        let body = match (b["value"].as_u64(), b["ceiling"].as_u64(), state) {
            (Some(v), Some(c), _) => format!("{v} of {c} ({state})"),
            (Some(v), None, "within") => format!("{v} recorded (no ceiling)"),
            _ => state.to_string(),
        };
        lines.push(format!("  {}: {}", b["name"].as_str().unwrap_or(""), body));
    }
    lines.push(format!(
        "readings: {} fire(s), loop check-ins {} (expected: {}, stale: {}), compactions {} ({}), inherited undelivered {}, closed in window {}, scanned {} rows across {} journal(s)",
        payload["fires"],
        payload["checkins"],
        payload["checkins_expected"],
        payload["checkins_stale"],
        payload["compactions"],
        payload["compactions_source"].as_str().unwrap_or(""),
        payload["inherited_undelivered"],
        payload["inherited_closed_in_window"],
        payload["scanned"],
        payload["journals"].as_array().map(|a| a.len()).unwrap_or(0),
    ));
    if payload["lifetime"] == "partial" {
        lines.push(format!(
            "lifetime partial: complete since {}",
            payload["coverage"]["complete_since"]
                .as_str()
                .unwrap_or("?")
        ));
    }
    if let Some(error) = payload["compactions_error"].as_str() {
        lines.push(format!("compactions read: {error}"));
    }
    lines.push(render_term_line(&payload["term"]));
    if let Some(hygiene) = payload.get("hygiene") {
        lines.push(render_hygiene_line(hygiene));
    }
    lines.join("\n")
}

/// JSON shape of the verdict's `term` field: the team's declared or default
/// spec, whether a lead declared it, and the reading's state, evidence only -
/// a `Reached` term does not change the `Verdict` enum, the Stop hook gate
/// does the forcing.
fn term_reading_payload(reading: &crate::lead_term::TermReading) -> Value {
    let (used, of, unreadable_reason) = match &reading.state {
        crate::lead_term::TermState::Within { used, of }
        | crate::lead_term::TermState::Reached { used, of } => {
            (Some(used.clone()), Some(of.clone()), None)
        }
        crate::lead_term::TermState::Unreadable(why) => (None, None, Some(why.clone())),
    };
    json!({
        "spec": reading.spec,
        "declared": reading.declared,
        "state": crate::lead_term::state_word(&reading.state),
        "used": used,
        "of": of,
        "unreadable_reason": unreadable_reason,
    })
}

fn render_term_line(term: &Value) -> String {
    let spec = term["spec"].as_str().unwrap_or("");
    let default_note = if term["declared"].as_bool().unwrap_or(true) {
        ""
    } else {
        " (default)"
    };
    match term["state"].as_str().unwrap_or("") {
        "unreadable" => format!(
            "term: {spec}{default_note} unreadable: {}",
            term["unreadable_reason"].as_str().unwrap_or("unknown")
        ),
        state => format!(
            "term: {spec}{default_note} {} of {}, {state}",
            term["used"].as_str().unwrap_or("?"),
            term["of"].as_str().unwrap_or("?"),
        ),
    }
}

fn checkins_stale(
    readings: &VerdictReadings,
    now_epoch: i64,
    interval_secs: i64,
    team_age_secs: i64,
) -> bool {
    if !readings.checkins_expected {
        return false;
    }
    if readings.checkins == 0 {
        return team_age_secs >= interval_secs.saturating_mul(2);
    }
    readings
        .last_checkin_epoch
        .map(|last| now_epoch - last >= interval_secs.saturating_mul(2))
        .unwrap_or(true)
}

fn compaction_reading(
    manifest: &crate::loopcheck::LeadManifest,
    harness: &str,
    journal_count: u64,
) -> (u64, &'static str, Option<String>) {
    let session_id = manifest.harness_session_id.as_deref().unwrap_or_default();
    if session_id.is_empty() {
        return (journal_count, "unmeasurable", None);
    }
    if harness != "claude" {
        return (journal_count, "journal", None);
    }
    let Some(path) = crate::claude_drive::find_transcript_in(
        &crate::claude_drive::claude_projects_dir(),
        session_id,
    ) else {
        return (
            journal_count,
            "journal",
            Some(format!(
                "transcript not found for harness session {session_id}"
            )),
        );
    };
    let since = manifest.created_at.as_deref().and_then(|ts| {
        chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|value| value.timestamp())
    });
    match crate::compaction::count_boundaries_since(&path, since) {
        Ok(count) => (count, "transcript", None),
        Err(err) => (
            journal_count,
            "journal",
            Some(format!("{}: {err}", path.display())),
        ),
    }
}

#[cfg(test)]
mod verdict_tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    fn readings() -> VerdictReadings {
        VerdictReadings {
            fires: 0,
            last_actionable: None,
            max_iterations: 40,
            respawn_count: 0,
            respawn_ceiling: 4,
            compactions: 0,
            compaction_ceiling: Some(3),
            block_cap: None,
            compactions_measurable: true,
            checkins: 0,
            checkins_expected: false,
            checkins_stale: false,
            last_checkin_epoch: None,
            nudges: 0,
            terminations: Vec::new(),
            inherited_undelivered: 0,
            inherited_closed_in_window: 0,
            lifetime_partial: false,
        }
    }

    #[test]
    fn ac1_iterations_exceeded_reads_degraded() {
        let mut r = readings();
        r.fires = 87;
        let (v, bounds) = verdict(&r);
        assert_eq!(v, Verdict::Degraded);
        let it = bounds.iter().find(|b| b.name == "iterations").unwrap();
        assert_eq!(it.value, Some(87));
        assert_eq!(it.ceiling, Some(40));
        assert_eq!(it.state, BoundState::Exceeded);
    }

    #[test]
    fn the_fire_before_the_ceiling_reads_exceeded() {
        // bound_breached terminates on total + 1 >= max BEFORE appending the
        // breaching fire, so 39 recorded fires against a ceiling of 40 is a
        // spent bound, not a within one.
        let mut r = readings();
        r.fires = 39;
        let (v, bounds) = verdict(&r);
        assert_eq!(v, Verdict::Degraded);
        let it = bounds.iter().find(|b| b.name == "iterations").unwrap();
        assert_eq!(it.state, BoundState::Exceeded);
    }

    #[test]
    fn compactions_before_the_team_start_do_not_count() {
        // A harness session that compacted before it was teamed must not
        // hand the new lead a spent bound.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        writeln!(
            fh,
            "{}",
            json!({"ts": "2026-09-01T00:00:00Z", "type": "context_snapshot", "source": "hook",
                   "data": {"session_id": "hs1", "harness": "claude", "entry_state": "post_compact",
                            "context_bytes": 1, "estimated_tokens": 1, "source_hashes": [],
                            "source_manifest": [], "measurement_complete": true}})
        )
        .unwrap();
        writeln!(
            fh,
            "{}",
            json!({"ts": "2026-09-12T00:00:00Z", "type": "context_snapshot", "source": "hook",
                   "data": {"session_id": "hs1", "harness": "claude", "entry_state": "post_compact",
                            "context_bytes": 1, "estimated_tokens": 1, "source_hashes": [],
                            "source_manifest": [], "measurement_complete": true}})
        )
        .unwrap();
        drop(fh);
        let (r, _, _, _, _) = scan_readings(
            std::slice::from_ref(&path),
            "kg1",
            "hs1",
            "x-bbbb",
            "2026-09-10T00:00:00Z",
        )
        .unwrap();
        assert_eq!(r.compactions, 1);
    }

    #[test]
    fn ac2_absent_bounds_never_read_within() {
        let mut r = readings();
        r.respawn_ceiling = 0;
        r.block_cap = None;
        r.compactions_measurable = false;
        let (v, bounds) = verdict(&r);
        assert_eq!(v, Verdict::Converging);
        for name in ["respawns", "block_cap", "compactions"] {
            let b = bounds.iter().find(|b| b.name == name).unwrap();
            assert_eq!(b.state, BoundState::Absent, "{name}");
            assert_ne!(b.state, BoundState::Within, "{name}");
        }
    }

    #[test]
    fn an_old_team_without_readable_checkins_reads_unknown() {
        let mut r = readings();
        r.checkins_expected = true;
        r.checkins_stale = true;
        let (v, _) = verdict(&r);
        assert_eq!(v, Verdict::Unknown);

        r.checkins = 1;
        r.checkins_stale = false;
        let (v, _) = verdict(&r);
        assert_eq!(v, Verdict::Converging);
    }

    #[test]
    fn checkin_staleness_respects_the_two_interval_hook_cadence() {
        let mut r = readings();
        r.checkins_expected = true;
        r.checkins = 1;
        r.last_checkin_epoch = Some(1_000);
        assert!(!checkins_stale(&r, 1_000 + 1_800, 1_800, 1_800));
        assert!(checkins_stale(&r, 1_000 + 3_600, 1_800, 3_600));
        r.checkins = 0;
        r.last_checkin_epoch = None;
        assert!(!checkins_stale(&r, 1_000 + 1_800, 1_800, 1_800));
        assert!(checkins_stale(&r, 1_000 + 3_600, 1_800, 3_600));
    }

    #[test]
    fn a_breached_bound_outweighs_missing_checkins() {
        let mut r = readings();
        r.checkins_expected = true;
        r.checkins_stale = true;
        r.fires = 40;
        let (v, _) = verdict(&r);
        assert_eq!(v, Verdict::Degraded);
    }

    #[test]
    fn ac3_quiet_board_with_inherited_backlog_and_no_closure_reads_stalled() {
        let mut r = readings();
        r.last_actionable = Some(0);
        r.inherited_undelivered = 5;
        r.inherited_closed_in_window = 0;
        let (v, _) = verdict(&r);
        assert_eq!(v, Verdict::Stalled);
    }

    #[test]
    fn ac4_filed_nodes_never_make_a_lead_read_stalled() {
        let mut r = readings();
        r.last_actionable = Some(0);
        r.inherited_undelivered = 5;
        r.inherited_closed_in_window = 1;
        let (v, _) = verdict(&r);
        assert_eq!(v, Verdict::Converging);
    }

    #[test]
    fn filed_undelivered_alone_never_stalls() {
        let mut r = readings();
        r.last_actionable = Some(0);
        r.inherited_undelivered = 0;
        r.inherited_closed_in_window = 0;
        let (v, _) = verdict(&r);
        assert_eq!(v, Verdict::Converging);
    }

    #[test]
    fn blind_board_never_reads_quiet() {
        let mut r = readings();
        r.last_actionable = None;
        r.inherited_undelivered = 5;
        let (v, _) = verdict(&r);
        assert_eq!(v, Verdict::Converging);
    }

    #[test]
    fn recorded_block_cap_reads_within_with_its_value() {
        let mut r = readings();
        r.block_cap = Some(BlockCapReading {
            value: 9,
            source: "default".to_string(),
        });
        let (v, bounds) = verdict(&r);
        assert_eq!(v, Verdict::Converging);
        let bc = bounds.iter().find(|b| b.name == "block_cap").unwrap();
        assert_eq!(bc.state, BoundState::Within);
        assert_eq!(bc.value, Some(9));
    }

    #[test]
    fn scan_counts_kinds_for_the_right_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        let rows = [
            json!({"ts": "2026-09-10T08:00:00Z", "type": "lead_loop_check", "source": "loop",
                   "data": {"session_id": "kg1", "actionable": 2}}),
            json!({"ts": "2026-09-10T09:00:00Z", "type": "lead_loop_check", "source": "loop",
                   "data": {"session_id": "kg1", "actionable": 0}}),
            json!({"ts": "2026-09-10T09:30:00Z", "type": "lead_loop_check", "source": "loop",
                   "data": {"session_id": "other", "actionable": 0}}),
            json!({"ts": "2026-09-10T10:00:00Z", "type": "termination", "source": "loop",
                   "data": {"session_id": "kg1", "driver": "lead", "reason": "Budget"}}),
            json!({"ts": "2026-09-10T10:01:00Z", "type": "termination", "source": "loop",
                   "data": {"session_id": "kg1", "driver": "lead", "reason": "NoProgress"}}),
            json!({"ts": "2026-09-10T10:02:00Z", "type": "termination", "source": "loop",
                   "data": {"session_id": "kg1", "driver": "target", "reason": "DonePRGreen"}}),
            json!({"ts": "2026-09-10T10:03:00Z", "type": "context_snapshot", "source": "hook",
                   "data": {"session_id": "hs1", "harness": "claude", "entry_state": "post_compact",
                            "context_bytes": 1, "estimated_tokens": 1, "source_hashes": [],
                            "source_manifest": [], "measurement_complete": true}}),
            json!({"ts": "2026-09-10T10:04:00Z", "type": "context_snapshot", "source": "hook",
                   "data": {"session_id": "hs1", "harness": "claude", "entry_state": "startup",
                            "context_bytes": 1, "estimated_tokens": 1, "source_hashes": [],
                            "source_manifest": [], "measurement_complete": true}}),
            json!({"ts": "2026-09-10T10:05:00Z", "type": "loop_check_config", "source": "loop",
                   "data": {"session_id": "kg1", "block_cap": 9, "block_cap_source": "default"}}),
            json!({"ts": "2026-09-10T10:06:00Z", "type": "lead_context_nudge", "source": "hook",
                   "data": {"used_pct": 60, "trigger": 40, "role_level": 0, "role_scope": "x-bbbb"}}),
        ];
        for row in rows {
            writeln!(fh, "{row}").unwrap();
        }
        drop(fh);
        let (r, scanned, duplicates, _skipped, journals) = scan_readings(
            std::slice::from_ref(&path),
            "kg1",
            "hs1",
            "x-bbbb",
            "2026-09-01T00:00:00Z",
        )
        .unwrap();
        assert_eq!(scanned, 10);
        assert_eq!(duplicates, 0);
        assert_eq!(r.fires, 2);
        assert_eq!(r.last_actionable, Some(0));
        assert_eq!(
            r.terminations,
            vec![("Budget".to_string(), 1), ("NoProgress".to_string(), 1)]
        );
        assert_eq!(r.compactions, 1);
        assert_eq!(r.block_cap.as_ref().unwrap().value, 9);
        assert_eq!(r.block_cap.as_ref().unwrap().source, "default");
        assert_eq!(r.nudges, 1);
        assert_eq!(journals.len(), 1);
    }

    #[test]
    fn scan_counts_only_current_team_checkins_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("events.jsonl");
        let second = dir.path().join("events.2.jsonl");
        let old = json!({
            "ts": "2026-09-01T00:00:00Z",
            "type": LEAD_CHECKIN,
            "source": "loop",
            "data": {"scope": "x-bbbb", "change": "old"}
        });
        let current = json!({
            "ts": "2026-09-10T00:00:00Z",
            "type": LEAD_CHECKIN,
            "source": "loop",
            "data": {"scope": "x-bbbb", "change": "current"}
        });
        let hook = json!({
            "ts": "2026-09-11T00:00:00Z",
            "type": LEAD_CHECKIN,
            "source": "hook",
            "data": {"scope": "x-bbbb", "change": "missed beat"}
        });
        std::fs::write(&first, format!("{old}\n{current}\n{hook}\n")).unwrap();
        std::fs::write(&second, format!("{current}\n")).unwrap();

        let (r, _, _, _, _) = scan_readings(
            &[first, second],
            "kg1",
            "hs1",
            "x-bbbb",
            "2026-09-05T00:00:00Z",
        )
        .unwrap();
        assert_eq!(r.checkins, 1);
        assert_eq!(
            r.last_checkin_epoch,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-09-10T00:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
    }

    #[test]
    fn the_last_fire_answers_even_when_it_names_no_board() {
        // A board_error row carries no actionable; the newest row must still
        // be the one that answers, never an older quiet one.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        writeln!(
            fh,
            "{}",
            json!({"ts": "2026-09-10T08:00:00Z", "type": "lead_loop_check", "source": "loop",
                   "data": {"session_id": "kg1", "actionable": 0}})
        )
        .unwrap();
        writeln!(
            fh,
            "{}",
            json!({"ts": "2026-09-10T09:00:00Z", "type": "lead_loop_check", "source": "loop",
                   "data": {"session_id": "kg1", "board_error": "timeout"}})
        )
        .unwrap();
        drop(fh);
        let (r, _, _, _, _) = scan_readings(
            std::slice::from_ref(&path),
            "kg1",
            "hs1",
            "x-bbbb",
            "2026-09-01T00:00:00Z",
        )
        .unwrap();
        assert_eq!(r.fires, 2);
        assert_eq!(r.last_actionable, None);
    }

    #[test]
    fn a_config_row_never_ingests_through_an_empty_harness_id() {
        // An identity-less manifest reads block_cap absent; it must not
        // inherit another session's row that also lacks a session id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        writeln!(
            fh,
            "{}",
            json!({"ts": "2026-09-10T08:00:00Z", "type": "loop_check_config", "source": "loop",
                   "data": {"session_id": "", "block_cap": 9, "block_cap_source": "default"}})
        )
        .unwrap();
        drop(fh);
        let (r, _, _, _, _) = scan_readings(
            std::slice::from_ref(&path),
            "kg1",
            "",
            "x-bbbb",
            "2026-09-01T00:00:00Z",
        )
        .unwrap();
        assert!(r.block_cap.is_none());
    }

    #[test]
    fn the_question_summary_is_rendered_natively() {
        // The escalation question embeds this line verbatim; its first word
        // is always the verdict name, then exceeded with numbers, then absent.
        let mut r = readings();
        r.fires = 87;
        r.block_cap = None;
        r.compactions_measurable = false;
        let (v, bounds) = verdict(&r);
        let summary = bound_summary(v, &bounds);
        assert_eq!(
            summary,
            "degraded (exceeded: iterations 87 of 40; absent: compactions, block_cap)"
        );
        let (v2, b2) = verdict(&readings());
        assert_eq!(bound_summary(v2, &b2), "converging (absent: block_cap)");
    }

    /// The input tree the self-assembling verb reads: config, graph, manifest,
    /// journal. Spaces and the agents home resolve through a `DeclaredRoot`
    /// pin, the config anchors on `--cwd`, and the graph path rides that
    /// config - no ambient resolution, so a parallel test's pins never race.
    fn input_tree(lead_config: &str) -> (crate::paths::DeclaredRoot, PathBuf, PathBuf, PathBuf) {
        let root_pin = crate::paths::DeclaredRoot::declare("kvh");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let graph = root.join("home/graph.json");
        std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join(".fno")).unwrap();
        let config = root.join(".fno/config.toml");
        std::fs::write(
            &config,
            format!(
                "[paths]\ngraph_json = {:?}\n[[work.workspaces.t.projects]]\nname = \"fno\"\n{lead_config}",
                graph.display()
            ),
        )
        .unwrap();
        // The explicit pin beats cwd anchoring AND an ambient FNO_CONFIG a
        // concurrent unlocked test may have left behind (the crate's env
        // locks are fragmented; see the note on the backlog node).
        std::env::set_var("FNO_CONFIG", &config);
        // graph_json_path ignores the retired paths.graph_json config key and
        // resolves through FNO_HOME, so the home pin rides the env like the
        // config pin does: leaked deliberately, same posture as FNO_CONFIG.
        std::env::set_var("FNO_HOME", graph.parent().unwrap());
        crate::graph_store::seed_rows(
            &graph,
            &[
                json!({"id": "x-bbbb", "type": "epic", "project": "fno", "created_at": "2026-09-01T00:00:00Z"}),
                json!({"id": "x-old", "parent": "x-bbbb", "created_at": "2026-09-05T00:00:00Z"}),
                json!({"id": "x-new", "parent": "x-bbbb", "created_at": "2026-09-12T00:00:00Z"}),
            ],
        )
        .unwrap();
        let manifest = root.join("leads/x-bbbb.md");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            "---\nfno_id: kg1\nscope: x-bbbb\nharness_session_id: hs1\ncreated_at: 2026-09-10T00:00:00Z\nbudget_max_iterations: 2\nrespawn_count: 0\nrespawn_ceiling: 4\n---\nbody\n",
        )
        .unwrap();
        let journal = root.join("events.jsonl");
        let mut fh = std::fs::File::create(&journal).unwrap();
        for i in 0..3 {
            writeln!(
                fh,
                "{}",
                json!({"ts": format!("2026-09-10T0{i}:00:00Z"), "type": "lead_loop_check",
                       "source": "loop", "data": {"session_id": "kg1", "actionable": 0}})
            )
            .unwrap();
        }
        drop(fh);
        // Leak the tempdir: the config pin points inside it, so the tree must
        // outlive the test (a few KB per run, bounded by test count).
        let root = dir.keep();
        (root_pin, root, manifest, journal)
    }

    fn verdict_args(root: &Path, manifest: &Path, journal: &Path) -> Vec<String> {
        vec![
            "--cwd".to_string(),
            root.display().to_string(),
            "--scope".to_string(),
            "x-bbbb".to_string(),
            "--manifest".to_string(),
            manifest.display().to_string(),
            "--events-path".to_string(),
            journal.display().to_string(),
            "--json".to_string(),
        ]
    }

    #[test]
    fn run_end_to_end_degraded_from_inputs() {
        // No lock take here: input_tree's DeclaredRoot::declare takes the env
        // lock and the returned pin holds it for the test's lifetime; a
        // second take deadlocks (paths.rs note on declare_held).
        let (_pin, root, manifest, journal) =
            input_tree("[lead]\ncheckin_interval = \"30m\"\ncompaction_ceiling = 3\n");
        // AC1-HP: a clock in the declared agents home carries an earlier
        // start and the declared term into the verdict read.
        let store = crate::paths::AgentsHome::from_env().team_names_json();
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(
            &store,
            json!({"version": 1, "teams": {"x-bbbb": {
                "name": "warden", "generation": 1, "holder_session": "hs1",
                "nodes": [], "updated_at": "2026-09-05T00:00:00Z",
                "lead": {"session": "hs1", "scope": "x-old",
                          "armed_at": "2026-09-10T00:00:00Z",
                          "started_at": "2026-09-01T00:00:00Z",
                          "term": "span:200h"}}}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            run_lead_verdict(&verdict_args(&root, &manifest, &journal)),
            0
        );
        let parsed =
            crate::loopcheck::parse_lead_manifest(&std::fs::read_to_string(&manifest).unwrap())
                .unwrap();
        let view = crate::team_names::lead_view_in(&store, &parsed);
        assert_eq!(view.created_at.as_deref(), Some("2026-09-01T00:00:00Z"));
        assert_eq!(view.term.as_deref(), Some("span:200h"));
    }

    #[test]
    fn compaction_reading_names_transcript_and_journal_fallbacks() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let session = "a1b2c3d4-1111-2222-3333-444455556666";
        let transcript = project.join(format!("{session}.jsonl"));
        std::fs::write(
            &transcript,
            "{\"subtype\":\"compact_boundary\",\"timestamp\":\"2026-09-10T00:00:00Z\"}\n",
        )
        .unwrap();
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, dir.path());

        let mut manifest = crate::loopcheck::LeadManifest::default();
        manifest.harness_session_id = Some(session.into());
        manifest.created_at = Some("2026-09-01T00:00:00Z".into());
        assert_eq!(
            compaction_reading(&manifest, "claude", 7),
            (1, "transcript", None)
        );

        assert_eq!(
            compaction_reading(&manifest, "codex", 7),
            (7, "journal", None)
        );

        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        let (count, source, error) = compaction_reading(&manifest, "claude", 7);
        assert_eq!((count, source), (7, "journal"));
        assert!(error.unwrap().contains("transcript not found"));
    }

    #[test]
    fn a_garbage_ceiling_config_refuses_the_read_instead_of_reading_absent() {
        // Same posture the count flags had: a mistyped ceiling must not
        // degrade into an absent bound that prints as a clean reading. The
        // refusal is a config refusal now, exit 1 naming the key.
        let (_pin, root, manifest, journal) = input_tree("[lead]\ncompaction_ceiling = \"1O\"\n");
        assert_eq!(
            run_lead_verdict(&verdict_args(&root, &manifest, &journal)),
            1
        );
    }

    #[test]
    fn usage_failure_exit_two() {
        assert_eq!(run_lead_verdict(&[]), 2);
        assert_eq!(run_lead_verdict(&["--cwd".into(), "/tmp".into()]), 2);
        assert_eq!(run_lead_verdict(&["--nope".into()]), 2);
        // The precomputed facts are gone on purpose: passing one is a usage
        // failure, never a silently accepted input.
        assert_eq!(
            run_lead_verdict(&["--inherited-undelivered".into(), "5".into()]),
            2
        );
        assert_eq!(
            run_lead_verdict(&["--compaction-ceiling".into(), "3".into()]),
            2
        );
        assert_eq!(run_lead_verdict(&["--window".into(), "90m".into()]), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn journal(rows: &[Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        for row in rows {
            writeln!(fh, "{row}").unwrap();
        }
        (dir, path)
    }

    fn checkin(ts: &str, data: Value) -> Value {
        json!({"ts": ts, "type": "lead_checkin", "source": "loop", "data": data})
    }

    #[test]
    fn hook_beat_never_writes_for_a_blank_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let history = crate::loop_lead::LeadFireHistory {
            total: 0,
            dry: 0,
            last_ids: vec![],
            last_undelivered: None,
            last_terminal: None,
        };
        assert!(!hook_beat(
            &path,
            dir.path(),
            "",
            "sess",
            None,
            &history,
            chrono::Utc::now()
        ));
        assert!(!path.exists());
    }

    #[test]
    fn hook_beat_writes_one_row_per_missed_beat() {
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior_config = std::env::var_os("FNO_CONFIG");
        let (dir, path) = journal(
            &[json!({"ts": "2026-09-15T10:00:00Z", "type": "lead_checkin",
                 "source": "loop", "data": {"scope": "x-bbbb", "change": "beat"}})],
        );
        std::env::set_var("FNO_CONFIG", dir.path().join("config.toml"));
        let history = crate::loop_lead::LeadFireHistory {
            total: 3,
            dry: 1,
            last_ids: vec!["undispatched:x-1".into()],
            last_undelivered: Some(4),
            last_terminal: None,
        };
        let base = "2026-09-15T10:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let at = |mins: i64| base + chrono::Duration::minutes(mins);
        // 109 minutes old: under two 55-minute intervals, nothing writes.
        let early = hook_beat(&path, dir.path(), "x-bbbb", "sess", None, &history, at(109));
        let rows_after_early = crate::events::committed_journal_text(&path).lines().count();
        // 111 minutes old: the beat is due, one hook row.
        let due = hook_beat(&path, dir.path(), "x-bbbb", "sess", None, &history, at(111));
        let rows = crate::events::committed_journal_text(&path);
        // A fresh row resets the clock: the next stop writes nothing.
        let fresh = hook_beat(&path, dir.path(), "x-bbbb", "sess", None, &history, at(112));
        let rows_after_fresh = crate::events::committed_journal_text(&path).lines().count();
        // AC6-HP: a fresh beat under scope A by the same holder keeps scope
        // B not due - the hook's due check walks by holder, not by scope.
        let fresh_a_path = dir.path().join("events-a.jsonl");
        std::fs::write(
            &fresh_a_path,
            format!(
                "{}\n",
                json!({"ts": format!("{at}", at = at(115).to_rfc3339()), "type": "lead_checkin",
                     "source": "loop",
                     "data": {"scope": "x-aaaa", "change": "beat", "holder_session": "sess"}})
            ),
        )
        .unwrap();
        let carried = hook_beat(
            &fresh_a_path,
            dir.path(),
            "x-bbbb",
            "sess",
            Some("sess"),
            &history,
            at(116),
        );
        assert!(!carried, "the holder's fresh beat under A keeps B not due");
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }

        assert!(!early);
        assert_eq!(rows_after_early, 1);
        assert!(due);
        assert_eq!(rows.lines().count(), 2, "rows: {rows}");
        assert!(rows.contains("\"source\":\"hook\""), "rows: {rows}");
        let written: Value = serde_json::from_str(rows.lines().last().unwrap()).unwrap();
        assert_eq!(written["data"]["scope"], "x-bbbb");
        assert!(!written["data"]["change"].as_str().unwrap().is_empty());
        // AC6-HP: the hook row carries the holder session it looked up by.
        assert_eq!(written["data"]["holder_session"], json!(null));
        // The fresh row resets the clock: the next stop writes nothing.
        assert!(!fresh);
        assert_eq!(rows_after_fresh, 2);
    }

    #[test]
    fn scope_optional_scan_returns_all_canonical_scopes_and_rejections() {
        let (_dir, path) = journal(&[
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"scope": "fno", "change": "fno change"}),
            ),
            checkin(
                "2026-09-10T09:00:00Z",
                json!({"scope": "x-bbbb", "change": "epic change"}),
            ),
            checkin(
                "2026-09-10T10:00:00Z",
                json!({"role": "fno", "change": "legacy"}),
            ),
        ]);
        let payload = scan_scopes(std::slice::from_ref(&path), None).unwrap();
        assert_eq!(payload["matched"], json!(2));
        assert_eq!(payload["rejected"], json!(1));
        let scopes: Vec<&str> = payload["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["data"]["scope"].as_str())
            .collect();
        assert_eq!(scopes, ["x-bbbb", "fno"]);
        // AC2-HP: the check-in's previous-beat lookup crosses the re-scope:
        // a loop row under A carrying the holder's session is the baseline
        // B reads, so "vs last beat" names A's ts.
        let (_adir, apath) = journal(&[checkin(
            "2026-09-10T08:00:00Z",
            json!({"scope": "x-aaaa", "change": "beat on A", "holder_session": "sess-k"}),
        )]);
        let beat = previous_beat(std::slice::from_ref(&apath), "x-bbbb", Some("sess-k"), true)
            .unwrap()
            .expect("the holder's row under A is the baseline for B");
        assert_eq!(beat["data"]["scope"], json!("x-aaaa"));
        assert_eq!(beat["ts"], json!("2026-09-10T08:00:00Z"));
        // AC5-EDGE: rows journalled before this change carry no
        // holder_session; the lookup falls back to the scope's own newest
        // loop row, exactly the pre-holder read.
        let fallback = previous_beat(std::slice::from_ref(&path), "x-bbbb", Some("sess-x"), true)
            .unwrap()
            .map(|r| r["data"]["scope"].clone());
        assert_eq!(fallback, Some(json!("x-bbbb")));
        let none =
            previous_beat(std::slice::from_ref(&apath), "x-bbbb", Some("sess-x"), true).unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn newest_first_with_evidence_intact() {
        let (_dir, path) = journal(&[
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"scope": "x-aaaa", "change": "first"}),
            ),
            json!({"ts": "2026-09-10T09:00:00Z", "type": "phase_transition", "source": "loop", "data": {"phase": "review"}}),
            checkin(
                "2026-09-10T09:30:00Z",
                json!({"scope": "other", "change": "elsewhere"}),
            ),
            checkin(
                "2026-09-10T12:00:00Z",
                json!({"scope": "x-aaaa", "change": "merged PR 1710", "open_prs_fleet": 3}),
            ),
        ]);
        let payload = scan(std::slice::from_ref(&path), "x-aaaa").unwrap();
        assert_eq!(payload["scanned"], json!(2), "only lead rows are read");
        assert_eq!(payload["matched"], json!(2));
        let events = payload["events"].as_array().unwrap();
        assert_eq!(events[0]["data"]["open_prs_fleet"], json!(3));
        assert_eq!(events[0]["ts"], json!("2026-09-10T12:00:00Z"));
        assert_eq!(events[1]["data"]["change"], json!("first"));
    }

    #[test]
    fn alias_rows_are_rejected_evidence_attributed_by_line() {
        let (_dir, path) = journal(&[
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"role_scope": "x-aaaa", "change": "old"}),
            ),
            checkin(
                "2026-09-10T08:30:00Z",
                json!({"scope": "x-aaaa", "result": "no change"}),
            ),
            checkin("2026-09-10T09:00:00Z", json!({"change": "no scope named"})),
        ]);
        let payload = scan(std::slice::from_ref(&path), "x-aaaa").unwrap();
        assert_eq!(payload["matched"], json!(0));
        assert_eq!(payload["rejected"], json!(3));
        let legacy = payload["rejected_legacy"].as_array().unwrap();
        assert_eq!(legacy.len(), 2);
        assert_eq!(legacy[0]["forbidden"], json!(["role_scope"]));
        assert_eq!(legacy[0]["missing"], json!(["scope"]));
        assert_eq!(legacy[1]["forbidden"], json!(["result"]));
    }

    #[test]
    fn missing_journal_reads_as_positive_zero() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.jsonl");
        let payload = scan(std::slice::from_ref(&absent), "x-aaaa").unwrap();
        assert_eq!(payload["scanned"], json!(0));
        assert_eq!(payload["matched"], json!(0));
        let journals = payload["journals"].as_array().unwrap();
        assert_eq!(journals.len(), 1);
        assert!(journals[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("absent.jsonl"));
        assert_eq!(journals[0]["scanned"], json!(0));
    }

    #[test]
    fn corrupt_line_is_stored_and_history_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        writeln!(fh, "{{not json").unwrap();
        drop(fh);
        // A corrupt line is stored with its reject_reason; it is never a
        // lead row, so the read succeeds and reports zero.
        let payload = scan(std::slice::from_ref(&path), "x-aaaa").unwrap();
        assert_eq!(payload["scanned"], json!(0));
        assert_eq!(payload["matched"], json!(0));
        let journals = payload["journals"].as_array().unwrap();
        assert_eq!(journals[0]["corrupt"], json!(1));
    }

    #[test]
    fn newest_first_across_rotations() {
        let dir = tempfile::tempdir().unwrap();
        let rotated = dir.path().join("events.jsonl.1");
        let live = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&rotated).unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-09T08:00:00Z",
                json!({"scope": "x-aaaa", "change": "rotated past"}),
            )
        )
        .unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"scope": "x-aaaa", "change": "older"}),
            )
        )
        .unwrap();
        drop(fh);
        let mut fh = std::fs::File::create(&live).unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-10T12:00:00Z",
                json!({"scope": "x-aaaa", "change": "newest"}),
            )
        )
        .unwrap();
        drop(fh);
        let payload = scan(&[rotated, live], "x-aaaa").unwrap();
        assert_eq!(payload["scanned"], json!(3));
        assert_eq!(payload["matched"], json!(3));
        let events = payload["events"].as_array().unwrap();
        assert_eq!(events[0]["data"]["change"], json!("newest"));
        assert_eq!(events[1]["data"]["change"], json!("older"));
        assert_eq!(events[2]["data"]["change"], json!("rotated past"));
        let journals = payload["journals"].as_array().unwrap();
        // Generations collapse: one live journal, one store entry naming it.
        assert_eq!(journals.len(), 1);
        assert!(journals[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("events.jsonl"));
        assert!(journals[0]["store"]
            .as_str()
            .unwrap()
            .ends_with("events.db"));
        assert_eq!(journals[0]["ingested"], json!(3));
        assert_eq!(journals[0]["scanned"], json!(3));
        assert_eq!(journals[0]["matched"], json!(3));
    }

    #[test]
    fn mirrored_duplicate_collapses_and_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let space = dir.path().join("events.jsonl");
        let mirror = dir.path().join("global.jsonl");
        let row = checkin(
            "2026-09-10T12:00:00Z",
            json!({"scope": "x-aaaa", "change": "mirrored"}),
        );
        for path in [&space, &mirror] {
            let mut fh = std::fs::File::create(path).unwrap();
            writeln!(fh, "{row}").unwrap();
            drop(fh);
        }
        let payload = scan(&[space, mirror], "x-aaaa").unwrap();
        assert_eq!(payload["matched"], json!(1));
        assert_eq!(payload["duplicates"], json!(1));
    }

    #[test]
    fn legacy_rows_name_their_file() {
        let dir = tempfile::tempdir().unwrap();
        let rotated = dir.path().join("events.jsonl.1");
        let mut fh = std::fs::File::create(&rotated).unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"role_scope": "x-aaaa", "change": "old"}),
            )
        )
        .unwrap();
        drop(fh);
        let payload = scan(std::slice::from_ref(&rotated), "x-aaaa").unwrap();
        let legacy = payload["rejected_legacy"].as_array().unwrap();
        assert_eq!(legacy.len(), 1);
        assert!(
            legacy[0]["file"].as_str().unwrap().ends_with("events.db"),
            "a line number does not survive rotation; the store does"
        );
        assert_eq!(legacy[0]["ts"], json!("2026-09-10T08:00:00Z"));
    }

    #[test]
    fn unreadable_store_errors_and_names_it() {
        // AC4-ERR: a blocked store is an error, never an empty history.
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("events.jsonl");
        std::fs::create_dir(dir.path().join("events.db")).unwrap();
        let err = scan(std::slice::from_ref(&live), "x-aaaa").unwrap_err();
        assert!(err.contains("events.db"), "err: {err}");
    }

    #[test]
    fn run_relays_json_and_human_formats() {
        let (_dir, path) = journal(&[checkin(
            "2026-09-10T12:00:00Z",
            json!({"scope": "x-aaaa", "change": "did a thing", "open_prs_fleet": 3}),
        )]);
        let args = vec![
            "--scope".to_string(),
            "x-aaaa".to_string(),
            "--events-path".to_string(),
            path.display().to_string(),
            "--json".to_string(),
        ];
        assert_eq!(run_lead_history(&args), 0);
        let text_args = args.clone();
        assert_eq!(run_lead_history(&text_args), 0);
    }

    #[test]
    fn the_short_json_spelling_parses_like_the_long_one() {
        let (_dir, path) = journal(&[checkin(
            "2026-09-10T12:00:00Z",
            json!({"scope": "x-aaaa", "change": "did a thing", "open_prs_fleet": 3}),
        )]);
        let args = vec![
            "-J".to_string(),
            "--scope".to_string(),
            "x-aaaa".to_string(),
            "--events-path".to_string(),
            path.display().to_string(),
        ];
        // -J is accepted, then the normal required-args validation runs.
        assert_eq!(run_lead_history(&args), 0);
        // Missing required args refuse with usage (2), never "unknown flag".
        // --scope keeps the assert off the registry resolve, which refuses
        // an undeclared test home instead of answering.
        assert_eq!(
            run_lead_history(&[
                "-J".to_string(),
                "--scope".to_string(),
                "x-aaaa".to_string()
            ]),
            2
        );
    }

    #[test]
    fn usage_failure_exit_two() {
        assert_eq!(run_lead_history(&[]), 2);
        assert_eq!(
            run_lead_history(&["--scope".to_string(), "x".to_string()]),
            2
        );
        assert_eq!(run_lead_history(&["--nope".to_string()]), 2);
    }

    #[test]
    fn in_tenure_reads_a_fractional_event_as_its_own_second() {
        // The manifest stamp carries no fraction; the snapshot does. After
        // normalization both land on the same second, so a compaction AT the
        // teaming second stays in tenure instead of sorting before it.
        assert!(in_tenure(
            "2026-09-10T12:00:00.500Z",
            "2026-09-10T12:00:00Z"
        ));
        assert!(in_tenure(
            "2026-09-10T12:00:00.500+00:00",
            "2026-09-10T12:00:00Z"
        ));
        assert!(in_tenure("2026-09-10T12:00:01Z", "2026-09-10T12:00:00Z"));
        assert!(!in_tenure(
            "2026-09-10T11:59:59.999Z",
            "2026-09-10T12:00:00Z"
        ));
    }

    #[test]
    fn scan_readings_counts_a_store_committed_fire() {
        // AC4-TEAM
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        let line = json!({
            "ts": "2026-09-10T12:00:01Z", "type": LEAD_LOOP_CHECK, "source": "hook",
            "data": {"session_id": "team-1", "actionable": 0}
        });
        crate::event_store::append_envelope(&journal, &line.to_string(), None).unwrap();
        let (r, scanned, _dupes, _skipped, journals) =
            scan_readings(&[journal], "team-1", "", "fno", "2026-09-10T12:00:00Z").unwrap();
        assert_eq!(r.fires, 1, "{r:?}");
        assert_eq!(scanned, 1);
        assert_eq!(journals.len(), 1);
    }

    #[test]
    fn scan_readings_skips_a_malformed_line_and_counts_it() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        let good = json!({
            "ts": "2026-09-10T12:00:01Z", "type": LEAD_LOOP_CHECK, "source": "hook",
            "data": {"session_id": "team-1", "actionable": 0}
        });
        // The torn-write shape: after `"source":"daemon",` the parser meets
        // `{` where a key belongs, the error class the read used to die on.
        let torn =
            r#"{"ts":"2026-10-01T00:44:44Z","type":"pane_closed","source":"daemon",{"bad":1}"#;
        std::fs::write(&journal, format!("{good}\n{torn}\n")).unwrap();
        let (r, scanned, _dupes, skipped, journals) =
            scan_readings(&[journal], "team-1", "", "fno", "2026-09-10T12:00:00Z").unwrap();
        assert_eq!(r.fires, 1, "{r:?}");
        assert_eq!(scanned, 1, "only the good line counts as scanned");
        assert_eq!(skipped, 1);
        assert_eq!(journals[0].2, 1);
    }

    struct EnvRestore {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn verdict_payload(hygiene: Value) -> Value {
        json!({
            "verdict": "healthy",
            "manifest": {"path": "/tmp/lead.md"},
            "bounds": [],
            "fires": 0,
            "checkins": 0,
            "checkins_expected": false,
            "checkins_stale": false,
            "compactions": 0,
            "compactions_source": "manifest",
            "compactions_error": null,
            "inherited_undelivered": 0,
            "inherited_closed_in_window": 0,
            "scanned": 0,
            "journals": [],
            "term": {
                "spec": "span:96h",
                "declared": false,
                "state": "within",
                "used": "0h",
                "of": "96h",
                "unreadable_reason": null
            },
            "hygiene": hygiene
        })
    }

    fn claude_tool(name: &str, input: Value) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "tool_use", "name": name, "input": input}]
            }
        })
    }

    #[test]
    fn holder_hygiene_is_measured_and_renders_after_term() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let projects = tempfile::tempdir().unwrap();
        let project = projects.path().join("-Users-x-code-proj");
        std::fs::create_dir_all(&project).unwrap();
        let session_id = "a1b2c3d4-1111-2222-3333-444455556666";
        let transcript = project.join(format!("{session_id}.jsonl"));
        let rows = [
            claude_tool(
                "Read",
                json!({"file_path":"crates/fno-agents/src/lead_history.rs"}),
            ),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "crates/fno-agents/src/lead_history.rs:40 owns the reading."}]
                }
            }),
            claude_tool("Bash", json!({"command":"fno agents org --json"})),
            claude_tool("Bash", json!({"command":"fno do pr watch status"})),
            claude_tool("Bash", json!({"command":"fno whoami context"})),
            claude_tool("Bash", json!({"command":"fno agents spawn worker"})),
            claude_tool("Bash", json!({"command":"fno agents mail send ruling"})),
        ];
        std::fs::write(
            &transcript,
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let _projects_env = EnvRestore::set(crate::claude_drive::PROJECTS_DIR_ENV, projects.path());
        let transcript = hygiene_transcript_for_holder("claude", session_id);
        let team = TeamLineage {
            inherited: Some(true),
            from_session: Some("grantor-session".to_string()),
        };
        let hygiene = hygiene_reading("claude", session_id, "org", transcript.as_deref(), &team);
        assert_eq!(hygiene["state"], "measured");
        assert_eq!(hygiene["applicable"], 4);
        assert_eq!(hygiene["declared"], 5);
        assert!(hygiene["violations"].as_array().unwrap().is_empty());
        assert_eq!(hygiene["role"]["inherited"], true);
        assert_eq!(hygiene["role"]["from_session"], "grantor-session");

        let rendered = render_verdict(&verdict_payload(hygiene));
        let term_at = rendered.find("term:").expect("term line present");
        let hygiene_at = rendered.find("hygiene:").expect("hygiene line present");
        assert!(term_at < hygiene_at, "{rendered}");
        assert!(rendered.contains(
            "hygiene: 4 of 5 checks applicable, 0 violations; team inherited from grantor-session"
        ));
    }

    #[test]
    fn missing_holder_transcript_is_unmeasurable() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let projects = tempfile::tempdir().unwrap();
        let _projects_env = EnvRestore::set(crate::claude_drive::PROJECTS_DIR_ENV, projects.path());
        let session_id = "a1b2c3d4-1111-2222-3333-444455556667";
        let transcript = hygiene_transcript_for_holder("claude", session_id);
        assert!(transcript.is_none());
        let hygiene = hygiene_reading(
            "claude",
            session_id,
            "org",
            transcript.as_deref(),
            &TeamLineage::default(),
        );
        assert_eq!(hygiene["state"], "unmeasurable");
        assert!(hygiene["reason"]
            .as_str()
            .unwrap()
            .contains("claude session a1b2c3d4-1111-2222-3333-444455556667"));
        let rendered = render_verdict(&verdict_payload(hygiene));
        assert!(rendered.contains("hygiene: unmeasurable ("), "{rendered}");
        assert!(rendered.starts_with("verdict: healthy"), "{rendered}");
    }

    #[test]
    fn codex_rollout_context_probe_is_measured() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let session_id = "codex-thread-123";
        let rollout = home
            .path()
            .join("sessions/2026/09/24/rollout-2026-codex-thread-123.jsonl");
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(
            &rollout,
            concat!(
                "{\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"How much context remains?\"}}\n",
                "{\"payload\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"fno whoami context\"}}\n"
            ),
        )
        .unwrap();
        let _codex_env = EnvRestore::set("CODEX_HOME", home.path());
        let transcript = hygiene_transcript_for_holder("codex", session_id)
            .expect("matching rollout is found under CODEX_HOME");
        assert_eq!(transcript, rollout);
        let hygiene = hygiene_reading(
            "codex",
            session_id,
            "org",
            Some(&transcript),
            &TeamLineage::default(),
        );
        assert_eq!(hygiene["state"], "measured");
        assert!(hygiene["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["check"] == "check5_context_timing_heuristic" }));
    }

    #[test]
    fn oversized_holder_transcript_is_unmeasurable() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("large.jsonl");
        let file = std::fs::File::create(&transcript).unwrap();
        file.set_len(crate::lead_hygiene::CHECKIN_TRANSCRIPT_BUDGET_BYTES + 1)
            .unwrap();
        let hygiene = hygiene_reading(
            "claude",
            "large-session",
            "org",
            Some(&transcript),
            &TeamLineage::default(),
        );
        assert_eq!(hygiene["state"], "unmeasurable");
        assert!(hygiene["reason"]
            .as_str()
            .unwrap()
            .contains("transcript over cap"));
    }

    #[test]
    fn partial_lifetime_reads_unmeasured_never_within() {
        // The team predates the epoch: an under-ceiling fire count is a
        // lower bound, so the iterations bound reads unmeasured, never
        // within. A lower bound over the ceiling is still exceeded.
        let mut r = VerdictReadings {
            max_iterations: 40,
            respawn_ceiling: 4,
            compaction_ceiling: Some(3),
            ..Default::default()
        };
        r.lifetime_partial = true;
        r.fires = 2;
        let (v, bounds) = verdict(&r);
        assert_ne!(v, Verdict::Degraded);
        let iterations = bounds.iter().find(|b| b.name == "iterations").unwrap();
        assert_eq!(iterations.state, BoundState::Unmeasured);
        r.fires = 87;
        let (v, bounds) = verdict(&r);
        assert_eq!(v, Verdict::Degraded);
        let iterations = bounds.iter().find(|b| b.name == "iterations").unwrap();
        assert_eq!(iterations.state, BoundState::Exceeded);
    }

    #[test]
    fn scoped_checkin_survives_vanished_live_journal() {
        // The durable-history acceptance: the check-in lives in the store,
        // so a vanished live journal (the rotation-era failure mode) never
        // blanks the lead history.
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("events.jsonl");
        let checkin = json!({
            "ts": "2026-09-10T00:00:00Z",
            "type": LEAD_CHECKIN,
            "source": "loop",
            "data": {"scope": "x-bbbb", "change": "tenure"}
        });
        std::fs::write(&live, format!("{checkin}\n")).unwrap();
        crate::event_store::sync(&live).unwrap();
        // The live journal leaves its path: the row survives in the SQL
        // store. Removal, not a rename, so only the store can serve the row.
        std::fs::remove_file(&live).unwrap();
        let payload = scan(&[live], "x-bbbb").unwrap();
        assert_eq!(payload["matched"], 1, "{payload}");
        assert_eq!(payload["complete_since"].as_str().is_some(), true);
    }
}
