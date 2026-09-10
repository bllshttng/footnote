//! The king termination board read: what work and operator questions remain.

use serde_json::Value;
use std::path::Path;

pub(crate) struct KingBoard {
    pub(crate) actionable: i64,
    pub(crate) top_row: Option<String>,
    pub(crate) unreadable: i64,
    pub(crate) over_budget: i64,
    pub(crate) actionable_ids: Vec<String>,
    pub(crate) operator_question_sessions: Vec<String>,
    pub(crate) operator_questions_unreadable: bool,
}

fn row_identity(queue: &str, row: &Value) -> String {
    let id = row
        .get("id")
        .or_else(|| row.get("key"))
        .or_else(|| row.get("number"))
        .map(|v| v.to_string())
        .unwrap_or_else(|| row.to_string());
    format!("{queue}:{}", id.trim_matches('"'))
}

pub(crate) fn parse_king_board_value(value: &Value) -> Option<KingBoard> {
    let actionable = value.get("actionable")?.as_i64()?;
    let unreadable = value
        .get("unreadable")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let over_budget = value
        .get("over_budget")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut top_row = None;
    let mut actionable_ids: Vec<String> = Vec::new();
    let mut operator_question_sessions: Vec<String> = Vec::new();
    let mut operator_questions_unreadable = false;
    if let Some(queues) = value.get("queues").and_then(|q| q.as_array()) {
        for queue in queues {
            let name = queue.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let status = queue.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if name == "operator_question" && crate::king_board::not_read_status(status) {
                operator_questions_unreadable = true;
            }
            if crate::king_board::not_read_status(status) {
                if top_row.is_none() {
                    let err = queue.get("error").and_then(|v| v.as_str()).unwrap_or("");
                    top_row = Some(if status == "over_budget" {
                        format!("{name} not read: {err}")
                    } else {
                        format!("{name} is unreadable: {err}")
                    });
                }
                continue;
            }
            if name == "operator_question" {
                operator_question_sessions.extend(
                    queue
                        .get("rows")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|row| row.get("session_id").and_then(|v| v.as_str()))
                        .map(str::to_owned),
                );
            }
            if queue.get("actionable").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            for row in queue
                .get("rows")
                .and_then(|v| v.as_array())
                .unwrap_or(&vec![])
            {
                let identity = row_identity(name, row);
                if top_row.is_none() {
                    top_row = Some(identity.clone());
                }
                actionable_ids.push(identity);
            }
        }
    }
    Some(KingBoard {
        actionable,
        top_row,
        unreadable,
        over_budget,
        actionable_ids,
        operator_question_sessions,
        operator_questions_unreadable,
    })
}

pub(crate) fn read_king_board(
    fno_bin: &str,
    cwd: &Path,
    state_path: &Path,
) -> Result<KingBoard, String> {
    let _ = fno_bin;
    let opts = crate::king_board::BoardOpts {
        budget_ms: crate::loopcheck::stopgate_read_timeout().as_millis() as u64,
        max_pr_reads: 20,
        state_path: Some(state_path.to_path_buf()),
        cwd: Some(cwd.to_path_buf()),
    };
    let payload = crate::king_board::read_board(&opts);
    parse_king_board_value(&payload).ok_or_else(|| {
        "unparseable board payload: the collector returned a shape parse_king_board_value cannot read"
            .to_string()
    })
}

/// The spawn gate's read-only verdict (`fno agents gate-status --json`):
/// whether a dispatch would be admitted right now, and which constraint binds.
pub(crate) struct GateProbe {
    pub(crate) verdict: String,
    pub(crate) reason: Option<String>,
    pub(crate) message: Option<String>,
}

