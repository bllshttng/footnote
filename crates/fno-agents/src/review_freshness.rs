//! Review freshness: one predicate, both producers (/).
//!
//! Freshness used to be decided TWICE with two different rules: a `github_app`
//! verdict got none at all (a bot opinion was inherited across commits it never
//! read), while a `local_attestation` got a bare sha equality so strict that
//! addressing a review destroyed the proof the review happened. One design,
//! failing opposite ways on its two producers. [`review_freshness`] is the
//! single rule both now go through.
//!
//! This module is named by its question ("is a review at sha A still about sha
//! B?") because `loopcheck.rs` is over the file budget and shrink-only; the
//! predicate and its git reads live here so the budget gate stays honest.

use std::path::Path;

use crate::loopcheck::{git_bounded, is_documentation_path};

/// Whether a review verdict still describes the code at HEAD.
///
/// The `Carried` variants are the reason a carry was granted, recorded on
/// the event so a carry is auditable and can never be mistaken for a fresh
/// read. Only `Stale` stops a verdict counting toward coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The reviewer read this exact commit.
    Fresh,
    /// The PR's own code delta is byte-identical; any tree difference came from
    /// the base moving under it. A rebase is this shape, which is what makes
    /// the mandatory pre-merge rebase stop destroying attestations.
    CarriedBaseSync,
    /// Only documentation paths changed between the reviewed commit and HEAD.
    CarriedDocsOnly,
    /// The PR's own code delta only SHRANK since the review: every raw diff
    /// line still shipping is byte-identical to one the reviewer read, and the
    /// vanished lines are paths the base absorbed on the rebase. A strict
    /// subset of the reviewed diff; the partly-docs partly-shrink rebase that used to fall
    /// through to `Stale` because the grades were whole-diff and mutually
    /// exclusive.
    CarriedSubset,
    /// The PR's own code patch changed, but by FEWER than
    /// `review.carry_interdiff_lines` lines (multiset symmetric difference of
    /// the two patches' content lines): the interdiff-carry arm of law
    /// d-608344c1. A rebase whose conflict resolution touched three lines
    /// reads here instead of costing a full round. `lines` is the measured
    /// difference, `cap` the configured bound, so the verdict is auditable
    /// without re-running git.
    CarriedInterdiff { lines: usize, cap: usize },
    /// Everything else, including every failure path.
    Stale,
}

impl Freshness {
    /// Whether a verdict at this freshness counts toward coverage.
    pub fn counts(&self) -> bool {
        !matches!(self, Freshness::Stale)
    }

    /// Whether a verdict recorded at another sha covers the WHOLE of
    /// `merge_base..head` here, which is the tiling question rather than the
    /// coverage question.
    ///
    /// `counts()` answers "did a reviewer read this PR". This answers the
    /// stronger "was every line still shipping read", so it is deliberately
    /// narrower: `CarriedInterdiff { lines: n }` with `n > 0` counts as a
    /// review under law d-608344c1 and still does, but `n` lines nobody read
    /// are `n` lines nobody read, and a tile may not assert otherwise. A zero
    /// interdiff is the same proof `CarriedBaseSync` carries by another route
    /// (PR 2137 measured one), so it tiles.
    pub fn tiles_whole_range(&self) -> bool {
        match self {
            Freshness::Fresh
            | Freshness::CarriedBaseSync
            | Freshness::CarriedDocsOnly
            | Freshness::CarriedSubset => true,
            Freshness::CarriedInterdiff { lines, .. } => *lines == 0,
            Freshness::Stale => false,
        }
    }
}

impl Freshness {
    /// The event-facing label. Carries are named by their reason and, for the
    /// interdiff arm, by their numbers, so a reader of the emitted verdict can
    /// see how close the carry sits to its cap without re-running git.
    pub fn as_label(&self) -> String {
        match self {
            Freshness::Fresh => "fresh".to_string(),
            Freshness::CarriedBaseSync => "carried_base_sync".to_string(),
            Freshness::CarriedDocsOnly => "carried_docs_only".to_string(),
            Freshness::CarriedSubset => "carried_subset".to_string(),
            Freshness::CarriedInterdiff { lines, cap } => {
                format!("carried_interdiff(n={lines}, cap={cap})")
            }
            Freshness::Stale => "stale".to_string(),
        }
    }
}

impl serde::Serialize for Freshness {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_label())
    }
}

impl<'de> serde::Deserialize<'de> for Freshness {
    /// The inverse of [`Freshness::as_label`], so an emitted verdict row can be
    /// re-read by the language that wrote it. Anything unparseable - including
    /// a label from a FUTURE variant - reads `Stale`, the fail-closed
    /// direction: an unknown label must never count toward coverage.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let label = String::deserialize(d)?;
        Ok(match label.as_str() {
            "fresh" => Freshness::Fresh,
            "carried_base_sync" => Freshness::CarriedBaseSync,
            "carried_docs_only" => Freshness::CarriedDocsOnly,
            "carried_subset" => Freshness::CarriedSubset,
            "stale" => Freshness::Stale,
            other => {
                // "carried_interdiff(n=37, cap=100)" - the one parameterized
                // label. A malformed render is not evidence of freshness.
                let Some(rest) = other.strip_prefix("carried_interdiff(n=") else {
                    return Ok(Freshness::Stale);
                };
                let Some((n, cap)) = rest.trim_end_matches(')').split_once(", cap=") else {
                    return Ok(Freshness::Stale);
                };
                match (n.parse::<usize>(), cap.parse::<usize>()) {
                    (Ok(lines), Ok(cap)) => Freshness::CarriedInterdiff { lines, cap },
                    _ => Freshness::Stale,
                }
            }
        })
    }
}

/// One side's code-diff identity: the blake3 hash plus the sorted raw diff
/// lines it was computed over. The hash answers "identical or not"; the line
/// set also answers "is HEAD a subset of what was reviewed", which is the
/// [`Freshness::CarriedSubset`] question and cannot be asked of a hash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeDiffIdentity {
    pub hash: String,
    pub lines: Vec<String>,
}

