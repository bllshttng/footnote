//! Native authority for machine-wide update source eligibility.
//!
//! `fno doctor update` installs whatever source its resolution picks, and that
//! resolution cached a bare path: a linked worktree whose HEAD diverged from
//! origin/main got installed machine-wide and every later update re-installed
//! it (measured 2026-09-09 and 2026-09-13). This module owns the decision
//! natively so Python keeps only transport. The gate covers any git checkout
//! picked implicitly: a proven non-ancestor of the remote-default ref refuses,
//! whatever the kind, because a divergent main checkout is the canonical
//! checkout after a pull merged a ref origin never merged. Ancestry that
//! cannot be proven refuses a linked worktree and allows a main checkout. An
//! explicit `--source` stays the escape hatch (accepted, but loud). Non-git
//! packaged candidates keep existing behavior. Ancestry is probed live; the
//! cached companion never supplies it.

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Companion-record schema. Bump only for a breaking shape change; readers
/// treat an unknown or missing version as unknown provenance, never evidence.
const PIN_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeKind {
    MainCheckout,
    LinkedWorktree,
    NonGit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Refuse,
}
/// One `source-pin resolve` answer: full typed evidence for the winning
/// candidate plus the safety decision. `ancestor: null` means "could not be
/// proven", which refuses for implicit picks, never a pass.
#[derive(Debug, Serialize, PartialEq)]
pub struct ResolveAnswer {
    pub decision: Decision,
    /// Resolved CLI source directory; null when nothing validated.
    pub path: Option<String>,
    /// explicit | env | checkout | cache | candidate.
    pub origin: Option<String>,
    pub worktree_kind: Option<WorktreeKind>,
    /// Symbolic branch name; null when detached or unreadable.
    pub branch: Option<String>,
    /// true/false; null when the branch state is unreadable.
    pub detached: Option<bool>,
    pub source_head: Option<String>,
    /// Remote-default ref name used (origin/HEAD's target, else origin/main).
    pub remote_ref: Option<String>,
    pub remote_head: Option<String>,
    /// None = ancestry could not be proven (instrument named in `detail`).
    pub ancestor: Option<bool>,
    /// eligible | divergent | ancestry_unknown | invalid_override | no_source
    pub eligibility: String,
    /// Commits HEAD..remote-default, when the source is a proven-ancestor
    /// checkout that is strictly behind. None = current, divergent, or
    /// unreadable (the `sync` detail names which).
    pub behind: Option<u64>,
    /// Operator-facing text for the allow-with-warning outcome.
    pub warning: Option<String>,
    /// Operator-facing text for the refuse outcome.
    pub refusal: Option<String>,
    /// One-line verdict for update --check and the TUI: the refuse reason, or
    /// the behind distance. None when there is nothing to act on.
    pub guidance: Option<String>,
    /// The instrument that failed, for ancestry_unknown.
    pub detail: Option<String>,
}

/// One `source-pin sync` answer: byte-compatible with doctor's
/// `source_checkout_sync` contract: status, behind, both heads, detail.
#[derive(Debug, Serialize, PartialEq)]
pub struct SyncAnswer {
    pub status: String,
    pub behind: Option<u64>,
    pub source_head: Option<String>,
    pub remote_head: Option<String>,
    pub detail: String,
}

/// One bounded git probe. `None` = could not run at all (spawn failure or the
/// deadline fired and the child was killed); `Some` = the child ran and ended.
struct GitOut {
    ok: bool,
    code: i32,
    stdout: String,
    stderr: String,
}

fn git(dir: &str, args: &[&str]) -> Option<GitOut> {
    if !Path::new(dir).is_dir() {
        return None;
    }
    let mut command = Command::new("git");
    command
        .args(["-C", dir])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    // Outputs are single-line revs/paths, far below the pipe buffer, so the
    // child can never have blocked on write; reading after exit is safe.
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
    Some(GitOut {
        ok: code == 0,
        code,
        stdout,
        stderr,
    })
}

