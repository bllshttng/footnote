//! Persisted per-client sideline view state (`~/.fno/mux-view.json`).
//!
//! Client-local display preference, NOT session state: which sideline sections
//! the operator left expanded, live-only, or collapsed. Keyed by squad NAME
//! (not the ephemeral session `u64`) so a choice survives a restart.
//!
//! Same degradation posture as [`crate::squad_store`]: all I/O degrades the
//! persistence, never the session. A missing or corrupt file reads as empty
//! (defaults), and a failed write is dropped on the floor - a display
//! preference is never worth refusing to start or interrupting a paint over.
//! Unlike the squad store there is no flock: this is one client's own display
//! state, and the documented policy is last-writer-wins.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const STORE_VERSION: u32 = 1;

/// Which sideline section a view state belongs to. Squads key by NAME so the
/// state survives a restart that re-mints session ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SectionKey {
    Squad(String),
    /// The `~ elsewhere` catch-all for agents matched to no squad.
    Elsewhere,
    /// The `~ work queue` backlog lane.
    WorkQueue,
}

impl SectionKey {
    /// The on-disk key. Prefixed so a squad literally named `elsewhere` can
    /// never collide with the fixed section.
    fn to_wire(&self) -> String {
        match self {
            SectionKey::Squad(name) => format!("squad:{name}"),
            SectionKey::Elsewhere => "elsewhere".into(),
            SectionKey::WorkQueue => "work-queue".into(),
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "elsewhere" => Some(SectionKey::Elsewhere),
            "work-queue" => Some(SectionKey::WorkQueue),
            _ => s
                .strip_prefix("squad:")
                .map(|n| SectionKey::Squad(n.into())),
        }
    }
}

/// How much of a section renders. `LiveOnly` is the middle state x-975a adds:
/// exited agent rows are hidden while the header's `✗N` rollup keeps them
/// discoverable. Display filtering only - no row is reaped (that is x-f300).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionView {
    Expanded,
    LiveOnly,
    Collapsed,
}

/// One click on a section header, as a pure function so the cycle is testable
/// without a View. `has_dead` false skips the `LiveOnly` state entirely (there
/// would be nothing to hide, so the click would look like a no-op), and
/// `binary` forces the same for a section whose rows can never be dead (the
/// work queue). `LiveOnly -> Collapsed` unconditionally, so a section whose
/// last dead row was reaped elsewhere can never wedge in `LiveOnly`.
pub fn next_view(current: SectionView, has_dead: bool, binary: bool) -> SectionView {
    match current {
        SectionView::Expanded if has_dead && !binary => SectionView::LiveOnly,
        SectionView::Expanded => SectionView::Collapsed,
        SectionView::LiveOnly => SectionView::Collapsed,
        SectionView::Collapsed => SectionView::Expanded,
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PATH: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point this thread's store at `dir/mux-view.json` (test-only), so a
/// store-touching test never reads a real `$HOME` nor mutates the
/// process-global environment.
#[cfg(test)]
pub(crate) fn set_test_path(dir: &std::path::Path) {
    TEST_PATH.with(|c| *c.borrow_mut() = Some(dir.join("mux-view.json")));
}

#[cfg(test)]
pub(crate) fn clear_test_path() {
    TEST_PATH.with(|c| *c.borrow_mut() = None);
}

/// A sibling of the squad store: under `FNO_AGENTS_HOME`, else
/// `$HOME/.fno/mux-view.json`.
pub fn view_path() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = TEST_PATH.with(|c| c.borrow().clone()) {
        return p;
    }
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(v).join("mux-view.json");
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("mux-view.json")
}

/// `sections` rather than a bare map so a later view preference (x-b186's
/// density/sort) extends this file instead of minting another one.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sections: HashMap<String, SectionView>,
}

/// Read the persisted view state. Missing, empty, corrupt, or a key/value the
/// current build does not recognize all degrade to "no preference" for that
/// entry - never a refusal to start.
pub fn load() -> HashMap<SectionKey, SectionView> {
    // Hermetic by default under test, for the same reason as [`save`]: every
    // `View::new` loads, so an unguarded read would make the whole client suite
    // depend on the developer's real `~/.fno/mux-view.json`.
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return HashMap::new();
    }
    let Ok(raw) = std::fs::read_to_string(view_path()) else {
        return HashMap::new();
    };
    let Ok(file) = serde_json::from_str::<StoreFile>(&raw) else {
        return HashMap::new();
    };
    file.sections
        .into_iter()
        .filter_map(|(k, v)| SectionKey::from_wire(&k).map(|k| (k, v)))
        .collect()
}