/// Pre-computed git facts for one `(reviewed_sha, head_sha)` pair, so
/// [`review_freshness`] is pure and unit-tests with no git and no repository.
#[derive(Debug, Clone, Default)]
pub struct FreshnessFacts {
    /// PR code-diff identity at the reviewed commit (see
    /// [`pr_code_diff_identity`]).
    pub reviewed_identity: Option<CodeDiffIdentity>,
    /// The same identity at HEAD.
    pub head_identity: Option<CodeDiffIdentity>,
    /// Paths differing between the two TREES (two-dot). `None` on git failure.
    pub tree_paths: Option<Vec<String>>,
    /// Multiset symmetric difference of the two patches' content lines
    /// ([`interdiff_lines_between`]). `None` on git failure, an over-cap read,
    /// or whenever the identities did not need it (equal, or absent). `None`
    /// never carries.
    pub interdiff_lines: Option<usize>,
    /// The resolved `review.carry_interdiff_lines`. `0` disables the
    /// [`Freshness::CarriedInterdiff`] arm, which is also what a
    /// `Default` value does: default-constructed facts fail closed.
    pub carry_interdiff_lines: usize,
}

/// The one freshness rule. Pure over pre-computed facts.
///
/// `Carried` requires a POSITIVE identity match between two successfully
/// computed identities. Two `None`s never match, and neither does an empty
/// result: matching an absence against an absence is what produced this plan's
/// first (wrong) 63% carry-forward measurement, where every merged PR's
/// three-dot diff against current `origin/main` was empty and `e3b0c442` - the
/// hash of the empty string - compared equal to itself twelve times. The real
/// figure was 2 of 22. Every failure path lands on `Stale`; there is no input
/// on which a failure produces a carry.
pub fn review_freshness(reviewed_sha: &str, head_sha: &str, facts: &FreshnessFacts) -> Freshness {
    // No pinned commit is not evidence of freshness. An absent `commit.oid`, an
    // attestation with no `head_sha`, and an unresolvable HEAD all land here.
    if reviewed_sha.is_empty() || head_sha.is_empty() {
        return Freshness::Stale;
    }
    if reviewed_sha == head_sha {
        return Freshness::Fresh;
    }
    let (Some(reviewed), Some(head)) = (
        facts.reviewed_identity.as_ref(),
        facts.head_identity.as_ref(),
    ) else {
        return Freshness::Stale;
    };
    if reviewed.hash != head.hash {
        // One side is docs-only and the other carries code: a reviewer who
        // read zero code lines has not read the new code, and a head that
        // dropped all code is a change the reviewer never saw.
        if reviewed.lines.is_empty() || head.lines.is_empty() {
            return Freshness::Stale;
        }
        // The code delta changed, but it can still have only SHRUNK: every raw
        // line still shipping was read, and the lines that vanished are paths
        // the base absorbed on the rebase. Each raw line carries both blob
        // shas for one path, so a line present in both sets means that path's
        // change is byte-identical to what the reviewer read. A strict subset
        // carries; a superset (new unreviewed code) and a rewrite do not.
        if is_strict_subset(&head.lines, &reviewed.lines) {
            return Freshness::CarriedSubset;
        }
        // Law d-608344c1's small-change arm: the delta changed, but by fewer
        // than the configured interdiff budget. A rebase whose conflict
        // resolution touched three lines was a full round under the
        // subset-or-stale rule; under the law it carries, with its numbers on
        // the verdict. Unequal identities that read as a subset above never
        // reach here; equal identities took the CarriedBaseSync branch below.
        return match (facts.interdiff_lines, facts.carry_interdiff_lines) {
            (Some(n), cap) if cap > 0 && n < cap => Freshness::CarriedInterdiff { lines: n, cap },
            _ => Freshness::Stale,
        };
    }
    // The identities match, so the code under review is unchanged. The tree
    // diff only names WHY, and a carry that cannot name its reason is not
    // auditable - so an unreadable tree diff is Stale like any other failure.
    let Some(paths) = facts.tree_paths.as_deref() else {
        return Freshness::Stale;
    };
    if !paths.is_empty() && paths.iter().all(|p| is_documentation_path(p)) {
        Freshness::CarriedDocsOnly
    } else {
        Freshness::CarriedBaseSync
    }
}

/// Strict subset over two SORTED raw-diff line sets: strictly smaller, and
/// every HEAD line present in the reviewed set. An empty HEAD set is vacuously
/// a subset, so [`review_freshness`] rejects empty sets before calling this.
fn is_strict_subset(head: &[String], reviewed: &[String]) -> bool {
    head.len() < reviewed.len() && {
        let have: std::collections::HashSet<&str> = reviewed.iter().map(|s| s.as_str()).collect();
        head.iter().all(|l| have.contains(l.as_str()))
    }
}

/// Multiset symmetric difference between two patches' content lines: how many
/// line occurrences differ in total (added on one side, removed on the other,
/// counted separately). Multiset, not set, so a line duplicated three times
/// against once still reads as changed.
fn multiset_symmetric_difference(a: &[String], b: &[String]) -> usize {
    let mut counts: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for line in a {
        *counts.entry(line.as_str()).or_insert(0) += 1;
    }
    for line in b {
        *counts.entry(line.as_str()).or_insert(0) -= 1;
    }
    counts.values().map(|c| c.unsigned_abs() as usize).sum()
}

/// Each patch read is capped at this many stdout bytes; a read at or over the
/// cap is a FAILED read (`None`), never a small number that could carry. A
/// patch big enough to hit this is big enough that no interdiff question
/// about it is cheap to answer.
const PATCH_READ_CAP_BYTES: usize = 8 * 1024 * 1024;

/// The path from a `diff --git` line, or `None` for any other line. Content
/// lines start with `+`, `-`, ` `, or `\`, never `diff --git `, so this is a
/// file-section boundary test, not a content classifier.
fn diff_git_line_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git a/")?;
    rest.split(" b/").next().map(|p| p.trim_matches('"'))
}

