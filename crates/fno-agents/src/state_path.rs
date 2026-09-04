//! `fno-agents state path <name>` -- the shell hook surface for project-space
//! path resolution, mirroring the accessor table in Python `fno.paths`. Shell
//! hooks call this instead of spelling `<repo>/.fno/<file>`, so the location
//! has one owner per language and the two agree through the slug contract in
//! `paths::space_slug`. Binary-direct (no daemon), resolved from the process
//! cwd.

use std::path::PathBuf;

use crate::paths::{space_dir, worktree_repo_root, worktree_space_dir};

/// Print the resolved space path for one named state file. Unknown names
/// exit 2 with the known set, so a stale hook fails loud instead of writing
/// through a guessed path.
pub fn run(args: &[String]) -> i32 {
    // `state path <name>` and `state <name>` both work; the spelled-out
    // "path" matches how the hooks and docs say it.
    let args = if args.first().map(|a| a.as_str() == "path").unwrap_or(false) {
        &args[1..]
    } else {
        args
    };
    let Some(name) = args.first() else {
        eprintln!("usage: fno-agents state path <target-state|run-log|events|plans|inbox|kings|scratchpad|status-sinks|worktree-log|codemap>");
        return 2;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Canonicalize so the slug matches what a caller passing the canonical
    // path (e.g. loop-check's resolved --cwd) hashes: /var vs /private/var
    // must never mint two spaces.
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let Some(path) = resolve(name, &cwd) else {
        eprintln!(
            "error: unknown state path {name} (known: codemap, events, inbox, kings, plans, run-log, scratchpad, status-sinks, target-state, worktree-log)"
        );
        return 2;
    };
    println!("{}", path.display());
    0
}

/// The name-to-path table, mirroring Python `fno do state path`'s accessors
/// (which the hooks previously called). `events` honors `FNO_EVENTS_PATH` and
/// migrates the legacy checkout journal so a hook append never splits the
/// file; `target-state` falls back to the pre-space checkout manifest.
fn resolve(name: &str, cwd: &std::path::Path) -> Option<PathBuf> {
    let space = space_dir(cwd);
    let wt = worktree_space_dir(cwd);
    match name {
        "target-state" => {
            let path = wt.join("target-state.md");
            if path.exists() {
                return Some(path);
            }
            let legacy = worktree_repo_root(cwd).join(".fno").join("target-state.md");
            Some(if legacy.exists() { legacy } else { path })
        }
        "run-log" => Some(wt.join("run-log.jsonl")),
        "events" => {
            if let Some(v) = std::env::var_os("FNO_EVENTS_PATH").filter(|v| !v.is_empty()) {
                return Some(PathBuf::from(v));
            }
            Some(crate::paths::events_path(cwd))
        }
        "plans" => Some(space.join("plans")),
        "inbox" => Some(space.join("inbox")),
        "kings" => Some(space.join("kings")),
        "scratchpad" => Some(wt.join("scratchpad")),
        "status-sinks" => Some(space.join("status-sinks")),
        "worktree-log" => Some(space.join("worktree-log.jsonl")),
        "codemap" => Some(wt.join("codemap.md")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::test_env_lock;
    use crate::paths::space_slug;
    use std::fs;
    use std::path::Path;

    struct EnvGuard {
        home: PathBuf,
        spaces: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new(home: &Path) -> Self {
            let spaces = std::env::var_os("FNO_SPACES_DIR");
            std::env::set_var("FNO_AGENTS_HOME", home.join("agents-home"));
            std::env::set_var("FNO_SPACES_DIR", home.join("spaces"));
            Self {
                home: home.to_path_buf(),
                spaces,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.spaces {
                Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
                None => std::env::remove_var("FNO_SPACES_DIR"),
            }
            std::env::remove_var("FNO_AGENTS_HOME");
            let _ = &self.home;
        }
    }

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let status = std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["-C", dir.to_str().unwrap()])
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success(), "git init failed for {}", dir.display());
        let steps: &[&[&str]] = &[
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["commit", "-q", "--allow-empty", "-m", "init"],
        ];
        for args in steps {
            let status = std::process::Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .args(["-C", dir.to_str().unwrap()])
                .args(*args)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "git {args:?} failed for {}",
                dir.display()
            );
        }
    }

    #[test]
    fn state_path_answers_space_paths_from_a_worktree() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("fno-state-path-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let repo = base.join("repo");
        init_repo(&repo);
        let wt = base.join("wt");
        let status = std::process::Command::new("git")
            .args([
                "-C",
                repo.to_str().unwrap(),
                "worktree",
                "add",
                "-b",
                "wt",
                wt.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "worktree add failed");

        let _env = EnvGuard::new(&base);
        let expected_slug = space_slug(&fs::canonicalize(&repo).unwrap());

        let events = resolve("events", &wt).unwrap();
        assert_eq!(
            events,
            base.join("spaces")
                .join(&expected_slug)
                .join("events.jsonl")
        );
        let manifest = resolve("target-state", &wt).unwrap();
        assert_eq!(
            manifest,
            base.join("spaces")
                .join(&expected_slug)
                .join("worktrees")
                .join("wt")
                .join("target-state.md")
        );
        let scratch = resolve("scratchpad", &wt).unwrap();
        assert!(scratch.starts_with(
            base.join("spaces")
                .join(&expected_slug)
                .join("worktrees")
                .join("wt")
        ));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn state_path_migrates_legacy_events_and_keeps_target_state_fallback() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let base =
            std::env::temp_dir().join(format!("fno-state-path-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let repo = base.join("repo");
        init_repo(&repo);

        let _env = EnvGuard::new(&base);
        let legacy = repo.join(".fno");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("events.jsonl"), "{\"k\":1}\n").unwrap();
        let legacy_manifest = legacy.join("target-state.md");
        fs::write(&legacy_manifest, "_repo").unwrap();

        let events = resolve("events", &repo).unwrap();
        assert_eq!(
            events,
            base.join("spaces")
                .join(space_slug(&fs::canonicalize(&repo).unwrap()))
                .join("events.jsonl")
        );
        assert!(events.exists(), "events migrated onto the space");
        assert!(
            !legacy.join("events.jsonl").exists(),
            "legacy journal moved"
        );
        assert!(legacy.join("MOVED-TO").exists(), "pointer left behind");

        let manifest = resolve("target-state", &repo).unwrap();
        assert_eq!(
            manifest,
            fs::canonicalize(&legacy_manifest).unwrap(),
            "missing space manifest falls back to legacy"
        );
        assert!(
            legacy_manifest.exists(),
            "fallback never migrates the manifest"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
