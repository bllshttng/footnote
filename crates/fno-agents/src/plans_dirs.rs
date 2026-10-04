//! `fno-agents state plans-dirs [cwd]` - every REGISTERED project's plans dir,
//! one per line, for the plan-location guard. A session anchored outside a
//! project (a lead in `~/.fno`, a subagent in an unrelated checkout) still
//! writes that project's plans, so the guard must judge a target against the
//! plans dirs of every project in `work.workspaces`, not only the one its own
//! cwd resolves to.
//!
//! Rust-owned like `escalations`: nothing in Python reads this list, so there
//! is no Python accessor to mirror. The per-project resolution is the
//! `plans_path` chain (the ported `plansDirectory -> plans_dir` owner), run
//! in-process per project root - the shell-out to `fno do plan path` this
//! module once paid per project is gone, and the probe slug never touches
//! disk. A probe may migrate a legacy `<root>/.fno/plans` onto the project's
//! space; that is the chain's own designed behavior, and the session-cwd
//! probe the guard already runs does the same thing today.
//!
//! The set is cached under `<state_dir>/cache/plans-dirs-v1.txt`, keyed on a
//! blake3 stamp over the project list and every config file the chain reads.
//! A stamp miss recomputes; a cache read failure just recomputes. Callers
//! (the shell helper) treat empty output identically: fewer accepted dirs,
//! never a wrong one.

use std::path::{Path, PathBuf};

use crate::agents_config::state_dir;
use crate::territory::workspace_paths;

/// Per-project files the `plansDirectory -> plans_dir` chain reads. A change
/// to any of them must invalidate the cached set.
const PROBE_CONFIG_FILES: &[&str] = &[
    ".claude/settings.local.json",
    ".claude/settings.json",
    ".fno/config.toml",
    ".fno/settings.yaml",
];

const GLOBAL_CONFIG_FILES: &[&str] = &[".fno/config.toml", ".fno/settings.yaml"];

pub fn run(args: &[String]) -> i32 {
    let process_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // The caller anchors resolution at the SESSION cwd it resolved from its
    // payload, not wherever the hook process happens to run: a local
    // `.fno/config.toml` with a `[work]` table must win for that session,
    // exactly as it does for every other config read.
    let anchor = match args.first() {
        // An explicit anchor wins even when its directory is gone: the guard
        // passes a session cwd whose checkout may already be reaped, and a
        // silent process-cwd fallback reads the wrong session's [work] table.
        Some(p) => PathBuf::from(p),
        None => process_cwd,
    };
    for dir in plans_dirs(&anchor) {
        println!("{}", dir.display());
    }
    0
}

/// The accepted plans dirs for `anchor`: one per distinct registered project
/// root, sorted, deduplicated. Probe failures contribute nothing.
pub(crate) fn plans_dirs(anchor: &Path) -> Vec<PathBuf> {
    let roots = project_roots(anchor);
    if roots.is_empty() {
        return Vec::new();
    }
    let stamp = config_stamp(&roots);
    let cache = cache_path(anchor);
    if let Some(cached) = read_cache(&cache, &stamp) {
        return cached;
    }
    let dirs = probe_all(&roots);
    write_cache(&cache, &stamp, &dirs);
    dirs
}

fn project_roots(anchor: &Path) -> Vec<PathBuf> {
    let mut out: Vec<String> = workspace_paths(anchor)
        .into_values()
        .map(normalize_root)
        .collect();
    out.sort();
    out.dedup();
    out.into_iter().map(PathBuf::from).collect()
}

/// `~`-expansion for roots that `workspace_paths::normalize_path` left
/// tilde-led (no HOME at read time, or a bare `~`).
fn normalize_root(raw: String) -> String {
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = raw.strip_prefix('~').unwrap().trim_start_matches('/');
            let joined = if rest.is_empty() {
                home.to_string_lossy().into_owned()
            } else {
                format!("{}/{}", home.to_string_lossy(), rest)
            };
            return joined;
        }
    }
    raw
}