/// Content lines of the PR's own CODE patch at `sha`: the three-dot patch
/// from `merge-base(base, sha)`, documentation file sections dropped, the
/// header scaffolding (`diff --git`, `index`, `---`, `+++`, `@@`) dropped.
///
/// `None` on any git failure or an over-cap read. An empty result (`Some` of
/// nothing) means only documentation changed at this sha; the caller pairs it
/// with the identity check, which is `None` on the same shape, so an empty
/// patch never reaches a comparison (the absence rule above).
fn code_patch_lines(git_bin: &str, cwd: &Path, base: &str, sha: &str) -> Option<Vec<String>> {
    let out = git_bounded(
        git_bin,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            &format!("{base}...{sha}"),
        ],
        cwd,
    )?;
    if !out.status.success() || out.stdout.len() > PATCH_READ_CAP_BYTES {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = Vec::new();
    let mut in_docs_section = false;
    for line in text.lines() {
        if let Some(path) = diff_git_line_path(line) {
            in_docs_section = is_documentation_path(path);
            continue;
        }
        if in_docs_section
            || line.is_empty()
            || line.starts_with("index ")
            || line.starts_with("---")
            || line.starts_with("+++")
            || line.starts_with("@@")
        {
            continue;
        }
        lines.push(line.trim_end().to_string());
    }
    Some(lines)
}

/// Multiset symmetric difference of the two commits' code patches, or `None`
/// when either patch is unreadable. This is the law's "diff-to-base before vs
/// after" measure - computed against the BASE on both sides, never a direct
/// `a..b` diff, so base movement contributes nothing.
pub fn interdiff_lines_between(
    git_bin: &str,
    cwd: &Path,
    base: &str,
    reviewed_sha: &str,
    head_sha: &str,
) -> Option<usize> {
    let reviewed_patch = code_patch_lines(git_bin, cwd, base, reviewed_sha)?;
    let head_patch = code_patch_lines(git_bin, cwd, base, head_sha)?;
    Some(multiset_symmetric_difference(&reviewed_patch, &head_patch))
}

/// Content identity of the PR's own CODE changes at `sha`: the three-dot diff
/// from `merge-base(base, sha)`, documentation paths dropped, hashed.
///
/// `--raw --no-abbrev` emits one line per changed path carrying both blob
/// SHAs, so the identity is content-exact without materializing a patch.
/// `--no-renames` pins it against a per-user `diff.renames` config that would
/// otherwise make two runs of the same comparison disagree.
///
/// `None` on any git failure AND when the three-dot diff itself is empty. An
/// empty diff is not positive evidence of anything (a merged PR reads empty
/// against current base), and letting two of them compare equal is the
/// absence-matched-against-absence trap above.
///
/// A readable, non-empty diff whose every path is documentation yields an
/// identity with empty `lines`: positive evidence that no code is under review.
/// [`review_freshness`] never carries across a move between that and a
/// code-bearing identity.
fn pr_code_diff_identity(
    git_bin: &str,
    cwd: &Path,
    base: &str,
    sha: &str,
) -> Option<CodeDiffIdentity> {
    let out = git_bounded(
        git_bin,
        &[
            "diff",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            &format!("{base}...{sha}"),
        ],
        cwd,
    )?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let raw: Vec<String> = text
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if raw.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = raw
        .into_iter()
        .filter(|l| !is_documentation_path(raw_diff_line_path(l)))
        .collect();
    lines.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    Some(CodeDiffIdentity {
        hash: hasher.finalize().to_hex().to_string(),
        lines,
    })
}

/// The path from a `git diff --raw` line (`:<meta>\t<path>`), or `""`.
/// `--no-renames` guarantees one path per line, so there is no second field.
pub(crate) fn raw_diff_line_path(line: &str) -> &str {
    line.split('\t').nth(1).unwrap_or("").trim()
}

