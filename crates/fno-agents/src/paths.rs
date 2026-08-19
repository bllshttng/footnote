//! `~/.fno/agents/` filesystem layout (Wave 3).
//!
//! One source of truth for every path the daemon and workers touch, so the
//! layout is changed in one place and tests can redirect the whole tree to a
//! tempdir via `FNO_AGENTS_HOME`.
//!
//! ```text
//! <home>/
//!   registry.json            registry (schema v4)
//!   events.jsonl             operator-facing audit log
//!   route-settings/          --settings floors the spawn arms write (0600,
//!                            content-addressed; read by claude, not by us)
//!   supervisor.sock          daemon's client-facing socket (mode 0600)
//!   <short_id>/
//!     state.json             per-agent state (schema v1)
//!     timeline.jsonl         model-facing transcript
//!     worker.sock            daemon<->worker channel (Outcome B)
//!   .orphaned/<short_id>-<ts>/   archived orphan state dirs
//! ```

use std::path::{Path, PathBuf};

/// Environment variable that redirects the entire agents tree (used by tests
/// and by operators who keep state outside `$HOME`).
pub const HOME_ENV: &str = "FNO_AGENTS_HOME";

/// Resolved `~/.fno/agents/` root and the paths under it.
#[derive(Debug, Clone)]
pub struct AgentsHome {
    root: PathBuf,
}

impl AgentsHome {
    /// Resolve from the environment: `FNO_AGENTS_HOME` if set, else
    /// `$HOME/.fno/agents`. Falls back to `./.fno/agents` if `$HOME`
    /// is somehow unset (CI containers), so the daemon never panics on a missing
    /// home — it degrades to a relative tree.
    pub fn from_env() -> Self {
        if let Some(v) = std::env::var_os(HOME_ENV) {
            return AgentsHome {
                root: PathBuf::from(v),
            };
        }
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        AgentsHome {
            root: base.join(".fno").join("agents"),
        }
    }

    /// Construct rooted at an explicit directory (tests).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        AgentsHome { root: root.into() }
    }

    /// The agents root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the root directory with mode 0700 (owner-only). The daemon
    /// fstat-verifies this separately; this just gets it created.
    pub fn ensure_root(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        set_dir_mode_0700(&self.root)
    }

    pub fn registry_json(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    /// Per-provider injection gate record (`injection-gate.json`), stored next
    /// to `registry.json` in the agents root.
    pub fn injection_gate_json(&self) -> PathBuf {
        self.root.join("injection-gate.json")
    }

    /// Durable roster-progress sidecar (x-cdc7 SECOND HALF): per-row git
    /// evidence (last commit sha/age, branch-ahead, PR number) keyed by row
    /// name, refreshed by the reconcile sweep alongside `registry.json`. A
    /// sidecar, not new `RegistryEntry` fields, deliberately: ~20 existing
    /// literal constructions of that struct across the crate would each need
    /// a matching field, for data no reader needs schema-version protection
    /// on (a stale reader simply sees no progress row, same as today). Keyed
    /// by `name` so a future reader (t-x9de7's `agents_view.rs` render pass)
    /// joins it the same way the registry itself is joined.
    pub fn roster_progress_json(&self) -> PathBuf {
        self.root.join("roster-progress.json")
    }

    pub fn events_jsonl(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    /// Directory of terminal-stop markers (x-fcbf). `finalize` drops one file
    /// per fire-and-forget `claude --bg` session whose loop reached a terminal
    /// decision; the daemon sweep consumes it to `claude stop` the parked
    /// worker. Filename = the claude session uuid; content = the reason.
    pub fn terminal_stop_dir(&self) -> PathBuf {
        self.root.join("terminal-stop")
    }

    /// Operator override dir for detection manifests: a readable
    /// `<provider>.toml` here beats the bundled copy
    /// (`crate::manifest::load_manifest` resolution chain).
    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }

    pub fn supervisor_sock(&self) -> PathBuf {
        self.root.join("supervisor.sock")
    }

    /// Sidecar advisory-lock file for the supervisor-socket singleton guard
    /// (x-ef7f). Held exclusively, for the daemon's whole process lifetime, by
    /// whichever process is entitled to bind `supervisor_sock()`. Since x-3498
    /// the holder also writes `<pid> <pid_start_time>` into the content at
    /// bind time; see [`supervisor_lock_holder`].
    pub fn supervisor_lock(&self) -> PathBuf {
        self.root.join("supervisor.sock.lock")
    }

    /// Per-agent directory.
    pub fn agent_dir(&self, short_id: &str) -> PathBuf {
        self.root.join(short_id)
    }

    pub fn state_json(&self, short_id: &str) -> PathBuf {
        self.agent_dir(short_id).join("state.json")
    }

    pub fn timeline_jsonl(&self, short_id: &str) -> PathBuf {
        self.agent_dir(short_id).join("timeline.jsonl")
    }

    /// Per-agent worker socket (Outcome B): the daemon reconnects here on
    /// restart, and discovers live workers by scanning for these sockets.
    pub fn worker_sock(&self, short_id: &str) -> PathBuf {
        self.agent_dir(short_id).join("worker.sock")
    }

    /// Archive directory for orphan state dirs (recovery step 2).
    pub fn orphaned_dir(&self) -> PathBuf {
        self.root.join(".orphaned")
    }

    /// Destination for archiving a specific orphan, timestamped to avoid
    /// collisions across repeated recoveries.
    pub fn orphan_archive_dest(&self, short_id: &str, ts: &str) -> PathBuf {
        self.orphaned_dir().join(format!("{short_id}-{ts}"))
    }

    /// Scan the root for per-agent directories that contain a `worker.sock`,
    /// returning their `short_id`s. Used by recovery to discover live workers
    /// independent of the registry (a worker can outlive a registry write).
    pub fn scan_worker_sockets(&self) -> Vec<String> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(n) if !n.starts_with('.') => n.to_string(),
                _ => continue,
            };
            if self.worker_sock(&name).exists() {
                out.push(name);
            }
        }
        out.sort();
        out
    }
}

