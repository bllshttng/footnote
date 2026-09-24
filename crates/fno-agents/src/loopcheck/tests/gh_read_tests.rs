use super::*;

#[test]
fn stderr_tail_multibyte_boundary_no_panic() {
    // gemini HIGH on #447: tail slice must land on a char boundary.
    let mut payload = String::new();
    while payload.len() < 300 {
        payload.push('\u{00e9}'); // 2-byte char so len-200 can split one
    }
    let tail = stderr_tail(payload.as_bytes());
    assert!(tail.len() <= 200);
    assert!(!tail.is_empty());
}

/// Shared fixture for the head_is_shipped cases: a PR recording `pr_head`.
pub(super) fn shipped_pr(pr_head: &str, state: PrState) -> PrInfo {
    PrInfo {
        range_tiling: RangeTiling::default(),
        head_oid: pr_head.to_string(),
        state,
        ..reviewers_gate_pr()
    }
}

/// A git stub answering the three probes `head_is_shipped` can make: the
/// working-tree status, the base resolution, and `--is-ancestor`.
/// Mirrors the classify_payload stubs above.
fn git_stub(dir: &Path, ancestor: bool, clean: bool) -> std::path::PathBuf {
    let verdict = if ancestor { "exit 0" } else { "exit 1" };
    // A clean tree is empty stdout with exit 0; a dirty one names a file.
    let status = if clean {
        "exit 0"
    } else {
        "printf ' M src/lib.rs\\n'"
    };
    write_exec(
            dir,
            "git",
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  status*) {status} ;;\n  rev-parse*origin/main*) exit 0 ;;\n  *is-ancestor*) {verdict} ;;\n  *) exit 1 ;;\nesac\n"
            ),
        )
}

#[test]
fn head_is_shipped_takes_equality_without_touching_git() {
    // The common path costs no subprocess: a git that would panic the test
    // if invoked is never invoked, because equality answers first.
    let pr = shipped_pr("abc", PrState::Open);
    assert!(head_is_shipped(
        &pr,
        "abc",
        "definitely-not-a-real-git-binary",
        Path::new(".")
    ));
}

#[test]
fn head_is_shipped_accepts_a_head_already_on_the_base() {
    // The 2026-07-30 and 2026-08-22 repros: a merged PR, a local HEAD that
    // differs because the branch moved past its own merge, and nothing left
    // to ship.
    let dir = tempfile::tempdir().unwrap();
    let git = git_stub(dir.path(), true, true);
    let pr = shipped_pr("23480a0e", PrState::Merged);
    // Retry the spawn, for the same measured reason probe_graphql_quota
    // does: this suite forks hundreds of fake `git` subprocesses in
    // parallel, and a loaded runner intermittently fails one fork/exec.
    // This arm reaches git twice (`git_tree_clean` then `git_head_on_base`)
    // and both fail CLOSED, so a blip in either reads as "not shipped" and
    // reds a PR that changed nothing here. Observed doing exactly that.
    //
    // The retry cannot hide a regression: the stub answers clean and
    // ancestor unconditionally, so the only way to get `false` is a failed
    // spawn, and a real regression fails all five attempts.
    //
    // Its sibling `head_is_shipped_still_refuses_a_commit_stacked_on_a_
    // merged_pr` asserts the NEGATIVE, so a blip there passes it for the
    // wrong reason rather than failing. That is a quieter defect and is
    // left alone here; it needs a positive control on the stub, not a retry.
    let mut shipped = false;
    for _ in 0..5 {
        shipped = head_is_shipped(&pr, "fe407c3b", git.to_str().unwrap(), Path::new("."));
        if shipped {
            break;
        }
    }
    assert!(
        shipped,
        "the stub git kept failing to spawn across 5 retries - a real regression, not a blip"
    );
}

/// THE #447 REGRESSION TEST. A change that makes this case pass as shipped
/// re-opens the defect the guard exists for: unpushed work terminating as
/// DonePRGreen without ever shipping. If this assertion is ever inverted to
/// make a wedge go away, the wedge was the wrong thing to fix.
#[test]
fn head_is_shipped_still_refuses_a_commit_stacked_on_a_merged_pr() {
    let dir = tempfile::tempdir().unwrap();
    let git = git_stub(dir.path(), false, true);
    let pr = shipped_pr("23480a0e", PrState::Merged);
    assert!(!head_is_shipped(
        &pr,
        "deadbeef",
        git.to_str().unwrap(),
        Path::new(".")
    ));
}

