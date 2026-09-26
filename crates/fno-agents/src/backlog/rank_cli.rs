//! `fno backlog rank`'s lane pin, ported from the Python command the grouped
//! dispatcher routes here (the Python leg is deleted). Owns: the liberal id
//! gate, the operator fence (owned harness, then the canonical stamp), the
//! exactly-one-action contract, the (column, project) lane ladder with live-
//! epic child scoping and effective-priority promotion, midpoint arithmetic,
//! the truthful dispatcher note, and the plan-projection seam. Byte-faithful
//! to the captured goldens (handoff-evidence goldens-rank).

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::settings;
use crate::backlog_ready::{
    descendants_of, epics_with_child_progress, get_str, in_progress_epic_ids, is_dict,
    live_epic_for, make_effective_priority, priority_name, rank_band, truthy,
};
use crate::graph_store::entry_id;
use crate::paths::AgentsHome;

const RANK_HELP: &str = "Curate a node's position within its (column, project) board lane.

Usage: fno backlog rank [OPTIONS] <TASK_ID>

Arguments:
  <TASK_ID>  Feature ID (ab-XXXXXXXX) to rank

Options:
      --top               Pin to the front of its (column, project) lane
      --bottom            Send to the back of the ranked band in its lane
      --before <ID>       Place just before a ranked anchor in the same lane
      --after <ID>        Place just after a ranked anchor in the same lane
      --clear             Clear the rank (rejoin the unranked priority flow)
      --within-epic       Rank within the node's live epic (child default; refused without one)
      --operator          The operator's own pin, from inside an agent session
  -h, --help              Print help";

/// The parsed invocation; `None` is the usage-error shape.
struct RankArgs {
    task_id: String,
    top: bool,
    bottom: bool,
    before: Option<String>,
    after: Option<String>,
    clear: bool,
    within_epic: bool,
    operator: bool,
}

impl RankArgs {
    fn parse(tail: &[String]) -> Option<RankArgs> {
        let mut args = RankArgs {
            task_id: String::new(),
            top: false,
            bottom: false,
            before: None,
            after: None,
            clear: false,
            within_epic: false,
            operator: false,
        };
        let mut task_id: Option<String> = None;
        let mut i = 0;
        while i < tail.len() {
            match tail[i].as_str() {
                "--top" => args.top = true,
                "--bottom" => args.bottom = true,
                "--before" => {
                    i += 1;
                    args.before = Some(tail.get(i)?.clone());
                }
                "--after" => {
                    i += 1;
                    args.after = Some(tail.get(i)?.clone());
                }
                "--clear" => args.clear = true,
                "--within-epic" => args.within_epic = true,
                "--operator" => args.operator = true,
                other => {
                    if other.starts_with('-') {
                        return None;
                    }
                    if task_id.is_some() {
                        return None;
                    }
                    task_id = Some(other.to_string());
                }
            }
            i += 1;
        }
        args.task_id = task_id?;
        Some(args)
    }
}

/// `has_node_id_prefix`: a well-formed `<prefix>-<4..8 hex>` id, or any
/// string carrying the configured/legacy prefix with a non-strict suffix.
fn has_node_id_prefix(s: &str) -> bool {
    if is_wellformed_node_id(s) {
        return true;
    }
    s.starts_with(settings::node_id_prefix().as_str()) || s.starts_with("ab-")
}

/// `[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}` fullmatch.
fn is_wellformed_node_id(s: &str) -> bool {
    let Some((prefix, suffix)) = s.split_once('-') else {
        return false;
    };
    let mut chars = prefix.chars();
    let valid_prefix = matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.count() <= 7
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let valid_suffix = (4..=8).contains(&suffix.len())
        && suffix
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    valid_prefix && valid_suffix
}

/// The fence: the harness name when an agent runs this, `None` in an
/// operator shell. The owned-harness prover leads; a usable canonical stamp
/// (anything but absent) reads as an agent even without an owned harness.
fn agent_harness_writing_rank() -> Option<String> {
    let get = |name: &str| std::env::var(name).ok();
    if let Some(harness) =
        crate::spawn_context::resolve_self_identity(&get, None, None, &AgentsHome::from_env())
            .harness
            .filter(|h| !h.is_empty())
    {
        return Some(harness);
    }
    let (_, harness, disposition) = crate::spawn_context::parse_canonical_stamp(&get);
    match disposition {
        crate::claims::CanonicalDisposition::Absent => None,
        _ => Some(harness.unwrap_or_else(|| "agent".to_string())),
    }
}

