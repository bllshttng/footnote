//! Squads: manual cross-repo grid teams (x-5b3e, Phase 2 of sidelines).
//!
//! A squad is a user-named team formed by RECRUITING already-spawned agents by
//! reference - a playlist, never a move: a recruited agent still appears under
//! its derived repo sideline (`group_by`) AND in every squad that recruited it.
//! This is the differentiator vs `claude agents` (cwd-only, claude-only, cannot
//! form a cross-repo team).
//!
//! ## Two decisions that diverge from the design doc's locked list (x-5b3e)
//!
//! - **Membership keys on the agent `name`, not a `session_uuid`.** The grid's
//!   registry rows carry no session uuid - sessions are identified by `name`
//!   (`run.rs` opens one watch pane per registry name). `name` is also MORE
//!   respawn-stable than a uuid: a respawn reusing the same name auto-rejoins
//!   its squads, where a uuid is re-minted on every respawn (the design's own
//!   Open Questions flag uuid as un-rematchable). Confirmed with the maintainer.
//! - **Recruit binds to plain `m` on the rail, not `leader m`.** The leader-key
//!   model (x-b563) landed but is TILED-ONLY; the rail keeps its own
//!   RailNav/PaneDrive keymap until the rail leader (x-d97d) lands. So recruit
//!   is a plain `m` arm in the rail keymap today; [`SquadStore::recruit`] is the
//!   reusable verb both that binding and a future `leader m` call into, so the
//!   rebind is a one-line change. Keyed on `name` per above.
//!
//! Persisted GLOBALLY at `~/.fno/squads.json` (a squad spans repos, so it cannot
//! live in any one project's `.fno`). Writes go through [`update`], which mirrors
//! the registry's flock-protected read-modify-write + atomic tempfile/rename
//! (`state.rs`): the exclusive lock is held across the whole RMW so two grid
//! instances cannot interleave a recruit (last writer re-reads first).

use crate::grid::group::Group;
use crate::paths::AgentsHome;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One manual team: a name, its recruited member agent-names (by reference,
/// deduped), and a creation stamp.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Squad {
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub created_at: String,
}

/// The whole `~/.fno/squads.json` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SquadStore {
    #[serde(default)]
    pub squads: Vec<Squad>,
}

/// The result of a recruit attempt - drives the recruit toast (AC1-UI: a
/// recruit MUST show feedback, and re-recruiting an existing member is a
/// *visible* no-op).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecruitOutcome {
    /// The member was added to the squad.
    Recruited,
    /// The member was already in the squad (idempotent no-op).
    AlreadyMember,
    /// No squad by that name exists.
    NoSuchSquad,
}

impl SquadStore {
    /// The squad with this name, if any.
    pub fn find(&self, name: &str) -> Option<&Squad> {
        self.squads.iter().find(|s| s.name == name)
    }

    /// Create a new squad. Returns `false` (and does nothing) when the name is
    /// blank (Boundaries: reject a nameless squad) or already taken (name
    /// uniqueness). `created_at` is passed in so creation stays pure/testable.
    pub fn create(&mut self, name: &str, created_at: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.find(name).is_some() {
            return false;
        }
        self.squads.push(Squad {
            name: name.to_string(),
            members: Vec::new(),
            created_at: created_at.to_string(),
        });
        true
    }

    /// Recruit `member` (an agent name) into the named squad. Idempotent: a
    /// re-recruit of an existing member is an `AlreadyMember` no-op (dedup on
    /// name - Invariant: member uniqueness). Returns `NoSuchSquad` when the
    /// squad is absent.
    pub fn recruit(&mut self, squad: &str, member: &str) -> RecruitOutcome {
        let Some(s) = self.squads.iter_mut().find(|s| s.name == squad) else {
            return RecruitOutcome::NoSuchSquad;
        };
        if s.members.iter().any(|m| m == member) {
            return RecruitOutcome::AlreadyMember;
        }
        s.members.push(member.to_string());
        RecruitOutcome::Recruited
    }

    /// Remove `member` from the named squad (explicit removal - an exited member
    /// is a tombstone kept until removed this way). Returns `true` iff a member
    /// was actually dropped.
    pub fn remove_member(&mut self, squad: &str, member: &str) -> bool {
        let Some(s) = self.squads.iter_mut().find(|s| s.name == squad) else {
            return false;
        };
        let before = s.members.len();
        s.members.retain(|m| m != member);
        s.members.len() != before
    }
}

