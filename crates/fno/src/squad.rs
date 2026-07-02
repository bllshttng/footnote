//! Squads: cwd-keyed pane-tree containers, plus the canonical-repo-root
//! resolution that keys them.
//!
//! A squad is born from a client's `Attach.cwd` and keyed by the canonical
//! repo root, so a repo's main checkout and all of its worktrees converge on
//! ONE squad (the fno-agents grid solved the same problem in
//! `grid/repo.rs` (x-cb89); the resolution here is a copy-adaptation of that
//! shipped code - copied, never a cross-crate dependency). A non-git cwd, a
//! failing git, or a HANGING git (bounded timeout) all fall back to a squad
//! keyed by the literal cwd - resolution problems can never refuse an attach.
//!
//! The container model (`Session` -> `Squad` -> `tree::Tab`) is pure and
//! I/O-free like `tree.rs`; only [`Resolver::resolve`] shells out (once per
//! distinct cwd per server lifetime, cached including failures, always OFF
//! the render path - the epic's origin-freeze class of bug is a sync
//! subprocess on a hot loop).

use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

use crate::tree::{self, PaneId, Tab};

/// How long the one `git rev-parse` per distinct cwd may take before the
/// literal-cwd fallback wins. Generous for a warm disk, tight enough that a
/// hung git (network filesystem, broken libc) delays one attach by two
/// seconds exactly once (the failure is cached).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Container model
// ---------------------------------------------------------------------------

/// One squad: a canonical-cwd-keyed group of tabs. Identity is `id`
/// (session-scoped, monotonic, never reused - Locked Decision 6); the
/// grouping key is `canonical_cwd` (resolved PATH, never the basename - two
/// repos sharing a basename stay distinct). Display naming happens at Layout
/// build via [`display_names`].
#[derive(Debug, Clone, PartialEq)]
pub struct Squad {
    pub id: u64,
    pub canonical_cwd: String,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

/// The server's whole layout world: squads in creation order (stable sideline
/// ordering within a session) plus the server-global active squad (Locked
/// Decision 9; per-client views are Phase 3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub squads: Vec<Squad>,
    /// `None` only while the session has no squads (cold start, or after the
    /// last squad died - which ends the session anyway).
    pub active_squad: Option<u64>,
}

/// What [`Session::remove_tab`] left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The squad still has tabs; `active_tab` was re-clamped.
    TabRemoved,
    /// The tab was its squad's last: the squad is gone too, and the active
    /// squad fell back to a survivor.
    SquadRemoved,
    /// That was the last tab of the last squad: the session is over (the
    /// server sends `Bye` and exits - Locked Decision 8).
    SessionEmpty,
}

impl Session {
    pub fn squad(&self, id: u64) -> Option<&Squad> {
        self.squads.iter().find(|s| s.id == id)
    }

    pub fn squad_mut(&mut self, id: u64) -> Option<&mut Squad> {
        self.squads.iter_mut().find(|s| s.id == id)
    }

    pub fn active(&self) -> Option<&Squad> {
        self.squad(self.active_squad?)
    }

    pub fn active_mut(&mut self) -> Option<&mut Squad> {
        self.squad_mut(self.active_squad?)
    }