fn agent_rank_refusal(task_id: &str, harness: &str) -> String {
    format!(
        "Error: rank is operator-only; this {harness} session may not write it.\n\
         Every --top writes min(rank) - 1, so agent pins form a stack in which \
         the last writer wins and importance is never computed.\nVote instead:\n\
         \x20 fno backlog encounter {task_id} --evidence \"what it cost you\"\n\
         \x20 fno backlog update {task_id} --priority p1|p2|p3\n\
         The graph records no writer for a rank, so nothing downstream could \
         tell yours from the operator's. The escape hatch in --help is theirs."
    )
}

/// The kanban column (`render._kanban_column`): roadmap/deferred/superseded
/// are off-board, done/queued/claimed-route and the promoted priority decide
/// the rest.
fn kanban_column(
    e: &Value,
    in_progress_epics: &BTreeSet<String>,
    effective: &BTreeMap<String, String>,
    by_id: &BTreeMap<String, Value>,
    child_progress: &BTreeSet<String>,
) -> Option<String> {
    if get_str(e, "type") == Some("roadmap") {
        return None;
    }
    if truthy(e.get("completed_at")) {
        return Some("Done".to_string());
    }
    let status = get_str(e, "status");
    if status == Some("done") {
        return Some("Done".to_string());
    }
    if matches!(status, Some("deferred") | Some("superseded")) {
        return None;
    }
    if status == Some("in_progress") {
        return Some("In Progress".to_string());
    }
    if let Some(id) = entry_id(e) {
        if in_progress_epics.contains(id) {
            return Some("In Progress".to_string());
        }
    }
    if truthy(e.get("queued_at")) {
        return Some("Triage".to_string());
    }
    // make_kanban_column always binds the effective-priority projection, so
    // the row's own priority only answers for rows outside the id map.
    let priority = match entry_id(e).and_then(|id| effective.get(id)) {
        Some(p) => p.clone(),
        None => match live_epic_for(e, by_id, child_progress) {
            Some(epic) => {
                let child = priority_name(e);
                let epic_p = priority_name(&epic);
                if epic_p < child {
                    epic_p
                } else {
                    child
                }
            }
            None => priority_name(e),
        },
    };
    match priority.as_str() {
        "p0" | "p1" => Some("Now".to_string()),
        "p3" => Some("Later".to_string()),
        _ => Some("Next".to_string()),
    }
}

fn project_key(e: &Value) -> String {
    match get_str(e, "project") {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => "(unscoped)".to_string(),
    }
}

/// `_find_node`: exact id first, then a unique short `ab-` prefix; ambiguity
/// returns no match plus the stderr line python writes.
fn find_rank_node(entries: &[Value], node_id: &str) -> (Option<usize>, Option<String>) {
    if let Some(i) = entries.iter().position(|e| entry_id(e) == Some(node_id)) {
        return (Some(i), None);
    }
    if node_id.starts_with("ab-") && node_id.len() < 11 {
        let suffix = &node_id[3..];
        let partial = (4..=7).contains(&suffix.len())
            && suffix
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if partial {
            let hits: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| entry_id(e).map(|i| i.starts_with(node_id)).unwrap_or(false))
                .map(|(i, _)| i)
                .collect();
            if hits.len() == 1 {
                return (Some(hits[0]), None);
            }
            if hits.len() > 1 {
                let ids: Vec<String> = hits
                    .iter()
                    .map(|&i| entry_id(&entries[i]).unwrap_or("?").to_string())
                    .collect();
                return (
                    None,
                    Some(format!(
                        "[graph] ambiguous prefix '{node_id}' matches: {}",
                        ids.join(", ")
                    )),
                );
            }
        }
    }
    (None, None)
}

/// python float repr: a whole f64 prints `4.0`, not `4`.
fn format_rank(r: f64) -> String {
    if r.fract() == 0.0 {
        format!("{r:.1}")
    } else {
        format!("{r}")
    }
}

/// One performed rank: what the receipt line names.
enum Receipt {
    Clear {
        id: String,
        lane: String,
    },
    Pin {
        id: String,
        action: String,
        rank: f64,
        scope_label: String,
        epic: bool,
    },
}

