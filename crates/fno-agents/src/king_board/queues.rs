//! The operator lane parser and the eleven-queue board build (pure; no I/O).
use super::classify::node_driver;
use super::prs::derived_status;
use super::scope::operator_lane_path;
use super::{
    as_int, s_str, SourceRead, DEAD_CLAIM_STATES, KING_PRIORITIES, LEGACY_DEFER_PREFIX, SRC_CLAIMS,
    SRC_NEEDS, SRC_PRS, SRC_PR_NODES, SRC_QUESTIONS, SRC_READY, SRC_UNDISPATCHED, TERMINAL_RUNGS,
};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Per-project rows rendered for the capture stream; the count stays whole.
pub(crate) const CAPTURE_PROJECT_CAP: usize = 8;

pub(crate) const NODE_ID_BODY: &str = "[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}";

// ---------------------------------------------------------------------------
// Lane: the operator's own ranked file (king/lane.py)
// ---------------------------------------------------------------------------

pub(crate) struct LaneItem {
    pub(crate) text: String,
    pub(crate) node: Option<String>,
    pub(crate) parked: Option<String>,
    pub(crate) done: bool,
    pub(crate) line: usize,
}

pub(crate) fn parse_lane(path: &Path) -> Result<Vec<LaneItem>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read operator lane {}: {e}", path.display())),
    };
    let item_re = regex::Regex::new(r"^- \[( |x|X)\] (.*)$").expect("static regex");
    let body = "[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}";
    let suffix_re = regex::Regex::new(&format!(
        r"->\s*(?:(?P<node>{body})|parked:\s*(?P<reason>\S.*?))\s*$"
    ))
    .expect("static regex");
    let mut items = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let Some(caps) = item_re.captures(raw) else {
            continue;
        };
        let done = &caps[1] != " ";
        let rest = caps[2].to_string();
        let (mut node, mut parked, mut text_out) = (None, None, rest.clone());
        if let Some(sc) = suffix_re.captures(&rest) {
            node = sc.name("node").map(|m| m.as_str().to_string());
            parked = sc.name("reason").map(|m| m.as_str().to_string());
            text_out = rest[..sc.get(0).unwrap().start()].trim_end().to_string();
        }
        items.push(LaneItem {
            text: text_out.trim().to_string(),
            node,
            parked,
            done,
            line: i + 1,
        });
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Board construction: the eleven queues
// ---------------------------------------------------------------------------

pub(crate) struct Queue {
    pub(crate) name: &'static str,
    pub(crate) source: String,
    pub(crate) status: &'static str,
    pub(crate) error: String,
    pub(crate) count: i64,
    pub(crate) rows: Vec<Value>,
    pub(crate) actionable: bool,
    pub(crate) note: String,
    pub(crate) verb: &'static str,
}

pub(crate) fn queue(
    name: &'static str,
    source: String,
    read: &SourceRead,
    rows: Vec<Value>,
    actionable: bool,
    note: String,
    verb: &'static str,
    count: Option<i64>,
) -> Queue {
    if !read.is_ok() {
        return Queue {
            name,
            source,
            status: "unreadable",
            error: read.error.clone().unwrap_or_default(),
            count: -1,
            rows: Vec::new(),
            actionable,
            note,
            verb,
        };
    }
    Queue {
        name,
        source,
        status: "ok",
        error: String::new(),
        count: count.unwrap_or(rows.len() as i64),
        rows,
        actionable,
        note,
        verb,
    }
}

pub(crate) fn queue_json(q: &Queue) -> Value {
    json!({
        "name": q.name,
        "source": q.source,
        "status": q.status,
        "error": q.error,
        "count": if q.status == "unreadable" { Value::Null } else { json!(q.count) },
        "rows": q.rows,
        "actionable": q.actionable,
        "note": q.note,
        "verb": q.verb,
    })
}