fn probe_all(roots: &[PathBuf]) -> Vec<PathBuf> {
    let handles: Vec<_> = roots
        .iter()
        .map(|root| {
            let root = root.clone();
            std::thread::spawn(move || probe(&root))
        })
        .collect();
    let mut out = Vec::new();
    for handle in handles {
        if let Ok(Some(dir)) = handle.join() {
            out.push(dir);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// One anchored probe: the plans dir the chain names for `root`.
fn probe(root: &Path) -> Option<PathBuf> {
    let dir = crate::plans_path::plans_content_dir(root)?;
    // Same shape the old verb probe enforced: absolute, never the root.
    if !dir.is_absolute() || dir.parent().is_none() {
        return None;
    }
    Some(dir)
}

/// blake3 over the project list plus every config file the chain reads
/// (per-project and global). Covers the chain's real inputs; anything it
/// misses only costs a recompute, never a stale accept.
fn config_stamp(roots: &[PathBuf]) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut feed = |s: &str| {
        hasher.update(s.as_bytes());
    };
    for root in roots {
        feed(root.to_string_lossy().as_ref());
        feed("\n");
    }
    for root in roots {
        for rel in PROBE_CONFIG_FILES {
            feed(&file_stamp(&root.join(rel)));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        for rel in GLOBAL_CONFIG_FILES {
            feed(&file_stamp(&PathBuf::from(&home).join(rel)));
        }
    }
    if let Some(explicit) = std::env::var_os("FNO_CONFIG") {
        feed(&file_stamp(&PathBuf::from(&explicit)));
    }
    hasher.finalize().to_hex().to_string()
}

fn file_stamp(path: &Path) -> String {
    let meta = std::fs::metadata(path);
    let mtime = meta
        .as_ref()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());
    let len = meta
        .as_ref()
        .ok()
        .map(|m| m.len().to_string())
        .unwrap_or_else(|| "-".to_string());
    format!("{}\t{}\t{}\n", path.display(), mtime, len)
}

fn cache_path(anchor: &Path) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FNO_PLANS_DIRS_CACHE_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("plans-dirs-v1.txt"));
        }
    }
    Some(state_dir(anchor)?.join("cache").join("plans-dirs-v1.txt"))
}

fn read_cache(path: &Option<PathBuf>, stamp: &str) -> Option<Vec<PathBuf>> {
    let path = path.as_ref()?;
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    if lines.next()? != stamp {
        return None;
    }
    let dirs: Vec<PathBuf> = lines
        .filter_map(|l| {
            let p = PathBuf::from(l);
            // Same shape the probe enforces: absolute, never the root.
            (l.starts_with('/') && p.parent().is_some()).then_some(p)
        })
        .collect();
    Some(dirs)
}

