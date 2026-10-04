//! The state-root drift reading: top-level entries of the fno state root that
//! `docs/state-root-inventory.md` does not name. The doc is the wire contract:
//! the Python gate (`fno.graph._state_root_inventory`) parses it for the CLI
//! test, and this module parses it for the daemon's daily reclaim lane and the
//! lead check-in. Keep the two parsers dialect-identical (the mail-hold
//! shape: one contract, two legs).

use std::path::{Path, PathBuf};

/// Where the inventory doc lives for THIS machine: an explicit override
/// (tests, custom installs), then the plugin stage beside the state root.
/// The stage copy is a filtered checkout (docs/ is git-tracked), so both a
/// repo cwd and the daemon's anchor resolve the same contract through it;
/// no cwd walk, so the reading never depends on the caller's directory.
pub fn doc_path(state_root: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FNO_STATE_ROOT_INVENTORY_DOC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // One join per line: a bare `join("fno")` would read as a porcelain
    // resolver to the seam-crossings shape rule, which cannot see that this
    // "fno" is the plugin-stage directory's name.
    let staged = state_root
        .join("plugin-stage/fno")
        .join("docs/state-root-inventory.md");
    staged.is_file().then_some(staged)
}

/// One pattern that can name a top-level entry, extracted from a row's Entry
/// cell. Dialect-identical to `top_level_patterns` in
/// `cli/src/fno/graph/_state_root_inventory.py`: a bare name matches itself;
/// a subfolder row documents its first segment; `<ts>` becomes `*`; an
/// extension span (`.lock`) covers only the named bases it sits beside. A
/// pattern reduced to `*` is dropped: it would whitelist the whole root.
pub fn top_level_patterns(doc: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in doc.lines() {
        if !line.trim_start().starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.split('|').collect();
        if cells.len() < 2 {
            continue;
        }
        let spans: Vec<String> = backtick_spans(cells[1])
            .into_iter()
            .map(|t| placeholder_star(t.trim()))
            .filter(|s| !s.is_empty())
            .collect();
        let slashed = spans.iter().any(|s| s.contains('/'));
        let bases: Vec<&str> = spans
            .iter()
            .map(String::as_str)
            .filter(|s| !s.contains('/') && !s.contains('*') && !s.starts_with('.'))
            .collect();
        for span in &spans {
            if span.contains('/') {
                let first = span.split('/').next().unwrap_or(span);
                add(&mut out, first);
            } else if !slashed {
                add(&mut out, span);
                if span.starts_with('.') && span.len() > 1 && !span.contains('*') {
                    for base in &bases {
                        let combined = format!("{base}{span}");
                        add(&mut out, &combined);
                    }
                }
            }
        }
    }
    out
}

fn add(out: &mut Vec<String>, pattern: &str) {
    if pattern.is_empty() || pattern == "*" || pattern == "." || pattern == ".." {
        return;
    }
    if !out.iter().any(|p| p == pattern) {
        out.push(pattern.to_string());
    }
}

/// Backtick-delimited spans of one table cell.
fn backtick_spans(cell: &str) -> Vec<String> {
    cell.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, t)| t.to_string())
        .collect()
}

/// `<anything>` becomes `*` (the doc's placeholder convention).
fn placeholder_star(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut inner = String::new();
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '>' {
                    closed = true;
                    break;
                }
                inner.push(next);
            }
            if closed {
                out.push('*');
            } else {
                // Unterminated '<': keep it literally, like the Python
                // regex, which would never match here.
                out.push('<');
                out.push_str(&inner);
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// `*` is the only wildcard a doc pattern can carry.
fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let Some(mut rest) = name.strip_prefix(parts[0]) else {
        return false;
    };
    let last = parts[parts.len() - 1];
    for mid in &parts[1..parts.len() - 1] {
        let Some(at) = rest.find(mid) else {
            return false;
        };
        rest = &rest[at + mid.len()..];
    }
    rest.ends_with(last)
}

pub struct DriftReport {
    pub count: usize,
    /// The undocumented names, sorted.
    pub entries: Vec<String>,
    pub doc: PathBuf,
}

