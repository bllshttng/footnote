//! The daemon's interval-gated maintenance sweeps (stale questions, park
//! records), split out of daemon.rs for the file budget. Each sweep
//! shares one shape: a stamp-gated interval floor, a dispatch-pause skip with
//! a paced sidecar row, and one journal row per run including a quiet one.

use super::worktree_sweep::registry_repo_roots;
use super::*;
use crate::pr_park;

/// How long between stale-question reconciles. Stale rows are measured in
/// hundreds of hours, so the interval bounds discovery lag, not freshness:
/// a row that crosses the wake ceiling waits at most one interval before a
/// human is told. Identity-keyed dedupe lives in the command, so an eager run
/// costs one sweep and changes nothing.
pub(crate) const STALE_SWEEP_INTERVAL_SECS: i64 = 21_600;

/// How long between park sweeps. A parked row owes its merge when its head
/// moves; the sweep costs one `fno do pr info` per open parked row.
pub(crate) const PARK_SWEEP_INTERVAL_SECS: i64 = 600;

/// One fleet's stale-sweep reading, parsed from the command's JSON line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleSweepReport {
    pub stale: usize,
    pub oldest_h: i64,
    pub outcome: String,
}

/// Parse the JSON object `fno agents stale-escalate --json` prints on stdout.
///
/// Returns `None` rather than a zeroed report when no readable object is
/// present. A sweep that could not read its own output must not report
/// "0 stale", which is indistinguishable from a clean machine. The outcome
/// word rides along because on the refused path the count is NOT a real
/// reading - the event must be able to say so rather than fabricate a
/// measured zero.
pub fn parse_stale_sweep(stdout: &str) -> Option<StaleSweepReport> {
    let line = stdout
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with('{'))?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(StaleSweepReport {
        stale: usize::try_from(value.get("stale_count")?.as_u64()?).ok()?,
        oldest_h: value.get("oldest_h")?.as_i64()?,
        outcome: value.get("outcome")?.as_str()?.to_string(),
    })
}

/// Stale-question reconcile on a 6h floor: report-only, no apply mode.
///
/// Emits one `stale_sweep` event per run, INCLUDING on outcome `none` or
/// `duplicate`: a tick that stays silent when it finds nothing cannot be told
/// from a tick that never ran. The run closure is injected so the policy is
/// testable without shelling out.
pub fn stale_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    now: i64,
    run: &dyn Fn() -> Option<String>,
) -> usize {
    let stamp = home.root().join("stale-escalate.stamp");
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < STALE_SWEEP_INTERVAL_SECS {
        return 0;
    }
    // An effective dispatch pause suspends the sweep without consuming its
    // cadence: no closure call, no stamp write, and a paced positive skip
    // row (sidecar stamp at the sweep's own interval) so intentional silence
    // cannot read as a dead arm.
    let pause = crate::loops_pause::dispatch_pause();
    if pause.is_paused() {
        let skip_stamp = home.root().join("stale-escalate.skipstamp");
        let last_skip = std::fs::read_to_string(&skip_stamp)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        if now.saturating_sub(last_skip) >= STALE_SWEEP_INTERVAL_SECS {
            let _ = emitter.emit(
                "stale_sweep",
                &json!({
                    "outcome": "skipped",
                    "reason": pause.skip_reason(),
                    "detail": pause.detail(),
                }),
            );
            let _ = std::fs::write(&skip_stamp, now.to_string());
        }
        return 0;
    }
    let outcome = match run().as_deref().and_then(parse_stale_sweep) {
        Some(r) => {
            let _ = emitter.emit(
                "stale_sweep",
                &json!({
                    "stale_count": r.stale,
                    "oldest_h": r.oldest_h,
                    "outcome": r.outcome,
                }),
            );
            1
        }
        None => {
            let _ = emitter.emit("stale_sweep", &json!({"error": "unreadable-summary"}));
            0
        }
    };
    let _ = std::fs::write(&stamp, now.to_string());
    outcome
}