/// The plan-projection seam: the plan-doc writer's keeper `plan_docs` project
/// op takes the ranked ids here once it lands on this branch.
fn project_plans(_ids: &[String]) {}

/// The truthful dispatcher note for a successfully ranked node.
fn dispatch_note(task_id: &str, graph: &Path) -> Option<String> {
    let read = || -> Result<Vec<Value>, String> {
        Ok(crate::backlog::read_entries(graph)?
            .into_iter()
            .filter(|r| r.get("archived_at").is_none())
            .collect())
    };
    match read().and_then(|entries| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let registry = AgentsHome::from_env().registry_json();
        let report = crate::active_backlog::resolve_targets_report(&cwd, &registry);
        note_from_receipt(task_id, &entries, &report)
    }) {
        Ok(note) => note,
        Err(exc) => Some(format!("dispatcher scope unavailable ({exc})")),
    }
}

/// The note's decision table, pure over the served rows and the drain
/// reading, so every branch stays unit-testable without an environment.
fn note_from_receipt(
    task_id: &str,
    entries: &[Value],
    report: &crate::active_backlog::DrainResolve,
) -> Result<Option<String>, String> {
    if let Some(failure) = &report.failure {
        // python's call_binary_json truncates the composed stderr line at
        // 200 chars; the inner failure arrives separately pre-truncated.
        let line: String = format!("active-backlog: {failure}")
            .chars()
            .take(200)
            .collect();
        return Err(line);
    }
    let mut missions: Vec<String> = Vec::new();
    for target in &report.targets {
        match &target.mission {
            None => continue,
            Some(m) if m.is_empty() => {
                return Err("active-backlog target has no readable mission".to_string())
            }
            Some(m) => missions.push(m.clone()),
        }
    }
    missions.sort();
    missions.dedup();
    if missions
        .iter()
        .any(|m| descendants_of(entries, m).contains(task_id))
    {
        return Ok(None);
    }
    // A switched-off drain is a config fact, not a mission fact: prescribe
    // the config fix, never the epic lever that cannot work.
    match report.skip_reason.as_deref() {
        Some("drain_disabled") => {
            return Ok(Some(format!(
                "no live dispatcher will take it (the drain is disabled in config; \
                 {} active missions); Enable it: fno config set active_backlog.enabled true",
                report.missions
            )))
        }
        Some("bad_interval") => {
            return Ok(Some(format!(
                "no live dispatcher will take it (the drain interval is invalid; \
                 {} active missions); Fix it: fno config set active_backlog.interval 5m",
                report.missions
            )))
        }
        _ => {}
    }
    // The remedy, not just the diagnosis: name the one command that makes a
    // dispatcher take the node. With no epic parent it says so, so the note
    // never prints a command that cannot work.
    let parent = entries
        .iter()
        .find(|e| entry_id(e) == Some(task_id))
        .and_then(|e| get_str(e, "parent"));
    let remedy = match parent {
        Some(p) => format!("; Activate its epic: fno backlog advance --epic {p}"),
        None => "; no epic to activate (missions are activated per epic with \
                 fno backlog advance --epic <epic-id>)"
            .to_string(),
    };
    let scope = if missions.is_empty() {
        "no resolved active missions".to_string()
    } else {
        format!("outside active mission scopes: {}", missions.join(", "))
    };
    Ok(Some(format!(
        "no live dispatcher will take it ({scope}){remedy}"
    )))
}

