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

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

const STORE_VERSION: u32 = 1;

/// Which sideline section a view state belongs to.
///
/// Keyed by what is STABLE, which is deliberately not the rendered name: an
/// attach-born squad's derived label is rewritten (`foo` -> `parent/foo`) as
/// soon as a sibling collides. That would orphan the operator's choice on an
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
    /// The `~ elsewhere` catch-all for agents matched to no squad.
    Elsewhere,
}

impl SectionKey {
    /// The on-disk key. Prefixed so a squad whose identity is literally
    /// `elsewhere` can never collide with the fixed section. `strip_prefix`
    /// removes only the leading occurrence, so a cwd containing `squad:` or
    /// any number of colons still round-trips.
    fn to_wire(&self) -> String {
        match self {
            SectionKey::Squad(cwd) => format!("squad:{cwd}"),
            SectionKey::Elsewhere => "elsewhere".into(),
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "elsewhere" => Some(SectionKey::Elsewhere),
            // Anything else, including a `mission:`, `missions`, or `work-queue`
            // key saved by an older build, reads as None and `load` drops that
            // key alone.
            _ => s
                .strip_prefix("squad:")
                .map(|cwd| SectionKey::Squad(cwd.into())),
        }
    }
}

/// How much of a section renders. `LiveOnly` is the middle state adds:
/// exited agent rows are hidden while the header's `✗N` rollup keeps them
/// discoverable. Display filtering only - no row is reaped (that is).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionView {
    Expanded,
    LiveOnly,
    Collapsed,
}

/// One click on a section header, as a pure function so the cycle is testable
/// without a View. `has_dead` false skips the `LiveOnly` state entirely (there
/// would be nothing to hide, so the click would look like a no-op).
/// `LiveOnly -> Collapsed` unconditionally,
/// so a section whose last dead row was reaped elsewhere can never wedge in
/// `LiveOnly`.
pub fn next_view(current: SectionView, has_dead: bool) -> SectionView {
    match current {
        SectionView::Expanded if has_dead => SectionView::LiveOnly,
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

/// A sibling of the squad store: under `FNO_AGENTS_HOME`, else the mux's
/// resolved state root (so a pinned `FNO_CONFIG` isolates the view prefs with
/// the sockets; `FNO_MUX_DIR` relocates sockets alone and does not move this
/// file). The test arm keeps the plain `$HOME` fallback: a test that reaches
/// it sets either `TEST_PATH` or `FNO_AGENTS_HOME` and never wanted config
/// resolution in the first place.
pub fn view_path() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = TEST_PATH.with(|c| c.borrow().clone()) {
        return p;
    }
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(v).join("mux-view.json");
    }
    #[cfg(not(test))]
    return crate::proto::mux_sidecar_root().join("mux-view.json");
    #[cfg(test)]
    {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join(".fno").join("mux-view.json")
    }
}

/// `sections` rather than a bare map so a later view preference (
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
    /// Sideline density and agent-sort order. `Value`, not the typed
    /// enums, for the same reason `sections` is: a state a newer build wrote
    /// must survive a round-trip through this one rather than being reset to
    /// the default on the next gesture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    density: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sort: Option<serde_json::Value>,
    /// The operator's chosen sideline width, once they drag the border
    /// off its density-canonical size. `Value` for the same forward-compat
    /// reason as `density`/`sort`; `None` (absent) means "use the current
    /// density's canonical width" - the back-compat default so existing installs
    /// are unchanged until their first drag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    width: Option<serde_json::Value>,
    /// Ask before stop/remove. Default absent = false: the operator
    /// said the stop-then-confirm two-step costs too many taps, so the confirm
    /// is opt-in, and the next lifecycle gesture persists a clean value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirm_lifecycle: Option<serde_json::Value>,
    /// The operator's chosen feed-panel width, once they drag the
    /// panel's border. `Value` for the same forward-compat reason as `width`;
    /// absent means "use the default width" - existing installs are unchanged
    /// until their first drag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    feed_width: Option<serde_json::Value>,
    /// The experimental backlog board in the sidebar menu. Default
    /// absent = off: the view is experimental, so the next toggle persists a
    /// clean value. Same contract as `confirm_lifecycle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    experimental_backlog_view: Option<serde_json::Value>,
    /// The backlog board's column layout (which columns, order, focus
    /// width). Default absent = the shipped default. Same contract as
    /// `experimental_backlog_view`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_layout: Option<serde_json::Value>,
    /// The sideline's active view. Default absent = agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sideline_view: Option<serde_json::Value>,
    /// The backlog board's full-screen toggle. Default absent = false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_full: Option<serde_json::Value>,
}

