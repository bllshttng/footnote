//! The king termination board read: what work and operator questions remain.

use crate::loopcheck::TerminationReason;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub(crate) struct KingManifest {
    pub(crate) fno_id: String,
    pub(crate) scope: String,
    pub(crate) created_at: Option<String>,
    /// The crowned session the manifest names; `loop_reign`'s split read keys
    /// on it. Empty on manifests written before identity fields existed.
    pub(crate) harness_session_id: Option<String>,
    /// The harness that crowned this scope, written at coronation
    /// (`FNO_HARNESS`, default `claude`); absent on manifests written before
    /// the field existed.
    pub(crate) harness: Option<String>,
    /// `pass` | `court`; absent reads as `pass`, never a third shape.
    pub(crate) shape: String,
    pub(crate) max_iterations: u64,
    pub(crate) respawn_count: u64,
    pub(crate) respawn_ceiling: u64,
    /// `span:<N>[smhd]` | `compactions:<N>`; absent reads
    /// [`crate::king_term::DEFAULT_TERM`], undeclared.
    pub(crate) term: Option<String>,
    /// The written reason for a declared/reached term's replacement; absent
    /// on the first declaration.
    pub(crate) term_reason: Option<String>,
}

pub(crate) fn parse_king_manifest(content: &str) -> Option<KingManifest> {
    let mut out = KingManifest {
        max_iterations: 40,
        respawn_ceiling: 4,
        ..Default::default()
    };
    let mut saw_frontmatter = false;
    for line in content.lines() {
        if line.trim() == "---" {
            if saw_frontmatter {
                break;
            }
            saw_frontmatter = true;
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let value = raw.trim().trim_matches('"').to_string();
        match key.trim() {
            "fno_id" => out.fno_id = value,
            "scope" => out.scope = value,
            "created_at" => out.created_at = Some(value),
            "harness_session_id" => {
                if !value.is_empty() && value != "null" {
                    out.harness_session_id = Some(value);
                }
            }
            "harness" => out.harness = Some(value),
            "shape" => out.shape = value,
            "term" => out.term = Some(value),
            "term_reason" => out.term_reason = Some(value),
            "budget_max_iterations" => {
                if let Ok(n) = value.parse::<u64>() {
                    out.max_iterations = n;
                }
            }
            "respawn_count" => {
                if let Ok(n) = value.parse::<u64>() {
                    out.respawn_count = n;
                }
            }
            "respawn_ceiling" => {
                if let Ok(n) = value.parse::<u64>() {
                    out.respawn_ceiling = n;
                }
            }
            _ => {}
        }
    }
    if saw_frontmatter && !out.fno_id.is_empty() {
        Some(out)
    } else {
        None
    }
}

pub(crate) struct StandDownGate {
    pub(crate) reading: String,
    pub(crate) message: String,
}

pub(crate) fn stand_down_gate(
    manifest: &KingManifest,
    transcript: &Path,
    cwd: &Path,
) -> Option<StandDownGate> {
    let session = manifest
        .harness_session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())?;
    let capture_dir = std::env::var_os("FNO_OPERATOR_CAPTURE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::agents_config::state_dir(cwd).map(|dir| dir.join("operator-capture")))?;
    let pending = match crate::operator_turns::pending_stand_down(
        session,
        transcript,
        &capture_dir,
        chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
    ) {
        Ok(turns) => turns,
        Err(error) => {
            eprintln!("loop-check: stand-down gate skipped: {error}");
            return None;
        }
    };
    let (turn_id, excerpt) = pending.first()?;
    Some(StandDownGate {
        reading: format!("operator stand-down turn {turn_id} unacked"),
        message: format!(
            "operator stand-down turn {turn_id} is unacked: \"{excerpt}\". Answer it as a verdict on this reign, then run fno inbox operator ack {turn_id} --outcome <nothing|law:<id>|node:<id>> --why \"<your verdict>\""
        ),
    })
}

pub(crate) struct KingBoard {
    pub(crate) actionable: i64,
    pub(crate) top_row: Option<String>,
    /// any queue on this board failed to read. The quiet branch
    /// refuses to certify a quiet board while this is true, instead of
    /// trusting a count that cannot see the blind queues.
    pub(crate) unreadable_sources: bool,
    pub(crate) actionable_ids: Vec<String>,
    pub(crate) spawn_held_ids: Vec<String>,
    pub(crate) blind_queues: Vec<String>,
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
    let mut top_row = None;
    let mut actionable_ids: Vec<String> = Vec::new();
    let mut spawn_held_ids: Vec<String> = Vec::new();
    let mut blind_queues: Vec<String> = Vec::new();
    let mut operator_question_sessions: Vec<String> = Vec::new();
    let mut operator_questions_unreadable = false;
    let mut unreadable_sources = false;
    if let Some(queues) = value.get("queues").and_then(|q| q.as_array()) {
        for queue in queues {
            let name = queue.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let status = queue.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if crate::king_board::not_read_status(status) {
                unreadable_sources = true;
            }
            if name == "operator_question" && crate::king_board::not_read_status(status) {
                operator_questions_unreadable = true;
            }
            if crate::king_board::not_read_status(status) {
                let err = queue.get("error").and_then(|v| v.as_str()).unwrap_or("");
                blind_queues.push(if status == "over_budget" {
                    format!("{name} not read: {err}")
                } else {
                    format!("{name} is unreadable: {err}")
                });
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
            let spawn_held = queue.get("verb").and_then(|v| v.as_str()) == Some("/fno:target");
            for row in queue
                .get("rows")
                .and_then(|v| v.as_array())
                .unwrap_or(&vec![])
            {
                // Row-level veto: a mergeable_pr row the merge gate
                // found not-ready carries `actionable: false` and names no
                // next action, however green the listing called it.
                if row.get("actionable").and_then(|v| v.as_bool()) == Some(false) {
                    continue;
                }
                let identity = row_identity(name, row);
                if top_row.is_none() {
                    top_row = Some(identity.clone());
                }
                if spawn_held {
                    spawn_held_ids.push(identity.clone());
                }
                actionable_ids.push(identity);
            }
        }
    }
    Some(KingBoard {
        actionable,
        top_row,
        unreadable_sources,
        actionable_ids,
        spawn_held_ids,
        blind_queues,
        operator_question_sessions,
        operator_questions_unreadable,
    })
}

/// The quiet journal row both blind-board blocks emit: a blind board must
/// still advance the fire counter with its row.
pub(crate) fn king_quiet_body(session_id: &str, actionable: i64) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "actionable": actionable,
        "actionable_ids": [],
        "cleared": false,
    })
}

