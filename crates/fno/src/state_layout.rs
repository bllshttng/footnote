//! The `fno` crate's copy of the state-root layout resolver: the same table
//! (`docs/state-root-layout.tsv`, read at build time) and the same `place`
//! semantics as `fno-agents/src/state_layout.rs` - the dual-implementation
//! inventory pattern. No migration lives here; the daemon crate owns it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const LAYOUT_TSV: &str = include_str!("state-root-layout.tsv");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Sqlite,
    Anchor,
    Append,
    Overwrite,
    Marker,
    Page,
    Lock,
    Park,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    Daemon,
    Mux,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub legacy: String,
    pub new: String,
    pub kind: Kind,
    pub owner: Owner,
}

/// Parse the table. Failures name their 1-based line, so a bad row can never
/// ship silently (pinned by unit tests).
pub fn parse_table(tsv: &str) -> Result<Vec<Row>, String> {
    let mut out = Vec::new();
    for (idx, line) in tsv.lines().enumerate() {
        let no = idx + 1;
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let bad = |why: &str| -> String {
            format!("state-root layout table line {no}: {why} ({line:?})")
        };
        let [legacy, new, kind, owner] = fields.as_slice() else {
            return Err(bad("expected 4 TAB-separated fields"));
        };
        if legacy.is_empty() || new.is_empty() {
            return Err(bad("empty name"));
        }
        let kind = match Kind::parse(kind) {
            Some(k) => k,
            None => return Err(bad("unknown kind")),
        };
        let owner = match Owner::parse(owner) {
            Some(o) => o,
            None => return Err(bad("unknown owner")),
        };
        out.push(Row {
            legacy: legacy.to_string(),
            new: new.to_string(),
            kind,
            owner,
        });
    }
    if out.is_empty() {
        return Err("state-root layout table: no rows".to_string());
    }
    Ok(out)
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sqlite" => Self::Sqlite,
            "anchor" => Self::Anchor,
            "append" => Self::Append,
            "overwrite" => Self::Overwrite,
            "marker" => Self::Marker,
            "page" => Self::Page,
            "lock" => Self::Lock,
            "park" => Self::Park,
            _ => return None,
        })
    }
}

impl Owner {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "daemon" => Self::Daemon,
            "mux" => Self::Mux,
            _ => return None,
        })
    }
}

/// The shipped rows, parsed once. A malformed shipped table panics loud
/// rather than resolving guessed paths.
pub fn rows() -> &'static [Row] {
    static ROWS: OnceLock<&'static [Row]> = OnceLock::new();
    *ROWS.get_or_init(|| {
        let parsed = parse_table(LAYOUT_TSV).unwrap_or_else(|e| panic!("{e}"));
        Box::leak(parsed.into_boxed_slice())
    })
}

pub fn find(legacy_name: &str) -> Option<&'static Row> {
    rows().iter().find(|r| r.legacy == legacy_name)
}

/// The anchor probe: a virtual graph.json exists when its .db sibling does.
fn db_twin(p: &Path) -> PathBuf {
    p.with_extension("db")
}

/// Where `legacy_name` lives under `root`: the new path when it exists, else
/// the legacy path when it exists, else the new path (fresh install). The
/// `anchor` kind probes the .db twin. A name the table does not carry
/// resolves to root/<name> unchanged.
pub fn place(root: &Path, legacy_name: &str) -> PathBuf {
    let Some(row) = find(legacy_name) else {
        return root.join(legacy_name);
    };
    let new = root.join(&row.new);
    let legacy = root.join(&row.legacy);
    match row.kind {
        Kind::Anchor => {
            if db_twin(&new).exists() {
                new
            } else if db_twin(&legacy).exists() {
                legacy
            } else {
                new
            }
        }
        _ => {
            if new.exists() {
                new
            } else if legacy.exists() {
                legacy
            } else {
                new
            }
        }
    }
}