/// Which view the sideline column paints. `Agents` is the agent list the
/// sideline shipped with; `Backlog` is the backlog's one-column list (the
/// docked window's replacement).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SidelineView {
    #[default]
    Agents,
    Backlog,
}

/// Read the sideline view pref. Absent, corrupt, or unknown reads as
/// `Agents`, and the next cycle persists a clean value.
pub fn load_sideline_view() -> SidelineView {
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return SidelineView::default();
    }
    read_raw()
        .sideline_view
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Persist the sideline view pref. Best-effort like every other write here.
pub fn save_sideline_view(v: SidelineView) {
    mutate(|file| {
        file.sideline_view = serde_json::to_value(v).ok();
    });
}

/// Read the board full-screen pref. Absent or corrupt reads as `false`.
pub fn load_board_full() -> bool {
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return false;
    }
    read_raw()
        .board_full
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Persist the board full-screen pref. Best-effort like every other write.
pub fn save_board_full(full: bool) {
    mutate(|file| {
        file.board_full = serde_json::to_value(full).ok();
    });
}

/// Read the operator's stop/remove confirm pref. Absent, corrupt, or
/// non-bool reads as `false` - a stop means stop, a remove means remove - and
/// the next toggle persists a clean value.
pub fn load_confirm_lifecycle() -> bool {
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return false;
    }
    read_raw()
        .confirm_lifecycle
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Persist the operator's stop/remove confirm pref. Best-effort like
/// every other write here.
pub fn save_confirm_lifecycle(confirm: bool) {
    mutate(|file| {
        file.confirm_lifecycle = serde_json::to_value(confirm).ok();
    });
}

/// Read the operator's feed-panel width pref. Absent, corrupt, or
/// out-of-range reads as `None` - "use the default width" - and the first
/// drag persists a clean value, the same degrade-independently posture every
/// pref here keeps.
pub fn load_feed_width() -> Option<u16> {
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return None;
    }
    read_raw()
        .feed_width
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok())
}

/// Persist the operator's dragged feed-panel width. Best-effort and
/// fire-and-forget, the same locked read-modify-write core as [`save_width`].
pub fn save_feed_width(width: u16) {
    mutate(|file| {
        file.feed_width = serde_json::to_value(width).ok();
    });
}

/// Read the experimental backlog board pref. Absent, corrupt, or
/// non-bool reads as `false` - the view is off until the operator turns it
/// on, and the next toggle persists a clean value. The same degrade posture
/// every pref here keeps.
pub fn load_experimental_backlog_view() -> bool {
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return false;
    }
    read_raw()
        .experimental_backlog_view
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Persist the experimental backlog board pref. Best-effort like every
/// other write here.
pub fn save_experimental_backlog_view(on: bool) {
    mutate(|file| {
        file.experimental_backlog_view = serde_json::to_value(on).ok();
    });
}

/// How much of each sideline row renders. Orthogonal to the panel's
/// on/off toggle: `Slim` is a narrow rail, NOT a hidden panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// A narrow rail: section headers and their rollup counts only.
    Slim,
    /// Today's tree.
    #[default]
    Regular,
    /// One table row per agent: name | status | tail | PR | last update.
    Extended,
}