/// Build the rail [`Group`]s for every squad, resolving each squad's member
/// names against `rows` (the same registry-row slice `group::group_by` buckets).
///
/// A squad becomes a `Group` whose `members` are the row INDICES of its live
/// members (a stored name with no live row is simply absent - it never tiles,
/// satisfying AC1-EDGE's "drops from the live grid" for free; its offline
/// presence is reported by [`offline_members`]). Members are name-sorted to
/// match `group_by`'s member ordering. The header is `*<name>` (the `*` marker
/// distinguishes a manual squad from a derived sideline); `key_value` is
/// `squad:<name>` so it can never collide with a repo-root path key.
///
/// An empty squad (zero live members) still yields a `Group` so the rail shows
/// it - the GroupTile path already guards a 0-member selection (`selected_group`
/// returns `None`), so an empty squad renders an empty view, never a degenerate
/// grid.
///
/// KNOWN v1 LIMITATION (codex peer P1): membership is by reference, so the same
/// agent name can be in two squads, which puts the SAME row index in two of the
/// `Group`s returned here. The rail selects by agent index (`selected_agent_idx`),
/// so in the Squad view `selected_group` resolves such a shared agent to the
/// FIRST squad and the render highlights every occurrence - a second squad whose
/// members are all shared can be hard to select/tile. The fix is a group-aware
/// selection cursor (flat row position, not a bare index) - the same nav-identity
/// rework the simultaneous sideline+squad union needs. Tracked as a carveout;
/// distinct-membership squads (the common case) are unaffected.
pub fn squad_groups(rows: &[Value], store: &SquadStore) -> Vec<Group> {
    store
        .squads
        .iter()
        .map(|squad| {
            // (name, index) for every live row whose name is a member, name-sorted.
            let mut hits: Vec<(&str, usize)> = rows
                .iter()
                .enumerate()
                .filter_map(|(idx, row)| {
                    let name = row.get("name").and_then(Value::as_str)?;
                    squad
                        .members
                        .iter()
                        .any(|m| m == name)
                        .then_some((name, idx))
                })
                .collect();
            hits.sort_by(|a, b| a.0.cmp(b.0));
            // Surface offline (ghost) members in the header so a reopened squad
            // whose member is not live reads as `*stack +1 off`, never as a
            // silently smaller squad (AC1-EDGE / AC1-FR). The rail appends the
            // live `(count)` after this, giving `*stack +1 off (2)`. Counted
            // without allocating (this runs every paint frame): a recruited name
            // is offline iff no live row carries it.
            let off = squad
                .members
                .iter()
                .filter(|m| {
                    !rows
                        .iter()
                        .any(|r| r.get("name").and_then(Value::as_str) == Some(m.as_str()))
                })
                .count();
            let header = if off > 0 {
                format!("*{} +{off} off", squad.name)
            } else {
                format!("*{}", squad.name)
            };
            Group {
                header,
                key_value: format!("squad:{}", squad.name),
                members: hits.into_iter().map(|(_, idx)| idx).collect(),
            }
        })
        .collect()
}

/// The member names of `squad` that have NO live row in `rows` (offline /
/// ghost members). These are kept in the store as tombstones (composition is
/// stable across churn) and surfaced in the rail as an offline count rather
/// than silently dropped (AC1-EDGE / AC1-FR). Sorted for a stable display.
pub fn offline_members(rows: &[Value], squad: &Squad) -> Vec<String> {
    let live: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.get("name").and_then(Value::as_str))
        .collect();
    let mut out: Vec<String> = squad
        .members
        .iter()
        .filter(|m| !live.contains(&m.as_str()))
        .cloned()
        .collect();
    out.sort();
    out
}

// ── Persistence (mirrors state.rs: flock sidecar + atomic rename) ─────────────

/// `~/.fno/squads.json` - GLOBAL (sibling of the agents tree, alongside
/// graph.json / ledger.json). Derived from [`AgentsHome`] so `FNO_AGENTS_HOME`
/// redirects it too: the agents root is `<base>/agents`, so its parent is the
/// `.fno` base that holds squads.json.
pub fn squads_path() -> PathBuf {
    let home = AgentsHome::from_env();
    home.root()
        .parent()
        .map(|p| p.join("squads.json"))
        .unwrap_or_else(|| PathBuf::from("squads.json"))
}

/// Load the store, never failing: an absent file is an empty store and a
/// corrupt/unparseable file is an empty store (AC1-ERR: the grid must start
/// with no squads and never crash on a malformed store). Use [`load_reporting`]
/// at startup to also surface the corruption warning.
pub fn load(path: &Path) -> SquadStore {
    load_reporting(path).0
}

