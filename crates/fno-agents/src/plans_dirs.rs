//! `fno-agents state plans-dirs [cwd]` - every REGISTERED project's plans dir,
//! one per line, for the plan-location guard. A session anchored outside a
//! project (a king in `~/.fno`, a subagent in an unrelated checkout) still
//! writes that project's plans, so the guard must judge a target against the
//! plans dirs of every project in `work.workspaces`, not only the one its own
//! cwd resolves to.
//!
//! Rust-owned like `escalations`: nothing in Python reads this list, so there
//! is no Python accessor to mirror. The per-project resolution itself is NOT
//! reimplemented here - each project root is probed with the real
//! `fno do plan path --slug _plans_dir_probe` (cwd anchored at the root), so
//! the `plansDirectory -> config.plans_dir` chain keeps exactly one owner. The
//! probe slug never touches disk. A probe may migrate a legacy `<root>/.fno/plans`
//! onto the project's space; that is the verb's own designed behavior, and the
//! session-cwd probe the guard already runs does the same thing today.
//!
//! Probes are one Python CLI startup each, so the set is cached under
//! `<state_dir>/cache/plans-dirs-v1.txt`, keyed on a blake3 stamp over the
//! project list and every config file the chain reads. A stamp miss
//! recomputes; a cache read failure just recomputes. Callers (the shell
//! helper) treat empty output, a missing binary, and an unknown subcommand
//! identically: fewer accepted dirs, never a wrong one.

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
        Some(p) if Path::new(p).is_dir() => PathBuf::from(p),
        _ => process_cwd,
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