/// `--is-ancestor` sees committed history only. Without the clean-tree
/// condition, a rebase onto a base that already holds the merge would read
/// as shipped while uncommitted follow-up edits sat in the tree, and the
/// run would terminate DonePRGreen on work nobody had committed. The old
/// equality predicate blocked that state incidentally; this keeps it
/// blocked deliberately.
#[test]
fn head_is_shipped_refuses_a_dirty_tree_on_the_ancestor_arm() {
    let dir = tempfile::tempdir().unwrap();
    let git = git_stub(dir.path(), true, false);
    let pr = shipped_pr("23480a0e", PrState::Merged);
    assert!(!head_is_shipped(
        &pr,
        "fe407c3b",
        git.to_str().unwrap(),
        Path::new(".")
    ));
}

#[test]
fn head_is_shipped_falls_back_to_equality_when_git_cannot_answer() {
    // A broken git leaves today's behavior exactly as it was, rather than
    // failing open and letting unpushed work terminate.
    let pr = shipped_pr("23480a0e", PrState::Merged);
    assert!(!head_is_shipped(
        &pr,
        "fe407c3b",
        "definitely-not-a-real-git-binary",
        Path::new(".")
    ));
}

#[test]
fn head_is_shipped_refuses_a_pr_with_no_recorded_head() {
    let pr = shipped_pr("", PrState::Open);
    assert!(!head_is_shipped(
        &pr,
        "abc",
        "definitely-not-a-real-git-binary",
        Path::new(".")
    ));
}

#[test]
fn graphql_exhausted_reason_names_reset_and_rest_lane() {
    // The message must make a session STOP retrying and say where the
    // answer still lives; "retrying next fire" is the advice it replaces.
    let q = GraphqlQuota {
        remaining: 0,
        reset_epoch: Utc::now().timestamp() + 40 * 60 + 5,
        core_remaining: None,
    };
    let msg = graphql_exhausted_reason(&q);
    assert!(msg.contains("GraphQL quota exhausted"), "got: {msg}");
    assert!(msg.contains("~40m"), "got: {msg}");
    assert!(msg.contains("fno do pr status"), "got: {msg}");
    assert!(!msg.contains("retrying next fire"), "got: {msg}");
}

#[test]
fn graphql_exhausted_reason_never_reports_a_past_reset() {
    let q = GraphqlQuota {
        remaining: 0,
        reset_epoch: Utc::now().timestamp() - 120,
        core_remaining: None,
    };
    assert!(graphql_exhausted_reason(&q).contains("~0m"));
}

#[test]
fn probe_graphql_quota_parses_the_graphql_bucket() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = write_exec(
        tmp.path(),
        "gh",
        "#!/bin/sh\n[ \"$1\" = api ] && [ \"$2\" = rate_limit ] && \
             echo '{\"resources\":{\"graphql\":{\"remaining\":0,\"reset\":1750000000},\
             \"core\":{\"remaining\":4980,\"limit\":5000,\"reset\":1750000000}}}' && exit 0\n\
             exit 1\n",
    );
    // Retry the spawn a few times: under a loaded CI runner (this crate's
    // suite forks hundreds of fake `gh`/`git` subprocesses in parallel),
    // `Command::output()` has measured an intermittent fork/exec failure
    // that has nothing to do with the parser under test - probe_graphql_
    // quota's own `.ok()?` already treats that as "unavailable, degrade
    // gracefully" in production, so retrying here absorbs the same
    // transient blip instead of failing the build on an infra hiccup.
    let mut q = None;
    for _ in 0..5 {
        q = probe_graphql_quota(gh.to_str().unwrap(), tmp.path());
        if q.is_some() {
            break;
        }
    }
    let q = q.expect("gh spawn kept failing across 5 retries - a real regression, not a blip");
    assert_eq!(q.remaining, 0);
    assert_eq!(q.reset_epoch, 1750000000);
    // The core bucket from the SAME probe read feeds the secondary-limit
    // classifier; a payload without it must degrade to None, not to 0.
    assert_eq!(q.core_remaining, Some(4980));
}

