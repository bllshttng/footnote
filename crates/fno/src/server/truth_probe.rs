//! The mux server's whole-fleet truth probe: the cadence it runs on, the
//! single-flight latch around it, and the reading one probe yields.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// (v48) How often the off-loop task re-probes the fleet's reachability
/// evidence. Each interval is one whole-fleet CLI process, about eight
/// seconds of Python over the full roster, so the cadence must stay well
/// clear of that wall time: at 10s the probe child was alive in 14 of 25
/// process samples, effectively always running. 60s keeps the refresh an
/// order of magnitude below the 600s attention threshold the ages feed;
/// it cannot change a row's tier, only polish its displayed age, which
/// lags by at most this interval.
pub(crate) const TRUTH_PROBE_EVERY: Duration = Duration::from_secs(60);

/// The single-flight latch for one truth probe: `begin` wins exactly once
/// while the flag is clear, and the held guard clears it on every exit
/// path, including a probe that fails and yields `None`. The clear lives
/// in `Drop`, not in the task body - a latch that leaks set once turns the
/// overlap bug into a silent never-probe bug, which is worse.
pub(crate) struct TruthProbeLatch(Arc<AtomicBool>);

impl TruthProbeLatch {
    pub(crate) fn begin(flag: &Arc<AtomicBool>) -> Option<Self> {
        if flag.swap(true, Ordering::AcqRel) {
            None
        } else {
            Some(Self(flag.clone()))
        }
    }
}

impl Drop for TruthProbeLatch {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// (v48) One whole-fleet reachability probe: `fno agents list --json`, the
/// surface whose row shape already pins the triple. Join key is the registry
/// name, the same field both list lanes and this server's registry rows
/// carry. `None` on any failure (no binary, unparseable output) so the caller
/// can keep the last good map rather than blanking every row on one miss.
/// Rows whose probe fields are null still enter the map: a probe that did not
/// answer for one row is that row's absence, not the fleet's.
pub(crate) fn probe_truth_map() -> Option<HashMap<String, TruthReading>> {
    let mut command = crate::process_admission::std_command("fno");
    command.args(["agents", "list", "--json"]);
    let out = crate::process_admission::std_output(&mut command).ok()?;
    if !out.status.success() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut map = HashMap::new();
    for row in parsed.get("agents")?.as_array()? {
        // A malformed row is skipped, not fatal: one bad entry must not cost
        // the whole fleet its readings.
        let Some(name) = row.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        // Identity-first key: the full harness
        // session id when the row carries one, the label otherwise (legacy
        // rows). A rename no longer orphans a row's readings.
        let key = row
            .get("harness_session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| name.to_string());
        let basis = row
            .get("basis")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let age_s = row
            .get("last_activity_age_s")
            .and_then(|v| v.as_f64())
            .map(|f| f as u64);
        map.insert(key, TruthReading { basis, age_s });
    }
    Some(map)
}

/// One registry row's reachability evidence, as the off-loop probe read
/// it: which basis answered and the transcript age it measured. The verdict
/// word is deliberately absent - it is derivable from the basis and it is the
/// half of the triple that reads healthy for a worker dead under two hours.
#[derive(Debug, Clone, Default)]
pub(crate) struct TruthReading {
    pub(crate) basis: Option<String>,
    pub(crate) age_s: Option<u64>,
}

#[cfg(test)]
mod truth_probe_cadence {
    use super::*;

    #[test]
    fn cadence_stays_well_below_the_attention_threshold_it_feeds() {
        assert!(
            TRUTH_PROBE_EVERY >= Duration::from_secs(60),
            "a cadence under 60s keeps the ~8s whole-fleet probe child effectively always running"
        );
        assert!(
            TRUTH_PROBE_EVERY * 10 <= Duration::from_secs(600),
            "the refresh must stay an order of magnitude below the 600s attention threshold its ages feed"
        );
    }

    #[test]
    fn latch_admits_one_probe_and_clears_on_every_exit_path() {
        let flag = Arc::new(AtomicBool::new(false));
        let first = TruthProbeLatch::begin(&flag);
        assert!(first.is_some(), "a clear latch admits the first probe");
        assert!(
            TruthProbeLatch::begin(&flag).is_none(),
            "a second tick during an in-flight probe must spawn nothing"
        );
        drop(first); // the probe task's only exit path, including the None (failed) shape
        assert!(
            !flag.load(Ordering::Acquire),
            "the latch clears after a probe that returns None"
        );
        assert!(
            TruthProbeLatch::begin(&flag).is_some(),
            "the tick after a cleared latch probes normally"
        );
    }
}
