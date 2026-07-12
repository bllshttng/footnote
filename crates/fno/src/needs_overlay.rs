//! The needs-me-queue events-fold leg (x-feec): a bounded, fail-open shell-out
//! to `fno-agents needs --json`, mirroring [`crate::digest_overlay`]'s idiom.
//!
//! The client owns the live badge leg (blocked/done-unseen rows from the
//! layout) and renders it instantly; this module supplies the event-derived
//! leg (`review_wedged` / `budget_stop`) the client cannot see from badges. The
//! call is off the UI loop: it runs on a spawned task and reports back over a
//! channel, so a slow `fno-agents` never blocks the overlay from opening.

use serde::Deserialize;
use std::time::Duration;

/// Same 800ms cap as the digest overlay: a fold slower than this degrades the
/// queue to its live badge leg with a visible notice, never blocks the UI.
const SHELLOUT_TIMEOUT: Duration = Duration::from_millis(800);

/// One event-derived need, as emitted by `fno-agents needs --json`. The `live`
/// bit is the claim-liveness stamp (x-feec 1.4): the client renders an item
/// that joins no roster row only when it is `live`, so a dead session's stale
/// stop never nags.
#[derive(Debug, Clone, Deserialize)]
pub struct FoldItem {
    pub kind: String,
    pub session_id: String,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub live: bool,
}

/// Fold the needs-me events leg over the `since_epoch` window. `None` on any
/// failure (timeout, nonzero exit, unparseable JSON) — the caller shows the
/// degraded notice; `Some(vec)` (possibly empty) is a clean fold.
pub async fn fold_now(since_epoch: &str) -> Option<Vec<FoldItem>> {
    let fut = tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())
        .args(["needs", "--since-epoch", since_epoch, "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Dropped on timeout; kill_on_drop reaps the child so a slow fold can't
        // orphan a process on each overlay open.
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&output.stdout)
}

/// Parse the verb's JSON array. Fails quiet (returns `None`) on unparseable
/// output so a torn stdout degrades the overlay rather than crashing it.
fn parse(stdout: &[u8]) -> Option<Vec<FoldItem>> {
    serde_json::from_slice(stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_fold_array() {
        let json = br#"[{"kind":"review_wedged","session_id":"s","node":"x-1","name":"x-1","title":"t","ts":"2026-07-03T02:00:00Z","evidence":"green PR wedged","live":true}]"#;
        let items = parse(json).expect("valid array parses");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "review_wedged");
        assert_eq!(items[0].node.as_deref(), Some("x-1"));
        assert!(items[0].live);
    }

    #[test]
    fn empty_array_is_a_clean_empty_fold() {
        assert_eq!(parse(b"[]").expect("empty array parses").len(), 0);
    }

    #[test]
    fn missing_optional_fields_default() {
        // node/name/title/live absent -> defaults, not a parse failure.
        let json = br#"[{"kind":"budget_stop","session_id":"s","ts":"","evidence":"stopped"}]"#;
        let items = parse(json).expect("parses with defaults");
        assert_eq!(items[0].node, None);
        assert!(!items[0].live);
    }

    #[test]
    fn torn_json_fails_quiet() {
        assert!(parse(b"[{not json").is_none());
    }
}