/// The pid (and start time, when recorded) written into the supervisor
/// lockfile by the daemon that holds it. `None` for an absent, empty, or
/// malformed file -- and a `None` here is NEVER "no holder": the flock on the
/// lockfile is the authority on whether a holder exists; the content only
/// names one. Callers treat `None` as "no force target", not as a free socket.
pub fn supervisor_lock_holder(home: &AgentsHome) -> Option<(u32, Option<u64>)> {
    let content = std::fs::read_to_string(home.supervisor_lock()).ok()?;
    let fields: Vec<&str> = content.split_whitespace().collect();
    // Strict on purpose: this value becomes a SIGKILL target, so anything the
    // writer would not have produced (extra fields, a non-numeric token) is
    // corruption and answers None rather than a best-effort parse.
    match fields.as_slice() {
        [pid] => Some((pid.parse().ok()?, None)),
        [pid, start] => Some((pid.parse().ok()?, Some(start.parse().ok()?))),
        _ => None,
    }
}

/// Set a directory to mode 0700 (rwx for owner only), regardless of umask. The
/// daemon refuses to serve if `supervisor.sock`'s parent is not 0700 (finding
/// #6 Critical); this is the corrective write.
pub fn set_dir_mode_0700(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir, perms)?;
    }
    let _ = dir;
    Ok(())
}

/// Set a file to mode 0600 (rw for owner only), regardless of umask.
pub fn set_file_mode_0600(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}

/// True if `path` is a directory with exactly mode 0700 (low 9 bits).
#[cfg(unix)]
pub fn is_dir_mode_0700(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => (m.permissions().mode() & 0o777) == 0o700,
        _ => false,
    }
}

/// True if `path` is a file/socket with exactly mode 0600 (low 9 bits).
#[cfg(unix)]
pub fn is_file_mode_0600(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => (m.permissions().mode() & 0o777) == 0o600,
        _ => false,
    }
}

