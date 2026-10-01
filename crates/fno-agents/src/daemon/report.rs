//! The `agent.report` store: inside-leg ingestion, the early-push buffer, and
//! the null-id backfill. Split out of daemon.rs (shrink-only) so report-side
//! additions land here, never back in the daemon file.

use super::*;

/// `agent.report` — the inside-leg state push (inside-out E3.2). A per-turn hook
/// calls `fno agents report --session-id <uuid> --seq <n> --state
/// working|blocked|done [--reason ...] [--ttl-ms <n>]`; the daemon stamps
/// `received_at` and STORES the report on the matching registry row's
/// [`RegistryEntry::inside_leg`] field (contract v2 / X2). Storage-only: the
/// seq-drop (a `seq <= last_seq` is rejected so a reordered/duplicate report
/// cannot clobber a newer one, AC-X2-1) and the unknown-session drop (no phantom
/// row, AC-X2-5) live here; TTL-aging, the 3-tier render authority, and the
/// ordered exit teardown are E3.3. The row is matched by the daemon-pinned
/// session id via [`entry_holds_session`], so a claude pane reports under the
/// same UUID E1 recorded. A DROP is non-fatal: an unregistered session (the row
/// not up yet) or a stale seq returns `ok` with `stored:false`, so the hook stays
/// fire-and-forget and never reds a turn.
/// Outcome of trying to buffer an early-push inside-leg report (E3.3).
pub(crate) enum BufferOutcome {
    /// Held in the pending buffer until the row registers.
    Buffered,
    /// A reordered/duplicate early push (`seq <= buffered seq`); dropped.
    StaleSeq { last: u64 },
    /// The buffer is at cap and this is a new session; dropped (logged).
    Full,
}

/// Insert an early-push report into the bounded pending buffer, highest-seq-wins
/// per session (a reorder cannot regress a buffered report, the same seq rule the
/// registered path enforces). Pure over the map so it is unit-testable without a
/// daemon (inside-out E3.3, buffer-on-early-push).
pub(crate) fn buffer_pending_report(
    map: &mut std::collections::HashMap<String, state::InsideLegReport>,
    session_id: &str,
    report: state::InsideLegReport,
) -> BufferOutcome {
    if let Some(prev) = map.get(session_id) {
        if report.seq <= prev.seq {
            return BufferOutcome::StaleSeq { last: prev.seq };
        }
        map.insert(session_id.to_string(), report);
        return BufferOutcome::Buffered;
    }
    if map.len() >= PENDING_INSIDE_LEG_CAP {
        return BufferOutcome::Full;
    }
    map.insert(session_id.to_string(), report);
    BufferOutcome::Buffered
}

/// Flush a buffered early-push report onto its session's row AFTER the row is
/// registered (E3.3 flush).
///
/// Called only on a winning insert with the row's pinned claude session uuid.
/// Takes the buffered report out of the pending map (highest-seq, since
/// `buffer_pending_report` keeps only the newest) and applies it to the row
/// under a seq gate, so a report that raced in on the row's *store* path between
/// insert and this drain is never regressed (codex P2: highest-seq-wins must
/// survive the flush). Draining strictly after the insert closes the
/// peek-then-commit window where a newer buffered report could be deleted by an
/// unconditional remove. A no-op for a row with no buffered report; a poisoned
/// lock leaves the report buffered.
pub(super) fn flush_buffered_inside_leg(ctx: &Ctx, session_uuid: &str, name: &str) {
    let rep = match ctx.pending_inside_leg.lock() {
        Ok(mut buf) => buf.remove(session_uuid),
        Err(_) => None,
    };
    let Some(rep) = rep else {
        return;
    };
    let (seq, state_str) = (rep.seq, inside_leg_state_str(rep.state));
    let mut notify: Option<(String, String, bool)> = None;
    // Apply under the seq gate: a store-path report that landed on the row after
    // it became visible (but before this drain) set a >= seq; never regress it.
    let _ = state::update_registry(&ctx.home.registry_json(), |r| {
        if let Some((body, is_done)) = gate_inside_leg_onto_row(r, session_uuid, rep.clone()) {
            notify = Some((name.to_string(), body, is_done));
        }
    });
    if let Some((title, body, is_done)) = notify {
        let o = &ctx.opts;
        notify_badge(title, body, is_done, o.notify_on_blocked, o.notify_on_done);
    }
    let _ = ctx.emitter.emit(
        "inside_leg_buffer_flushed",
        &json!({"name": name, "session_id": session_uuid, "state": state_str, "seq": seq}),
    );
}

