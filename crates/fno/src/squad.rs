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

use crate::tree::{self, PaneId, Tab, TabId};

/// How long the one `git rev-parse` per distinct cwd may take before the
/// literal-cwd fallback wins. Generous for a warm disk, tight enough that a
/// hung git (network filesystem, broken libc) delays one attach by two
/// seconds exactly once (the failure is cached).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Container model
// ---------------------------------------------------------------------------

/// One squad: a NAMED grouping of tabs (a workspace). Identity is `id`
/// (session-scoped, monotonic, never reused - Locked Decision 6), NOT a cwd:
/// two squads may now share OR lack an origin path without merging or
/// orphaning. `origins` are the paths this squad groups (`origins.first()` is
/// the optional source-origination path, kept as an attach-time convenience);
/// a squad born from an explicit `NewSquad` carries a user-given `name`, while
/// an auto-created (attach) squad leaves `name` `None` and derives its display
/// label from `origins` via [`display_names`].
#[derive(Debug, Clone, PartialEq)]
pub struct Squad {
    pub id: u64,
    /// The paths this squad groups. Empty for a named squad created with no
    /// origin; one entry for an attach-born or single-origin squad; several
    /// for the epic's multi-origin north star.
    pub origins: Vec<String>,
    /// A user-given workspace name (explicit `NewSquad`), or `None` when the
    /// display label derives from `origins`.
    pub name: Option<String>,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

impl Squad {
    /// The source-origination path (`origins.first()`), or `""` for a named
    /// squad with no origin. Reads at call sites that only ever want the one
    /// primary path (a new pane's cwd, the status-row label) without churning
    /// through the whole `origins` vec.
    pub fn canonical_cwd(&self) -> &str {
        self.origins.first().map(String::as_str).unwrap_or("")
    }

    /// Whether `path` is one of this squad's origins or a child of one. The
    /// shared predicate behind attach origin-matching and the watch-only
    /// agent→squad fallback: an empty-origin (named) squad owns nothing, so it
    /// is never auto-joined.
    pub fn owns_path(&self, path: &str) -> bool {
        self.origins.iter().any(|o| {
            path == o
                || path
                    .strip_prefix(o.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        })
    }
}

/// The server's whole layout world: squads in creation order (stable sideline
/// ordering within a session). Phase 3: routing is per-client (each `Client`
/// carries its own view); `active_squad` and each squad's `active_tab`
/// survive as the most-recently-active anchors a FRESH attach and a view
/// re-anchor fall back to, not as routing state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub squads: Vec<Squad>,
    /// `None` only while the session has no squads (cold start, or after the
    /// last squad died - which ends the session anyway).
    pub active_squad: Option<u64>,
    /// Monotonic [`TabId`] mint - session-scoped, never reused (Locked 6).
    next_tab_id: TabId,
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
    /// Mint the next stable tab id (1, 2, 3, ... - 0 is never minted, so
    /// tests can use it as a sentinel).
    pub fn mint_tab_id(&mut self) -> TabId {
        self.next_tab_id += 1;
        self.next_tab_id
    }

    /// Which squad holds tab `tid`, and at which index. Ids are
    /// session-unique, so the first hit is the only hit.
    pub fn find_tab(&self, tid: TabId) -> Option<(u64, usize)> {
        for squad in &self.squads {
            if let Some(idx) = squad.tabs.iter().position(|t| t.id == tid) {
                return Some((squad.id, idx));
            }
        }
        None
    }

    pub fn squad(&self, id: u64) -> Option<&Squad> {
        self.squads.iter().find(|s| s.id == id)
    }

    pub fn squad_mut(&mut self, id: u64) -> Option<&mut Squad> {
        self.squads.iter_mut().find(|s| s.id == id)
    }

    /// The squad whose `origins` own `cwd` (exact or child path): worktree
    /// attaches still converge on the resolved repo root, and an attach from a
    /// subdir of a named squad's origin joins it rather than spawning a new
    /// squad. Named (empty-origin) squads own nothing, so they are bypassed
    /// here - explicit creation is the only way into them.
    pub fn find_by_cwd(&self, cwd: &str) -> Option<u64> {
        self.squads.iter().find(|s| s.owns_path(cwd)).map(|s| s.id)
    }

