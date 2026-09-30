//! Per-beat refusal-rate trend baseline for the reign check-in.
//!
//! The `reign_checkin` journal is the diff corpus, not the trend baseline: a
//! beat that prints but never journals (`--no-emit`, a refused `emit_row`, a
//! missing emit path) leaves the comparison on the same old journalled pair,
//! so a falling series reads RISING beat after beat. The baseline therefore
//! lives here and advances on every beat that measured a rate, whether or
//! not its full row landed. One file per scope: concurrent beats of
//! different crowns share no document, so no write can erase another
//! scope's ring.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Measurements kept per scope. The trend reader needs two; the third is
/// slack so a single torn or skipped write cannot blind the reader.
const KEPT_PER_SCOPE: usize = 3;

fn scope_path(dir: &Path, scope: &str) -> PathBuf {
    dir.join(format!(
        "{}.json",
        crate::provider_cap::lane_file_token(scope)
    ))
}

fn read_ring(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Records one beat's measured rate for its scope. Best effort: a failed
/// record is warned and the next beat falls back to reading unmeasured, the
/// fail-safe direction for a handoff signal.
pub(crate) fn record(dir: &Path, scope: &str, ts: &str, rate: f64) {
    if scope.is_empty() {
        return;
    }
    let path = scope_path(dir, scope);
    let mut ring: Vec<Value> = read_ring(&path);
    ring.push(json!({"ts": ts, "rate": rate}));
    let excess = ring.len().saturating_sub(KEPT_PER_SCOPE);
    if excess > 0 {
        ring.drain(..excess);
    }
    let text = serde_json::to_string(&ring).unwrap_or_default();
    if text.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The rename is atomic, so a reader never sees a torn document and
    // falls back to a stale pair - the exact defect this store exists to
    // remove. Two beats racing on the SAME scope keep the file valid: the
    // last rename wins and at worst one entry is lost, which reads
    // unmeasured, never rising.
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::write(&tmp, &text).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        eprintln!("king-checkin: WARNING: refusal trend not recorded: {e}");
    }
}

/// The two newest known measurements for `scope` as (p1, p2): the handoff
/// signal needs a rise ACROSS two consecutive beats, so one prior alone
/// reads unmeasured, never rising.
pub(crate) fn priors(dir: &Path, scope: &str) -> (Option<f64>, Option<f64>) {
    let ring = read_ring(&scope_path(dir, scope));
    let rate_at = |idx: usize| -> Option<f64> {
        ring.iter()
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
        let trend = dir.path();
        // An absent or torn store reads empty: unmeasured is the fail-safe
        // direction for a handoff signal.
        assert_eq!(priors(trend, "x-bbbb"), (None, None));
        std::fs::write(scope_path(trend, "x-bbbb"), "{not json").unwrap();
        assert_eq!(priors(trend, "x-bbbb"), (None, None));
        record(trend, "x-bbbb", "2026-09-15T10:00:00Z", 0.175);
        // One prior alone is not a pair: the reader holds at unmeasured.
        assert_eq!(priors(trend, "x-bbbb"), (Some(0.175), None));
        record(trend, "x-bbbb", "2026-09-15T10:05:00Z", 0.165);
        record(trend, "x-bbbb", "2026-09-15T10:10:00Z", 0.150);
        assert_eq!(priors(trend, "x-bbbb"), (Some(0.150), Some(0.165)));
        // The ring keeps three: the fourth pushes the oldest off.
        record(trend, "x-bbbb", "2026-09-15T10:15:00Z", 0.120);
        assert_eq!(priors(trend, "x-bbbb"), (Some(0.120), Some(0.150)));
        // Scopes are independent files: one scope's ring never touches
        // another's, so no concurrent write can erase it.
        record(trend, "x-aaaa", "2026-09-15T10:16:00Z", 0.10);
        assert_eq!(priors(trend, "x-aaaa"), (Some(0.10), None));
        assert_eq!(priors(trend, "x-bbbb"), (Some(0.120), Some(0.150)));
    }
}
