//! The check-in's blueprint reading: which unplanned nodes to blueprint and
//! which to route straight to target. Extracted from king_checkin.rs under
//! the file budget's shrink rule; the beat's print block reads the JSON this
//! module returns. Each unplanned row routes through the lifecycle verb
//! table (`dispatch.blueprint_floor`), so the beat's starts list can never
//! disagree with the routing the dispatch doors answer.
use serde_json::{json, Value};

use std::path::Path;

/// The default blueprint-subagent ceiling when no provider budget applies
/// (no new config key: a registry row is barred, so the default is a named
/// Rust constant). One per king: law d-6eb2cbbd.
const DEFAULT_BLUEPRINT_CEILING: usize = 1;

/// The check-in's blueprint reading: this session's live blueprint
/// subagents against the ceiling, then which unplanned nodes to start and
/// which to skip. Each unplanned row is routed through the lifecycle verb
/// table first: a row whose verb resolves to /blueprint is a candidate, one
/// that resolves to /target prints as target-ready (no blueprint ceiling
/// applies), and an undecidable row skips with the refusal. `floor` is the
/// operator's `dispatch.blueprint_floor`; the claim list and the session id
/// arrive through the seam (arguments, not ambient reads) so the unit tests
/// need no claims directory. A failed source is this reading's error, never
/// a zero: an unreadable claim list read as `running 0` would name starts
/// past the ceiling. Starts also wait until plans ready fall below the
/// king's worker slots - a blueprint nobody can build is the exact spend the
/// wake meter exists to name.
pub(crate) fn r_blueprint(
    board: &Result<Value, String>,
    cwd: &Path,
    session_id: Option<String>,
    claims: Result<Vec<String>, String>,
    slots: Result<usize, String>,
    floor: &str,
) -> Result<Value, String> {
    let session_id = session_id
        .ok_or_else(|| "no session id; cannot count this king's blueprint subagents".to_string())?;
    let holders = claims?;
    let holder = format!("blueprint-session:{session_id}");
    let running = holders.iter().filter(|h| **h == holder).count();
    let board = board.as_ref()?;
    let rows = crate::king_checkin::board_queue(board, "unplanned")?
        .get("rows")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();
    let mut target_ready: Vec<String> = Vec::new();
    let mut skips: Vec<Value> = Vec::new();
    for row in &rows {
        // A row with no string id answers no verb: skip it, never dispatch
        // an "unknown" node the way a silent default would invite.
        let id = match row.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                skips.push(json!({"id": null, "reason": "row carries no id"}));
                continue;
            }
        };
        match crate::backlog_ready::effective_verb_with_floor(row, floor) {
            Ok((verb, _)) if verb.as_deref() == Some("/blueprint") => candidates.push(id),
            Ok((verb, _)) if verb.as_deref() == Some("/target") => target_ready.push(id),
            Ok((_, note)) => skips.push(json!({"id": id, "reason": note})),
            Err(refusal) => skips.push(json!({"id": id, "reason": refusal})),
        }
    }
    let provider = std::env::var("FNO_ROUTE_PROVIDER").unwrap_or_default();
    let provider_cap = crate::spawn_gate_lanes::provider_subagents_cap(cwd, &provider);
    // A provider budget can only lower the one-per-king ceiling, never raise it.
    let (ceiling, ceiling_source) = match provider_cap {
        Some(cap) if cap < DEFAULT_BLUEPRINT_CEILING => {
            (cap, format!("agents.provider_limits.{provider}.subagents"))
        }
        _ => (DEFAULT_BLUEPRINT_CEILING, "one per king".to_string()),
    };
    let plans_ready = crate::king_checkin::board_queue(board, "undispatched")
        .ok()
        .and_then(|q| q.get("count").and_then(Value::as_u64).map(|c| c as usize));
    let (pr_v, slots_v, gate_reason): (Value, Value, Option<String>) = match (plans_ready, &slots) {
        (Some(pr), Ok(s)) => {
            if pr < *s {
                (json!(pr), json!(s), None)
            } else {
                (
                    json!(pr),
                    json!(s),
                    Some(format!(
                        "plans ready {pr} / slots {s}: build slots are the bottleneck"
                    )),
                )
            }
        }
        (None, Ok(_)) => (
            Value::Null,
            slots.clone().map(|s| json!(s)).unwrap_or(Value::Null),
            Some("plans ready unmeasured: the board names no undispatched count".to_string()),
        ),
        (_, Err(e)) => (
            Value::Null,
            Value::Null,
            Some(format!("plans ready unmeasured: {e}")),
        ),
    };
    let open = if gate_reason.is_some() {
        0
    } else {
        ceiling.saturating_sub(running)
    };
    let starts: Vec<String> = candidates.iter().take(open).cloned().collect();
    match &gate_reason {
        Some(reason) => candidates
            .iter()
            .for_each(|id| skips.push(json!({"id": id, "reason": reason}))),
        None => candidates.iter().skip(open).for_each(|id| {
            skips.push(json!({"id": id, "reason": format!("at ceiling {running} of {ceiling}")}))
        }),
    };
    if rows.is_empty() && running == 0 {
        skips.push(json!({"id": null, "reason": "no unplanned node in scope"}));
    }
    Ok(json!({
        "running": running,
        "ceiling": ceiling,
        "ceiling_source": ceiling_source,
        "plans_ready": pr_v,
        "slots": slots_v,
        "starts": starts,
        "target_ready": target_ready,
        "skips": skips,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A board payload naming one `unplanned` queue whose rows carry
    /// `(id, difficulty)` and, when `plans_ready` is Some, one `undispatched`
    /// queue counting it.
    fn unplanned_board(rows: &[(&str, &str)], plans_ready: Option<usize>) -> Value {
        let mut queues = vec![json!({
            "name": "unplanned", "status": "ok", "count": rows.len(),
            "rows": rows.iter().map(|(id, d)| json!({"id": id, "difficulty": d})).collect::<Vec<_>>()
        })];
        if let Some(pr) = plans_ready {
            queues.push(json!({
                "name": "undispatched", "status": "ok", "count": pr, "rows": []
            }));
        }
        json!({"queues": queues})
    }

    /// Call `r_blueprint` with a fixed provider (so the ambient
    /// FNO_ROUTE_PROVIDER cannot leak in) and a temp config dir.
    fn blueprint_reading(
        dir: &std::path::Path,
        config_toml: &str,
        board: Value,
        session_id: Option<&str>,
        holders: Vec<String>,
        slots: Result<usize, String>,
        floor: &str,
    ) -> Result<Value, String> {
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(fnodir.join("config.toml"), config_toml).unwrap();
        // FNO_CONFIG pins the walk to the fixture the way the lanes-cap test
        // does: a repo-root .fno/config.toml must never leak into the read.
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", fnodir.join("config.toml"));
        let prior_provider = std::env::var_os("FNO_ROUTE_PROVIDER");
        std::env::set_var("FNO_ROUTE_PROVIDER", "zai");
        let reading = r_blueprint(
            &Ok(board),
            dir,
            session_id.map(str::to_string),
            Ok(holders),
            slots,
            floor,
        );
        match prior_provider {
            Some(v) => std::env::set_var("FNO_ROUTE_PROVIDER", v),
            None => std::env::remove_var("FNO_ROUTE_PROVIDER"),
        }
        match prior_config {
            Some(v) => std::env::set_var("FNO_CONFIG", v),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        reading
    }

    /// The happy path: 4 unplanned candidates, running 0, ceiling capped to
    /// one per king even under a higher provider budget: only the first id in
    /// board order starts, the rest skip at the ceiling.
    #[test]
    fn r_blueprint_starts_up_to_the_ceiling_in_board_order() {
        let dir = std::env::temp_dir().join(format!("fno-bp-happy-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "[agents.provider_limits.zai]\nsubagents = 2\n",
            unplanned_board(
                &[
                    ("x-1", "high"),
                    ("x-2", "high"),
                    ("x-3", "high"),
                    ("x-4", "high"),
                ],
                Some(0),
            ),
            Some("sess-1"),
            vec![],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["running"], 0);
        assert_eq!(reading["ceiling"], 1);
        assert_eq!(reading["ceiling_source"], "one per king");
        assert_eq!(reading["plans_ready"], 0);
        assert_eq!(reading["slots"], 4);
        assert_eq!(reading["starts"], json!(["x-1"]), "{reading}");
        assert_eq!(
            reading["skips"],
            json!([
                {"id": "x-2", "reason": "at ceiling 0 of 1"},
                {"id": "x-3", "reason": "at ceiling 0 of 1"},
                {"id": "x-4", "reason": "at ceiling 0 of 1"}
            ]),
            "{reading}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC7: a provider budget can only lower the one-per-king ceiling, and a
    /// start waits until plans ready are fewer than the king's worker slots.
    #[test]
    fn r_blueprint_ceiling_is_one_per_king_under_a_bigger_budget() {
        let dir = std::env::temp_dir().join(format!("fno-bp-one-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "[agents.provider_limits.zai]\nsubagents = 3\n",
            unplanned_board(&[("x-1", "high"), ("x-2", "high")], Some(2)),
            Some("sess-1"),
            vec![],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["ceiling"], 1);
        assert_eq!(reading["ceiling_source"], "one per king");
        assert_eq!(reading["plans_ready"], 2);
        assert_eq!(reading["slots"], 4);
        assert_eq!(reading["starts"], json!(["x-1"]), "{reading}");
        assert_eq!(
            reading["skips"][0]["reason"], "at ceiling 0 of 1",
            "{reading}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC6: plans ready at or above slots stops every start.
    #[test]
    fn r_blueprint_skips_everything_when_build_slots_are_the_bottleneck() {
        let dir = std::env::temp_dir().join(format!("fno-bp-bottl-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "",
            unplanned_board(
                &[("x-1", "high"), ("x-2", "high"), ("x-3", "high")],
                Some(5),
            ),
            Some("sess-1"),
            vec![],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["running"], 0);
        assert_eq!(reading["starts"], json!([]));
        assert_eq!(reading["skips"].as_array().unwrap().len(), 3);
        assert_eq!(
            reading["skips"][0]["reason"],
            "plans ready 5 / slots 4: build slots are the bottleneck",
            "{reading}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC8: an unreadable undispatched queue reads plans ready unmeasured,
    /// and no candidate starts on an unmeasured gate.
    #[test]
    fn r_blueprint_skips_everything_when_plans_ready_unmeasured() {
        let dir = std::env::temp_dir().join(format!("fno-bp-unmeas-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "",
            unplanned_board(&[("x-1", "high"), ("x-2", "high")], None),
            Some("sess-1"),
            std::vec![],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["starts"], json!([]), "{reading}");
        assert_eq!(
            reading["skips"][0]["reason"],
            "plans ready unmeasured: the board names no undispatched count",
            "{reading}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The floor: 0 candidates and running 0 leaves the one null skip, so a
    /// beat with nothing to start still leaves a receipt.
    #[test]
    fn r_blueprint_leaves_a_receipt_when_nothing_is_unplanned() {
        let dir = std::env::temp_dir().join(format!("fno-bp-empty-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "",
            unplanned_board(&[], Some(0)),
            Some("sess-1"),
            vec![],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["starts"], json!([]));
        assert_eq!(
            reading["skips"],
            json!([{"id": null, "reason": "no unplanned node in scope"}])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// At the ceiling no candidate starts, whatever the queue holds.
    #[test]
    fn r_blueprint_skips_everything_at_the_ceiling() {
        let dir = std::env::temp_dir().join(format!("fno-bp-ceil-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "[agents.provider_limits.zai]\nsubagents = 2\n",
            unplanned_board(
                &[("x-1", "high"), ("x-2", "high"), ("x-3", "high")],
                Some(0),
            ),
            Some("sess-1"),
            vec![
                "blueprint-session:sess-1".to_string(),
                "blueprint-session:sess-1".to_string(),
            ],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["running"], 2);
        assert_eq!(reading["starts"], json!([]));
        assert_eq!(reading["skips"].as_array().unwrap().len(), 3);
        assert_eq!(
            reading["skips"][0]["reason"], "at ceiling 2 of 1",
            "{reading}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another session's blueprint claim never counts toward this king's
    /// running total.
    #[test]
    fn r_blueprint_does_not_count_another_sessions_claim() {
        let dir = std::env::temp_dir().join(format!("fno-bp-other-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "",
            unplanned_board(&[("x-1", "high")], Some(0)),
            Some("sess-1"),
            vec!["blueprint-session:someone-else".to_string()],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["running"], 0);
        assert_eq!(reading["starts"], json!(["x-1"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VERIFY (default floor): a low-difficulty unplanned row reads
    /// target-ready and a high one stays a blueprint candidate.
    #[test]
    fn r_blueprint_routes_low_to_target_ready_and_high_to_blueprint_at_the_default_floor() {
        let dir = std::env::temp_dir().join(format!("fno-bp-part-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "",
            unplanned_board(&[("x-low", "low"), ("x-high", "high")], Some(0)),
            Some("sess-1"),
            vec![],
            Ok(4),
            "high",
        )
        .unwrap();
        assert_eq!(reading["starts"], json!(["x-high"]), "{reading}");
        assert_eq!(reading["target_ready"], json!(["x-low"]), "{reading}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VERIFY (floor medium): a medium row flips to a blueprint candidate
    /// and a low row stays target-ready.
    #[test]
    fn r_blueprint_flips_a_medium_row_to_blueprint_at_the_medium_floor() {
        let dir = std::env::temp_dir().join(format!("fno-bp-med-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "",
            unplanned_board(&[("x-med", "medium"), ("x-low", "low")], Some(0)),
            Some("sess-1"),
            vec![],
            Ok(4),
            "medium",
        )
        .unwrap();
        assert_eq!(reading["starts"], json!(["x-med"]), "{reading}");
        assert_eq!(reading["target_ready"], json!(["x-low"]), "{reading}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing session id fails the reading: it must never read as
    /// `running 0`, which would name starts past the ceiling.
    #[test]
    fn r_blueprint_fails_without_a_session_id() {
        let dir = std::env::temp_dir().join(format!("fno-bp-nosess-{}", std::process::id()));
        let reading = blueprint_reading(
            &dir,
            "",
            unplanned_board(&[("x-1", "high")], Some(0)),
            None,
            vec![],
            Ok(4),
            "high",
        );
        assert!(reading.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unreadable unplanned queue fails the reading instead of reading as
    /// an empty candidate list.
    #[test]
    fn r_blueprint_fails_on_an_unreadable_unplanned_queue() {
        let dir = std::env::temp_dir().join(format!("fno-bp-badq-{}", std::process::id()));
        let board = json!({"queues": [{"name": "unplanned", "status": "error",
            "error": "graph unreadable", "rows": []}]});
        let reading = blueprint_reading(&dir, "", board, Some("sess-1"), vec![], Ok(4), "high");
        assert!(reading.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A row with no string id skips with a reason; it never surfaces as an
    /// "unknown" candidate a king would try to dispatch.
    #[test]
    fn r_blueprint_skips_a_row_without_a_string_id() {
        let dir = std::env::temp_dir().join(format!("fno-bp-noid-{}", std::process::id()));
        let board = json!({"queues": [{"name": "unplanned", "status": "ok", "count": 1,
            "rows": [{"id": null, "difficulty": "high"}]}]});
        let reading =
            blueprint_reading(&dir, "", board, Some("sess-1"), vec![], Ok(4), "high").unwrap();
        assert_eq!(reading["starts"], json!([]), "{reading}");
        assert_eq!(reading["target_ready"], json!([]), "{reading}");
        assert_eq!(
            reading["skips"],
            json!([{"id": null, "reason": "row carries no id"}]),
            "{reading}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
