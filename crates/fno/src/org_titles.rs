//! Tolerant title read for the mux: `teams.<scope>.title` from
//! `team_names.json` beside `registry.json`. The mux never links fno-agents,
//! so this mirrors the FILE contract (the same shape
//! `crates/fno-agents/src/team_names.rs` freezes). A missing or malformed
//! file reads as no title, like every other registry-file read here.

use std::collections::BTreeMap;
use std::path::Path;

/// Every stored title keyed by its scope key. Tolerant: a missing or
/// malformed file reads as an empty map.
pub fn titles(store_path: &Path) -> BTreeMap<String, String> {
    let bytes = match std::fs::read(store_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            // One release of dual-read: a store the daemon-start move has
            // not reached yet still lives under the pre-rename name.
            if store_path.file_name().and_then(|n| n.to_str()) == Some("team_names.json") {
                let legacy = store_path.with_file_name("crown_names.json");
                match std::fs::read(legacy) {
                    Ok(bytes) => bytes,
                    Err(_) => return BTreeMap::new(),
                }
            } else {
                return BTreeMap::new();
            }
        }
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(teams) = value.get("teams").and_then(|c| c.as_object()) {
        for (scope, rec) in teams {
            if let Some(title) = rec.get("title").and_then(|t| t.as_str()) {
                out.insert(scope.clone(), title.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tolerant_title_reads() {
        fn a_missing_or_malformed_store_reads_as_no_title() {
            let tmp = tempfile::TempDir::new().unwrap();
            assert!(titles(&tmp.path().join("team_names.json")).is_empty());
            std::fs::write(tmp.path().join("team_names.json"), "{not json").unwrap();
            assert!(titles(&tmp.path().join("team_names.json")).is_empty());
        }

        fn titles_read_from_the_store_and_skip_records_without_one() {
            let tmp = tempfile::TempDir::new().unwrap();
            let path = tmp.path().join("team_names.json");
            std::fs::write(
                &path,
                r#"{"version": 1, "teams": {
                    "x-aaaa": {"name": "kestrel", "regnal": 1, "nodes": [], "updated_at": "t",
                               "title": "Lead of native backlog"},
                    "fno": {"name": "folio", "regnal": 1, "nodes": [], "updated_at": "t"}
                }}"#,
            )
            .unwrap();
            let map = titles(&path);
            assert_eq!(
                map.get("x-aaaa").map(String::as_str),
                Some("Lead of native backlog")
            );
            assert!(!map.contains_key("fno"));
        }
        a_missing_or_malformed_store_reads_as_no_title();
        titles_read_from_the_store_and_skip_records_without_one();
    }
    use super::*;
}
