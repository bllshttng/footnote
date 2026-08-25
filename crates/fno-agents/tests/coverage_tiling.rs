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
    classify_coverage_tiled, compute_range_tiling, CoverageProducer, CoverageVerdict, Freshness,
    RangeTiling,
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
