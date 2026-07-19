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

/// Which sideline section a view state belongs to.
///
/// Keyed by what is STABLE, which is deliberately not the rendered name: a
/// mission header's name embeds its live `done/total` counters, and an
/// attach-born squad's derived label is rewritten (`foo` -> `parent/foo`) as
/// soon as a sibling collides. Either would orphan the operator's choice on an
/// unrelated event, and the derived label is not even unique - the
/// disambiguation is one level deep, and the server's uniqueness gate compares
/// explicit names only.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SectionKey {
    /// A real workspace, keyed by its canonical repo root - stable across both
    /// the label churn above and a restart. Two workspaces rooted at the SAME
    /// canonical cwd share one view state; that is the accepted residual, and
    /// strictly better than sharing it with whatever squad happens to render
    /// under the same name today.
    Squad(String),
    /// A synthetic mission header, keyed by its per-epic id. That id is a pure
    /// hash of the epic id, so unlike the mission's name it survives a progress
    /// tick and a restart alike.
    Mission(u64),
    /// The `~ elsewhere` catch-all for agents matched to no squad.
    Elsewhere,
    /// The `~ work queue` backlog lane.
    WorkQueue,
}

impl SectionKey {
    /// The on-disk key. Prefixed so a squad whose identity is literally
    /// `elsewhere` can never collide with the fixed section. `strip_prefix`
    /// removes only the leading occurrence, so a cwd containing `squad:` or
    /// any number of colons still round-trips.
    fn to_wire(&self) -> String {
        match self {
            SectionKey::Squad(cwd) => format!("squad:{cwd}"),
            SectionKey::Mission(id) => format!("mission:{id:x}"),
            SectionKey::Elsewhere => "elsewhere".into(),
            SectionKey::WorkQueue => "work-queue".into(),
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "elsewhere" => Some(SectionKey::Elsewhere),
            "work-queue" => Some(SectionKey::WorkQueue),
            _ => {
                if let Some(cwd) = s.strip_prefix("squad:") {
                    return Some(SectionKey::Squad(cwd.into()));
                }
                let id = s.strip_prefix("mission:")?;
                u64::from_str_radix(id, 16).ok().map(SectionKey::Mission)
            }
        }
    }

    /// Whether this section's cycle is binary (expanded <-> collapsed). The
    /// work queue's rows are cards, which have no exited state, so its middle
    /// state would hide nothing.
    fn is_binary(&self) -> bool {
        matches!(self, SectionKey::WorkQueue)
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
/// would be nothing to hide, so the click would look like a no-op), as does a
/// binary section - a rule this owns via `key` rather than taking as a second
/// transposable bool from its caller. `LiveOnly -> Collapsed` unconditionally,
/// so a section whose last dead row was reaped elsewhere can never wedge in
/// `LiveOnly`.
pub fn next_view(current: SectionView, has_dead: bool, key: &SectionKey) -> SectionView {
    match current {
        SectionView::Expanded if has_dead && !key.is_binary() => SectionView::LiveOnly,
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
/// Values stay `Value` on the way in so ONE unrecognized state does not fail
/// the whole map: a file written by a build with a fourth `SectionView` would
/// otherwise read as zero preferences here, and the next click would overwrite
/// it - unrecoverable loss of the newer build's state. Parsing per entry is
/// what makes [`load`]'s degrade-entry-wise promise true.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sections: HashMap<String, serde_json::Value>,
}

/// The raw file, entries untyped. Missing, empty, or corrupt all read as a
/// fresh store - never a refusal to start.
fn read_raw() -> StoreFile {
    std::fs::read_to_string(view_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<StoreFile>(&raw).ok())
        .unwrap_or_default()
}

/// Read the persisted view state. A key or value the current build does not
/// recognize degrades to "no preference" for THAT entry; the raw entry itself
/// survives on disk (see [`save`]).
pub fn load() -> HashMap<SectionKey, SectionView> {
    // Hermetic by default under test, for the same reason as [`save`]: every
    // `View::new` loads, so an unguarded read would make the whole client suite
    // depend on the developer's real `~/.fno/mux-view.json`.
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return HashMap::new();
    }
    read_raw()
        .sections
        .into_iter()
        .filter_map(|(k, v)| {
            let key = SectionKey::from_wire(&k)?;
            let view: SectionView = serde_json::from_value(v).ok()?;
            Some((key, view))
        })
        .collect()
}

/// MERGE this client's sections into the file rather than replacing it.
///
/// The store is machine-global but a client only ever knows its OWN session's
/// squads, so a wholesale write would delete the preferences of every squad
/// belonging to another running mux - and would also drop a newer build's
/// unrecognized entry, which [`load`] deliberately kept on disk. Overlaying
/// touches only the keys this client actually has an opinion about.
///
/// The cost is that an entry is never removed: a workspace that goes away
/// leaves a few dozen stale bytes behind. That is the right trade against
/// silently deleting a live sibling session's state.
///
/// Best-effort throughout: any failure leaves the session running on its
/// in-memory state.
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
    let mut file = read_raw();
    file.version = STORE_VERSION;
    for (k, v) in sections {
        if let Ok(value) = serde_json::to_value(v) {
            file.sections.insert(k.to_wire(), value);
        }
    }
    let Ok(bytes) = serde_json::to_vec_pretty(&file) else {
        return;
    };
    // temp+rename so a concurrent reader sees the old or the new file, never a
    // torn one. The counter keeps two writers inside ONE process (several
    // clients, or the test suite) off each other's temp file.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!("mux-view.json.tmp.{}.{seq}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
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
        let sq = SectionKey::Squad("/repo".into());
        assert_eq!(next_view(Expanded, false, &sq), Collapsed);
        assert_eq!(next_view(Collapsed, false, &sq), Expanded);
        assert_eq!(
            next_view(Expanded, true, &SectionKey::WorkQueue),
            Collapsed,
            "work queue binary even when told rows are dead"
        );
    }