/// The quiet-branch block message: what the drain read measured, or the
/// error when the read failed (the sentinel count i64::MAX rides beside it).
pub(crate) fn king_quiet_message(
    undelivered: i64,
    drain_error: Option<&crate::loop_king::ScopeDrainError>,
) -> String {
    match drain_error {
        Some(e) => {
            format!("board quiet but scope delivery is unreadable: {e}; blocking completion")
        }
        None => format!("board quiet; {undelivered} scope nodes still undelivered (drive each scope node to done or superseded to drain)"),
    }
}

/// The quiet-branch journal row: like `king_quiet_body`, plus the drain read
/// it measured and whether that count shrank since the last fire.
pub(crate) fn king_undelivered_body(session_id: &str, undelivered: i64, shrank: bool) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "actionable": 0,
        "undelivered": undelivered,
        "actionable_ids": [],
        "cleared": shrank,
    })
}

/// The drain-reserve journal row: what the fire's last, reserved read cost
/// against the bound it was granted and the budget it started with. The
/// reserve is silent by design; this row is its series.
pub(crate) fn drain_reserve_body(
    session_id: &str,
    scope: &str,
    bound_ms: u64,
    elapsed_ms: u64,
    remaining_budget_ms: u64,
) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "scope": scope,
        "bound_ms": bound_ms,
        "elapsed_ms": elapsed_ms,
        "remaining_budget_ms": remaining_budget_ms,
    })
}

/// The stop hook's one JSON verdict line: decision, reason, and the counts
/// a wrapper reads.
pub(crate) fn king_output(
    decision: &str,
    reason: Option<TerminationReason>,
    message: &str,
    actionable: i64,
    fires: u64,
) -> String {
    serde_json::json!({
        "driver": "king",
        "decision": decision,
        "termination_reason": reason,
        // `reason` carries the human-readable why, distinct from the enum
        // above: a stop hook reader wants the top actionable row, not a tag.
        "reason": message,
        "message": message,
        "actionable": actionable,
        "fires": fires,
    })
    .to_string()
}