fn git_ok(dir: &str, args: &[&str]) -> Option<String> {
    let out = git(dir, args)?;
    if !out.ok {
        return None;
    }
    let s = out.stdout.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// True if `dir` holds a `pyproject.toml` whose `[project] name` is `fno`.
/// Parsed, not substring-matched, so a stray `name = "fno"` in another table
/// cannot false-match. Any read/parse failure is "not a source".
fn is_fno_source(dir: &str) -> bool {
    let raw = match std::fs::read_to_string(Path::new(dir).join("pyproject.toml")) {
        Ok(raw) => raw,
        Err(_) => return false,
    };
    let value: toml::Value = match raw.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    value
        .get("project")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(|n| n == "fno")
        .unwrap_or(false)
}

/// Main checkout vs linked worktree vs not-a-repo. Anything unprovable is
/// `NonGit` (today's packaged-candidate behavior); only a PROVEN linked
/// worktree enters the ancestry gate.
fn worktree_kind(dir: &str) -> WorktreeKind {
    let common = match git_ok(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        Some(c) => c,
        None => return WorktreeKind::NonGit,
    };
    let own = match git_ok(dir, &["rev-parse", "--path-format=absolute", "--git-dir"]) {
        Some(d) => d,
        None => return WorktreeKind::NonGit,
    };
    if common == own {
        WorktreeKind::MainCheckout
    } else {
        WorktreeKind::LinkedWorktree
    }
}

/// Discover the local remote-default ref: `refs/remotes/origin/HEAD`'s target
/// when set, else the `origin/main` compatibility fallback. Returns the NAME;
/// the caller verifies it actually resolves.
fn remote_ref(dir: &str) -> String {
    match git_ok(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        Some(full) => full
            .strip_prefix("refs/remotes/")
            .unwrap_or(&full)
            .to_string(),
        None => "origin/main".to_string(),
    }
}

struct HeadEvidence {
    branch: Option<String>,
    detached: Option<bool>,
    source_head: Option<String>,
}

fn head_evidence(dir: &str) -> HeadEvidence {
    match git_ok(dir, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Some(b) if b == "HEAD" => HeadEvidence {
            branch: None,
            detached: Some(true),
            source_head: git_ok(dir, &["rev-parse", "HEAD"]),
        },
        Some(b) => HeadEvidence {
            branch: Some(b),
            detached: Some(false),
            source_head: git_ok(dir, &["rev-parse", "HEAD"]),
        },
        None => HeadEvidence {
            branch: None,
            detached: None,
            source_head: None,
        },
    }
}

fn short(sha: &Option<String>) -> String {
    sha.as_ref()
        .map_or("?".to_string(), |s| s[..s.len().min(12)].to_string())
}

/// The safety classification for one validated candidate. Live probes only:
/// nothing here reads the cached companion record.
fn classify(path: &str, origin: &str) -> ResolveAnswer {
    let kind = worktree_kind(path);
    let heads = head_evidence(path);
    let rref = remote_ref(path);
    let remote_head = git_ok(
        path,
        &["rev-parse", "--verify", &format!("{rref}^{{commit}}")],
    );

    // Ancestry is probed for every git checkout, not only a linked worktree:
    // a divergent main checkout is the same hazard on an implicit pick (the
    // canonical checkout after a pull merged a ref origin never merged). A
    // main checkout with no remote-default ref cannot prove divergence, so it
    // keeps today's eligible answer with the instrument detail cleared.
    let (ancestor, detail): (Option<bool>, Option<String>) = if kind == WorktreeKind::NonGit
        || (kind == WorktreeKind::MainCheckout && remote_head.is_none())
    {
        (None, None)
    } else if remote_head.is_none() {
        (
            None,
            Some(format!(
                "remote-default ref {rref} does not resolve (origin/HEAD and its origin/main fallback both unreadable)"
            )),
        )
    } else {
        match git(path, &["merge-base", "--is-ancestor", "HEAD", &rref]) {
            // rc 0: proven ancestor. rc 1: git's typed "not an ancestor"
            // answer, so divergence is a fact, not an error. Any other exit
            // is a broken instrument, named for the refusal text.
            Some(out) if out.ok => (Some(true), None),
            Some(out) if out.code == 1 => (Some(false), None),
            Some(out) => (
                None,
                Some(format!(
                    "git merge-base --is-ancestor exited {}: {}",
                    out.code,
                    out.stderr.trim()
                )),
            ),
            None => (
                None,
                Some(
                    "git merge-base --is-ancestor could not be run (timeout or spawn failure)"
                        .to_string(),
                ),
            ),
        }
    };

    let eligibility = match (kind, ancestor) {
        (WorktreeKind::LinkedWorktree, None) => "ancestry_unknown",
        (_, Some(false)) => "divergent",
        _ => "eligible",
    };

    let explicit = origin == "explicit";
    let state = branch_label(heads.branch.as_deref(), heads.detached);
    let (decision, mut warning, refusal) = match eligibility {
        "eligible" => (Decision::Allow, None, None),
        "divergent" if explicit && kind == WorktreeKind::MainCheckout => (
            Decision::Allow,
            Some(format!(
                "warning: --source {path} is a divergent main checkout ({state} HEAD {sh} vs {rref} HEAD {rh}); installing by explicit request",
                sh = short(&heads.source_head),
                rh = short(&remote_head),
            )),
            None,
        ),
        "divergent" if explicit => (
            Decision::Allow,
            Some(format!(
                "warning: --source {path} is a divergent linked worktree ({state} HEAD {sh} vs {rref} HEAD {rh}); installing by explicit request",
                sh = short(&heads.source_head),
                rh = short(&remote_head),
            )),
            None,
        ),
        "divergent" if kind == WorktreeKind::MainCheckout => (
            Decision::Refuse,
            None,
            Some(format!(
                "refusing source {path}: main checkout {state} HEAD {sh} is not an ancestor of {rref} HEAD {rh}, so it holds commits {rref} never merged. A machine-wide install ships them to every fno process on the machine. Save those commits on a branch and reset the checkout to {rref}, or re-run with --source {path} to install it on purpose.",
                sh = short(&heads.source_head),
                rh = short(&remote_head),
            )),
        ),
        "divergent" => (
            Decision::Refuse,
            None,
            Some(format!(
                "refusing source {path}: linked worktree {state} HEAD {sh} is not an ancestor of {rref} HEAD {rh}. A machine-wide install from a feature worktree reverts every fno binary on the machine. Re-run with --source {path} to override, or run the update from the canonical checkout to repin the cache.",
                sh = short(&heads.source_head),
                rh = short(&remote_head),
            )),
        ),
        "ancestry_unknown" if explicit => (
            Decision::Allow,
            Some(format!(
                "warning: --source {path} is a linked worktree whose ancestry could not be proven ({}); installing by explicit request",
                detail.as_deref().unwrap_or("git evidence unreadable"),
            )),
            None,
        ),
        "ancestry_unknown" => (
            Decision::Refuse,
            None,
            Some(format!(
                "refusing source {path}: linked worktree ancestry could not be proven ({}). Refusing rather than installing from unproven state. Re-run with --source {path} to override.",
                detail.as_deref().unwrap_or("git evidence unreadable"),
            )),
        ),
        _ => (Decision::Allow, None, None),
    };

    // Distance and guidance come from `sync`, the one staleness reader - no
    // second rev-list probe here. A non-ancestor HEAD reads unknown from
    // `sync`, so `behind` only ever pairs with an eligible allow.
    let (mut behind, mut guidance) = (None, None);
    if decision == Decision::Refuse {
        guidance = Some(format!(
            "update blocked: {}",
            refusal
                .as_deref()
                .unwrap_or("the resolved source failed the source-pin gate")
        ));
    } else if kind != WorktreeKind::NonGit {
        let s = sync(path);
        if s.status == "behind" {
            if let Some(n) = s.behind {
                behind = Some(n);
                let repo = Path::new(path)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string());
                let sh = short(&s.source_head);
                let rh = short(&s.remote_head);
                guidance = Some(format!(
                    "source checkout {sh} is {n} commit(s) behind {rref} {rh}; merged changes there are not installed. Sync it (git -C {repo} pull --ff-only), then run fno doctor update"
                ));
                if warning.is_none() {
                    warning = Some(format!(
                        "warning: source {path} is {n} commit(s) behind {rref}; this installs the older snapshot. Sync first: git -C {repo} pull --ff-only"
                    ));
                }
            }
        }
    }

    ResolveAnswer {
        decision,
        path: Some(path.to_string()),
        origin: Some(origin.to_string()),
        worktree_kind: Some(kind),
        branch: heads.branch,
        detached: heads.detached,
        source_head: heads.source_head,
        remote_ref: Some(rref),
        remote_head,
        ancestor,
        eligibility: eligibility.to_string(),
        behind,
        warning,
        refusal,
        guidance,
        detail,
    }
}