/// The mux server moves its `owner = mux` rows at its own start, before it
/// opens them: it is their long-lived writer, and agents never restart the
/// shared server. A client never moves them, so an old-build server keeps
/// writing the root file and new clients read it through the gated fallback
/// until a new-build server starts. Returns how many rows resolved.
pub fn migrate_mux_sidecars_at(root: &Path) -> usize {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format!("mux-{secs}");
    let mut resolved = 0;
    for row in rows() {
        if row.owner != Owner::Mux {
            continue;
        }
        let legacy = root.join(&row.legacy);
        if !legacy.exists() {
            continue;
        }
        let new = root.join(&row.new);
        let ok = match row.kind {
            Kind::Lock => mux_lock_can_leave(root, row),
            Kind::Overwrite => {
                if new.exists() {
                    if mtime_of(&legacy) > mtime_of(&new) {
                        move_mux(&legacy, &new)
                    } else {
                        park_mux(root, &stamp, &legacy)
                    }
                } else {
                    move_mux(&legacy, &new)
                }
            }
            _ => continue,
        };
        if ok {
            resolved += 1;
        }
    }
    resolved
}

/// A legacy lock leaves only once its data file sits at the new path (so no
/// waiter still needs it) and a non-blocking flock succeeds (so no holder
/// still owns it).
fn mux_lock_can_leave(root: &Path, row: &Row) -> bool {
    let legacy = root.join(&row.legacy);
    let base = row.legacy.strip_suffix(".lock").unwrap_or(&row.legacy);
    let data_moved = match find(base) {
        Some(base_row) => root.join(&base_row.new).exists(),
        None => false,
    };
    if !data_moved {
        return false;
    }
    match std::fs::File::open(&legacy) {
        Ok(f) => {
            let rc = unsafe {
                libc::flock(
                    std::os::unix::io::AsRawFd::as_raw_fd(&f),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            };
            if rc != 0 {
                return false;
            }
            std::fs::remove_file(&legacy).is_ok()
        }
        Err(_) => false,
    }
}

fn move_mux(legacy: &Path, new: &Path) -> bool {
    if let Some(parent) = new.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    std::fs::rename(legacy, new).is_ok()
}

/// The run's backup folder, `backups/state-root-migration/mux-<unix secs>/`.
fn park_mux(root: &Path, stamp: &str, legacy: &Path) -> bool {
    let dir = root
        .join("backups")
        .join("state-root-migration")
        .join(stamp);
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    match legacy.file_name() {
        Some(name) => std::fs::rename(legacy, dir.join(name)).is_ok(),
        None => false,
    }
}

fn mtime_of(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// The gated sidecar resolution the mux stores share: `mux/<file>` when it
/// exists, else the legacy root spelling when allowed and present, else
/// `mux/<file>` (fresh install). The legacy gate is the test-isolation read
/// (`proto::legacy_fallback_allowed`), which `place` cannot see.
pub fn resolve_sidecar(root: &Path, file: &str, legacy_allowed: bool) -> PathBuf {
    let new = root.join("mux").join(file);
    if new.exists() {
        return new;
    }
    let legacy = root.join(file);
    if legacy_allowed && legacy.exists() {
        return legacy;
    }
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_table_parses() {
        let rows = parse_table(LAYOUT_TSV).expect("shipped table must parse");
        assert!(
            rows.len() > 40,
            "expected the full table, got {}",
            rows.len()
        );
    }

    #[test]
    fn parse_rejects_bad_rows_naming_the_line() {
        let err = parse_table("a\tx/a\tmarker\tdaemon\nbad\tnew/bad\tmarker\n").unwrap_err();
        assert!(err.contains("line 2"), "error must name the line: {err}");
        let err = parse_table("a\tx/a\tmarker\tdaemon\nb\tx/b\tvault\tdaemon\n").unwrap_err();
        assert!(err.contains("line 2"), "error must name the line: {err}");
    }

    #[test]
    fn place_resolves_new_legacy_and_default() {
        let root = std::env::temp_dir().join(format!("fno-place-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            place(&root, "graph.json"),
            root.join("db").join("graph.json")
        );
        std::fs::write(root.join("graph.db"), b"x").unwrap();
        assert_eq!(place(&root, "graph.json"), root.join("graph.json"));
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join("db").join("graph.db"), b"x").unwrap();
        assert_eq!(
            place(&root, "graph.json"),
            root.join("db").join("graph.json")
        );
        std::fs::remove_dir_all(&root).ok();
    }
    #[test]
    fn vendored_table_matches_the_repo_copy() {
        let repo = include_str!("../../../docs/state-root-layout.tsv");
        assert_eq!(
            LAYOUT_TSV, repo,
            "the vendored layout table drifted from docs/state-root-layout.tsv;              edit the repo copy and copy it into both crates"
        );
    }

    #[test]
    fn mux_migration_moves_the_sidecars() {
        let root = std::env::temp_dir().join(format!("fno-mux-mig-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("squads.json"), b"members").unwrap();
        std::fs::write(root.join("session-names.json"), b"{}").unwrap();
        let moved = migrate_mux_sidecars_at(&root);
        assert!(moved >= 2, "expected the two data rows, moved {moved}");
        assert_eq!(
            std::fs::read(root.join("mux").join("squads.json")).unwrap(),
            b"members"
        );
        assert_eq!(
            std::fs::read(root.join("mux").join("session-names.json")).unwrap(),
            b"{}"
        );
        assert!(!root.join("squads.json").exists());
        assert!(!root.join("session-names.json.root-move").exists());
        assert_eq!(
            place(&root, "squads.json"),
            root.join("mux").join("squads.json")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mux_lock_leaves_only_after_its_data_file() {
        let root = std::env::temp_dir().join(format!("fno-mux-lock-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("mux-view.json.lock"), b"").unwrap();
        let moved = migrate_mux_sidecars_at(&root);
        assert_eq!(moved, 0, "the lock waits for its data file");
        assert!(root.join("mux-view.json.lock").exists());
        std::fs::create_dir_all(root.join("mux")).unwrap();
        std::fs::write(root.join("mux").join("mux-view.json"), b"{}").unwrap();
        let moved = migrate_mux_sidecars_at(&root);
        assert!(moved >= 1, "the lock leaves once its data file moved");
        assert!(!root.join("mux-view.json.lock").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mux_both_exist_newer_side_wins() {
        let root = std::env::temp_dir().join(format!("fno-mux-both-{}", std::process::id()));
        std::fs::create_dir_all(root.join("mux")).unwrap();
        let legacy_path = root.join("squads.json");
        let new_path = root.join("mux").join("squads.json");
        std::fs::write(&legacy_path, b"legacy-newer").unwrap();
        std::fs::write(&new_path, b"new-older").unwrap();
        let older = std::time::SystemTime::UNIX_EPOCH;
        let newer = std::time::SystemTime::now();
        touch_mtime(&legacy_path, newer);
        touch_mtime(&new_path, older);
        let moved = migrate_mux_sidecars_at(&root);
        assert!(moved >= 1, "the overlap row resolved, moved {moved}");
        assert_eq!(
            std::fs::read(&new_path).unwrap(),
            b"legacy-newer",
            "the newer legacy bytes win"
        );
        assert!(!legacy_path.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sidecar_resolution_matches_the_acceptance() {
        let root = std::env::temp_dir().join(format!("fno-mux-res-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("squads.json"), b"members").unwrap();
        // Unmigrated root, client lane: the legacy file is the store.
        assert_eq!(
            resolve_sidecar(&root, "squads.json", true),
            root.join("squads.json")
        );
        assert!(!root.join("mux").exists(), "nothing lands under mux/");
        // Gate off: even a present legacy file never resolves (test isolation).
        assert_eq!(
            resolve_sidecar(&root, "squads.json", false),
            root.join("mux").join("squads.json")
        );
        // Migrated root: mux/ wins over the parked legacy twin.
        std::fs::create_dir_all(root.join("mux")).unwrap();
        std::fs::write(root.join("mux").join("squads.json"), b"new").unwrap();
        assert_eq!(
            resolve_sidecar(&root, "squads.json", true),
            root.join("mux").join("squads.json")
        );
        // Fresh root: mux/ is the default.
        let fresh = std::env::temp_dir().join(format!("fno-mux-fresh-{}", std::process::id()));
        std::fs::create_dir_all(&fresh).unwrap();
        assert_eq!(
            resolve_sidecar(&fresh, "mux-view.json", true),
            fresh.join("mux").join("mux-view.json")
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&fresh).ok();
    }

    fn touch_mtime(p: &Path, t: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t).set_accessed(t))
            .unwrap();
    }
}
