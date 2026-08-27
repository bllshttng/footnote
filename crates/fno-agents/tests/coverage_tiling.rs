//! Range tiling over the attestation chain (AC4-HP / AC4-ERR / AC4-EDGE).
//!
//! These tests run the REAL git binary against a REAL repo, because the
//! predicate under test IS a git walk: `git rev-list --ancestry-path` over
//! merge_base..head. A mock that echoes shas proves nothing about ancestry.
//! The attestation fixtures are hand-written in the post-1170 event shape
//! (branch, head_sha, reviewed_base_sha, reviewed_head_sha) - the shape this
//! repo's own emitter produces at HEAD source, which is what the tiling walk
//! is built against.
//!
//! Also pins the live-specimen regressions from 2026-08-25 (ruling
//! d-747656e4's writeup): a same-pair re-attest after a push keeps the NEWER
//! pass, and distinct attesters yield distinct verdicts. The live incident
//! was a stale served row, not logic, and these tests keep it that way.

use fno_agents::loopcheck::{
    classify_coverage_tiled, compute_range_tiling, coverage_receipt_line, Coverage,
    CoverageProducer, CoverageVerdict, Freshness, RangeTiling, ReviewState,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const BRANCH: &str = "feature/x";

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(repo: &Path, name: &str) -> String {
    fs::write(repo.join(format!("{name}.txt")), name).unwrap();
    git(repo, &["add", &format!("{name}.txt")]);
    git(repo, &["commit", "-qm", name]);
    git(repo, &["rev-parse", "HEAD"])
}

/// A repo with one base commit on origin/main plus `n` feature commits on
/// BRANCH. Returns (merge_base, [commit shas oldest-first], head).
fn repo_with(repo: &Path, n: usize) -> (String, Vec<String>, String) {
    Command::new("git")
        .current_dir(repo)
        .args(["init", "-q", "-b", BRANCH])
        .output()
        .expect("git init");
    git(repo, &["config", "user.email", "t@t.t"]);
    git(repo, &["config", "user.name", "t"]);
    let base = commit(repo, "base");
    let main_ref = "refs/remotes/origin/main".to_string();
    git(repo, &["update-ref", &main_ref, &base]);
    let mut shas = Vec::new();
    for i in 1..=n {
        shas.push(commit(repo, &format!("c{i}")));
    }
    let head = shas.last().cloned().unwrap_or_else(|| base.clone());
    (base, shas, head)
}

/// One review_attestation line in the full post-1170 shape.
fn attestation(reviewer: &str, base: &str, head: &str, verdict: &str) -> String {
    serde_json::json!({
        "ts": "2026-08-25T23:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": reviewer,
            "head_sha": head,
            "verdict": verdict,
            "session_id": "s-run",
            "attester_session_id": "sess-a",
            "branch": BRANCH,
            "reviewed_base_sha": base,
            "reviewed_head_sha": head,
            "reviewed_file_count": 3,
            "reviewed_line_count": 40,
        },
    })
    .to_string()
}

fn events_file(repo: &Path, lines: &[String]) -> String {
    let text = lines.join("\n") + "\n";
    fs::write(repo.join("events.jsonl"), &text).unwrap();
    text
}

fn tiling_for(repo: &Path, events: &str) -> RangeTiling {
    compute_range_tiling(
        "git",
        repo,
        "origin/main",
        events,
        BRANCH,
        &git(repo, &["rev-parse", "HEAD"]),
        2,
    )
}

#[test]
fn ac4_hp_three_ranges_tile_with_an_empty_gap_set() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    let events = events_file(
        repo,
        &[
            attestation("code-review", &base, &shas[0], "pass"),
            attestation("code-review", &shas[0], &shas[2], "pass"),
            attestation("code-review", &shas[2], &head, "pass"),
        ],
    );
    let tiling = tiling_for(repo, &events);
    assert!(tiling.tiled, "chain must tile: {:?}", tiling.gaps);
    // The LITERAL empty gap list, not merely a covered verdict, so a tiler
    // that returned tiled for the wrong reason fails here.
    assert_eq!(tiling.gaps, Vec::<(String, String)>::new());
    assert!(tiling.dropped.is_empty());
    assert_eq!(tiling.chain_heads.len(), 3);
}

#[test]
fn ac4_err_missing_middle_range_names_the_gap_by_sha() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // The middle attestation removed: nothing covers c2 or c3.
    let events = events_file(
        repo,
        &[
            attestation("code-review", &base, &shas[0], "pass"),
            attestation("code-review", &shas[2], &head, "pass"),
        ],
    );
    let tiling = tiling_for(repo, &events);
    assert!(!tiling.tiled);
    // The gap is named by sha: parent-of-first-uncovered (c1) .. last
    // uncovered (c3). "Run a review at HEAD" is the instruction that caused
    // the loop; the refusal must name the range instead.
    assert_eq!(tiling.gaps, vec![(shas[0].clone(), shas[2].clone())]);
}

#[test]
fn ac4_edge_off_branch_and_unresolvable_ranges_are_dropped_by_sha() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 3);

    // A range from a REBASED-AWAY history: build a side branch off the base,
    // then delete it, so its shas are not ancestors of head.
    git(repo, &["checkout", "-q", "-b", "gone", &base]);
    let orphan = commit(repo, "orphan");
    git(repo, &["checkout", "-q", BRANCH]);
    git(repo, &["branch", "-D", "gone"]);

    // A range whose head sha no longer resolves: a well-formed 40-hex that
    // names no object in the repo.
    let unresolvable = "0123456789abcdef0123456789abcdef01234567";

    let events = events_file(
        repo,
        &[
            attestation("code-review", &orphan, &shas[0], "pass"),
            attestation("code-review", &base, unresolvable, "pass"),
            attestation("code-review", &shas[0], &head, "pass"),
        ],
    );
    let tiling = tiling_for(repo, &events);
    // Both bad ranges are dropped, reported by sha (the unresolvable head,
    // and the off-branch range's head).
    assert!(
        tiling.dropped.contains(&shas[0].to_string()),
        "off-branch range head dropped: {:?}",
        tiling.dropped
    );
    assert!(
        tiling.dropped.contains(&unresolvable.to_string()),
        "unresolvable head dropped: {:?}",
        tiling.dropped
    );
    // The remaining chain is judged on its own: it does not reach the base,
    // so c1 is uncovered and the chain is not tiled.
    assert!(!tiling.tiled);
    assert_eq!(tiling.gaps, vec![(base.clone(), shas[0].clone())]);
}

#[test]
fn fail_verdicts_tile_too_coverage_counts_what_was_read() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // A review that FOUND things (fail) still read its range; the disposition
    // gate, not coverage, is what its findings must satisfy.
    let events = events_file(
        repo,
        &[
            attestation("code-review", &base, &shas[1], "fail"),
            attestation("code-review", &shas[1], &head, "pass"),
        ],
    );
    let tiling = tiling_for(repo, &events);
    assert!(tiling.tiled);
    assert_eq!(tiling.gaps, Vec::<(String, String)>::new());
}