/// "branch feature/x" / "detached HEAD" / "unreadable branch state" for the
/// operator-facing refusal and warning lines.
fn branch_label(branch: Option<&str>, detached: Option<bool>) -> String {
    match (detached, branch) {
        (Some(true), _) => "detached HEAD".to_string(),
        (Some(false), Some(b)) => format!("branch {b}"),
        _ => "unreadable branch state".to_string(),
    }
}

struct ResolveArgs {
    override_path: Option<String>,
    env_source: Option<String>,
    /// `<main checkout>/cli` of the repo the process runs in, set by the verb
    /// from its own cwd.
    checkout: Option<String>,
    cache: Option<String>,
    candidate_paths: Vec<String>,
}

/// Precedence lives HERE, natively: override > env > checkout > cache >
/// candidates, first directory whose `[project] name` is `fno` wins. Paths
/// arrive already expanded and resolved from the transport layer; duplicate
/// paths keep the earliest origin bucket. `checkout` is the `cli/` of the main
/// checkout that owns the process cwd, from a linked worktree too, so a run
/// inside the repo installs that repo's main checkout and the cache answers
/// only a run from outside any fno checkout.
fn resolve(args: &ResolveArgs) -> ResolveAnswer {
    let mut candidates: Vec<(&str, String)> = Vec::new();
    if let Some(o) = &args.override_path {
        candidates.push(("explicit", o.clone()));
    }
    if let Some(e) = &args.env_source {
        candidates.push(("env", e.clone()));
    }
    if let Some(c) = &args.checkout {
        candidates.push(("checkout", c.clone()));
    }
    if let Some(cache) = &args.cache {
        if let Some(line) = std::fs::read_to_string(cache)
            .ok()
            .map(|raw| raw.trim().to_string())
            .filter(|l| !l.is_empty())
        {
            candidates.push(("cache", line));
        }
    }
    for c in &args.candidate_paths {
        candidates.push(("candidate", c.clone()));
    }

    let mut seen: Vec<&str> = Vec::new();
    for (origin, path) in &candidates {
        if seen.contains(&path.as_str()) {
            continue;
        }
        seen.push(path.as_str());
        if !is_fno_source(path) {
            if *origin == "explicit" {
                return no_source_answer(Some(path));
            }
            continue;
        }
        return classify(path, origin);
    }
    no_source_answer(None)
}

/// Terminal answer when nothing validated (or an explicit override is not a
/// source): the Python transport maps these to its existing error types.
fn no_source_answer(invalid_override: Option<&str>) -> ResolveAnswer {
    let (eligibility, refusal) = if let Some(path) = invalid_override {
        (
            "invalid_override",
            format!(
                "--source {path} does not contain a pyproject.toml with name = 'fno'. Pass a path to the fno CLI source directory."
            ),
        )
    } else {
        (
            "no_source",
            // The plugins dir is named in prose, not as a path literal: the
            // placement-rule gate bars ~/.claude path construction here, and
            // the guidance does not need the exact prefix to be followable.
            "Could not locate the fno CLI source. Run it from inside an fno checkout, pass --source /path/to/fno/cli, set $FNO_SOURCE, or install the fno plugin (the Claude plugins directory).".to_string(),
        )
    };
    ResolveAnswer {
        decision: Decision::Refuse,
        path: None,
        origin: None,
        worktree_kind: None,
        branch: None,
        detached: None,
        source_head: None,
        remote_ref: None,
        remote_head: None,
        ancestor: None,
        eligibility: eligibility.to_string(),
        behind: None,
        warning: None,
        refusal: Some(refusal),
        guidance: None,
        detail: None,
    }
}