/// One anchored probe: the plans dir the real resolver names for `root`.
fn probe(root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("fno")
        .args(["do", "plan", "path", "--slug", "_plans_dir_probe"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().rev().find(|l| l.starts_with('/'))?;
    let dir = PathBuf::from(line).parent()?.to_path_buf();
    // A `/` plans dir would accept every path; the resolver never names one.
    if dir.parent().is_none() {
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Pins FNO_CONFIG, PATH, and the cache dir for one test. FNO_CONFIG
    /// replaces the whole config candidate list, so a planted file is the only
    /// config this process (and `workspace_paths`) can read.
    struct EnvGuard {
        saved_config: Option<std::ffi::OsString>,
        saved_path: Option<std::ffi::OsString>,
        saved_cache: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new(config: &Path, fake_bin: &Path, cache: &Path) -> Self {
            let saved_config = std::env::var_os("FNO_CONFIG");
            let saved_path = std::env::var_os("PATH");
            let saved_cache = std::env::var_os("FNO_PLANS_DIRS_CACHE_DIR");
            std::env::set_var("FNO_CONFIG", config);
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    fake_bin.display(),
                    saved_path.as_deref().unwrap_or_default().to_string_lossy()
                ),
            );
            std::env::set_var("FNO_PLANS_DIRS_CACHE_DIR", cache);
            Self {
                saved_config,
                saved_path,
                saved_cache,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.saved_config {
                Some(v) => std::env::set_var("FNO_CONFIG", v),
                None => std::env::remove_var("FNO_CONFIG"),
            }
            match &self.saved_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            match &self.saved_cache {
                Some(v) => std::env::set_var("FNO_PLANS_DIRS_CACHE_DIR", v),
                None => std::env::remove_var("FNO_PLANS_DIRS_CACHE_DIR"),
            }
        }
    }

    struct Fixture {
        base: PathBuf,
    }

    impl Fixture {
        /// Two registered projects, each with a fake `fno` that answers the
        /// probe from its own cwd, and a project-scoped config file so the
        /// stamp has something per-project to watch.
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("fno-plans-dirs-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&base);
            for name in ["alpha", "beta"] {
                let root = base.join(name);
                fs::create_dir_all(root.join("plans")).unwrap();
                fs::create_dir_all(root.join(".fno")).unwrap();
                fs::write(root.join(".fno").join("config.toml"), "").unwrap();
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
            let fake_bin = base.join("bin");
            fs::create_dir_all(&fake_bin).unwrap();
            // Answers from its own cwd; `$PWD` comes back in the physical
            // form (`/private/var/...` on macOS), which the expectations
            // below must match.
            crate::write_exec_stub(
                &fake_bin,
                "fno",
                "#!/bin/sh\necho \"$PWD/plans/20260101-x.md\"\n",
            );
            fs::create_dir_all(base.join("cache")).unwrap();
            Fixture { base }
        }

        fn config(&self) -> PathBuf {
            self.base.join("work.toml")
        }

        fn fake_bin(&self) -> PathBuf {
            self.base.join("bin")
        }

        fn cache(&self) -> PathBuf {
            self.base.join("cache")
        }

        fn probe_count(&self) -> usize {
            // The fake fno appends one byte to this file per run.
            fs::read_to_string(self.fake_bin().join("count"))
                .unwrap_or_default()
                .len()
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

    fn guard_for<'a>(fx: &'a Fixture) -> EnvGuard {
        EnvGuard::new(&fx.config(), &fx.fake_bin(), &fx.cache())
    }

    fn count_script() -> &'static str {
        // Appends one byte to $0.dir/count so tests can count real probe
        // invocations.
        "#!/bin/sh\nprintf 'x' >> \"$(dirname \"$0\")/count\"\necho \"$PWD/plans/20260101-x.md\"\n"
    }

    #[test]
    fn plans_dirs_probe_every_registered_project() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("enumerate");
        let _env = guard_for(&fx);

        let dirs = plans_dirs(&fx.base);
        // The probe answers in `$PWD`'s physical form, so the expectation is
        // canonicalized to the same namespace.
        let want = vec![
            fs::canonicalize(fx.base.join("alpha"))
                .unwrap()
                .join("plans"),
            fs::canonicalize(fx.base.join("beta"))
                .unwrap()
                .join("plans"),
        ];
        assert_eq!(dirs, want, "one sorted dir per registered project");
    }

    #[test]
    fn plans_dirs_cache_hit_skips_probes_until_stamp_moves() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("cache");
        crate::write_exec_stub(&fx.fake_bin(), "fno", &count_script());
        let _env = guard_for(&fx);

        let _ = plans_dirs(&fx.base);
        let after_first = fx.probe_count();
        assert_eq!(after_first, 2, "one probe per project");

        let _ = plans_dirs(&fx.base);
        assert_eq!(fx.probe_count(), after_first, "cache hit spawns nothing");

        // A per-project config touch moves the stamp and forces a re-probe.
        let path = fx.base.join("alpha").join(".fno").join("config.toml");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o200);
        fs::set_permissions(&path, perms).unwrap();
        fs::write(&path, "plans_dir = \".fno/plans/\"\n").unwrap();
        let now = std::time::SystemTime::now();
        let future = now + std::time::Duration::from_secs(5);
        set_mtime(&path, future);

        let _ = plans_dirs(&fx.base);
        assert_eq!(fx.probe_count(), after_first + 2, "stamp miss re-probes");
    }

    #[test]
    fn plans_dirs_skip_failed_and_non_absolute_probes() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("skip");
        let _env = guard_for(&fx);

        // The fake fno exits 1 for the beta project, so only alpha's dir
        // comes back.
        crate::write_exec_stub(
            &fx.fake_bin(),
            "fno",
            "#!/bin/sh\ncase \"$PWD\" in */beta) exit 1;; esac\necho \"$PWD/plans/20260101-x.md\"\n",
        );
        let dirs = plans_dirs(&fx.base);
        assert_eq!(
            dirs,
            vec![fs::canonicalize(fx.base.join("alpha"))
                .unwrap()
                .join("plans")]
        );

        // A probe that answers a relative line contributes nothing. The stamp
        // moves first, or the cache from phase 1 answers and no probe runs.
        let path = fx.base.join("alpha").join(".fno").join("config.toml");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o200);
        fs::set_permissions(&path, perms).unwrap();
        fs::write(&path, "plans_dir = \".fno/plans/\"\n").unwrap();
        let now = std::time::SystemTime::now();
        set_mtime(&path, now + std::time::Duration::from_secs(5));
        crate::write_exec_stub(
            &fx.fake_bin(),
            "fno",
            "#!/bin/sh\necho \"plans/20260101-x.md\"\n",
        );
        let dirs = plans_dirs(&fx.base);
        assert!(dirs.is_empty(), "relative probe lines are not dirs");
    }

    fn set_mtime(path: &Path, at: std::time::SystemTime) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(at).unwrap();
    }
}