#[test]
fn chain_members_count_as_reviewed_even_when_individually_stale() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // Three attestations tile the chain; only the last pins the head, so the
    // first two are individually stale under the single-sha rule.
    let events = events_file(
        repo,
        &[
            attestation("code-review", &base, &shas[0], "pass"),
            attestation("code-review", &shas[0], &shas[2], "pass"),
            attestation("code-review", &shas[2], &head, "pass"),
        ],
    );
    let tiling = tiling_for(repo, &events);
    assert!(tiling.tiled);
    let all_stale = |_: &str| Freshness::Stale;
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &all_stale,
        BRANCH,
        &head,
        Some(&tiling),
        None,
        false,
    );
    let local: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    // The pair key collapses the three same-pair attestations to their
    // latest (the head-pinned one); it counts as Reviewed ONLY through the
    // chain - its own freshness reads stale by the closure above.
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].verdict, CoverageVerdict::Reviewed);
    assert_eq!(local[0].reviewed_sha, head);

    // Without the tiling answer, the same closure leaves it Stale: the
    // pre-tiling rule stands alone when no chain is supplied.
    let rep_untiled = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &all_stale,
        BRANCH,
        &head,
        None,
        None,
        false,
    );
    let local_untiled: Vec<_> = rep_untiled
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(local_untiled[0].verdict, CoverageVerdict::Stale);
}

#[test]
fn a_gapped_chain_does_not_rescue_its_members() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    let events = events_file(
        repo,
        &[
            attestation("code-review", &base, &shas[0], "pass"),
            attestation("code-review", &shas[2], &head, "pass"),
        ],
    );
    let tiling = tiling_for(repo, &events);
    assert!(!tiling.tiled);
    let all_stale = |_: &str| Freshness::Stale;
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &all_stale,
        BRANCH,
        &head,
        Some(&tiling),
        None,
        false,
    );
    assert!(rep
        .verdicts
        .iter()
        .all(|v| v.verdict != CoverageVerdict::Reviewed));
}

#[test]
fn same_pair_reattest_keeps_the_newer_head() {
    // The live-specimen regression (2026-08-25 writeup, ruling d-747656e4):
    // a session re-attests after pushing a fix. The pair key collapses the
    // two attestations and the LATER one must win - its head, its freshness.
    // Assert the selected sha, then the verdict.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 4);
    let events = events_file(
        repo,
        &[
            attestation("code-review", &shas[1], &shas[2], "pass"),
            attestation("code-review", &shas[1], &head, "pass"),
        ],
    );
    let at_head = |sha: &str| {
        if sha == head {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        BRANCH,
        &head,
        None,
        None,
        false,
    );
    let local: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(local.len(), 1, "same pair collapses to one verdict");
    assert_eq!(
        local[0].reviewed_sha, head,
        "the NEWER attestation must win the pair"
    );
    assert_eq!(local[0].verdict, CoverageVerdict::Reviewed);
    assert_eq!(local[0].freshness, Some(Freshness::Fresh));
}

#[test]
fn distinct_attesters_yield_distinct_verdicts() {
    // The other half of the specimen: two sessions attesting under one
    // reviewer label coexist (the attester lives in the pair key).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    let older = attestation("code-review", &base, &shas[1], "pass");
    let newer =
        attestation("code-review", &shas[1], &head, "pass").replace("\"sess-a\"", "\"sess-b\"");
    let events = events_file(repo, &[older, newer]);
    let all_stale = |_: &str| Freshness::Stale;
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &all_stale,
        BRANCH,
        &head,
        None,
        None,
        false,
    );
    let local: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(local.len(), 2, "distinct attesters keep distinct verdicts");
}

// --- The disposition-complete pass condition, Rust gate side (AC5) ---------

use fno_agents::loopcheck::disposition_blockers;

fn dispositions_event(
    head: &str,
    findings: serde_json::Value,
    dispositions: serde_json::Value,
    truncated: bool,
) -> String {
    let mut data = serde_json::json!({
        "reviewer": "code-review",
        "head_sha": head,
        "verdict": "fail",
        "session_id": "s",
        "branch": BRANCH,
        "reviewed_file_count": 2,
        "reviewed_line_count": 20,
        "findings": findings,
    });
    if dispositions
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false)
    {
        data["dispositions"] = dispositions;
    }
    if truncated {
        data["findings_truncated"] = serde_json::json!(true);
    }
    serde_json::json!({"ts": "2026-08-25T22:00:00Z", "type": "review_attestation", "source": "hook", "data": data}).to_string()
}

fn finding(key: &str, category: &str, verdict: Option<&str>, blocking: bool) -> serde_json::Value {
    serde_json::json!({
        "category": category,
        "verdict": verdict,
        "blocking": blocking,
        "has_required_fields": true,
        "finding_key": key,
    })
}

const SPECIMEN_HEAD: &str = "46695fffd00000000000000000000000000000000";
const ROUND1_HEAD: &str = "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3";

fn specimen_events() -> String {
    // The byte-pinned shape of ruling d-fc3b3837's specimen: five codex
    // findings in round 1, five fixes, dispositions recorded on the round
    // that reviewed the fix delta.
    let keys = [
        "cli/src/fno/pr/_reviews.py:88:correctness",
        "cli/src/fno/pr/_merge.py:1411:correctness",
        "hooks/git-protection.py:302:security",
        "crates/fno-agents/src/loopcheck.rs:5355:correctness",
        "skills/review/scripts/emit-attestation.sh:273:correctness",
    ];
    let findings: Vec<_> = keys
        .iter()
        .map(|k| finding(k, "correctness", None, true))
        .collect();
    let dispositions: Vec<_> = keys
        .iter()
        .map(|k| {
            serde_json::json!({"finding_key": k, "disposition": "fixed", "reason": "commit abc"})
        })
        .collect();
    [
        dispositions_event(
            ROUND1_HEAD,
            serde_json::json!(findings),
            serde_json::json!([]),
            false,
        ),
        dispositions_event(
            SPECIMEN_HEAD,
            serde_json::json!([]),
            serde_json::json!(dispositions),
            false,
        ),
    ]
    .join("\n")
        + "\n"
}

#[test]
fn ac5_marker_specimen_blocks_nothing() {
    let blockers = disposition_blockers(&specimen_events(), BRANCH, SPECIMEN_HEAD, true);
    assert_eq!(blockers, Vec::new());
}

