//! The activity-feed shell-out leg (x-4433): a bounded, fail-open shell-out to
//! `fno agents feed --json`, in the same shape as [`crate::court_overlay`]'s
//! fold (x-d15a: the 800ms cap it first copied was half the projection's
//! measured runtime, so the feed could never render).
//!
//! The projection itself lives in `fno-agents` (`feed.rs`): it joins
//! questions.jsonl and graph.json into ordered rows, so no second
//! implementation of the join can drift from the verb's. This module only
//! carries the row shape the client renders and the one bounded call that
//! fetches it - off the UI loop, and a typed [`FeedError`] instead of a bare
//! `None` so the client can say WHICH failure fired.

use serde::Deserialize;
use std::time::Duration;

/// Ten seconds, court_overlay's budget for a comparable multi-store read.
/// The feed joins three stores and serialises 200 rows; its measured runtime
/// is 1.6s against the 800ms this file inherited from needs_overlay's cheap
/// reads, so under the old cap it could not succeed at all. A long budget
/// costs nothing: the fold runs off the UI loop, one at a time, and
/// `kill_on_drop` reaps the child on the timeout.
const SHELLOUT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Why a feed fold failed. Each variant is a different user action - retune a
/// budget, free an admission slot, read the projection's stderr - which is
/// why they stopped collapsing into one `None` (x-d15a).
#[derive(Debug, Clone)]
pub enum FeedError {
    /// The projection outlived [`SHELLOUT_TIMEOUT`].
    Timeout,
    /// The projection never ran: spawn failure or a process-admission
    /// refusal. Carries the io error's own words.
    Spawn(String),
    /// The projection ran and exited non-zero. Carries its stderr.
    Exit(String),
    /// stdout was not the row array. Carries the serde error and stderr.
    Malformed(String),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedError::Timeout => {
                write!(f, "feed timed out after {}s", SHELLOUT_TIMEOUT.as_secs())
            }
            FeedError::Spawn(e) => write!(f, "feed could not start: {e}"),
            FeedError::Exit(stderr) => write!(f, "feed exited non-zero: {stderr}"),
            FeedError::Malformed(e) => write!(f, "feed returned malformed JSON: {e}"),
        }
    }
}

/// The projection's stderr, one line, short enough for the overlay footer.
/// Empty stderr degrades to the exit's own spelling so the line never dangles.
fn stderr_note(stderr: &[u8], status: &std::process::ExitStatus) -> String {
    match std::str::from_utf8(stderr) {
        Ok(text) => text
            .lines()
            .next()
            .unwrap_or("no stderr")
            .chars()
            .take(160)
            .collect(),
        Err(_) => format!("{status}"),
    }
}

/// One fold result, as it travels the client's single-flight channel.
pub type FoldResult = Result<Vec<FeedItem>, FeedError>;

/// Run the feed projection, `Err` carrying the typed reason on any failure.
/// An empty feed is `Ok(vec![])` - a real answer, not a failure.
pub async fn feed_now(since_epoch: &str) -> FoldResult {
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
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .map_err(|_| FeedError::Timeout)?
        .map_err(|e| FeedError::Spawn(e.to_string()))?;
    if !output.status.success() {
        return Err(FeedError::Exit(stderr_note(&output.stderr, &output.status)));
    }
    parse_feed(&output.stdout, &output.stderr)
}

fn parse_feed(stdout: &[u8], stderr: &[u8]) -> Result<Vec<FeedItem>, FeedError> {
    serde_json::from_slice::<Vec<FeedItem>>(stdout).map_err(|e| {
        FeedError::Malformed(match std::str::from_utf8(stderr) {
            Ok(text) => format!("{e}; stderr: {}", text.lines().next().unwrap_or("none")),
            Err(_) => e.to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_deserializes() {
        let body = br#"[{"ts":"2026-09-02T17:12:52Z","kind":"node_started","node":"x-9223","session_id":"s-do","harness":"claude","title":"port the claim classifier","ref":null},{"ts":"2026-09-02T18:27:06Z","kind":"pr_created","node":"x-9223","session_id":"s-ship","title":"PR 1395","ref":"1395"}]"#;
        let items = parse_feed(body, b"").expect("a well-formed body parses");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "node_started");
        assert_eq!(items[0].session_id.as_deref(), Some("s-do"));
        assert_eq!(items[1].r#ref.as_deref(), Some("1395"));
    }

    #[test]
    fn empty_body_is_an_empty_vec_not_an_error() {
        let items = parse_feed(b"[]", b"").expect("an empty feed is a real answer");
        assert!(items.is_empty());
    }

    #[test]
    fn garbage_is_malformed_carrying_stderr() {
        let err = parse_feed(b"not json", b"skipped 3 malformed question lines\n")
            .expect_err("garbage is a typed error, not a silent None");
        assert!(matches!(err, FeedError::Malformed(_)));
        let rendered = err.to_string();
        assert!(rendered.contains("malformed JSON"));
        assert!(rendered.contains("skipped 3 malformed question lines"));
    }

    // x-d15a regression: the timeout names itself and its duration, so the
    // rendered line is a budget statement, never the old generic failure.
    #[test]
    fn timeout_renders_the_budget_it_outran() {
        let line = FeedError::Timeout.to_string();
        assert!(line.contains("timed out after 10s"), "got: {line}");
        assert!(!line.contains("failed"));
    }

    #[test]
    fn exit_carries_stderr_first_line() {
        let status = std::process::Command::new("true")
            .status()
            .expect("true runs");
        let note = stderr_note(b"unreadable store: graph.json\nsecond line", &status);
        assert_eq!(note, "unreadable store: graph.json");
        let rendered = FeedError::Exit(note).to_string();
        assert!(rendered.contains("unreadable store"), "got: {rendered}");
    }
}
