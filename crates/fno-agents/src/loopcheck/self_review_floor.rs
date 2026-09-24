//! Who must review this payload? The self-review floor, reviewer invocation table, and payload code-vs-docs classification.

use super::*;

/// The ordering every "you are not reviewed yet" remedy must teach, and the
/// producer it ends at. A verb name alone leaves two ways to waste the round:
/// attesting over an open finding (the gate then lies to the whole board), and
/// reviewing before the last commit is pushed (a later code commit voids the
/// attestation). The emitter is named because the fallback arms of these
/// messages render where `fno` does not resolve, so nothing else in the line
/// tells the reader what actually produces the event.
///
/// `_coverage_refused_reason` in `cli/src/fno/pr/_merge.py` carries the sentence
/// for the merge guard. Two gates refuse the same uncovered head, so they must
/// not teach two different remedies.
pub(super) const REVIEW_ORDER: &str = "close every finding, commit and push first, then \
     review and attest at the final head (`bash skills/review/scripts/emit-attestation.sh \
     <reviewer>` is recovery if a confirmed clean native review hook failed)";

/// The non-interactive invocation that satisfies each local reviewer, mirroring
/// the `invocation` field of `_RESOLVABLE_REVIEWERS` in
/// `cli/src/fno/config/__init__.py`. A block message that names a reviewer
/// without naming how to run it is only half a remedy.
///
/// Two languages, one table: kept honest by
/// `scripts/ci/check-reviewer-descriptor-parity.sh`, not by a comment asking a
/// human to remember.
/// The fourth element encodes per-harness verb overrides as
/// `"harness=verb;harness=verb"`, empty when the scalar invocation is the only
/// rendering - which is now every row: the machinery recommendation is the
/// fno-owned review lane, which runs as ordinary tool calls on every harness,
/// so no native per-harness verb is recommended anymore. The native verbs
/// (`/code-review` on claude, `/review` on codex) remain the operator's
/// explicit choice, documented in docs/architecture/review-lanes.md.
/// `sigma` is RETIRED: its invocation names the replacement (the default
/// lane), never a hint that the panel runs, and a config still naming sigma
/// is refused at init by the Python capability check with the same
/// replacement in the refusal. No `--fix` in any hint: a fix pass moves HEAD
/// and voids the attestation the round just earned. Kept honest against the
/// Python descriptor table by check-reviewer-descriptor-parity.sh.
pub(super) const REVIEWER_INVOCATIONS: &[(&str, &str, bool, &str)] = &[
    ("sigma", "/fno:review", false, ""),
    ("code-review", "/fno:review", false, ""),
    ("declare", "/fno:review declare", true, ""),
];

/// `(invocation, is_self_cert, per_harness)`. The flag mirrors the Python
/// descriptor's `asserts` field: a surface that names `declare` without saying
/// it asserts nothing invites an operator to clear the gate with no review
/// behind it.
pub(super) fn reviewer_entry(name: &str) -> Option<(&'static str, bool, &'static str)> {
    REVIEWER_INVOCATIONS
        .iter()
        .find(|(n, _, _, _)| *n == name)
        .map(|(_, inv, self_cert, per)| (*inv, *self_cert, *per))
}

/// The harness-correct verb. Falls back to the scalar default when the harness
/// is unknown or the reviewer declares no override. `harness` is the author
/// harness from `claims::resolve_harness`, threaded rather than re-read so a
/// unit test can pin a harness without touching the environment.
pub(super) fn reviewer_invocation_for(
    name: &str,
    harness: Option<&str>,
) -> Option<(&'static str, bool)> {
    let (inv, sc, per) = reviewer_entry(name)?;
    if per.is_empty() {
        return Some((inv, sc));
    }
    if let Some(h) = harness {
        for pair in per.split(';') {
            if let Some((ph, pv)) = pair.split_once('=') {
                if ph == h {
                    return Some((pv, sc));
                }
            }
        }
    }
    Some((inv, sc))
}

/// Documentation is `*.md` anywhere and anything under `docs/`. Plan files are
/// markdown, so the `.md` rule covers them; the `internal/` vault is gitignored
/// and never appears in a diff. A config file, a lockfile, and a shell script
/// all count as code.
pub(crate) fn is_documentation_path(path: &str) -> bool {
    // A single leading "./" is stripped once; trim_start_matches would also strip
    // a char set and lstrip a literal-repeated run, diverging from the Python
    // mirror (and mangling ".github"). The two classifiers must agree exactly.
    let trimmed = path.trim();
    let p = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if p.is_empty() {
        return false;
    }
    p.ends_with(".md") || p.starts_with("docs/")
}