#[test]
fn ac5b_marker_one_open_finding_blocks_by_key() {
    let mut events = specimen_events();
    events.push('\n');
    events.push_str(&dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([finding(
            "cli/src/fno/pr/_coverage_gate.py:999:correctness",
            "correctness",
            None,
            true
        )]),
        serde_json::json!([]),
        false,
    ));
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, true);
    assert_eq!(blockers.len(), 1);
    assert_eq!(
        blockers[0].finding_key,
        "cli/src/fno/pr/_coverage_gate.py:999:correctness"
    );
    assert_eq!(blockers[0].axis, "open");
}

#[test]
fn declined_without_corroboration_blocks() {
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([finding("sec.rs:1:security", "security", None, true)]),
        serde_json::json!([serde_json::json!({
            "finding_key": "sec.rs:1:security",
            "disposition": "declined",
            "reason": "not applicable here",
        })]),
        false,
    )]
    .join("\n");
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, true);
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].axis, "declined-uncorroborated");

    let clean = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, false);
    assert_eq!(clean, Vec::new());
}

#[test]
fn fixed_in_the_last_round_is_unreviewed() {
    // A fixed disposition recorded on the SAME round that raised the
    // finding: the fix delta was never reviewed.
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([finding("a.py:1:correctness", "correctness", None, true)]),
        serde_json::json!([serde_json::json!({
            "finding_key": "a.py:1:correctness",
            "disposition": "fixed",
            "reason": "same round",
        })]),
        false,
    )]
    .join("\n");
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, false);
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].axis, "fixed-unreviewed");
}

#[test]
fn producer_count_is_never_the_answer() {
    // AC5-EDGE's twin at the gate: an event claiming zero blocking over a
    // CONFIRMED finding tagged style. The gate re-derives; the count is
    // refused.
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([finding("lie.py:1:style", "style", Some("CONFIRMED"), false)]),
        serde_json::json!([]),
        false,
    )]
    .join("\n");
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, false);
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].finding_key, "lie.py:1:style");
}

#[test]
fn nonblocking_by_class_needs_no_disposition() {
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([
            finding("b.py:2:nit", "nit", None, false),
            finding("c.py:3:typo", "typo", None, false),
        ]),
        serde_json::json!([]),
        false,
    )]
    .join("\n");
    assert_eq!(
        disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, false),
        Vec::new()
    );
}

#[test]
fn truncated_remainder_blocks() {
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([finding("a.py:1:typo", "typo", None, false)]),
        serde_json::json!([]),
        true,
    )]
    .join("\n");
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, false);
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].axis, "truncated-remainder");
}

#[test]
fn empty_chain_blocks_nothing() {
    assert_eq!(
        disposition_blockers("", BRANCH, SPECIMEN_HEAD, true),
        Vec::new()
    );
}

// --- AC6: a non-author GitHub approval is a sufficient producer ---

/// One human APPROVED review object in the `gh pr view --json reviews` shape
/// (the same shape the REST adapter normalizes to).
fn approval(login: &str, oid: &str) -> serde_json::Value {
    serde_json::json!({
        "author": {"login": login},
        "state": "APPROVED",
        "submittedAt": "2026-08-25T23:30:00Z",
        "commit": {"oid": oid},
        "body": "",
    })
}

fn classify_approval(
    reviews: &[serde_json::Value],
    pr_author: Option<&str>,
    flag: bool,
    fresh: Freshness,
) -> fno_agents::loopcheck::CoverageReport {
    let head = "h1";
    classify_coverage_tiled(
        reviews,
        &[],
        "",
        &[],
        true,
        None,
        &|_| fresh,
        BRANCH,
        head,
        None,
        pr_author,
        flag,
    )
}

#[test]
fn github_approval_counts_when_flag_on_and_approver_is_not_the_author() {
    // AC6-HP: alice's PR, bob's APPROVED review pinned to the current head,
    // no local attestation at all. Default-config direction (flag on): the
    // approval covers on its own and corroborates by construction.
    let rep = classify_approval(
        &[approval("bob", "h1")],
        Some("alice"),
        true,
        Freshness::Fresh,
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    assert_eq!(rep.review_state(), Some(ReviewState::Reviewed));
    let bob = rep
        .verdicts
        .iter()
        .find(|v| v.name == "bob")
        .expect("bob's verdict is recorded");
    assert!(bob.human_approval && !bob.author_approval);
    assert_eq!(bob.producer, CoverageProducer::GithubApp);
    // The receipt's counted list names bob: "1 reviewed (bob)".
    let line = coverage_receipt_line(&rep, None, None);
    assert!(line.contains("1 reviewed (bob)"), "receipt was: {line}");
    // Corroboration falls out: a counted human approval is by construction
    // not the author's own attestation.
    assert!(!rep.rests_on_self_attestation_alone());
}

#[test]
fn github_approval_by_the_pr_author_is_recorded_but_never_counted() {
    // AC6-ERR: alice approving her own PR. The verdict stays on the list
    // (auditable) and the counted set stays literally empty, so a run where
    // no approval was collected at all cannot pass this test either.
    let rep = classify_approval(
        &[approval("alice", "h1")],
        Some("alice"),
        true,
        Freshness::Fresh,
    );
    assert!(rep
        .verdicts
        .iter()
        .any(|v| v.name == "alice" && v.human_approval && v.author_approval));
    assert_eq!(rep.coverage_count(), Some(0));
    assert_eq!(rep.review_state(), Some(ReviewState::Unreviewed));
}

#[test]
fn github_approval_flag_off_keeps_todays_exclusion() {
    // AC6-EDGE: github_approval_satisfies = false. bob's approval is still
    // RECORDED on the verdict list and still excluded from the count.
    let rep = classify_approval(
        &[approval("bob", "h1")],
        Some("alice"),
        false,
        Freshness::Fresh,
    );
    assert!(rep
        .verdicts
        .iter()
        .any(|v| v.name == "bob" && v.human_approval && !v.author_approval));
    assert_eq!(rep.coverage_count(), Some(0));
    assert_eq!(rep.review_state(), Some(ReviewState::Unreviewed));
}

#[test]
fn github_approval_with_unreadable_pr_author_fails_closed() {
    // An unreadable PR author cannot prove the approver is not the author,
    // so the fail-closed direction is "exclude", never "count".
    let rep = classify_approval(&[approval("bob", "h1")], None, true, Freshness::Fresh);
    assert!(rep
        .verdicts
        .iter()
        .any(|v| v.name == "bob" && v.human_approval && v.author_approval));
    assert_eq!(rep.coverage_count(), Some(0));
}

#[test]
fn github_approval_stale_review_is_not_counted() {
    // The freshness rule applies unchanged: an approval whose commit is not
    // fresh reads Stale and never counts, flag or no flag.
    let rep = classify_approval(
        &[approval("bob", "h0")],
        Some("alice"),
        true,
        Freshness::Stale,
    );
    assert!(rep
        .verdicts
        .iter()
        .any(|v| v.name == "bob" && v.verdict == CoverageVerdict::Stale));
    assert_eq!(rep.coverage_count(), Some(0));
}

// --- AC7: the round budget ---

fn attestation_round(
    reviewer: &str,
    base: &str,
    head: &str,
    verdict: &str,
    review_round: Option<i64>,
) -> String {
    let mut data = serde_json::json!({
        "reviewer": reviewer,
        "head_sha": head,
        "verdict": verdict,
        "session_id": "s-run",
        "attester_session_id": "sess-a",
        "branch": BRANCH,
        "reviewed_base_sha": base,
        "reviewed_head_sha": head,
        "reviewed_file_count": 3,
        "reviewed_line_count": 40,
    });
    if let Some(n) = review_round {
        data["review_round"] = serde_json::json!(n);
    }
    serde_json::json!({
        "ts": "2026-08-25T23:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": data,
    })
    .to_string()
}

#[test]
fn round_budget_counts_verdicts_since_the_last_pass() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, _head) = repo_with(repo, 4);
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &base, &shas[0], "fail", None),
            attestation_round("code-review", &shas[0], &shas[1], "fail", None),
            attestation_round("code-review", &shas[1], &shas[2], "pass", None),
            attestation_round("code-review", &shas[2], &shas[3], "fail", None),
        ],
    );
    // The pass resets: one round since.
    assert_eq!(rounds_since_last_pass(&events, BRANCH, &shas[3], None), 1);
    // Drop the pass from the chain: three rounds.
    let no_pass = events_file(
        repo,
        &[
            attestation_round("code-review", &base, &shas[0], "fail", None),
            attestation_round("code-review", &shas[0], &shas[1], "fail", None),
            attestation_round("code-review", &shas[1], &shas[2], "fail", None),
        ],
    );
    assert_eq!(rounds_since_last_pass(&no_pass, BRANCH, &shas[2], None), 3);
}