#[test]
fn probe_graphql_quota_failure_is_none_not_a_false_exhaustion() {
    // A failed probe must degrade to the transient wording, never
    // fabricate an exhaustion verdict that stalls a healthy session.
    let tmp = tempfile::tempdir().unwrap();
    let gh = write_exec(tmp.path(), "gh", "#!/bin/sh\nexit 1\n");
    assert!(probe_graphql_quota(gh.to_str().unwrap(), tmp.path()).is_none());
}

fn quota_with(graphql_remaining: i64, core_remaining: Option<i64>) -> GraphqlQuota {
    GraphqlQuota {
        remaining: graphql_remaining,
        reset_epoch: 1_750_000_000,
        core_remaining,
    }
}

#[test]
fn refusal_is_secondary_classifies_the_verbatim_body_by_the_live_bucket() {
    // The p0 shape: the measured 403 says only "API rate limit exceeded"
    // with both buckets healthy - that IS the secondary limit, whatever
    // the prose says.
    assert!(!VERBATIM_403.to_lowercase().contains("secondary"));
    assert!(refusal_is_secondary(
        VERBATIM_403,
        Some(&quota_with(4446, Some(4980))),
        true
    ));
    assert!(refusal_is_secondary(
        VERBATIM_403,
        Some(&quota_with(4446, Some(4980))),
        false
    ));
}

#[test]
fn refusal_is_secondary_names_the_drained_buckets_as_the_primary_quota() {
    // A drained explaining bucket is primary exhaustion, not secondary -
    // on either transport.
    assert!(!refusal_is_secondary(
        VERBATIM_403,
        Some(&quota_with(0, Some(4980))),
        true
    ));
    assert!(!refusal_is_secondary(
        VERBATIM_403,
        Some(&quota_with(4446, Some(0))),
        true
    ));
}

#[test]
fn refusal_is_secondary_low_but_positive_core_is_not_proof_of_the_quota() {
    // A secondary refusal lands with core wherever it stood; only 0
    // names the core quota. Mislabeling 1..=20 as the primary quota
    // sends the session to wait for a reset instead of backing off -
    // the exact harm this classifier exists to prevent.
    assert!(refusal_is_secondary(
        VERBATIM_403,
        Some(&quota_with(4446, Some(3))),
        false
    ));
    assert!(refusal_is_secondary(
        VERBATIM_403,
        Some(&quota_with(4446, Some(20))),
        true
    ));
}

#[test]
fn refusal_is_secondary_fails_toward_back_off_on_an_unreadable_probe() {
    // No probe (failed, or a caller with none), or a probe that names no
    // core bucket: reading unknown as the primary quota sends the
    // session to wait for a reset that never comes, so unknown still
    // says secondary. This is the fail-safe the Python side ships.
    assert!(refusal_is_secondary(VERBATIM_403, None, true));
    assert!(refusal_is_secondary(
        VERBATIM_403,
        Some(&quota_with(4446, None)),
        true
    ));
}

#[test]
fn refusal_is_secondary_ignores_stderr_that_does_not_smell_of_a_rate_limit() {
    // The wide wording is only the TRIGGER; without it there is nothing
    // to classify and the transient wording stands.
    assert!(!refusal_is_secondary(
        "gh: Not Found (https://api.github.com/)",
        Some(&quota_with(0, Some(0))),
        true
    ));
    assert!(!refusal_is_secondary("", None, false));
}

#[test]
fn refusal_is_secondary_phrase_alone_does_not_classify_the_bucket_does() {
    // Even stderr that DOES say "secondary rate limit" classifies by the
    // bucket: wording is GitHub's to change, so it is never the verdict.
    let phrase = "HTTP 403: You have exceeded a secondary rate limit";
    assert!(!refusal_is_secondary(
        phrase,
        Some(&quota_with(0, Some(0))),
        true
    ));
    assert!(refusal_is_secondary(
        phrase,
        Some(&quota_with(4890, Some(4922))),
        true
    ));
}

#[test]
fn no_pr_stderr_detected() {
    assert!(is_no_pr_stderr(
        b"no pull requests found for branch \"feat\""
    ));
    assert!(is_no_pr_stderr(b"No pull requests found for branch \"x\""));
    // Outage shapes are NOT no-PR.
    assert!(!is_no_pr_stderr(b"connect: network is unreachable"));
    assert!(!is_no_pr_stderr(b"API rate limit exceeded"));
    assert!(!is_no_pr_stderr(b""));
}