/// Whether this harness can run the review this machinery recommends. The
/// recommendation is the fno-owned review lane, which runs as ordinary tool
/// calls wherever the plugin runs, so every harness qualifies unconditionally.
/// The native per-harness verbs this used to gate on (claude `/code-review`,
/// codex `/review`) remain the operator's explicit choice; no machinery
/// depends on them any more, which is the point of owning the reviewer.
/// Mirrors `harness_can_self_review` in `cli/src/fno/review_capability.py`.
pub(super) fn harness_can_self_review(_harness: Option<&str>) -> bool {
    true
}

/// The KNOWN harnesses with no native self-review verb. A set, not
/// `!harness_can_self_review()`, so an UNRECOGNIZED spelling floors instead of
/// passing through as verbless. Python derives this set from `KNOWN_HARNESSES`
/// in `cli/src/fno/harness_names.py` minus the verb table in
/// `cli/src/fno/review_capability.py`; the two are pinned to each other by
/// `test_floor_verbless_set_stays_locked_to_the_rust_twin`, so a harness lands
/// on both sides or the suite goes red. Empty since the owned lane retired
/// the verb table: the fno review lane runs wherever the plugin runs, so no
/// KNOWN harness is verbless for review purposes and every attributed run
/// floors.
pub(super) const KNOWN_VERBLESS_HARNESSES: &[&str] = &[];

/// The self-review FLOOR policy on the author harness. Distinct from
/// the capability question above: `None` answers "unattributable", not
/// "verbless". With the owned lane as the default reviewer no KNOWN harness
/// escapes the floor - claude and codex never did, and gemini/agy/opencode
/// stopped when their release table emptied. An UNRESOLVED harness (absent
/// or ambiguous ambient markers - a claude session started from a codex
/// shell) floors, because ambiguity about who authored the run is not
/// permission to skip its review. The explicit `--author-harness none` pin is
/// the hermetic opt-out and stays unfloored. Mirrors `_harness_can_self_review`
/// in cli/src/fno/pr/_merge.py so the stop gate and the merge gate cannot
/// disagree on the same PR.
pub(super) fn self_review_floor_applies(author_harness: Option<&str>, pinned_none: bool) -> bool {
    match author_harness {
        Some(h) => harness_can_self_review(Some(h)) || !KNOWN_VERBLESS_HARNESSES.contains(&h),
        None => !pinned_none,
    }
}

/// Pure payload classifier: CODE iff any changed path is not documentation.
/// An empty diff is NOT a code payload (no ship, so no gate). Pure over a path
/// slice so unit tests need no git; the git-caller wrapper is `classify_payload`.
pub(super) fn payload_is_code(paths: &[String]) -> bool {
    paths.iter().any(|p| !is_documentation_path(p))
}

/// The self-review reviewer to floor onto the required set, or None. Pure so a
/// unit test can pin the floor without git: a code payload on a lane-less stock
/// install gets `code-review`; a configured lane, an opt-out, a docs payload,
/// and a lane that already names code-review all get None. Returning the name
/// (not a bool) keeps "should floor" and "what to floor" in one place - the
/// reviewer name is the gate input, and splitting them invites drift.
pub(super) fn floor_self_review(
    required_reviewers: &[String],
    lane_configured: bool,
    is_code: bool,
    self_review_required: bool,
) -> Option<String> {
    if !self_review_required || !is_code || lane_configured {
        return None;
    }
    let already = required_reviewers
        .iter()
        .any(|r| r.trim_start_matches('/') == "code-review");
    if already {
        return None;
    }
    Some("code-review".to_string())
}