impl Density {
    /// One press of the density key. A three-state cycle, so every press
    /// changes both the glyph and the panel geometry - no press is inert.
    pub fn next(self) -> Density {
        match self {
            Density::Slim => Density::Regular,
            Density::Regular => Density::Extended,
            Density::Extended => Density::Slim,
        }
    }
}

/// The column an extended table sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSortColumn {
    Status,
    Agent,
    LastMessage,
    Pr,
    Age,
}

/// Direction for one active table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Ascending,
    Descending,
}

/// One persisted table sort choice. Legacy string values remain readable so a
/// preference upgrade cannot silently reset the operator's selected order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgentSort {
    pub column: AgentSortColumn,
    pub direction: SortDirection,
}

impl Default for AgentSort {
    fn default() -> Self {
        Self::default_for(AgentSortColumn::Status)
    }
}

impl AgentSort {
    #[allow(non_upper_case_globals)]
    pub const Squad: Self = Self {
        column: AgentSortColumn::Agent,
        direction: SortDirection::Ascending,
    };
    #[allow(non_upper_case_globals)]
    pub const Attention: Self = Self {
        column: AgentSortColumn::Status,
        direction: SortDirection::Ascending,
    };

    pub const fn default_for(column: AgentSortColumn) -> Self {
        let direction = match column {
            AgentSortColumn::Age => SortDirection::Descending,
            AgentSortColumn::Status
            | AgentSortColumn::Agent
            | AgentSortColumn::LastMessage
            | AgentSortColumn::Pr => SortDirection::Ascending,
        };
        Self { column, direction }
    }

    pub const fn toggle_direction(self) -> Self {
        Self {
            column: self.column,
            direction: match self.direction {
                SortDirection::Ascending => SortDirection::Descending,
                SortDirection::Descending => SortDirection::Ascending,
            },
        }
    }

    pub const fn advance(self) -> Self {
        use AgentSortColumn::*;
        use SortDirection::*;
        match (self.column, self.direction) {
            (Status, Ascending) => Self {
                column: Status,
                direction: Descending,
            },
            (Status, Descending) => Self {
                column: Agent,
                direction: Ascending,
            },
            (Agent, Ascending) => Self {
                column: Agent,
                direction: Descending,
            },
            (Agent, Descending) => Self {
                column: LastMessage,
                direction: Ascending,
            },
            (LastMessage, Ascending) => Self {
                column: LastMessage,
                direction: Descending,
            },
            (LastMessage, Descending) => Self {
                column: Pr,
                direction: Ascending,
            },
            (Pr, Ascending) => Self {
                column: Pr,
                direction: Descending,
            },
            (Pr, Descending) => Self {
                column: Age,
                direction: Descending,
            },
            (Age, Descending) => Self {
                column: Age,
                direction: Ascending,
            },
            (Age, Ascending) => Self {
                column: Status,
                direction: Ascending,
            },
        }
    }

    pub const fn toggle(self) -> Self {
        match self {
            Self::Squad => Self::Attention,
            Self::Attention => Self::Squad,
            _ => self.toggle_direction(),
        }
    }
}

impl<'de> Deserialize<'de> for AgentSort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(legacy) = value.as_str() {
            return match legacy {
                "squad" => Ok(Self::Squad),
                "attention" | "status" => Ok(Self::Attention),
                _ => Err(de::Error::custom("unknown agent sort")),
            };
        }
        #[derive(Deserialize)]
        struct Wire {
            column: AgentSortColumn,
            direction: SortDirection,
        }
        let wire = Wire::deserialize(value).map_err(de::Error::custom)?;
        Ok(Self {
            column: wire.column,
            direction: wire.direction,
        })
    }
}