/// Write the whole map (small file, last-writer-wins). Best-effort: any
/// failure leaves the session running with the in-memory state intact.
pub fn save(sections: &HashMap<SectionKey, SectionView>) {
    // A unit test that has not explicitly pointed the store at a scratch dir
    // must never write a real `$HOME` (the whole `client.rs` view suite
    // mutates section state incidentally). Persistence is opt-in under test.
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return;
    }
    let path = view_path();
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let file = StoreFile {
        version: STORE_VERSION,
        sections: sections.iter().map(|(k, v)| (k.to_wire(), *v)).collect(),
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&file) else {
        return;
    };
    // temp+rename so a concurrent reader sees the old or the new file, never a
    // torn one.
    let tmp = path.with_file_name(format!("mux-view.json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("fno-view-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            set_test_path(&dir);
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            clear_test_path();
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // AC1-HP: a saved map round-trips, including a squad name and both fixed
    // sections.
    #[test]
    fn save_load_round_trips() {
        let _s = Scratch::new("round-trip");
        let mut m = HashMap::new();
        m.insert(SectionKey::Squad("footnote".into()), SectionView::LiveOnly);
        m.insert(SectionKey::Elsewhere, SectionView::Collapsed);
        m.insert(SectionKey::WorkQueue, SectionView::Expanded);
        save(&m);
        assert_eq!(load(), m);
    }

    // AC6-FR: invalid JSON degrades to defaults (no crash), and the next save
    // rewrites a valid file.
    #[test]
    fn corrupt_file_degrades_to_defaults_then_rewrites() {
        let _s = Scratch::new("corrupt");
        std::fs::write(view_path(), "{not json").unwrap();
        assert!(load().is_empty(), "corrupt reads as no preference");

        let mut m = HashMap::new();
        m.insert(SectionKey::Squad("a".into()), SectionView::Collapsed);
        save(&m);
        assert_eq!(load(), m, "next write repairs the file");
    }

    // A missing file is a fresh store, not an error.
    #[test]
    fn missing_file_loads_empty() {
        let _s = Scratch::new("missing");
        assert!(load().is_empty());
    }

    // An unknown key or value is dropped entry-wise, not fatally: a file
    // written by a newer build still yields its readable entries.
    #[test]
    fn unknown_entries_are_dropped_not_fatal() {
        let _s = Scratch::new("unknown");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{"squad:a":"expanded","mystery:b":"expanded"}}"#,
        )
        .unwrap();
        let got = load();
        assert_eq!(got.len(), 1, "unknown key dropped: {got:?}");
        assert_eq!(got[&SectionKey::Squad("a".into())], SectionView::Expanded);
    }

    // The `squad:` prefix keeps a squad named after a fixed section from
    // colliding with it.
    #[test]
    fn squad_named_elsewhere_does_not_collide() {
        let _s = Scratch::new("collide");
        let mut m = HashMap::new();
        m.insert(SectionKey::Squad("elsewhere".into()), SectionView::Expanded);
        m.insert(SectionKey::Elsewhere, SectionView::Collapsed);
        save(&m);
        let got = load();
        assert_eq!(
            got[&SectionKey::Squad("elsewhere".into())],
            SectionView::Expanded
        );
        assert_eq!(got[&SectionKey::Elsewhere], SectionView::Collapsed);
    }

    // AC5-EDGE: a section with no dead rows skips LiveOnly entirely, and the
    // work queue is binary in both directions.
    #[test]
    fn next_view_skips_live_only_without_dead() {
        use SectionView::*;
        assert_eq!(next_view(Expanded, false, false), Collapsed);
        assert_eq!(next_view(Collapsed, false, false), Expanded);
        assert_eq!(
            next_view(Expanded, true, true),
            Collapsed,
            "work queue binary"
        );
    }

    // AC4-UI: the full tri-state cycle when dead rows exist.
    #[test]
    fn next_view_cycles_tri_state_with_dead() {
        use SectionView::*;
        assert_eq!(next_view(Expanded, true, false), LiveOnly);
        assert_eq!(next_view(LiveOnly, true, false), Collapsed);
        assert_eq!(next_view(Collapsed, true, false), Expanded);
    }

    // AC12-FR: a section left in LiveOnly whose last dead row was reaped
    // elsewhere advances to Collapsed rather than wedging.
    #[test]
    fn live_only_never_wedges_when_dead_disappears() {
        assert_eq!(
            next_view(SectionView::LiveOnly, false, false),
            SectionView::Collapsed
        );
    }
}
