//! Is removing this worktree safe? The single answer, for every caller.
//!
//! A worktree blocks removal only when it holds content that removal would
//! DESTROY. A tracked file missing from disk is not that: HEAD holds its
//! content, so `git worktree remove` loses nothing and `git restore` brings
//! it back.
//!
//! Ported from the deleted `cli/src/fno/worktree_reapable.py` (this port): the
//! Python leg had become the only remaining new-code surface asking this
//! question, and `daemon.rs` already carried a hand copy of its
//! `branch_merged`. This module is now the one implementation; the Python
//! typer leaf and `scripts/lib/worktree-reapable.sh` exec the binary, and
//! `daemon.rs` calls the module in-process.
//!
//! Receipt grammar, unchanged and load-bearing: callers parse the one line
//! `reapable=<yes|no> reason=<r> recoverable_deletions=<n> discounted=<n>`
//! with an optional trailing ` detail=<d>` that is LAST because a path may
//! contain spaces. The `--done-node` arm (only when the flag is passed)
//! appends ` evidence=<e> untracked=<n> detached=<yes|no>` before `detail`,
//! still leaving `detail` the remainder of the line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::bounded_cmd::output_with_timeout;

/// A tree git created minutes ago is not a finished tree. Measured
/// 2026-09-12: a worktree on a new branch off origin/main reads
/// `reapable=yes reason=clean` before its first commit, because a
/// zero-commit branch is a literal ancestor of main. 29 removals in one
/// night were that read, three of them live.
pub(crate) const SETUP_WINDOW_SECS: u64 = 1800;

/// The done-node grace: a tree younger than 48 hours stays, whatever its
/// node reads. The crown's hand pass kept every tree under two days.
pub(crate) const DONE_GRACE_SECS: u64 = 48 * 3600;

/// Per-subprocess budget, matching the Python gate's remove bound.
const GIT_TIMEOUT_SECS: u64 = 30;

/// Unmerged (conflict) codes, per `git status` docs. These matter because two
/// of them carry only `D` and `A` letters: reading `DD` ("both deleted") as
/// two recoverable deletions throws away a merge the user has not resolved
/// yet.
const UNMERGED: [&str; 7] = ["DD", "AU", "UD", "UA", "DU", "AA", "UU"];

/// What `setup-worktree.sh` links, canonical-relative: these five roots, and
/// anything one level under `.claude/`. A shape, not a list: the script's
/// grows.
const SETUP_LINK_ROOTS: [&str; 5] = ["internal", ".agents", ".codex", ".codex-plugin", ".gemini"];

/// One worktree's answer, plus the evidence a caller may want to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) reapable: bool,
    pub(crate) reason: String,
    pub(crate) detail: String,
    pub(crate) recoverable_deletions: u32,
    pub(crate) discounted: Vec<String>,
    /// done-node arm only: `node:<ids>` or `merged`.
    pub(crate) evidence: Option<String>,
    /// done-node arm only: non-discounted `??` paths, the salvage set's size.
    pub(crate) untracked: u32,
    /// done-node arm only: no branch name, so a salvage pass must pin HEAD.
    pub(crate) detached: bool,
}

impl Verdict {
    fn block(reason: &str, detail: impl Into<String>) -> Self {
        Self {
            reapable: false,
            reason: reason.to_string(),
            detail: detail.into(),
            recoverable_deletions: 0,
            discounted: Vec::new(),
            evidence: None,
            untracked: 0,
            detached: false,
        }
    }

    /// The one-line receipt the bash and Rust callers parse. `detail` is last
    /// because a path may contain spaces; a caller reading fields left to
    /// right gets every fixed field intact and may take the remainder as the
    /// detail.
    pub(crate) fn line(&self) -> String {
        let mut head = format!(
            "reapable={} reason={} recoverable_deletions={} discounted={}",
            if self.reapable { "yes" } else { "no" },
            self.reason,
            self.recoverable_deletions,
            self.discounted.len()
        );
        if self.reason == "done-node" {
            head += &format!(
                " evidence={} untracked={} detached={}",
                self.evidence.as_deref().unwrap_or(""),
                self.untracked,
                if self.detached { "yes" } else { "no" }
            );
        }
        if !self.detail.is_empty() {
            head += &format!(" detail={}", self.detail);
        }
        head
    }
}

/// The path from a porcelain line, minus the two status chars and a space.
fn path_of(entry: &str) -> &str {
    if entry.len() > 3 {
        entry[3..].trim()
    } else {
        entry.trim()
    }
}

fn git_out(cwd: &Path, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(args);
    output_with_timeout(cmd, GIT_TIMEOUT_SECS)
}

/// The main checkout this worktree links back to, or None if unresolvable.
fn canonical_root(worktree: &Path) -> Option<PathBuf> {
    let out = git_out(worktree, &["rev-parse", "--git-common-dir"])?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut common = PathBuf::from(text);
    if !common.is_absolute() {
        common = worktree.join(common);
    }
    common.parent().map(Path::to_path_buf)
}

/// Does a link at `here` sitting over target `rel` read as setup's?
///
/// Setup writes each link at the same relative path as its target, one
/// directory deeper at most, so the two must agree. Without that, a
/// hand-made `vault -> $CANONICAL/internal` reaps under a receipt claiming
/// setup wrote it. The boundary is a whole segment: `x.agents` is not
/// `.agents`.
fn accepts(here: &str, rel: &str) -> bool {
    let (parent, name) = match rel.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", rel),
    };
    if name.is_empty() || !(SETUP_LINK_ROOTS.contains(&rel) || parent == ".claude") {
        return false;
    }
    here == rel || here.ends_with(&format!("/{rel}"))
}

/// Did setup-worktree.sh write this symlink?
///
/// Read the link ONE hop first: setup writes an absolute `$CANONICAL/$rel`,
/// so the raw target IS the attribution, and resolving follows `internal`
/// (itself a symlink) out of the checkout. The realpath pairs are the
/// fallback for a canonical reached by a different spelling (`/tmp` vs
/// `/private`).
fn is_setup_link(link: &Path, worktree: &Path, canonical: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    let Ok(relative) = link.strip_prefix(worktree) else {
        return false;
    };
    let here = relative.to_string_lossy().replace('\\', "/");
    if !target.is_absolute() {
        return false;
    }
    let real_canonical =
        std::fs::canonicalize(canonical).unwrap_or_else(|_| canonical.to_path_buf());
    let real_target = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    for base in [canonical, &real_canonical] {
        for candidate in [&target, &real_target] {
            let Ok(rel) = candidate.strip_prefix(base) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !rel.starts_with("..") && accepts(&here, &rel) {
                return true;
            }
        }
    }
    false
}

