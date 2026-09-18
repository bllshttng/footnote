//! The range-tiling answer: do a branch's attestation ranges, taken as a
//! chain, tile `merge_base(base, head)..head`? Named by its question because
//! `loopcheck.rs` is over the file budget and shrink-only, the same rule that
//! minted `review_freshness.rs`, whose carry vocabulary the chain asks in
//! change 3 of x-ee4c.

use std::path::Path;

use super::{git_bounded, in_scope_chain, rounds_since_last_pass};

/// The union-of-ranges coverage answer over a branch's attestations: whether
/// the `reviewed_base_sha..reviewed_head_sha` ranges on the branch's
/// attestations, taken as a CHAIN, tile `merge_base(base, head)..head`.
///
/// This is what lets a fix-and-re-review loop terminate: every commit that
/// fixes a finding moves the head, and under a single-attestation freshness
/// rule every fix voids the only artifact that can clear the gate. A chain
/// covers the union of what its members read, so round N+1 only has to cover
/// the delta round N left behind.
///
/// Fail-closed everywhere: an unresolvable sha, a range whose endpoints are
/// not on the branch (rebased-away history), a git invocation that fails, or
/// any uncovered commit, all produce `tiled: false` with the gap named by
/// sha - never a silent "covered".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeTiling {
    /// Whether the chain's ranges cover every commit in `merge_base..head`.
    pub tiled: bool,
    /// Uncovered stretches, each named `parent-of-first-uncovered..last-
    /// uncovered` by sha. Empty iff tiled.
    pub gaps: Vec<(String, String)>,
    /// Range head shas dropped from the chain (unresolvable, or off the
    /// branch's ancestry), reported by sha so the drop is auditable.
    pub dropped: Vec<String>,
    /// `reviewed_head_sha` of every range that participated in the chain.
    /// A local attestation whose head is in this list counts as Reviewed
    /// when the whole chain tiles, whatever its single-sha freshness says.
    pub chain_heads: Vec<String>,
    /// Review rounds across the whole life of the PR, one per reviewed head
    /// (see [`rounds_since_last_pass`]). Carried on the chain analysis because it
    /// reads the same events with the same scoping; computed even on the
    /// git-failure paths, which answer tiling fail-closed but rounds honestly.
    pub rounds_used: i64,
    /// The `config.review.max_rounds` budget `rounds_exhausted` was computed
    /// against, carried so the row is self-contained: `fno do pr status`
    /// prints the gate's pair verbatim instead of re-reading config (one
    /// producer per number; ).
    pub rounds_max: i64,
    /// Whether `rounds_used` reaches the resolved `config.review.max_rounds`.
    /// At the cap the review obligation is satisfied - the merge gate
    /// discharges every open finding - so this flag OPENS the gate, never
    /// closes it.
    pub rounds_exhausted: bool,
    /// Whether ANY hard non-terminal finding remains, budget aside. The
    /// standing operator-law waiver consults this below the cap: it may waive
    /// an uncovered review but never an unresolved CONFIRMED correctness or
    /// security finding. At the cap the budget discharges it like any other.
    pub hard_blocker: bool,
}