#[test]
fn round_budget_declared_review_round_wins_when_present() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 3);
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &base, &shas[0], "fail", Some(1)),
            attestation_round("code-review", &shas[0], &shas[1], "fail", Some(2)),
            attestation_round("code-review", &shas[1], &head, "fail", Some(3)),
        ],
    );
    assert_eq!(
        rounds_since_last_pass(&events, BRANCH, head.as_str(), None),
        3
    );
}

#[test]
fn round_budget_off_branch_events_do_not_count() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 2);
    let other = attestation_round("code-review", &base, &shas[0], "fail", None)
        .replace("\"feature/x\"", "\"feature/someone-else\"");
    let events = events_file(
        repo,
        &[
            other,
            attestation_round("code-review", &base, &head, "fail", None),
        ],
    );
    // Only the on-branch verdict counts: one round, not two.
    assert_eq!(
        rounds_since_last_pass(&events, BRANCH, head.as_str(), None),
        1
    );
}

#[test]
fn round_budget_is_computed_even_when_tiling_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 3);
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &base, &shas[0], "fail", None),
            attestation_round("code-review", &shas[0], &shas[1], "fail", None),
            attestation_round("code-review", &shas[1], &head, "fail", None),
        ],
    );
    // An unresolvable base ref answers tiling fail-closed, but the round
    // budget does not depend on the git walk: three rounds, exhausted.
    let tiling = compute_range_tiling(
        "git",
        repo,
        "refs/remotes/origin/nope",
        &events,
        BRANCH,
        &head,
        2,
    );
    assert!(!tiling.tiled);
    assert_eq!(tiling.rounds_used, 3);
    assert!(tiling.rounds_exhausted);
}

#[test]
fn round_budget_exhausted_needs_more_than_max() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 3);
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &base, &shas[0], "fail", None),
            attestation_round("code-review", &shas[0], &head, "fail", None),
        ],
    );
    let tiling = compute_range_tiling("git", repo, "origin/main", &events, BRANCH, &head, 2);
    assert_eq!(tiling.rounds_used, 2);
    assert!(
        !tiling.rounds_exhausted,
        "max_rounds is a budget, not an off-by-one"
    );
}

// --- rounds the attestation chain never saw: the GitHub review axis ---

/// One `gh pr view --json reviews` review object.
fn review_object(login: &str, state: &str, commit: &str, submitted_at: &str) -> serde_json::Value {
    serde_json::json!({
        "author": {"login": login},
        "state": state,
        "commit": {"oid": commit},
        "submittedAt": submitted_at,
    })
}

const CONNECTOR: &str = "chatgpt-codex-connector[bot]";
const PR_AUTHOR: &str = "bllshttng";

#[test]
fn round_budget_counts_rounds_that_only_github_review_objects_saw() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 4);
    // The connector lane: three review rounds, every one ended with
    // findings, NO attestation row exists anywhere on the branch. Each fix
    // moved the head and the connector reviewed the new head, so the rounds
    // exist only as three distinct reviewed commits. Today this answers 0
    // and the cap cannot fire; it must answer 3.
    let events = events_file(repo, &[]);
    let reviews = vec![
        review_object(CONNECTOR, "COMMENTED", &shas[0], "2026-08-26T11:00:00Z"),
        review_object(CONNECTOR, "COMMENTED", &shas[1], "2026-08-26T13:00:00Z"),
        review_object(CONNECTOR, "COMMENTED", &shas[2], "2026-08-26T15:00:00Z"),
    ];
    assert_eq!(
        rounds_since_last_pass(&events, BRANCH, &head, Some(&reviews)),
        3
    );
}

#[test]
fn round_budget_pass_resets_the_github_axis_too() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 4);
    // A clean pass at 12:00 resets both axes: the connector review at 11:00
    // is a spent round, the two reviews after the pass are fresh rounds.
    // Answer 2, never 3.
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &shas[0], &shas[1], "fail", None),
            {
                let mut row: serde_json::Value = serde_json::from_str(&attestation_round(
                    "code-review",
                    &shas[1],
                    &shas[2],
                    "pass",
                    None,
                ))
                .unwrap();
                row["ts"] = serde_json::json!("2026-08-26T12:00:00Z");
                row.to_string()
            },
        ],
    );
    let reviews = vec![
        review_object(CONNECTOR, "COMMENTED", &shas[0], "2026-08-26T11:00:00Z"),
        review_object(CONNECTOR, "COMMENTED", &shas[2], "2026-08-26T13:00:00Z"),
        review_object(CONNECTOR, "COMMENTED", &shas[3], "2026-08-26T15:00:00Z"),
    ];
    assert_eq!(
        rounds_since_last_pass(&events, BRANCH, &head, Some(&reviews)),
        2
    );
}