/// Paths differing between two TREES (two-dot), or `None` on git failure.
fn git_tree_paths(git_bin: &str, cwd: &Path, a: &str, b: &str) -> Option<Vec<String>> {
    let out = git_bounded(git_bin, &["diff", "--name-only", "--no-renames", a, b], cwd)?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// Resolves `reviewed_sha -> Freshness` against one HEAD, memoized so N
/// verdicts at one commit cost one pair of git calls rather than N.
///
/// The HEAD identity is computed once, on first use: a session whose reviewers
/// are all fresh (the common case) pays no git at all.
pub struct FreshnessResolver<'a> {
    git_bin: &'a str,
    cwd: &'a Path,
    /// The ref the PR merges into, already qualified (`origin/main`). An
    /// unresolvable base yields no identity, hence `Stale` - fail closed.
    pub(crate) base_ref: String,
    head_sha: String,
    /// The resolved `review.carry_interdiff_lines`; `0` disables the
    /// interdiff-carry arm.
    carry_interdiff_lines: usize,
    head_identity: std::cell::RefCell<Option<Option<CodeDiffIdentity>>>,
    cache: std::cell::RefCell<std::collections::HashMap<String, Freshness>>,
    /// One probe per sha: `true` = present in the local store, `false` =
    /// missing and unfetchable. Guarantees at most one fetch per sha.
    fetched: std::cell::RefCell<std::collections::HashMap<String, bool>>,
    /// Shas probed and still absent: commits this run could not measure.
    unmeasured: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

impl<'a> FreshnessResolver<'a> {
    pub fn new(
        git_bin: &'a str,
        cwd: &'a Path,
        base_ref: &str,
        head_sha: &str,
        carry_interdiff_lines: usize,
    ) -> Self {
        let base = base_ref.trim();
        Self {
            git_bin,
            cwd,
            // `gh pr view` returns a BARE branch name, and a branch name may
            // itself contain a slash (`release/2.0`), so "has a slash" does not
            // mean "already remote-qualified" - it only means the caller may
            // have passed one of ours. Test the `origin/` prefix instead: a
            // bare `release/2.0` resolves to a local ref that a fresh worktree
            // usually does not have, and the identity then fails to compute for
            // every commit, silently taking the carry away on exactly the
            // long-lived release branches that rebase most.
            base_ref: if base.is_empty() {
                "origin/main".to_string()
            } else if base.starts_with("origin/") {
                base.to_string()
            } else {
                format!("origin/{base}")
            },
            head_sha: head_sha.to_string(),
            carry_interdiff_lines,
            head_identity: std::cell::RefCell::new(None),
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
            fetched: std::cell::RefCell::new(std::collections::HashMap::new()),
            unmeasured: std::cell::RefCell::new(std::collections::BTreeSet::new()),
        }
    }

    /// True when `sha` names a commit object the local store can read,
    /// fetching it from `origin` once when it does not. A server-side rebase
    /// publishes the new head on GitHub before any local fetch ran, so the
    /// producer of the next coverage row is often that fetch; without it the
    /// identity reads `None` and a real review reads stale. Memoized per sha,
    /// so one sha costs at most one fetch however many verdicts ask about it.
    /// A sha that stays missing is recorded in `unmeasured` and named on
    /// stderr once.
    pub(crate) fn ensure_local(&self, sha: &str) -> bool {
        if sha.is_empty() {
            return false;
        }
        if let Some(&ok) = self.fetched.borrow().get(sha) {
            return ok;
        }
        let present = |s: &str| {
            git_bounded(
                self.git_bin,
                &["cat-file", "-e", &format!("{s}^{{commit}}")],
                self.cwd,
            )
            .map(|o| o.status.success())
            .unwrap_or(false)
        };
        let ok = if present(sha) {
            true
        } else {
            let fetched = git_bounded(
                self.git_bin,
                // `--` pins sha to the refspec slot: attestation rows feed
                // this string, and a `-`-leading one must parse as a ref,
                // never as an option.
                &["fetch", "--quiet", "--no-tags", "origin", "--", sha],
                self.cwd,
            )
            .map(|o| o.status.success())
            .unwrap_or(false);
            let ok = fetched && present(sha);
            if !ok {
                let short: String = sha.chars().take(9).collect();
                eprintln!(
                    "freshness: commit {short} is not in the local object store and fetch failed"
                );
                self.unmeasured.borrow_mut().insert(sha.to_string());
            }
            ok
        };
        self.fetched.borrow_mut().insert(sha.to_string(), ok);
        ok
    }

    /// The shas this resolver probed and could not measure: neither local nor
    /// fetchable from `origin`. A coverage verdict computed while this is
    /// non-empty read absences, not zeros, so the caller demotes a
    /// `Covered(0)` to `Coverage::Unknown` instead of storing it.
    pub fn unmeasured(&self) -> Vec<String> {
        self.unmeasured.borrow().iter().cloned().collect()
    }

    fn head_identity(&self) -> Option<CodeDiffIdentity> {
        let mut slot = self.head_identity.borrow_mut();
        slot.get_or_insert_with(|| {
            pr_code_diff_identity(self.git_bin, self.cwd, &self.base_ref, &self.head_sha)
        })
        .clone()
    }

    /// Freshness of a verdict recorded at `reviewed_sha`. Never panics, never
    /// fails: every unreadable input resolves to `Stale`.
    pub fn freshness(&self, reviewed_sha: &str) -> Freshness {
        if reviewed_sha.is_empty() {
            return Freshness::Stale;
        }
        if reviewed_sha == self.head_sha {
            return Freshness::Fresh;
        }
        if let Some(hit) = self.cache.borrow().get(reviewed_sha) {
            return *hit;
        }
        // A commit the store cannot read is not staleness evidence until the
        // store had its one chance to fetch it; a verdict over an absence
        // never carries, so measure what is present.
        self.ensure_local(reviewed_sha);
        self.ensure_local(&self.head_sha);
        let reviewed_identity =
            pr_code_diff_identity(self.git_bin, self.cwd, &self.base_ref, reviewed_sha);
        let head_identity = self.head_identity();
        // The interdiff read costs a full patch fetch per side, so it runs only
        // when the arm can possibly fire: two present, UNEQUAL identities. The
        // fresh and carried-identity paths - the common cases - pay no extra
        // git.
        let interdiff_lines = match (&reviewed_identity, &head_identity) {
            (Some(r), Some(h))
                if r.hash != h.hash && !r.lines.is_empty() && !h.lines.is_empty() =>
            {
                interdiff_lines_between(
                    self.git_bin,
                    self.cwd,
                    &self.base_ref,
                    reviewed_sha,
                    &self.head_sha,
                )
            }
            _ => None,
        };
        let facts = FreshnessFacts {
            reviewed_identity,
            head_identity,
            tree_paths: git_tree_paths(self.git_bin, self.cwd, reviewed_sha, &self.head_sha),
            interdiff_lines,
            carry_interdiff_lines: self.carry_interdiff_lines,
        };
        let verdict = review_freshness(reviewed_sha, &self.head_sha, &facts);
        self.cache
            .borrow_mut()
            .insert(reviewed_sha.to_string(), verdict);
        verdict
    }
}

/// Order for "which of this reviewer's reviews is the best evidence". Fresh
/// beats a carry beats stale; the carry reasons are equally good, since
/// all of them mean the code under review is unchanged, a subset of what was
/// read, or within the interdiff budget of it.
pub fn freshness_rank(f: Freshness) -> u8 {
    match f {
        Freshness::Fresh => 2,
        Freshness::CarriedBaseSync
        | Freshness::CarriedDocsOnly
        | Freshness::CarriedSubset
        | Freshness::CarriedInterdiff { .. } => 1,
        Freshness::Stale => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident_of(lines: &[&str]) -> CodeDiffIdentity {
        CodeDiffIdentity {
            hash: lines.join("\n"),
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn facts(
        reviewed: Option<&str>,
        head: Option<&str>,
        tree: Option<&[&str]>,
        interdiff: Option<usize>,
        cap: usize,
    ) -> FreshnessFacts {
        FreshnessFacts {
            reviewed_identity: reviewed.map(|h| ident_of(&[h])),
            head_identity: head.map(|h| ident_of(&[h])),
            tree_paths: tree.map(|p| p.iter().map(|s| s.to_string()).collect()),
            interdiff_lines: interdiff,
            carry_interdiff_lines: cap,
        }
    }

    #[test]
    fn tiles_whole_range_grants_only_whole_range_proofs() {
        // The tiling question is stronger than the coverage question: a tile
        // asserts every shipping line was read, so the interdiff arm tiles
        // only at zero, and Stale never tiles whatever counts() says.
        assert!(Freshness::Fresh.tiles_whole_range());
        assert!(Freshness::CarriedBaseSync.tiles_whole_range());
        assert!(Freshness::CarriedDocsOnly.tiles_whole_range());
        assert!(Freshness::CarriedSubset.tiles_whole_range());
        assert!(Freshness::CarriedInterdiff { lines: 0, cap: 100 }.tiles_whole_range());
        assert!(!Freshness::CarriedInterdiff { lines: 1, cap: 100 }.tiles_whole_range());
        assert!(!Freshness::Stale.tiles_whole_range());
    }

    #[test]
    fn interdiff_under_cap_carries_with_its_numbers() {
        // Law d-608344c1's headline shape: a 3-line conflict resolution
        // (measured as a small multiset difference) no longer costs a round.
        let verdict = review_freshness(
            "r",
            "h",
            &facts(Some("i-old"), Some("i-new"), Some(&["a.rs"]), Some(6), 100),
        );
        assert_eq!(verdict, Freshness::CarriedInterdiff { lines: 6, cap: 100 });
        assert!(verdict.counts());
        assert_eq!(
            verdict.as_label(),
            "carried_interdiff(n=6, cap=100)".to_string()
        );
    }

    #[test]
    fn interdiff_at_cap_stales() {
        // 99 carries; 100 does not: the law says UNDER 100 lines.
        assert_eq!(
            review_freshness(
                "r",
                "h",
                &facts(Some("i-old"), Some("i-new"), Some(&["a.rs"]), Some(99), 100)
            ),
            Freshness::CarriedInterdiff {
                lines: 99,
                cap: 100
            }
        );
        assert_eq!(
            review_freshness(
                "r",
                "h",
                &facts(
                    Some("i-old"),
                    Some("i-new"),
                    Some(&["a.rs"]),
                    Some(100),
                    100
                )
            ),
            Freshness::Stale
        );
    }

    #[test]
    fn interdiff_none_never_carries_even_with_both_identities() {
        // An unreadable patch read on either side is a failed read: absence
        // never carries, whatever the identities say.
        assert_eq!(
            review_freshness(
                "r",
                "h",
                &facts(Some("i-old"), Some("i-new"), Some(&["a.rs"]), None, 100)
            ),
            Freshness::Stale
        );
    }

    #[test]
    fn interdiff_cap_zero_disables_the_arm() {
        assert_eq!(
            review_freshness(
                "r",
                "h",
                &facts(Some("i-old"), Some("i-new"), Some(&["a.rs"]), Some(1), 0)
            ),
            Freshness::Stale
        );
    }

    #[test]
    fn docs_only_still_carries_before_the_interdiff_arm() {
        // Arm order: equal identities with an all-documentation tree diff are
        // CarriedDocsOnly, exactly as before the interdiff arm existed.
        assert_eq!(
            review_freshness(
                "r",
                "h",
                &facts(
                    Some("i"),
                    Some("i"),
                    Some(&["docs/architecture/x.md"]),
                    Some(0),
                    100
                )
            ),
            Freshness::CarriedDocsOnly
        );
    }

    #[test]
    fn interdiff_label_serializes_as_its_string() {
        // The event schema pins freshness to a string; the new arm rides the
        // same shape with its numbers inline.
        let value = serde_json::to_value(Freshness::CarriedInterdiff {
            lines: 37,
            cap: 100,
        })
        .unwrap();
        assert_eq!(value, serde_json::json!("carried_interdiff(n=37, cap=100)"));
    }

    #[test]
    fn every_label_round_trips_and_unknowns_fail_closed() {
        // Deserialize is the inverse of as_label, so an emitted row is
        // re-readable by the language that wrote it. A future or malformed
        // label reads Stale: an unknown freshness must never count.
        for fresh in [
            Freshness::Fresh,
            Freshness::CarriedBaseSync,
            Freshness::CarriedDocsOnly,
            Freshness::CarriedSubset,
            Freshness::CarriedInterdiff {
                lines: 37,
                cap: 100,
            },
            Freshness::Stale,
        ] {
            let label = serde_json::to_value(&fresh).unwrap();
            let back: Freshness = serde_json::from_value(label).unwrap();
            assert_eq!(back, fresh, "{fresh:?} did not round-trip");
        }
        for unknown in ["carried_weekly", "carried_interdiff(n=x, cap=100)", ""] {
            let back: Freshness = serde_json::from_value(serde_json::json!(unknown)).unwrap();
            assert_eq!(back, Freshness::Stale, "{unknown:?} must not count");
        }
    }

    #[test]
    fn multiset_difference_counts_duplicates() {
        let a = vec!["x".to_string(), "x".to_string(), "y".to_string()];
        let b = vec!["x".to_string()];
        assert_eq!(multiset_symmetric_difference(&a, &b), 2);
        assert_eq!(multiset_symmetric_difference(&[], &[]), 0);
    }

    // ── the resolver against REAL git history ───────────────────────────────

    fn git(repo: &Path, args: &[&str]) -> String {
        // A machine-level core.hooksPath may exist (editor trash helpers,
        // pre-push guards) and can fail on scratch repos; hook-free git keeps
        // the fixtures deterministic there and a no-op in CI.
        let out = std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// One repo where `reviewed` changed `lines` lines of f.txt and `head`
    /// changed `head_lines` more on top. Returns `(dir, repo, reviewed, head)`.
    fn changed_repo(lines: usize, head_lines: usize) -> (tempfile::TempDir, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        let body = |n: usize, tag: &str| (1..=n).map(|i| format!("{tag}{i}\n")).collect::<String>();
        std::fs::write(repo.join("f.txt"), body(100, "base")).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        // The resolver qualifies `main` to `origin/main`, which must exist as
        // a remote-tracking ref or every identity read fails to `Stale`.
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(repo.join("f.txt"), body(lines, "rev")).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "reviewed"]);
        let reviewed = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("f.txt"), body(head_lines, "head")).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "head"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        (tmp, reviewed, head)
    }

    #[test]
    fn resolver_small_delta_carries_and_large_delta_stales() {
        // Same-shape repo twice: a 5-then-8 line rewrite sits far under the
        // 100-line budget and carries; a 60-then-95 line rewrite sits far
        // over it and does not. One test, both directions, so neither can
        // regress silently behind the other.
        let (tmp, reviewed, head) = changed_repo(5, 8);
        let repo = tmp.path().join("r");
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        let verdict = resolver.freshness(&reviewed);
        assert!(verdict.counts(), "a small rewrite must carry: {verdict:?}");
        assert!(matches!(verdict, Freshness::CarriedInterdiff { .. }));

        let (tmp, reviewed, head) = changed_repo(60, 95);
        let repo = tmp.path().join("r");
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert_eq!(
            resolver.freshness(&reviewed),
            Freshness::Stale,
            "a large rewrite must not carry"
        );
    }

    #[test]
    fn resolver_rebase_still_carries_by_identity_first() {
        // The pre-existing contract on the moved code: a rebase that
        // rewrote every commit but changed no content carries by identity,
        // paying no interdiff read at all.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("code.txt"), "pr change\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "pr"]);
        let reviewed = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("other.txt"), "base moved\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base moved"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&repo, &["checkout", "-q", "feature"]);
        git(&repo, &["rebase", "-q", "origin/main"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert!(resolver.freshness(&reviewed).counts());
    }

    // ── docs-only PRs ───────────────────────────────────────────────────────

    /// A repo on `main` holding `f.txt`, with `origin/main` pointing at it.
    fn base_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        (tmp, repo)
    }

    /// Write `files` into `repo`, commit, and return the new sha.
    fn commit(repo: &Path, files: &[(&str, &str)]) -> String {
        for (path, body) in files {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "c"]);
        git(repo, &["rev-parse", "HEAD"])
    }

    #[test]
    fn resolver_docs_only_pr_carries_a_docs_advance() {
        let (_tmp, repo) = base_repo();
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        let reviewed = commit(&repo, &[("docs/x.md", "one\n")]);
        let head = commit(&repo, &[("docs/x.md", "one\ntwo\n")]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert_eq!(resolver.freshness(&reviewed), Freshness::CarriedDocsOnly);
    }

    #[test]
    fn resolver_docs_only_pr_carries_across_a_rebase() {
        let (_tmp, repo) = base_repo();
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        let reviewed = commit(&repo, &[("docs/x.md", "one\n")]);
        git(&repo, &["checkout", "-q", "main"]);
        commit(&repo, &[("other.txt", "base moved\n")]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&repo, &["checkout", "-q", "feature"]);
        git(&repo, &["rebase", "-q", "origin/main"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        let verdict = resolver.freshness(&reviewed);
        assert!(
            verdict.counts(),
            "a docs-only rebase must carry: {verdict:?}"
        );
    }

    #[test]
    fn resolver_docs_only_review_does_not_carry_new_code() {
        let (_tmp, repo) = base_repo();
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        let reviewed = commit(&repo, &[("docs/x.md", "one\n")]);
        let head = commit(&repo, &[("code.txt", "new code\n")]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert_eq!(resolver.freshness(&reviewed), Freshness::Stale);
    }

    #[test]
    fn resolver_code_review_does_not_carry_a_revert_to_docs_only() {
        let (_tmp, repo) = base_repo();
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        let reviewed = commit(&repo, &[("code.txt", "code\n"), ("docs/x.md", "one\n")]);
        git(&repo, &["rm", "-q", "code.txt"]);
        git(&repo, &["commit", "-q", "-m", "drop code"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert_eq!(resolver.freshness(&reviewed), Freshness::Stale);
    }

    #[test]
    fn resolver_merged_docs_only_pr_never_matches_absence() {
        // Both commits already sit in origin/main, so both three-dot diffs are
        // empty: the merged-PR absence, which must not carry.
        let (_tmp, repo) = base_repo();
        let reviewed = commit(&repo, &[("docs/x.md", "one\n")]);
        let head = commit(&repo, &[("docs/x.md", "one\ntwo\n")]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert_eq!(resolver.freshness(&reviewed), Freshness::Stale);
    }

    #[test]
    fn empty_code_identity_on_one_side_never_carries() {
        let docs_only = || Some(ident_of(&[]));
        let code = || Some(ident_of(&[":100644 100644 a b M\tcode.txt"]));
        for (reviewed_identity, head_identity) in [(docs_only(), code()), (code(), docs_only())] {
            let verdict = review_freshness(
                "r",
                "h",
                &FreshnessFacts {
                    reviewed_identity,
                    head_identity,
                    tree_paths: Some(vec!["code.txt".to_string()]),
                    interdiff_lines: Some(1),
                    carry_interdiff_lines: 100,
                },
            );
            assert_eq!(verdict, Freshness::Stale);
        }
    }

    #[test]
    fn resolver_rebase_of_different_hunks_carries_zero_interdiff() {
        // The measured defect shape: the PR and main edit one file in
        // different hunks, so the rebase rewrites both blob shas (the raw
        // identity moves) while the patch content is unchanged. The rule
        // carries at interdiff zero; this pins it so a later change cannot
        // break it quietly.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        let body = (1..=200).map(|i| format!("line{i}\n")).collect::<String>();
        std::fs::write(repo.join("f.txt"), &body).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            repo.join("f.txt"),
            format!("pr-top\n{}", &body["line1\n".len()..]),
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "pr"]);
        let reviewed = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(
            repo.join("f.txt"),
            format!("{}main-last\n", &body[..body.len() - "line200\n".len()]),
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base moved"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&repo, &["checkout", "-q", "feature"]);
        git(&repo, &["rebase", "-q", "origin/main"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert_eq!(
            resolver.freshness(&reviewed),
            Freshness::CarriedInterdiff { lines: 0, cap: 100 }
        );
        assert!(resolver.unmeasured().is_empty());
    }

    #[test]
    fn resolver_fetches_commits_that_exist_only_on_origin() {
        // The production defect: a server-side rebase publishes the new head
        // on GitHub before any local fetch ran, so the machine's next
        // coverage read found no commit and stored a false stale. The
        // resolver fetches what it is missing before it measures.
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("origin.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare"]);
        git(&bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let writer = tmp.path().join("w");
        git(tmp.path(), &["clone", "-q", bare.to_str().unwrap(), "w"]);
        git(&writer, &["config", "user.email", "t@t"]);
        git(&writer, &["config", "user.name", "t"]);
        std::fs::write(writer.join("f.txt"), "base\n").unwrap();
        git(&writer, &["add", "-A"]);
        git(&writer, &["commit", "-q", "-m", "base"]);
        git(&writer, &["push", "--no-verify", "-q", "origin", "main"]);
        // The measuring repo fetched only the old base; the reviewed commit
        // and the rebased head exist only on origin.
        let repo = tmp.path().join("r");
        git(tmp.path(), &["clone", "-q", bare.to_str().unwrap(), "r"]);
        git(&writer, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(writer.join("f.txt"), "pr change\n").unwrap();
        git(&writer, &["add", "-A"]);
        git(&writer, &["commit", "-q", "-m", "pr"]);
        let reviewed = git(&writer, &["rev-parse", "HEAD"]);
        // The reviewed sha rides its own ref: a push sends reachable objects
        // only, and the amend below orphans it.
        git(
            &writer,
            &[
                "push",
                "--no-verify",
                "-q",
                "origin",
                "HEAD:refs/heads/reviewed",
            ],
        );
        git(&writer, &["commit", "-q", "--amend", "-m", "pr rebased"]);
        let head = git(&writer, &["rev-parse", "HEAD"]);
        git(&writer, &["push", "--no-verify", "-q", "origin", "feature"]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        let verdict = resolver.freshness(&reviewed);
        assert!(
            matches!(verdict, Freshness::CarriedBaseSync),
            "both commits fetchable, identical trees: must carry, got {verdict:?}"
        );
        assert!(resolver.unmeasured().is_empty());
    }

    #[test]
    fn resolver_unfetchable_commit_reads_stale_and_is_recorded_once() {
        // A head neither local nor on origin is not evidence of anything: the
        // verdict is stale and the sha is named, so the caller demotes the
        // row instead of storing a false no. The fetch runs at most once per
        // sha: after the push below a re-probe WOULD carry, and the verdict
        // over a fresh reviewed commit staying stale is the memo's proof.
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("origin.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare"]);
        git(&bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let writer = tmp.path().join("w");
        git(tmp.path(), &["clone", "-q", bare.to_str().unwrap(), "w"]);
        git(&writer, &["config", "user.email", "t@t"]);
        git(&writer, &["config", "user.name", "t"]);
        std::fs::write(writer.join("f.txt"), "base\n").unwrap();
        git(&writer, &["add", "-A"]);
        git(&writer, &["commit", "-q", "-m", "base"]);
        git(&writer, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(writer.join("f.txt"), "rev2\n").unwrap();
        git(&writer, &["add", "-A"]);
        git(&writer, &["commit", "-q", "-m", "rev2"]);
        let reviewed = git(&writer, &["rev-parse", "HEAD"]);
        git(
            &writer,
            &["push", "--no-verify", "-q", "origin", "main", "feature"],
        );
        let repo = tmp.path().join("r");
        git(tmp.path(), &["clone", "-q", bare.to_str().unwrap(), "r"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        // The head: same tree as reviewed, a different sha, made in the
        // writer clone and never pushed anywhere.
        git(&writer, &["commit", "-q", "--amend", "-m", "head2"]);
        let head = git(&writer, &["rev-parse", "HEAD"]);
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        assert_eq!(resolver.freshness(&reviewed), Freshness::Stale);
        assert_eq!(resolver.unmeasured(), vec![head.clone()]);
        // A second, uncached reviewed commit re-asks about the same head.
        // Origin gains the head first; only the memoized probe keeps the
        // verdict stale (a real fetch would read CarriedInterdiff).
        std::fs::write(repo.join("f.txt"), "rev-r\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "rev-r"]);
        let reviewed2 = git(&repo, &["rev-parse", "HEAD"]);
        git(
            &writer,
            &[
                "push",
                "--no-verify",
                "-q",
                "origin",
                "HEAD:refs/heads/rebased",
            ],
        );
        assert_eq!(resolver.freshness(&reviewed2), Freshness::Stale);
        assert_eq!(resolver.unmeasured(), vec![head.clone()]);
    }
    fn facts3(reviewed: Option<&str>, head: Option<&str>, tree: Option<&[&str]>) -> FreshnessFacts {
        FreshnessFacts {
            reviewed_identity: reviewed.map(|h| ident_of(&[h])),
            head_identity: head.map(|h| ident_of(&[h])),
            tree_paths: tree.map(|p| p.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    // ── the rule and the resolver against REAL git history (moved verbatim
    // from loopcheck's tests, which read this module's types) ────────────
    //
    // The pure tests pin the predicate over synthetic facts (facts3 is the
    // moved name for loopcheck's three-arg helper); the git-repo tests drive
    // identity computation, base-ref qualification, and the rebase plumbing
    // for real. The Python merge gate's twin pair lives in
    // cli/tests/unit/test_review_freshness_rebase.py; the two gates must
    // agree on the same PR shape.

    fn write(repo: &Path, name: &str, body: &str) {
        std::fs::write(repo.join(name), body).unwrap();
    }

    /// One repo whose feature branch rebases onto a moved `origin/main`.
    /// Returns `(repo, reviewed_sha, head_sha)`. `conflict` selects whether
    /// the rebase stops on a conflict that the resolution CHANGES.
    fn rebased_repo(conflict: bool) -> (tempfile::TempDir, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        write(&repo, "f.txt", "base\n");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);

        git(&repo, &["checkout", "-q", "-b", "feature"]);
        if conflict {
            write(&repo, "f.txt", "feature says B\n");
        } else {
            write(&repo, "code.txt", "pr change\n");
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "pr"]);
        let reviewed = git(&repo, &["rev-parse", "HEAD"]);

        git(&repo, &["checkout", "-q", "main"]);
        if conflict {
            write(&repo, "f.txt", "main says C\n");
        } else {
            write(&repo, "other.txt", "base moved\n");
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base moved"]);
        let tip = git(&repo, &["rev-parse", "HEAD"]);
        // The machine's pre-push hook protects even scratch `main`s, so move
        // the remote-tracking ref directly.
        git(&repo, &["update-ref", "refs/remotes/origin/main", &tip]);

        git(&repo, &["checkout", "-q", "feature"]);
        let rebase = std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "rebase", "origin/main"])
            .current_dir(&repo)
            .output()
            .unwrap();
        if conflict {
            assert!(!rebase.status.success(), "scenario requires a conflict");
            write(&repo, "f.txt", "resolved differently\n");
            git(&repo, &["add", "-A"]);
            let cont = std::process::Command::new("git")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.editor=true",
                    "rebase",
                    "--continue",
                ])
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                cont.status.success(),
                "{}",
                String::from_utf8_lossy(&cont.stderr)
            );
        } else {
            assert!(
                rebase.status.success(),
                "{}",
                String::from_utf8_lossy(&rebase.stderr)
            );
        }
        let head = git(&repo, &["rev-parse", "HEAD"]);
        (tmp, reviewed, head)
    }

    #[test]
    fn resolver_carries_an_identical_rebase_and_a_small_conflict() {
        // The contract on the resolver itself, as amended by the
        // interdiff arm (law d-608344c1): a rebase that rewrote every commit
        // but changed no content keeps the attestation (CarriedBaseSync), and
        // a tiny conflict resolution carries as CarriedInterdiff with the
        // measured line count. The expiry boundary itself (>= cap stales) is
        // the pure tests' in review_freshness; a fixture cannot hit it
        // without a 100-line conflict edit.
        let (tmp, reviewed, head) = rebased_repo(false);
        let repo = tmp.path().join("r");
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        let verdict = resolver.freshness(&reviewed);
        assert!(verdict.counts(), "identical rebase must carry: {verdict:?}");

        let (tmp, reviewed, head) = rebased_repo(true);
        let repo = tmp.path().join("r");
        let resolver = FreshnessResolver::new("git", &repo, "main", &head, 100);
        let verdict = resolver.freshness(&reviewed);
        assert_eq!(
            verdict,
            Freshness::CarriedInterdiff { lines: 4, cap: 100 },
            "a 4-line conflict resolution must carry"
        );
    }

    #[test]
    fn freshness_base_sync_carries() {
        // PR 829's specimen: a 153-file rebase whose PR code diff is identical.
        assert_eq!(
            review_freshness(
                "3f64bc31",
                "83d2b4ce",
                &facts3(
                    Some("ident-a"),
                    Some("ident-a"),
                    Some(&["crates/fno/src/lib.rs"])
                )
            ),
            Freshness::CarriedBaseSync
        );
    }

    #[test]
    fn freshness_identical_trees_carry_as_base_sync() {
        // An empty tree diff must not fall through the "all paths are docs"
        // branch, which is vacuously true over an empty list.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts3(Some("i"), Some("i"), Some(&[]))),
            Freshness::CarriedBaseSync
        );
    }

    #[test]
    fn freshness_docs_only_carries_with_its_reason() {
        // PR 830's specimen: one documentation file moved the head.
        assert_eq!(
            review_freshness(
                "e2976abc",
                "1ef60959",
                &facts3(
                    Some("i"),
                    Some("i"),
                    Some(&["docs/architecture/x.md", "README.md"])
                )
            ),
            Freshness::CarriedDocsOnly
        );
    }

    #[test]
    fn freshness_code_change_dies() {
        // 20 of the 22 measured transitions are this: genuine code change, and
        // no rule that refuses to guess can absorb them.
        assert_eq!(
            review_freshness(
                "aaa",
                "bbb",
                &facts3(Some("i-old"), Some("i-new"), Some(&["a.rs"]))
            ),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_missing_identity_dies() {
        // Git failure on either side: fail closed, re-review.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts3(None, Some("i"), Some(&[]))),
            Freshness::Stale
        );
        assert_eq!(
            review_freshness("aaa", "bbb", &facts3(Some("i"), None, Some(&[]))),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_two_absent_identities_never_match() {
        // THE regression guard. A first measurement pass reported 63%
        // carry-forward and was wrong: merged PRs' three-dot diff against
        // current origin/main is empty, e3b0c442 is the SHA-256 of the empty
        // string, and twelve transitions matched absence against absence. The
        // true figure was 2 of 22. `Carried` requires two Some values that are
        // equal - never two empties, however they arose.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts3(None, None, Some(&[]))),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_absent_reviewed_sha_dies() {
        // A github_app review object with no `commit.oid`, or an attestation
        // with no head_sha. An empty sha must never match an empty head.
        assert_eq!(
            review_freshness("", "", &facts3(Some("i"), Some("i"), Some(&[]))),
            Freshness::Stale
        );
        assert_eq!(
            review_freshness("", "bbb", &facts3(Some("i"), Some("i"), Some(&[]))),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_unreadable_tree_diff_dies() {
        // Matching identities but no way to name the carry reason: a carry that
        // cannot say why it carried is not auditable.
        assert_eq!(
            review_freshness("aaa", "bbb", &facts3(Some("i"), Some("i"), None)),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_only_stale_stops_counting() {
        assert!(Freshness::Fresh.counts());
        assert!(Freshness::CarriedBaseSync.counts());
        assert!(Freshness::CarriedDocsOnly.counts());
        assert!(!Freshness::Stale.counts());
    }

    #[test]
    fn code_diff_identity_drops_docs_and_is_none_when_only_docs_changed() {
        // The identity is computed from `git diff --raw` lines, so exercise the
        // path classifier and the empty-result rule on that exact shape.
        let code = ":100644 100644 aaa bbb M\tcrates/fno/src/lib.rs";
        let docs = ":100644 100644 ccc ddd M\tdocs/architecture/review-lanes.md";
        assert_eq!(raw_diff_line_path(code), "crates/fno/src/lib.rs");
        assert!(!is_documentation_path(raw_diff_line_path(code)));
        assert!(is_documentation_path(raw_diff_line_path(docs)));
    }

    #[test]
    fn freshness_resolver_qualifies_a_bare_base_ref() {
        // `gh pr view` returns `main`, not `origin/main`; a bare branch name
        // resolves to the local ref, which in a stale worktree is not the base.
        let cwd = std::env::temp_dir();
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "main", "abc", 100).base_ref,
            "origin/main"
        );
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "origin/release", "abc", 100).base_ref,
            "origin/release"
        );
        // A slash in the name is not remote-qualification: `release/2.0` is a
        // bare branch and must still be qualified, or the identity resolves
        // against a local ref the worktree may not have.
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "release/2.0", "abc", 100).base_ref,
            "origin/release/2.0"
        );
        assert_eq!(
            FreshnessResolver::new("git", &cwd, "", "abc", 100).base_ref,
            "origin/main"
        );
    }
}