/// Run `git rev-list` and return its commit lines, oldest first only when
/// `--reverse` is in the args. None on any git failure (fail closed).
fn git_rev_list(git_bin: &str, cwd: &Path, args: &[&str]) -> Option<Vec<String>> {
    // Through the bounded runner like every other stop-gate git read: a hung
    // `rev-list` on a pathological history must read as not-tiled, never
    // wedge the hook that called it.
    let mut argv: Vec<&str> = vec!["rev-list"];
    argv.extend_from_slice(args);
    let out = git_bounded(git_bin, &argv, cwd)?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// Compute the range tiling for one PR's attestation chain.
///
/// Mechanics, no LLM, no new data: order the ancestry walk
/// (`git rev-list --ancestry-path --reverse merge_base..head`), mark every
/// walk commit covered by some in-scope attestation's range, and read the
/// gap set off the marking. A merge commit from main into the branch is
/// covered when a range spans it; it is not special-cased.
pub fn compute_range_tiling(
    git_bin: &str,
    cwd: &Path,
    base_ref: &str,
    events_text: &str,
    head_branch: &str,
    head_sha: &str,
    max_rounds: i64,
) -> RangeTiling {
    let mut tiling = RangeTiling::default();
    // Rounds do not depend on the git walk, so they are computed before the
    // fail-closed early returns: a merge-base failure answers tiling
    // not-tiled but the round budget honestly. This default is the
    // events-only answer; each caller that holds review objects refreshes
    // both axes on top of it (the external arm unconditionally, the
    // no-external arm behind the same gate the Python merge gate uses).
    tiling.rounds_used = rounds_since_last_pass(events_text, head_branch, head_sha, None);
    tiling.rounds_exhausted = tiling.rounds_used >= max_rounds.max(1);
    tiling.rounds_max = max_rounds;
    // The merge base decides where coverage must start. An unresolvable one
    // answers the whole question fail-closed.
    let merge_out = git_bounded(git_bin, &["merge-base", head_sha, base_ref], cwd);
    let merge_base = merge_out
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty());
    let Some(merge_base) = merge_base else {
        return tiling;
    };
    let walk_spec = format!("{merge_base}..{head_sha}");
    let Some(walk) = git_rev_list(git_bin, cwd, &["--ancestry-path", "--reverse", &walk_spec])
    else {
        return tiling;
    };
    let mut position: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, sha) in walk.iter().enumerate() {
        position.insert(sha.as_str(), i);
    }

    // The in-scope attestation ranges for this PR: same scoping rule the pass
    // scan uses (branch field, with the legacy exact-head admission). Both
    // verdicts count - a review that found bugs still READ its range, and the
    // disposition gate (not coverage) is what its findings must satisfy.
    // Collected through the ONE shared chain helper, not a fourth hand-copy.
    let mut ranges: Vec<(String, String)> = Vec::new();
    for val in in_scope_chain(events_text, head_branch, head_sha) {
        let range_base = val
            .pointer("/data/reviewed_base_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let range_head = val
            .pointer("/data/reviewed_head_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if range_base.is_empty() || range_head.is_empty() {
            continue; // pre-range events carry no tile; they cover nothing here
        }
        ranges.push((range_base, range_head));
    }

    // A valid range's endpoints sit on the branch walk: its head at some
    // position, and its base either the merge base itself or a walk commit.
    // Anything else is dropped BY SHA - a rebased-away base, a head that no
    // longer resolves - so the refusal names what fell out rather than
    // silently shrinking the chain.
    let mut valid: Vec<(usize, usize, String)> = Vec::new();
    for (base, head) in ranges {
        let head_pos = position.get(head.as_str()).copied();
        let base_pos = if base == merge_base {
            Some(usize::MAX) // sentinel: covers the walk from its first commit
        } else {
            position.get(base.as_str()).copied()
        };
        match (base_pos, head_pos) {
            (Some(bp), Some(hp)) => {
                let start = if bp == usize::MAX { 0 } else { bp + 1 };
                if hp + 1 >= start {
                    valid.push((start, hp, head));
                } else {
                    tiling.dropped.push(head);
                }
            }
            _ => tiling.dropped.push(head),
        }
    }

    let mut covered = vec![false; walk.len()];
    for (start, end, _) in &valid {
        for i in *start..=*end {
            if i < covered.len() {
                covered[i] = true;
            }
        }
    }
    tiling.tiled = covered.iter().all(|c| *c);
    tiling.chain_heads = valid
        .iter()
        .map(|(_, _, h)| h.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    // Name each maximal uncovered run `parent-of-first..last` so the remedy
    // is a range to review, never the head-loops "run the review verb at
    // HEAD" that produced six rounds on one PR.
    let mut i = 0usize;
    while i < covered.len() {
        if covered[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < covered.len() && !covered[i] {
            i += 1;
        }
        let end = i - 1;
        let gap_base = if start == 0 {
            merge_base.clone()
        } else {
            walk[start - 1].clone()
        };
        tiling.gaps.push((gap_base, walk[end].clone()));
    }
    tiling
}