/// Which null-uuid row (if any) should adopt a full session uuid seen on an
/// inside-leg report.
pub(crate) enum UuidBackfill {
    None,
    One(usize),
    Ambiguous,
}

/// Find the row awaiting a full session uuid seen on an inside-leg report.
///
/// Two null-id shapes backfill here. A `claude --bg` spawn writes the row
/// with the 8-hex jobId in `short_id` (v9) but `claude_session_uuid: null` --
/// the full uuid only arrives on the first inside-leg report, so until it is
/// backfilled `entry_holds_session` never matches and every report is
/// buffered-then-lost. It matches when the short-id is the leading hex group
/// of `full_uuid` (`3228ccad` -> `3228ccad-c078-...`). An adopted codex row
/// (kestrel shape) carries no session id at all -- only its rollout path --
/// so it matches when the rollout stem's FULL thread id equals `full_uuid`
/// (never a prefix: a shorter hex run must not adopt a longer id). Two rows
/// matching either way is ambiguous -> refuse rather than backfill the wrong
/// row (AC1-ERR).
pub(crate) fn find_uuid_backfill_row(entries: &[RegistryEntry], full_uuid: &str) -> UuidBackfill {
    let mut found = None;
    for (i, e) in entries.iter().enumerate() {
        // A claude bg row owns a jobId + uuid identity; a codex row owns a
        // rollout-thread identity. Anything else (or a row that already holds
        // its id) is not backfillable, so a malformed foreign row can't adopt
        // a session id that is not its own.
        match e.harness_name() {
            "claude" if e.claude_session_uuid.is_none() => {
                let Some(short) = e.transport_short() else {
                    continue;
                };
                // Require the group boundary (`<short>-`) so a short cannot
                // match a longer hex run it merely prefixes.
                if short.is_empty()
                    || !full_uuid
                        .strip_prefix(short)
                        .is_some_and(|rest| rest.starts_with('-'))
                {
                    continue;
                }
            }
            "codex" if e.codex_session_id.is_none() && e.session_id.is_none() => {
                // The rollout path is the only join an id-less codex row
                // carries; its stem's trailing thread id must EQUAL the
                // reported id. A row whose rollout path is missing or not
                // rollout-shaped never matches.
                let Some(tid) = e.log_path.as_deref().and_then(codex_rollout_thread_id) else {
                    continue;
                };
                if tid != full_uuid {
                    continue;
                }
            }
            _ => continue,
        }
        if found.is_some() {
            return UuidBackfill::Ambiguous;
        }
        found = Some(i);
    }
    found.map_or(UuidBackfill::None, UuidBackfill::One)
}

/// The codex thread id a rollout path names: the trailing 36-char uuid of the
/// file stem (`rollout-<ts>-<uuid>.jsonl`). `None` when the path carries no
/// stem-shaped uuid, so a plain transcript path can never act as a join.
fn codex_rollout_thread_id(path: &str) -> Option<String> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    if !stem.starts_with("rollout-") {
        return None;
    }
    let tid = crate::provenance::rollout_session_id(stem);
    (tid != stem).then_some(tid)
}

/// `"<sandbox>:<approval>"` -> the observed posture the report stores. `None`
/// on any other shape: a posture the daemon cannot parse must never store as
/// a real one.
fn parse_posture(raw: &str) -> Option<state::ObservedPosture> {
    let (sandbox, approval) = raw.split_once(':')?;
    let (sandbox, approval) = (sandbox.trim(), approval.trim());
    (!sandbox.is_empty() && !approval.is_empty())
        .then(|| state::ObservedPosture::observed(sandbox, approval))
}