    /// The active squad's active tab - where input, splits, and focus land.
    pub fn active_tab(&self) -> Option<&Tab> {
        let s = self.active()?;
        s.tabs.get(s.active_tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let s = self.active_mut()?;
        let idx = s.active_tab;
        s.tabs.get_mut(idx)
    }

    /// Squad identity keys on the resolved PATH (Invariant: worktree attaches
    /// converge; same-basename repos stay distinct).
    pub fn find_by_cwd(&self, canonical_cwd: &str) -> Option<u64> {
        self.squads
            .iter()
            .find(|s| s.canonical_cwd == canonical_cwd)
            .map(|s| s.id)
    }

    /// Register a fresh squad (with its first tab already built) and make it
    /// active. The caller allocates `id` and has already spawned the first
    /// pane's PTY (atomic split ordering, Locked Decision 7).
    pub fn add_squad(&mut self, id: u64, canonical_cwd: String, first_tab: Tab) {
        self.squads.push(Squad {
            id,
            canonical_cwd,
            tabs: vec![first_tab],
            active_tab: 0,
        });
        self.active_squad = Some(id);
    }

    /// Which squad/tab holds pane `pid`. `(squad_id, tab_index)`.
    pub fn find_pane(&self, pid: PaneId) -> Option<(u64, usize)> {
        for squad in &self.squads {
            for (ti, tab) in squad.tabs.iter().enumerate() {
                if tree::leaves(&tab.root).contains(&pid) {
                    return Some((squad.id, ti));
                }
            }
        }
        None
    }

    /// Remove one tab, cascading per Locked Decision 8: last tab removes the
    /// squad; last squad ends the session. `active_tab`/`active_squad` are
    /// re-anchored to survivors (nearest lower index - the tab you most
    /// recently sat next to - and the first surviving squad respectively).
    pub fn remove_tab(&mut self, squad_id: u64, tab_idx: usize) -> RemoveOutcome {
        let Some(pos) = self.squads.iter().position(|s| s.id == squad_id) else {
            return RemoveOutcome::TabRemoved; // unknown squad: fail-closed no-op
        };
        {
            let squad = &mut self.squads[pos];
            if tab_idx >= squad.tabs.len() {
                return RemoveOutcome::TabRemoved; // unknown tab: no-op
            }
            squad.tabs.remove(tab_idx);
            if !squad.tabs.is_empty() {
                if squad.active_tab >= squad.tabs.len() {
                    squad.active_tab = squad.tabs.len() - 1;
                } else if squad.active_tab > tab_idx {
                    squad.active_tab -= 1;
                }
                return RemoveOutcome::TabRemoved;
            }
        }
        // Last tab: the squad goes with it.
        self.squads.remove(pos);
        if self.squads.is_empty() {
            self.active_squad = None;
            return RemoveOutcome::SessionEmpty;
        }
        if self.active_squad == Some(squad_id) {
            self.active_squad = Some(self.squads[0].id);
        }
        RemoveOutcome::SquadRemoved
    }
}

/// Display names for the sideline: each squad shows its canonical root's
/// basename, and when two roots share a basename BOTH get a parent segment
/// prefix (`work/api` vs `oss/api`) so they stay tellable-apart. Pure; called
/// at Layout build time so a name is always consistent with the current
/// catalog, never stored.
pub fn display_names(canonical_cwds: &[String]) -> Vec<String> {
    let base = |p: &str| -> String {
        let t = p.trim_end_matches('/');
        t.rsplit('/').next().unwrap_or(t).to_string()
    };
    let parent_seg = |p: &str| -> Option<String> {
        let t = p.trim_end_matches('/');
        let parent = &t[..t.rfind('/')?];
        Some(parent.rsplit('/').next()?.to_string()).filter(|s| !s.is_empty())
    };
    let names: Vec<String> = canonical_cwds.iter().map(|p| base(p)).collect();
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let dup = names.iter().enumerate().any(|(j, n)| j != i && n == name);
            match (dup, parent_seg(&canonical_cwds[i])) {
                (true, Some(parent)) => format!("{parent}/{name}"),
                _ => name.clone(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Canonical-cwd resolution (the only I/O in this module)
// ---------------------------------------------------------------------------

/// Per-distinct-cwd resolution cache. Failures memoize the literal cwd so a
/// broken git is paid for at most once per cwd per server lifetime.
#[derive(Debug, Default)]
pub struct Resolver {
    cache: HashMap<String, String>,
}

impl Resolver {
    /// Resolve `cwd` to the squad key: the canonical repo root when git can
    /// name one within [`GIT_TIMEOUT`], else the literal cwd. Never fails.
    pub fn resolve(&mut self, cwd: &str) -> String {
        if let Some(hit) = self.cache.get(cwd) {
            return hit.clone();
        }
        let key = canonical_root("git", cwd, GIT_TIMEOUT).unwrap_or_else(|| cwd.to_string());
        self.cache.insert(cwd.to_string(), key.clone());
        key
    }
}

/// One bounded `git rev-parse --path-format=absolute --git-common-dir` run.
/// `None` on any failure: not a repo, git missing, non-zero exit, non-UTF-8
/// output, or the timeout expiring (the child is killed). The parent of the
/// common `.git` dir is the canonical root for the main checkout AND every
/// linked worktree; a bare repo's common-dir is the repo dir itself.
fn canonical_root(git: &str, cwd: &str, timeout: Duration) -> Option<String> {
    if !std::path::Path::new(cwd).is_dir() {
        return None; // Command::current_dir on a missing dir errors anyway
    }
    let mut child = std::process::Command::new(git)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                // Timed out (or the wait itself failed): kill and fall back.
                // The output, if any ever comes, is not worth blocking an
                // attach for - the failure is cached by the caller.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    // rev-parse output is tiny (one path), far below the pipe buffer, so the
    // child can never have blocked on write and reading after exit is safe.
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let common = out.trim();
    if common.is_empty() {
        return None;
    }
    Some(root_from_common_dir(common))
}

/// `<root>/.git` -> `<root>`; a bare repo's common-dir (no `.git` suffix) is
/// used as-is rather than mis-rooting at its parent. (Copied from the shipped
/// fno-agents `grid/repo.rs`.)
fn root_from_common_dir(common_dir: &str) -> String {
    let trimmed = common_dir.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, ".git")) if !parent.is_empty() => parent.to_string(),
        Some(("", ".git")) => "/".to_string(),
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Node;
    use std::path::{Path, PathBuf};

    fn tab(ids: &[PaneId]) -> Tab {
        let root = if ids.len() == 1 {
            Node::Leaf(ids[0])
        } else {
            Node::Branch {
                axis: crate::tree::Axis::Horizontal,
                children: ids
                    .iter()
                    .map(|id| (1.0 / ids.len() as f32, Node::Leaf(*id)))
                    .collect(),
            }
        };
        Tab {
            root,
            focus: ids[0],
        }
    }

    // -- container model -------------------------------------------------

    #[test]
    fn squad_add_and_find_by_canonical_cwd() {
        let mut s = Session::default();
        s.add_squad(1, "/code/footnote".into(), tab(&[10]));
        s.add_squad(2, "/code/other".into(), tab(&[20]));
        assert_eq!(s.find_by_cwd("/code/footnote"), Some(1));
        assert_eq!(s.find_by_cwd("/code/none"), None);
        assert_eq!(s.active_squad, Some(2), "a fresh squad becomes active");
        assert_eq!(s.find_pane(10), Some((1, 0)));
        assert_eq!(s.find_pane(99), None);
    }

    #[test]
    fn squad_remove_tab_clamps_active_and_cascades() {
        let mut s = Session::default();
        s.add_squad(1, "/a".into(), tab(&[10]));
        s.squad_mut(1).unwrap().tabs.push(tab(&[11]));
        s.squad_mut(1).unwrap().tabs.push(tab(&[12]));
        s.squad_mut(1).unwrap().active_tab = 2;

        // Removing the active last tab re-clamps active_tab.
        assert_eq!(s.remove_tab(1, 2), RemoveOutcome::TabRemoved);
        assert_eq!(s.squad(1).unwrap().active_tab, 1);
        // Removing an earlier tab shifts active_tab down with it.
        assert_eq!(s.remove_tab(1, 0), RemoveOutcome::TabRemoved);
        assert_eq!(s.squad(1).unwrap().active_tab, 0);
        // Last tab removes the squad; last squad ends the session.
        assert_eq!(s.remove_tab(1, 0), RemoveOutcome::SessionEmpty);
        assert_eq!(s.active_squad, None);
    }

    #[test]
    fn squad_remove_last_tab_falls_back_to_surviving_squad() {
        let mut s = Session::default();
        s.add_squad(1, "/a".into(), tab(&[10]));
        s.add_squad(2, "/b".into(), tab(&[20]));
        assert_eq!(s.active_squad, Some(2));
        assert_eq!(s.remove_tab(2, 0), RemoveOutcome::SquadRemoved);
        assert_eq!(s.active_squad, Some(1), "active falls back to a survivor");
    }

    #[test]
    fn squad_display_names_disambiguate_duplicate_basenames() {
        let cwds = vec![
            "/home/u/work/api".to_string(),
            "/home/u/oss/api".to_string(),
            "/home/u/code/footnote".to_string(),
        ];
        assert_eq!(
            display_names(&cwds),
            vec!["work/api", "oss/api", "footnote"]
        );
    }

    // -- resolution --------------------------------------------------------

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("fno-squad-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(args: &[&str], cwd: &Path) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs")
            .status
            .success();
        assert!(ok, "git {args:?} failed in {cwd:?}");
    }

    #[test]
    fn squad_worktree_and_main_checkout_resolve_to_one_root() {
        // AC6-HP's resolution half: a repo's main checkout and a worktree
        // placed OUTSIDE it (the conductor layout) share one canonical root.
        let scratch = Scratch::new("rollup");
        let repo = scratch.0.join("footnote");
        std::fs::create_dir(&repo).unwrap();
        git(&["init", "-q"], &repo);
        git(&["config", "user.email", "t@t"], &repo);
        git(&["config", "user.name", "t"], &repo);
        git(&["commit", "-q", "--allow-empty", "-m", "init"], &repo);
        let wt = scratch.0.join("workspaces").join("athens");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git(
            &["worktree", "add", "-q", wt.to_str().unwrap(), "HEAD"],
            &repo,
        );

        let mut r = Resolver::default();
        let main_key = r.resolve(repo.to_str().unwrap());
        let wt_key = r.resolve(wt.to_str().unwrap());
        assert_eq!(main_key, wt_key, "worktree must roll up to the main root");
        assert!(main_key.ends_with("/footnote"), "{main_key}");
    }

    #[test]
    fn squad_non_git_cwd_falls_back_to_literal_and_caches() {
        let scratch = Scratch::new("literal");
        let plain = scratch.0.join("plain");
        std::fs::create_dir(&plain).unwrap();
        let mut r = Resolver::default();
        let key = r.resolve(plain.to_str().unwrap());
        assert_eq!(key, plain.to_str().unwrap(), "literal-cwd fallback");
        assert!(
            r.cache.contains_key(plain.to_str().unwrap()),
            "failures are cached: resolution runs at most once per cwd"
        );
    }

    #[test]
    fn squad_missing_cwd_falls_back_to_literal() {
        let mut r = Resolver::default();
        assert_eq!(r.resolve("/definitely/not/here"), "/definitely/not/here");
    }

    #[test]
    fn squad_hanging_git_falls_back_within_the_timeout() {
        // AC6-ERR: a git that never returns must not hang the attach. Stub
        // `git` with a script that sleeps far past the timeout; the private
        // fn takes the program path directly so the test never touches PATH
        // (unit tests share the process). 2.6 exercises the PATH-stub route
        // end-to-end against a real server.
        let scratch = Scratch::new("hang");
        let stub = scratch.0.join("git");
        std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = Instant::now();
        let out = canonical_root(
            stub.to_str().unwrap(),
            scratch.0.to_str().unwrap(),
            Duration::from_millis(300),
        );
        assert_eq!(out, None, "a hung git must resolve to the fallback");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must bound the wait (took {:?})",
            started.elapsed()
        );
    }

    #[test]
    fn squad_root_from_common_dir_handles_normal_and_bare() {
        assert_eq!(root_from_common_dir("/code/foot/.git"), "/code/foot");
        assert_eq!(root_from_common_dir("/code/foot/.git/"), "/code/foot");
        assert_eq!(root_from_common_dir("/srv/bare.git"), "/srv/bare.git");
        assert_eq!(root_from_common_dir("/.git"), "/");
    }
}
