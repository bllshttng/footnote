//! Authorship for coverage classification: the worktree manifest is the
//! primary source, the previous coverage row is the fallback.

use serde::{Deserialize, Serialize};

/// Why a local verdict's authorship reads the way it does. Four states, not
/// three: collapsing the two unmeasured ones is what let one consumer read a
/// carried self-review as a peer and another read a manifest-less peer as
/// the author's own. `Unknown` means the ATTESTER was unobservable (an
/// env_only lane); `Unmeasured` means a concrete session attested but no
/// author session was available to compare, which is exactly the shape of an
/// author's own re-read from a cwd without the manifest.
///
/// Recorded, never gating. `coverage_count` does not read this field: every
/// `Reviewed` verdict counts regardless of origin, `SelfAttested` included.
/// The state after `SelfAttested`/`OtherSession` is deliberately not
/// `Independent`: the manifest names the session that ran `fno do target
/// init` in the worktree, so a self-handoff successor or a second agent in a
/// shared worktree is a different session and still not independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestationOrigin {
    SelfAttested,
    OtherSession,
    /// Attester present, author session unavailable to compare.
    Unmeasured,
    /// Attester absent or empty (env_only).
    Unknown,
}

/// The deserializer's default for rows written before the field existed.
pub(crate) fn default_attestation_origin() -> AttestationOrigin {
    AttestationOrigin::Unknown
}

/// Label a local attestation's authorship from its emitting session vs the
/// worktree's authoring session. A match is `SelfAttested`; a non-empty
/// mismatch is `OtherSession` (NOT "independent" - a self-handoff successor
/// or a shared-worktree sibling is a different session and still not
/// independent). A present attester with no author session is `Unmeasured`
/// (a concrete session attested, the comparison failed - the shape of an
/// author's own re-read from a cwd without the manifest), and an absent or
/// empty attester is `Unknown` (env_only; the recorded contract calls those
/// real reviews).
pub(crate) fn classify_attestation_origin(
    attester: Option<&str>,
    author: Option<&str>,
) -> AttestationOrigin {
    match (attester, author) {
        (Some(a), Some(auth)) if a == auth => AttestationOrigin::SelfAttested,
        (Some(_), Some(_)) => AttestationOrigin::OtherSession,
        (Some(_), None) => AttestationOrigin::Unmeasured,
        (None, _) => AttestationOrigin::Unknown,
    }
}

/// The newest non-empty `author_session_id` recorded on a `review_coverage`
/// event, for a read whose process resolved no target manifest. The field is
/// persisted for exactly this historical comparison (schema note on
/// `author_session_id`): a re-read from a manifest-less cwd must not
/// reclassify what an earlier measured read settled, because consumers take
/// the latest row. The carried id is recorded as the row's own
/// `author_session_id`, so later reads carry the same value forward.
pub(crate) fn carry_author_session_forward(events_text: &str) -> Option<String> {
    let mut found = None;
    for line in events_text.lines() {
        if !line.contains("\"review_coverage\"") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let data = event.get("data").unwrap_or(&event);
        if let Some(id) = data.get("author_session_id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                found = Some(id.to_string());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::carry_author_session_forward;

    #[test]
    fn carries_the_newest_recorded_author_session() {
        // The PR 1484 shape: the authoring session measured authorship on the
        // fresh head; after a rebase, a re-read from a manifest-less cwd
        // resolves none. The carried id lets the classify compare the
        // attester against the historical author instead of losing the
        // measurement.
        let events = concat!(
            "{\"type\":\"review_coverage\",\"data\":{\"pr\":1,\"author_session_id\":\"sess-a\"}}\n",
            "{\"type\":\"review_coverage\",\"data\":{\"pr\":1}}\n",
            "{\"type\":\"review_coverage\",\"data\":{\"pr\":1,\"author_session_id\":\"sess-b\"}}\n",
        );
        assert_eq!(
            carry_author_session_forward(events).as_deref(),
            Some("sess-b")
        );
    }

    #[test]
    fn none_when_no_row_ever_measured_authorship() {
        let events = "{\"type\":\"review_coverage\",\"data\":{\"pr\":1}}\n";
        assert_eq!(carry_author_session_forward(events), None);
        assert_eq!(carry_author_session_forward(""), None);
    }
}