/// A linked worktree's `.git` is a FILE pointing at its admin dir.
///
/// A main checkout's `.git` is a directory and a plain directory has none,
/// so both read false: only a linked leaf owns something `git worktree
/// remove` could take.
pub(crate) fn is_linked_worktree(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    Path::new(path).join(".git").is_file()
}

/// Was this worktree's `.git` file written within the setup window?
///
/// Git writes the `.git` file once at `worktree add` and does not rewrite it
/// in normal use, so its mtime is the tree's creation time. A stat that
/// fails reads as inside the window: an unanswerable probe never shortens
/// the keep.
fn inside_setup_window(path: &Path) -> bool {
    git_file_age_secs(path).map_or(true, |age| age < SETUP_WINDOW_SECS)
}

/// Seconds since the worktree's `.git` file was written; None on any stat
/// failure (the caller fails closed).
fn git_file_age_secs(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path.join(".git")).ok()?;
    let modified = meta.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// Has this worktree's branch never moved since `git worktree add` made it?
///
/// The reflog is the discriminator the merge status cannot supply: creation
/// writes one entry, any commit, reset or rebase writes more. Read alone it
/// would also hold a landed branch whose reflog has expired, so the caller
/// pairs it with the tree's age. A detached HEAD answers false: content, not
/// a branch name, judges those. Any git read that fails answers true - a
/// probe that cannot answer must not authorize a removal.
pub(crate) fn branch_unborn(path: &Path) -> bool {
    let Some(branch) = git_out(path, &["branch", "--show-current"]) else {
        return true;
    };
    if !branch.status.success() {
        return true;
    }
    let name = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    if name.is_empty() {
        return false;
    }
    let Some(log) = git_out(path, &["reflog", "show", &name, "--format=%gs"]) else {
        return true;
    };
    if !log.status.success() {
        return true;
    }
    let text = String::from_utf8_lossy(&log.stdout);
    text.lines().filter(|l| !l.trim().is_empty()).count() <= 1
}

