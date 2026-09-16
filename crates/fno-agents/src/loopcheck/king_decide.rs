//! The king's decision path (`--driver king`): the terminal short-circuit,
//! the watching lease, board reads, and the budget ceiling - split beside the
//! other loopcheck children so `loopcheck.rs` keeps shrinking.

use super::*;

pub(super) fn king_decide(parsed: &LoopCheckArgs) -> (i32, String) {
    // A missing manifest is the only safe silent allow, exactly as on the
    // target path: a session nobody crowned is not a king, and blocking one
    // would trap every ordinary session here.
    let Ok(content) = std::fs::read_to_string(&parsed.state_path) else {
        return (
            0,
            king_output("allow", None, "no king manifest; allowing exit", 0, 0),
        );
    };
    let Some(manifest) = parse_king_manifest(&content) else {
        eprintln!("loop-check: corrupt king manifest (no frontmatter)");
        return (
            0,
            king_output("allow", None, "corrupt king manifest; allowing exit", 0, 0),
        );
    };

    let project_events = parsed
        .events_path
        .clone()
        .unwrap_or_else(|| crate::paths::events_path(&parsed.cwd));
    let global_events = parsed
        .global_events_path
        .clone()
        .unwrap_or_else(|| project_events.clone());
    let session_id = std::env::var(crate::loop_king::WALK_SESSION_KEY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| manifest.fno_id.clone());
    // A walk-spawned pass tags its terminal with the per-invocation key so
    // the walk's resume guard never sees a PRIOR reign's terminal; every
    // other reader still filters on the driver tag, so both spellings agree.
    let emit = |event_type: &str, data: serde_json::Value| {
        emit_to_both(&project_events, &global_events, event_type, data);
    };
    // Every NoProgress or Budget terminal escalates, in the shared closure: a
    // king that quits with work pending is this feature's own failure, and
    // 15 Budget ceiling hits once told nobody because only one terminal did.
    let terminate = |reason: TerminationReason,
                     message: &str,
                     actionable: i64,
                     fires: u64,
                     stalled: &[String]| {
        let mut message = message.to_string();
        if matches!(
            reason,
            TerminationReason::NoProgress | TerminationReason::Budget
        ) {
            let outcome = crate::loop_king::escalate_stalled(
                &parsed.fno_bin,
                &parsed.cwd,
                stalled,
                &format!("{reason:?}"),
                &manifest.scope,
            );
            message = format!("{message}; {outcome}");
        }
        emit(
            "termination",
            serde_json::json!({
                "session_id": session_id,
                "driver": "king",
                "reason": format!("{reason:?}"),
                "message": message,
            }),
        );
        (
            0,
            king_output("allow", Some(reason), &message, actionable, fires),
        )
    };

    if let Some(hit) = check_cancel_sentinel(
        &parsed.cwd,
        &parsed.state_path,
        &manifest.created_at,
        "king",
    ) {
        return terminate(
            TerminationReason::Interrupted,
            &format!("cancel sentinel present{}", hit.attribution()),
            0,
            0,
            &[],
        );
    }

    let history = crate::loop_king::king_fire_history(&project_events, &session_id);
    // The hook's half of the reign record: a beat the model skipped still
    // lands a row. Sits after the cancel-sentinel check, so a cancelled crown
    // writes none. The return value is ignored, so no decision changes.
    crate::king_checkin::hook_beat(
        &project_events,
        &parsed.cwd,
        &manifest.scope,
        &session_id,
        &history,
        chrono::Utc::now(),
    );


    // A reign that already ended does not read the board again :
    // the journal's newest king termination row for this session, when newer
    // than the manifest's created_at, IS the verdict. 675 of 728 king Stop
    // fires in the operator's 15-day session were repeat terminals; each paid
    // a full board read (one measured 40,776ms against a 30,000ms budget) to
    // reach a verdict the journal already held. No new termination row, no
    // escalation, no board read.
    if let (Some((ts, reason)), Some(created_at)) = (
        &history.last_terminal,
        manifest
            .created_at
            .as_deref()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok()),
    ) {
        if let Ok(terminal_at) = ts.parse::<DateTime<Utc>>() {
            if terminal_at > created_at {
                return (
                    0,
                    king_output(
                        "allow",
                        None,
                        &format!("reign already terminal ({reason} at {ts}); re-arm with fno agents king init"),
                        0,
                        history.total,
                    ),
                );
            }
        }
    }

    // Budget before the board : the total-fires half of
    // bound_breached needs no board evidence, so it runs before the read; the
    // dry half stays after, because a cleared row resets it. The waiting text
    // reuses the last journal-recorded undelivered count so a repeat-ceiling
    // terminal says the same thing the board-reading path said.
    if history.total + 1 >= manifest.max_iterations {
        let waiting = match history.last_undelivered {
            Some(n) if n != i64::MAX => format!("{n} scope nodes still undelivered"),
            _ => "the board was not read".to_string(),
        };
        return terminate(
            TerminationReason::Budget,
            &format!(
                "{} fires reached the manifest ceiling of {}; {waiting}",
                history.total + 1,
                manifest.max_iterations
            ),
            0,
            history.dry,
            &[],
        );
    }
    let dry = history.dry;
    // The bounds every block below owes, in one place, so no branch grows its own.
    let bounded = |dry: u64, waiting: &str| {
        bound_breached(history.total, dry, manifest.max_iterations, waiting)
    };
    let (reading, term_json) = crate::king_term::current_reading(&manifest);
    let emit_term = |body| crate::king_term::emit_journal(&emit, &term_json, body);
    // Shared spine of both blind-board blocks: bounded, quiet emit, block.
    // The reading is what the branch measured, never a guess.
    let blind_block = |reading: &str, message: &str, actionable: i64, dry: u64| -> (i32, String) {
        if let Some(b) = bounded(dry, message) {
            return terminate(b.reason, &b.message, 0, b.fires, &[reading.to_owned()]);
        }
        emit_term(king_quiet_body(&session_id, actionable));
        (0, king_output("block", None, message, actionable, dry + 1))
    };

    if let Some(gate) =
        crate::king_termination::stand_down_gate(&manifest, &parsed.transcript_path, &parsed.cwd)
    {
        return blind_block(&gate.reading, &gate.message, 0, dry);
    }
    if let Some(result) = crate::king_term::gate(&reading, &manifest.scope, dry, &blind_block) {
        return result;
    }

    let board = match read_king_board(&parsed.fno_bin, &parsed.cwd, &parsed.state_path) {
        Ok(b) => b,
        Err(e) => {
            // Blind is not clean. Block on exit 2, but bounded: a board
            // that never answers still reaches a ceiling. The exit code
            // marks the degraded path; the shim keys on the decision field.
            if let Some(b) = bounded(dry, &format!("king board unreadable: {e}")) {
                let reading = crate::king_escalation::reading_board_unreadable();
                return terminate(b.reason, &b.message, 0, b.fires, &[reading]);
            }
            emit(
                "king_loop_check",
                serde_json::json!({
                    "session_id": session_id,
                    "board_error": e,
                }),
            );
            return (
                2,
                king_output(
                    "block",
                    None,
                    &format!("king board unreadable: {e}"),
                    0,
                    dry + 1,
                ),
            );
        }
    };

    if board.actionable == 0 {
        if board.operator_questions_unreadable {
            // Bounded, and each blocking fire emits its row so the counters advance.
            return blind_block(
                &crate::king_escalation::reading_questions_unreadable(),
                "board clean but outstanding operator questions are unreadable; blocking completion",
                0,
                dry,
            );
        }
        let open_question = board
            .operator_question_sessions
            .iter()
            .any(|owner| owner == &session_id);
        if open_question {
            return (
                0,
                king_output(
                    "block",
                    None,
                    "board clean but an operator question raised by this reign remains open",
                    0,
                    dry,
                ),
            );
        }
        // The goal keys completion on the crown draining, not on any queue
        // (2026-09-06 ruling): a board clean while driven-but-unshipped rows
        // sit is a quiet beat, never a finish line. An unreadable drain read
        // must not certify drained; the dry-fire ceiling bounds the wait.
        // Timeout and command failure demand opposite responses: both named.
        let (undelivered, drain_error) = if manifest.scope.is_empty() {
            (0, None)
        } else {
            crate::loop_king::scope_undelivered_with_reserve_watch(
                &parsed.fno_bin,
                &parsed.cwd,
                &manifest.scope,
                &session_id,
                &emit,
            )
        };
        if undelivered == 0 {
            //: a floor count cannot see blind queues; refuse to certify.
            if board.unreadable_sources {
                return blind_block(
                    &crate::king_escalation::reading_sources_unreadable(),
                    "board quiet but some sources are unreadable; blocking completion",
                    0,
                    dry,
                );
            }
            return terminate(
                TerminationReason::NoWork,
                "board clean; exiting NoWork",
                0,
                dry,
                &[],
            );
        }
        let message =
            crate::king_termination::king_quiet_message(undelivered, drain_error.as_ref());
        let shrank = undelivered != i64::MAX
            && history
                .last_undelivered
                .is_some_and(|prev| undelivered < prev);
        let dry = if shrank { 0 } else { dry };
        let reading = match &drain_error {
            Some(_) => crate::king_escalation::reading_delivery_unreadable(&manifest.scope),
            None => crate::king_escalation::reading_undelivered(&manifest.scope),
        };
        emit(
            "king_loop_check",
            crate::king_termination::king_undelivered_body(&session_id, undelivered, shrank),
        );
        if let Some(b) = bounded(dry, &message) {
            return terminate(b.reason, &b.message, 0, b.fires, &[reading]);
        }
        return (0, king_output("block", None, &message, 0, dry + 1));
    }

    // A row the previous fire called actionable and this one does not is work
    // the king cleared - the progress signal, read off the board; applied
    // BEFORE the bound, so a clearing fire is judged on its post-reset streak.
    let cleared = crate::loop_king::king_cleared_a_row(&history.last_ids, &board.actionable_ids);
    let dry = if cleared { 0 } else { dry };

    // The bounds `--max-iterations` advertises, checked after NoWork so a
    // clean board still exits clean; Budget before NoProgress names what
    // actually stopped it.
    let waiting = format!("{} rows still actionable", board.actionable);
    if let Some(b) = bounded(dry, &waiting) {
        // An actionable floor with no readable rows is the partially-blind
        // board: it names its reading, never an empty set.
        let stalled = if board.actionable_ids.is_empty() {
            vec![crate::king_escalation::reading_board_unreadable()]
        } else {
            board.actionable_ids.clone()
        };
        return terminate(b.reason, &b.message, board.actionable, b.fires, &stalled);
    }

    match crate::king_termination::capacity_gate(
        &board,
        &parsed.fno_bin,
        &parsed.cwd,
        &session_id,
        dry,
        &emit,
    ) {
        Some(crate::king_termination::CapacityGate::Saturated {
            message,
            blocked,
            fires,
        }) => {
            return terminate(TerminationReason::NoWork, &message, blocked, fires, &[]);
        }
        Some(crate::king_termination::CapacityGate::Split {
            message,
            actionable,
            fires,
            journal,
        }) => {
            emit("king_loop_check", journal);
            return (0, king_output("block", None, &message, actionable, fires));
        }
        None => {}
    }

    emit(
        "king_loop_check",
        serde_json::json!({
            "session_id": session_id,
            "actionable": board.actionable,
            "actionable_ids": board.actionable_ids,
            // Durable, because the dry-fire counter is rebuilt from this
            // journal on every fire. A reset that lived only in the local
            // binding was forgotten the moment this process exited.
            "cleared": cleared,
        }),
    );
    let top = board
        .top_row
        .unwrap_or_else(|| "an actionable queue".to_string());
    // decide()'s documented contract, one screen up: exit 0 for allow and for
    // this healthy block; the ONLY other verdict-bearing exit is the degraded
    // unreadable-board block above, which carries the same payload on 2. Any
    // non-zero without a `decision` field is an internal/CLI error. Encoding a
    // healthy block in the exit code made the shim read it as a broken checker
    // and count it toward the unavailable budget that ends in a ship-gate-off
    // allow. The JSON `decision` field is the block signal; the exit code
    // never is.
    (
        0,
        king_output(
            "block",
            None,
            &format!("{} actionable; next: {top}", board.actionable),
            board.actionable,
            dry + 1,
        ),
    )
}