/// Resolve the canonical (main checkout) repo root from `cwd` via
/// `git worktree list --porcelain`, returning the first real working tree (the
/// main checkout; git lists it first). This is robust across the layouts the
/// `--git-common-dir` parent gets wrong: a bare repo (no working tree) and a
/// `--separate-git-dir` mis-report both resolve to None here, so callers fall
/// back to the caller cwd -- the safe side (Failure Modes > Boundaries). From a
/// linked worktree this returns the main checkout; from the main checkout it
/// returns itself (so a `--fresh`-style redirect is a no-op there). Mirrors the
/// Python `resolve_canonical_worktree` in `cli/src/fno/paths.py` so the two
/// layers cannot drift (ab-77b691dc; review HIGH). Shared by the client `--fresh`
/// dispatch (`bin/client.rs`).
///
/// A THIRD implementation of this question lives in `fno::digest_overlay`, which
/// crate `fno` needs because it does not depend on `fno-agents`. It parses the
/// linked worktree's `.git` file instead of shelling out, so it costs no
/// subprocess on the mux attach path and, unlike this one, cannot see a repo
/// whose git dir lives outside the checkout. Change one and check the other.
pub fn canonical_repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    // Porcelain records are blank-line separated; each starts with
    // `worktree <path>` followed by attribute lines (`bare`, `HEAD`, ...). The
    // FIRST non-bare record is the main worktree. A real working tree has a `.git`
    // child; a `--separate-git-dir` mis-report (the external git dir listed as the
    // main worktree) does not, so it returns None rather than rooting under an
    // arbitrary git dir -- the same first-non-bare-decides rule as the Python side.
    for record in stdout.split("\n\n") {
        let mut lines = record.lines();
        let first = match lines.next() {
            Some(l) => l,
            None => continue,
        };
        let path_str = match first.strip_prefix("worktree ") {
            Some(p) => p.trim(),
            None => continue,
        };
        // `bare` appears as its own attribute line under the worktree path.
        if lines.any(|l| l.trim() == "bare") {
            continue;
        }
        if path_str.is_empty() {
            return None;
        }
        let candidate = Path::new(path_str);
        if candidate.join(".git").exists() {
            return Some(
                std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf()),
            );
        }
        return None;
    }
    None
}