/// The live reading. A failure is a REFUSAL, never a silent zero: an
/// unreadable doc or root makes the caller print a failed reading.
pub fn drift_report(state_root: &Path) -> Result<DriftReport, String> {
    let doc = doc_path(state_root).ok_or_else(|| {
        "inventory doc unreadable: no docs/state-root-inventory.md upward from the cwd or beside the state root".to_string()
    })?;
    let text = std::fs::read_to_string(&doc).map_err(|e| format!("read {}: {e}", doc.display()))?;
    let patterns = top_level_patterns(&text);
    let names: Vec<String> = std::fs::read_dir(state_root)
        .map_err(|e| format!("read {}: {e}", state_root.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let mut entries: Vec<String> = names
        .into_iter()
        .filter(|n| !patterns.iter().any(|p| glob_match(p, n)))
        .collect();
    entries.sort();
    Ok(DriftReport {
        count: entries.len(),
        entries,
        doc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
# inventory

| Entry | Writer | Lifetime |
|---|---|---|
| `graph.db`, `graph.db-wal` | store | durable |
| `config.toml`, `.lock` | loader | permanent |
| `agents/reap-receipts/<harness>-<session>.json` | receipt writer | 7 days |
| `latches/.context-nudge-*` | hook | 2 days |
| `backups/` | rotation | prunes itself |
| `board.py` | operator | permanent |

## prose mentioning `not-a-table-row` stays out
"#;

    #[test]
    fn patterns_match_the_python_dialect() {
        let pats = top_level_patterns(DOC);
        for want in [
            "graph.db",
            "graph.db-wal",
            "config.toml",
            "config.toml.lock",
            "agents",
            "latches",
            "backups",
            "board.py",
            ".lock",
        ] {
            assert!(pats.iter().any(|p| p == want), "missing {want} in {pats:?}");
        }
        assert!(!pats.contains(&"*".to_string()));
    }

    #[test]
    fn subfolder_rows_document_only_their_first_segment() {
        let pats = top_level_patterns(DOC);
        assert!(!pats
            .iter()
            .any(|p| p.contains("reap-receipts") && p.contains('/')));
    }

    #[test]
    fn undocumented_names_what_no_pattern_covers() {
        let dir = std::env::temp_dir().join(format!("fno-drift-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("graph.db"), "").unwrap();
        std::fs::write(dir.join("stray.out"), "").unwrap();
        let pats = top_level_patterns(DOC);
        let missing = undocumented_of(&dir, &pats);
        assert_eq!(missing, vec!["stray.out".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn undocumented_of(dir: &Path, pats: &[String]) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.retain(|n| !pats.iter().any(|p| glob_match(p, n)));
        names.sort();
        names
    }

    #[test]
    fn placeholder_suffixes_survive_the_star() {
        // Dialect parity with the Python gate: text after `<placeholder>`
        // must survive, so the pattern still anchors the extension.
        assert_eq!(
            placeholder_star("opencode-install-<hash>.json"),
            "opencode-install-*.json"
        );
        assert_eq!(placeholder_star("<ts>"), "*");
        assert_eq!(placeholder_star("a<b.c"), "a<b.c");
    }

    #[test]
    fn globs_cover_placeholders_and_extension_spans() {
        assert!(glob_match("graph.db", "graph.db"));
        assert!(!glob_match("graph.db", "graph.db-wal"));
        assert!(glob_match(".context-nudge-*", ".context-nudge-abc"));
        assert!(glob_match("*-*.json", "fleet-stop-d1f2.json"));
        assert!(!glob_match("*-*.json", "fleetstop.json"));
        assert!(glob_match("board*.py", "board_render.py"));
    }

    #[test]
    fn a_failed_doc_read_is_a_refusal_not_a_zero() {
        let file = std::env::temp_dir().join(format!("fno-drift-file-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, b"x").unwrap();
        // A root under a regular FILE is unreadable: read_dir refuses, so the
        // reading is a refusal, never a silent zero.
        let root = file.join("root");
        let report = drift_report(&root);
        let _ = std::fs::remove_file(&file);
        assert!(report.is_err());
    }
}
