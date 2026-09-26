//! Which reviewers are unattested, and which review findings are still open.

use super::*;

/// A configured local reviewer with no head-pinned `pass` attestation.
#[derive(Debug, Clone, PartialEq)]
pub struct UnattestedReviewer {
    pub(super) name: String,
    /// A head this reviewer DID attest at, which is no longer HEAD. Always a
    /// PASS and never empty - normalized at construction so `Some` means
    /// "there is a real prior pass to name", not "check is_empty() first".
    /// Without it the block message reads as "you never ran sigma" to a session
    /// that ran sigma and then pushed a commit, losing turns twice.
    pub(super) superseded_head: Option<String>,
    /// This reviewer DID attest at the current head, and the verdict was not
    /// `pass`. "No attestation exists" would be a lie to a session that ran the
    /// reviewer and was told no.
    pub(super) failed_at_head: bool,
}

/// The committed event lines for one journal family: the store's rows in
/// commit order, pre-cutover bytes imported first (hash-dedupe free).
pub(crate) fn event_lines(journal: &Path) -> Result<Vec<String>, String> {
    crate::event_store::import_all(journal)?;
    let q = crate::event_store::EventQuery {
        include_rejected: true,
        ..Default::default()
    };
    let rows = crate::event_store::query_events(journal, &q)?;
    Ok(rows.into_iter().map(|r| r.line).collect())
}

/// The `config.review.reviewers` entries NOT satisfied by a head-pinned
/// `review_attestation` event. A
/// reviewer is satisfied when events.jsonl carries a line with
/// `type == "review_attestation"`, `data.reviewer` matching (leading '/'
/// stripped on both sides), the line in scope for this PR
/// (`attestation_in_scope`: `data.branch == head_branch`, or the exact head
/// sha whatever branch it emitted on - a legacy line with no branch counts
/// only on that exact-head arm), and `data.verdict == "pass"`.
///
/// The gate reads `.is_empty()` and the block message reads the names, so the
/// decision and the explanation come from ONE scan. When they came from two,
/// the message told sessions to wait on a bot that was never required.
///
/// Fail closed everywhere: an empty/unreadable events file, a stale head_sha
/// (attestation for a prior commit), or a `fail` verdict leaves the reviewer
/// UNSATISFIED - except 's ONE softening directly below. An empty
/// reviewer list is vacuously satisfied (no reviewers gate).
///: the one softening of the `fail` arm - a `fail` whose own chain
/// raised keyed findings that are all terminally dispositioned ANSWERS this
/// head, so it satisfies the reviewer exactly like a pass ("answered at this
/// head", never "clean at this head"). A findings-free fail and a RETRACTION
/// never satisfy (the bystander and revoked-pass shapes stay unsatisfied).
/// Authorship is unknowable inside this scan, so the disposition read is
/// fail-open on origin; the `reviewed` conjunction re-runs the
/// disposition scan WITH authorship downstream and a solo author's decline
/// still withholds there.
/// `unattested_reviewers` plus the count of unparseable lines that LOOK like
/// attestations. A torn write leaves a corrupt `review_attestation` in the file
/// and the gate then reports "no head-pinned review_attestation", which is the
/// same class of lie this node exists to delete - so the count is surfaced in
/// the reason.
pub fn unattested_reviewers_scan(
    events_path: &Path,
    reviewers: &[String],
    freshness: &dyn Fn(&str) -> Freshness,
    head_branch: &str,
    head_sha: &str,
    rounds_exhausted: bool,
) -> (Vec<UnattestedReviewer>, usize) {
    // no committed evidence -> gate unmet (fail closed); an unreadable store
    // is the same shape, never an empty-but-satisfied read
    let content = match event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => {
            let unsatisfied = reviewers
                .iter()
                .map(|r| UnattestedReviewer {
                    name: r.trim_start_matches('/').to_string(),
                    superseded_head: None,
                    failed_at_head: false,
                })
                .collect();
            return (unsatisfied, 0);
        }
    };
    unattested_reviewers_scan_text(
        &content,
        reviewers,
        freshness,
        head_branch,
        head_sha,
        rounds_exhausted,
    )
}

/// An operator review finding still open: a `review_finding` event for
/// the node with no later `review_finding_resolved` for the same id.
#[derive(Debug, Clone)]
pub(super) struct OpenFinding {
    pub(super) id: String,
    pub(super) first_line: String,
}

/// Scan events.jsonl for OPEN operator review findings scoped to `node`.
///
/// Open review findings for `node` from the findings store: the typed rows
/// map to the gate's view, and a store read error is named (never read as
/// zero, which is the false-clean this module exists to prevent).
pub(super) fn open_findings_from_store(
    graph: &std::path::Path,
    node: &str,
) -> (Vec<OpenFinding>, Option<String>) {
    match crate::backlog::api::findings(&crate::backlog::api::Store::new(graph), Some(node), false)
    {
        Ok(rows) => {
            let mut open: Vec<OpenFinding> = rows
                .iter()
                .filter(|f| f.resolved_at.is_none())
                .map(|f| OpenFinding {
                    id: f.finding_id.clone(),
                    first_line: f.body.lines().next().unwrap_or("").to_string(),
                })
                .collect();
            open.sort_by(|a, b| a.id.cmp(&b.id)); // deterministic deny reason
            (open, None)
        }
        Err(e) => (Vec::new(), Some(e.0)),
    }
}

/// Deny reason for an open-finding gate: quote the first finding (id + first
/// line) + the resolve remedy, plus a `[+N more]` count so nothing vanishes
/// silently.
pub(super) fn build_findings_block_reason(open: &[OpenFinding]) -> String {
    let f = &open[0];
    let more = if open.len() > 1 {
        format!(" [+{} more]", open.len() - 1)
    } else {
        String::new()
    };
    format!(
        "open review finding {}: {} - address it, then `fno backlog note --resolve {}`{}",
        f.id, f.first_line, f.id, more
    )
}

/// A `Covered(0)` that rests on a commit the object store could not measure
/// is not a known zero - it is an unread. Demote it to `Unknown` so the row
/// publishes pending and the gate recomputes it on its next read (the
/// recompute fetches the commit through the resolver). A `Covered(n > 0)` is
/// real reviews counted and is never demoted, which also keeps the
/// spent-budget discharge (always `n >= 1`) intact.
pub(super) fn demote_unmeasured_coverage(coverage: &mut Coverage, resolver: &FreshnessResolver) {
    if matches!(coverage, Coverage::Covered(0)) && !resolver.unmeasured().is_empty() {
        *coverage = Coverage::Unknown;
    }
}

/// The local attestation axis: every project-log rotation PLUS the global
/// journal's slug-scoped attestations. A review fork emits into its own
/// checkout's project log and mirrors to the global journal, and when the
/// fork's checkout dies the mirror alone survives (measured on PR 2137:
/// three attestations for one head, zero copies in any surviving project
/// log). Mirrors of rows the project log still holds are deduped, so a round
/// is never counted twice. An unreadable journal degrades to project-only.
pub(super) fn review_journal_text(
    events_path: &Path,
    global_events_path: &Path,
    repo_slug: &str,
) -> String {
    let project_text = crate::events_store::review_text(events_path);
    let global_text = crate::event_store::review_text(global_events_path);
    let extra_global = missing_global_attestations(&global_text, &project_text, repo_slug);
    if extra_global.is_empty() {
        project_text
    } else {
        format!("{project_text}\n{extra_global}")
    }
}