/// `(is_code, assumed)`: classifies the branch's payload, failing CLOSED. An
/// unreadable diff (neither `origin/main` nor `origin/master`, git missing, any
/// non-zero exit) classifies as code with `assumed=true`, so a degraded probe
/// can never silently disable the obligation the way failing open would. The
/// `<base>...HEAD` three-dot diff names the branch's own changes (the PR
/// diff), not changes that landed on the base since the branch point. A ref
/// that RESOLVES but yields no readable diff (unrelated histories: the diff
/// exits 128 "no merge base") never falls through to the other candidate - a
/// stale pre-migration sibling would size the payload from a whole era.
pub(super) fn classify_payload(git_bin: &str, cwd: &Path) -> (bool, bool) {
    for base in ["origin/main", "origin/master"] {
        match git_bounded(
            git_bin,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{base}^{{commit}}"),
            ],
            cwd,
        ) {
            Some(v) if v.status.success() => {}
            _ => continue,
        }
        if let Some(o) = git_bounded(
            git_bin,
            &["diff", "--name-only", &format!("{base}...HEAD")],
            cwd,
        ) {
            if o.status.success() {
                let paths = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>();
                return (payload_is_code(&paths), false);
            }
        }
        return (true, true);
    }
    (true, true)
}

/// `(is_code, assumed)` for the self-review floor, mirroring the merge gate's
/// payload conjunct (`_pr_payload_is_code` in cli/src/fno/pr/_merge.py), which
/// classifies the PR. The cwd diff stays the FAST PATH: when it already says
/// code, the floor engages and no gh is spent. Only when the cwd diff says
/// not-code - the direction where a checkout's diff can diverge from the PR
/// (empty diff in a directory the fire does not sit in, a branch already
/// contained in the base) - is the PR consulted, because a floor that answers
/// for a directory can be more permissive than the merge it arms. A PR that
/// exists but cannot be read fails closed to code, matching
/// `classify_payload`'s degraded shape. A branch with no PR keeps the cwd
/// answer (the pre-PR fallback the merge gate has no counterpart for) - but a
/// NAMED PR (`pr_selector`, the review-coverage verb's --pr) that resolves to
/// nothing is a degraded read of the PR under evaluation, never a no-PR
/// branch, so it fails closed instead of falling back.
pub(super) fn classify_payload_for_floor(
    gh_bin: &str,
    git_bin: &str,
    cwd: &Path,
    pr_selector: Option<&str>,
) -> (bool, bool) {
    let local = classify_payload(git_bin, cwd);
    if local.0 {
        return local;
    }
    match read_pr_view(gh_bin, cwd, pr_selector) {
        Ok(Some(view)) => {
            let number = view
                .get("number")
                .and_then(|v| v.as_i64())
                .unwrap_or_default();
            if number <= 0 {
                // A malformed view is a degraded probe, not a docs PR.
                return (true, true);
            }
            let target = format!("repos/{{owner}}/{{repo}}/pulls/{number}/files");
            let files = bounded_read(
                gh_bin.as_ref(),
                &["api", &target, "--paginate"],
                cwd,
                "pr_files",
                stopgate_read_timeout(),
            );
            let Ok(out) = files else {
                return (true, true);
            };
            if !out.status.success() {
                return (true, true);
            }
            // --paginate may emit CONCATENATED JSON arrays (one per page), the
            // same shape the pulls-comments read parses.
            let mut paths: Vec<String> = Vec::new();
            for page in serde_json::Deserializer::from_slice(&out.stdout).into_iter::<Value>() {
                let Ok(page) = page else {
                    return (true, true);
                };
                let Some(entries) = page.as_array() else {
                    return (true, true);
                };
                for entry in entries {
                    if let Some(name) = entry.get("filename").and_then(|v| v.as_str()) {
                        let trimmed = name.trim();
                        if !trimmed.is_empty() {
                            paths.push(trimmed.to_string());
                        }
                    }
                }
            }
            // An empty file list is not code, matching _pr_payload_is_code.
            (payload_is_code(&paths), false)
        }
        // No PR for the branch: the cwd answer stands (pre-PR fire). A NAMED
        // PR resolving to nothing is the degraded direction - the PR under
        // evaluation could not be read - so it fails closed.
        Ok(None) => {
            if pr_selector.is_some() {
                (true, true)
            } else {
                local
            }
        }
        // The view failed without being a no-PR answer: degraded probe.
        Err(_) => (true, true),
    }
}

// ── review freshness: one predicate, both producers ─────────
//
// The predicate, its git reads, and the resolver live in
// `review_freshness.rs`, a module named by their question: `loopcheck.rs` is
// over the file budget and shrink-only, so the freshness machinery moved there
// rather than growing here. The interdiff-carry arm (law d-608344c1)
// rode the same move.