#[test]
fn round_budget_drops_the_github_axis_when_the_pass_has_no_ts() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 4);
    // A pass with no readable ts leaves nothing to filter the reviews axis
    // by. Counting the whole review history there would fire the cap on a
    // budget this very pass just defused. All three reviews below predate
    // the pass, so the honest answer is the events-only 0; an unfiltered
    // read answers 3, and that 3 is the regression this pins.
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &shas[0], &shas[1], "fail", None),
            {
                let mut row: serde_json::Value = serde_json::from_str(&attestation_round(
                    "code-review",
                    &shas[1],
                    &shas[2],
                    "pass",
                    None,
                ))
                .unwrap();
                row.as_object_mut().unwrap().remove("ts");
                row.to_string()
            },
        ],
    );
    let reviews = vec![
        review_object(CONNECTOR, "COMMENTED", &shas[0], "2026-08-26T09:00:00Z"),
        review_object(CONNECTOR, "COMMENTED", &shas[1], "2026-08-26T09:30:00Z"),
        review_object(CONNECTOR, "COMMENTED", &shas[2], "2026-08-26T09:45:00Z"),
    ];
    assert_eq!(
        rounds_since_last_pass(&events, BRANCH, &head, Some(&reviews)),
        0
    );
}

#[test]
fn round_budget_counts_review_objects_posted_under_the_pr_author_login() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 4);
    // The measured specimen: the codex cloud connector posts its review
    // objects under the PR AUTHOR's own login - 116 of 117 objects on the
    // branch that spun, one burst per reviewed commit, each body opening
    // with the connector's own review banner. An author filter deletes the
    // round trace on exactly that lane, so there is none: three bursts at
    // three distinct commits under the author login are three rounds, and
    // reply volume inside one burst is one round.
    let events = events_file(repo, &[]);
    let reviews = vec![
        review_object(PR_AUTHOR, "COMMENTED", &shas[0], "2026-08-26T11:00:00Z"),
        review_object(PR_AUTHOR, "COMMENTED", &shas[0], "2026-08-26T11:05:00Z"),
        review_object(PR_AUTHOR, "COMMENTED", &shas[1], "2026-08-26T12:00:00Z"),
        review_object(PR_AUTHOR, "COMMENTED", &shas[2], "2026-08-26T13:00:00Z"),
    ];
    assert_eq!(
        rounds_since_last_pass(&events, BRANCH, &head, Some(&reviews)),
        3,
        "reply volume at one commit is one round; three commits are three"
    );
}

#[test]
fn round_budget_takes_the_max_not_the_sum_of_both_axes() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 3);
    // A healthy lane leaves BOTH traces per round: a fail attestation and a
    // connector review of the same head. Two rounds, not four.
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &shas[0], &shas[1], "fail", None),
            attestation_round("code-review", &shas[1], &shas[2], "fail", None),
        ],
    );
    let reviews = vec![
        review_object(CONNECTOR, "COMMENTED", &shas[1], "2026-08-26T11:00:00Z"),
        review_object(CONNECTOR, "COMMENTED", &shas[2], "2026-08-26T12:00:00Z"),
    ];
    assert_eq!(
        rounds_since_last_pass(&events, BRANCH, &head, Some(&reviews)),
        2
    );
}

#[test]
fn round_budget_no_reviews_evidence_keeps_the_events_only_answer() {
    use fno_agents::loopcheck::rounds_since_last_pass;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 3);
    // The no-external lane passes no review payload: behavior is exactly
    // today's events-only answer.
    let events = events_file(
        repo,
        &[
            attestation_round("code-review", &shas[0], &shas[1], "fail", None),
            attestation_round("code-review", &shas[1], &shas[2], "fail", None),
        ],
    );
    assert_eq!(rounds_since_last_pass(&events, BRANCH, &head, None), 2);
}

// --- the round cap under the operator's ruling: file the rest, keep the hard ---

/// The three admission rules the pass scan applies, on the spent-budget fail
/// arm that re-implements them. Each case is a chain that tiles with the
/// budget spent, differing only in the one row the arm must refuse.
fn cap_verdict_count_for(repo: &std::path::Path, rows: &[String]) -> usize {
    let events = events_file(repo, rows);
    let tiling = tiling_for(repo, &events);
    assert!(tiling.tiled, "chain must tile: {:?}", tiling.gaps);
    assert!(tiling.rounds_exhausted, "budget must be spent for this arm");
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        BRANCH,
        &git(repo, &["rev-parse", "HEAD"]),
        Some(&tiling),
        None,
        false,
    );
    rep.verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .count()
}

#[test]
fn cap_a_spent_budget_discharges_coverage_with_no_attestation_at_all() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (_base, shas, head) = repo_with(repo, 4);
    // The case that produced the 12-round PRs. Three rounds against a budget
    // of 2, and the chain does NOT tile: every attestation starts at shas[0],
    // so base..shas[0] is an uncovered gap and the fail arm contributes no
    // verdict. Before the discharge this was Covered(0) - "uncovered" - and
    // the gate refused, naming a terminal act the same call would not permit.
    // Every remedy for it names a review verb, and running one spends a round
    // already spent, so nothing could ever clear it.
    let events = events_file(
        repo,
        &[
            attestation("code-review", &shas[0], &shas[1], "fail"),
            attestation("code-review", &shas[1], &shas[2], "fail"),
            attestation("code-review", &shas[2], &head, "fail"),
        ],
    );
    let tiling = tiling_for(repo, &events);
    assert!(
        tiling.rounds_exhausted,
        "three rounds must spend a budget of 2: {:?}",
        tiling.rounds_used
    );
    assert!(
        !tiling.tiled,
        "this specimen must NOT tile - a tiled chain covers by its own arm and \
         would not exercise the discharge: {:?}",
        tiling.gaps
    );
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        BRANCH,
        &head,
        Some(&tiling),
        None,
        false,
    );
    // The POSITIVE marker: covered, by the budget alone, with no attestation
    // verdict behind it. Covered(0) reads as "uncovered" downstream, so the
    // count is load-bearing and an assert on `matches!(Covered(_))` would
    // pass on the broken answer.
    assert_eq!(
        rep.coverage,
        Coverage::Covered(1),
        "a spent budget must discharge the obligation: {:?}",
        rep.verdicts
    );

    // The control: the SAME chain under a budget of 10 is NOT discharged and
    // stays uncovered, so the discharge cannot leak below the cap.
    let under = compute_range_tiling("git", repo, "origin/main", &events, BRANCH, &head, 10);
    assert!(!under.rounds_exhausted);
    let rep_under = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        BRANCH,
        &head,
        Some(&under),
        None,
        false,
    );
    assert_eq!(
        rep_under.coverage,
        Coverage::Covered(0),
        "under the cap the same chain must stay uncovered: {:?}",
        rep_under.verdicts
    );
}