pub(crate) fn parse_gate_probe(payload: &Value) -> Option<GateProbe> {
    let verdict = payload.get("verdict")?.as_str()?.to_string();
    Some(GateProbe {
        verdict,
        reason: payload
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        message: payload
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

impl GateProbe {
    /// The one-line constraint statement this verdict carries, for a stop-hook
    /// message: the probe's own sentence, else its reason token, else a bare
    /// statement that dispatch capacity is gone.
    pub(crate) fn constraint(&self) -> String {
        self.message
            .clone()
            .or_else(|| self.reason.clone())
            .unwrap_or_else(|| "dispatch capacity exhausted".to_string())
    }
}

/// Ask the gate the dispatch would ask. Any failure here is `Err`: the caller
/// must fall back to today's block, never read a broken probe as saturation.
pub(crate) fn probe_dispatch_capacity(fno_bin: &str, cwd: &Path) -> Result<GateProbe, String> {
    let out = crate::loopcheck::bounded_read(
        std::ffi::OsStr::new(fno_bin),
        &["agents", "gate-status"],
        cwd,
        "spawn gate status",
        crate::loopcheck::stopgate_read_timeout(),
    )
    .map_err(|error| format!("spawn gate status failed: {}", error.render()))?;
    if !out.status.success() {
        return Err(format!("spawn gate status exited {}", out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| "spawn gate status returned no JSON".to_string())?;
    parse_gate_probe(&payload).ok_or_else(|| "spawn gate status payload unparseable".to_string())
}

/// The saturation decision for one board fire (x-df28): a fire where the top
/// actionable row is undispatched and the gate refuses means every candidate
/// dispatch would be refused, so the stop is legitimate. `probe: None` (not
/// asked, or asked and failed) and an accepted probe both return `None` - a
/// broken probe must never convert a block into an allow.
#[derive(Debug)]
pub(crate) enum SaturationOutcome {
    /// Every actionable row is undispatched: nothing on the board is reachable
    /// without a dispatch.
    Saturated { blocked: i64 },
    /// Capacity-blocked rows exist but a non-dispatch row is still actionable,
    /// so the block survives - pointed at that row instead.
    BlockedWithNext { next: String, blocked: i64 },
}

pub(crate) fn saturation_verdict(
    board: &KingBoard,
    probe: Option<&GateProbe>,
) -> Option<SaturationOutcome> {
    let top = board.top_row.as_deref()?;
    if !top.starts_with("undispatched:") {
        return None;
    }
    let parsed = probe?;
    if parsed.verdict != "refused" {
        return None;
    }
    let blocked = board
        .actionable_ids
        .iter()
        .filter(|id| id.starts_with("undispatched:"))
        .count() as i64;
    if blocked == 0 {
        // The top row names an undispatched node but no actionable id agrees;
        // trust neither and let the normal block stand.
        return None;
    }
    let next = board
        .actionable_ids
        .iter()
        .find(|id| !id.starts_with("undispatched:"));
    next.map(|next| SaturationOutcome::BlockedWithNext {
        next: next.clone(),
        blocked,
    })
    .or(Some(SaturationOutcome::Saturated { blocked }))
}

/// What the capacity gate decided for this fire, rendered and ready for the
/// caller's two verdicts (x-df28). Composition of the probe read, the pure
/// saturation verdict, and the two messages the king block carries.
pub(crate) enum CapacityGate {
    /// Every actionable row is undispatched and the gate refuses: the stop is
    /// legitimate, so the caller terminates NoWork with this message.
    Saturated {
        message: String,
        blocked: i64,
        fires: u64,
    },
    /// Keep the block, pointed at a non-dispatch row, with the honest split.
    Split {
        message: String,
        actionable: i64,
        fires: u64,
        journal: Value,
    },
}

pub(crate) fn capacity_gate(
    board: &KingBoard,
    fno_bin: &str,
    cwd: &Path,
    session_id: &str,
    dry: u64,
    emit: &dyn Fn(&str, Value),
) -> Option<CapacityGate> {
    let probe = match board.top_row.as_deref() {
        Some(top) if top.starts_with("undispatched:") => {
            match probe_dispatch_capacity(fno_bin, cwd) {
                Ok(p) => Some(p),
                // A failed probe keeps today's block, never reads as saturation.
                Err(_) => None,
            }
        }
        _ => None,
    };
    let verdict = saturation_verdict(board, probe.as_ref())?;
    let constraint = probe
        .as_ref()
        .map(|p| p.constraint())
        .unwrap_or_else(|| "dispatch capacity exhausted".to_string());
    match verdict {
        SaturationOutcome::Saturated { blocked } => {
            let message = format!(
                "fleet saturated: {constraint}; \
                 {blocked} actionable rows all blocked on dispatch capacity"
            );
            Some(CapacityGate::Saturated {
                message,
                blocked,
                fires: dry + 1,
            })
        }
        SaturationOutcome::BlockedWithNext { next, blocked } => {
            let message = format!(
                "{} actionable now; next: {next}; \
                 {blocked} blocked on dispatch capacity ({constraint})",
                board.actionable - blocked
            );
            let journal = serde_json::json!({
                "session_id": session_id,
                "actionable": board.actionable,
                "actionable_now": board.actionable - blocked,
                "blocked_on_capacity": blocked,
                "actionable_ids": board.actionable_ids,
                "cleared": false,
            });
            Some(CapacityGate::Split {
                message,
                actionable: board.actionable,
                fires: dry + 1,
                journal,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn board_with_queues(queues: Value) -> Value {
        json!({
            "actionable": 0,
            "unreadable": 1,
            "over_budget": 1,
            "queues": queues,
        })
    }

    #[test]
    fn an_over_budget_top_row_says_not_read_never_unreadable() {
        let board = board_with_queues(json!([
            {"name": "undispatched", "status": "over_budget",
             "error": "killed at its 28.5s slice of the board budget; the source did not fail",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        let top = parsed.top_row.unwrap();
        assert!(top.starts_with("undispatched not read:"), "{top}");
        assert!(!top.contains("unreadable"), "{top}");
    }

    #[test]
    fn an_unreadable_top_row_still_says_unreadable() {
        let board = board_with_queues(json!([
            {"name": "claims", "status": "unreadable", "error": "exit 1: boom",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        let top = parsed.top_row.unwrap();
        assert!(top.contains("is unreadable: exit 1"), "{top}");
    }

    #[test]
    fn the_two_kinds_count_apart() {
        let board = board_with_queues(json!([
            {"name": "claims", "status": "unreadable", "error": "torn registry read",
             "actionable": true, "rows": []},
            {"name": "undispatched", "status": "over_budget",
             "error": "killed at its 28.5s slice of the board budget",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert_eq!(parsed.unreadable, 1);
        assert_eq!(parsed.over_budget, 1);
    }

    #[test]
    fn the_probe_payload_parses_in_both_verdicts() {
        let accepted = parse_gate_probe(&json!({
            "verdict": "accepted",
            "lanes": {"zai": {"cap": 10, "live": 3}},
        }))
        .expect("accepted payload parses");
        assert_eq!(accepted.verdict, "accepted");

        let refused = parse_gate_probe(&json!({
            "verdict": "refused", "reason": "provider_cap",
            "message": "every dispatch lane at cap: zai 10/10",
            "lanes": {"zai": {"cap": 10, "live": 10}},
        }))
        .expect("refused payload parses");
        assert_eq!(refused.verdict, "refused");
        assert_eq!(refused.reason.as_deref(), Some("provider_cap"));
        assert_eq!(
            refused.message.as_deref(),
            Some("every dispatch lane at cap: zai 10/10")
        );
    }

    fn board_undispatched_only() -> KingBoard {
        parse_king_board_value(&board_with_queues(json!([
            {"name": "undispatched", "status": "ok", "actionable": true,
             "rows": [{"id": "x-1"}, {"id": "x-2"}]},
        ])))
        .unwrap()
    }

    fn refused_probe() -> GateProbe {
        GateProbe {
            verdict: "refused".to_string(),
            reason: Some("provider_cap".to_string()),
            message: Some("every dispatch lane at cap: zai 10/10".to_string()),
        }
    }

    fn accepted_probe() -> GateProbe {
        GateProbe {
            verdict: "accepted".to_string(),
            reason: None,
            message: None,
        }
    }

    #[test]
    fn a_refused_probe_with_only_undispatched_rows_ends_the_reign_no_work() {
        let board = board_undispatched_only();
        let verdict = saturation_verdict(&board, Some(&refused_probe())).expect("saturated");
        match verdict {
            SaturationOutcome::Saturated { blocked } => assert_eq!(blocked, 2),
            other => panic!("expected Saturated, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_probe_with_a_non_dispatch_row_points_the_block_at_it() {
        let board = parse_king_board_value(&board_with_queues(json!([
            {"name": "undispatched", "status": "ok", "actionable": true,
             "rows": [{"id": "x-1"}, {"id": "x-2"}]},
            {"name": "claims", "status": "ok", "actionable": true,
             "rows": [{"id": "x-3"}]},
        ])))
        .unwrap();
        let verdict =
            saturation_verdict(&board, Some(&refused_probe())).expect("blocked with next");
        match verdict {
            SaturationOutcome::BlockedWithNext { next, blocked } => {
                assert_eq!(next, "claims:x-3");
                assert_eq!(blocked, 2);
            }
            other => panic!("expected BlockedWithNext, got {other:?}"),
        }
    }

    #[test]
    fn an_accepted_probe_never_allows_the_stop() {
        let board = board_undispatched_only();
        assert!(saturation_verdict(&board, Some(&accepted_probe())).is_none());
    }

    #[test]
    fn a_broken_probe_never_allows_the_stop() {
        let board = board_undispatched_only();
        assert!(saturation_verdict(&board, None).is_none());
    }

    #[test]
    fn a_non_undispatched_top_row_never_probes() {
        let board = parse_king_board_value(&board_with_queues(json!([
            {"name": "claims", "status": "ok", "actionable": true,
             "rows": [{"id": "x-3"}]},
        ])))
        .unwrap();
        assert!(saturation_verdict(&board, Some(&refused_probe())).is_none());
    }
}