pub(super) fn handle_report(ctx: &Ctx, req: &Request) -> Response {
    let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Response::err(req.id, ErrorCode::InvalidParams, "missing `session_id`"),
    };
    let seq = match req.params.get("seq").and_then(|v| v.as_u64()) {
        Some(n) => n,
        None => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "missing or non-integer `seq`",
            )
        }
    };
    // Validate against the wire vocabulary; keep the label for the event payload
    // and map to the typed enum for storage. `model` is the
    // PostModelSwitch posture: no inside-leg transition, the report only
    // diffs the row's SERVED model/effort axes, and it must carry at least
    // one of them.
    let state_label = match req.params.get("state").and_then(|v| v.as_str()) {
        Some(s @ ("working" | "blocked" | "done" | "model")) => s.to_string(),
        _ => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "`state` must be working|blocked|done|model",
            )
        }
    };
    let model_only = state_label == "model";
    let model = req
        .params
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let effort = req
        .params
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    if model_only && model.is_none() && effort.is_none() {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "state=model requires `model` or `effort`",
        );
    }
    // The observed sandbox posture, `"<sandbox>:<approval>"` on the wire.
    // A `model` report carries no inside-leg transition to store a posture
    // on, so one arriving there is refused: refusing beats silently
    // dropping the caller's data.
    let posture = req
        .params
        .get("posture")
        .and_then(|v| v.as_str())
        .and_then(parse_posture);
    if req.params.get("posture").is_some() && posture.is_none() {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "`posture` must be `<sandbox>:<approval>`",
        );
    }
    if model_only && posture.is_some() {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "posture requires state working|blocked|done",
        );
    }
    let state = match state_label.as_str() {
        "working" => Some(state::InsideLegState::Working),
        "blocked" => Some(state::InsideLegState::Blocked),
        "done" => Some(state::InsideLegState::Done),
        _ => None,
    };
    let reason = req
        .params
        .get("reason")
        .and_then(|v| v.as_str())
        .map(String::from);
    let ttl_ms = req.params.get("ttl_ms").and_then(|v| v.as_u64());

    // Build the report once; a clone moves into the locked store path, the
    // original is reused for the early-push buffer when no row exists yet.
    // `None` under the model posture: there is no transition to store.
    let report = state.map(|state| state::InsideLegReport {
        state,
        seq,
        reason,
        received_at: now_rfc3339_like(),
        ttl_ms,
        posture,
    });
    let report_for_store = report.clone();

    // The store/drop decision is made UNDER the registry flock so two concurrent
    // reporters on one session id can't both pass the seq gate.
    enum Outcome {
        Stored,
        StaleSeq { last: u64 },
        Unknown,
    }
    let mut outcome = Outcome::Unknown;
    // Badge-transition notify intent: (title, body, is_done). Captured
    // UNDER the flock from prev-vs-new state; fired AFTER the write so a slow
    // notifier can never stall ingestion.
    let mut notify: Option<(String, String, bool)> = None;
    // The row's label, captured under the flock for the axis-change
    // events emitted after the write.
    let mut entry_name: Option<String> = None;
    // Served-axis change records captured under the flock, emitted
    // after the write: (kind, from, to). `requested_*` are never touched -
    // they stay the spawn request, which is the provenance.
    let mut axis_changes: Vec<(&str, Option<String>, String)> = Vec::new();
    if let Err(e) = state::update_registry(&ctx.home.registry_json(), |r| {
        // Match by the pinned session id (fast path). If nothing holds it, a
        // `claude --bg` row may still be waiting for its uuid: backfill it by
        // short-id prefix so the report can store on it AND ask/mail/push route
        // to it. Ambiguous prefix -> no backfill (AC1-ERR).
        let idx = match r
            .entries
            .iter()
            .position(|e| entry_holds_session(e, &session_id))
        {
            Some(i) => Some(i),
            None => match find_uuid_backfill_row(&r.entries, &session_id) {
                UuidBackfill::One(i) => {
                    // Persist the canonical id AND the in-memory alias: the
                    // JSON keeps `harness_session_id`, the loaded row matches
                    // on the per-harness field.
                    if r.entries[i].harness_name() == "codex" {
                        r.entries[i].harness_session_id = Some(session_id.clone());
                        r.entries[i].codex_session_id = Some(session_id.clone());
                    } else {
                        r.entries[i].claude_session_uuid = Some(session_id.clone());
                    }
                    Some(i)
                }
                UuidBackfill::None | UuidBackfill::Ambiguous => None,
            },
        };
        let Some(idx) = idx else {
            outcome = Outcome::Unknown;
            return;
        };
        let entry = &mut r.entries[idx];
        entry_name = Some(entry.name.clone());
        if let Some(rep) = &report_for_store {
            if let Some(prev) = &entry.inside_leg {
                if !prev.yields_to(rep) {
                    outcome = Outcome::StaleSeq { last: prev.seq };
                    return;
                }
            }
            let prev_state = entry.inside_leg.as_ref().map(|r| r.state);
            let prev_posture = entry.inside_leg.as_ref().and_then(|r| r.posture.clone());
            if state::enters(prev_state, rep.state, state::InsideLegState::Blocked) {
                let body = rep.reason.clone().unwrap_or_else(|| state_label.clone());
                notify = Some((entry.name.clone(), body, false));
            } else if state::enters(prev_state, rep.state, state::InsideLegState::Done) {
                let body = rep.reason.clone().unwrap_or_else(|| state_label.clone());
                notify = Some((entry.name.clone(), body, true));
            }
            entry.inside_leg = Some(rep.clone());
            // Capability flip: the hook now owns this row's signal; a stale
            // scrape verdict must never shadow it (per-capability arbitration).
            entry.screen_state = None;
            // Served posture: a real change emits once beside the other
            // served-axis changes; a repeat of the same posture is quiet.
            if let Some(p) = &rep.posture {
                if prev_posture.as_ref() != Some(p) {
                    let render =
                        |p: &state::ObservedPosture| format!("{}:{}", p.sandbox, p.approval);
                    axis_changes.push((
                        "agent_posture_changed",
                        prev_posture.as_ref().map(render),
                        render(p),
                    ));
                }
            }
        }
        if let Some(m) = &model {
            if entry.model.as_deref() != Some(m.as_str()) {
                axis_changes.push(("agent_model_changed", entry.model.clone(), m.clone()));
                entry.model = Some(m.clone());
            }
            // Any report is an observation; a matching one is the success case.
            entry.model_basis = Some("verified".to_string());
        }
        if let Some(eff) = &effort {
            if entry.effort.as_deref() != Some(eff.as_str()) {
                axis_changes.push(("agent_effort_changed", entry.effort.clone(), eff.clone()));
                entry.effort = Some(eff.clone());
            }
        }
        outcome = Outcome::Stored;
    }) {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("registry write failed during inside-leg report: {e}"),
        );
    }

    match outcome {
        Outcome::Stored => {
            let _ = ctx.emitter.emit(
                "inside_leg_report",
                &json!({"session_id": session_id, "seq": seq, "state": state_label}),
            );
            // One event per served-axis change, emitted only after
            // the write landed.
            for (kind, from, to) in &axis_changes {
                let _ = ctx.emitter.emit(
                    kind,
                    &json!({
                        "name": entry_name,
                        "harness_session_id": session_id,
                        "from": from,
                        "to": to,
                    }),
                );
            }
            if let Some((title, body, is_done)) = notify {
                let o = &ctx.opts;
                notify_badge(title, body, is_done, o.notify_on_blocked, o.notify_on_done);
            }
            Response::ok(req.id, json!({"stored": true, "seq": seq}))
        }
        Outcome::StaleSeq { last } => {
            let _ = ctx.emitter.emit(
                "inside_leg_report_dropped",
                &json!({"session_id": session_id, "seq": seq, "last_seq": last, "reason": "stale_seq"}),
            );
            Response::ok(
                req.id,
                json!({"stored": false, "dropped": "stale_seq", "last_seq": last}),
            )
        }
        // E3.3 buffer-on-early-push: the row is not up yet (the hook fired before
        // the daemon registered the pane). Hold the report in the bounded buffer
        // instead of dropping it; the spawn path flushes it onto the row at
        // creation. Still fire-and-forget: every branch returns `ok`. The lock is
        // scoped to the buffer op (released before the emit) via `.map(..).ok()`;
        // a poisoned lock -> `None` -> the old hard-drop degrade. A
        // model-posture report has no transition to buffer: an unknown session
        // is a plain drop.
        Outcome::Unknown => {
            let buffered = report
                .map(|rep| {
                    ctx.pending_inside_leg
                        .lock()
                        .map(|mut buf| buffer_pending_report(&mut buf, &session_id, rep))
                        .ok()
                })
                .flatten();
            match buffered {
                Some(BufferOutcome::Buffered) => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_buffered",
                        &json!({"session_id": session_id, "seq": seq, "state": state_label}),
                    );
                    Response::ok(
                        req.id,
                        json!({"stored": false, "buffered": true, "seq": seq}),
                    )
                }
                Some(BufferOutcome::StaleSeq { last }) => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_dropped",
                        &json!({"session_id": session_id, "seq": seq, "last_seq": last, "reason": "stale_seq"}),
                    );
                    Response::ok(
                        req.id,
                        json!({"stored": false, "dropped": "stale_seq", "last_seq": last}),
                    )
                }
                Some(BufferOutcome::Full) => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_dropped",
                        &json!({"session_id": session_id, "seq": seq, "reason": "buffer_full"}),
                    );
                    Response::ok(req.id, json!({"stored": false, "dropped": "buffer_full"}))
                }
                // Poisoned buffer lock: degrade to the old hard-drop rather than
                // panicking a fire-and-forget hook.
                None => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_dropped",
                        &json!({"session_id": session_id, "seq": seq, "reason": "unknown_session"}),
                    );
                    Response::ok(
                        req.id,
                        json!({"stored": false, "dropped": "unknown_session"}),
                    )
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::{short_home, test_ctx};

    fn seed_posture_row(home: &AgentsHome, uuid: &str, prev: Option<state::ObservedPosture>) {
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                harness: Some("claude".into()),
                name: "posture-row".into(),
                short_id: "ps1".into(),
                legacy_provider: "claude".into(),
                claude_session_uuid: Some(uuid.into()),
                status: AgentStatus::Live,
                created_at: "2026-10-01T00:00:00Z".into(),
                cwd: "/tmp".into(),
                project_root: "/tmp".into(),
                inside_leg: Some(state::InsideLegReport {
                    state: state::InsideLegState::Working,
                    seq: 1,
                    reason: None,
                    received_at: "2026-10-01T00:00:00Z".into(),
                    ttl_ms: None,
                    posture: prev,
                }),
                ..Default::default()
            });
        })
        .unwrap();
    }

    /// AC5: the second, different posture stores with kind "observed" and
    /// emits exactly one agent_posture_changed naming from -> to.
    #[test]
    fn posture_rides_the_report_and_emits_one_change() {
        let home = short_home("posture");
        seed_posture_row(
            &home,
            "uuid-post",
            Some(state::ObservedPosture::observed(
                "workspace-write",
                "on-request",
            )),
        );
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let resp = handle_report(
            &ctx,
            &Request::new(
                1,
                "agent.report",
                json!({
                    "session_id": "uuid-post", "seq": 2, "state": "done",
                    "posture": "danger-full-access:never"
                }),
            ),
        );
        assert_eq!(resp.result().unwrap()["stored"], true);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let rep = reg.entries[0].inside_leg.as_ref().unwrap();
        let p = rep.posture.as_ref().unwrap();
        assert_eq!(p.kind, "observed");
        assert_eq!(p.sandbox, "danger-full-access");
        assert_eq!(p.approval, "never");
        let ev = std::fs::read_to_string(home.events_jsonl()).unwrap();
        let changed = ev
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "agent_posture_changed")
            .collect::<Vec<_>>();
        assert_eq!(changed.len(), 1, "one posture change event: {ev}");
        assert_eq!(changed[0]["from"], "workspace-write:on-request");
        assert_eq!(changed[0]["to"], "danger-full-access:never");

        // A posture the daemon cannot parse (no colon, an empty half) is an
        // InvalidParams refusal, never a stored guess.
        for shape in ["workspace-write", "danger-full-access:", ":never"] {
            let bad = handle_report(
                &ctx,
                &Request::new(
                    1,
                    "agent.report",
                    json!({
                        "session_id": "uuid-post", "seq": 3,
                        "state": "working", "posture": shape
                    }),
                ),
            );
            assert!(bad.is_err(), "posture {shape:?} must be refused");
        }
        std::fs::remove_dir_all(home.root()).ok();
    }
}