#[test]
fn cap_a_retraction_never_mints_coverage_at_the_head_it_revoked() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // A retraction is a fail row carrying retracts_attester, and
    // local_latest_passes drops the pass it names. If the spent-budget arm
    // admits it too, a REVOKE adds the coverage it exists to destroy.
    // The revoked reviewer's ONLY row is the retraction. A retraction that
    // replaces one of its own live fails would leave the count unchanged and
    // hide the defect, so the specimen is a reviewer whose whole presence in
    // the chain IS the revocation.
    let retraction = {
        let mut row: serde_json::Value =
            serde_json::from_str(&attestation("peer", &shas[2], &head, "fail")).unwrap();
        row["data"]["retracts_attester"] = serde_json::json!("sess-a");
        row.to_string()
    };
    let rows = vec![
        attestation("code-review", &base, &shas[0], "fail"),
        attestation("code-review", &shas[0], &shas[2], "fail"),
        attestation("code-review", &shas[2], &head, "fail"),
        retraction,
    ];
    // The control first: the same chain WITHOUT the retraction is covered by
    // one verdict, so a 1 below is the arm working, not the arm dead.
    assert_eq!(cap_verdict_count_for(repo, &rows[..3]), 1);
    // With it, still 1. A 2 here is `peer` counted as Reviewed on the
    // strength of a row that REVOKES a review.
    assert_eq!(cap_verdict_count_for(repo, &rows), 1);
}

#[test]
fn cap_a_zero_evidence_fail_row_never_counts() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // A row that measured no lines and no files read nothing, whatever its
    // verdict says. The pass scan refuses it by name; the fail arm must too,
    // or a hand-crafted row mints Covered(1) past the cap.
    let hollow = {
        let mut row: serde_json::Value =
            serde_json::from_str(&attestation("hollow-reviewer", &shas[2], &head, "fail")).unwrap();
        row["data"]["reviewed_line_count"] = serde_json::json!(0);
        row["data"]["reviewed_file_count"] = serde_json::json!(0);
        row.to_string()
    };
    let rows = vec![
        attestation("code-review", &base, &shas[0], "fail"),
        attestation("code-review", &shas[0], &shas[2], "fail"),
        attestation("code-review", &shas[2], &head, "fail"),
        hollow,
    ];
    // One real reviewer counts; the hollow row adds nothing.
    assert_eq!(cap_verdict_count_for(repo, &rows), 1);
}

#[test]
fn cap_a_slash_prefixed_reviewer_is_the_same_reviewer() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // The pass scan normalizes the leading slash. Without the same
    // normalization here, `/code-review` and `code-review` are two reviewers
    // and the count doubles - the exact double count the dedup guards.
    let slashed = {
        let mut row: serde_json::Value =
            serde_json::from_str(&attestation("/code-review", &shas[2], &head, "fail")).unwrap();
        row["data"]["attester_session_id"] = serde_json::json!("sess-b");
        row.to_string()
    };
    let rows = vec![
        attestation("code-review", &base, &shas[0], "fail"),
        attestation("code-review", &shas[0], &shas[2], "fail"),
        slashed,
    ];
    assert_eq!(cap_verdict_count_for(repo, &rows), 1);
}

#[test]
fn cap_a_reviewer_with_both_a_pass_and_a_fail_link_counts_once() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // One REVIEWER across two attester sessions: a pass from sess-a on the
    // first link, fails from sess-b on the rest. Two sessions are what it
    // takes to hold both verdicts at once - local_latest_passes keys on
    // (reviewer, attester), so one session's later fail replaces its own
    // pass. The shape is ordinary: a handoff, or a review fork, re-runs the
    // same verb under a new session id.
    //
    // The pass loop pushes a verdict for "code-review"; the spent-budget
    // fail arm keys on the reviewer NAME alone, so without the guard it
    // pushes a SECOND one and Covered(n) plus the row's reviewed_count both
    // read 2 for a single reviewer. That is the coverage count lying.
    let from_sess_b = |base: &str, head: &str| {
        let mut row: serde_json::Value =
            serde_json::from_str(&attestation("code-review", base, head, "fail")).unwrap();
        row["data"]["attester_session_id"] = serde_json::json!("sess-b");
        row.to_string()
    };
    let events = events_file(
        repo,
        &[
            attestation("code-review", &base, &shas[0], "pass"),
            from_sess_b(&shas[0], &shas[1]),
            from_sess_b(&shas[1], &shas[2]),
            from_sess_b(&shas[2], &head),
        ],
    );
    let tiling = tiling_for(repo, &events);
    assert!(tiling.tiled, "chain must tile: {:?}", tiling.gaps);
    assert!(
        tiling.rounds_exhausted,
        "three fails after the pass must spend a budget of 2: {:?}",
        tiling.rounds_used
    );
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        BRANCH,
        &head,
        Some(&tiling),
        None,
        false,
    );
    let local: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(
        local.len(),
        1,
        "one reviewer, one verdict: {:?}",
        rep.verdicts
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

#[test]
fn cap_a_declined_tiling_chain_counts_as_coverage_past_the_budget() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 4);
    // The declined lane: every round ended with findings (verdict fail),
    // findings get declined or filed, and no clean pass ever lands. The
    // chain still READ base..head across its rounds, so it tiles.
    let events = events_file(
        repo,
        &[
            attestation("code-review", &base, &shas[0], "fail"),
            attestation("code-review", &shas[0], &shas[2], "fail"),
            attestation("code-review", &shas[2], &head, "fail"),
        ],
    );
    // Three fail rounds against a budget of 2: exhausted, and tiled.
    let tiling = tiling_for(repo, &events);
    assert!(tiling.tiled, "chain must tile: {:?}", tiling.gaps);
    assert_eq!(tiling.rounds_used, 3);
    assert!(tiling.rounds_exhausted);
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        BRANCH,
        &head,
        Some(&tiling),
        None,
        false,
    );
    // Past the budget the newest fail link counts as Reviewed at its chain
    // head (freshness rescued by the chain, the same rule a pass link
    // gets), so the terminal act the receipt names is reachable: the PR is
    // covered and the disposition gate alone decides.
    let local: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(
        local.len(),
        1,
        "one verdict per reviewer: {:?}",
        rep.verdicts
    );
    assert_eq!(local[0].verdict, CoverageVerdict::Reviewed);
    assert_eq!(local[0].reviewed_sha, head);
    assert_eq!(rep.coverage, Coverage::Covered(1));

    // The control: the SAME fail chain under the budget changes nothing -
    // no pass exists, so no local verdict at all, coverage uncovered. The
    // spent-budget arm must not leak below the cap.
    let under = compute_range_tiling("git", repo, "origin/main", &events, BRANCH, &head, 10);
    assert!(!under.rounds_exhausted);
    let rep_under = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        BRANCH,
        &head,
        Some(&under),
        None,
        false,
    );
    assert!(
        !rep_under
            .verdicts
            .iter()
            .any(|v| v.producer == CoverageProducer::LocalAttestation),
        "under the budget a fail chain still leaves no pass: {:?}",
        rep_under.verdicts
    );
}