/// Resolve the current worktree root from any path inside it.
///
/// Unlike [`canonical_repo_root`], this preserves linked-worktree isolation.
/// A non-git path falls back to the caller path so journal construction remains
/// available in tests and degraded environments.
pub fn worktree_repo_root(cwd: &Path) -> PathBuf {
    if cwd.join(".git").exists() {
        return std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    }
    let resolved = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_string())
        .filter(|root| !root.is_empty())
        .map(PathBuf::from);
    resolved
        .and_then(|root| std::fs::canonicalize(&root).ok().or(Some(root)))
        .unwrap_or_else(|| cwd.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-paths-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn layout_paths_are_under_root() {
        let root = tmp("layout");
        let home = AgentsHome::at(&root);
        assert_eq!(home.registry_json(), root.join("registry.json"));
        assert_eq!(home.supervisor_sock(), root.join("supervisor.sock"));
        assert_eq!(home.state_json("wkA"), root.join("wkA/state.json"));
        assert_eq!(home.worker_sock("wkA"), root.join("wkA/worker.sock"));
        assert!(home
            .orphan_archive_dest("wkA", "20260524T000000Z")
            .starts_with(root.join(".orphaned")));
    }

    #[test]
    fn ensure_root_creates_0700_dir() {
        let root = tmp("perms");
        let home = AgentsHome::at(&root);
        home.ensure_root().unwrap();
        assert!(root.is_dir());
        #[cfg(unix)]
        assert!(is_dir_mode_0700(&root));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_worker_sockets_finds_only_dirs_with_sockets() {
        let root = tmp("scan");
        let home = AgentsHome::at(&root);
        home.ensure_root().unwrap();
        // wkA has a worker.sock; wkB does not; .orphaned is skipped.
        std::fs::create_dir_all(home.agent_dir("wkA")).unwrap();
        std::fs::write(home.worker_sock("wkA"), b"").unwrap();
        std::fs::create_dir_all(home.agent_dir("wkB")).unwrap();
        std::fs::create_dir_all(home.orphaned_dir()).unwrap();

        let found = home.scan_worker_sockets();
        assert_eq!(found, vec!["wkA".to_string()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn from_env_honors_override() {
        let root = tmp("env");
        std::env::set_var(HOME_ENV, &root);
        let home = AgentsHome::from_env();
        assert_eq!(home.root(), root.as_path());
        std::env::remove_var(HOME_ENV);
    }

    // ── supervisor_lock_holder (x-3498) ─────────────────────────────────────

    #[test]
    fn supervisor_lock_holder_parses_pid_and_start_time() {
        let root = tmp("holder");
        let home = AgentsHome::at(&root);
        home.ensure_root().unwrap();
        std::fs::write(home.supervisor_lock(), b"4242 1700000000000\n").unwrap();
        assert_eq!(
            supervisor_lock_holder(&home),
            Some((4242, Some(1_700_000_000_000)))
        );
        // pid alone (a platform that could not read the start time).
        std::fs::write(home.supervisor_lock(), b"4242\n").unwrap();
        assert_eq!(supervisor_lock_holder(&home), Some((4242, None)));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn supervisor_lock_holder_garbage_is_none_never_a_guess() {
        let root = tmp("holder-garbage");
        let home = AgentsHome::at(&root);
        home.ensure_root().unwrap();
        for garbage in [
            "",
            "garbage\n",
            "-1 5\n",
            "99999999999 5\n",
            "12 13 14 extra\n",
        ] {
            std::fs::write(home.supervisor_lock(), garbage).unwrap();
            assert_eq!(
                supervisor_lock_holder(&home),
                None,
                "content {garbage:?} must answer None (no target), never a parsed guess"
            );
        }
        // Absent file, same answer.
        std::fs::remove_file(home.supervisor_lock()).unwrap();
        assert_eq!(supervisor_lock_holder(&home), None);
        std::fs::remove_dir_all(&root).ok();
    }

    // ── canonical_repo_root (ab-77b691dc) ────────────────────────────────────

    #[test]
    fn canonical_repo_root_none_outside_git() {
        // A non-git directory yields None so callers keep the caller cwd.
        let dir = tmp("nogit");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(canonical_repo_root(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worktree_repo_root_resolves_from_subdirectory() {
        fn git(dir: &Path, args: &[&str]) -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        }
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let repo = tmp("repo-root");
        std::fs::create_dir_all(repo.join("nested/source")).unwrap();
        assert!(git(&repo, &["init", "-q"]));

        let resolved = worktree_repo_root(&repo.join("nested/source"));
        assert_eq!(resolved, std::fs::canonicalize(&repo).unwrap());
        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn canonical_repo_root_resolves_main_from_linked_worktree() {
        // The core failure mode: resolution must give the MAIN checkout, not the
        // linked worktree, via git-common-dir (athens/milan-style fixture).
        fn git(dir: &Path, args: &[&str]) -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        // Skip when git is unavailable (mirrors the Python skipif(no git)).
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = tmp("wt");
        let main = base.join("main");
        std::fs::create_dir_all(&main).unwrap();
        assert!(git(&main, &["init", "-q"]));
        assert!(git(&main, &["config", "user.email", "t@t"]));
        assert!(git(&main, &["config", "user.name", "t"]));
        assert!(git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]));
        let linked = base.join("wt");
        assert!(git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().unwrap(),
                "-b",
                "feat"
            ]
        ));

        let want = std::fs::canonicalize(&main).unwrap();
        // From the linked worktree -> canonical main.
        assert_eq!(canonical_repo_root(&linked).unwrap(), want);
        // From the main checkout itself -> the same root (no-op redirect).
        assert_eq!(canonical_repo_root(&main).unwrap(), want);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn canonical_repo_root_none_for_bare_repo() {
        // A bare repo reports a git dir whose basename is not `.git`; resolution
        // must return None (caller-cwd fallback), never a wrong parent. Same
        // guard protects the --separate-git-dir layout (review HIGH 1).
        fn git(dir: &Path, args: &[&str]) -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = tmp("bare");
        std::fs::create_dir_all(&base).unwrap();
        let bare = base.join("repo.git");
        assert!(git(
            &base,
            &["init", "--bare", "-q", bare.to_str().unwrap()]
        ));
        assert!(
            canonical_repo_root(&bare).is_none(),
            "a bare repo must resolve to None (safe-side fallback), not a wrong parent"
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