    /// Register a fresh squad (with its first tab already built) and make it
    /// active. The caller allocates `id` and has already spawned the first
    /// pane's PTY (atomic split ordering, Locked Decision 7). `name` is `Some`
    /// only for an explicit `NewSquad`; an attach-born squad passes `None` and
    /// derives its label from `origins`.
    pub fn add_squad(
        &mut self,
        id: u64,
        origins: Vec<String>,
        name: Option<String>,
        first_tab: Tab,
    ) {
        // A caller-built tab id must never collide with a future mint.
        self.next_tab_id = self.next_tab_id.max(first_tab.id);
        self.squads.push(Squad {
            id,
            origins,
            name,
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

    /// Re-home a whole tab (and every pane in it) into another squad (x-96e8).
    /// Pure data surgery - the caller does pane-free view fixup after. Panes
    /// are NEVER reaped here (they ride with the tab); only [`RemoveOutcome`]'s
    /// remove_tab path reaps. Can never produce a session-empty state: `dst`
    /// gains a tab, so at least one squad always survives.
    pub fn move_tab(&mut self, tab: TabId, dst: u64) -> MoveTabOutcome {
        let Some((src_id, tab_idx)) = self.find_tab(tab) else {
            return MoveTabOutcome::Refused("no such tab");
        };
        if self.squad(dst).is_none() {
            return MoveTabOutcome::Refused("no such squad");
        }
        if src_id == dst {
            return MoveTabOutcome::Refused("already there");
        }
        let src_pos = self
            .squads
            .iter()
            .position(|s| s.id == src_id)
            .expect("find_tab live squad");
        // Detach the Tab (id kept - ids are session-unique, never reused).
        let moved = self.squads[src_pos].tabs.remove(tab_idx);
        // Source active_tab re-clamps exactly like remove_tab's TabRemoved arm.
        {
            let src = &mut self.squads[src_pos];
            if !src.tabs.is_empty() {
                if src.active_tab >= src.tabs.len() {
                    src.active_tab = src.tabs.len() - 1;
                } else if src.active_tab > tab_idx {
                    src.active_tab -= 1;
                }
            }
        }
        // Push onto dst; dst.active_tab is untouched (the caller re-homes any
        // viewer of the moved tab via set_view, which is the only thing that
        // moves the MRU pointer).
        self.squad_mut(dst)
            .expect("dst checked live above")
            .tabs
            .push(moved);
        // Source squad's last tab moved out: drop the now-empty squad (its
        // panes already left with the tab) and re-anchor active_squad. Never
        // SessionEmpty - dst just gained a tab.
        if self.squads[src_pos].tabs.is_empty() {
            self.squads.remove(src_pos);
            if self.active_squad == Some(src_id) {
                self.active_squad = Some(self.squads[0].id);
            }
            return MoveTabOutcome::MovedSquadRemoved;
        }
        MoveTabOutcome::Moved
    }
}

/// What [`Session::move_tab`] did, so the handler can fix up client views: a
/// plain move, a move that emptied (and removed) the source squad, or a
/// fail-closed refusal carrying the notice text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTabOutcome {
    /// The tab moved; the source squad still has tabs.
    Moved,
    /// The tab moved AND its (now empty) source squad was removed. Panes were
    /// NOT reaped - they rode with the tab.
    MovedSquadRemoved,
    /// No mutation happened; the `&str` is the notice text for the sender.
    Refused(&'static str),
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
///
/// The blocking git run must NEVER happen on the server's core loop (the
/// grid rail's drive-freeze class: one hung subprocess freezes every pane),
/// so the cache is split from the resolution: callers on async tasks check
/// [`Resolver::cached`], run [`resolve_key`] off-loop (`spawn_blocking`),
/// then [`Resolver::insert`]. Two racing misses for one cwd both run git and
/// insert the same idempotent answer - harmless, and cheaper than holding a
/// lock across a 2s subprocess.
#[derive(Debug, Default)]
pub struct Resolver {
    cache: HashMap<String, String>,
}

impl Resolver {
    pub fn cached(&self, cwd: &str) -> Option<String> {
        self.cache.get(cwd).cloned()
    }

    pub fn insert(&mut self, cwd: String, key: String) {
        self.cache.insert(cwd, key);
    }

    /// Cache-through resolve for synchronous callers (unit tests). Server
    /// code must use the split API above so the git run stays off-loop.
    pub fn resolve(&mut self, cwd: &str) -> String {
        if let Some(hit) = self.cached(cwd) {
            return hit;
        }
        let key = resolve_key(cwd);
        self.insert(cwd.to_string(), key.clone());
        key
    }
}

/// One uncached resolution: the canonical repo root when git can name one
/// within [`GIT_TIMEOUT`], else the literal cwd. Never fails. Blocking -
/// run it on a blocking-capable thread, never the core loop.
pub fn resolve_key(cwd: &str) -> String {
    canonical_root("git", cwd, GIT_TIMEOUT).unwrap_or_else(|| cwd.to_string())
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
            name: None,
            id: ids[0], // unique-enough stable id for container tests
            root,
            focus: ids[0],
        }
    }

    #[test]
    fn squad_tab_ids_mint_monotonic_and_resolve() {
        let mut s = Session::default();
        let (t1, t2) = (s.mint_tab_id(), s.mint_tab_id());
        assert!(t1 > 0 && t2 > t1, "ids are monotonic and never 0");
        s.add_squad(1, vec!["/a".into()], None, tab(&[10]));
        s.squad_mut(1).unwrap().tabs.push(tab(&[11]));
        s.add_squad(2, vec!["/b".into()], None, tab(&[20]));
        assert_eq!(s.find_tab(10), Some((1, 0)));
        assert_eq!(s.find_tab(11), Some((1, 1)));
        assert_eq!(s.find_tab(20), Some((2, 0)));
        assert_eq!(s.find_tab(999), None);
        // Removal never recycles: the mint keeps counting upward.
        s.remove_tab(1, 1);
        let t3 = s.mint_tab_id();
        assert!(t3 > t2, "a removed tab's id is never reused");
    }

    // -- container model -------------------------------------------------

    #[test]
    fn squad_add_and_find_by_canonical_cwd() {
        let mut s = Session::default();
        s.add_squad(1, vec!["/code/footnote".into()], None, tab(&[10]));
        s.add_squad(2, vec!["/code/other".into()], None, tab(&[20]));
        assert_eq!(s.find_by_cwd("/code/footnote"), Some(1));
        assert_eq!(s.find_by_cwd("/code/none"), None);
        // Child-path attach joins the owning squad rather than spawning a new
        // one (change #4); a bare-prefix non-child (`/code/footnote-2`) does not.
        assert_eq!(s.find_by_cwd("/code/footnote/sub/dir"), Some(1));
        assert_eq!(s.find_by_cwd("/code/footnote-2"), None);
        assert_eq!(s.active_squad, Some(2), "a fresh squad becomes active");
        assert_eq!(s.find_pane(10), Some((1, 0)));
        assert_eq!(s.find_pane(99), None);
    }

    #[test]
    fn squad_named_squads_stay_distinct_on_shared_or_empty_origins() {
        // AC1-EDGE (epic Invariants: identity is the stable id, not cwd). Two
        // named squads with the SAME origin, and two with NO origin, remain
        // distinct squads keyed on id - never merged or orphaned.
        let mut s = Session::default();
        s.add_squad(1, vec!["/shared".into()], Some("work".into()), tab(&[10]));
        s.add_squad(2, vec!["/shared".into()], Some("play".into()), tab(&[20]));
        s.add_squad(3, vec![], Some("scratch-a".into()), tab(&[30]));
        s.add_squad(4, vec![], Some("scratch-b".into()), tab(&[40]));
        assert_eq!(s.squads.len(), 4, "shared/empty origins never collapse");
        // A named squad's `canonical_cwd()` degrades to "" when it has no
        // origin, and it owns no path (attach can never auto-join it).
        assert_eq!(s.squad(3).unwrap().canonical_cwd(), "");
        assert!(!s.squad(3).unwrap().owns_path("/anything"));
        // Shared-origin squads are BOTH owners; find_by_cwd returns the first
        // (deterministic), and the second still exists independently.
        assert_eq!(s.squad(1).unwrap().canonical_cwd(), "/shared");
        assert!(s.squad(2).unwrap().owns_path("/shared/deep"));
    }

    #[test]
    fn squad_remove_tab_clamps_active_and_cascades() {
        let mut s = Session::default();
        s.add_squad(1, vec!["/a".into()], None, tab(&[10]));
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
        s.add_squad(1, vec!["/a".into()], None, tab(&[10]));
        s.add_squad(2, vec!["/b".into()], None, tab(&[20]));
        assert_eq!(s.active_squad, Some(2));
        assert_eq!(s.remove_tab(2, 0), RemoveOutcome::SquadRemoved);
        assert_eq!(s.active_squad, Some(1), "active falls back to a survivor");
    }

    // -- x-96e8 move_tab ------------------------------------------------

    #[test]
    fn move_tab_rehomes_a_tab_and_reclamps_source_active() {
        // US4/AC-HP: a tab moves into the destination squad; the source squad's
        // active_tab re-clamps like remove_tab, and dst keeps its own active_tab.
        let mut s = Session::default();
        s.add_squad(1, vec!["/a".into()], None, tab(&[10]));
        s.squad_mut(1).unwrap().tabs.push(tab(&[11]));
        s.squad_mut(1).unwrap().tabs.push(tab(&[12]));
        s.squad_mut(1).unwrap().active_tab = 2;
        s.add_squad(2, vec!["/b".into()], None, tab(&[20]));

        assert_eq!(s.move_tab(10, 2), MoveTabOutcome::Moved);
        // Source lost tab 10 (index 0), keeps 11/12; active_tab shifted down.
        assert_eq!(s.squad(1).unwrap().tabs.len(), 2);
        assert_eq!(s.squad(1).unwrap().active_tab, 1);
        // Destination gained the tab (id preserved); its active_tab untouched.
        assert_eq!(s.find_tab(10), Some((2, 1)));
        assert_eq!(s.squad(2).unwrap().active_tab, 0);
    }

    #[test]
    fn move_tab_of_last_tab_removes_empty_source_without_reaping() {
        // Invariant: moving a squad's only tab removes the now-empty source and
        // re-anchors active_squad; the panes ride with the tab (no reap here).
        let mut s = Session::default();
        s.add_squad(1, vec!["/a".into()], None, tab(&[10]));
        s.add_squad(2, vec!["/b".into()], None, tab(&[20]));
        assert_eq!(s.active_squad, Some(2));

        assert_eq!(s.move_tab(20, 1), MoveTabOutcome::MovedSquadRemoved);
        assert_eq!(s.squads.len(), 1, "empty source squad removed");
        assert_eq!(s.squad(2), None);
        assert_eq!(s.active_squad, Some(1), "active re-anchors to survivor");
        // The moved pane is still present in the destination (never reaped).
        assert_eq!(s.find_tab(20), Some((1, 1)));
        assert_eq!(s.find_pane(20), Some((1, 1)));
    }

    #[test]
    fn move_tab_refuses_unknown_tab_unknown_dst_and_same_squad() {
        let mut s = Session::default();
        s.add_squad(1, vec!["/a".into()], None, tab(&[10]));
        s.add_squad(2, vec!["/b".into()], None, tab(&[20]));

        assert_eq!(s.move_tab(999, 2), MoveTabOutcome::Refused("no such tab"));
        assert_eq!(
            s.move_tab(10, 999),
            MoveTabOutcome::Refused("no such squad")
        );
        assert_eq!(
            s.move_tab(10, 1),
            MoveTabOutcome::Refused("already there"),
            "dst == src is refused, not a corrupting no-op"
        );
        // No refusal mutated anything.
        assert_eq!(s.find_tab(10), Some((1, 0)));
        assert_eq!(s.squads.len(), 2);
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