#[test]
fn cap_only_a_confirmed_correctness_or_security_finding_is_hard() {
    use fno_agents::loopcheck::{blockers_impossible, blockers_withhold};
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([
            finding("a.py:1:security", "security", Some("CONFIRMED"), true),
            finding("b.py:2:correctness", "correctness", None, true),
            finding("c.py:3:performance", "performance", Some("CONFIRMED"), true),
        ]),
        serde_json::json!([]),
        false,
    )]
    .join("\n");
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, true);
    assert_eq!(blockers.len(), 3);
    let hard: Vec<&str> = blockers
        .iter()
        .filter(|b| b.hard)
        .map(|b| b.finding_key.as_str())
        .collect();
    // CONFIRMED security is hard. Unconfirmed correctness is not. CONFIRMED
    // performance is not - the two categories are the whole list.
    assert_eq!(hard, vec!["a.py:1:security"]);
    // Under the budget every blocker withholds; at the cap only the hard one.
    assert!(blockers_withhold(&blockers, false));
    assert!(blockers_withhold(&blockers, true));
    assert!(blockers_impossible(&blockers, true));
    assert!(!blockers_impossible(&blockers, false));
}

#[test]
fn cap_with_only_fileable_findings_stops_withholding() {
    use fno_agents::loopcheck::{blockers_impossible, blockers_withhold};
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([finding("b.py:2:correctness", "correctness", None, true)]),
        serde_json::json!([]),
        false,
    )]
    .join("\n");
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, true);
    assert_eq!(blockers.len(), 1);
    assert!(!blockers[0].hard);
    assert!(
        blockers_withhold(&blockers, false),
        "under the budget it still blocks"
    );
    assert!(
        !blockers_withhold(&blockers, true),
        "at the cap it is filed, not held"
    );
    assert!(!blockers_impossible(&blockers, true));
}

#[test]
fn cap_truncated_remainder_is_always_hard() {
    use fno_agents::loopcheck::blockers_withhold;
    let events = [dispositions_event(
        SPECIMEN_HEAD,
        serde_json::json!([]),
        serde_json::json!([]),
        true,
    )]
    .join("\n");
    let blockers = disposition_blockers(&events, BRANCH, SPECIMEN_HEAD, false);
    assert!(blockers[0].hard, "what cannot be inspected cannot be filed");
    assert!(blockers_withhold(&blockers, true));
}

// --- x-aecc: declining must satisfy coverage ---------------------------------
//
// The tiling predicate is ANSWERED at this head, never clean at this head.
// A branch whose ONLY attestation is a `fail` whose findings are all
// terminally dispositioned must read covered exactly like a pass chain; one
// non-terminal finding withholds that answer and is named by key. A
// pass-chain covering proves nothing here (the pass arms above already pin
// those), so every test's only attestation is a fail.

use fno_agents::loopcheck::{blockers_withhold, unattested_reviewers_scan};

/// A fail at `head` that reviewed base..head, raised `findings`, and recorded
/// `dispositions` - the declining round in one event.
fn declined_round(
    base: &str,
    head: &str,
    findings: serde_json::Value,
    dispositions: serde_json::Value,
) -> String {
    let mut data = serde_json::json!({
        "reviewer": "code-review",
        "head_sha": head,
        "verdict": "fail",
        "session_id": "s",
        "attester_session_id": "sess-peer",
        "branch": BRANCH,
        "reviewed_base_sha": base,
        "reviewed_head_sha": head,
        "reviewed_file_count": 2,
        "reviewed_line_count": 20,
        "findings": findings,
    });
    if dispositions
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false)
    {
        data["dispositions"] = dispositions;
    }
    serde_json::json!({
        "ts": "2026-08-26T19:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": data,
    })
    .to_string()
}

fn declined(key: &str) -> serde_json::Value {
    serde_json::json!({"finding_key": key, "disposition": "declined", "reason": "not worth the churn"})
}

#[test]
fn xaecc_marker1_fail_only_chain_fully_dispositioned_reads_covered() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, _shas, head) = repo_with(repo, 2);
    let k1 = "a.py:1:correctness";
    let k2 = "b.py:2:correctness";
    let events = events_file(
        repo,
        &[declined_round(
            &base,
            &head,
            serde_json::json!([
                finding(k1, "correctness", None, true),
                finding(k2, "correctness", None, true),
            ]),
            serde_json::json!([declined(k1), declined(k2)]),
        )],
    );
    let tiling = tiling_for(repo, &events);
    assert!(
        tiling.tiled,
        "the fail's range read base..head: {:?}",
        tiling.gaps
    );
    let at_head = |sha: &str| {
        if sha == head {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        BRANCH,
        &head,
        Some(&tiling),
        None,
        false,
    );
    // POSITIVE markers, the row's own words: a positive count and the
    // reviewed state, never an absence. Covered(0) serializes "uncovered", so
    // the n > 0 match is exactly the "covered" string the Python gate reads.
    assert!(
        matches!(rep.coverage, Coverage::Covered(n) if n > 0),
        "an answered fail must count like a pass: {:?}",
        rep.coverage
    );
    assert_eq!(rep.review_state(), Some(ReviewState::Reviewed));
    let local: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].name, "code-review");
    assert_eq!(local[0].verdict, CoverageVerdict::Reviewed);
    // Marker 3: the covered answer pins the SAME head the declining round
    // attested - no commit, no further review, between declining and merging.
    assert_eq!(local[0].reviewed_sha, head);
    assert_eq!(local[0].freshness, Some(Freshness::Fresh));

    // The same answered fail satisfies the config.review.reviewers gate.
    let (unattested, _malformed) = unattested_reviewers_scan(
        repo.join("events.jsonl").as_path(),
        &["code-review".to_string()],
        &at_head,
        BRANCH,
        &head,
        false,
    );
    assert!(
        unattested.is_empty(),
        "an answered fail satisfies the reviewers gate: {unattested:?}"
    );
}