/// Park sweep on a 10-minute floor: un-parks open rows whose PR head moved since
/// the park baseline or whose park passed 24 hours, and marks finished rows
/// handled. The command inside (`fno-agents pr-park sweep`) is idempotent on an
/// untouched store, so a re-run costs one head probe per open row and
/// changes nothing.
///
/// Emits one `park_sweep` event per run INCLUDING a quiet or skipped run: a
/// tick that stays silent cannot be told from a tick that never ran. The run
/// closure is injected so the policy is testable without shelling out.
pub fn park_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    now: i64,
    run: &dyn Fn() -> Option<(usize, usize, usize)>,
) -> usize {
    let stamp = home.root().join("park-sweep.stamp");
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < PARK_SWEEP_INTERVAL_SECS {
        return 0;
    }
    // A dispatch pause suspends the sweep on the same sidecar-stamp shape
    // stale_sweep uses: the child probes gh once per open parked row.
    let pause = crate::loops_pause::dispatch_pause();
    if pause.is_paused() {
        let skip_stamp = home.root().join("park-sweep.skipstamp");
        let last_skip = std::fs::read_to_string(&skip_stamp)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        if now.saturating_sub(last_skip) >= PARK_SWEEP_INTERVAL_SECS {
            let _ = emitter.emit(
                "park_sweep",
                &json!({"outcome": "skipped", "reason": pause.skip_reason()}),
            );
            let _ = std::fs::write(&skip_stamp, now.to_string());
        }
        return 0;
    }
    let (outcome, counts) = match run() {
        Some((unparked, handled, total)) => ("ok", Some((unparked, handled, total))),
        None => ("error", None),
    };
    let mut row = json!({"outcome": outcome});
    if let Some((unparked, handled, total)) = counts {
        row["unparked"] = json!(unparked);
        row["handled"] = json!(handled);
        row["total"] = json!(total);
    }
    let _ = emitter.emit("park_sweep", &row);
    let _ = std::fs::write(&stamp, now.to_string());
    1
}

/// The park sweep's run closure: sweep every repo root the registry knows,
/// so parked rows of other repos are judged from THEIR checkout (the head
/// probe resolves a PR number against the repo it belongs to).
pub fn sweep_all_roots(home: &AgentsHome) -> Option<(usize, usize, usize)> {
    let mut unparked = 0usize;
    let mut handled = 0usize;
    let mut total = 0usize;
    for root in registry_repo_roots(home) {
        // Per-root resolution: the store follows the config a watcher in THAT
        // checkout would read, not this process's home.
        let paths = pr_park::Paths::resolve(std::path::Path::new(&root));
        let ctx = pr_park::Ctx::live(std::path::Path::new(&root), paths.clone());
        if ctx.slug.is_empty() {
            continue;
        }
        // One repo's store failure must not starve the others: skip it and
        // keep sweeping, or one read-only checkout parks the whole fleet's
        // board behind it.
        let Ok(r) = pr_park::sweep(&ctx) else {
            continue;
        };
        unparked += r.unparked;
        handled += r.handled;
        total += r.total;
    }
    Some((unparked, handled, total))
}

/// How long between trust-answer passes. A codex pane parked on the
/// folder-trust modal blocks its worker entirely (one sat six hours), so the
/// floor is tick magnitude, not the maintenance-sweep minutes.
pub(crate) const TRUST_SWEEP_INTERVAL_SECS: i64 = 60;

/// The codex folder-trust auto-answer: a fno-spawned codex pane parked on the
/// folder-trust modal blocks its own worker, so the daemon answers the modal's
/// first option itself - it trusts the worker's own launch cwd, the one the
/// pane argv carries - and journals the answer, instead of waiting for a lead
/// to read the check-in line. The subject is structural: the modal only ever
/// asks about the `-C` cwd the pane launched with. Every other parked row
/// (approvals, permissions) stays report-only: those remain a human's call.
///
/// The send closure is injected so the policy is testable without a mux.
/// Emits one `trust_prompt_answered` event per attempt, `sent` naming whether
/// the keystroke landed; a failed send waits one interval and retries.
pub(crate) fn trust_answer_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    now: i64,
    send: &dyn Fn(u64, &str) -> bool,
) -> usize {
    let stamp = home.root().join("trust-answer.stamp");
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < TRUST_SWEEP_INTERVAL_SECS {
        return 0;
    }
    let _ = std::fs::write(&stamp, now.to_string());
    let Ok(reading) = crate::lead_checkin::prompt_parked::reading() else {
        return 0;
    };
    let Some(rows) = reading.get("rows").and_then(Value::as_array) else {
        return 0;
    };
    let mut answered = 0usize;
    for row in rows {
        if row.get("rule_id").and_then(Value::as_str) != Some("trust_prompt")
            || row.get("harness").and_then(Value::as_str) != Some("codex")
        {
            continue;
        }
        let Some(key) = row.get("answer_key").and_then(Value::as_str) else {
            continue;
        };
        let Some(pane) = row.get("pane_id").and_then(Value::as_u64) else {
            continue;
        };
        let sent = send(pane, key);
        if sent {
            answered += 1;
        }
        let _ = emitter.emit(
            "trust_prompt_answered",
            &json!({
                "holder": row.get("holder").and_then(Value::as_str).unwrap_or("unknown"),
                "pane_id": pane,
                "answer": key,
                "sent": sent,
            }),
        );
    }
    answered
}

#[cfg(test)]
mod trust_sweep_tests {
    use super::*;