pub(crate) fn read_king_board(
    fno_bin: &str,
    cwd: &Path,
    state_path: &Path,
) -> Result<KingBoard, String> {
    let _ = fno_bin;
    let opts = crate::king_board::BoardOpts {
        budget_ms: crate::loopcheck::stopgate_read_timeout().as_millis() as u64,
        max_pr_reads: crate::king_board::DEFAULT_MAX_PR_READS,
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

    pub(crate) fn owner(&self) -> String {
        match self.reason.as_deref() {
            Some("fleet-stop" | "fleet-stop-unavailable") => {
                "fno agents incident status; fno agents incident clear --reason <text>".into()
            }
            Some("max_live" | "king_share") => {
                "live workers exiting (`fno agents gate-status` slot_rows)".into()
            }
            Some("provider_cap" | "provider_quota_lock") => {
                "provider lanes (`fno-agents provider-cap status`)".into()
            }
            _ => "fno agents gate-status".into(),
        }
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

/// The saturation decision for one board fire: a refused gate blocks rows
/// from queues with `/fno:target` (currently `undispatched`, `unheld_progress`,
/// and `undriven_pr`). A readable non-spawn row keeps the block pointed at it.
/// `probe: None` (not asked, or asked and failed) and an accepted probe return
/// `None` - a broken probe must never convert a block into an allow.
#[derive(Debug)]
pub(crate) enum SaturationOutcome {
    /// Every readable actionable row uses `/fno:target` and needs dispatch
    /// capacity. Current kinds are `undispatched`, `unheld_progress`, and
    /// `undriven_pr`.
    Saturated { blocked: i64 },
    /// Every readable row needs dispatch, and at least one queue was not read.
    SaturatedBlind { blocked: i64 },
    /// Capacity-blocked rows exist but a non-dispatch row is still actionable,
    /// so the block survives - pointed at that row instead.
    BlockedWithNext { next: String, blocked: i64 },
}

pub(crate) fn saturation_verdict(
    board: &KingBoard,
    probe: Option<&GateProbe>,
) -> Option<SaturationOutcome> {
    let parsed = probe?;
    if parsed.verdict != "refused" {
        return None;
    }
    let blocked = board.spawn_held_ids.len() as i64;
    if blocked == 0 {
        return None;
    }
    let next = board
        .actionable_ids
        .iter()
        .find(|id| !board.spawn_held_ids.contains(id));
    if let Some(next) = next {
        return Some(SaturationOutcome::BlockedWithNext {
            next: next.clone(),
            blocked,
        });
    }
    if !board.blind_queues.is_empty() {
        return Some(SaturationOutcome::SaturatedBlind { blocked });
    }
    Some(SaturationOutcome::Saturated { blocked })
}

/// What the capacity gate decided for this fire, rendered and ready for the
/// caller's two verdicts. Composition of the probe read, the pure
/// saturation verdict, and the two messages the king block carries.
pub(crate) enum CapacityGate {
    /// Every readable actionable row uses `/fno:target` (`undispatched`,
    /// `unheld_progress`, or `undriven_pr`) and the gate refuses. With no blind
    /// queues, the caller may terminate NoWork with this message.
    Saturated {
        message: String,
        blocked: i64,
        fires: u64,
    },
    /// Every readable row waits on dispatch while at least one queue is blind.
    SaturatedBlind { message: String, actionable: i64 },
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
    _emit: &dyn Fn(&str, Value),
) -> Option<CapacityGate> {
    let probe = if board.spawn_held_ids.is_empty() {
        None
    } else {
        match probe_dispatch_capacity(fno_bin, cwd) {
            Ok(p) => Some(p),
            // A failed probe keeps today's block, never reads as saturation.
            Err(_) => None,
        }
    };
    let verdict = saturation_verdict(board, probe.as_ref())?;
    let probe = probe.as_ref()?;
    let constraint = probe.constraint();
    let owner = probe.owner();
    match verdict {
        SaturationOutcome::Saturated { blocked } => {
            let message = format!(
                "fleet saturated: {constraint}; \
                 {blocked} actionable rows all blocked on dispatch capacity; owner: {owner}"
            );
            Some(CapacityGate::Saturated {
                message,
                blocked,
                fires: dry + 1,
            })
        }
        SaturationOutcome::SaturatedBlind { blocked } => {
            let blind = board.blind_queues.join(", ");
            Some(CapacityGate::SaturatedBlind {
                message: format!(
                    "{blocked} rows waiting on dispatch capacity ({constraint}; owner: {owner}); not read: {blind}"
                ),
                actionable: board.actionable,
            })
        }
        SaturationOutcome::BlockedWithNext { next, blocked } => {
            let blind = if board.blind_queues.is_empty() {
                String::new()
            } else {
                format!("; not read: {}", board.blind_queues.join(", "))
            };
            let message = format!(
                "{} actionable now; next: {next}; \
                 {blocked} waiting on dispatch capacity ({constraint}; owner: {owner}){blind}",
                board.actionable - blocked,
            );
            let journal = serde_json::json!({
                "session_id": session_id,
                "actionable": board.actionable,
                "actionable_now": board.actionable - blocked,
                "blocked_on_capacity": blocked,
                "waiting_owner": owner,
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

/// Why a blocking branch must stop instead of blocking again.
pub(crate) struct BoundBreach {
    pub(crate) reason: TerminationReason,
    pub(crate) message: String,
    pub(crate) fires: u64,
}

/// The bounds every blocking branch of `king_decide` owes: the manifest
/// ceiling `--max-iterations` advertises, and the dry-fire backstop. One
/// function because a branch that grew its own copy of either lost both: the
/// quiet-board return sat above both and a crown with undelivered scope
/// blocked forever, never reaching the parked state that asks the operator.
/// Budget is checked first, matching the ordering the ceiling branch commits
/// to: an exhausted king reports the reason that actually stopped it.
pub(crate) fn bound_breached(
    total: u64,
    dry: u64,
    max_iterations: u64,
    waiting_on: &str,
) -> Option<BoundBreach> {
    if total + 1 >= max_iterations {
        return Some(BoundBreach {
            reason: TerminationReason::Budget,
            message: format!(
                "{} fires reached the manifest ceiling of {max_iterations}; {waiting_on}",
                total + 1
            ),
            fires: dry,
        });
    }
    if dry + 1 >= crate::loop_king::KING_DRY_FIRE_CEILING {
        return Some(BoundBreach {
            reason: TerminationReason::NoProgress,
            message: format!("{} fires with nothing cleared; {waiting_on}", dry + 1),
            fires: dry + 1,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_null_harness_session_is_treated_as_legacy_missing_identity() {
        let manifest = parse_king_manifest("---\nfno_id: k\nharness_session_id: null\n---\n")
            .expect("manifest parses");
        assert!(manifest.harness_session_id.is_none());
    }

    fn board_with_queues(queues: Value) -> Value {
        json!({
            "actionable": 0,
            "queues": queues,
        })
    }

    #[test]
    fn an_over_budget_queue_stays_blind_without_becoming_the_next_action() {
        let board = board_with_queues(json!([
            {"name": "undispatched", "status": "over_budget",
             "error": "killed at its 28.5s slice of the board budget; the source did not fail",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert!(parsed.top_row.is_none());
        assert_eq!(
            parsed.blind_queues,
            vec!["undispatched not read: killed at its 28.5s slice of the board budget; the source did not fail".to_string()]
        );
    }

    #[test]
    fn an_unreadable_queue_never_becomes_the_next_action() {
        let board = board_with_queues(json!([
            {"name": "claims", "status": "unreadable", "error": "exit 1: boom",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert!(parsed.top_row.is_none());
        assert_eq!(
            parsed.blind_queues,
            vec!["claims is unreadable: exit 1: boom".to_string()]
        );
    }

    #[test]
    fn a_readable_action_is_next_behind_a_blind_queue() {
        let board = board_with_queues(json!([
            {"name": "unplanned", "status": "over_budget", "error": "truth probe timed out",
             "actionable": true, "rows": []},
            {"name": "mergeable_pr", "status": "ok", "actionable": true,
             "rows": [{"number": 2398}]},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert_eq!(parsed.top_row.as_deref(), Some("mergeable_pr:2398"));
        assert_eq!(
            parsed.blind_queues,
            vec!["unplanned not read: truth probe timed out".to_string()]
        );
    }

    #[test]
    fn queue_verb_marks_unheld_progress_and_undriven_pr_as_spawn_held() {
        let board = board_with_queues(json!([
            {"name": "unheld_progress", "status": "ok", "actionable": true,
             "verb": "/fno:target", "rows": [{"id": "x-1"}]},
            {"name": "undriven_pr", "status": "ok", "actionable": true,
             "verb": "/fno:target", "rows": [{"number": 2398}]},
            {"name": "unplanned", "status": "ok", "actionable": true,
             "verb": "/fno:blueprint", "rows": [{"id": "x-2"}]},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert_eq!(
            parsed.spawn_held_ids,
            vec![
                "unheld_progress:x-1".to_string(),
                "undriven_pr:2398".to_string()
            ]
        );
        assert_eq!(
            parsed.actionable_ids,
            vec![
                "unheld_progress:x-1".to_string(),
                "undriven_pr:2398".to_string(),
                "unplanned:x-2".to_string(),
            ]
        );
    }

    #[test]
    fn a_blind_queue_sets_the_unreadable_sources_flag() {
        // one unreadable queue means the quiet branch must refuse to
        // certify; the named boolean carries that, never a count sentinel.
        let board = board_with_queues(json!([
            {"name": "undispatched", "status": "unreadable", "error": "exit 1: flo",
             "actionable": true, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert!(parsed.unreadable_sources);
        assert_eq!(parsed.actionable, 0);
    }

    #[test]
    fn a_fully_readable_board_leaves_the_flag_off() {
        let board = board_with_queues(json!([
            {"name": "undispatched", "status": "ok", "actionable": true,
             "count": 2, "rows": []},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        assert!(!parsed.unreadable_sources);
    }

    #[test]
    fn a_not_ready_mergeable_row_names_no_next_action() {
        // the stop hook offered mergeable_pr:1709 on four
        // consecutive stops while the merge gate refused that exact head.
        // The not-ready row names no next action.
        let board = board_with_queues(json!([
            {"name": "mergeable_pr", "status": "ok", "actionable": true,
             "count": 0, "rows": [
                {"number": 1709, "title": "green but held", "ready": false,
                 "actionable": false, "ready_blockers": ["review_in_flight"]},
                {"number": 1702},
            ]},
        ]));
        let parsed = parse_king_board_value(&board).unwrap();
        let top = parsed.top_row.unwrap();
        assert_eq!(top, "mergeable_pr:1702", "{top}");
        assert_eq!(parsed.actionable_ids, vec!["mergeable_pr:1702".to_string()]);
    }

    #[test]
    fn the_drain_reserve_row_carries_the_pair_that_tells_starved_from_expensive() {
        let body = drain_reserve_body("sess", "scope-b", 8000, 13_000, 900);
        assert_eq!(body["scope"], "scope-b");
        assert_eq!(body["session_id"], "sess");
        assert_eq!(body["bound_ms"], 8000);
        assert_eq!(body["elapsed_ms"], 13_000);
        assert_eq!(body["remaining_budget_ms"], 900);
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
             "verb": "/fno:target", "rows": [{"id": "x-1"}, {"id": "x-2"}]},
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

    fn gate_status_stub(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let fno = dir.join("fno");
        let payload = json!({
            "verdict": "refused",
            "reason": "max_live",
            "message": "15 live worker slots >= max_live 15",
        });
        std::fs::write(&fno, format!("#!/bin/sh\nprintf '%s\\n' '{payload}'\n")).unwrap();
        let mut permissions = std::fs::metadata(&fno).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fno, permissions).unwrap();
        fno
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
             "verb": "/fno:target", "rows": [{"id": "x-1"}, {"id": "x-2"}]},
            {"name": "unplanned", "status": "ok", "actionable": true,
             "verb": "/fno:blueprint",
             "rows": [{"id": "x-3"}]},
        ])))
        .unwrap();
        let verdict =
            saturation_verdict(&board, Some(&refused_probe())).expect("blocked with next");
        match verdict {
            SaturationOutcome::BlockedWithNext { next, blocked } => {
                assert_eq!(next, "unplanned:x-3");
                assert_eq!(blocked, 2);
            }
            other => panic!("expected BlockedWithNext, got {other:?}"),
        }
    }

    #[test]
    fn refused_probe_saturates_unheld_progress_and_undriven_pr() {
        let board = parse_king_board_value(&json!({
            "actionable": 2,
            "queues": [
                {"name": "unheld_progress", "status": "ok", "actionable": true,
                 "verb": "/fno:target", "rows": [{"id": "x-1"}]},
                {"name": "undriven_pr", "status": "ok", "actionable": true,
                 "verb": "/fno:target", "rows": [{"number": 2398}]},
            ],
        }))
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let fno = gate_status_stub(tmp.path());
        let emit = |_: &str, _: Value| {};

        let gate = capacity_gate(&board, fno.to_str().unwrap(), tmp.path(), "king", 0, &emit)
            .expect("a refused probe must classify the spawn-held queues");
        match gate {
            CapacityGate::Saturated {
                blocked, message, ..
            } => {
                assert_eq!(blocked, 2);
                assert!(message.contains("live workers exiting"), "{message}");
            }
            _ => panic!("expected all spawn-held rows to be saturated"),
        }
    }

    #[test]
    fn a_blind_board_with_only_spawn_held_rows_is_saturated_blind() {
        let board = parse_king_board_value(&json!({
            "actionable": 2,
            "queues": [
                {"name": "unheld_progress", "status": "ok", "actionable": true,
                 "verb": "/fno:target", "rows": [{"id": "x-1"}]},
                {"name": "undriven_pr", "status": "ok", "actionable": true,
                 "verb": "/fno:target", "rows": [{"number": 2398}]},
                {"name": "unplanned", "status": "unreadable", "actionable": true,
                 "error": "truth probe timed out", "rows": []},
            ],
        }))
        .unwrap();
        let verdict = saturation_verdict(&board, Some(&refused_probe())).expect("saturated blind");
        match verdict {
            SaturationOutcome::SaturatedBlind { blocked } => assert_eq!(blocked, 2),
            _ => panic!("expected a blind saturation verdict"),
        }
        let tmp = tempfile::tempdir().unwrap();
        let fno = gate_status_stub(tmp.path());
        let emit = |_: &str, _: Value| {};
        let gate = capacity_gate(&board, fno.to_str().unwrap(), tmp.path(), "king", 0, &emit)
            .expect("the blind saturation remains a gate result");
        match gate {
            CapacityGate::SaturatedBlind {
                message,
                actionable,
            } => {
                assert_eq!(actionable, 2);
                assert!(
                    message.contains("not read: unplanned is unreadable"),
                    "{message}"
                );
                assert!(message.contains("live workers exiting"), "{message}");
            }
            _ => panic!("expected blind saturation to stay distinct from NoWork"),
        }
    }

    #[test]
    fn a_fleet_stop_probe_names_the_incident_owner() {
        let probe = GateProbe {
            verdict: "refused".into(),
            reason: Some("fleet-stop".into()),
            message: None,
        };
        let owner = probe.owner();
        assert!(owner.contains("fno agents incident status"), "{owner}");
        assert!(
            owner.contains("fno agents incident clear --reason"),
            "{owner}"
        );
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

    #[test]
    fn the_manifest_ceiling_breaches_budget_on_the_advertised_fire() {
        let b = bound_breached(39, 0, 40, "3 scope nodes still undelivered").expect("breach");
        assert_eq!(b.reason, TerminationReason::Budget);
        assert!(
            b.message.contains("ceiling of 40")
                && b.message.contains("3 scope nodes still undelivered"),
            "{}",
            b.message
        );
        assert_eq!(b.fires, 0);
    }

    #[test]
    fn one_fire_below_the_manifest_ceiling_still_blocks() {
        assert!(bound_breached(38, 0, 40, "waiting").is_none());
    }

    #[test]
    fn the_dry_backstop_breaches_noprogress_on_the_third_quiet_fire() {
        let b = bound_breached(0, 2, 40, "waiting").expect("breach");
        assert_eq!(b.reason, TerminationReason::NoProgress);
        assert_eq!(b.fires, 3);
    }

    #[test]
    fn budget_is_reported_when_both_bounds_breach_on_one_fire() {
        let b = bound_breached(2, 2, 3, "waiting").expect("breach");
        assert_eq!(b.reason, TerminationReason::Budget);
    }

    #[test]
    fn a_budget_breach_is_an_escalation_reason_now() {
        // The terminate closure escalates on NoProgress OR Budget.
        // The reason tag the closure hands escalate_stalled is the enum's
        // own name, so the question says Budget when Budget stopped it.
        let b = bound_breached(40, 0, 40, "waiting").expect("breach");
        assert_eq!(format!("{:?}", b.reason), "Budget");
    }

    #[test]
    fn the_quiet_board_reading_id_shape_is_stable() {
        // The quiet-board terminal escalates this one reading id; its
        // stability is what keeps reconcile at one question per crown, not
        // one per count. The mint lives in king_escalation.
        let scope = "x-aaaa";
        let row = crate::king_escalation::reading_undelivered(scope);
        assert_eq!(row, "reading:undelivered:x-aaaa");
    }
}