#[test]
fn xaecc_fixed_in_a_later_round_answers_too() {
    // The other terminal disposition: findings raised in round 1, fixed, and
    // a LATER round reviewed the fix delta (the specimen shape). The only
    // attestations are fails.
    let at_specimen = |sha: &str| {
        if sha == SPECIMEN_HEAD {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let events = specimen_events();
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_specimen,
        BRANCH,
        SPECIMEN_HEAD,
        None,
        None,
        false,
    );
    assert!(
        matches!(rep.coverage, Coverage::Covered(n) if n > 0),
        "fixed-and-reviewed findings answer the head: {:?}",
        rep.coverage
    );
    assert_eq!(rep.review_state(), Some(ReviewState::Reviewed));
}

#[test]
fn xaecc_marker2_one_nonterminal_finding_withholds_and_is_named() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, _shas, head) = repo_with(repo, 2);
    let k1 = "a.py:1:correctness";
    let k2 = "b.py:2:correctness";
    // Only k1 is dispositioned; k2 is left open on the same head.
    let events = events_file(
        repo,
        &[declined_round(
            &base,
            &head,
            serde_json::json!([
                finding(k1, "correctness", None, true),
                finding(k2, "correctness", None, true),
            ]),
            serde_json::json!([declined(k1)]),
        )],
    );
    let at_head = |sha: &str| {
        if sha == head {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        BRANCH,
        &head,
        None,
        None,
        false,
    );
    // The pair-half of the marker: NOT covered, and not by absence - the
    // blockers list NAMES the open finding by key.
    assert!(
        !rep.verdicts
            .iter()
            .any(|v| v.producer == CoverageProducer::LocalAttestation
                && v.verdict == CoverageVerdict::Reviewed),
        "an unanswered fail must not count as a review"
    );
    assert_eq!(rep.review_state(), Some(ReviewState::Unreviewed));
    let blockers = disposition_blockers(&events, BRANCH, &head, false);
    assert_eq!(blockers.len(), 1, "the refusal names exactly one finding");
    assert_eq!(blockers[0].finding_key, k2);
    assert_eq!(blockers[0].axis, "open");
    assert!(blockers_withhold(&blockers, false));

    // The reviewers gate withholds on the same evidence, failed_at_head.
    let (unattested, _malformed) = unattested_reviewers_scan(
        repo.join("events.jsonl").as_path(),
        &["code-review".to_string()],
        &at_head,
        BRANCH,
        &head,
        false,
    );
    assert_eq!(unattested.len(), 1);
}

#[test]
fn xaecc_cap_files_the_soft_remainder_and_answers() {
    // Filed at the cap is the third terminal disposition: rounds spent with
    // only non-hard blockers, the fail still answers (the merge gate files
    // them; only a CONFIRMED correctness/security finding keeps withholding).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 3);
    let k = "a.py:1:correctness";
    // Three fail rounds, each at its own head (the loop's non-fix commits),
    // finding never dispositioned, unconfirmed so not hard.
    let lines: Vec<String> = [&shas[0], &shas[1], &head]
        .iter()
        .map(|h| {
            declined_round(
                &base, // every round read the full range
                h,
                serde_json::json!([finding(k, "correctness", None, true)]),
                serde_json::json!([]),
            )
        })
        .collect();
    let events = events_file(repo, &lines);
    let tiling = tiling_for(repo, &events);
    assert!(tiling.rounds_exhausted, "3 rounds spends the budget of 2");
    let at_head = |sha: &str| {
        if sha == head {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        BRANCH,
        &head,
        Some(&tiling),
        None,
        false,
    );
    assert!(
        matches!(rep.coverage, Coverage::Covered(n) if n > 0),
        "the cap files the soft remainder; the fail answers: {:?}",
        rep.coverage
    );
    assert_eq!(rep.review_state(), Some(ReviewState::Reviewed));
}

/// A same-head RETRACTION of a pass (review finding 1): the retraction is
/// the pair's latest fail, the pair's own round-1 fail carried findings that
/// are all declined, and the chain's blockers are empty - the exact shape a
/// chain-global guard would read as answered. The retribution it must not
/// buy: the revoked pass must NOT come back as a Reviewed verdict.
#[test]
fn xaecc_r1_a_retraction_never_resurrects_the_revoked_pass() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, shas, head) = repo_with(repo, 2);
    let k = "a.py:1:correctness";
    let fail_with_findings = declined_round(
        &base,
        &shas[0],
        serde_json::json!([finding(k, "correctness", None, true)]),
        serde_json::json!([declined(k)]),
    );
    let pass =
        attestation("code-review", &shas[0], &head, "pass").replace("\"sess-a\"", "\"sess-peer\"");
    let retraction = serde_json::json!({
        "ts": "2026-08-26T20:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": head,
            "verdict": "fail",
            "session_id": "s-op",
            "attester_session_id": "sess-op",
            "retracts_attester": "sess-peer",
            "branch": BRANCH,
            "reviewed_file_count": 2,
            "reviewed_line_count": 20,
        }
    })
    .to_string();
    let events = events_file(repo, &[fail_with_findings, pass, retraction]);
    let at_head = |sha: &str| {
        if sha == head {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        BRANCH,
        &head,
        None,
        None,
        false,
    );
    // The pair's latest entry is the retraction: is_pass=false,
    // is_retraction=true -> no verdict at all, covered nowhere. A `pass`
    // arm would have counted; the retraction revoked it and nothing may
    // restore it by decline.
    assert!(
        !rep.verdicts
            .iter()
            .any(|v| v.producer == CoverageProducer::LocalAttestation),
        "a retraction revokes; it never covers: {:?}",
        rep.verdicts
    );
    assert_eq!(rep.review_state(), Some(ReviewState::Unreviewed));
    let (unattested, _m) = unattested_reviewers_scan(
        repo.join("events.jsonl").as_path(),
        &["code-review".to_string()],
        &at_head,
        BRANCH,
        &head,
        false,
    );
    assert_eq!(
        unattested.len(),
        1,
        "the reviewers gate withholds on the retraction too"
    );
}

/// A bystander reviewer's findings-free fail (review finding 2): another
/// reviewer's declined findings must not satisfy it, on the coverage axis or
/// the reviewers gate.
#[test]
fn xaecc_r2_a_bystanders_findings_free_fail_stays_unanswered() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let (base, _shas, head) = repo_with(repo, 2);
    let k = "a.py:1:correctness";
    let events = events_file(
        repo,
        &[
            declined_round(
                &base,
                &head,
                serde_json::json!([finding(k, "correctness", None, true)]),
                serde_json::json!([declined(k)]),
            ),
            attestation("sigma", &base, &head, "fail"),
        ],
    );
    let at_head = |sha: &str| {
        if sha == head {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let rep = classify_coverage_tiled(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &at_head,
        BRANCH,
        &head,
        None,
        None,
        false,
    );
    let locals: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::LocalAttestation)
        .collect();
    // The findings carrier answers; the bystander does not.
    assert_eq!(locals.len(), 1, "only the carrier pair promotes");
    assert_eq!(locals[0].name, "code-review");
    assert_eq!(locals[0].verdict, CoverageVerdict::Reviewed);
    let (unattested, _m) = unattested_reviewers_scan(
        repo.join("events.jsonl").as_path(),
        &["code-review".to_string(), "sigma".to_string()],
        &at_head,
        BRANCH,
        &head,
        false,
    );
    assert_eq!(
        unattested.len(),
        1,
        "sigma stays unattested: {unattested:?}"
    );
}