    // The auto-answer: a codex pane parked on the folder-trust modal is
    // answered with its pinned key and the attempt is journaled; a claude
    // permission row is never answered; the 60s interval floor holds.
    #[test]
    fn the_daemon_answers_a_parked_codex_trust_modal_and_journals_it() {
        let _lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home_dir = tempfile::tempdir().unwrap();
        let saved_home = std::env::var_os("FNO_AGENTS_HOME");
        let saved_bin = std::env::var_os("FNO_BIN");
        std::env::set_var("FNO_AGENTS_HOME", home_dir.path());
        let events = home_dir.path().join("events-test.jsonl");
        let emitter = EventEmitter::new(&events, "daemon");

        // Registry: one live codex row joined to the stub pane.
        let reg = crate::state::Registry {
            entries: vec![{
                let mut e = crate::state::RegistryEntry::default();
                e.name = "trust-me".into();
                e.harness = Some("codex".into());
                e.status = crate::AgentStatus::Live;
                e.fno_id = Some("fid-77".into());
                e.cwd = "/tmp/trust-target".into();
                e
            }],
            ..Default::default()
        };
        crate::registry_store::seed_raw(
            AgentsHome::from_env().registry_json(),
            serde_json::to_string(&reg).unwrap(),
        );

        // Stub mux: pane 9 shows the codex 1.x trust modal; the \\n pairs
        // reach printf as literal backslash-n, the JSON escape for newline.
        let script = r#"#!/bin/sh
case "$1 $2" in
  "mux pane")
    if [ "$3" = "ls" ]; then
      printf '[{"pane_id": 9, "fno_id": "fid-77"}]\n'
    else
      printf '{"text": "Folder access\\n/tmp\\nTrust this folder?\\n\\n› 1. Trust and continue\\n  2. Back to Agent Command Center"}\n'
    fi
    ;;
  *) exit 3 ;;
esac
"#;
        let stub = crate::write_exec_stub(home_dir.path(), "fno-stub", script);
        std::env::set_var("FNO_BIN", &stub);

        let sent = std::sync::Mutex::new(None);
        let home = AgentsHome::from_env();
        let answered = trust_answer_sweep(&home, &emitter, 1_000, &|pane, key| {
            *sent.lock().unwrap() = Some((pane, key.to_string()));
            true
        });
        assert_eq!(answered, 1);
        assert_eq!(
            sent.lock().unwrap().as_ref(),
            Some(&(9u64, "1".to_string()))
        );

        // The journal names the row, the pane, the answer, and the landing;
        // the store commit is the emitter's acknowledgement boundary.
        let store = crate::event_store::store_path(&events);
        let conn = crate::event_store::open_read(&store).unwrap();
        let line: String = conn
            .query_row(
                "SELECT line FROM events WHERE type = 'trust_prompt_answered'",
                [],
                |r| r.get(0),
            )
            .expect("the answer is journaled");
        assert!(line.contains("\"sent\":true"), "{line}");
        assert!(line.contains("\"holder\":\"trust-me\""), "{line}");

        // The interval floor: a pass 59s later answers nothing.
        let again = trust_answer_sweep(&home, &emitter, 1_059, &|_, _| {
            panic!("the floor holds");
        });
        assert_eq!(again, 0);

        // A claude permission row is report-only, never auto-answered.
        let reg = crate::state::Registry {
            entries: vec![{
                let mut e = crate::state::RegistryEntry::default();
                e.name = "claude-ask".into();
                e.harness = Some("claude".into());
                e.status = crate::AgentStatus::Live;
                e.fno_id = Some("fid-88".into());
                e.cwd = "/tmp/claude-target".into();
                e
            }],
            ..Default::default()
        };
        crate::registry_store::seed_raw(
            AgentsHome::from_env().registry_json(),
            serde_json::to_string(&reg).unwrap(),
        );
        let script = r#"#!/bin/sh
case "$1 $2" in
  "mux pane")
    if [ "$3" = "ls" ]; then
      printf '[{"pane_id": 12, "fno_id": "fid-88"}]\n'
    else
      printf '{"text": "Do you want to proceed?\\n  ❯ 1. Yes\\n  2. No"}\n'
    fi
    ;;
  *) exit 3 ;;
esac
"#;
        let stub = crate::write_exec_stub(home_dir.path(), "fno-stub-2", script);
        std::env::set_var("FNO_BIN", &stub);
        let answered = trust_answer_sweep(&home, &emitter, 1_060, &|_, _| {
            panic!("claude approvals are never auto-answered");
        });
        assert_eq!(answered, 0);

        match saved_home {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        match saved_bin {
            Some(v) => std::env::set_var("FNO_BIN", v),
            None => std::env::remove_var("FNO_BIN"),
        }
    }
}