/// All the board's fetched sources, ready for the pure build.
pub(crate) struct BoardInputs {
    pub(crate) ready: SourceRead,
    pub(crate) claims: SourceRead,
    pub(crate) claimed_nodes: SourceRead,
    pub(crate) holder_activity: HashMap<String, crate::truth_probe::TruthProbe>,
    pub(crate) prs: SourceRead,
    pub(crate) pr_nodes: SourceRead,
    pub(crate) outstanding: SourceRead,
    pub(crate) needs: SourceRead,
    pub(crate) lane: SourceRead,
    pub(crate) undispatched: SourceRead,
    pub(crate) warnings: Vec<String>,
    pub(crate) autonomous_merge: bool,
    pub(crate) scope_ids: Option<HashSet<String>>,
    pub(crate) crown_scope: Option<String>,
}

/// Build the board payload. Pure; does no I/O. Queue names, order, and row
/// shapes match board.py's `build_board` exactly.
pub(crate) fn build_board(inputs: &BoardInputs) -> Value {
    let warnings = inputs.warnings.clone();
    let mut out_of_scope: Vec<Value> = Vec::new();
    let scope_ids = inputs.scope_ids.as_ref();

    let in_scope = |queue: &str, node_id: &Value, row: &Value, out: &mut Vec<Value>| -> bool {
        let Some(ids) = scope_ids else {
            return true;
        };
        let Some(id) = node_id.as_str() else {
            return true;
        };
        if ids.contains(id) {
            return true;
        }
        let mut extra = Map::new();
        extra.insert("queue".to_string(), json!(queue));
        extra.insert("id".to_string(), json!(id));
        if let Some(title) = row.get("title").filter(|t| !t.is_null()) {
            extra.insert("title".to_string(), title.clone());
        }
        out.push(Value::Object(extra));
        false
    };

    let claim_rows = inputs.claims.rows();
    let mut claim_by_node: HashMap<String, Value> = HashMap::new();
    for row in &claim_rows {
        if let Some(key) = s_str(row, "key") {
            if let Some(node_id) = key.strip_prefix("node:") {
                claim_by_node.insert(node_id.to_string(), row.clone());
            }
        }
    }

    // Undispatched: planned work with no claim, king priorities only.
    let undispatched_rows = if inputs.undispatched.is_ok() {
        inputs
            .undispatched
            .rows()
            .into_iter()
            .filter(|node| KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")))
            .filter(|node| {
                in_scope(
                    "undispatched",
                    node.get("id").unwrap_or(&Value::Null),
                    node,
                    &mut out_of_scope,
                )
            })
            .map(|node| {
                json!({
                    "id": node.get("id"),
                    "priority": node.get("priority"),
                    "title": node.get("title"),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // Unplanned: cold-dispatchable ideas off the ready list.
    let unplanned_rows: Vec<Value> = inputs
        .ready
        .rows()
        .into_iter()
        .filter(|node| KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")))
        .filter(|node| {
            node.get("plan_path")
                .map(|p| p.is_null() || p.as_str().map(|s| s.is_empty()).unwrap_or(false))
                .unwrap_or(true)
        })
        .filter(|node| {
            let dead = claim_by_node
                .get(s_str(node, "id").unwrap_or(""))
                .map(|c| DEAD_CLAIM_STATES.contains(&s_str(c, "state").unwrap_or("")))
                .unwrap_or(false);
            !dead
        })
        .filter(|node| {
            in_scope(
                "unplanned",
                node.get("id").unwrap_or(&Value::Null),
                node,
                &mut out_of_scope,
            )
        })
        .map(|node| {
            json!({
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
            })
        })
        .collect();

    // Stalled holder: starts from the CLAIM, never the ready list (a live
    // holder is exactly what `ready` has already removed).
    let mut stalled_rows: Vec<Value> = Vec::new();
    for node in &inputs.claimed_nodes.rows() {
        if !KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")) {
            continue;
        }
        if s_str(node, "status")
            .map(|s| TERMINAL_RUNGS.contains(&s))
            .unwrap_or(false)
        {
            continue;
        }
        let (state, claim) = node_driver(
            s_str(node, "id").unwrap_or(""),
            &claim_by_node,
            &inputs.holder_activity,
        );
        if state != "stalled" {
            continue;
        }
        let claim = claim.expect("stalled always carries its claim");
        if !in_scope(
            "stalled_holder",
            node.get("id").unwrap_or(&Value::Null),
            &node,
            &mut out_of_scope,
        ) {
            continue;
        }
        stalled_rows.push(json!({
            "id": node.get("id"),
            "priority": node.get("priority"),
            "title": node.get("title"),
            "holder": claim.get("holder"),
            "claim_state": claim.get("state"),
        }));
    }

    // Stale claims: locks nobody will reap.
    let stale_claim_rows: Vec<Value> = claim_rows
        .iter()
        .filter(|row| DEAD_CLAIM_STATES.contains(&s_str(row, "state").unwrap_or("")))
        .filter(|row| {
            let node_id = s_str(row, "key")
                .and_then(|k| k.strip_prefix("node:"))
                .unwrap_or("");
            in_scope("stale_claim", &json!(node_id), row, &mut out_of_scope)
        })
        .map(|row| {
            json!({
                "key": row.get("key"),
                "holder": row.get("holder"),
                "state": row.get("state"),
            })
        })
        .collect();

    // Operator lane.
    let lane_ok = inputs.lane.is_ok();
    let lane_items: Vec<LaneItem> = if lane_ok {
        inputs
            .lane
            .rows()
            .iter()
            .map(|r| LaneItem {
                text: s_str(r, "text").unwrap_or("").to_string(),
                node: r.get("node").and_then(Value::as_str).map(str::to_string),
                parked: r.get("parked").and_then(Value::as_str).map(str::to_string),
                done: r.get("done").and_then(Value::as_bool).unwrap_or(false),
                line: r.get("line").and_then(Value::as_u64).unwrap_or(0) as usize,
            })
            .collect()
    } else {
        Vec::new()
    };
    let lane_open: Vec<&LaneItem> = lane_items
        .iter()
        .filter(|i| !i.done && i.node.is_none() && i.parked.is_none())
        .collect();
    let parked_count = lane_items.iter().filter(|i| i.parked.is_some()).count();
    let mut lane_note = "the operator's own ranking. File each with `fno backlog idea \"<text>\"` and stamp `-> <id>` onto its line, or park it with `-> parked: <reason>`.".to_string();
    if parked_count > 0 {
        lane_note.push_str(&format!(" {parked_count} parked, reasons are in the file."));
    }
    let scoped = scope_ids.is_some();
    if scoped {
        lane_note.push_str(" report-only under a crown: lane lines are the operator's global priorities and carry no node id, so a scoped king cannot attribute them to its subtree");
    }
    let lane_rows: Vec<Value> = if lane_ok {
        lane_open
            .iter()
            .map(|i| json!({"text": i.text, "line": i.line}))
            .collect()
    } else {
        Vec::new()
    };

    let pr_rows: Vec<Value> = inputs
        .prs
        .rows()
        .iter()
        .map(|r| json!({"number": r.get("number"), "title": r.get("title")}))
        .collect();

    // Undriven PR: the complement of stalled_holder, the second half of ONE
    // predicate. Fail CLOSED on an unreadable claim list: every node would
    // read "none" and the king would dispatch over every live worker at once.
    let mergeable_numbers: HashSet<i64> = if inputs.autonomous_merge {
        pr_rows
            .iter()
            .filter_map(|r| r.get("number").and_then(Value::as_i64))
            .collect()
    } else {
        HashSet::new()
    };
    let mut undriven_rows: Vec<Value> = Vec::new();
    if inputs.pr_nodes.is_ok() && inputs.claims.is_ok() {
        for node in &inputs.pr_nodes.rows() {
            if !KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")) {
                continue;
            }
            let terminal = s_str(node, "status")
                .map(|s| TERMINAL_RUNGS.contains(&s))
                .unwrap_or(false)
                || node.get("superseded_by").is_some_and(|v| !v.is_null())
                || node
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .map(|c| !c.is_empty() && !c.starts_with(LEGACY_DEFER_PREFIX))
                    .unwrap_or(false);
            if terminal {
                continue;
            }
            let status = derived_status(node);
            if status == "deferred" || status == "blocked" {
                continue;
            }
            let (state, _claim) = node_driver(
                s_str(node, "id").unwrap_or(""),
                &claim_by_node,
                &inputs.holder_activity,
            );
            if state != "none" {
                continue;
            }
            let pr_number = node.get("pr_number").and_then(Value::as_i64);
            if let Some(n) = pr_number {
                if mergeable_numbers.contains(&n) {
                    continue;
                }
            }
            if !in_scope(
                "undriven_pr",
                node.get("id").unwrap_or(&Value::Null),
                &node,
                &mut out_of_scope,
            ) {
                continue;
            }
            undriven_rows.push(json!({
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
                "status": status,
                "pr_number": node.get("pr_number"),
                "pr_url": node.get("pr_url"),
            }));
        }
    }

    // One outstanding read, three streams.
    let outstanding = inputs.outstanding.dict();
    let question_rows: Vec<Value> = outstanding
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .map(|r| {
                    json!({"id": r.get("id"), "question": r.get("question"), "ts": r.get("ts")})
                })
                .collect()
        })
        .unwrap_or_default();

    let carveout_stream = outstanding
        .get("carveouts")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let mut carveout_by_kind: Vec<(String, i64)> = carveout_stream
        .get("by_kind")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), as_int(v))).collect())
        .unwrap_or_default();
    carveout_by_kind.sort();
    let carveout_rows: Vec<Value> = carveout_by_kind
        .into_iter()
        .map(|(kind, n)| json!({"kind": kind, "n": n}))
        .collect();
    let carveout_root = outstanding
        .get("roots")
        .and_then(|r| r.get("carveouts"))
        .and_then(|c| c.get("root"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let capture_stream = outstanding
        .get("captures")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let capture_by_project: Vec<(String, i64)> = capture_stream
        .get("by_project")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), as_int(v))).collect())
        .unwrap_or_default();
    let total_projects = capture_by_project.len();
    let mut capture_sorted = capture_by_project;
    capture_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut capture_rows: Vec<Value> = capture_sorted
        .iter()
        .take(CAPTURE_PROJECT_CAP)
        .map(|(project, n)| json!({"project": project, "n": n}))
        .collect();
    let elided = total_projects.saturating_sub(capture_rows.len());
    if elided > 0 {
        capture_rows.push(json!({"elided_projects": elided}));
    }

    // `fno agents needs` emits operator questions in the same list; the queue
    // above already carries them, so this one drops the kind.
    let needs_rows: Vec<Value> = inputs
        .needs
        .rows()
        .iter()
        .filter(|row| s_str(row, "kind") != Some("operator_question"))
        .filter(|row| {
            in_scope(
                "unreachable_worker",
                row.get("node").unwrap_or(&Value::Null),
                row,
                &mut out_of_scope,
            )
        })
        .map(|row| {
            json!({"kind": row.get("kind"), "name": row.get("name"), "node": row.get("node")})
        })
        .collect();

    let lane_source = format!("cat {}", operator_lane_path(Path::new(".")).display());

    let mut queues = vec![
        queue(
            "operator_lane",
            lane_source,
            &inputs.lane,
            lane_rows,
            !scoped,
            lane_note,
            "",
            None,
        ),
        queue(
            "undispatched",
            format!("{SRC_UNDISPATCHED} + {SRC_CLAIMS}"),
            &if inputs.undispatched.is_ok() && inputs.claims.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    SourceRead {
                        error: inputs.undispatched.error.clone(),
                        ..Default::default()
                    }
                    .error
                    .or_else(|| inputs.claims.error.clone())
                    .unwrap_or_default(),
                )
            },
            undispatched_rows,
            true,
            "one worker per node; these already carry a plan".to_string(),
            "/fno:target",
            None,
        ),
        queue(
            "unplanned",
            format!("{SRC_READY} + {SRC_CLAIMS}"),
            &if inputs.ready.is_ok() && inputs.claims.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .ready
                        .error
                        .clone()
                        .or_else(|| inputs.claims.error.clone())
                        .unwrap_or_default(),
                )
            },
            unplanned_rows,
            true,
            "batch: up to 3 blueprints per session; merge same-shape nodes into one waved plan".to_string(),
            "/fno:blueprint",
            None,
        ),
        queue(
            "stalled_holder",
            format!("{SRC_CLAIMS} + fno backlog get <id> + fno agents peek <holder>"),
            &if inputs.claims.is_ok() && inputs.claimed_nodes.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .claims
                        .error
                        .clone()
                        .or_else(|| inputs.claimed_nodes.error.clone())
                        .unwrap_or_default(),
                )
            },
            stalled_rows,
            true,
            String::new(),
            "",
            None,
        ),
        queue(
            "undriven_pr",
            SRC_PR_NODES.to_string(),
            &if inputs.pr_nodes.is_ok() && inputs.claims.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .pr_nodes
                        .error
                        .clone()
                        .or_else(|| inputs.claims.error.clone())
                        .unwrap_or_default(),
                )
            },
            undriven_rows,
            true,
            "an open PR with nobody driving it; report only, never close or defer one - that judgment is the operator's".to_string(),
            "/fno:target",
            None,
        ),
        queue(
            "mergeable_pr",
            SRC_PRS.to_string(),
            &inputs.prs,
            pr_rows,
            inputs.autonomous_merge,
            if inputs.autonomous_merge {
                String::new()
            } else {
                "report-only: merging is outward and hard to reverse, so it waits on config.king.autonomous_merge".to_string()
            },
            "",
            None,
        ),
        queue(
            "stale_claim",
            SRC_CLAIMS.to_string(),
            &inputs.claims,
            stale_claim_rows,
            true,
            String::new(),
            "",
            None,
        ),
        queue(
            "operator_question",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            question_rows,
            false,
            "report-only: a human answers these, so counting them would hold the loop open forever".to_string(),
            "",
            None,
        ),
        queue(
            "carveout_pending",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            carveout_rows,
            false,
            if carveout_root.is_empty() {
                "report-only: the sweep is a human verb".to_string()
            } else {
                format!("report-only: the sweep is a human verb; root {carveout_root}")
            },
            "",
            Some(as_int(carveout_stream.get("total").unwrap_or(&Value::Null))),
        ),
        queue(
            "capture_pending",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            capture_rows,
            false,
            "report-only: per-project counts only; the rows cannot be listed".to_string(),
            "",
            Some(as_int(capture_stream.get("total").unwrap_or(&Value::Null))),
        ),
        queue(
            "unreachable_worker",
            SRC_NEEDS.to_string(),
            &inputs.needs,
            needs_rows,
            false,
            "report-only: the refusal event a king would act on does not exist yet".to_string(),
            "",
            None,
        ),
    ];

    if let Some(scope) = &inputs.crown_scope {
        if scope_ids.is_some() {
            queues.push(queue(
                "out_of_scope",
                format!("king manifest scope {scope}"),
                &SourceRead::ok(Value::Array(out_of_scope.clone())),
                out_of_scope.clone(),
                false,
                format!("report-only: outside crown scope {scope}"),
                "",
                None,
            ));
        }
    }

    let mut actionable: i64 = 0;
    let mut unreadable: i64 = 0;
    for q in &queues {
        if q.status == "unreadable" {
            unreadable += 1;
            // A blind ACTIONABLE queue is work: the king may not exit while it
            // cannot see a queue it could have shrunk. A blind report-only
            // queue is loud (the exit code) and still uncounted.
            if q.actionable {
                actionable += 1;
            }
        } else if q.actionable {
            actionable += q.count;
        }
    }

    let queues_json: Vec<Value> = queues.iter().map(queue_json).collect();
    json!({
        "actionable": actionable,
        "unreadable": unreadable,
        "queues": queues_json,
        "warnings": warnings,
        "exit_code": if unreadable > 0 { 1 } else { 0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lane_parser_carries_node_and_parked_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("my-priorities.md");
        std::fs::write(
            &lane,
            "- [ ] ship the board -> x-25b8\n- [ ] park me -> parked: waiting\n- [x] done item\n- [ ] open item\nnot an item\n",
        )
        .unwrap();
        let items = parse_lane(&lane).unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].node.as_deref(), Some("x-25b8"));
        assert_eq!(items[0].text, "ship the board");
        assert_eq!(items[1].parked.as_deref(), Some("waiting"));
        assert!(items[2].done);
        assert!(items[3].node.is_none() && items[3].parked.is_none() && !items[3].done);
    }

    #[test]
    fn a_missing_lane_file_is_an_empty_lane_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let items = parse_lane(&dir.path().join("absent.md")).unwrap();
        assert!(items.is_empty());
    }
}
