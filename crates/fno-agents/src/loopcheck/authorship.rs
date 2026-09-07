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
/// independent). A present
/// attester with no author session is `Unmeasured` (a concrete session
/// attested, the comparison failed - the shape of an author's own re-read
/// from a cwd without the manifest), and an absent or empty attester is
/// `Unknown` (no evidence at all; an empty id is what harness_identity
/// returns when no harness marker is in the env, so this bucket is where an
/// author's own bare-lane self-review would land).
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

/// The manifest's `harness_session_id` for this cwd: the PRIMARY authorship
/// source, before the carry-forward fallback below. The manifest moved into
/// the worktree slice of the space; the legacy checkout path is the read
/// fallback for one release.
pub(crate) fn resolve_manifest_author(cwd: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(crate::paths::worktree_space_dir(cwd).join("target-state.md"))
        .or_else(|_| std::fs::read_to_string(cwd.join(".fno/target-state.md")))
        .ok()
        .and_then(|content| super::scan_manifest_field(&content, "harness_session_id"))
        .filter(|s| s != "null")
}

/// The newest non-empty `author_session_id` recorded on a `review_coverage`
/// event FOR THIS PR, for a read whose process resolved no target manifest.
/// The field is persisted for exactly this historical comparison (schema note
/// on `author_session_id`): a re-read from a manifest-less cwd must not
/// reclassify what an earlier measured read settled, because consumers take
/// the latest row. The carried id is recorded as the row's own
/// `author_session_id`, so later reads carry the same value forward.
///
/// The scan filters on `data.pr` and the event type: the events file is
/// PROJECT-wide (one file per repo across every worktree and PR), so an
/// unfiltered scan carries a foreign PR's author and every later row of this
/// PR then classifies against it - a foreign session on this PR reads as
/// the author's own, which is the laundering this module exists to refuse.
pub(crate) fn carry_author_session_forward(events_text: &str, pr: i64) -> Option<String> {
    let mut found = None;
    for line in events_text.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event.get("type").and_then(|v| v.as_str()) != Some("review_coverage") {
            continue;
        }
        let data = event.get("data").unwrap_or(&event);
        if data.get("pr").and_then(|v| v.as_i64()) != Some(pr) {
            continue;
        }
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
            carry_author_session_forward(events, 1).as_deref(),
            Some("sess-b")
        );
    }

    #[test]
    fn never_carries_a_foreign_pr_author() {
        // The project events file holds every PR's rows. A scan without the
        // pr filter carries the newest author ON THE FILE - another PR's
        // session - and a foreign attester on this PR then compares equal to
        // it and reads as the author's own. Only this PR's rows may answer.
        let events = concat!(
            "{\"type\":\"review_coverage\",\"data\":{\"pr\":9,\"author_session_id\":\"sess-foreign\"}}\n",
            "{\"type\":\"review_coverage\",\"data\":{\"pr\":1}}\n",
        );
        assert_eq!(carry_author_session_forward(events, 1), None);
        assert_eq!(
            carry_author_session_forward(events, 9).as_deref(),
            Some("sess-foreign")
        );
    }

    #[test]
    fn ignores_rows_that_are_not_review_coverage_events() {
        // Any event whose payload merely embeds the type string must not be
        // read as a coverage row: the type check, not a substring, decides.
        let events = concat!(
            "{\"type\":\"review_attestation\",\"data\":{\"pr\":1,\"author_session_id\":\"sess-x\"}}\n",
            "{\"type\":\"review_coverage\",\"data\":{\"pr\":1}}\n",
        );
        assert_eq!(carry_author_session_forward(events, 1), None);
    }

    #[test]
    fn none_when_no_row_ever_measured_authorship() {
        let events = "{\"type\":\"review_coverage\",\"data\":{\"pr\":1}}\n";
        assert_eq!(carry_author_session_forward(events, 1), None);
        assert_eq!(carry_author_session_forward("", 1), None);
    }
}
