//! Review freshness: one predicate, both producers (x-5b99 / x-62a1).
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

/// Strict subset over two SORTED raw-diff line sets: non-empty (an empty HEAD
/// identity is `None`, never an empty set, per the absence-matching rule
/// above), strictly smaller, and every HEAD line present in the reviewed set.
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
/// `None` on any git failure AND when nothing outside documentation changed.
/// An empty code diff is not positive evidence of anything, and letting two of
/// them compare equal is the absence-matched-against-absence trap above. The
/// cost is that a documentation-only PR never carries an attestation, which is
/// the fail-closed direction and matches today's behavior exactly.
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
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty() && !is_documentation_path(raw_diff_line_path(l)))
        .collect();
    if lines.is_empty() {
        return None;
    }
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
        }
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
        let reviewed_identity =
            pr_code_diff_identity(self.git_bin, self.cwd, &self.base_ref, reviewed_sha);
        let head_identity = self.head_identity();
        // The interdiff read costs a full patch fetch per side, so it runs only
        // when the arm can possibly fire: two present, UNEQUAL identities. The
        // fresh and carried-identity paths - the common cases - pay no extra
        // git.
        let interdiff_lines = match (&reviewed_identity, &head_identity) {
            (Some(r), Some(h)) if r.hash != h.hash => interdiff_lines_between(
                self.git_bin,
                self.cwd,
                &self.base_ref,
                reviewed_sha,
                &self.head_sha,
            ),
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
    fn multiset_difference_counts_duplicates() {
        let a = vec!["x".to_string(), "x".to_string(), "y".to_string()];
        let b = vec!["x".to_string()];
        assert_eq!(multiset_symmetric_difference(&a, &b), 2);
        assert_eq!(multiset_symmetric_difference(&[], &[]), 0);
    }

    // ── the resolver against REAL git history ───────────────────────────────

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
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
        // The pre-existing x-e8db contract on the moved code: a rebase that
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
}
