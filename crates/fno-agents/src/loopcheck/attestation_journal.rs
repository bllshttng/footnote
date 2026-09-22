//! Where a review's attestation evidence comes from: the project log plus
//! the global journal's surviving mirrors, scoped and deduped.
use super::{
    attestation_in_scope, blockers_withhold, disposition_blockers_on_chain,
    line_carries_keyed_findings, zero_evidence_attestation, UnattestedReviewer,
};
use crate::review_freshness::Freshness;
use serde_json::Value;

/// One global-journal `review_attestation` line's identity: the tuple that
/// makes a mirror the SAME evidence as a project-log row, not a second
/// review round. A mirror adds `data.repo` and re-stamps `ts`; everything
/// that makes the attestation what it is, is in the tuple.
fn attestation_identity(val: &Value) -> Option<(String, String, String, String)> {
    Some((
        val.pointer("/data/reviewer")?
            .as_str()?
            .trim_start_matches('/')
            .to_string(),
        val.pointer("/data/head_sha")?.as_str()?.to_string(),
        val.pointer("/data/verdict")?
            .as_str()
            .unwrap_or("")
            .to_string(),
        val.pointer("/data/attester_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    ))
}

/// The global journal's `review_attestation` lines that this repo's project
/// log does not already hold. The local attestation axis reads the cwd
/// project log; a review fork emits into its own checkout's log AND mirrors
/// to the global journal, and when the fork's checkout is deleted the mirror
/// alone survives (measured on PR 2137: three attestations for one head,
/// zero copies in any surviving project log). Without this merge a producer
/// recomputing after the deletion grades zero local attestations and forces
/// the exact re-review the branch-scoped re-emits were meant to prevent.
///
/// Scoping is the same rule the Python replay path applies
/// (`cli/src/fno/pr/_reviews.py`, project unscoped + global scoped by the
/// full `host/owner/repo` identity): a global row is admitted only when
/// `data.repo` names THIS repo, and a row with no `repo` is never admitted -
/// unscoped evidence cannot prove which checkout it came from. Dedup keys on
/// the attestation's identity, so a mirrored copy of a row the project log
/// still holds never counts a second review round.
pub(super) fn missing_global_attestations(
    global_text: &str,
    project_text: &str,
    repo_slug: &str,
) -> String {
    if repo_slug.is_empty() {
        return String::new();
    }
    let mut seen: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();
    for line in project_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        if let Some(k) = attestation_identity(&val) {
            seen.insert(k);
        }
    }
    let mut out = String::new();
    for line in global_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        if val.pointer("/data/repo").and_then(|v| v.as_str()) != Some(repo_slug) {
            continue;
        }
        if let Some(k) = attestation_identity(&val) {
            if seen.insert(k) {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// The global journal's tail, at most `cap` bytes, starting on a line
/// boundary: a seek into the middle of a line drops that partial line, so
/// every admitted row is whole. Evidence older than the window is invisible
/// to the merge, which is acceptable by the same measure that makes the
/// merge correct: the failure it fixes (a deleted fork's surviving mirror)
/// is recent by construction, and the dedup keys on identity, not recency.
pub(super) const GLOBAL_TAIL_BYTES: u64 = 8 * 1024 * 1024;

// The reader itself lives at the crate root (`crate::tail_text`) so the
// dead-call crown reading reuses the one line-boundary tail walk.
pub(super) use crate::tail_text;

/// The text-taking body of [`unattested_reviewers_scan`], split so the
/// producer can feed the merged project-plus-global attestation text without
/// a temp file.
pub fn unattested_reviewers_scan_text(
    content: &str,
    reviewers: &[String],
    freshness: &dyn Fn(&str) -> Freshness,
    head_branch: &str,
    head_sha: &str,
    rounds_exhausted: bool,
) -> (Vec<UnattestedReviewer>, usize) {
    if reviewers.is_empty() {
        return (Vec::new(), 0);
    }
    let mut malformed = 0usize;
    // Single pass (gemini review): record the LATEST verdict per reviewer at the
    // current head. events.jsonl is append-ordered, so a later attestation
    // supersedes an earlier one for the same reviewer - a `fail` posted after a
    // `pass` must revoke it, and a re-run `pass` after a `fail` must restore it
    // (codex peer review P1: a later fail was previously ignored). A reviewer is
    // satisfied iff its latest head-pinned verdict is exactly `pass`. O(lines).
    let mut latest_pass: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    // reviewer -> every OLD head it attested at, in first-seen order, each
    // carrying that head's LATEST verdict. A single-entry "most recent pass"
    // map cannot survive a retraction: `pass A, pass B, fail B` overwrites A
    // with B and then drops B, reporting no prior pass while A is still a real
    // one (codex P2 on this PR). Multi-round review/fix cycles produce exactly
    // that sequence.
    let mut other_heads: std::collections::HashMap<String, Vec<(String, bool)>> =
        std::collections::HashMap::new();
    // per-reviewer answered-fail marks, collected in the SAME pass
    // (review findings 1-2): which reviewers' own fails raised keyed
    // findings, and whether a reviewer's latest counting line is a RETRACTION
    // (which revokes and never satisfies). Plus the in-scope chain for the
    // disposition read, so the whole scan parses the file once.
    let mut fail_carries: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    let mut latest_is_retraction: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    let mut chain: Vec<Value> = Vec::new();
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            if line.contains("review_attestation") {
                malformed += 1;
            }
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let Some(r) = val.pointer("/data/reviewer").and_then(|v| v.as_str()) else {
            continue;
        };
        let r = r.trim_start_matches('/').to_string();
        // An event with no `head_sha` is not head-pinned evidence and is
        // skipped outright. Defaulting it to "" would make it MATCH a caller
        // whose own head_sha is "", turning unpinned data into a pass (codex
        // P1 on this PR).
        let Some(line_head) = val.pointer("/data/head_sha").and_then(|v| v.as_str()) else {
            continue;
        };
        let is_pass = val.pointer("/data/verdict").and_then(|v| v.as_str()) == Some("pass");
        // The SAME scope predicate the coverage axis uses (attestation_in_scope),
        // applied before any freshness call: an attestation from another
        // branch is not evidence about this PR at all, so it must never reach
        // `latest_pass` or `other_heads` - recording it as a superseded head
        // would name a reviewer that never touched this PR.
        let line_branch = val
            .pointer("/data/branch")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !attestation_in_scope(line_branch, line_head, head_branch, head_sha) {
            continue;
        }
        chain.push(val.clone());
        let is_retraction = val
            .pointer("/data/retracts_attester")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !is_pass && !is_retraction && line_carries_keyed_findings(&val) {
            fail_carries.insert(r.clone(), true);
        }
        // The SAME zero-evidence predicate the coverage axis uses: a review
        // of nothing must not satisfy the config.review.reviewers gate either,
        // which is the surface that floors the self-review obligation.
        if zero_evidence_attestation(&val) {
            continue;
        }
        // The SAME predicate the coverage axis uses, not a second head-equality
        // rule beside it. Leaving this one a bare equality would have made the
        // softening decorative: this is the scan that satisfies
        // `config.review.reviewers`, so a rebase that carried the coverage
        // count would still have killed the required `code-review` entry and
        // demanded the re-review the carry exists to prevent.
        if !freshness(line_head).counts() {
            // Empty is not a head; recording it would put a `Some` in the
            // message with nothing to print.
            if line_head.is_empty() {
                continue;
            }
            let seen = other_heads.entry(r).or_default();
            match seen.iter().position(|(h, _)| h == line_head) {
                Some(i) => seen[i].1 = is_pass, // latest verdict wins for that head
                None => seen.push((line_head.to_string(), is_pass)),
            }
            continue;
        }
        latest_pass.insert(r.clone(), is_pass);
        latest_is_retraction.insert(r, is_retraction);
    }
    // a latest-`fail` reviewer is satisfied when its OWN fails raised
    // keyed findings AND the chain's blocking findings are all terminal (or
    // cap-filed, which only hard findings survive). A bystander's
    // findings-free fail never rides another reviewer's dispositions, and a
    // RETRACTION latest never satisfies - it revokes, it never covers.
    let any_answerable_fail = latest_pass
        .iter()
        .any(|(name, p)| !*p && fail_carries.get(name) == Some(&true));
    let disposition_clear = any_answerable_fail && {
        let blockers = disposition_blockers_on_chain(&chain);
        !blockers_withhold(&blockers, rounds_exhausted)
    };
    let out = reviewers
        .iter()
        .map(|entry| entry.trim_start_matches('/'))
        .filter(|name| match latest_pass.get(*name) {
            Some(true) => false,
            Some(false) => {
                !(disposition_clear
                    && latest_is_retraction.get(*name) != Some(&true)
                    && fail_carries.get(*name) == Some(&true))
            }
            // No attestation from this reviewer AT THIS HEAD. Held while the
            // budget can still fund a round - one review stays the floor, so
            // an unreviewed PR is still unattested and rounds_exhausted is
            // false at zero rounds.
            //
            // Past the cap it must yield, for the same reason the Some(false)
            // arm beside it already does: the demand is unsatisfiable there.
            // The only thing that clears "attest at this head" is another
            // review round, and the budget will not fund one. Worse, this arm
            // is re-armed by every FIX: an attestation is head-pinned, so
            // closing the findings from round 2 moves HEAD and voids it. That
            // is the treadmill the round cap exists to end, and leaving it
            // here would keep the stop gate demanding rounds the merge gate
            // has already discharged.
            None => !rounds_exhausted,
        })
        .map(|name| UnattestedReviewer {
            name: name.to_string(),
            // An old head whose LATEST verdict is still a pass. Heads keep
            // first-seen order, so a head re-attested later keeps its original
            // slot and this may name a slightly older one - both are real
            // passes, so the line stays true either way. Only a pass is worth
            // naming: an old-head `fail` rendered as "passed at X, superseded"
            // would imply a successful review that never happened.
            superseded_head: other_heads
                .get(name)
                .and_then(|heads| heads.iter().rev().find(|(_, ok)| *ok))
                .map(|(h, _)| h.clone()),
            failed_at_head: latest_pass.get(name) == Some(&false),
        })
        .collect();
    (out, malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopcheck::{
        classify_coverage_tiled, local_latest_attestations, AttestationScope, Coverage,
        CoverageProducer,
    };

    // ── the fork log died, the mirror survived (global-journal merge) ──

    /// A mirrored attestation: what `emit_to_both` leaves in the global
    /// journal after the project-log copy died with a deleted review
    /// checkout. Measured on PR 2137: three attestations for one head, zero
    /// copies in any surviving project log.
    fn global_attest_line(ts: &str, head: &str, branch: &str, repo: &str) -> String {
        format!(
                "{{\"ts\":\"{ts}\",\"type\":\"review_attestation\",\"source\":\"subagent\",\"data\":{{\"reviewer\":\"code-review\",\"head_sha\":\"{head}\",\"verdict\":\"pass\",\"branch\":\"{branch}\",\"attester_session_id\":\"fork-session\",\"repo\":\"{repo}\"}}}}"
            )
    }

    #[test]
    fn global_journal_rows_reach_the_scan_when_the_project_log_holds_none() {
        let global = format!(
            "{}\n",
            global_attest_line(
                "2026-09-17T17:11:47Z",
                "954ee57fed",
                "feature/gonefork",
                "github.com/bllshttng/footnote"
            )
        );
        let merged = missing_global_attestations(&global, "", "github.com/bllshttng/footnote");
        let (passes, _) = local_latest_attestations(&merged, "feature/gonefork", "954ee57fed");
        assert_eq!(passes.len(), 1, "the surviving mirror must grade");
        assert_eq!(passes[0].branch, "feature/gonefork");
        assert!(passes[0].is_pass);
    }

    #[test]
    fn global_journal_rows_are_repo_scoped_and_deduped_against_the_project_log() {
        let mine = global_attest_line("t1", "aaaaaaaaaa", "feature/x", "github.com/o/r");
        let foreign = global_attest_line("t2", "bbbbbbbbbb", "other/branch", "github.com/o/other");
        let unscoped = "{\"type\":\"review_attestation\",\"data\":{\"reviewer\":\"code-review\",\"head_sha\":\"cccccccccc\",\"verdict\":\"pass\"}}";
        let global = format!("{mine}\n{foreign}\n{unscoped}\n");
        // A repo-less global row is unscoped evidence: never admitted.
        // A foreign-repo row describes another checkout's work: never admitted.
        let merged = missing_global_attestations(&global, "", "github.com/o/r");
        assert_eq!(merged, format!("{mine}\n"));
        // The project log already holds the same attestation identity: the
        // mirror must not double it.
        let merged = missing_global_attestations(&global, &format!("{mine}\n"), "github.com/o/r");
        assert_eq!(merged, String::new());
        // No repo identity resolved: the global arm contributes nothing.
        assert_eq!(missing_global_attestations(&global, "", ""), String::new());
    }

    #[test]
    fn coverage_grades_a_gone_fork_pass_from_the_global_journal_alone() {
        let global = format!(
            "{}\n",
            global_attest_line(
                "2026-09-17T17:11:47Z",
                "954ee57fed",
                "feature/gonefork",
                "github.com/bllshttng/footnote"
            )
        );
        let merged = missing_global_attestations(&global, "", "github.com/bllshttng/footnote");
        let coverage = classify_coverage_tiled(
            &[],
            &[],
            &merged,
            &[],
            false,
            None,
            &|_| Freshness::Fresh,
            "feature/gonefork",
            "954ee57fed",
            None,
            None,
            false,
        );
        assert!(matches!(coverage.coverage, Coverage::Covered(n) if n > 0));
        let local: Vec<_> = coverage
            .verdicts
            .iter()
            .filter(|v| v.producer == CoverageProducer::LocalAttestation)
            .collect();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].scope, Some(AttestationScope::AttestedBranch));
    }

    #[test]
    fn tail_text_starts_on_a_line_boundary_and_drops_no_whole_row() {
        let dir = std::env::temp_dir().join(format!(
            "fno-tail-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let first = format!(
            "{}\n",
            global_attest_line("t1", "aaaaaaaaaa", "feature/x", "github.com/o/r")
        );
        let second = format!(
            "{}\n",
            global_attest_line("t2", "bbbbbbbbbb", "feature/y", "github.com/o/r")
        );
        let body = format!("{first}{second}");
        std::fs::write(&path, &body).unwrap();
        // A window that begins inside the FIRST line must drop that partial
        // line and still return the whole second row.
        let tail = tail_text(&path, (second.len() + 5) as u64);
        assert_eq!(tail, second, "partial head line dropped, second row whole");
        // A window covering everything returns everything.
        let tail = tail_text(&path, body.len() as u64);
        assert_eq!(tail, body);
        std::fs::remove_dir_all(&dir).ok();
    }
}