/// `source-pin sync`: the doctor staleness measurement, ported verbatim. A
/// missing or non-ancestor remote ref is unknown, never a fabricated distance.
fn sync(source: &str) -> SyncAnswer {
    let mut ans = SyncAnswer {
        status: "unknown".to_string(),
        behind: None,
        source_head: None,
        remote_head: None,
        detail: String::new(),
    };
    let source_head = match git_ok(source, &["rev-parse", "HEAD"]) {
        Some(h) => h,
        None => {
            ans.detail = "source checkout HEAD is unreadable".to_string();
            return ans;
        }
    };
    ans.source_head = Some(source_head.clone());
    let rref = remote_ref(source);
    let remote_head = match git_ok(
        source,
        &["rev-parse", "--verify", &format!("{rref}^{{commit}}")],
    ) {
        Some(h) => h,
        None => {
            ans.detail = format!("{rref} ref is unreadable");
            return ans;
        }
    };
    ans.remote_head = Some(remote_head.clone());
    if source_head == remote_head {
        ans.status = "current".to_string();
        ans.behind = Some(0);
        return ans;
    }
    let ancestor = match git(source, &["merge-base", "--is-ancestor", "HEAD", &rref]) {
        Some(out) => out,
        None => {
            ans.detail = "source-sync ancestry probe failed".to_string();
            return ans;
        }
    };
    if !ancestor.ok {
        ans.detail = format!("source HEAD is not an ancestor of {rref}");
        return ans;
    }
    let raw = match git_ok(source, &["rev-list", "--count", &format!("HEAD..{rref}")]) {
        Some(c) => c,
        None => {
            ans.detail = "source-sync distance probe failed".to_string();
            return ans;
        }
    };
    match raw.parse::<u64>() {
        Err(_) => {
            ans.detail = "origin/main distance is not an integer".to_string();
        }
        Ok(0) => {
            ans.detail = "origin/main distance is unavailable".to_string();
        }
        Ok(n) => {
            ans.status = "behind".to_string();
            ans.behind = Some(n);
        }
    }
    ans
}

/// Atomic write in the destination's own directory: temp file then rename, so
/// a concurrent `fno doctor` reader never sees a torn or empty pin. 0600 on
/// the companion (it names checkouts and revs); the legacy path file keeps
/// today's plain content and default permissions.
fn atomic_write(path: &Path, data: &str, mode_0600: bool) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("pin path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pin".to_string());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    if mode_0600 {
        let _ = crate::paths::set_file_mode_0600(&tmp);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// `source-pin record`: consume one resolve answer and maintain BOTH pins,
/// companion first (evidence lands before the pointer flips), legacy
/// `source-path` second. A companion that disagrees with the legacy file reads
/// as unknown provenance on the read side - never evidence; eligibility is
/// always decided by live probes at resolve time.
fn record_from_str(raw: &str, cache: &str, companion: &str) -> Result<(), String> {
    let mut answer: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|e| format!("stdin is not valid JSON: {e}"))?;
    let path = answer
        .get("path")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "stdin JSON carries no usable path".to_string())?
        .to_string();
    let obj = answer
        .as_object_mut()
        .ok_or("stdin JSON is not an object")?;
    obj.insert("schema".into(), serde_json::json!(PIN_SCHEMA));
    obj.insert(
        "recorded_at".into(),
        serde_json::json!(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    let body = serde_json::to_string_pretty(&answer)
        .map_err(|e| format!("companion serialization failed: {e}"))?;
    atomic_write(Path::new(companion), &format!("{body}\n"), true)
        .map_err(|e| format!("companion write failed: {e}"))?;
    atomic_write(Path::new(cache), &format!("{path}\n"), false)
        .map_err(|e| format!("legacy source-path write failed: {e}"))?;
    Ok(())
}

/// `fno-agents source-pin`: hidden binary-direct verb for update/doctor.
/// Exit 0 whenever an answer was computed (a refusal is data, not an error);
/// exit 2 on malformed args; exit 1 on a failed record write.
pub fn run_source_pin(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usage: fno-agents source-pin resolve [--override <cli>] [--env-source <cli>]\n\
             [--cache <file>] [--candidate <cli>]...\n\
             | sync --source <cli>\n\
             | record --cache <file> --companion <file>   (resolve JSON on stdin)"
        );
        return 0;
    }
    let (sub, rest) = match args.split_first() {
        Some((s, r)) => (s.as_str(), r),
        None => {
            eprintln!("fno-agents source-pin: a subcommand is required (resolve|sync|record)");
            return 2;
        }
    };
    match sub {
        "resolve" => match parse_resolve_args(rest) {
            Ok(mut a) => {
                // The verb fills `checkout` from its own cwd: the cli/ of the
                // main checkout that owns this process, linked worktree
                // included, so a bare run inside the repo installs that repo.
                a.checkout = std::env::current_dir()
                    .ok()
                    .and_then(|d| crate::paths::canonical_repo_root(&d))
                    .map(|root| root.join("cli").to_string_lossy().into_owned());
                print_json(&resolve(&a))
            }
            Err(e) => {
                eprintln!("fno-agents source-pin: {e}");
                2
            }
        },
        "sync" => match value_of(rest, "--source") {
            Ok(Some(src)) => print_json(&sync(&src)),
            Ok(None) => {
                eprintln!("fno-agents source-pin: --source is required");
                2
            }
            Err(e) => {
                eprintln!("fno-agents source-pin: {e}");
                2
            }
        },
        "record" => match (value_of(rest, "--cache"), value_of(rest, "--companion")) {
            (Ok(Some(c)), Ok(Some(m))) if !c.is_empty() && !m.is_empty() => match record(&c, &m) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("fno-agents source-pin: {e}");
                    1
                }
            },
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("fno-agents source-pin: {e}");
                2
            }
            _ => {
                eprintln!("fno-agents source-pin: --cache and --companion are required");
                2
            }
        },
        other => {
            eprintln!("fno-agents source-pin: unknown subcommand: {other}");
            2
        }
    }
}

