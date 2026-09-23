//! What coverage status does the PR show, and is it waived? The fno/review-coverage commit status, its publisher, and the operator waiver read.

use super::*;

pub(super) const COVERAGE_STATUS_CONTEXT: &str = "fno/review-coverage";

pub(super) const COVERAGE_UNAVAILABLE_STATUS_CONTEXT: &str = "fno/review-coverage-unavailable";

pub(super) fn coverage_unavailable_description(head: &str) -> String {
    format!(
        "coverage read unavailable at {}; retry the review verb",
        short_sha(head)
    )
}

pub(super) fn coverage_instrument_status(
    coverage: &Coverage,
    head: &str,
) -> (&'static str, String) {
    match coverage {
        Coverage::Unknown => ("pending", coverage_unavailable_description(head)),
        Coverage::Covered(_) => (
            "success",
            format!("coverage read healthy at {}", short_sha(head)),
        ),
    }
}

/// Whether `name` is the local reviewer Python's gate demands a pass from,
/// with the same leading-slash tolerance `_coverage_has_local_pass` applies.
pub(super) fn is_code_review_reviewer(name: &str) -> bool {
    normalize_reviewer(name) == "code-review"
}

/// A bounded run's successful stdout: the completed payload's stdout when
/// the child exited zero. A timeout or unrunnable child is not a success
/// (None), which preserves each caller's degrade semantics.
pub(super) fn bounded_success_text(result: Result<BoundedOutput, GhReadError>) -> Option<String> {
    let out = result.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Publish the just-emitted `review_coverage` verdict as a commit status on
/// the PR head, so every path that can WRITE the row also leaves the
/// server-visible marker `gh pr merge`, the web button, and the auto-merge
/// queue are judged by. Direct `gh api` POST on the same `gh_bin` seam as the
/// nudge comment; the outcome never gates the caller - the durable verdict is
/// the event row, and a failed POST leaves the status absent, which the
/// ruleset reads as not-passing (fail-closed).
///
/// The success conjunction MIRRORS `coverage_verdict` in
/// cli/src/fno/pr/_coverage_gate.py: covered count > 0, the row pinned to the PR head
/// (not merely the local HEAD), and - when `code-review` is a configured
/// reviewer - a head-pinned local pass from it. The two must agree, because
/// this status is exactly what lets a merge through where `fno do pr merge`
/// already looked; the post-merge audit on main is what catches a drift.
///
/// Skipped entirely when no review lane is configured: a stock install has no
/// ruleset requiring the context, and a permanent failure status there would
/// be noise that teaches readers to ignore the check.
/// Whether the PR carries the `coverage-override` label. The label is durable
/// shared state: EVERY writer of the coverage status re-reads it before
/// posting, so a green stamped by the gate workflow's labeled arm survives a
/// later stop-hook or verb fire instead of being clobbered red. `None` means
/// all three reads failed; the caller then checks the current marker
/// description before deciding whether an override green needs protection.
pub(super) fn pr_has_override_label(gh_bin: &str, cwd: &Path, pr_number: i64) -> Option<bool> {
    for attempt in 1..=3 {
        let pr_arg = pr_number.to_string();
        let out = bounded_read(
            gh_bin.as_ref(),
            &[
                "pr",
                "view",
                &pr_arg,
                "--json",
                "labels",
                "--jq",
                "[.labels[].name] | index(\"coverage-override\") != null",
            ],
            cwd,
            "coverage_override_label",
            stopgate_read_timeout(),
        );
        if let Some(text) = bounded_success_text(out) {
            match text.as_str() {
                "true" => return Some(true),
                "false" => return Some(false),
                _ => {}
            }
        }
        if attempt < 3 {
            eprintln!(
                "review-coverage publisher: override label read attempt {attempt} failed; retrying"
            );
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
    None
}

pub(super) fn current_coverage_description(gh_bin: &str, cwd: &Path, head: &str) -> Option<String> {
    let target = format!("repos/:owner/:repo/commits/{head}/status");
    bounded_success_text(bounded_read(
        gh_bin.as_ref(),
        &[
            "api",
            target.as_str(),
            "--jq",
            "[.statuses[] | select(.context == \"fno/review-coverage\")] | first | .description // \"\"",
        ],
        cwd,
        "coverage_status_read",
        stopgate_read_timeout(),
    ))
}

/// The standing operator-law subject for review coverage. One live law
/// verdict here waives the coverage conjunct fleet-wide; the per-head exit
/// beside it is the attended `fno do pr coverage-waive` command's scoped
/// subject. Mirrors `STANDING_WAIVER_SUBJECT` in
/// cli/src/fno/pr/_coverage_gate.py; the two spellings are held equal by the
/// coverage-path tests, not by trust.
pub(super) const STANDING_WAIVER_SUBJECT: &str = "review-coverage-waiver";

/// The one decision value that counts as an affirmative waiver. Mirrors
/// `WAIVER_DECISION` in cli/src/fno/pr/_coverage_gate.py: a single law row at
/// a waiver subject counts ONLY when its decision equals this string - row
/// existence carries no polarity, so a note or a denial recorded at the
/// subject reads as no waiver. An exact match against a constant, never free
/// prose parsing.
pub(super) const WAIVER_DECISION: &str = "review coverage waived for this head";

pub(super) fn scoped_waiver_subject(repo_slug: &str, pr_number: i64, head: &str) -> String {
    format!("{STANDING_WAIVER_SUBJECT}:{repo_slug}#{pr_number}@{head}")
}

/// Three-state current-law verdict for one subject, read through the one
/// canonical CLI query (`fno backlog decisions <subject> --lane law --state
/// live --json`). The verdict is derived from the row list, never trusted
/// from `current_law.status`: the rows are filtered to `operator` authority
/// BEFORE the count, because a waiver asserts a person at a terminal read
/// the diff and `chat_attested` rows cannot carry that fact. A single
/// filtered row counts ONLY when its decision equals `WAIVER_DECISION`
/// (row existence carries no polarity); everything else is `NoLaw` or
/// `Unknown`: a nonzero exit (the reader refuses a damaged index), a dead
/// probe, a conflict among operator rows, or malformed output all answer
/// UNKNOWN authority, which no consumer may read as either permission or
/// absence. Mirrors `law_authority` on the Python side, seam for seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LawStatus {
    Single,
    NoLaw,
    Unknown,
}

pub(super) fn current_law_status(fno_bin: &str, cwd: &Path, subject: &str) -> LawStatus {
    match run_bounded(
        fno_bin.as_ref(),
        &[
            "backlog",
            "decisions",
            subject,
            "--lane",
            "law",
            "--state",
            "live",
            "--json",
        ],
        cwd,
        stopgate_read_timeout(),
    ) {
        BoundedRun::Completed(out) if out.status.success() => {
            let parsed = serde_json::from_slice::<Value>(&out.stdout).ok();
            // Mirror `law_authority` exactly: filter the rows to operator
            // authority BEFORE the count, because a waiver asserts a person
            // at a terminal read the diff and only `operator` carries that
            // fact (any harness-identified session records `chat_attested`
            // through the law door). Deriving from the rows rather than
            // trusting current_law.status is the point: the CLI's verdict
            // counts chat_attested rows too, so a forged row beside a real
            // operator waiver would read conflict here and wedge the loop on
            // a waiver the merge gate honors. An affirmative row with no
            // readable decision stays malformed authority, Unknown, never a
            // clean no.
            let Some(rows) = parsed
                .as_ref()
                .and_then(|v| v.get("decisions"))
                .and_then(|d| d.as_array())
            else {
                // A payload without the decisions array is not a shape the
                // CLI emits: a failed instrument, never a clean no.
                return LawStatus::Unknown;
            };
            let operator_rows: Vec<&Value> = rows
                .iter()
                .filter(|r| r.get("authority_source").and_then(|a| a.as_str()) == Some("operator"))
                .collect();
            match operator_rows.as_slice() {
                [] => LawStatus::NoLaw,
                [row] => match row.get("decision").and_then(|s| s.as_str()) {
                    Some(d) if d == WAIVER_DECISION => LawStatus::Single,
                    Some(_) => LawStatus::NoLaw,
                    None => LawStatus::Unknown,
                },
                _ => LawStatus::Unknown,
            }
        }
        _ => LawStatus::Unknown,
    }
}

/// The operator-waiver overlay for a head the computed verdict did not cover:
/// `(Some(description), authority_unknown)`. Scoped first - explicit,
/// per-head, strong enough to clear even a hard finding, dead on the next
/// push - then the standing ruling, which never clears an unresolved
/// CONFIRMED correctness or security finding. `authority_unknown` is true
/// when either probe could not answer; a caller naming it must never report
/// "no waiver" on that evidence. Mirrors `operator_waiver_verdict` in
/// cli/src/fno/pr/_coverage_gate.py.
pub fn operator_waiver(
    fno_bin: &str,
    cwd: &Path,
    repo_slug: &str,
    pr_number: i64,
    head: &str,
    hard_blocker: bool,
) -> (Option<String>, bool) {
    let scoped = if repo_slug.is_empty() || head.is_empty() {
        LawStatus::NoLaw
    } else {
        current_law_status(
            fno_bin,
            cwd,
            &scoped_waiver_subject(repo_slug, pr_number, head),
        )
    };
    if scoped == LawStatus::Single {
        return (
            Some(format!(
                "head-pinned operator waiver at {}",
                short_sha(head)
            )),
            false,
        );
    }
    let standing = current_law_status(fno_bin, cwd, STANDING_WAIVER_SUBJECT);
    if standing == LawStatus::Single && !hard_blocker {
        return (Some("standing operator law".to_string()), false);
    }
    (
        None,
        scoped == LawStatus::Unknown || standing == LawStatus::Unknown,
    )
}

/// The one POST shape every coverage-marker writer uses.
pub(super) fn post_coverage_status(
    gh_bin: &str,
    cwd: &Path,
    head: &str,
    context: &str,
    state: &str,
    description: &str,
) {
    let target = format!("repos/:owner/:repo/statuses/{head}");
    let state_arg = format!("state={state}");
    let context_arg = format!("context={context}");
    let description_arg = format!("description={description}");
    let result = bounded_read(
        gh_bin.as_ref(),
        &[
            "api",
            "--method",
            "POST",
            target.as_str(),
            "-f",
            state_arg.as_str(),
            "-f",
            context_arg.as_str(),
            "-f",
            description_arg.as_str(),
        ],
        cwd,
        "coverage_status_post",
        stopgate_read_timeout(),
    );
    match result {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let error = GhReadError::failed("coverage_status_post", stderr_tail(&out.stderr_tail));
            log_bounded_read_error("review-coverage publisher", &error);
        }
        Err(error) => log_bounded_read_error("review-coverage publisher", &error),
    }
}

#[allow(clippy::too_many_arguments)]
/// The `failure` status description for an uncovered head.
///
/// NEVER names a harness-specific verb, and the rule is the whole reason this
/// is a named function rather than an inline format. This description is ONE
/// string on a shared GitHub commit status: written once by whichever harness
/// happened to publish it, then read by every harness, every CI runner, and
/// every human on the PR page. Embedding the publisher's own verb hands a
/// claude `/code-review ...` to a codex worker, which has no such command -
/// measured, not theoretical. The harness-neutral command named here resolves
/// the right verb LOCALLY, on whatever harness runs it, which is the only
/// place that resolution is correct.
///
/// The worker's own held reason (`coverage_receipt_line`) still names the
/// sized verb, and rightly: that text is read in the session that produced
/// it, on that session's harness. Shared artifact, neutral; local text,
/// sized. That is the line.
///
/// Stays well inside GitHub's 140-char description cap at any realistic PR
/// number, so there is no length branch to get wrong.
pub(super) fn uncovered_status_description(pr_head_oid: &str, pr_number: i64) -> String {
    format!(
        "no covered review at {}; run `fno do target request-self-review --pr {}`",
        short_sha(pr_head_oid),
        pr_number
    )
}

pub(super) fn publish_coverage_status(
    gh_bin: &str,
    fno_bin: &str,
    cwd: &Path,
    repo_slug: &str,
    pr_number: i64,
    pr_head_oid: &str,
    event_head: &str,
    coverage: &CoverageReport,
    required_bots: &[String],
    _optional_bots: &[String],
    optional_lane_configured: bool,
    reviewers: &[String],
    // Whether the review round budget is spent. Past the cap the
    // required-local-pass veto below must not hold the status red: the only
    // thing that clears it is another review round, and the budget will not
    // fund one.
    budget_spent: bool,
    // Whether an unresolved CONFIRMED correctness or security finding
    // remains, budget aside. The standing operator-law waiver consults it and
    // never clears a hard finding; the head-scoped waiver does.
    hard_blocker: bool,
) {
    // A status target that is not a real 40-hex sha (an unresolved local
    // HEAD, the "unknown" sentinel from a failed git read) would POST to a
    // garbage path, or worse to the canonical checkout's default-branch tip -
    // a red marker on a commit whose coverage was never evaluated.
    if pr_number <= 0 || !crate::verify_evidence::full_sha(pr_head_oid) {
        eprintln!(
            "review-coverage publisher: not posting for invalid target pr={pr_number} head={pr_head_oid}"
        );
        return;
    }
    // The lane predicate is the gate's, not a local variant: bots and
    // reviewers, never external_reviewers (the /pr invocation list, not a
    // gate axis). Counting it made this writer post failure on configs the
    // Python merge gate answers "no lane" to - two writers of one context
    // posting opposite states.
    let lane = !(required_bots.is_empty() && !optional_lane_configured && reviewers.is_empty());
    if !lane {
        eprintln!(
            "review-coverage publisher: not posting for {pr_head_oid}: no review lane configured"
        );
        return;
    }
    // The verdict describes event_head. A marker on pr_head_oid built from a
    // different sha - an unpushed local HEAD at stop time, the canonical
    // checkout's default-branch tip - aims at a commit the row never
    // described; the refresher owns head moves, this writer only speaks for
    // the head it evaluated.
    if event_head != pr_head_oid {
        eprintln!(
            "review-coverage publisher: not posting for PR head {pr_head_oid}: event describes {event_head}"
        );
        return;
    }
    // The override first, mirroring the Python publisher: the label outranks
    // the verdict, and its green must not be clobbered by this writer. The
    // actor is named by the workflow's labeled arm, which sees the event.
    // A label read that remains unavailable after retrying consults the
    // current description. Only an existing override marker stays protected;
    // any other marker is stale relative to the fresh computed verdict below.
    match pr_has_override_label(gh_bin, cwd, pr_number) {
        Some(true) => {
            post_coverage_status(
                gh_bin,
                cwd,
                pr_head_oid,
                COVERAGE_STATUS_CONTEXT,
                "success",
                "coverage-override label applied on the PR",
            );
            // The diagnostic mirrors the Python override arm, not the
            // possibly-Unknown computed read: a waived review must not wear
            // "retry the review verb" beside its override success, and the
            // two writers of one context must post the same state.
            post_coverage_status(
                gh_bin,
                cwd,
                pr_head_oid,
                COVERAGE_UNAVAILABLE_STATUS_CONTEXT,
                "success",
                &format!("coverage read healthy at {}", short_sha(pr_head_oid)),
            );
            return;
        }
        None => match current_coverage_description(gh_bin, cwd, pr_head_oid) {
            Some(description) if description.starts_with("coverage-override") => {
                eprintln!(
                    "review-coverage publisher: not posting for {pr_head_oid}: protected existing coverage-override marker"
                );
                return;
            }
            Some(description) => {
                eprintln!(
                    "review-coverage publisher: override label unreadable; current description '{}' is not an override, publishing computed verdict for {pr_head_oid}",
                    description
                );
            }
            None => {
                eprintln!(
                    "review-coverage publisher: override label and current description unreadable; publishing computed verdict for {pr_head_oid}"
                );
            }
        },
        Some(false) => {}
    }
    // Past the cap the configured code-review pass is no longer required, for
    // the same reason the merge gate discharges there: "attest at this head"
    // is satisfiable only by a round the budget will not fund, and every FIX
    // moves HEAD and voids the last attestation. Leaving the veto in place
    // kept fno/review-coverage RED forever on exactly the PRs the cap had
    // already released, which is the unpassable-guard shape the cap exists to
    // end. Coverage itself already discharged upstream; this is the same
    // ruling applied to the published status.
    let local_pass_required = !budget_spent && reviewers.iter().any(|r| is_code_review_reviewer(r));
    // event_head == pr_head_oid already holds; the early return above enforces it.
    let covered = coverage.coverage.is_covered()
        && (!local_pass_required
            || coverage.verdicts.iter().any(|v| {
                is_code_review_reviewer(&v.name)
                    && v.producer == CoverageProducer::LocalAttestation
                    && v.verdict == CoverageVerdict::Reviewed
            }));
    // The operator-law overlay, consulted ONLY where the computed verdict did
    // not cover, exactly as the Python merge gate consults it: a waiver green
    // posted here is what the merge gate would allow, and the two writers of
    // one context must never disagree about that. Unknown authority never
    // posts success - it names itself on the failure description instead, so
    // a reader can tell "no waiver" from "the store could not answer".
    let mut waiver_description: Option<String> = None;
    let mut authority_unknown = false;
    if !covered && !matches!(coverage.coverage, Coverage::Unknown) {
        let (waiver, unknown) = operator_waiver(
            fno_bin,
            cwd,
            repo_slug,
            pr_number,
            pr_head_oid,
            hard_blocker,
        );
        waiver_description = waiver;
        authority_unknown = unknown;
    }
    if let Some(description) = waiver_description {
        eprintln!(
            "review-coverage publisher: operator waiver green for {pr_head_oid}: {description}"
        );
        post_coverage_status(
            gh_bin,
            cwd,
            pr_head_oid,
            COVERAGE_STATUS_CONTEXT,
            "success",
            &description,
        );
        // The diagnostic mirrors the waiver, not the computed read: a waived
        // review must not wear "retry the review verb" beside its waiver
        // success, same as the override arm above.
        post_coverage_status(
            gh_bin,
            cwd,
            pr_head_oid,
            COVERAGE_UNAVAILABLE_STATUS_CONTEXT,
            "success",
            &format!("coverage read healthy at {}", short_sha(pr_head_oid)),
        );
        return;
    }
    let (state, description) = if covered {
        let Coverage::Covered(n) = coverage.coverage else {
            eprintln!(
                "review-coverage publisher: not posting for {pr_head_oid}: covered predicate had no Covered count"
            );
            return;
        };
        (
            "success",
            format!("covered: {} reviewed at {}", n, short_sha(pr_head_oid)),
        )
    } else if matches!(coverage.coverage, Coverage::Unknown) {
        ("pending", coverage_unavailable_description(pr_head_oid))
    } else {
        let mut description = uncovered_status_description(pr_head_oid, pr_number);
        if authority_unknown {
            description = format!("{description}; operator waiver authority unknown");
        }
        // ASCII only, so a byte truncate cannot split a character.
        if description.len() > 140 {
            description.truncate(140);
        }
        ("failure", description)
    };
    post_coverage_status(
        gh_bin,
        cwd,
        pr_head_oid,
        COVERAGE_STATUS_CONTEXT,
        state,
        &description,
    );
    let (diagnostic_state, diagnostic_description) =
        coverage_instrument_status(&coverage.coverage, pr_head_oid);
    post_coverage_status(
        gh_bin,
        cwd,
        pr_head_oid,
        COVERAGE_UNAVAILABLE_STATUS_CONTEXT,
        diagnostic_state,
        &diagnostic_description,
    );
}