fn write_cache(path: &Option<PathBuf>, stamp: &str, dirs: &[PathBuf]) {
    let Some(path) = path.as_ref() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let mut body = format!("{stamp}\n");
    for dir in dirs {
        body.push_str(&dir.to_string_lossy());
        body.push('\n');
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::test_env_lock;
    use crate::paths::space_slug;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Pins config, state roots, and the cache dir for one test. FNO_CONFIG
    /// replaces the whole config candidate list, so a planted file is the only
    /// config this process (and `workspace_paths`) can read.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new(pins: &[(&'static str, String)]) -> Self {
            let saved = pins
                .iter()
                .map(|(k, _)| (*k, std::env::var_os(k)))
                .collect();
            for (k, v) in pins {
                std::env::set_var(k, v);
            }
            EnvGuard { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(old) => std::env::set_var(k, old),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    struct Fixture {
        base: PathBuf,
    }

    impl Fixture {
        /// Two registered git repos, each with its own canonical root (the
        /// default plans dir is space-keyed), plus the config registering
        /// both.
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("fno-plans-dirs-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&base);
            for name in ["alpha", "beta"] {
                let root = base.join(name);
                fs::create_dir_all(root.join(".fno")).unwrap();
                fs::write(root.join(".fno").join("config.toml"), "").unwrap();
                git_init(&root);
            }
            let config = base.join("work.toml");
            fs::write(
                &config,
                format!(
                    "[[work.workspaces.main.projects]]\nname = \"alpha\"\npath = \"{}\"\n[[work.workspaces.main.projects]]\nname = \"beta\"\npath = \"{}\"\n",
                    base.join("alpha").display_str(),
                    base.join("beta").display_str(),
                ),
            )
            .unwrap();
            fs::create_dir_all(base.join("cache")).unwrap();
            Fixture { base }
        }

        fn config(&self) -> PathBuf {
            self.base.join("work.toml")
        }

        fn cache(&self) -> PathBuf {
            self.base.join("cache")
        }

        fn pins(&self) -> Vec<(&'static str, String)> {
            vec![
                ("FNO_CONFIG", self.config().display().to_string()),
                (
                    "FNO_STATE_DIR",
                    self.base.join("state").display().to_string(),
                ),
                (
                    "FNO_SPACES_DIR",
                    self.base.join("spaces").display().to_string(),
                ),
                (
                    "FNO_PLANS_DIRS_CACHE_DIR",
                    self.cache().display().to_string(),
                ),
            ]
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    trait DisplayStr {
        fn display_str(&self) -> String;
    }
    impl DisplayStr for PathBuf {
        fn display_str(&self) -> String {
            self.display().to_string()
        }
    }

    fn git_init(dir: &Path) {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .status()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    fn guard_for(fx: &Fixture) -> EnvGuard {
        EnvGuard::new(&fx.pins())
    }

    /// The plans dir the default chain names for one fixture repo: the
    /// pinned spaces root, keyed on the repo's canonical slug.
    fn expected_space_plans(base: &Path, repo: &str) -> PathBuf {
        let slug = space_slug(&fs::canonicalize(base.join(repo)).unwrap());
        fs::canonicalize(base)
            .unwrap()
            .join("spaces")
            .join(slug)
            .join("plans")
    }

    fn set_mtime(path: &Path, at: std::time::SystemTime) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(at).unwrap();
    }

    #[test]
    fn plans_dirs_probe_every_registered_project() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("enumerate");
        let _env = guard_for(&fx);

        let dirs = plans_dirs(&fx.base);
        let want = vec![
            expected_space_plans(&fx.base, "alpha"),
            expected_space_plans(&fx.base, "beta"),
        ];
        assert_eq!(dirs, want, "one sorted dir per registered project");
    }

    #[test]
    fn plans_dirs_cache_hit_skips_probes_until_stamp_moves() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("cache");
        let _env = guard_for(&fx);

        let real = plans_dirs(&fx.base);
        assert_eq!(real.len(), 2, "one dir per project");

        // Poison the cached dirs under the live stamp: a cache hit answers
        // with the poison, proving no recompute ran.
        let cache_file = fx.cache().join("plans-dirs-v1.txt");
        let content = fs::read_to_string(&cache_file).unwrap();
        let stamp = content.lines().next().unwrap().to_string();
        fs::write(
            &cache_file,
            format!("{stamp}\n/cache/canary-a\n/cache/canary-b\n"),
        )
        .unwrap();
        let dirs = plans_dirs(&fx.base);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/cache/canary-a"),
                PathBuf::from("/cache/canary-b")
            ],
            "cache hit skips the probes"
        );

        // A per-project config touch moves the stamp and forces a re-probe.
        let path = fx.base.join("alpha").join(".fno").join("config.toml");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o200);
        fs::set_permissions(&path, perms).unwrap();
        fs::write(&path, "plans_dir = \".fno/plans/\"\n").unwrap();
        let now = std::time::SystemTime::now();
        set_mtime(&path, now + std::time::Duration::from_secs(5));

        let dirs = plans_dirs(&fx.base);
        assert_eq!(
            dirs,
            vec![
                expected_space_plans(&fx.base, "alpha"),
                expected_space_plans(&fx.base, "beta")
            ],
            "stamp miss re-probes"
        );
    }

    #[test]
    fn plans_dirs_skip_failed_probes() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("skip");
        let _env = guard_for(&fx);

        // A chain error ({vault} with no obsidian block) makes the probe
        // contribute nothing; here it fails for both registered projects.
        fs::write(fx.config(), "plans_dir = \"{vault}/plans\"\n").unwrap();
        let dirs = plans_dirs(&fx.base);
        assert!(dirs.is_empty(), "failed probes are not dirs");
    }
}