fn value_of(args: &[String], flag: &str) -> Result<Option<String>, String> {
    match args.iter().position(|a| a == flag) {
        Some(i) => args
            .get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value"))
            .map(Some),
        None => Ok(None),
    }
}

fn parse_resolve_args(args: &[String]) -> Result<ResolveArgs, String> {
    fn require(args: &[String], i: usize, flag: &str) -> Result<String, String> {
        args.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value"))
    }
    let mut p = ResolveArgs {
        override_path: None,
        env_source: None,
        checkout: None,
        cache: None,
        candidate_paths: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        // A no-value flag advances by one; a value flag consumes two tokens.
        let advance = match args[i].as_str() {
            "--override" => {
                p.override_path = Some(require(args, i, "--override")?);
                2
            }
            "--env-source" => {
                p.env_source = Some(require(args, i, "--env-source")?);
                2
            }
            "--cache" => {
                p.cache = Some(require(args, i, "--cache")?);
                2
            }
            "--candidate" => {
                p.candidate_paths.push(require(args, i, "--candidate")?);
                2
            }
            // stdout is only the resolve JSON; the flag is accepted for parity.
            "--json" | "-J" => 1,
            other => return Err(format!("unknown flag: {other}")),
        };
        i += advance;
    }
    Ok(p)
}

fn print_json<T: serde::Serialize>(value: &T) -> i32 {
    match serde_json::to_string(value) {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => {
            eprintln!("fno-agents source-pin: serialization error: {e}");
            1
        }
    }
}

