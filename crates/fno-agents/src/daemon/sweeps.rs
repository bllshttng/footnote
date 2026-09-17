//! The daemon's interval-gated maintenance sweeps (stale questions, park
//! records), split out of daemon.rs for the file budget. Each sweep
//! shares one shape: a stamp-gated interval floor, a dispatch-pause skip with
//! a paced sidecar row, and one journal row per run including a quiet one.

use super::*;
use crate::pr_park;

/// How long between stale-question reconciles. Stale rows are measured in
/// hundreds of hours, so the interval bounds discovery lag, not freshness:
/// a row that crosses the wake ceiling waits at most one interval before a
/// human is told. Identity-keyed dedupe lives in the verb, so an eager run
/// costs one sweep and changes nothing.
pub(crate) const STALE_SWEEP_INTERVAL_SECS: i64 = 21_600;

/// How long between park sweeps. A parked PR comes back on the next push, so
/// the sweep's job is to notice the push; 6h bounds discovery lag on a clock
/// that already ticks.
pub(crate) const PARK_SWEEP_INTERVAL_SECS: i64 = 21_600;

/// One fleet's stale-sweep reading, parsed from the verb's JSON line.
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

/// Park sweep on a 6h floor: un-parks open rows whose PR head moved since
/// the park baseline or whose park passed 24 hours, and marks finished rows
/// handled. The verb inside (`fno-agents pr-park sweep`) is idempotent on an
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
    let paths = pr_park::Paths::from_home();
    let mut unparked = 0usize;
    let mut handled = 0usize;
    let mut total = 0usize;
    for root in registry_repo_roots(home) {
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
