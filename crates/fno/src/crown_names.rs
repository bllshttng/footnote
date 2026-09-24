//! The mux reader of the crown-name file contract. fno-agents owns
//! `~/.fno/agents/crown_names.json` (see `fno-agents/src/crown_names.rs` for
//! the frozen shape); the mux applies no liveness rule of its own - a
//! missing or malformed file reads as no names, because the sideline is a
//! display and fno-agents keeps the file pruned (the succession sweep).

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The crown-name path beside the registry, resolved the way
/// `agents_view::registry_path` resolves the registry.
pub(crate) fn crown_names_path() -> PathBuf {
    crate::agents_view::registry_path().with_file_name("crown_names.json")
}

/// Canonical scope the way the store keys it (members split on commas,
/// trimmed, sorted, deduped, comma-joined). A focused copy of
/// `fno-agents/src/territory.rs::canonical_scope`; the mux reads the FILE
/// contract, not the crate.
fn canonical_scope(scope: &str) -> String {
    let mut members: Vec<String> = scope
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    members.sort();
    members.dedup();
    members.join(",")
}

/// The two lookups the row stamp reads: canonical scope -> display name,
/// and node id -> display name. A missing or malformed file reads as empty
/// (a display reader never refuses to render).
pub(crate) fn read_crown_names() -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    read_crown_names_from(&crown_names_path())
}

/// [`read_crown_names`] for an explicit path (tests pass a tempdir).
fn read_crown_names_from(
    path: &std::path::Path,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return (BTreeMap::new(), BTreeMap::new()),
    };
    let doc: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return (BTreeMap::new(), BTreeMap::new()),
    };
    let empty = serde_json::Map::new();
    let by_scope = doc
        .get("crowns")
        .and_then(|c| c.as_object())
        .unwrap_or(&empty);
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut by_node: BTreeMap<String, String> = BTreeMap::new();
    for (scope, rec) in by_scope {
        let display = match display_of(rec) {
            Some(d) => d,
            None => continue,
        };
        names.insert(canonical_scope(scope), display.clone());
        for node in rec
            .get("nodes")
            .and_then(|n| n.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            by_node.insert(node.to_string(), display.clone());
        }
    }
    (names, by_node)
}

/// The display string (`Barnaby II`), capitalized like the store's
/// `display` - the file is the contract, so the reader re-derives it from
/// `name` + `regnal` rather than trusting a stored display field.
fn display_of(rec: &Value) -> Option<String> {
    let name = rec.get("name")?.as_str()?;
    let regnal = rec.get("regnal").and_then(|r| r.as_u64()).unwrap_or(1);
    let mut chars = name.chars();
    let first = chars
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    let numeral = match regnal {
        0 | 1 => String::new(),
        2..=20 => {
            let roman = [
                "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII", "XIII", "XIV",
                "XV", "XVI", "XVII", "XVIII", "XIX", "XX",
            ];
            format!(" {}", roman[(regnal - 2) as usize])
        }
        n => format!(" {n}"),
    };
    Some(format!("{first}{}{numeral}", chars.as_str()))
}

/// The display name for one registry row: a crowned row matches by its
/// canonical `crown_scope`; any other row by its registry `node`.
pub(crate) fn crown_name_for(
    names: &BTreeMap<String, String>,
    by_node: &BTreeMap<String, String>,
    crown_scope: Option<&str>,
    node: Option<&str>,
) -> Option<String> {
    if let Some(scope) = crown_scope.filter(|s| !s.trim().is_empty()) {
        return names.get(&canonical_scope(scope)).cloned();
    }
    node.and_then(|n| by_node.get(n).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_missing_file_reads_as_no_names() {
        // Tests never touch the machine's real store: the reader variant
        // below takes an explicit path (a tempdir here), and the ambient
        // wrapper only resolves beside the registry.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("crown_names.json");
        let (names, by_node) = read_crown_names_from(&path);
        assert!(names.is_empty());
        assert!(by_node.is_empty());
    }

    #[test]
    fn a_malformed_file_reads_as_no_names_and_scope_keys_canonicalize() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("crown_names.json");
        std::fs::write(&path, "{not json").unwrap();
        let (names, by_node) = read_crown_names_from(&path);
        assert!(names.is_empty());
        assert!(by_node.is_empty());
        // A member-set scope key sorts, so the row-side canonicalization
        // finds it either spelling.
        let doc = serde_json::json!({
            "version": 1,
            "crowns": {"epic-b,epic-a": {"name": "merle", "regnal": 1,
                        "holder_session": null, "nodes": ["x-1"], "updated_at": ""}}
        });
        std::fs::write(&path, doc.to_string()).unwrap();
        let (names, by_node) = read_crown_names_from(&path);
        assert_eq!(names.get("epic-a,epic-b").unwrap(), "Merle");
        assert_eq!(by_node.get("x-1").unwrap(), "Merle");
    }

    #[test]
    fn display_capitalizes_and_numbers_the_regnal() {
        let rec = serde_json::json!({"name": "barnaby", "regnal": 2});
        assert_eq!(display_of(&rec).unwrap(), "Barnaby II");
        let rec = serde_json::json!({"name": "barnaby", "regnal": 1});
        assert_eq!(display_of(&rec).unwrap(), "Barnaby");
    }

    #[test]
    fn a_crowned_row_matches_by_scope_and_a_worker_row_by_node() {
        let names = names_map(&[("fno", "Barnaby II")]);
        let by_node = names_map(&[("x-f42d", "Barnaby II")]);
        assert_eq!(
            crown_name_for(&names, &by_node, Some("fno"), Some("x-f42d")).unwrap(),
            "Barnaby II",
            "the crowned row answers by scope, never by node"
        );
        assert_eq!(
            crown_name_for(&names, &by_node, None, Some("x-f42d")).unwrap(),
            "Barnaby II"
        );
        assert_eq!(crown_name_for(&names, &by_node, Some("other"), None), None);
    }
}