/// The worktree's current branch name, or None when detached (or unreadable,
/// which every caller treats as detached: content judges those).
fn branch_show_current(path: &Path) -> Option<String> {
    let out = git_out(path, &["branch", "--show-current"])?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Is the worktree's branch merged into the repo's main line?
///
/// The worktree contract's third bucket: content-clean is not enough for an
/// automatic prune, because a clean-and-unmerged branch is exactly where
/// abandoned-but-real work lives, and a human judges that (the `--merged`
/// sweep merge-filters BEFORE asking the gate; a caller without that
/// pre-filter must ask here). `None`: nothing names the work or the main
/// line - detached HEAD, no main ref, git error - and the caller keeps the
/// tree. Moved here from `daemon.rs`, where it was a hand copy of the
/// Python gate's same question.
pub(crate) fn branch_merged(cwd: &str) -> Option<bool> {
    let mut bases = vec!["origin/main".to_string(), "main".to_string()];
    if let Some(out) = git_out(
        Path::new(cwd),
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !head.is_empty() {
            bases.insert(0, head);
        }
    }
    let mut base: Option<String> = None;
    for candidate in &bases {
        let known = git_out(
            Path::new(cwd),
            &["rev-parse", "--verify", "--quiet", candidate],
        );
        if known.is_some_and(|out| out.status.success()) {
            base = Some(candidate.clone());
            break;
        }
    }
    let base = base?;
    let branch = branch_show_current(Path::new(cwd))?;
    let merged = git_out(
        Path::new(cwd),
        &["merge-base", "--is-ancestor", &branch, &base],
    )?;
    match merged.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Classify `git status --porcelain` output. Pure: no clock, no disk.
///
/// Blocking, in precedence order: an unmerged conflict, untracked content,
/// then any staged or unstaged modification of tracked content. Everything
/// else is a deletion of a tracked file, which is recoverable from HEAD.
///
/// `discount` names untracked paths carrying no human work. It is asked
/// about `??` lines only, so tracked and unmerged dirt block as before;
/// omit it and this answers exactly what it always answered.
pub(crate) fn classify(porcelain: &str, discount: Option<&dyn Fn(&str) -> bool>) -> Verdict {
    let mut deletions: u32 = 0;
    let mut discounted: Vec<String> = Vec::new();
    for raw in porcelain.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        if raw.len() < 2 {
            continue;
        }
        let code = &raw[..2];
        if UNMERGED.contains(&code) {
            return Verdict::block("unmerged", path_of(raw));
        }
        if code == "??" {
            let path = path_of(raw).to_string();
            if discount.is_some_and(|d| d(&path)) {
                discounted.push(path);
                continue;
            }
            return Verdict::block("untracked", path);
        }
        let letters: String = code.chars().filter(|c| *c != ' ').collect();
        if !letters.is_empty() && letters.chars().all(|c| c == 'D') {
            deletions += 1;
            continue;
        }
        return Verdict::block("modified-tracked", path_of(raw));
    }
    if !discounted.is_empty() {
        let detail = discounted.join(", ");
        return Verdict {
            reapable: true,
            reason: "setup-links".to_string(),
            detail,
            recoverable_deletions: deletions,
            discounted,
            evidence: None,
            untracked: 0,
            detached: false,
        };
    }
    Verdict {
        reapable: true,
        reason: "clean".to_string(),
        detail: String::new(),
        recoverable_deletions: deletions,
        discounted,
        evidence: None,
        untracked: 0,
        detached: false,
    }
}

/// Node-id candidates as delimiter-bounded segments of a branch name or
/// directory basename: the `(?:^|[/-])(<prefix>-<hex>)(?=$|[/-])` rule
/// `cli/src/fno/pr/closure.py` scans with, replicated without a regex
/// dependency. Non-overlapping left-to-right: on "feature/x-cccc-1234" the
/// scan consumes "x-cccc" and resumes at "-1234", which is not letter-led,
/// so the bogus tail candidate is never produced.
pub(crate) fn scan_node_tokens(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let bounded_start = i == 0 || b[i - 1] == b'/' || b[i - 1] == b'-';
        if bounded_start {
            if let Some((len, cand)) = try_node_id(&b[i..]) {
                if !out.contains(&cand) {
                    out.push(cand);
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Match `[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}` at the slice start, greedily
/// with backtracking, returning (consumed bytes, candidate). The match must
/// be delimiter-bounded on the right: end of string, `/` or `-`.
fn try_node_id(b: &[u8]) -> Option<(usize, String)> {
    if b.is_empty() || !b[0].is_ascii_lowercase() {
        return None;
    }
    let mut run = 1usize;
    while run < 8 && run < b.len() && (b[run].is_ascii_lowercase() || b[run].is_ascii_digit()) {
        run += 1;
    }
    for plen in (1..=run).rev() {
        if plen >= b.len() || b[plen] != b'-' {
            continue;
        }
        let hex_start = plen + 1;
        let mut hex_run = 0usize;
        while hex_run < 8
            && hex_start + hex_run < b.len()
            && b[hex_start + hex_run].is_ascii_hexdigit()
            && !b[hex_start + hex_run].is_ascii_uppercase()
        {
            hex_run += 1;
        }
        for hlen in (4..=hex_run).rev() {
            let end = hex_start + hlen;
            if end < b.len() && b[end] != b'/' && b[end] != b'-' {
                continue;
            }
            return Some((end, String::from_utf8_lossy(&b[..end]).to_string()));
        }
    }
    None
}

/// External truth the done-node arm reads, injected so unit tests can feed
/// fixtures instead of building a graph store and claims files.
pub(crate) struct DoneNodeReaders<'a> {
    /// Working graph plus archive rows, or None when the store cannot be
    /// read (every node then reads unknown -> fail closed).
    pub(crate) read_rows: &'a dyn Fn() -> Option<Vec<serde_json::Value>>,
    /// Does a live or suspect `node:<id>` claim exist?
    pub(crate) claim_live: &'a dyn Fn(&str) -> bool,
}

/// Production readers: the graph store the sweep already reads (working
/// graph first, archive appended advisory) and the global claims root.
pub(crate) fn production_readers() -> DoneNodeReaders<'static> {
    DoneNodeReaders {
        read_rows: &|| {
            let home = crate::paths::AgentsHome::from_env_opt()?;
            crate::gc_sweep::read_graph_rows(&home)
        },
        claim_live: &|id: &str| {
            matches!(
                crate::claims::status(&format!("node:{id}"), None).0,
                crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
            )
        },
    }
}

/// The done-node arm: with the flag, a tree whose node reads done or
/// superseded (or, node-less, whose branch is merged) may be removed with
/// its branch kept, untracked files salvaged by the caller first. Only
/// `--done-node` reaches here, so every other receipt is unchanged.
fn done_node_arm(
    target: &Path,
    porcelain: &str,
    base: Verdict,
    discount: &dyn Fn(&str) -> bool,
    readers: &DoneNodeReaders,
) -> Verdict {
    // Content removal would destroy stays blocking, unchanged: modified
    // tracked files (a tree holding real uncommitted work), conflicts, a
    // worker mid-setup, and
    // an unanswerable probe.
    if matches!(
        base.reason.as_str(),
        "modified-tracked" | "unmerged" | "unborn" | "probe-failed"
    ) {
        return base;
    }
    let untracked = porcelain
        .lines()
        .filter(|l| l.len() >= 2 && &l[..2] == "??")
        .filter(|l| !discount(path_of(l)))
        .count() as u32;
    let detached = {
        let b = branch_show_current(target);
        b.as_ref().map_or(true, |n| n.is_empty())
    };
    let branch = branch_show_current(target);

    // Resolve the tree's nodes: manifest first (target-minted, exact), then
    // node-id tokens in the branch name, then in the directory basename.
    let ids = resolve_node_ids(target, branch.as_deref());
    let evidence;
    if !ids.is_empty() {
        let rows = (readers.read_rows)().unwrap_or_default();
        // Working-graph rows come before archive rows; first writer wins so
        // the live store outranks its own archive.
        let mut status: HashMap<String, String> = HashMap::new();
        for row in &rows {
            if let Some(id) = crate::graph_store::entry_id(row) {
                status.entry(id.to_string()).or_insert_with(|| {
                    row.get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                });
            }
        }
        for id in &ids {
            match status.get(id) {
                None => {
                    return Verdict::block("node-unknown", format!("no store knows {id}"));
                }
                Some(s) if s == "done" || s == "superseded" => {}
                // open or blocked still owns its tree (blocked treated as open)
                Some(_) => return base,
            }
        }
        for id in &ids {
            if (readers.claim_live)(id) {
                return Verdict::block("claim-live", format!("node:{id} holds a live claim"));
            }
        }
        // The grace window on the NODE path: a tree younger than 48 hours
        // stays whatever its node reads (the crown kept every tree under two
        // days). An unreadable mtime reads as young.
        match git_file_age_secs(target) {
            Some(age) if age >= DONE_GRACE_SECS => {}
            _ => return Verdict::block("done-grace", "tree is younger than the 48h grace"),
        }
        evidence = format!("node:{}", ids.join(","));
    } else if branch_merged(&target.to_string_lossy()) == Some(true) {
        if base.reapable {
            // A CLEAN tree whose branch is merged was ALREADY removable: the
            // sweep's step-2 merged filter archives it today, arm or no arm.
            // Returning the base verdict keeps that authority untouched
            // (never shrink an existing grant); the arm's grace window does
            // not apply here because it grants nothing new.
            return base;
        }
        // New authority: untracked content beside a merged branch, salvaged
        // by the caller first. The grace window protects the fresh ones.
        match git_file_age_secs(target) {
            Some(age) if age >= DONE_GRACE_SECS => {}
            _ => return Verdict::block("done-grace", "tree is younger than the 48h grace"),
        }
        evidence = "merged".to_string();
    } else {
        return base;
    }

    Verdict {
        reapable: true,
        reason: "done-node".to_string(),
        detail: base.detail,
        recoverable_deletions: base.recoverable_deletions,
        discounted: base.discounted,
        evidence: Some(evidence),
        untracked,
        detached,
    }
}

/// The tree's node ids: `graph_node_id` from the target manifest, else every
/// node-id token in the branch name, else in the directory basename.
fn resolve_node_ids(target: &Path, branch: Option<&str>) -> Vec<String> {
    if let Ok(content) = std::fs::read_to_string(target.join(".fno").join("target-state.md")) {
        let fields = crate::finalize::parse_manifest_fields(&content);
        if let Some(id) = fields.graph_node_id {
            let id = id.trim().trim_matches('"').to_string();
            if !id.is_empty() {
                return vec![id];
            }
        }
    }
    if let Some(b) = branch {
        if !b.is_empty() {
            let v = scan_node_tokens(b);
            if !v.is_empty() {
                return v;
            }
        }
    }
    target
        .file_name()
        .map(|n| scan_node_tokens(&n.to_string_lossy()))
        .unwrap_or_default()
}

/// Classify a worktree on disk. Fails CLOSED on any probe it cannot trust.
///
/// A probe that cannot answer must not read as "safe to remove": an absence
/// of reported dirt has two explanations, and only one of them is a clean
/// tree.
///
/// `allow_unborn` lifts the setup-window refusal for one tree a human NAMED
/// - the orphan-recovery path. `done_node` turns on the done-node arm;
/// every receipt a caller did not ask for is byte-identical to the base
/// gate.
pub(crate) fn reapable_opts(path: &str, allow_unborn: bool, done_node: bool) -> Verdict {
    reapable_opts_with(path, allow_unborn, done_node, &production_readers())
}

/// The base gate, what every caller before the port asked.
pub(crate) fn reapable(path: &str) -> Verdict {
    reapable_opts(path, false, false)
}

pub(crate) fn reapable_opts_with(
    path: &str,
    allow_unborn: bool,
    done_node: bool,
    readers: &DoneNodeReaders,
) -> Verdict {
    let target = Path::new(path);
    if !target.is_dir() {
        return Verdict::block("probe-failed", "path is not a directory");
    }
    // `-uall`, so every untracked entry is a FILE. The default collapses a
    // directory to one line, and judging that from disk asks about children
    // git does not track.
    let Some(out) = git_out(target, &["status", "--porcelain", "--untracked-files=all"]) else {
        return Verdict::block("probe-failed", "git-error: status did not run");
    };
    if !out.status.success() {
        return Verdict::block("probe-failed", "git status exited non-zero");
    }
    let porcelain = String::from_utf8_lossy(&out.stdout).to_string();

    // Resolved once, on demand: a clean tree still costs one `git status`
    // and nothing else. The discount is shared with the arm's untracked
    // count so the receipt and the salvage pass cannot disagree.
    let canonical = std::cell::OnceCell::new();
    let discount = |rel: &str| -> bool {
        let root = canonical.get_or_init(|| canonical_root(target));
        match root {
            Some(r) => is_setup_link(&target.join(rel), target, r),
            None => false,
        }
    };

    let verdict = classify(&porcelain, Some(&discount));
    // Cheapest reads first: an aged tree pays one stat and no git subprocess.
    if !allow_unborn
        && verdict.reapable
        && is_linked_worktree(path)
        && inside_setup_window(target)
        && branch_unborn(target)
    {
        return Verdict::block(
            "unborn",
            "branch has no commit of its own and the tree is inside the setup window",
        );
    }
    if done_node {
        return done_node_arm(target, &porcelain, verdict, &discount, readers);
    }
    verdict
}

/// The direct client verb: `fno-agents worktree-reapable <path>
/// [--allow-unborn] [--done-node]`. Prints the receipt line; exit 0 yes,
/// 1 no. Beside `reclaim` in `bin/client.rs`, the same daemon-free direct
/// dispatch.
pub fn run_client(args: &[String]) -> i32 {
    let mut path: Option<&String> = None;
    let mut allow_unborn = false;
    let mut done_node = false;
    for a in args {
        match a.as_str() {
            "--allow-unborn" => allow_unborn = true,
            "--done-node" => done_node = true,
            other => {
                if other.starts_with('-') {
                    eprintln!("fno-agents worktree-reapable: unknown flag {other}");
                    return 2;
                }
                if path.is_some() {
                    eprintln!("fno-agents worktree-reapable: one path only");
                    return 2;
                }
                path = Some(a);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: fno-agents worktree-reapable <path> [--allow-unborn] [--done-node]");
        return 2;
    };
    let verdict = reapable_opts(path, allow_unborn, done_node);
    println!("{}", verdict.line());
    if verdict.reapable {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    //! The corpus the deleted `test_worktree_reapable.py` pinned, plus the
    //! done-node arm's table. Real temp git repos throughout: the gate's job
    //! is to be right about what git actually prints.

    use super::*;
    use std::fs;
    use std::time::Duration;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git ran");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn seed_repo(tmp: &Path) -> PathBuf {
        let wt = tmp.join("wt");
        fs::create_dir_all(&wt).unwrap();
        git(&wt, &["init", "-q", "-b", "main"]);
        git(&wt, &["config", "user.email", "t@example.com"]);
        git(&wt, &["config", "user.name", "t"]);
        fs::write(wt.join("keep.py"), "x = 1\n").unwrap();
        fs::write(wt.join("also.py"), "y = 2\n").unwrap();
        fs::write(wt.join(".gitignore"), "ignored/\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "seed"]);
        wt
    }

    fn linked_wt(tmp: &Path, repo: &Path, name: &str, branch: &str) -> PathBuf {
        let wt = tmp.join(name);
        git(
            repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", branch],
        );
        wt
    }

    fn backdate(wt: &Path, secs: u64) {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(wt.join(".git"))
            .unwrap();
        let t = SystemTime::now()
            .checked_sub(Duration::from_secs(secs))
            .unwrap();
        f.set_times(fs::FileTimes::new().set_accessed(t).set_modified(t))
            .unwrap();
    }

    fn symlink(from: &Path, to: &Path) {
        std::os::unix::fs::symlink(to, from).unwrap();
    }

    // -- AC1-HP: deletions are recoverable -----------------------------------

    #[test]
    fn deletions_only_is_reapable_and_counts_them() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::remove_file(repo.join("keep.py")).unwrap();
        fs::remove_file(repo.join("also.py")).unwrap();

        let v = reapable(repo.to_str().unwrap());

        assert!(v.reapable);
        assert_eq!(v.reason, "clean");
        assert_eq!(v.recoverable_deletions, 2);
    }

    #[test]
    fn staged_deletion_is_also_recoverable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        git(&repo, &["rm", "-q", "keep.py"]);

        let v = reapable(repo.to_str().unwrap());

        assert!(v.reapable);
        assert_eq!(v.recoverable_deletions, 1);
    }

    #[test]
    fn clean_worktree_is_reapable_with_zero_deletions() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());

        let v = reapable(repo.to_str().unwrap());

        assert!(v.reapable);
        assert_eq!(v.reason, "clean");
        assert_eq!(v.recoverable_deletions, 0);
    }

    // -- AC1-EDGE: modified tracked content blocks ---------------------------

    #[test]
    fn modified_tracked_file_blocks_and_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::write(repo.join("keep.py"), "x = 999\n").unwrap();

        let v = reapable(repo.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "modified-tracked");
        assert!(v.detail.contains("keep.py"));
    }

    #[test]
    fn one_modification_beside_many_deletions_still_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::remove_file(repo.join("also.py")).unwrap();
        fs::write(repo.join("keep.py"), "x = 999\n").unwrap();

        let v = reapable(repo.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "modified-tracked");
    }

    #[test]
    fn staged_addition_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::write(repo.join("new.py"), "z = 3\n").unwrap();
        git(&repo, &["add", "new.py"]);

        let v = reapable(repo.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "modified-tracked");
    }

    // -- AC1-ERR: untracked non-ignored content blocks ------------------------

    #[test]
    fn untracked_file_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::write(repo.join("scratch.py"), "nope\n").unwrap();

        let v = reapable(repo.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
        assert!(v.detail.contains("scratch.py"));
    }

    #[test]
    fn ignored_file_is_invisible_and_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::create_dir(repo.join("ignored")).unwrap();
        fs::write(repo.join("ignored").join("junk.bin"), "junk\n").unwrap();

        let v = reapable(repo.to_str().unwrap());

        assert!(v.reapable);
        assert_eq!(v.reason, "clean");
    }

    // -- Conflicts are never recoverable, even when both sides deleted -------

    #[test]
    fn unmerged_codes_block_even_when_only_d_chars() {
        for code in UNMERGED {
            let v = classify(&format!("{code} conflicted.py\n"), None);
            assert!(!v.reapable, "{code} must block");
            assert_eq!(v.reason, "unmerged");
        }
    }

    // -- Probe failure fails CLOSED -------------------------------------------

    #[test]
    fn non_repo_path_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("not-a-repo");
        fs::create_dir(&plain).unwrap();

        let v = reapable(plain.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "probe-failed");
    }

    #[test]
    fn missing_path_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone");

        let v = reapable(gone.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "probe-failed");
    }

    // -- The receipt line the bash and rust callers parse ---------------------

    #[test]
    fn receipt_line_is_one_parseable_line() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::remove_file(repo.join("keep.py")).unwrap();

        let line = reapable(repo.to_str().unwrap()).line();

        assert!(line.starts_with("reapable=yes "));
        assert!(line.contains("reason=clean"));
        assert!(line.contains("recoverable_deletions=1"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn blocking_receipt_names_the_reason_and_detail() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::write(repo.join("scratch.py"), "nope\n").unwrap();

        let line = reapable(repo.to_str().unwrap()).line();

        assert!(line.starts_with("reapable=no "));
        assert!(line.contains("reason=untracked"));
        assert!(line.contains("detail=scratch.py"));
    }

    #[test]
    fn detail_never_breaks_the_line_grammar() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        fs::write(repo.join("two words.py"), "nope\n").unwrap();

        let line = reapable(repo.to_str().unwrap()).line();

        assert!(!line.contains('\n'));
        assert_eq!(line.matches("reapable=").count(), 1);
    }

    // -- Pure classify: the contract the equivalence test pins ----------------

    #[test]
    fn classify_is_pure_over_porcelain_text() {
        let v = classify(" D a.py\nD  b.py\n D c.py\n", None);
        assert!(v.reapable);
        assert_eq!(v.recoverable_deletions, 3);
    }

    #[test]
    fn classify_empty_is_clean() {
        let v = classify("", None);
        assert!(v.reapable);
        assert_eq!(v.recoverable_deletions, 0);
    }

    // -- The merge check: the rm door's half of the third bucket --------------

    fn commit_in(wt: &Path, msg: &str) {
        git(
            wt,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                msg,
            ],
        );
    }

    #[test]
    fn clean_but_unmerged_branch_blocks_the_rm_question() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "leaf", "feature");
        fs::write(wt.join("new.py"), "n = 1\n").unwrap();
        git(&wt, &["add", "-A"]);
        commit_in(&wt, "work");

        assert_eq!(branch_merged(wt.to_str().unwrap()), Some(false));
    }

    #[test]
    fn a_fast_forwarded_branch_reads_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "leaf2", "done");
        git(&wt, &["commit", "--allow-empty", "-qm", "w"]);
        git(&repo, &["merge", "-q", "done"]);

        assert_eq!(branch_merged(wt.to_str().unwrap()), Some(true));
    }

    #[test]
    fn detached_head_answers_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "leaf3", "scratch");
        git(&wt, &["checkout", "-q", "--detach"]);

        assert_eq!(branch_merged(wt.to_str().unwrap()), None);
    }

    // -- setup symlinks: footnote's own links are not dirt --------------------

    /// `seed_repo` plus the shared state setup links, and a tracked `cli/`.
    fn canonical_with_cli(tmp: &Path) -> PathBuf {
        let repo = seed_repo(tmp);
        fs::create_dir(repo.join("cli")).unwrap();
        fs::write(repo.join("cli").join("keep.py"), "x = 1\n").unwrap();
        git(&repo, &["add", "cli/keep.py"]);
        git(&repo, &["commit", "-qm", "cli"]);
        for rel in [
            ".agents",
            ".codex",
            ".codex-plugin",
            ".claude",
            ".claude/skills",
        ] {
            fs::create_dir_all(repo.join(rel)).unwrap();
        }
        fs::write(repo.join(".claude").join("settings.local.json"), "{}\n").unwrap();
        repo
    }

    /// What setup-worktree.sh leaves behind, one directory deeper.
    fn put_setup_links(wt: &Path, canonical: &Path) {
        fs::create_dir_all(wt.join("cli").join(".claude")).unwrap();
        symlink(&wt.join("cli").join(".agents"), &canonical.join(".agents"));
        symlink(&wt.join("cli").join(".codex"), &canonical.join(".codex"));
        symlink(
            &wt.join("cli").join(".codex-plugin"),
            &canonical.join(".codex-plugin"),
        );
        symlink(
            &wt.join("cli").join(".claude").join("skills"),
            &canonical.join(".claude").join("skills"),
        );
        symlink(
            &wt.join("cli").join(".claude").join("settings.local.json"),
            &canonical.join(".claude").join("settings.local.json"),
        );
    }

    #[test]
    fn setup_links_only_is_reapable_and_names_what_it_discounted() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "setup", "feature/setup");
        backdate(&wt, 7200);
        put_setup_links(&wt, &canonical);

        let v = reapable(wt.to_str().unwrap());

        assert!(v.reapable, "line was: {}", v.line());
        assert_eq!(v.reason, "setup-links");
        for named in [
            "cli/.agents",
            "cli/.claude/skills",
            "cli/.codex",
            "cli/.codex-plugin",
        ] {
            assert!(
                v.detail.contains(named),
                "{named} not named in {}",
                v.detail
            );
        }
        assert!(v
            .line()
            .contains(&format!("discounted={}", v.discounted.len())));
    }

    #[test]
    fn one_modified_tracked_file_beside_setup_links_still_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "modified", "feature/modified");
        put_setup_links(&wt, &canonical);
        fs::write(wt.join("cli").join("keep.py"), "x = 999\n").unwrap();

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "modified-tracked");
    }

    #[test]
    fn one_plain_untracked_file_beside_setup_links_still_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "scratch", "feature/scratch");
        put_setup_links(&wt, &canonical);
        fs::write(wt.join("cli").join("scratch.py"), "real work\n").unwrap();

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
        assert!(v.detail.contains("scratch.py"));
    }

    #[test]
    fn a_symlink_out_of_the_canonical_checkout_still_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "outward", "feature/outward");
        put_setup_links(&wt, &canonical);
        let elsewhere = tmp.path().join("somewhere-else");
        fs::create_dir(&elsewhere).unwrap();
        symlink(&wt.join("cli").join("elsewhere"), &elsewhere);

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
        assert!(v.detail.contains("elsewhere"));
    }

    #[test]
    fn a_canonical_symlink_setup_never_writes_still_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "unknown", "feature/unknown");
        put_setup_links(&wt, &canonical);
        symlink(
            &wt.join("cli").join("borrowed.py"),
            &canonical.join("keep.py"),
        );

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
        assert!(v.detail.contains("borrowed.py"));
    }

    #[test]
    fn a_directory_mixing_a_setup_link_with_real_content_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "mixed", "feature/mixed");
        put_setup_links(&wt, &canonical);
        fs::write(wt.join("cli").join(".claude").join("notes.md"), "mine\n").unwrap();

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
        assert!(v.detail.contains("cli/.claude"));
    }

    #[test]
    fn classify_without_a_discount_answers_exactly_as_before() {
        let v = classify("?? cli/.agents\n", None);
        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
        assert!(v.discounted.is_empty());
    }

    // -- Parity: the discount must track what setup-worktree.sh actually links

    #[test]
    fn every_path_setup_links_is_discounted() {
        // Read the script's own link calls; each must pass the predicate.
        // Without this, adding `link_dir ".cursor"` to setup-worktree.sh
        // silently puts every fresh worktree back in the kept-forever bucket,
        // and no test fails. The script is the authority.
        let script_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/setup/setup-worktree.sh");
        let body = fs::read_to_string(&script_path).unwrap();
        let mut literals: Vec<String> = Vec::new();
        let mut dynamic: Vec<String> = Vec::new();
        for line in body.lines() {
            let trimmed = line.trim_start();
            let rest = trimmed
                .strip_prefix("link_dir ")
                .or_else(|| trimmed.strip_prefix("link_file "))
                .or_else(|| trimmed.strip_prefix("link_artifact "));
            let Some(rest) = rest else { continue };
            let arg = rest
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            let unquoted = arg.trim_matches('"');
            if arg.starts_with('"') && arg.ends_with('"') && !unquoted.contains('$') {
                literals.push(unquoted.to_string());
            } else if !arg.is_empty() {
                dynamic.push(arg);
            }
        }
        assert!(
            !literals.is_empty(),
            "no link_* literals parsed from the script"
        );
        for arg in &dynamic {
            assert!(
                arg.starts_with("\".claude/"),
                "unknown dynamic link target {arg}"
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let canonical_path = tmp.path().join("canonical");
        let worktree = tmp.path().join("wt");
        fs::create_dir(&canonical_path).unwrap();
        fs::create_dir_all(worktree.join("cli")).unwrap();
        for rel in &literals {
            let target = canonical_path.join(rel);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, "x").unwrap();
            // Both placements setup uses: at the worktree root, and one
            // directory deeper, the shape that reads untracked.
            for link in [worktree.join(rel), worktree.join("cli").join(rel)] {
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                symlink(&link, &target);
                assert!(
                    is_setup_link(&link, &worktree, &canonical_path),
                    "setup links {rel}, unknown"
                );
            }
        }
    }

    #[test]
    fn an_ignored_sibling_does_not_veto_the_discount() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "ignored", "feature/ignored");
        fs::write(wt.join(".gitignore"), "**/.claude/hooks/\n").unwrap();
        git(&wt, &["add", ".gitignore"]);
        commit_in(&wt, "ignore");
        put_setup_links(&wt, &canonical);
        fs::create_dir_all(wt.join("cli").join(".claude").join("hooks")).unwrap();
        fs::write(
            wt.join("cli").join(".claude").join("hooks").join("log.txt"),
            "runtime noise\n",
        )
        .unwrap();
        // The fixture really does reproduce the trap: the default read collapses.
        let default = git(&wt, &["status", "--porcelain"]);
        assert!(default.contains("?? cli/.claude/\n"));

        let v = reapable(wt.to_str().unwrap());

        assert!(
            v.reapable,
            "an ignored sibling must not block: {}",
            v.line()
        );
        assert_eq!(v.reason, "setup-links");
    }

    #[test]
    fn a_setup_target_linked_from_the_wrong_place_still_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonical_with_cli(tmp.path());
        let wt = linked_wt(tmp.path(), &canonical, "misplaced", "feature/misplaced");
        fs::create_dir_all(canonical.join("internal")).unwrap();
        symlink(&wt.join("vault"), &canonical.join("internal"));

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
        assert!(v.detail.contains("vault"));
    }

    // -- an unborn worktree is not a finished tree ----------------------------

    #[test]
    fn an_unborn_linked_worktree_refuses_inside_the_setup_window() {
        // AC1-HP: the defect itself. A tree minutes old with no commit of its
        // own is a worker mid-setup, and the sweep ate those.
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "unborn", "feature/unborn");

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "unborn");
        assert!(v.detail.contains("no commit of its own"));
    }

    #[test]
    fn a_rebased_and_landed_branch_inside_the_window_still_reaps() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "landed", "feature/landed");
        fs::write(wt.join("new.py"), "n = 1\n").unwrap();
        git(&wt, &["add", "-A"]);
        commit_in(&wt, "work");
        git(&repo, &["merge", "-q", "feature/landed"]);

        let v = reapable(wt.to_str().unwrap());

        assert!(v.reapable, "line was: {}", v.line());
    }

    #[test]
    fn an_old_unborn_worktree_past_the_window_is_reclaimable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "old", "feature/old");
        backdate(&wt, 7200);

        let v = reapable(wt.to_str().unwrap());

        assert!(v.reapable);
        assert_eq!(v.reason, "clean");
    }

    #[test]
    fn an_unreadable_reflog_never_authorizes_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "nolog", "feature/nolog");
        let common = git(&wt, &["rev-parse", "--git-common-dir"]);
        let mut common = PathBuf::from(common.trim());
        if !common.is_absolute() {
            common = wt.join(common);
        }
        fs::remove_dir_all(
            common
                .join("logs")
                .join("refs")
                .join("heads")
                .join("feature"),
        )
        .unwrap();

        let v = reapable(wt.to_str().unwrap());

        assert!(!v.reapable);
        assert_eq!(v.reason, "unborn");
    }

    #[test]
    fn a_named_tree_may_skip_the_setup_window_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "named", "feature/named");

        assert!(!reapable(wt.to_str().unwrap()).reapable);
        let v = reapable_opts(wt.to_str().unwrap(), true, false);
        assert!(v.reapable);
        assert_eq!(v.reason, "clean");
    }

    #[test]
    fn a_detached_head_is_not_judged_by_the_branch_reflog() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "detached", "scratch");
        git(&wt, &["checkout", "-q", "--detach"]);

        let v = reapable(wt.to_str().unwrap());

        assert!(v.reapable, "line was: {}", v.line());
    }

    // -- the node-token scanner (the branch_node_ids rule) --------------------

    #[test]
    fn scan_node_tokens_finds_delimiter_bounded_ids() {
        assert_eq!(scan_node_tokens("feature/x-cccc-1234"), vec!["x-cccc"]);
        assert_eq!(scan_node_tokens("feature/x-1179a"), vec!["x-1179a"]);
        assert_eq!(scan_node_tokens("x-7b9cd"), vec!["x-7b9cd"]);
        assert_eq!(scan_node_tokens("repro/x-7aafb-repro"), vec!["x-7aafb"]);
        assert!(scan_node_tokens("main").is_empty());
        assert!(scan_node_tokens("fix/thing").is_empty());
        // fixed-width hex: the greedy read takes the longest valid id
        assert_eq!(scan_node_tokens("feature/x-5b667"), vec!["x-5b667"]);
        assert_eq!(
            scan_node_tokens("x-ab123-x-cd456"),
            vec!["x-ab123", "x-cd456"]
        );
    }

    // -- the done-node arm -----------------------------------------------------

    fn value_row(id: &str, status: &str) -> serde_json::Value {
        serde_json::json!({ "id": id, "status": status })
    }

    /// A linked tree with a manifest naming `id`, aged past the grace window,
    /// holding one untracked file.
    fn done_node_fixture(tmp: &Path, id: &str) -> PathBuf {
        let repo = seed_repo(tmp);
        let wt = linked_wt(tmp, &repo, "fixture", &format!("feature/{id}"));
        backdate(&wt, 49 * 3600);
        // Ignore the manifest dir the way every real repo ignores .fno/, so
        // the untracked count below counts only the scratch file.
        fs::write(wt.join(".gitignore"), ".fno/\nignored/\n").unwrap();
        git(&wt, &["add", ".gitignore"]);
        commit_in(&wt, "ignore fno state");
        fs::create_dir_all(wt.join(".fno")).unwrap();
        fs::write(
            wt.join(".fno").join("target-state.md"),
            format!("graph_node_id: {id}\n"),
        )
        .unwrap();
        fs::write(wt.join("scratch.py"), "nope\n").unwrap();
        wt
    }

    /// Injected external truth: rows stand in for the graph store, claims for
    /// the claims root.
    struct FakeReaders {
        rows: Vec<serde_json::Value>,
        claims: Vec<String>,
    }

    impl FakeReaders {
        /// Run the gate with this fixture's truth. The closures live in this
        /// frame beside the struct they feed, so the borrows compile.
        fn reap(&self, wt: &str, done_node: bool) -> Verdict {
            let read_rows = || Some(self.rows.clone());
            let claim_live = |id: &str| self.claims.iter().any(|c| c == id);
            let readers = DoneNodeReaders {
                read_rows: &read_rows,
                claim_live: &claim_live,
            };
            reapable_opts_with(wt, false, done_node, &readers)
        }
    }

    #[test]
    fn done_node_yes_with_manifest_untracked_and_age() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        let fakes = FakeReaders {
            rows: vec![value_row("x-abc123", "done")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(v.reapable, "line was: {}", v.line());
        assert_eq!(v.reason, "done-node");
        assert_eq!(v.evidence.as_deref(), Some("node:x-abc123"));
        assert_eq!(v.untracked, 1);
        assert!(!v.detached);
        assert!(v
            .line()
            .contains("evidence=node:x-abc123 untracked=1 detached=no"));
    }

    #[test]
    fn without_the_flag_the_same_tree_reads_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        let fakes = FakeReaders {
            rows: vec![value_row("x-abc123", "done")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), false);

        assert!(!v.reapable);
        assert_eq!(v.reason, "untracked");
    }

    #[test]
    fn done_node_modified_tracked_stays_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        fs::write(wt.join("keep.py"), "x = 999\n").unwrap();
        let fakes = FakeReaders {
            rows: vec![value_row("x-abc123", "done")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(!v.reapable);
        assert_eq!(v.reason, "modified-tracked");
    }

    #[test]
    fn open_or_blocked_node_status_returns_the_base_verdict() {
        for status in ["in_progress", "deferred", "blocked", "triage"] {
            let tmp = tempfile::tempdir().unwrap();
            let wt = done_node_fixture(tmp.path(), "x-abc123");
            let fakes = FakeReaders {
                rows: vec![value_row("x-abc123", status)],
                claims: vec![],
            };

            let v = fakes.reap(wt.to_str().unwrap(), true);

            assert!(!v.reapable, "status {status} must not reap");
            assert_eq!(v.reason, "untracked", "status {status}");
        }
    }

    #[test]
    fn superseded_node_reaps_like_done() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        let fakes = FakeReaders {
            rows: vec![value_row("x-abc123", "superseded")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(v.reapable, "line was: {}", v.line());
        assert_eq!(v.reason, "done-node");
    }

    #[test]
    fn a_younger_tree_reads_done_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        backdate(&wt, 47 * 3600);
        let fakes = FakeReaders {
            rows: vec![value_row("x-abc123", "done")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(!v.reapable);
        assert_eq!(v.reason, "done-grace");
    }

    #[test]
    fn a_live_claim_keeps_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        let fakes = FakeReaders {
            rows: vec![value_row("x-abc123", "done")],
            claims: vec!["x-abc123".to_string()],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(!v.reapable);
        assert_eq!(v.reason, "claim-live");
    }

    #[test]
    fn a_node_no_store_knows_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        let fakes = FakeReaders {
            rows: vec![value_row("x-other1", "done")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(!v.reapable);
        assert_eq!(v.reason, "node-unknown");
    }

    #[test]
    fn an_unreadable_store_refuses_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        let readers = DoneNodeReaders {
            read_rows: &|| None,
            claim_live: &|_: &str| false,
        };

        let v = reapable_opts_with(wt.to_str().unwrap(), false, true, &readers);

        assert!(!v.reapable);
        assert_eq!(v.reason, "node-unknown");
    }

    #[test]
    fn branch_tokens_resolve_when_the_manifest_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "fixture", "feature/x-dead11");
        backdate(&wt, 49 * 3600);
        let fakes = FakeReaders {
            rows: vec![value_row("x-dead11", "done")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(v.reapable, "line was: {}", v.line());
        assert_eq!(v.evidence.as_deref(), Some("node:x-dead11"));
    }

    #[test]
    fn directory_basename_tokens_resolve_last() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        // detached HEAD, no manifest: only the basename can speak.
        let wt = linked_wt(tmp.path(), &repo, "x-f00d1-repro", "scratch");
        git(&wt, &["checkout", "-q", "--detach"]);
        backdate(&wt, 49 * 3600);
        let fakes = FakeReaders {
            rows: vec![value_row("x-f00d1", "done")],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(v.reapable, "line was: {}", v.line());
        assert!(v.detached);
        assert_eq!(v.evidence.as_deref(), Some("node:x-f00d1"));
    }

    #[test]
    fn a_nodeless_unmerged_branch_returns_the_base_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "leafy", "plain-branch");
        fs::write(wt.join("new.py"), "n = 1\n").unwrap();
        git(&wt, &["add", "-A"]);
        commit_in(&wt, "work");
        backdate(&wt, 49 * 3600);
        let fakes = FakeReaders {
            rows: vec![],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(
            v.reapable,
            "clean content still reaps on its own: {}",
            v.line()
        );
        assert_eq!(
            v.reason, "clean",
            "no done-node evidence, so no arm receipt"
        );
    }

    #[test]
    fn a_nodeless_merged_branch_reads_merged_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "leafy2", "done-branch");
        git(&wt, &["commit", "--allow-empty", "-qm", "w"]);
        git(&repo, &["merge", "-q", "done-branch"]);
        backdate(&wt, 49 * 3600);
        // Untracked content is what the arm adds authority for: a clean tree
        // with a merged branch was already the sweep's to archive.
        fs::write(wt.join("scratch.py"), "nope\n").unwrap();
        let fakes = FakeReaders {
            rows: vec![],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(v.reapable, "line was: {}", v.line());
        assert_eq!(v.reason, "done-node");
        assert_eq!(v.evidence.as_deref(), Some("merged"));
        assert_eq!(v.untracked, 1);
    }

    #[test]
    fn a_clean_merged_tree_keeps_todays_base_verdict_under_the_arm() {
        // The arm never shrinks an existing grant: a clean tree with a merged
        // branch is archived by the sweep's step-2 filter today, and the arm's
        // grace window must not veto it.
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());
        let wt = linked_wt(tmp.path(), &repo, "leafy3", "young-merged");
        git(&wt, &["commit", "--allow-empty", "-qm", "w"]);
        git(&repo, &["merge", "-q", "young-merged"]);
        let fakes = FakeReaders {
            rows: vec![],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(v.reapable, "line was: {}", v.line());
        assert_eq!(v.reason, "clean", "base verdict, no arm receipt");
    }

    #[test]
    fn the_working_graph_outranks_the_archive_on_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = done_node_fixture(tmp.path(), "x-abc123");
        // Archive (appended second) says done; the working graph says open.
        let fakes = FakeReaders {
            rows: vec![
                value_row("x-abc123", "in_progress"),
                value_row("x-abc123", "done"),
            ],
            claims: vec![],
        };

        let v = fakes.reap(wt.to_str().unwrap(), true);

        assert!(!v.reapable, "the live store must outrank the archive");
        assert_eq!(v.reason, "untracked");
    }

    #[test]
    fn client_verb_exit_codes_follow_the_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path());

        assert_eq!(run_client(&[repo.to_string_lossy().to_string()]), 0);
        assert_eq!(run_client(&["/definitely/not/a/dir".to_string()]), 1);
        assert_eq!(run_client(&[]), 2);
        assert_eq!(
            run_client(&["--bogus".to_string(), repo.to_string_lossy().to_string()]),
            2
        );
    }
}
