//! The mux server's whole-fleet truth probe: the cadence it runs on, the
//! single-flight latch around it, and the reading one probe yields.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// (v48) How often the off-loop task re-probes the fleet's reachability
/// evidence. One `agent.list` RPC per interval (the Python
/// CLI it replaced spent its wall time cold-starting an interpreter inside
/// the mux server's own child set), so the cost of the cadence is the
/// daemon's server-side truth batch, amortized across every client of the
/// daemon. 60s keeps the refresh an order of magnitude below the 600s
/// attention threshold the ages feed; it cannot change a row's tier, only
/// polish its displayed age, which lags by at most this interval.
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

/// (v48) One whole-fleet reachability probe: the daemon's `agent.list` RPC,
/// the same answer `fno agents list --json` prints after its Python cold
/// start (no interpreter is ever a mux child for this). Join
/// key is the registry name, the same field both list lanes and this
/// server's registry rows carry. `None` on any failure (no daemon,
/// transport fault, unparseable answer) so the caller can keep the last
/// good map rather than blanking every row on one miss. Rows whose probe
/// fields are null still enter the map: a probe that did not answer for one
/// row is that row's absence, not the fleet's.
pub(crate) async fn probe_truth_map() -> Option<HashMap<String, TruthReading>> {
    let result = crate::agents_view::agent_list_rpc(&crate::agents_view::supervisor_sock_path())
        .await
        .ok()?;
    truth_map_from_list(&result)
}

/// The pure half of the probe: the `agent.list` result's rows into the
/// truth map. Split from the transport so the row contract has a unit test
/// that needs no socket.
fn truth_map_from_list(result: &serde_json::Value) -> Option<HashMap<String, TruthReading>> {
    let mut map = HashMap::new();
    for row in result.get("agents")?.as_array()? {
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
        let cost_cents = row.get("session_cost_cents").and_then(|v| v.as_u64());
        let tokens = row.get("session_tokens").and_then(|v| v.as_u64());
        let compaction_count = row.get("compaction_count").and_then(|v| v.as_u64());
        map.insert(
            key,
            TruthReading {
                basis,
                age_s,
                cost_cents,
                tokens,
                compaction_count,
            },
        );
    }
    Some(map)
}

/// One registry row's reachability evidence, as the off-loop probe read
/// it: which basis answered and the transcript age it measured. The verdict
/// word is deliberately absent - it is derivable from the basis and it is the
/// half of the triple that reads healthy for a worker dead under two hours.
/// Beside it ride the daemon's served running-cost pair, `None` before the
/// first sweep measured the session.
#[derive(Debug, Clone, Default)]
pub(crate) struct TruthReading {
    pub(crate) basis: Option<String>,
    pub(crate) age_s: Option<u64>,
    pub(crate) cost_cents: Option<u64>,
    pub(crate) tokens: Option<u64>,
    pub(crate) compaction_count: Option<u64>,
}

impl TruthReading {
    /// The served running-cost pair for one agent row: what the daemon
    /// measured, keyed by the same identity the probe joined on.
    pub(crate) fn cost(&self) -> (Option<u64>, Option<u64>) {
        (self.cost_cents, self.tokens)
    }
}

/// The server-core side of the served cost: one accessor beside
/// `truth_basis` / `truth_age` (which stay in server.rs, shrink-only), so
/// the row builders never touch the reading map directly.
use super::Core;
use crate::agents_view::RegistryAgent;

impl Core {
    /// The daemon-served running-cost pair for one registry row: what the
    /// reconcile sweep measured, keyed by the same identity the probe
    /// joined on. `(None, None)` before the first measurement.
    pub(crate) fn truth_cost(&self, a: &RegistryAgent) -> (Option<u64>, Option<u64>) {
        self.truth_reading(a)
            .map(|t| t.cost())
            .unwrap_or((None, None))
    }
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

    /// the probe reads the daemon's `agent.list` RPC, never
    /// a spawned `fno agents list` interpreter. The pure half carries the
    /// row contract: identity-first join, the four readings, malformed rows
    /// skipped, no `agents` key refused.
    #[test]
    fn truth_rows_join_identity_first_and_skip_malformed() {
        let result: serde_json::Value = serde_json::from_str(
            r#"{"agents": [
                {"name": "row-a", "harness_session_id": "sess-1",
                 "basis": "transcript", "last_activity_age_s": 12.0,
                 "session_cost_cents": 7, "session_tokens": 9001,
                 "compaction_count": 2},
                {"name": "row-b", "basis": "process-gone"},
                "junk",
                {"basis": "silent"}
            ]}"#,
        )
        .unwrap();
        let map = super::truth_map_from_list(&result).expect("parses");
        assert_eq!(map.len(), 2, "malformed rows are skipped, not fatal");
        let a = map.get("sess-1").expect("identity-first key");
        assert_eq!(a.basis.as_deref(), Some("transcript"));
        assert_eq!(a.age_s, Some(12));
        assert_eq!(a.cost_cents, Some(7));
        assert_eq!(a.tokens, Some(9001));
        assert_eq!(a.compaction_count, Some(2));
        let b = map.get("row-b").expect("label key for a legacy row");
        assert_eq!(b.basis.as_deref(), Some("process-gone"));
        assert_eq!(b.age_s, None, "nulls are absence, not zeros");
        assert!(
            super::truth_map_from_list(&serde_json::json!({})).is_none(),
            "no agents key is a refused answer, not an empty map"
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
