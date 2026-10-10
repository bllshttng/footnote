//! The cancel sentinel: is this run asked to stop?
//!
//! Two drivers, two sentinel families: a `/target` run reads the tombstone
//! then the sentinel under the project's `.fno/`; the lead reads only the
//! `cancelled` twin of its own state file, so cancelling one lane never
//! answers for the other.
//!
//! A sentinel MAY carry a payload of `author:` / `reason:` lines so a
//! cancelled run explains itself afterward. A zero-byte file (bare `touch`)
//! stays valid and reads as unattributed.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

/// Which sentinel family fired; decides what a reader may consume.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CancelKind {
    /// The plain `.target-cancelled` sentinel. Consumed (deleted) once it has
    /// terminated a run: a sentinel that keeps firing converts a single cancel
    /// into a wedge for every later stop of a session that recovers.
    TargetSentinel,
    /// The `.target-cancelled-final` tombstone. Never consumed here; it is
    /// keyed to a session and cleared by init.
    TargetTombstone,
    /// The lead's scoped `cancelled` twin. Never consumed here.
    LeadTwin,
}

pub(crate) struct CancelHit {
    pub kind: CancelKind,
    pub path: PathBuf,
    pub author: Option<String>,
    pub reason: Option<String>,
    /// Who invoked the cancel: `user` when the newest user entry carries
    /// the cancel verb, `assistant` when the session itself asked (F6: a
    /// self-cancel is a stuck signal, not a stop). Advisory; the loop
    /// check re-derives it from the transcript at honor time.
    pub via: Option<String>,
}

impl CancelHit {
    /// The attribution fragment for a human-readable termination line.
    pub(crate) fn attribution(&self) -> String {
        match (&self.author, &self.reason) {
            (Some(a), Some(r)) => format!(" (author: {a}, reason: {r})"),
            (Some(a), None) => format!(" (author: {a}, no reason given)"),
            (None, Some(r)) => format!(" (unattributed, reason: {r})"),
            (None, None) => " (no author recorded)".to_string(),
        }
    }

    /// The reader's human-readable termination line, attribution included.
    pub(crate) fn termination_message(&self) -> String {
        format!("cancel sentinel present{}", self.attribution())
    }

    /// The `termination` event payload, attribution fields included.
    pub(crate) fn termination_data(&self, session_id: &str) -> serde_json::Value {
        let mut data = serde_json::json!({
            "session_id": session_id,
            "reason": "Interrupted",
            "message": self.termination_message(),
        });
        if let Some(author) = &self.author {
            data["cancel_author"] = serde_json::json!(author);
        }
        if let Some(reason) = &self.reason {
            data["cancel_reason"] = serde_json::json!(reason);
        }
        data
    }
}

/// Parse `author:` / `reason:` / `via:` lines out of a sentinel payload.
/// First occurrence wins; unknown lines are ignored; all three may be None.
pub(crate) fn parse_cancel_payload(
    content: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut author = None;
    let mut reason = None;
    let mut via = None;
    for line in content.lines() {
        let line = line.trim();
        if author.is_none() {
            if let Some(value) = line.strip_prefix("author:") {
                author = Some(value.trim().to_string());
                continue;
            }
        }
        if via.is_none() {
            if let Some(value) = line.strip_prefix("via:") {
                via = Some(value.trim().to_string());
                continue;
            }
        }
        if reason.is_none() {
            if let Some(value) = line.strip_prefix("reason:") {
                reason = Some(value.trim().to_string());
            }
        }
    }
    (author, reason, via)
}

pub(crate) fn check_cancel_sentinel(
    cwd: &Path,
    state_path: &Path,
    created_at: &Option<String>,
    driver: &str,
) -> Option<CancelHit> {
    let target_sentinel = cwd.join(".fno/.target-cancelled");
    let target_tombstone = cwd.join(".fno/.target-cancelled-final");
    let lead_sentinel = state_path.with_extension("cancelled");
    let paths: Vec<(&Path, CancelKind)> = if driver == "lead" {
        vec![(lead_sentinel.as_path(), CancelKind::LeadTwin)]
    } else {
        vec![
            (target_tombstone.as_path(), CancelKind::TargetTombstone),
            (target_sentinel.as_path(), CancelKind::TargetSentinel),
        ]
    };

    for (path, kind) in paths {
        if !path.exists() {
            continue;
        }
        // Check mtime >= created_at
        if let Some(ca) = created_at {
            if let Ok(parsed_ca) = ca.parse::<DateTime<Utc>>() {
                if let Ok(meta) = std::fs::metadata(path) {
                    if let Ok(modified) = meta.modified() {
                        let sentinel_time: DateTime<Utc> = modified.into();
                        if sentinel_time >= parsed_ca {
                            return Some(read_hit(path, kind));
                        }
                        // Stale sentinel (older than created_at) -> ignore
                        continue;
                    }
                }
            }
            // Can't read mtime -> treat as present (fail-closed)
            return Some(read_hit(path, kind));
        }
        return Some(read_hit(path, kind));
    }
    None
}

fn read_hit(path: &Path, kind: CancelKind) -> CancelHit {
    let (author, reason, via) = std::fs::read_to_string(path)
        .map(|content| parse_cancel_payload(&content))
        .unwrap_or((None, None, None));
    CancelHit {
        kind,
        path: path.to_path_buf(),
        author,
        reason,
        via,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_is_unattributed() {
        let (author, reason, via) = parse_cancel_payload("");
        assert_eq!(author, None);
        assert_eq!(reason, None);
        assert_eq!(via, None);
    }

    #[test]
    fn author_reason_and_via_lines_parse() {
        let (author, reason, via) =
            parse_cancel_payload("author: operator\nreason: wrong direction\nvia: user\n");
        assert_eq!(author.as_deref(), Some("operator"));
        assert_eq!(reason.as_deref(), Some("wrong direction"));
        assert_eq!(via.as_deref(), Some("user"));
        // First occurrence wins; unknown lines are ignored.
        let (author, reason, via) = parse_cancel_payload(
            "note: hello\nauthor: first\nauthor: second\nreason: r1\nreason: r2\nvia: assistant\nvia: user\n",
        );
        assert_eq!(author.as_deref(), Some("first"));
        assert_eq!(reason.as_deref(), Some("r1"));
        assert_eq!(via.as_deref(), Some("assistant"));
    }

    #[test]
    fn reason_value_may_contain_colons_and_blank_lines_are_skipped() {
        let (author, reason, _via) =
            parse_cancel_payload("\nauthor: init\n\nreason: claim held by other: session X\n");
        assert_eq!(author.as_deref(), Some("init"));
        assert_eq!(reason.as_deref(), Some("claim held by other: session X"));
    }

    #[test]
    fn attribution_covers_all_four_shapes() {
        let hit = |author: Option<&str>, reason: Option<&str>| CancelHit {
            kind: CancelKind::TargetSentinel,
            path: PathBuf::from("/tmp/s"),
            author: author.map(str::to_string),
            reason: reason.map(str::to_string),
            via: None,
        };
        assert_eq!(
            hit(Some("op"), Some("why")).attribution(),
            " (author: op, reason: why)"
        );
        assert_eq!(
            hit(Some("op"), None).attribution(),
            " (author: op, no reason given)"
        );
        assert_eq!(
            hit(None, Some("why")).attribution(),
            " (unattributed, reason: why)"
        );
        assert_eq!(hit(None, None).attribution(), " (no author recorded)");
    }
}
