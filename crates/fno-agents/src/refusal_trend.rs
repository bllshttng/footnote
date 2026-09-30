//! Per-beat refusal-rate trend baseline for the reign check-in.
//!
//! The `reign_checkin` journal is the diff corpus, not the trend baseline: a
//! beat that prints but never journals (`--no-emit`, a refused `emit_row`, a
//! missing emit path) leaves the comparison on the same old journalled pair,
//! so a falling series reads RISING beat after beat. The baseline therefore
//! lives here and advances on every beat that measured a rate, whether or
//! not its full row landed.

use serde_json::{json, Map, Value};
use std::path::Path;

/// Measurements kept per scope. The trend reader needs two; the third is
/// slack so a single torn or skipped write cannot blind the reader.
const KEPT_PER_SCOPE: usize = 3;

type Scopes = Map<String, Value>;

fn read_scopes(path: &Path) -> Scopes {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Records one beat's measured rate for its scope. Best effort: a failed
/// record is warned and the next beat falls back to reading unmeasured, the
/// fail-safe direction for a handoff signal.
pub(crate) fn record(path: &Path, scope: &str, ts: &str, rate: f64) {
    let mut scopes = read_scopes(path);
    let entry = scopes.entry(scope.to_string()).or_insert(json!([]));
    if let Some(arr) = entry.as_array_mut() {
        arr.push(json!({"ts": ts, "rate": rate}));
        let excess = arr.len().saturating_sub(KEPT_PER_SCOPE);
        if excess > 0 {
            arr.drain(..excess);
        }
    }
    let text = serde_json::to_string(&Value::Object(scopes)).unwrap_or_default();
    if text.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // One store, one truth: the rename is atomic, so a reader never sees a
    // torn document and falls back to a stale pair - the exact defect this
    // store exists to remove.
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::write(&tmp, &text).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        eprintln!("king-checkin: WARNING: refusal trend not recorded: {e}");
    }
}

/// The two newest known measurements for `scope` as (p1, p2): the handoff
/// signal needs a rise ACROSS two consecutive beats, so one prior alone
/// reads unmeasured, never rising.
pub(crate) fn priors(path: &Path, scope: &str) -> (Option<f64>, Option<f64>) {
    let scopes = read_scopes(path);
    let Some(entries) = scopes.get(scope).and_then(Value::as_array) else {
        return (None, None);
    };
    let rate_at = |idx: usize| -> Option<f64> {
        entries
            .iter()
            .rev()
            .nth(idx)
            .and_then(|e| e.get("rate").and_then(Value::as_f64))
    };
    (rate_at(0), rate_at(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_advances_the_baseline_across_unjournalled_beats() {
        let dir = tempfile::tempdir().unwrap();
        let trend = dir.path().join("refusal-trend.json");
        // An absent or torn store reads empty: unmeasured is the fail-safe
        // direction for a handoff signal.
        assert_eq!(priors(&trend, "x-bbbb"), (None, None));
        std::fs::write(&trend, "{not json").unwrap();
        assert_eq!(priors(&trend, "x-bbbb"), (None, None));
        record(&trend, "x-bbbb", "2026-09-15T10:00:00Z", 0.175);
        // One prior alone is not a pair: the reader holds at unmeasured.
        assert_eq!(priors(&trend, "x-bbbb"), (Some(0.175), None));
        record(&trend, "x-bbbb", "2026-09-15T10:05:00Z", 0.165);
        record(&trend, "x-bbbb", "2026-09-15T10:10:00Z", 0.150);
        assert_eq!(priors(&trend, "x-bbbb"), (Some(0.165), Some(0.175)));
        // The ring keeps three: the fourth pushes the oldest off.
        record(&trend, "x-bbbb", "2026-09-15T10:15:00Z", 0.120);
        assert_eq!(priors(&trend, "x-bbbb"), (Some(0.120), Some(0.150)));
        // Scopes are independent rows in one store.
        record(&trend, "x-aaaa", "2026-09-15T10:16:00Z", 0.10);
        assert_eq!(priors(&trend, "x-aaaa"), (Some(0.10), None));
        assert_eq!(priors(&trend, "x-bbbb"), (Some(0.120), Some(0.150)));
    }
}