/// The run entry: returns the process exit code.
pub fn run(tail: &[String]) -> i32 {
    if tail.iter().any(|t| t == "--help" || t == "-h") {
        println!("{RANK_HELP}");
        return 0;
    }
    let Some(args) = RankArgs::parse(tail) else {
        eprintln!(
            "fno backlog rank: usage: fno backlog rank <task-id> [--top | --bottom | \
             --before <id> | --after <id> | --clear] [--within-epic] [--operator] \
             (--help for detail)"
        );
        return 2;
    };
    if !has_node_id_prefix(&args.task_id) {
        eprintln!(
            "Error: task_id must be a <prefix>-<4..8 hex> node id, got '{}'",
            args.task_id
        );
        return 1;
    }
    if !args.operator {
        if let Some(harness) = agent_harness_writing_rank() {
            eprintln!("{}", agent_rank_refusal(&args.task_id, &harness));
            return 1;
        }
    }
    let chosen: Vec<&str> = [
        ("--top", args.top),
        ("--bottom", args.bottom),
        ("--before", args.before.is_some()),
        ("--after", args.after.is_some()),
        ("--clear", args.clear),
    ]
    .into_iter()
    .filter(|(_, on)| *on)
    .map(|(name, _)| name)
    .collect();
    if chosen.len() != 1 {
        eprintln!(
            "Error: pass exactly one of --top / --bottom / --before <id> / --after <id> / --clear"
        );
        return 1;
    }
    let anchor_id = args.before.clone().or_else(|| args.after.clone());
    if let Some(anchor) = &anchor_id {
        if !has_node_id_prefix(anchor) {
            eprintln!("Error: anchor must be a <prefix>-<4..8 hex> node id, got '{anchor}'");
            return 1;
        }
    }

    let graph = settings::graph_path();
    let receipt: RefCell<Option<Receipt>> = RefCell::new(None);
    let stderr_lines: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let performed = crate::backlog::mutate_single_row(&graph, "rank", |rows| {
        let by_id: BTreeMap<String, Value> = rows
            .iter()
            .filter(|e| is_dict(e))
            .filter_map(|e| entry_id(e).map(|i| (i.to_string(), e.clone())))
            .collect();
        let child_progress = epics_with_child_progress(&by_id);
        let epic_in_progress =
            in_progress_epic_ids(rows, &by_id, &child_progress, &BTreeSet::new());
        let effective = make_effective_priority(&by_id, &child_progress);
        let epic_of = |e: &Value| -> Option<String> {
            live_epic_for(e, &by_id, &child_progress)
                .and_then(|ep| entry_id(&ep).map(str::to_string))
        };
        let column_for =
            |e: &Value| kanban_column(e, &epic_in_progress, &effective, &by_id, &child_progress);
        let lane = |e: &Value| -> (Option<String>, String) { (column_for(e), project_key(e)) };
        let lane_label = |e: &Value| -> String {
            let (col, proj) = lane(e);
            format!(
                "{}/{proj}",
                col.unwrap_or_else(|| "(off-board)".to_string())
            )
        };

        let (node_idx, ambiguity) = find_rank_node(rows, &args.task_id);
        if let Some(line) = ambiguity {
            stderr_lines.borrow_mut().push(line);
        }
        let Some(node_idx) = node_idx else {
            return Err(format!("Error: feature {} not found", args.task_id));
        };
        let tid = entry_id(&rows[node_idx])
            .unwrap_or(&args.task_id)
            .to_string();
        let epic_id = epic_of(&rows[node_idx]);
        if args.within_epic && epic_id.is_none() {
            return Err(format!(
                "Error: --within-epic refused: {tid} has no live epic parent. \
                 Child ranking needs a live epic; loose nodes and epic \
                 containers keep the (column, project) lane scope."
            ));
        }

        if args.clear {
            let lane_label = lane_label(&rows[node_idx]);
            rows[node_idx]
                .as_object_mut()
                .expect("row is an object")
                .insert("rank".to_string(), Value::Null);
            receipt.borrow_mut().replace(Receipt::Clear {
                id: tid,
                lane: lane_label,
            });
            return Ok(true);
        }

        // Peers exclude the target; ranked peers (anchor included) sorted
        // ascending give us the band to insert into.
        let mut ranked: Vec<f64> = Vec::new();
        match &epic_id {
            Some(epic) => {
                for e in rows.iter() {
                    if is_dict(e)
                        && entry_id(e) != Some(tid.as_str())
                        && epic_of(e).as_deref() == Some(epic.as_str())
                    {
                        let band = rank_band(e);
                        if band.0 == 0 {
                            ranked.push(band.1);
                        }
                    }
                }
            }
            None => {
                let target_lane = lane(&rows[node_idx]);
                for e in rows.iter() {
                    if is_dict(e) && entry_id(e) != Some(tid.as_str()) && lane(e) == target_lane {
                        let band = rank_band(e);
                        if band.0 == 0 {
                            ranked.push(band.1);
                        }
                    }
                }
            }
        }
        ranked.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let new_rank;
        let action;
        if args.top {
            new_rank = ranked.first().map(|r| r - 1.0).unwrap_or(0.0);
            action = "--top".to_string();
        } else if args.bottom {
            new_rank = ranked.last().map(|r| r + 1.0).unwrap_or(0.0);
            action = "--bottom".to_string();
        } else {
            let raw_anchor = anchor_id.as_deref().unwrap_or_default();
            let (anchor_idx, ambiguity) = find_rank_node(rows, raw_anchor);
            if let Some(line) = ambiguity {
                stderr_lines.borrow_mut().push(line);
            }
            let Some(anchor_idx) = anchor_idx else {
                return Err(format!("Error: anchor {raw_anchor} not found"));
            };
            if entry_id(&rows[anchor_idx]) == Some(tid.as_str()) {
                return Err("Error: cannot rank a node relative to itself".to_string());
            }
            if epic_id.is_some() {
                let anchor_epic = epic_of(&rows[anchor_idx]);
                if anchor_epic.as_deref() != epic_id.as_deref() {
                    let described = match &anchor_epic {
                        None => "loose".to_string(),
                        Some(ae) => format!("a child of {ae}"),
                    };
                    return Err(format!(
                        "Error: cross-epic rank rejected: {} is a child of \
                         {} but anchor {raw_anchor} is {described}. \
                         Child rank is scoped to its live epic.",
                        args.task_id,
                        epic_id.as_deref().unwrap_or_default()
                    ));
                }
            } else {
                let target_lane = lane(&rows[node_idx]);
                if lane(&rows[anchor_idx]) != target_lane {
                    return Err(format!(
                        "Error: cross-lane rank rejected: {} is in {} but anchor \
                         {raw_anchor} is in {}. Rank is scoped per (column, project) lane.",
                        args.task_id,
                        lane_label(&rows[node_idx]),
                        lane_label(&rows[anchor_idx]),
                    ));
                }
            }
            let anchor_band = rank_band(&rows[anchor_idx]);
            if anchor_band.0 != 0 {
                return Err(format!(
                    "Error: anchor {raw_anchor} is unranked; rank it first \
                     (e.g. `fno backlog rank {raw_anchor} --top`) or use --top/--bottom."
                ));
            }
            let anchor_rank = anchor_band.1;
            if args.before.is_some() {
                let lo = ranked.iter().filter(|r| **r < anchor_rank).cloned().fold(
                    None::<f64>,
                    |acc, r| {
                        Some(match acc {
                            Some(prev) if prev > r => prev,
                            _ => r,
                        })
                    },
                );
                new_rank = match lo {
                    None => anchor_rank - 1.0,
                    Some(lo) => (lo + anchor_rank) / 2.0,
                };
                action = format!("--before {raw_anchor}");
            } else {
                let hi = ranked.iter().filter(|r| **r > anchor_rank).cloned().fold(
                    None::<f64>,
                    |acc, r| {
                        Some(match acc {
                            Some(prev) if prev < r => prev,
                            _ => r,
                        })
                    },
                );
                new_rank = match hi {
                    None => anchor_rank + 1.0,
                    Some(hi) => (anchor_rank + hi) / 2.0,
                };
                action = format!("--after {raw_anchor}");
            }
        }

        rows[node_idx]
            .as_object_mut()
            .expect("row is an object")
            .insert("rank".to_string(), json!(new_rank));
        receipt.borrow_mut().replace(Receipt::Pin {
            id: tid.clone(),
            action,
            rank: new_rank,
            epic: epic_id.is_some(),
            scope_label: match &epic_id {
                Some(epic) => format!("epic {epic}"),
                None => format!("lane {}", lane_label(&rows[node_idx])),
            },
        });
        let _ = &tid;
        Ok(true)
    });

    match performed {
        Err(line) => {
            for l in stderr_lines.borrow().iter() {
                eprintln!("{l}");
            }
            eprintln!("{line}");
            1
        }
        Ok(false) => 0,
        Ok(true) => {
            let r = receipt.into_inner().expect("the mutator stores a receipt");
            match r {
                Receipt::Clear { id, lane } => {
                    println!("Cleared rank on {id} (rejoined the unranked flow in {lane})");
                    project_plans(&[id]);
                }
                Receipt::Pin {
                    id,
                    action,
                    rank,
                    scope_label,
                    epic,
                } => {
                    let note = dispatch_note(&id, &graph);
                    let suffix = note.as_ref().map(|n| format!("; {n}")).unwrap_or_default();
                    // A bare "Ranked --top" read as "runs next across the project" and
                    // meant "top of its own epic". Name what the pin is top OF.
                    let scope_note = if epic {
                        "orders it among that epic's children only, and the epic's own \
                         rank decides where the group runs"
                    } else {
                        "orders it within that board lane only"
                    };
                    println!(
                        "Ranked {id} {action} of {scope_label} (rank={}); {scope_note}{suffix}",
                        format_rank(rank)
                    );
                    project_plans(&[id]);
                }
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::active_backlog::DrainResolve;
    use serde_json::json;

    fn wellformed_cases() -> Vec<(&'static str, bool)> {
        vec![
            ("x-aaaa1111", true),
            ("ab-12345678", true),
            ("ab-1234", true),
            ("x-a123", true),
            ("abcdefgh-1234", true), // prefix may run 8 chars
            ("1abc-1234", false),    // prefix not letter-led
            ("x-nope", false),       // suffix not hex
            ("ab-123", false),       // suffix shorter than 4
            ("ab-123456789", false), // suffix longer than 8
            ("x-ABCDEF12", false),   // uppercase hex
        ]
    }

    #[test]
    fn the_wellformed_id_gate_matches_python() {
        for (input, expected) in wellformed_cases() {
            assert_eq!(is_wellformed_node_id(input), expected, "{input}");
        }
    }

    #[test]
    fn the_liberal_gate_admits_prefix_carrying_non_hex_ids() {
        // The configured/legacy prefix admits the non-hex test ids that
        // resolve by exact graph lookup.
        assert!(has_node_id_prefix("ab-test-1"));
        assert!(!has_node_id_prefix("nope"));
        assert!(has_node_id_prefix("x-aaaa1111"));
    }

    #[test]
    fn whole_floats_print_python_style() {
        assert_eq!(format_rank(4.0), "4.0");
        assert_eq!(format_rank(-1.0), "-1.0");
        assert_eq!(format_rank(4.5), "4.5");
        assert_eq!(format_rank(4.25), "4.25");
    }

    #[test]
    fn the_column_ladder_routes_like_render() {
        let by_id = BTreeMap::new();
        let empty = BTreeSet::new();
        let effective = BTreeMap::new();
        let col =
            |e: &Value| kanban_column(e, &empty, &effective, &by_id, &empty).unwrap_or_default();
        assert_eq!(col(&json!({"type": "roadmap", "priority": "p1"})), "");
        assert_eq!(col(&json!({"completed_at": "2026-01-01"})), "Done");
        assert_eq!(col(&json!({"status": "done"})), "Done");
        assert_eq!(col(&json!({"status": "deferred"})), "");
        assert_eq!(col(&json!({"status": "superseded"})), "");
        assert_eq!(col(&json!({"status": "in_progress"})), "In Progress");
        assert_eq!(col(&json!({"queued_at": "2026-01-01"})), "Triage");
        assert_eq!(col(&json!({"priority": "p0"})), "Now");
        assert_eq!(col(&json!({})), "Next");
        assert_eq!(col(&json!({"priority": "p3"})), "Later");
    }

    #[test]
    fn a_promoted_child_shows_in_its_epics_column() {
        let epic = json!({"id": "x-eeee5555", "type": "epic", "status": "ready",
                          "priority": "p1", "created_at": "2026-01-01"});
        let child = json!({"id": "x-77778888", "status": "ready", "priority": "p3",
                           "parent": "x-eeee5555"});
        let by_id = BTreeMap::from([
            ("x-eeee5555".to_string(), epic.clone()),
            ("x-77778888".to_string(), child.clone()),
        ]);
        let child_progress = epics_with_child_progress(&by_id);
        let effective = make_effective_priority(&by_id, &child_progress);
        let in_progress = in_progress_epic_ids(
            &[epic.clone(), child.clone()],
            &by_id,
            &child_progress,
            &BTreeSet::new(),
        );
        let col = kanban_column(&child, &in_progress, &effective, &by_id, &child_progress);
        // p3 child promoted to its p1 epic: Now, not Later.
        assert_eq!(col, Some("Now".to_string()));
    }

    #[test]
    fn project_key_keeps_unstripped_names_and_degrades_blank() {
        assert_eq!(project_key(&json!({"project": "fno"})), "fno");
        assert_eq!(project_key(&json!({"project": "  "})), "(unscoped)");
        assert_eq!(project_key(&json!({})), "(unscoped)");
    }

    #[test]
    fn find_rank_node_ambiguity_names_the_candidates() {
        let entries = vec![json!({"id": "ab-12345678"}), json!({"id": "ab-1234abcd"})];
        let (hit, line) = find_rank_node(&entries, "ab-1234");
        assert!(hit.is_none());
        assert_eq!(
            line,
            Some(
                "[graph] ambiguous prefix 'ab-1234' matches: ab-12345678, ab-1234abcd".to_string()
            )
        );
        let (hit, line) = find_rank_node(&entries, "ab-12345678");
        assert_eq!(hit, Some(0));
        assert!(line.is_none());
    }

    fn receipt(targets: Value, missions: u64, skip_reason: Option<&str>) -> DrainResolve {
        DrainResolve {
            targets: serde_json::from_value(targets).expect("valid target rows"),
            missions,
            skip_reason: skip_reason.map(str::to_string),
            failure: None,
        }
    }

    /// The required ResolvedTarget fields a test row must carry.
    fn mission_row(mission: &str) -> Value {
        json!({
            "project": "fno",
            "cwd": "/repo",
            "interval_seconds": 300,
            "failure_limit": 5,
            "mission": mission,
        })
    }

    fn outside_node() -> Value {
        json!({"id": "x-0bad", "title": "Outside", "status": "ready", "priority": "p1"})
    }

    #[test]
    fn a_mission_reaching_the_node_prints_no_note() {
        let report = receipt(json!([mission_row("x-beef")]), 1, None);
        let entries = vec![
            json!({"id": "x-beef", "type": "epic", "status": "in_progress", "parent": null}),
            json!({"id": "x-0cab", "parent": "x-beef", "status": "ready"}),
        ];
        let note = note_from_receipt("x-0cab", &entries, &report).unwrap();
        assert_eq!(note, None);
    }

    #[test]
    fn an_outside_loose_node_gets_the_epic_activation_remedy() {
        let report = receipt(json!([mission_row("x-beef")]), 1, None);
        let note = note_from_receipt("x-0bad", &[outside_node()], &report)
            .unwrap()
            .unwrap();
        assert!(note.contains("no live dispatcher will take it"));
        // The remedy, or the honest absence of one: with no epic parent the
        // note names the per-epic activation axis and prints no command that
        // cannot work.
        assert!(note.contains("no epic to activate"));
        assert!(!note.contains("advance --epic x-0bad"));
    }

    #[test]
    fn an_outside_child_names_its_epic_activation_command() {
        let report = receipt(json!([mission_row("x-beef")]), 1, None);
        let child = json!({"id": "x-0dad", "parent": "x-feed", "status": "ready"});
        let note = note_from_receipt("x-0dad", &[child], &report)
            .unwrap()
            .unwrap();
        assert!(note.contains("Activate its epic: fno backlog advance --epic x-feed"));
    }

    #[test]
    fn a_disabled_drain_prescribes_the_config_fix() {
        let report = receipt(json!([]), 6, Some("drain_disabled"));
        let note = note_from_receipt("x-0eef", &[outside_node()], &report)
            .unwrap()
            .unwrap();
        assert!(note.contains("the drain is disabled in config"));
        assert!(note.contains("6 active missions"));
        assert!(note.contains("fno config set active_backlog.enabled true"));
        assert!(!note.contains("advance --epic"));
    }

    #[test]
    fn an_unreadable_drain_wraps_the_truncated_failure_line() {
        let report = DrainResolve {
            targets: Vec::new(),
            missions: 0,
            skip_reason: None,
            failure: Some("territory: graph unreadable".to_string()),
        };
        let exc = note_from_receipt("x-0bad", &[outside_node()], &report).unwrap_err();
        assert_eq!(exc, "active-backlog: territory: graph unreadable");
        // The caller wraps any inner failure in the unavailable note.
        let note = match note_from_receipt("x-0bad", &[outside_node()], &report) {
            Err(exc) => Some(format!("dispatcher scope unavailable ({exc})")),
            Ok(note) => note,
        };
        assert!(note.unwrap().contains("dispatcher scope unavailable"));
    }

    #[test]
    fn mission_lists_dedupe_and_sort() {
        let report = receipt(json!([mission_row("z-c"), mission_row("a-a")]), 2, None);
        let note = note_from_receipt("x-0bad", &[outside_node()], &report)
            .unwrap()
            .unwrap();
        assert!(note.contains("outside active mission scopes: a-a, z-c"));
    }
}
