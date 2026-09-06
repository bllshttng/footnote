//! The activity-feed shell-out leg (x-4433): a bounded, fail-open shell-out to
//! `fno agents feed --json`, mirroring [`crate::needs_overlay`]'s idiom.
//!
//! The projection itself lives in `fno-agents` (`feed.rs`): it joins
//! questions.jsonl and graph.json into ordered rows, so no second
//! implementation of the join can drift from the verb's. This module only
//! carries the row shape the client renders and the one bounded call that
//! fetches it - off the UI loop, 800ms cap, `None` on any failure.

use serde::Deserialize;
use std::time::Duration;

/// Same 800ms cap as the digest and needs overlays: a feed slower than this
/// degrades the overlay to a visible notice, never blocks the UI.
const SHELLOUT_TIMEOUT: Duration = Duration::from_millis(800);

/// One feed row, as emitted by `fno agents feed --json`. `session_id` is what
/// the deep link resolves through the sideline's own attach path.
#[derive(Debug, Clone, Deserialize)]
pub struct FeedItem {
    pub ts: String,
    pub kind: String,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
}

/// Run the feed projection, `None` on timeout, spawn failure, or a non-zero
/// exit. An empty feed is `Some(vec![])` - a real answer, not a failure.
pub async fn feed_now(since_epoch: &str) -> Option<Vec<FeedItem>> {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args([
            "agents",
            "feed",
            "--json",
            "--since-epoch",
            since_epoch,
            "--limit",
            "200",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_feed(&output.stdout)
}

fn parse_feed(stdout: &[u8]) -> Option<Vec<FeedItem>> {
    serde_json::from_slice::<Vec<FeedItem>>(stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_deserializes() {
        let body = br#"[{"ts":"2026-09-02T17:12:52Z","kind":"node_started","node":"x-9223","session_id":"s-do","harness":"claude","title":"port the claim classifier","ref":null},{"ts":"2026-09-02T18:27:06Z","kind":"pr_created","node":"x-9223","session_id":"s-ship","title":"PR 1395","ref":"1395"}]"#;
        let items = parse_feed(body).expect("a well-formed body parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "node_started");
        assert_eq!(items[0].session_id.as_deref(), Some("s-do"));
        assert_eq!(items[1].r#ref.as_deref(), Some("1395"));
    }

    #[test]
    fn empty_body_is_an_empty_vec_not_none() {
        let items = parse_feed(b"[]").expect("an empty feed is a real answer");
        assert!(items.is_empty());
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_feed(b"not json").is_none());
    }
}