/// Like [`load`] but also returns a one-line warning when the store was present
/// but unparseable (AC1-ERR's warning). The caller surfaces it as a grid hint
/// rather than `eprintln!`, which would corrupt the active TUI (raw mode /
/// alternate screen) - never write to stderr while the compositor owns the
/// terminal.
pub fn load_reporting(path: &Path) -> (SquadStore, Option<String>) {
    let lock = acquire_shared(&lock_path(path));
    let out = read_tolerant(path);
    if let Some(l) = lock {
        let _ = l.unlock();
    }
    out
}

/// Read-modify-write the store under an exclusive lock, publishing atomically.
/// The lock is held across the whole RMW so two grid instances never interleave
/// a recruit; the read happens INSIDE the lock so the last writer re-reads the
/// other's change first (Concurrency invariant). The closure mutates in place
/// and returns a value handed back to the caller (e.g. a [`RecruitOutcome`]).
pub fn update<F, T>(path: &Path, f: F) -> std::io::Result<T>
where
    F: FnOnce(&mut SquadStore) -> T,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Lock a stable `.lock` sidecar, not the data file: renaming the data file
    // out from under a held flock would invalidate the lock fd (state.rs).
    let lock = acquire_exclusive(&lock_path(path))?;
    let (mut store, _warn) = read_tolerant(path);
    let out = f(&mut store);
    let res = write_atomic(path, &store);
    let _ = lock.unlock();
    res?;
    Ok(out)
}

/// Read + parse the store with no lock. Returns the parsed store (or an empty
/// one on absence / read error / parse failure) plus a warning string when the
/// file was present but unparseable. NEVER writes to stderr (the caller owns the
/// TUI); corruption is reported through the returned warning instead.
fn read_tolerant(path: &Path) -> (SquadStore, Option<String>) {
    let mut buf = String::new();
    match OpenOptions::new().read(true).open(path) {
        Ok(mut file) => {
            if file.read_to_string(&mut buf).is_err() {
                return (SquadStore::default(), None);
            }
        }
        Err(_) => return (SquadStore::default(), None),
    }
    if buf.trim().is_empty() {
        return (SquadStore::default(), None);
    }
    match serde_json::from_str(&buf) {
        Ok(store) => (store, None),
        Err(e) => (
            SquadStore::default(),
            Some(format!("ignoring malformed {}: {e}", path.display())),
        ),
    }
}

fn write_atomic(path: &Path, store: &SquadStore) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(store)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// Exclusive lock on the sidecar for a MUTATING read-modify-write. Fails CLOSED
/// (propagates the lock error) like `state.rs::acquire_exclusive`: a recruit must
/// hold the lock or not write at all - a fail-open write could lose a concurrent
/// grid instance's recruit, defeating the whole point of the lock (codex peer P2).
fn acquire_exclusive(lock_file: &Path) -> std::io::Result<File> {
    if let Some(parent) = lock_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_file)?;
    file.lock()?;
    Ok(file)
}