/// `source-pin record`: stdin wrapper around `record_from_str`.
fn record(cache: &str, companion: &str) -> Result<(), String> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| format!("could not read stdin: {e}"))?;
    record_from_str(&raw, cache, companion)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    // Real git fixtures: the whole point is ancestry evidence, so every AC
    // test builds a tiny clone + worktree with actual refs.

    /// One git command, panicking on failure - fixtures abort the test loudly.
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git binary available");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one commit on main and a `cli/` fno source inside.
    fn new_repo(dir: &std::path::Path) {
        fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-b", "main", "-q"]);
        fs::write(dir.join("f.txt"), "1\n").unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "c1"]);
    }

    /// `cli/pyproject.toml` naming the package `fno` inside the given root.
    fn add_cli(dir: &std::path::Path) -> String {
        let cli = dir.join("cli");
        fs::create_dir_all(&cli).unwrap();
        fs::write(
            cli.join("pyproject.toml"),
            "[project]\nname = \"fno\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        cli.to_string_lossy().into_owned()
    }

    /// Clone of `origin` plus a linked worktree at origin/main. Returns
    /// (clone, worktree cli) paths; tests diverge the worktree by committing.
    fn clone_with_worktree(base: &std::path::Path) -> (String, String) {
        let origin = base.join("origin");
        new_repo(&origin);
        add_cli(&origin);
        let clone = base.join("clone");
        git_in(
            base,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        add_cli(&clone);
        let wt = base.join("wt");
        git_in(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                wt.to_str().unwrap(),
            ],
        );
        let wt_cli = add_cli(&wt);
        (clone.to_string_lossy().into_owned(), wt_cli)
    }

    #[test]
    fn ac1_hp_worktree_ancestor_is_eligible() {
        let base = tempfile::tempdir().unwrap();
        let (_clone, wt) = clone_with_worktree(&base.path().join("a1"));
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![wt],
        });
        assert_eq!(a.decision, Decision::Allow);
        assert_eq!(a.origin.as_deref(), Some("candidate"));
        assert_eq!(a.worktree_kind, Some(WorktreeKind::LinkedWorktree));
        assert_eq!(a.ancestor, Some(true));
    }

    #[test]
    fn ac1_err_divergent_implicit_worktree_refuses() {
        let base = tempfile::tempdir().unwrap();
        let (_clone, wt) = clone_with_worktree(&base.path().join("e1"));
        fs::write(std::path::Path::new(&wt).join("d.txt"), "x\n").unwrap();
        git_in(std::path::Path::new(&wt), &["add", "-A"]);
        git_in(
            std::path::Path::new(&wt),
            &["commit", "-q", "-m", "diverge"],
        );
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![wt.clone()],
        });
        assert_eq!(a.decision, Decision::Refuse);
        let refusal = a.refusal.unwrap();
        assert!(refusal.contains(&wt), "refusal names the path: {refusal}");
        assert!(refusal.contains("feature/x"), "names the branch: {refusal}");
        assert!(
            refusal.contains("--source"),
            "names the override: {refusal}"
        );
        assert!(
            refusal.contains("not an ancestor"),
            "names the divergence: {refusal}"
        );
    }

    #[test]
    fn ac2_edge_explicit_divergent_accepted_with_warning() {
        let base = tempfile::tempdir().unwrap();
        let (_clone, wt) = clone_with_worktree(&base.path().join("a2"));
        fs::write(std::path::Path::new(&wt).join("d.txt"), "x\n").unwrap();
        git_in(std::path::Path::new(&wt), &["add", "-A"]);
        git_in(
            std::path::Path::new(&wt),
            &["commit", "-q", "-m", "diverge"],
        );
        let a = resolve(&ResolveArgs {
            override_path: Some(wt.clone()),
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![],
        });
        assert_eq!(a.decision, Decision::Allow);
        let warning = a.warning.unwrap();
        assert!(
            warning.contains("divergent"),
            "warning names divergence: {warning}"
        );
        assert_eq!(a.ancestor, Some(false));
    }

    #[test]
    fn ac3_err_unproven_ancestry_refuses_and_names_instrument() {
        let base = tempfile::tempdir().unwrap();
        let (clone, wt) = clone_with_worktree(&base.path().join("e3"));
        git_in(
            std::path::Path::new(&clone),
            &["update-ref", "-d", "refs/remotes/origin/main"],
        );
        git_in(
            std::path::Path::new(&clone),
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        );
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![wt.clone()],
        });
        assert_eq!(a.decision, Decision::Refuse);
        assert_eq!(a.ancestor, None);
        assert_eq!(a.eligibility, "ancestry_unknown");
        let refusal = a.refusal.unwrap();
        assert!(
            refusal.contains("could not be proven"),
            "names the gap: {refusal}"
        );
    }

    #[test]
    fn ac3_hp_record_writes_legacy_plus_companion() {
        let base = tempfile::tempdir().unwrap();
        let (_clone, wt) = clone_with_worktree(&base.path().join("r1"));
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![wt.clone()],
        });
        let raw = serde_json::to_string(&a).unwrap();
        let cache = base.path().join("cache").join("source-path");
        let companion = base.path().join("cache").join("source-pin.json");
        record_from_str(&raw, cache.to_str().unwrap(), companion.to_str().unwrap()).unwrap();
        assert_eq!(fs::read_to_string(&cache).unwrap(), format!("{wt}\n"));
        let pin: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&companion).unwrap()).unwrap();
        assert_eq!(pin["schema"], json!(1));
        assert!(!pin["recorded_at"].as_str().unwrap().is_empty());
        assert_eq!(pin["path"], json!(wt));
        assert_eq!(pin["worktree_kind"], json!("linked_worktree"));
        assert_eq!(pin["ancestor"], json!(true));
        assert_eq!(pin["eligibility"], json!("eligible"));
    }

    #[test]
    fn precedence_override_beats_env() {
        let base = tempfile::tempdir().unwrap();
        let o_root = base.path().join("o");
        let e_root = base.path().join("e");
        new_repo(&o_root);
        new_repo(&e_root);
        let o_cli = add_cli(&o_root);
        add_cli(&e_root);
        let r = resolve(&ResolveArgs {
            override_path: Some(o_cli.clone()),
            env_source: Some(e_root.join("cli").to_string_lossy().into_owned()),
            checkout: None,
            cache: None,
            candidate_paths: vec![],
        });
        assert_eq!(r.origin, Some("explicit".to_string()));
        assert_eq!(r.path, Some(o_cli));
    }

    #[test]
    fn no_valid_candidate_refuses_with_locate_message() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("not-fno");
        let cli = add_cli(&root);
        let pyproject = std::path::Path::new(&cli).join("pyproject.toml");
        std::fs::write(&pyproject, "[project]\nname = \"other\"\n").unwrap();
        let r = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![cli],
        });
        assert_eq!(r.eligibility, "no_source");
        assert!(r.refusal.unwrap().contains("Could not locate"));
    }

    #[test]
    fn non_git_candidate_keeps_existing_behavior() {
        let base = tempfile::tempdir().unwrap();
        let plain = add_cli(&base.path().join("plain"));
        let r = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![plain],
        });
        assert_eq!(r.worktree_kind, Some(WorktreeKind::NonGit));
        assert_eq!(r.decision, Decision::Allow);
    }
    #[test]
    fn sync_current_when_heads_match() {
        let base = tempfile::tempdir().unwrap();
        let (clone, _wt) = clone_with_worktree(&base.path().join("s1"));
        let s = sync(&clone);
        assert_eq!(s.status, "current");
        assert_eq!(s.behind, Some(0));
        assert_eq!(s.detail, "");
    }

    #[test]
    fn sync_behind_counts_distance() {
        let base = tempfile::tempdir().unwrap();
        let (clone, _wt) = clone_with_worktree(&base.path().join("s2"));
        // Commit B locally, reset to A, move origin/main ref to B.
        fs::write(std::path::Path::new(&clone).join("2.txt"), "x\n").unwrap();
        git_in(std::path::Path::new(&clone), &["add", "-A"]);
        git_in(std::path::Path::new(&clone), &["commit", "-q", "-m", "b"]);
        let b = git_ok(&clone, &["rev-parse", "HEAD"]).unwrap();
        git_in(
            std::path::Path::new(&clone),
            &["reset", "-q", "--hard", "HEAD~1"],
        );
        git_in(
            std::path::Path::new(&clone),
            &["update-ref", "refs/remotes/origin/main", b.trim()],
        );
        let s = sync(&clone);
        assert_eq!(s.status, "behind", "detail: {}", s.detail);
        assert_eq!(s.detail, "");
    }

    #[test]
    fn sync_not_ancestor_reads_unknown() {
        let base = tempfile::tempdir().unwrap();
        let (clone, _wt) = clone_with_worktree(&base.path().join("s3"));
        fs::write(std::path::Path::new(&clone).join("3.txt"), "x\n").unwrap();
        git_in(std::path::Path::new(&clone), &["add", "-A"]);
        git_in(std::path::Path::new(&clone), &["commit", "-q", "-m", "c"]);
        let s = sync(&clone);
        assert_eq!(s.status, "unknown");
        assert_eq!(s.behind, None);
        assert_eq!(s.detail, "source HEAD is not an ancestor of origin/main");
    }

    #[test]
    fn ac1_hp_behind_main_checkout_names_distance() {
        let base = tempfile::tempdir().unwrap();
        let (clone, _wt) = clone_with_worktree(&base.path().join("a4"));
        // Move origin/main one commit ahead of the clone's HEAD. Stage only
        // 2.txt: `add -A` would track the untracked cli/pyproject.toml and the
        // reset below would delete it, breaking the fno-source check.
        fs::write(std::path::Path::new(&clone).join("2.txt"), "x\n").unwrap();
        git_in(std::path::Path::new(&clone), &["add", "2.txt"]);
        git_in(std::path::Path::new(&clone), &["commit", "-q", "-m", "b"]);
        let b = git_ok(&clone, &["rev-parse", "HEAD"]).unwrap();
        git_in(
            std::path::Path::new(&clone),
            &["reset", "-q", "--hard", "HEAD~1"],
        );
        git_in(
            std::path::Path::new(&clone),
            &["update-ref", "refs/remotes/origin/main", b.trim()],
        );
        let cli = std::path::Path::new(&clone)
            .join("cli")
            .to_string_lossy()
            .into_owned();
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![cli],
        });
        assert_eq!(a.decision, Decision::Allow);
        assert_eq!(a.behind, Some(1));
        let guidance = a.guidance.unwrap();
        assert!(
            guidance.contains("1 commit(s) behind origin/main"),
            "guidance names the distance: {guidance}"
        );
        assert!(!guidance.contains("current"), "no word current: {guidance}");
        let warning = a.warning.unwrap();
        assert!(
            warning.contains("1 commit(s) behind origin/main"),
            "warning names the distance: {warning}"
        );
        assert!(!warning.contains("current"), "no word current: {warning}");
    }

    #[test]
    fn ac2_edge_current_main_checkout_is_silent() {
        let base = tempfile::tempdir().unwrap();
        let (clone, _wt) = clone_with_worktree(&base.path().join("a5"));
        let cli = std::path::Path::new(&clone)
            .join("cli")
            .to_string_lossy()
            .into_owned();
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![cli],
        });
        assert_eq!(a.decision, Decision::Allow);
        assert_eq!(a.behind, None);
        assert_eq!(a.guidance, None);
        assert_eq!(a.warning, None);
    }

    #[test]
    fn ac3_edge_divergent_guidance_wraps_refusal() {
        let base = tempfile::tempdir().unwrap();
        let (_clone, wt) = clone_with_worktree(&base.path().join("e4"));
        fs::write(std::path::Path::new(&wt).join("d.txt"), "x\n").unwrap();
        git_in(std::path::Path::new(&wt), &["add", "-A"]);
        git_in(
            std::path::Path::new(&wt),
            &["commit", "-q", "-m", "diverge"],
        );
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![wt],
        });
        assert_eq!(a.decision, Decision::Refuse);
        assert_eq!(a.behind, None);
        assert_eq!(
            a.guidance.unwrap(),
            format!("update blocked: {}", a.refusal.unwrap())
        );
    }

    #[test]
    fn record_rejects_payload_without_path() {
        let base = tempfile::tempdir().unwrap();
        let c = base.path().join("source-path");
        let m = base.path().join("pin.json");
        let err = record_from_str("{}", c.to_str().unwrap(), m.to_str().unwrap());
        assert!(err.is_err());
    }

    /// The resolve parser accepts the parity flag, and a no-value flag must
    /// not swallow the token after it.
    #[test]
    fn resolve_parse_accepts_both_json_spellings() {
        let p = parse_resolve_args(&["--cache".to_string(), "c.json".to_string()]).unwrap();
        assert_eq!(p.cache.as_deref(), Some("c.json"));

        let short = parse_resolve_args(&[
            "-J".to_string(),
            "--cache".to_string(),
            "c.json".to_string(),
        ])
        .unwrap();
        assert_eq!(short.cache.as_deref(), Some("c.json"));

        let long = parse_resolve_args(&[
            "--json".to_string(),
            "--cache".to_string(),
            "c.json".to_string(),
        ])
        .unwrap();
        assert_eq!(long.cache.as_deref(), Some("c.json"));

        let err = match parse_resolve_args(&["--bogus".to_string()]) {
            Ok(_) => panic!("bogus flag parsed"),
            Err(e) => e,
        };
        assert!(err.contains("unknown flag"), "err was {err}");
    }

    /// A main checkout whose main merged a local branch origin never merged
    /// (the 2026-09-19 incident shape): origin/main pinned at the base commit,
    /// then a --no-ff merge of a side branch. cli/ stays untracked throughout.
    fn divergent_main_checkout(root: &std::path::Path) -> String {
        new_repo(root);
        let cli = add_cli(root);
        git_in(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_in(root, &["checkout", "-q", "-b", "side"]);
        fs::write(root.join("side.txt"), "x\n").unwrap();
        git_in(root, &["add", "side.txt"]);
        git_in(root, &["commit", "-q", "-m", "side"]);
        git_in(root, &["checkout", "-q", "main"]);
        git_in(root, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
        cli
    }

    #[test]
    fn main_checkout_merged_unmerged_branch_refuses() {
        let base = tempfile::tempdir().unwrap();
        let cli = divergent_main_checkout(&base.path().join("m1"));
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![cli.clone()],
        });
        assert_eq!(a.decision, Decision::Refuse);
        assert_eq!(a.eligibility, "divergent");
        assert_eq!(a.worktree_kind, Some(WorktreeKind::MainCheckout));
        assert_eq!(a.ancestor, Some(false));
        let refusal = a.refusal.unwrap();
        assert!(refusal.contains(&cli), "refusal names the path: {refusal}");
        assert!(
            refusal.contains("main checkout"),
            "names the kind: {refusal}"
        );
        assert!(
            refusal.contains("not an ancestor"),
            "names the divergence: {refusal}"
        );
        assert!(
            refusal.contains("origin/main"),
            "names the remote ref: {refusal}"
        );
        assert!(
            refusal.contains("--source"),
            "names the override: {refusal}"
        );
        let guidance = a.guidance.unwrap();
        assert!(
            guidance.starts_with("update blocked:"),
            "guidance leads with the block: {guidance}"
        );
    }

    #[test]
    fn main_checkout_divergent_explicit_warns_and_allows() {
        let base = tempfile::tempdir().unwrap();
        let cli = divergent_main_checkout(&base.path().join("m2"));
        let a = resolve(&ResolveArgs {
            override_path: Some(cli),
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![],
        });
        assert_eq!(a.decision, Decision::Allow);
        assert_eq!(a.eligibility, "divergent");
        let warning = a.warning.unwrap();
        assert!(
            warning.contains("divergent main checkout"),
            "warning names a divergent main checkout: {warning}"
        );
    }

    #[test]
    fn main_checkout_without_remote_ref_keeps_allow() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("m3");
        new_repo(&root);
        let cli = add_cli(&root);
        git_in(&root, &["commit", "-q", "--allow-empty", "-m", "extra"]);
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: None,
            candidate_paths: vec![cli],
        });
        assert_eq!(a.decision, Decision::Allow);
        assert_eq!(a.eligibility, "eligible");
        assert_eq!(a.ancestor, None);
        assert_eq!(a.refusal, None);
        assert_eq!(a.detail, None);
    }

    #[test]
    fn checkout_outranks_dangling_cache() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repo");
        new_repo(&root);
        let cli = add_cli(&root);
        let cache = base.path().join("gone").join("source-path");
        std::fs::create_dir_all(base.path().join("gone")).unwrap();
        std::fs::write(
            &cache,
            base.path()
                .join("noped")
                .join("cli")
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: Some(cli.clone()),
            cache: Some(cache.to_string_lossy().into_owned()),
            candidate_paths: vec![],
        });
        assert_eq!(a.decision, Decision::Allow);
        assert_eq!(a.origin.as_deref(), Some("checkout"));
        assert_eq!(a.path, Some(cli.clone()));

        // Today's failure, pinned: no checkout bucket and the same dangling
        // cache answers no_source.
        let b = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: None,
            cache: Some(cache.to_string_lossy().into_owned()),
            candidate_paths: vec![],
        });
        assert_eq!(b.eligibility, "no_source");
        assert!(
            b.refusal.unwrap().contains("inside an fno checkout"),
            "refusal names the new remedy"
        );
    }

    #[test]
    fn checkout_outranks_live_worktree_cache() {
        let base = tempfile::tempdir().unwrap();
        let (clone, wt) = clone_with_worktree(&base.path().join("w1"));
        let cache = base.path().join("w1").join("cache").join("source-path");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, &wt).unwrap();
        let clone_cli = std::path::Path::new(&clone)
            .join("cli")
            .to_string_lossy()
            .into_owned();
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: Some(clone_cli.clone()),
            cache: Some(cache.to_string_lossy().into_owned()),
            candidate_paths: vec![],
        });
        assert_eq!(a.origin.as_deref(), Some("checkout"));
        assert_eq!(a.path, Some(clone_cli));
        assert_eq!(a.worktree_kind, Some(WorktreeKind::MainCheckout));
    }

    #[test]
    fn divergent_checkout_refuses_without_cache_fallback() {
        let base = tempfile::tempdir().unwrap();
        let (clone, wt) = clone_with_worktree(&base.path().join("w2"));
        // The clone's main commits past origin/main: HEAD is no longer an
        // ancestor. The worktree cache entry stays eligible throughout.
        fs::write(std::path::Path::new(&clone).join("new.txt"), "x\n").unwrap();
        git_in(std::path::Path::new(&clone), &["add", "new.txt"]);
        git_in(
            std::path::Path::new(&clone),
            &["commit", "-q", "-m", "ahead"],
        );
        let cache = base.path().join("w2").join("cache").join("source-path");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, &wt).unwrap();
        let clone_cli = std::path::Path::new(&clone)
            .join("cli")
            .to_string_lossy()
            .into_owned();
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            checkout: Some(clone_cli.clone()),
            cache: Some(cache.to_string_lossy().into_owned()),
            candidate_paths: vec![],
        });
        assert_eq!(a.decision, Decision::Refuse);
        assert_eq!(a.origin.as_deref(), Some("checkout"));
        assert_eq!(a.eligibility, "divergent");
        assert_eq!(a.path, Some(clone_cli));
    }

    #[test]
    fn env_outranks_checkout() {
        let base = tempfile::tempdir().unwrap();
        let e_root = base.path().join("e");
        let c_root = base.path().join("c");
        new_repo(&e_root);
        new_repo(&c_root);
        add_cli(&e_root);
        let c_cli = add_cli(&c_root);
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: Some(e_root.join("cli").to_string_lossy().into_owned()),
            checkout: Some(c_cli),
            cache: None,
            candidate_paths: vec![],
        });
        assert_eq!(a.origin, Some("env".to_string()));
        assert_eq!(
            a.path,
            Some(e_root.join("cli").to_string_lossy().into_owned())
        );
    }

    #[test]
    fn non_fno_checkout_falls_through_to_cache() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repo");
        new_repo(&root);
        let cli = add_cli(&root);
        let cache = base.path().join("cache").join("source-path");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, &cli).unwrap();
        let a = resolve(&ResolveArgs {
            override_path: None,
            env_source: None,
            // A cwd whose repo has no fno cli/: the bucket skips and the
            // cache answers as before.
            checkout: Some(
                base.path()
                    .join("other")
                    .join("cli")
                    .to_string_lossy()
                    .into_owned(),
            ),
            cache: Some(cache.to_string_lossy().into_owned()),
            candidate_paths: vec![],
        });
        assert_eq!(a.origin, Some("cache".to_string()));
        assert_eq!(a.path, Some(cli));
    }
}
