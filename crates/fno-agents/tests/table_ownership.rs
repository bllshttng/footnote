//! The ruling-4 gate: every SQL write to an owned backlog table lives in
//! its owning module. A planted-violation self-test proves the scanner
//! catches writes before the real-tree scan is trusted, and a positive
//! control proves the scanner sees the real in-owner writes - a zero there
//! means the scanner or the walk is broken, not that the tree is clean.
//! AC12 adds one stricter rule: backlog/api.rs composes through module APIs
//! and may not even NAME an owned table.

use std::fs;
use std::path::{Path, PathBuf};

const WRITE_VERBS: &[&str] = &[
    // "insert or replace into" must be tried before "insert into".
    "insert or replace into",
    "replace into",
    "insert into",
    "delete from",
    "update",
];

/// Identifier characters, for whole-word matching.
fn word_boundary_chars(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when the lowercased line writes to `table`: some write verb is
/// followed by whitespace and the table as a whole word.
fn contains_write(table: &str, line_lower: &str) -> bool {
    for verb in WRITE_VERBS {
        let mut search_from = 0;
        while let Some(hit) = line_lower[search_from..].find(verb) {
            let verb_start = search_from + hit;
            let verb_end = verb_start + verb.len();
            search_from = verb_end;
            // A verb glued to earlier word characters is someone else's word
            // ("reupdate", "updated_at").
            if verb_start > 0
                && line_lower[..verb_start]
                    .chars()
                    .next_back()
                    .map_or(false, word_boundary_chars)
            {
                continue;
            }
            let token: String = line_lower[verb_end..]
                .chars()
                .skip_while(|c: &char| c.is_whitespace())
                .take_while(|c| word_boundary_chars(*c))
                .collect();
            if token == table {
                return true;
            }
        }
    }
    false
}

/// True when the lowercased line contains `table` as a whole word.
fn contains_name(table: &str, line_lower: &str) -> bool {
    let mut search_from = 0;
    while let Some(hit) = line_lower[search_from..].find(table) {
        let start = search_from + hit;
        let end = start + table.len();
        search_from = end;
        let before_ok = start == 0
            || !line_lower[..start]
                .chars()
                .next_back()
                .map_or(false, word_boundary_chars);
        let after_ok = end == line_lower.len()
            || !line_lower[end..]
                .chars()
                .next()
                .map_or(false, word_boundary_chars);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn owner_of<'a>(table: &str, owners: &'a [(&'a str, &'a str)]) -> Option<&'a str> {
    owners
        .iter()
        .find(|(owned, _)| *owned == table)
        .map(|(_, owner)| *owner)
}

/// Every line of `text` that writes an owned table, as (line_no, table).
fn scan_text<'a>(text: &str, owners: &'a [(&'a str, &'a str)]) -> Vec<(usize, &'a str)> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        if let Some(found) = owners
            .iter()
            .find(|(table, _)| contains_write(table, &lower))
        {
            hits.push((index + 1, found.0));
        }
    }
    hits
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Scan every .rs file under `root`, judging by paths relative to `root`,
/// the way the real-tree scan does. Returns the violations and the count of
/// in-owner writes (the positive control).
fn scan_root(root: &Path, owners: &[(&str, &str)]) -> (Vec<String>, usize) {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut violations = Vec::new();
    let mut in_owner_writes = 0;
    for path in files {
        let rel = path
            .strip_prefix(root)
            .expect("every walked file sits under the scan root")
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        let text = fs::read_to_string(&path).expect("read source file");
        for (line_no, table) in scan_text(&text, owners) {
            let owner = owner_of(table, owners).expect("an owned table has an owner");
            if rel == owner {
                in_owner_writes += 1;
            } else {
                violations.push(format!(
                    "{rel}:{line_no}: SQL write to {table} outside {owner}"
                ));
            }
        }
        // AC12: the api surface composes through module APIs; it may not
        // name an owned table at all.
        if rel == "backlog/api.rs" {
            for (index, line) in text.lines().enumerate() {
                let lower = line.to_lowercase();
                if let Some(found) = owners
                    .iter()
                    .find(|(table, _)| contains_name(table, &lower))
                {
                    violations.push(format!(
                        "backlog/api.rs:{}: names owned table {} (compose through module APIs)",
                        index + 1,
                        found.0
                    ));
                }
            }
        }
    }
    (violations, in_owner_writes)
}

#[test]
fn scanner_catches_a_planted_violation() {
    let owners = fno_agents::backlog::TABLE_OWNERS;
    let root =
        std::env::temp_dir().join(format!("table-ownership-selftest-{}", std::process::id()));
    fs::create_dir_all(root.join("backlog")).expect("create the self-test temp root");
    fs::write(
        root.join("planted.rs"),
        "UPDATE nodes SET title = 'x' WHERE id = 'y';\n\
         INSERT INTO sessions (node_id, seq) VALUES ('a', 1);\n\
         DELETE FROM graph_meta WHERE key = 'k';\n",
    )
    .expect("write the planted file");
    fs::write(root.join("backlog").join("api.rs"), "let t = \"nodes\";\n")
        .expect("write the planted api.rs");
    let planted_text = fs::read_to_string(root.join("planted.rs")).expect("read the planted file");
    let (violations, _) = scan_root(&root, owners);
    let _ = fs::remove_dir_all(&root);
    let nodes_flagged = violations
        .iter()
        .any(|line| line.starts_with("planted.rs:1: SQL write to nodes"));
    let sessions_flagged = violations
        .iter()
        .any(|line| line.starts_with("planted.rs:2: SQL write to sessions"));
    // The graph_meta line must not light up any owned-table matcher.
    let graph_meta_clean = violations.iter().all(|line| !line.contains("graph_meta"))
        && planted_text
            .lines()
            .nth(2)
            .map(|line| {
                let lower = line.to_lowercase();
                owners
                    .iter()
                    .all(|(table, _)| !contains_write(table, &lower))
            })
            .unwrap_or(false);
    let api_flagged = violations.iter().any(|line| {
        line == "backlog/api.rs:1: names owned table nodes (compose through module APIs)"
    });
    if !nodes_flagged || !sessions_flagged || !graph_meta_clean || !api_flagged {
        panic!("scanner missed the planted lines; it returned {violations:?}");
    }
    println!("table ownership self-test: PASS");
}

#[test]
fn src_tree_writes_only_in_owning_modules() {
    let owners = fno_agents::backlog::TABLE_OWNERS;
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    assert!(src.is_dir(), "{} is missing", src.display());
    let (violations, in_owner_writes) = scan_root(&src, owners);
    assert!(
        in_owner_writes > 0,
        "the scan found zero in-owner SQL writes; the scanner or the walk is broken"
    );
    assert!(
        violations.is_empty(),
        "SQL writes outside their owning modules:\n{}",
        violations.join("\n")
    );
}