fn acquire_shared(lock_file: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_file)
        .ok()?;
    file.lock_shared().ok()?;
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(names: &[&str]) -> Vec<Value> {
        names.iter().map(|n| json!({ "name": n })).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-squads-{}-{}-{}.json",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    // ── create: blank + dup rejected (Boundaries, name uniqueness) ──────────
    #[test]
    fn create_rejects_blank_and_duplicate() {
        let mut s = SquadStore::default();
        assert!(!s.create("   ", "t"), "blank name rejected");
        assert!(s.squads.is_empty());
        assert!(s.create("stack", "t"));
        assert!(!s.create("stack", "t"), "duplicate name rejected");
        assert!(!s.create("  stack  ", "t"), "duplicate after trim rejected");
        assert_eq!(s.squads.len(), 1);
    }

    // ── recruit: dedup + no-such (AC1-UI idempotency, member uniqueness) ─────
    #[test]
    fn recruit_dedups_and_reports_outcome() {
        let mut s = SquadStore::default();
        assert_eq!(s.recruit("ghost", "wkA"), RecruitOutcome::NoSuchSquad);
        s.create("stack", "t");
        assert_eq!(s.recruit("stack", "wkA"), RecruitOutcome::Recruited);
        assert_eq!(
            s.recruit("stack", "wkA"),
            RecruitOutcome::AlreadyMember,
            "re-recruit is a visible no-op"
        );
        assert_eq!(s.find("stack").unwrap().members, vec!["wkA"]);
    }

    #[test]
    fn remove_member_drops_only_named() {
        let mut s = SquadStore::default();
        s.create("stack", "t");
        s.recruit("stack", "wkA");
        s.recruit("stack", "wkB");
        assert!(s.remove_member("stack", "wkA"));
        assert!(!s.remove_member("stack", "wkA"), "second remove is a no-op");
        assert_eq!(s.find("stack").unwrap().members, vec!["wkB"]);
    }

    // ── AC1-HP: a squad resolves to a cross-repo group of live rows ──────────
    #[test]
    fn squad_groups_resolves_names_to_indices_sorted() {
        // Members recruited cross-repo; rows are in a different order than the
        // recruit order, so this also proves name-sorting (not recruit order).
        let r = rows(&["wkZ", "other", "wkA"]); // idx 0,1,2
        let mut store = SquadStore::default();
        store.create("stack", "t");
        store.recruit("stack", "wkZ");
        store.recruit("stack", "wkA");
        let groups = squad_groups(&r, &store);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].header, "*stack");
        assert_eq!(groups[0].key_value, "squad:stack");
        // wkA (idx 2) sorts before wkZ (idx 0); "other" (idx 1) is not a member.
        assert_eq!(groups[0].members, vec![2, 0]);
    }

    #[test]
    fn squad_groups_empty_squad_yields_empty_group() {
        let r = rows(&["wkA"]);
        let mut store = SquadStore::default();
        store.create("empty", "t"); // no members recruited
        let groups = squad_groups(&r, &store);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].members.is_empty(), "renders, but tiles nothing");
    }

    #[test]
    fn squad_groups_ghost_member_absent_from_live_group() {
        // A recruited name with no live row never appears in the tiled group.
        let r = rows(&["wkA"]);
        let mut store = SquadStore::default();
        store.create("stack", "t");
        store.recruit("stack", "wkA");
        store.recruit("stack", "wkGONE");
        let groups = squad_groups(&r, &store);
        assert_eq!(groups[0].members, vec![0], "only the live member tiles");
        assert_eq!(
            groups[0].header, "*stack +1 off",
            "the offline ghost is surfaced in the header, never silently dropped"
        );
    }

    // ── AC1-EDGE: offline members are reported, never dropped from the store ─
    #[test]
    fn offline_members_reports_ghosts() {
        let r = rows(&["wkA", "wkB"]);
        let mut store = SquadStore::default();
        store.create("stack", "t");
        store.recruit("stack", "wkB");
        store.recruit("stack", "wkGONE");
        store.recruit("stack", "wkAlsoGone");
        let off = offline_members(&r, store.find("stack").unwrap());
        assert_eq!(off, vec!["wkAlsoGone", "wkGONE"], "sorted, only ghosts");
    }

    // ── AC1-ERR: corrupt store -> empty + no panic ──────────────────────────
    #[test]
    fn load_corrupt_store_is_empty_not_a_panic() {
        let p = tmp("corrupt");
        std::fs::write(&p, b"{ this is not json").unwrap();
        let store = load(&p);
        assert!(store.squads.is_empty(), "malformed store degrades to empty");
        // The warning is RETURNED (for the caller to show as a TUI hint), not
        // printed to stderr (which would corrupt the active compositor).
        let (store2, warn) = load_reporting(&p);
        assert!(store2.squads.is_empty());
        assert!(
            warn.is_some(),
            "corruption is reported through the return value"
        );
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(lock_path(&p)).ok();
    }

    #[test]
    fn load_absent_store_is_empty() {
        let p = tmp("absent");
        assert!(load(&p).squads.is_empty());
    }

    // ── AC1-FR: persistence round-trips through update + load ────────────────
    #[test]
    fn update_then_load_restores_squad_and_members() {
        let p = tmp("persist");
        let out = update(&p, |s| {
            s.create("stack", "2026-06-28T00:00:00Z");
            s.recruit("stack", "wkA")
        })
        .unwrap();
        assert_eq!(out, RecruitOutcome::Recruited);
        // A fresh reader sees the squad and its member (survives "reopen").
        let reloaded = load(&p);
        assert_eq!(reloaded.squads.len(), 1);
        let sq = reloaded.find("stack").unwrap();
        assert_eq!(sq.members, vec!["wkA"]);
        assert_eq!(sq.created_at, "2026-06-28T00:00:00Z");
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(lock_path(&p)).ok();
    }

    #[test]
    fn update_reread_under_lock_sees_prior_write() {
        // Two sequential updates: the second must re-read the first's write
        // (last-writer-re-reads), not clobber it from a stale in-memory store.
        let p = tmp("reread");
        update(&p, |s| s.create("a", "t")).unwrap();
        update(&p, |s| s.create("b", "t")).unwrap();
        let store = load(&p);
        let names: Vec<&str> = store.squads.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a", "b"],
            "second update kept the first's squad"
        );
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(lock_path(&p)).ok();
    }
}
