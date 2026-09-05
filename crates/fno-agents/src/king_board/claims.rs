//! The merged both-roots claim scan (claims.cli._merge_claims_across_roots).
use super::{s_str, SourceRead};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Claims: the merged both-roots scan (claims.cli._merge_claims_across_roots)
// ---------------------------------------------------------------------------

/// Percent-decode a claim filename back to its key (io.decode_key /
/// `urllib.parse.unquote`); `%` escapes are the only ones the encoder writes.
pub(crate) fn decode_key(filename: &str) -> String {
    let bytes = filename.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One root's live + dead claim rows (core._list_claims_impl with
/// include_stale=true): every `.lock` file, classified, dead states kept.
pub(crate) fn scan_claims_dir(dir: &Path) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return rows;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".lock") {
            continue;
        }
        if path.is_dir() {
            continue; // the .expired archive dir and any future subdirs
        }
        let key = decode_key(name.trim_end_matches(".lock"));
        if !key.starts_with("node:") {
            continue;
        }
        match crate::claims::read_claim_file(&path) {
            Err(_) => continue, // gone between list and read: not a state
            Ok(rec) => {
                let state = crate::claims::classify(&rec, None);
                let state = state.as_str();
                // The board consumes live/suspect (stalled_holder's locks,
                // undriven_pr's driver read) and stale/corrupted (its own
                // queue); `free` never has a file to scan.
                if matches!(state, "live" | "suspect" | "stale" | "corrupted") {
                    rows.push(json!({
                        "key": rec.key,
                        "state": state,
                        "holder": rec.holder,
                        "host": rec.host,
                        "pid": rec.pid,
                    }));
                }
            }
        }
    }
    rows
}

/// Both roots, best-state-wins merged into one view (the Python merge's
/// priority order: live beats suspect beats stale beats corrupted).
pub(crate) fn read_claims(cwd: &Path) -> SourceRead {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let push = |dir: Option<PathBuf>, seen: &mut HashSet<PathBuf>, dirs: &mut Vec<PathBuf>| {
        if let Some(d) = dir {
            let resolved = d.canonicalize().unwrap_or_else(|_| d.clone());
            if seen.insert(resolved) {
                dirs.push(d);
            }
        }
    };
    // The global root (claims_root_for("node:") = FNO_CLAIMS_ROOT or $HOME),
    // then the canonical checkout's own root; dedup when they are the same.
    push(crate::claims::claims_dir_for(None), &mut seen, &mut dirs);
    let canonical = crate::paths::canonical_repo_root(cwd);
    push(
        canonical.and_then(|c| crate::claims::claims_dir_for(Some(&c))),
        &mut seen,
        &mut dirs,
    );
    if dirs.is_empty() {
        return SourceRead::err("agents claim list: no claims root resolves");
    }
    const PRIORITY: [&str; 5] = ["live", "suspect", "stale", "corrupted", "free"];
    let prio = |state: &str| PRIORITY.iter().position(|s| *s == state).unwrap_or(5);
    let mut best: BTreeMap<String, Value> = BTreeMap::new();
    for dir in &dirs {
        for row in scan_claims_dir(dir) {
            let key = s_str(&row, "key").unwrap_or_default().to_string();
            let state = s_str(&row, "state").unwrap_or_default().to_string();
            match best.get(&key) {
                Some(existing)
                    if prio(s_str(existing, "state").unwrap_or("free")) <= prio(&state) =>
                {
                    continue;
                }
                _ => {
                    best.insert(key, row);
                }
            }
        }
    }
    SourceRead::ok(Value::Array(best.into_values().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claim_keys_decode_from_filenames() {
        assert_eq!(
            decode_key("node%3Ax-25b8.lock".trim_end_matches(".lock")),
            "node:x-25b8"
        );
        assert_eq!(decode_key("node%3Ax%20sp"), "node:x sp");
    }
}
