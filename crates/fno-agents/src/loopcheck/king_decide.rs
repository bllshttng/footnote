//! The king's decision path (`--driver king`): the terminal short-circuit,
//! the watching lease, board reads, and the budget ceiling - split beside the
//! other loopcheck children so `loopcheck.rs` keeps shrinking.

use super::*;

pub(super) fn king_decide(parsed: &LoopCheckArgs) -> (i32, String) {
    // The drain reserve arms HERE, on the shared king entry, and not at the
    // fire stamp: both king routes (the `--driver king` hook and the bound
    // Crown row) converge on this function, so the route into it, not the
    // driver string the fire was stamped with, decides who pays for the
    // drain. A missing manifest allows right after the hold - an uncrowned
    // session pays nothing.
    super::stopgate_hold_drain_reserve();
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
    if let Some((gate_reading, gate_message)) = stale_crown_doc_gate(
        &manifest,
        &parsed.transcript_path,
        &parsed.cwd,
        &parsed.fno_bin,
        std::time::SystemTime::now(),
    ) {
        return blind_block(&gate_reading, &gate_message, 0, dry);
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
            return terminate(
                TerminationReason::NoWork,
                "waiting on the user: an operator question this reign raised is open; the answer wakes the king",
                0,
                dry,
                &[],
            );
        }
        if board.unreadable_sources {
            return blind_block(
                &crate::king_escalation::reading_sources_unreadable(),
                "board quiet but some sources are unreadable; blocking completion",
                0,
                dry,
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
        if drain_error.is_none() {
            return terminate(
                TerminationReason::NoWork,
                &format!(
                    "waiting on CI or a worker: {undelivered} driven rows undelivered; a quiet beat, not a finish line"
                ),
                0,
                dry,
                &[],
            );
        }
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

/// The stale-crown-doc gate: past the compaction ceiling, a crown handoff doc
/// that is gone or 24h stale means the king acts on a snapshot nothing has
/// refreshed. Claude-only: the boundary count is measured on claude
/// transcripts, and a harness whose transcript carries no `compact_boundary`
/// line counts zero, so the gate stays silent there by design. Fail-open on
/// anything it cannot measure: it blocks exit, and a false block traps a
/// session for a fact it cannot see.
fn stale_crown_doc_gate(
    manifest: &KingManifest,
    transcript: &Path,
    cwd: &Path,
    fno_bin: &str,
    now: std::time::SystemTime,
) -> Option<(String, String)> {
    if manifest.harness.as_deref().unwrap_or("claude") != "claude" {
        return None;
    }
    let scope = manifest.scope.trim();
    if scope.is_empty() {
        return None;
    }
    let ceiling = match crate::agents_config::config_lookup(cwd, &["king", "compaction_ceiling"]) {
        Some(v) => v
            .as_integer()
            .filter(|n| *n >= 0)
            .unwrap_or(crate::king_verdict_inputs::DEFAULT_COMPACTION_CEILING),
        None => crate::king_verdict_inputs::DEFAULT_COMPACTION_CEILING,
    } as u64;
    // Count first: a scan of the transcript this fire already holds. The doc
    // resolution (a subprocess) is paid only past the ceiling, so the common
    // path adds no cost to the board read.
    let crown_start = manifest
        .created_at
        .as_deref()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())?;
    let boundaries =
        crate::compaction::count_boundaries_since(transcript, Some(crown_start.timestamp()))
            .ok()?;
    if boundaries <= ceiling {
        return None;
    }
    // The handoffs-dir resolver lives only in the Python CLI; shell it rather
    // than copy it, then pick newest + mtime with the same resolver
    // king_checkin uses, so the two cannot drift.
    let out = std::process::Command::new(fno_bin)
        .args(["config", "paths", "handoff", "--scope", scope])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let printed = String::from_utf8_lossy(&out.stdout);
    let printed = printed.lines().next()?.trim();
    if printed.is_empty() {
        return None;
    }
    let dir = std::path::Path::new(printed).parent()?;
    let doc = match crate::king_checkin::crown_handoff_doc(dir, scope) {
        Ok(doc) => doc,
        Err(_) => return Some(stale_doc_block(scope, ceiling, boundaries, None)),
    };
    let age_secs = match std::fs::metadata(&doc).and_then(|m| m.modified()) {
        Ok(mtime) => now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0),
        // An unstattable doc cannot prove itself fresh; treat it as ancient,
        // the same direction the resolver's own UNIX_EPOCH floor takes.
        Err(_) => u64::MAX,
    };
    if age_secs <= crate::king_term::STALE_CROWN_DOC_MAX_AGE_SECS as u64 {
        return None;
    }
    Some(stale_doc_block(
        scope,
        ceiling,
        boundaries,
        Some(age_secs / 3600),
    ))
}

fn stale_doc_block(
    scope: &str,
    ceiling: u64,
    boundaries: u64,
    age_hours: Option<u64>,
) -> (String, String) {
    let age = match age_hours {
        Some(h) => format!("{h}h old"),
        None => "missing".to_string(),
    };
    (
        crate::king_escalation::reading_stale_crown_doc(),
        format!(
            "the crown's handoff doc for {scope} is {age} and this reign is past its compaction ceiling ({boundaries} > {ceiling}); refresh it: bash \"$PLUGIN_ROOT/hooks/precompact-canon-doc.sh\" < /dev/null"
        ),
    )
}

#[cfg(test)]
mod stale_crown_doc_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    const CROWN_START: &str = "2026-09-15T00:00:00Z";

    #[test]
    fn king_decide_holds_the_drain_reserve_on_entry() {
        // The Crown route enters the king path under a fire stamped reserve 0
        // (the entry stamp is driver-blind now). The hold on the first line
        // is what arms the drain slice on BOTH king routes; a missing
        // manifest lets the call return at its first read with the hold
        // already landed.
        let dir = tempfile::tempdir().unwrap();
        let parsed = LoopCheckArgs {
            state_path: dir.path().join("no-such-king.md"),
            transcript_path: dir.path().join("t.jsonl"),
            cwd: dir.path().to_path_buf(),
            global_settings_path: None,
            events_path: Some(dir.path().join("events.jsonl")),
            global_events_path: Some(dir.path().join("global-events.jsonl")),
            settings_path: None,
            ledger_path: None,
            gh_budget_ledger: None,
            now_override: None,
            gh_bin: "/nonexistent-gh".into(),
            git_bin: "/nonexistent-git".into(),
            author_harness_override: Some("none".into()),
            hook_input_stdin: false,
            driver: "target".into(),
            fno_bin: "/nonexistent-fno".into(),
            read_timeout_ms: None,
            harness: None,
            harness_session: None,
        };
        super::stopgate_stamp_fire(0, std::time::Instant::now() + Duration::from_secs(50), 0);
        let (code, out) = king_decide(&parsed);
        assert_eq!(code, 0);
        assert!(out.contains("no king manifest"), "{out}");
        // The reserve is armed despite the driver-blind stamp: pre-drain
        // reads clamp to remaining-minus-reserve, the drain reads it whole.
        let pre_drain = super::stopgate_read_timeout();
        assert!(
            pre_drain <= Duration::from_secs(34) && pre_drain >= Duration::from_secs(33),
            "{pre_drain:?}"
        );
        let drain = super::stopgate_drain_timeout();
        assert!(
            drain <= Duration::from_secs(50) && drain >= Duration::from_secs(49),
            "{drain:?}"
        );
    }

    fn manifest(harness: &str) -> KingManifest {
        KingManifest {
            scope: "footnote".into(),
            created_at: Some(CROWN_START.into()),
            harness: Some(harness.into()),
            ..Default::default()
        }
    }

    fn write_transcript(dir: &Path, name: &str, boundaries: usize) -> PathBuf {
        let path = dir.join(name);
        let mut body = String::new();
        for i in 0..boundaries {
            body.push_str(&format!(
                "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"timestamp\":\"2026-09-16T0{i}:00:00Z\"}}\n"
            ));
        }
        body.push_str("{\"type\":\"user\",\"message\":\"tail\"}\n");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The CLI stub answers `config paths handoff --scope` with a path inside
    /// `handoffs_dir`, the way the real verb prints the scope's newest doc.
    fn stub_fno(dir: &Path, handoffs_dir: &Path) -> String {
        let stub = dir.join("fno-stub.sh");
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\n",
                handoffs_dir.join("unused-crown-footnote.md").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        stub.to_string_lossy().into_owned()
    }

    fn seed_doc(handoffs_dir: &Path) {
        std::fs::create_dir_all(handoffs_dir).unwrap();
        std::fs::write(handoffs_dir.join("20260916-crown-footnote.md"), "# canon").unwrap();
    }

    #[test]
    fn stale_doc_past_ceiling_blocks_and_names_the_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_transcript(tmp.path(), "t.jsonl", 4);
        let handoffs = tmp.path().join("handoffs");
        seed_doc(&handoffs);
        let fno = stub_fno(&tmp.path().join("bin"), &handoffs);
        let now = std::time::SystemTime::now() + Duration::from_secs(30 * 3600);
        let gate = stale_crown_doc_gate(&manifest("claude"), &transcript, tmp.path(), &fno, now);
        let (reading, message) = gate.expect("gate must block on a 30h-old doc");
        assert_eq!(reading, crate::king_escalation::reading_stale_crown_doc());
        assert!(message.contains("precompact-canon-doc.sh"), "{message}");
        assert!(message.contains("4 > 3"), "{message}");
    }

    #[test]
    fn fresh_doc_past_ceiling_allows_the_board_read() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_transcript(tmp.path(), "t.jsonl", 4);
        let handoffs = tmp.path().join("handoffs");
        seed_doc(&handoffs);
        let fno = stub_fno(&tmp.path().join("bin"), &handoffs);
        let now = std::time::SystemTime::now() + Duration::from_secs(3600);
        assert!(
            stale_crown_doc_gate(&manifest("claude"), &transcript, tmp.path(), &fno, now).is_none()
        );
    }

    #[test]
    fn no_compaction_boundary_stays_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_transcript(tmp.path(), "t.jsonl", 0);
        let handoffs = tmp.path().join("handoffs");
        seed_doc(&handoffs);
        let fno = stub_fno(&tmp.path().join("bin"), &handoffs);
        let now = std::time::SystemTime::now() + Duration::from_secs(30 * 3600);
        assert!(
            stale_crown_doc_gate(&manifest("claude"), &transcript, tmp.path(), &fno, now).is_none()
        );
    }

    #[test]
    fn non_claude_harness_never_fires() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_transcript(tmp.path(), "t.jsonl", 4);
        let handoffs = tmp.path().join("handoffs");
        seed_doc(&handoffs);
        let fno = stub_fno(&tmp.path().join("bin"), &handoffs);
        let now = std::time::SystemTime::now() + Duration::from_secs(30 * 3600);
        assert!(
            stale_crown_doc_gate(&manifest("codex"), &transcript, tmp.path(), &fno, now).is_none()
        );
    }

    #[test]
    fn missing_doc_past_ceiling_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_transcript(tmp.path(), "t.jsonl", 4);
        let handoffs = tmp.path().join("empty-handoffs");
        std::fs::create_dir_all(&handoffs).unwrap();
        let fno = stub_fno(&tmp.path().join("bin"), &handoffs);
        let now = std::time::SystemTime::now() + Duration::from_secs(3600);
        let (reading, message) =
            stale_crown_doc_gate(&manifest("claude"), &transcript, tmp.path(), &fno, now)
                .expect("a missing doc past the ceiling must block");
        assert_eq!(reading, crate::king_escalation::reading_stale_crown_doc());
        assert!(message.contains("missing"), "{message}");
    }

    #[test]
    fn ceiling_reached_but_not_exceeded_allows() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_transcript(tmp.path(), "t.jsonl", 3);
        let handoffs = tmp.path().join("handoffs");
        seed_doc(&handoffs);
        let fno = stub_fno(&tmp.path().join("bin"), &handoffs);
        let now = std::time::SystemTime::now() + Duration::from_secs(30 * 3600);
        assert!(
            stale_crown_doc_gate(&manifest("claude"), &transcript, tmp.path(), &fno, now).is_none()
        );
    }

    #[test]
    fn future_mtime_reads_as_fresh_not_ancient() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_transcript(tmp.path(), "t.jsonl", 4);
        let handoffs = tmp.path().join("handoffs");
        seed_doc(&handoffs);
        // A clock stepped back after the doc's write leaves its mtime in the
        // future; the doc IS fresh, so the gate must not read it as ancient.
        let future = std::time::SystemTime::now() + Duration::from_secs(3600);
        let doc = handoffs.join("20260916-crown-footnote.md");
        let mut f = std::fs::File::options().write(true).open(&doc).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(future))
            .unwrap();
        f.flush().unwrap();
        drop(f);
        let fno = stub_fno(&tmp.path().join("bin"), &handoffs);
        let now = std::time::SystemTime::now();
        assert!(
            stale_crown_doc_gate(&manifest("claude"), &transcript, tmp.path(), &fno, now).is_none()
        );
    }
}