    // AC4-UI: the full tri-state cycle when dead rows exist.
    #[test]
    fn next_view_cycles_tri_state_with_dead() {
        use SectionView::*;
        let sq = SectionKey::Squad("/repo".into());
        assert_eq!(next_view(Expanded, true, &sq), LiveOnly);
        assert_eq!(next_view(LiveOnly, true, &sq), Collapsed);
        assert_eq!(next_view(Collapsed, true, &sq), Expanded);
        // A mission header is a normal tri-state section, not a binary one.
        let m = SectionKey::Mission(0x8000_0000_0000_0001);
        assert_eq!(next_view(Expanded, true, &m), LiveOnly);
    }

    // AC12-FR: a section left in LiveOnly whose last dead row was reaped
    // elsewhere advances to Collapsed rather than wedging.
    #[test]
    fn live_only_never_wedges_when_dead_disappears() {
        assert_eq!(
            next_view(
                SectionView::LiveOnly,
                false,
                &SectionKey::Squad("/repo".into())
            ),
            SectionView::Collapsed
        );
    }

    // A mission key round-trips through the wire form, so a mission section's
    // state survives a restart (its NAME would not - it carries done/total).
    #[test]
    fn mission_key_round_trips() {
        let _s = Scratch::new("mission");
        let mut m = HashMap::new();
        m.insert(
            SectionKey::Mission(0x8000_0000_dead_beef),
            SectionView::Collapsed,
        );
        save(&m);
        assert_eq!(load(), m);
    }

    // `strip_prefix` removes only the leading marker, so an identity that
    // itself contains `squad:` or extra colons still round-trips exactly.
    #[test]
    fn identity_containing_the_prefix_round_trips() {
        let _s = Scratch::new("prefixy");
        let mut m = HashMap::new();
        m.insert(
            SectionKey::Squad("squad:/a/b:c".into()),
            SectionView::LiveOnly,
        );
        save(&m);
        assert_eq!(load(), m);
    }

    // One unrecognized VALUE must drop only its own entry. Typing the map as
    // SectionView would fail the whole parse here, so a newer build's file
    // would read as zero preferences and then be overwritten.
    #[test]
    fn unknown_value_drops_only_its_entry() {
        let _s = Scratch::new("unknown-value");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{"squad:/a":"expanded","squad:/b":"peek_only"}}"#,
        )
        .unwrap();
        let got = load();
        assert_eq!(
            got.len(),
            1,
            "only the unreadable entry is dropped: {got:?}"
        );
        assert_eq!(got[&SectionKey::Squad("/a".into())], SectionView::Expanded);
    }
}
