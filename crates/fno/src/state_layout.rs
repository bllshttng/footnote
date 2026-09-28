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
}