/// The raw file, entries untyped. Missing, empty, or corrupt all read as a
/// fresh store - never a refusal to start. A MISSING (NotFound only) file
/// falls back to the pre-state-root location once per read (no copy), but
/// ONLY under fully ambient state resolution: the fallback exists for an
/// upgrading user whose prefs sit at the old spot, and a pinned demo env
/// must inherit nothing from the operator's real root. The gate and the
/// location are `proto::legacy_sidecar`, the one spelling the squad store
/// shares. Any other read error stays an error-shaped miss (defaults): an
/// unreadable primary must never seed the next mutate with legacy content,
/// which would overwrite the file's own prefs.
fn read_raw() -> StoreFile {
    let path = view_path();
    let raw = std::fs::read_to_string(&path).or_else(|e| {
        #[cfg(not(test))]
        {
            if e.kind() != std::io::ErrorKind::NotFound {
                return std::io::Result::Err(e);
            }
            crate::proto::legacy_sidecar("mux-view.json")
        }
        #[cfg(test)]
        std::io::Result::Err(e)
    });
    raw.ok()
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

/// Read the persisted density, sort, and sideline width.
///
/// Missing, corrupt, or written by a build with a state this one cannot parse
/// all resolve to the defaults (`Regular` + status/attention-first + no width), independently
/// per field: an unreadable `sort` never costs the operator their `density`.
/// This is AC7-FR - the mux always starts, and the next gesture persists cleanly
/// over whatever was there.
///
/// The width is `Option`: `None` (absent, unparsable, or out of range) means
/// "use the current density's canonical width" (AC5-ERR). A non-numeric value
/// degrades to `None` rather than being retained, so the caller never carries a
/// corrupt width and the first drag persists a clean number over it.
pub fn load_prefs() -> (Density, AgentSort, Option<u16>) {
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return (Density::default(), AgentSort::default(), None);
    }
    fn parse<T: serde::de::DeserializeOwned + Default>(v: Option<serde_json::Value>) -> T {
        v.and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
    let file = read_raw();
    (
        parse(file.density),
        parse(file.sort),
        parse::<Option<u16>>(file.width),
    )
}

/// Persist the density and sort the operator chose. Best-effort, like every
/// other write here: a failure leaves the session on its in-memory state.
pub fn save_prefs(density: Density, sort: AgentSort) {
    mutate(|file| {
        file.density = serde_json::to_value(density).ok();
        file.sort = serde_json::to_value(sort).ok();
    });
}

/// Persist the operator's dragged sideline width. The same locked
/// read-modify-write core as [`save_prefs`], so a width write never clobbers a
/// concurrent density/sort write from another mux client. Best-effort and
/// fire-and-forget: a drag release must never block on the file write.
pub fn save_width(width: u16) {
    mutate(|file| {
        file.width = serde_json::to_value(width).ok();
    });
}

/// Persist a density PRESET - mode, sort, and canonical width - in ONE
/// locked mutation. A preset is a single logical choice of both mode and width,
/// so writing density and width through separate `save_prefs`/`save_width` calls
/// could interleave with another mux client (or be interrupted between them) and
/// leave a persisted mode paired with a width from a different press. This makes
/// the pair atomic so a later attach always observes a preset that was actually
/// selected.
pub fn save_preset(density: Density, sort: AgentSort, width: u16) {
    mutate(|file| {
        file.density = serde_json::to_value(density).ok();
        file.sort = serde_json::to_value(sort).ok();
        file.width = serde_json::to_value(width).ok();
    });
}

/// Persist the sections the operator EXPLICITLY chose, merging them into the
/// file under an exclusive lock.
///
/// Three properties, each protecting against a different way state was lost:
///
/// - **Merge, not replace.** The store is machine-global but a client only ever
///   knows its OWN session's squads, so a wholesale write would delete the
///   preferences of every squad belonging to another running mux.
/// - **Only explicit choices.** `chosen` is the operator-touched set, not the
///   whole in-memory map. A seeded default (auto-expand on activation) is
///   recomputed every attach and is not a preference; writing it would let an
///   older build that could not parse a newer `SectionView` re-seed its own
///   default and overwrite that newer value on disk.
/// - **Locked read-modify-write.** Read and write under one exclusive `flock`,
///   so two clients overlaying disjoint keys cannot each write a snapshot taken
///   before the other's change and silently revert it.
///
/// The accepted cost is that an entry is never removed: a workspace that goes
/// away leaves a few dozen stale bytes behind. That beats deleting a live
/// sibling session's state.
///
/// Best-effort throughout - a contended lock or any I/O failure leaves the
/// session running on its in-memory state, because a display preference is
/// never worth interrupting a paint over.
pub fn save(chosen: &HashMap<SectionKey, SectionView>) {
    if chosen.is_empty() {
        return;
    }
    mutate(|file| {
        for (k, v) in chosen {
            if let Ok(value) = serde_json::to_value(v) {
                file.sections.insert(k.to_wire(), value);
            }
        }
    });
}

/// Apply `f` to the store under an exclusive lock and write it back atomically.
///
/// The locked read-modify-write core [`save`] and [`save_prefs`] share, so the
/// two preference kinds can never clobber each other: both re-read INSIDE the
/// lock and overlay, rather than writing a snapshot taken before the other's
/// change. Best-effort at every step - a contended lock or any I/O failure
/// leaves the session on its in-memory state, because a display preference is
/// never worth interrupting a paint over.
fn mutate(f: impl FnOnce(&mut StoreFile)) {
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
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path.with_file_name("mux-view.json.lock"))
    else {
        return;
    };
    let Ok(_guard) = crate::squad_store::FlockGuard::acquire(lock) else {
        return; // contended: the next gesture writes, nothing is corrupted
    };

    // Re-read INSIDE the lock so the overlay lands on the current file.
    let mut file = read_raw();
    file.version = STORE_VERSION;
    f(&mut file);
    let Ok(bytes) = serde_json::to_vec_pretty(&file) else {
        return;
    };
    // temp+rename so a concurrent READER (which takes no lock) sees the old or
    // the new file, never a torn one. The counter keeps two writers inside ONE
    // process off each other's temp file.
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

    // AC1-HP: a saved map round-trips, including a squad name and the fixed
    // section.
    #[test]
    fn save_load_round_trips() {
        let _s = Scratch::new("round-trip");
        let mut m = HashMap::new();
        m.insert(SectionKey::Squad("footnote".into()), SectionView::LiveOnly);
        m.insert(SectionKey::Elsewhere, SectionView::Collapsed);
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

    // The experimental backlog toggle: absent reads off, a corrupt
    // value reads off, and save/load round-trips (AC3-HP).
    #[test]
    fn experimental_backlog_view_absent_corrupt_and_round_trip() {
        let _s = Scratch::new("backlog-view");
        assert!(!load_experimental_backlog_view(), "absent reads off");
        std::fs::write(view_path(), r#"{"experimental_backlog_view":"yes-please"}"#).unwrap();
        assert!(!load_experimental_backlog_view(), "corrupt reads off");
        save_experimental_backlog_view(true);
        assert!(load_experimental_backlog_view());
        save_experimental_backlog_view(false);
        assert!(!load_experimental_backlog_view());
    }

    // The sideline view pref: absent reads agents, a corrupt or unknown
    // value reads agents, and save/load round-trips.
    #[test]
    fn sideline_view_absent_corrupt_and_round_trip() {
        let _s = Scratch::new("sideline-view");
        assert_eq!(
            load_sideline_view(),
            SidelineView::Agents,
            "absent reads agents"
        );
        std::fs::write(view_path(), r#"{"sideline_view":"diagonal"}"#).unwrap();
        assert_eq!(
            load_sideline_view(),
            SidelineView::Agents,
            "unknown reads agents"
        );
        save_sideline_view(SidelineView::Backlog);
        assert_eq!(load_sideline_view(), SidelineView::Backlog);
        save_sideline_view(SidelineView::Agents);
        assert_eq!(load_sideline_view(), SidelineView::Agents);
    }

    // The board layout pref: absent reads the shipped default (every model
    // column, half focus), a corrupt value reads the default, and
    // save/load round-trips a subset with a different focus.
    #[test]
    fn board_layout_absent_corrupt_and_round_trip() {
        let _s = Scratch::new("board-layout");
        let dflt = load_board_layout();
        assert_eq!(
            dflt.columns,
            backlog_default_columns(),
            "absent reads all columns"
        );
        assert_eq!(dflt.focus_pct, 50, "absent reads half focus");
        std::fs::write(view_path(), r#"{"board_layout":{"columns":"wide"}}"#).unwrap();
        let corrupt = load_board_layout();
        assert_eq!(
            corrupt.columns,
            backlog_default_columns(),
            "corrupt reads default"
        );
        let custom = BoardLayout {
            columns: vec!["Now".into(), "Done".into()],
            focus_pct: 65,
        };
        save_board_layout(&custom);
        assert_eq!(load_board_layout(), custom);
    }

    // The board full-screen pref: absent or corrupt reads false, and
    // save/load round-trips.
    #[test]
    fn board_full_absent_corrupt_and_round_trip() {
        let _s = Scratch::new("board-full");
        assert!(!load_board_full(), "absent reads false");
        std::fs::write(view_path(), r#"{"board_full":"wide"}"#).unwrap();
        assert!(!load_board_full(), "corrupt reads false");
        save_board_full(true);
        assert!(load_board_full());
        save_board_full(false);
        assert!(!load_board_full());
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

    // AC5-EDGE: a section with no dead rows skips LiveOnly entirely.
    #[test]
    fn next_view_skips_live_only_without_dead() {
        use SectionView::*;
        assert_eq!(next_view(Expanded, false), Collapsed);
        assert_eq!(next_view(Collapsed, false), Expanded);
    }

    // AC4-UI: the full tri-state cycle when dead rows exist.
    #[test]
    fn next_view_cycles_tri_state_with_dead() {
        use SectionView::*;
        assert_eq!(next_view(Expanded, true), LiveOnly);
        assert_eq!(next_view(LiveOnly, true), Collapsed);
        assert_eq!(next_view(Collapsed, true), Expanded);
    }

    // AC12-FR: a section left in LiveOnly whose last dead row was reaped
    // elsewhere advances to Collapsed rather than wedging.
    #[test]
    fn live_only_never_wedges_when_dead_disappears() {
        assert_eq!(
            next_view(SectionView::LiveOnly, false),
            SectionView::Collapsed
        );
    }

    // A mux-view.json saved by an older build with the deleted `work-queue`
    // and `missions` section keys loads, and only the readable entries
    // survive (AC6-EDGE for the sections' removal).
    #[test]
    fn legacy_work_queue_and_missions_keys_load_and_drop() {
        let _s = Scratch::new("legacy-keys");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{"work-queue":"expanded","missions":"collapsed","squad:/x":"live_only"}}"#,
        )
        .unwrap();
        let got = load();
        assert_eq!(got.len(), 1, "only the squad key survives: {got:?}");
        assert_eq!(got[&SectionKey::Squad("/x".into())], SectionView::LiveOnly);
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

    // A save must not clobber a key it was not asked to write - that is what
    // keeps one mux session from deleting a sibling session's preferences.
    #[test]
    fn save_merges_and_leaves_foreign_keys_alone() {
        let _s = Scratch::new("merge");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{"squad:/other-session":"collapsed"}}"#,
        )
        .unwrap();

        let mut mine = HashMap::new();
        mine.insert(SectionKey::Squad("/mine".into()), SectionView::LiveOnly);
        save(&mine);

        let got = load();
        assert_eq!(
            got[&SectionKey::Squad("/mine".into())],
            SectionView::LiveOnly
        );
        assert_eq!(
            got[&SectionKey::Squad("/other-session".into())],
            SectionView::Collapsed,
            "another session's key must survive this session's write"
        );
    }

    // An entry this build cannot parse survives a write, so an older client
    // never destroys a newer build's state. Load drops it (no opinion), and
    // save never writes a key it was not explicitly given.
    #[test]
    fn save_preserves_an_unparseable_entry_on_disk() {
        let _s = Scratch::new("preserve-unknown");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{"squad:/a":"peek_only"}}"#,
        )
        .unwrap();
        assert!(load().is_empty(), "this build has no opinion on that value");

        let mut mine = HashMap::new();
        mine.insert(SectionKey::Squad("/b".into()), SectionView::Collapsed);
        save(&mine);

        let raw = std::fs::read_to_string(view_path()).unwrap();
        assert!(
            raw.contains("peek_only"),
            "the newer build's value must still be on disk: {raw}"
        );
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

    // ----: density + sort preferences ----

    #[test]
    fn prefs_default_then_round_trip() {
        let _s = Scratch::new("prefs-roundtrip");
        // AC7-FR: a missing file is not an error, it is the defaults. The
        // default sort is attention (evidence of neglect first); only a stored
        // preference can return the table to tree order.
        assert_eq!(load_prefs(), (Density::Regular, AgentSort::Attention, None));
        save_prefs(Density::Extended, AgentSort::Squad);
        assert_eq!(
            load_prefs(),
            (Density::Extended, AgentSort::Squad, None),
            "save_prefs leaves width untouched"
        );
    }

    #[test]
    fn width_round_trips_and_coexists_with_prefs() {
        // US1: a dragged width persists, and shares the file with
        // density/sort without either clobbering the other (one locked RMW).
        let _s = Scratch::new("width-roundtrip");
        save_prefs(Density::Slim, AgentSort::Attention);
        save_width(45);
        assert_eq!(
            load_prefs(),
            (Density::Slim, AgentSort::Attention, Some(45))
        );
        // A later density write keeps the width; a later width write keeps the
        // density.
        save_prefs(Density::Extended, AgentSort::Squad);
        assert_eq!(
            load_prefs(),
            (Density::Extended, AgentSort::Squad, Some(45))
        );
        save_width(60);
        assert_eq!(
            load_prefs(),
            (Density::Extended, AgentSort::Squad, Some(60))
        );
    }

    #[test]
    fn save_preset_writes_mode_and_width_together() {
        // A preset is one choice of both fields; save_preset persists them in one
        // mutation so a reader never sees a mode paired with a stale width.
        let _s = Scratch::new("preset-atomic");
        save_width(45); // a prior dragged width
        save_preset(Density::Extended, AgentSort::Attention, 70);
        assert_eq!(
            load_prefs(),
            (Density::Extended, AgentSort::Attention, Some(70)),
            "the preset overwrote both the mode and the dragged width"
        );
    }

    #[test]
    fn corrupt_width_degrades_to_none_then_writes_clean() {
        // AC5-ERR: a non-numeric width resolves to None (canonical), never a
        // panic, and is NOT retained - the first save_width writes a clean
        // number over the corruption.
        let _s = Scratch::new("width-corrupt");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{},"density":"regular","width":"wide"}"#,
        )
        .unwrap();
        assert_eq!(load_prefs(), (Density::Regular, AgentSort::Attention, None));
        save_width(28);
        assert_eq!(load_prefs().2, Some(28));
    }

    #[test]
    fn prefs_and_sections_do_not_clobber_each_other() {
        // Both writers overlay the same file under one lock; persisting a
        // density must not drop a section choice, and vice versa.
        let _s = Scratch::new("prefs-coexist");
        let chosen: HashMap<SectionKey, SectionView> =
            [(SectionKey::Elsewhere, SectionView::Collapsed)]
                .into_iter()
                .collect();
        save(&chosen);
        save_prefs(Density::Slim, AgentSort::Attention);
        assert_eq!(load_prefs(), (Density::Slim, AgentSort::Attention, None));
        assert_eq!(load()[&SectionKey::Elsewhere], SectionView::Collapsed);
        // And the reverse order.
        save(&chosen);
        assert_eq!(load_prefs(), (Density::Slim, AgentSort::Attention, None));
    }

    #[test]
    fn corrupt_prefs_fall_back_per_field_and_never_refuse() {
        // AC7-FR: an unparsable density resolves to Regular. Degradation is
        // PER FIELD, so a readable sort beside it still survives.
        let _s = Scratch::new("prefs-corrupt");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{},"density":"holographic","sort":"status"}"#,
        )
        .unwrap();
        assert_eq!(load_prefs(), (Density::Regular, AgentSort::Attention, None));
        // Wholly corrupt file: still the defaults, still no panic.
        std::fs::write(view_path(), "{ not json").unwrap();
        assert_eq!(load_prefs(), (Density::Regular, AgentSort::Attention, None));
        // And the next gesture writes cleanly over it.
        save_prefs(Density::Extended, AgentSort::Squad);
        assert_eq!(load_prefs().0, Density::Extended);
    }

    #[test]
    fn sort_preferences_persist_explicit_shape_and_migrate_legacy_values() {
        let _s = Scratch::new("legacy-sort-values");
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{},"density":"extended","sort":"squad"}"#,
        )
        .unwrap();
        assert_eq!(load_prefs().1, AgentSort::Squad);
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{},"density":"extended","sort":"attention"}"#,
        )
        .unwrap();
        assert_eq!(load_prefs().1, AgentSort::Attention);
        std::fs::write(
            view_path(),
            r#"{"version":1,"sections":{},"density":"extended","sort":"status"}"#,
        )
        .unwrap();
        assert_eq!(load_prefs().1, AgentSort::Attention);
        save_prefs(Density::Extended, AgentSort::Squad);
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(view_path()).unwrap()).unwrap();
        assert!(
            raw["sort"].get("column").is_some(),
            "new preferences persist an explicit sort column and direction"
        );
    }

    #[test]
    fn density_cycles_three_states_and_sort_toggles() {
        // Every press changes state, so no press can be visually inert.
        assert_eq!(Density::Regular.next(), Density::Extended);
        assert_eq!(Density::Extended.next(), Density::Slim);
        assert_eq!(Density::Slim.next(), Density::Regular);
        // Three presses return to the start: the cycle is closed.
        let d = Density::Regular;
        assert_eq!(d.next().next().next(), d);
        assert_eq!(AgentSort::Squad.toggle(), AgentSort::Attention);
        assert_eq!(AgentSort::Attention.toggle(), AgentSort::Squad);
    }
}

/// The board's column layout as the store sees it: JSON so the schema can
/// evolve without a breaking read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoardLayout {
    pub columns: Vec<String>,
    pub focus_pct: u16,
}

/// Read the board layout pref. Absent or corrupt reads as the shipped
/// default: every model column, model order, half focus.
pub fn load_board_layout() -> BoardLayout {
    #[cfg(test)]
    if TEST_PATH.with(|c| c.borrow().is_none()) {
        return BoardLayout {
            columns: backlog_default_columns(),
            focus_pct: 50,
        };
    }
    let cfg = read_raw()
        .board_layout
        .and_then(|v| serde_json::from_value::<BoardLayout>(v).ok());
    cfg.unwrap_or(BoardLayout {
        columns: backlog_default_columns(),
        focus_pct: 50,
    })
}

/// Persist the board layout pref. Best-effort like every other write here.
pub fn save_board_layout(layout: &BoardLayout) {
    mutate(|file| {
        file.board_layout = serde_json::to_value(layout).ok();
    });
}

/// The model's column names, as the store's default.
fn backlog_default_columns() -> Vec<String> {
    crate::backlog_view::KANBAN_COLUMNS
        .iter()
        .map(|s| s.to_string())
        .collect()
}
