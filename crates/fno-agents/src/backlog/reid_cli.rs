//! The re-id verb: rename one node id in place and keep the old id resolving.
//!
//! `fno backlog re-id <old-id> --to <new-id>` renames the row, every sibling
//! reference field, every fold-uncovered satellite table, the claim key, the
//! plan frontmatter tokens and the plan and brief filenames inside one locked
//! publish. The old id lands on the row as an alias, so history written
//! before the rename keeps resolving through the node_ref tiers.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::autobrief::briefs_dir;
use super::node_ref::{entry_aliases, find_node, is_wellformed_node_id};
use super::settings;
use super::workflows::refuse_non_graph_backend;
use crate::claim_store;
use crate::graph_store;

const EXAMPLE: &str = "fno backlog re-id <old-id> --to <new-id>";

pub fn run(tail: &[String]) -> i32 {
    if tail.is_empty() || tail.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "re-id: rename one node id in place; the old id stays resolving as an alias.\n\
                  \n\
                  Usage:\n\
                  \x20 {EXAMPLE}\n\
                  \n\
                  Refuses: an unknown old id, an ill-formed new id, a collision with any row\n\
                  id or retired alias, and a live claim on the old key. One locked publish\n\
                  covers the row, its sibling reference fields, the satellite tables the row\n\
                  fold does not carry, the claim key, plan frontmatter tokens, and the plan\n\
                  and brief filenames."
        );
        return 0;
    }
    let mut old_id: Option<String> = None;
    let mut new_id: Option<String> = None;
    let mut iter = tail.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--to" => match iter.next() {
                Some(v) => new_id = Some(v.clone()),
                None => {
                    eprintln!("Error: --to needs a value.\n  {EXAMPLE}");
                    return 2;
                }
            },
            other => {
                if other.starts_with('-') {
                    eprintln!("Error: unknown flag {other}.\n  {EXAMPLE}");
                    return 2;
                }
                if old_id.is_some() {
                    eprintln!("Error: exactly one old id is taken.\n  {EXAMPLE}");
                    return 2;
                }
                old_id = Some(other.to_string());
            }
        }
    }
    let (Some(old_id), Some(new_id)) = (old_id, new_id) else {
        eprintln!("Error: both the old id and --to <new-id> are required.\n  {EXAMPLE}");
        return 2;
    };
    if let Some(code) = refuse_non_graph_backend("re-id") {
        return code;
    };
    reid_write(&settings::graph_path(), &old_id, &new_id)
}

/// The sibling reference fields one row carries about OTHER rows: scalar
/// pointers first, then the array fields. All exact-id semantics.
const SIBLING_SCALARS: &[&str] = &[
    "parent",
    "contained_in",
    "superseded_by",
    "caused_by",
    "source_node_id",
];
const SIBLING_ARRAYS: &[&str] = &[
    "blocked_by",
    "related",
    "collisions_acknowledged",
    "supersedes",
];

/// The frontmatter keys a plan file names node ids in. Only these lines pay
/// the token rewrite; body text is history and keeps the old spelling.
const PLAN_ID_KEYS: &[&str] = &["claims", "node", "blocked_by", "depends_on", "parent"];

fn reid_write(graph: &Path, old: &str, new: &str) -> i32 {
    if !is_wellformed_node_id(new) {
        eprintln!("Error: --to must be a well-formed node id (<prefix>-<4..8 lowercase hex>), got '{new}'");
        return 2;
    }
    if old == new {
        eprintln!("Error: --to names the id being renamed");
        return 2;
    }
    let Ok(snapshot) = graph_store::read_rows(graph) else {
        eprintln!("Error: cannot read the graph store at {}", graph.display());
        return 1;
    };
    // The old token must be a row's actual id (dash variants accepted), not a
    // retired alias: renaming by alias would make the target row ambiguous.
    let Some(old_row) = find_node(&snapshot, old) else {
        eprintln!("Error: no node has id '{old}' (retired aliases cannot re-id)");
        return 2;
    };
    let old_canon = old_row
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(old)
        .to_string();
    let taken = snapshot.iter().any(|e| {
        e.get("id").and_then(Value::as_str) == Some(new) || entry_aliases(e).contains(&new)
    });
    if taken {
        eprintln!("Error: {new} is already taken (a row id or a retired alias)");
        return 2;
    }
    // A live claim on the old key would strand its holder mid-lease.
    match claim_store::live_holder(&format!("node:{old_canon}")) {
        Ok(Some(holder)) => {
            eprintln!(
                "Error: node:{old_canon} is claimed live by {holder}; release it, re-id, re-claim."
            );
            return 2;
        }
        Ok(None) => {}
        Err(reason) => {
            eprintln!("Error: cannot read the claim store: {reason}");
            return 1;
        }
    }

    let exit_code: RefCell<i32> = RefCell::new(0);
    let receipt: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let new_owned = new.to_string();
    let old_canon_for_hook = old_canon.clone();
    let applied = graph_store::mutate_rows(
        graph,
        std::time::Duration::from_secs(5),
        None,
        Some(
            &mut |raw: &[Value]| -> Result<(), graph_store::StoreError> {
                let _ = raw;
                // Satellites the row fold does not carry, rewritten under the
                // graph lock so no reader sees a half-renamed graph. The fold's
                // upserts land on the renamed rows; its delete of the old nodes
                // row finds no satellites left behind. FKs stay off for this
                // connection only: the new parent row does not exist yet.
                let satellite_line = rewrite_satellites(graph, &old_canon_for_hook, &new_owned)
                    .map_err(graph_store::StoreError::Invalid)?;
                receipt.borrow_mut().push(satellite_line);
                rewrite_claims(&old_canon_for_hook, &new_owned, &receipt);
                rewrite_plan_files(graph, &old_canon_for_hook, &new_owned, &receipt);
                rewrite_brief(&old_canon_for_hook, &new_owned, &receipt);
                Ok(())
            },
        ),
        |working| {
            if *exit_code.borrow() != 0 {
                return Ok(false);
            }
            let Some(idx) = working
                .iter()
                .position(|e| e.get("id").and_then(Value::as_str) == Some(old_canon.as_str()))
            else {
                exit_code.replace(1);
                eprintln!("Error: no node has id '{old_canon}'");
                return Ok(false);
            };
            let taken = working.iter().any(|e| {
                e.get("id").and_then(Value::as_str) == Some(new) || entry_aliases(e).contains(&new)
            });
            if taken {
                exit_code.replace(2);
                eprintln!("Error: {new} is already taken (a row id or a retired alias)");
                return Ok(false);
            }
            let mut siblings = 0usize;
            for (i, e) in working.iter_mut().enumerate() {
                let Some(obj) = e.as_object_mut() else {
                    continue;
                };
                for key in SIBLING_SCALARS {
                    if obj.get(*key).and_then(Value::as_str) == Some(old_canon.as_str()) {
                        obj.insert(key.to_string(), Value::String(new.to_string()));
                        siblings += 1;
                    }
                }
                for key in SIBLING_ARRAYS {
                    if let Some(items) = obj.get_mut(*key).and_then(Value::as_array_mut) {
                        for item in items.iter_mut() {
                            if item.as_str() == Some(old_canon.as_str()) {
                                *item = Value::String(new.to_string());
                                siblings += 1;
                            }
                        }
                    }
                }
                if i != idx {
                    continue;
                }
                // The row itself: id set, old id kept as an alias. Chains
                // collapse here: prior aliases ride along, so a lookup never
                // chases more than one hop.
                obj.insert("id".to_string(), Value::String(new.to_string()));
                let mut aliases = obj
                    .get("aliases")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                aliases.push(Value::String(old_canon.clone()));
                obj.insert("aliases".to_string(), Value::Array(apply_dedup(aliases)));
                // A plan_path naming the renamed file follows the rename, or
                // the row points at a pathname that no longer exists.
                if let Some(pp) = obj.get("plan_path").and_then(Value::as_str) {
                    let old_suffix = format!("-{old_canon}.md");
                    if let Some(stem) = pp.strip_suffix(old_suffix.as_str()) {
                        obj.insert(
                            "plan_path".to_string(),
                            Value::String(format!("{stem}-{new}.md")),
                        );
                        siblings += 1;
                    }
                }
            }
            receipt
                .borrow_mut()
                .push(format!("  siblings: {siblings} reference field(s)"));
            Ok(true)
        },
    );
    match applied {
        Ok(Some(_)) => {
            println!("re-id: {old_canon} -> {new}");
            for line in receipt.borrow().iter() {
                println!("{line}");
            }
            0
        }
        Ok(None) => *exit_code.borrow(),
        Err(error) => {
            eprintln!("Error: the store publish failed: {error}");
            eprintln!("The satellite and file rewrites under the lock are idempotent; re-run the verb to converge.");
            1
        }
    }
}

fn apply_dedup(mut aliases: Vec<Value>) -> Vec<Value> {
    aliases.dedup();
    aliases
}

/// The fold-uncovered satellite tables: edges endpoints (no row carrier),
/// the supersession rows OTHER nodes hold (successor_id), the decisions join
/// and the free-text decision subjects. The dispatch/provenance/sessions
/// satellites ride the fold's upserts; renaming them here first keeps the
/// cascade on the old nodes row empty either way. Any SQL failure aborts the
/// publish loudly; a silent partial rename is the one outcome this verb must
/// never produce.
fn rewrite_satellites(graph: &Path, old: &str, new: &str) -> Result<String, String> {
    let mut connection = rusqlite::Connection::open(graph).map_err(|e| e.to_string())?;
    connection
        .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA busy_timeout = 5000;")
        .map_err(|e| e.to_string())?;
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let count = |tx: &rusqlite::Transaction, sql: &str| -> Result<usize, String> {
        tx.execute(sql, rusqlite::params![new, old])
            .map_err(|e| e.to_string())
    };
    let edges = count(
        &tx,
        "UPDATE edges SET src_id=?1 WHERE src_kind='backlog' AND src_id=?2",
    )? + count(
        &tx,
        "UPDATE edges SET dst_id=?1 WHERE dst_kind='backlog' AND dst_id=?2",
    )?;
    let supersessions = count(&tx, "UPDATE supersessions SET node_id=?1 WHERE node_id=?2")?
        + count(
            &tx,
            "UPDATE supersessions SET successor_id=?1 WHERE successor_id=?2",
        )?;
    let joins = count(&tx, "UPDATE node_decisions SET node_id=?1 WHERE node_id=?2")?;
    let subjects = count(&tx, "UPDATE decisions SET subject=?1 WHERE subject=?2")?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(format!(
        "  edges: {edges} endpoint(s), supersessions: {supersessions} row(s), decisions: {joins} join(s), {subjects} subject(s)"
    ))
}

fn rewrite_claims(old: &str, new: &str, receipt: &RefCell<Vec<String>>) {
    match claim_store::rename_key(&format!("node:{old}"), &format!("node:{new}")) {
        Ok(moved) => receipt
            .borrow_mut()
            .push(format!("  claims: {moved} key(s)")),
        Err(reason) => receipt
            .borrow_mut()
            .push(format!("  claims: rename failed ({reason})")),
    }
}

/// Plan files: frontmatter id lines rewritten token-exact, a filename ending
/// `-{old}.md` renamed. Body text keeps the old spelling; the alias resolves
/// it on read.
fn rewrite_plan_files(graph: &Path, old: &str, new: &str, receipt: &RefCell<Vec<String>>) {
    let mut rewritten = 0usize;
    let mut renamed = 0usize;
    // Every plan dir the row's plan_path points at; one node, one dir in
    // practice.
    if let Some(dir) = plan_dirs(graph, old).into_iter().next() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (updated, hits) = rewrite_frontmatter(&text, old, new);
                if hits > 0 {
                    if std::fs::write(&path, updated).is_ok() {
                        rewritten += 1;
                    }
                }
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Some(stem) = file_name.strip_suffix(&format!("-{old}.md")) {
                    let target = dir.join(format!("{stem}-{new}.md"));
                    if !target.exists() {
                        if std::fs::rename(&path, target).is_ok() {
                            renamed += 1;
                        }
                    }
                }
            }
        }
    }
    receipt.borrow_mut().push(format!(
        "  plans: {rewritten} file(s) rewritten, {renamed} renamed"
    ));
}

/// The plans dir the old row pointed at, read from the graph as it stood
/// before the rename.
fn plan_dirs(graph: &Path, old: &str) -> Vec<PathBuf> {
    let Ok(rows) = graph_store::read_rows(graph) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|e| e.get("id").and_then(Value::as_str) == Some(old))
        .filter_map(|e| e.get("plan_path").and_then(Value::as_str))
        .filter_map(|p| Path::new(p).parent().map(|d| d.to_path_buf()))
        .collect()
}

fn rewrite_frontmatter(text: &str, old: &str, new: &str) -> (String, usize) {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let mut hits = 0usize;
    let mut in_frontmatter = lines.first().is_some_and(|l| l.trim() == "---");
    let mut seen_closer = false;
    for line in lines.iter_mut() {
        if in_frontmatter {
            if line.trim() == "---" {
                if seen_closer {
                    in_frontmatter = false;
                } else {
                    seen_closer = true;
                }
                continue;
            }
            let key = line.split(':').next().unwrap_or("").trim().to_string();
            if PLAN_ID_KEYS.contains(&key.as_str()) {
                let (updated, n) = replace_id_tokens(line, old, new);
                hits += n;
                *line = updated;
            }
            continue;
        }
    }
    (lines.concat(), hits)
}

/// Replace exact `old` tokens delimited by non-id characters. A longer hex
/// run around the token leaves it alone.
fn replace_id_tokens(haystack: &str, old: &str, new: &str) -> (String, usize) {
    let bytes = haystack.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut hits = 0usize;
    let mut at = 0usize;
    while let Some(found) = haystack[at..].find(old) {
        let start = at + found;
        let end = start + old.len();
        let before_ok = start == 0 || is_id_delim(bytes[start - 1]);
        let after_ok = end == bytes.len() || is_id_delim(bytes[end]);
        if before_ok && after_ok {
            out.push_str(&haystack[at..start]);
            out.push_str(new);
            hits += 1;
        } else {
            out.push_str(&haystack[at..end]);
        }
        at = end;
    }
    out.push_str(&haystack[at..]);
    (out, hits)
}

fn is_id_delim(b: u8) -> bool {
    !b.is_ascii_alphanumeric() && b != b'-'
}

/// The brief sidecar is filename-keyed: rename it, keep the body as history.
fn rewrite_brief(old: &str, new: &str, receipt: &RefCell<Vec<String>>) {
    let Some(dir) = briefs_dir() else {
        return;
    };
    let from = dir.join(format!("{old}.md"));
    let to = dir.join(format!("{new}.md"));
    if from.exists() && !to.exists() && std::fs::rename(&from, &to).is_ok() {
        receipt.borrow_mut().push("  briefs: 1 renamed".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OLD: &str = "ab-aaaaaaaa";
    const NEW: &str = "x-7777";

    fn temp_state() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("FNO_STATE_DIR", dir.path());
        dir
    }

    fn seed(graph: &Path, rows: &[Value]) {
        graph_store::seed_rows(graph, rows).unwrap();
    }

    fn read(graph: &Path) -> Vec<Value> {
        graph_store::read_rows(graph).unwrap()
    }

    #[test]
    fn reid_renames_row_satellites_and_aliases_old() {
        let state = temp_state();
        let graph = state.path().join("graph.db");
        seed(
            &graph,
            &[
                json!({
                    "id": OLD, "title": "t", "type": "bug", "status": "idea", "priority": "p2",
                    "sessions": [{"phase": "blueprint", "harness": "claude", "session_id": "s1"}]
                }),
                json!({"id": "ab-bbbbbbbb", "title": "kid", "type": "bug", "status": "idea", "priority": "p2", "parent": OLD}),
            ],
        );
        // One edge endpoint naming the old id, written straight to the store.
        let conn = rusqlite::Connection::open(&graph).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "INSERT INTO edges (seq, src_kind, src_id, dst_kind, dst_id, kind, valid_from)
             VALUES (1, 'backlog', ?1, 'backlog', 'ab-bbbbbbbb', 'blocks', '2026-10-17T00:00:00Z')",
            rusqlite::params![OLD],
        )
        .unwrap();
        drop(conn);

        let code = reid_write(&graph, OLD, NEW);
        assert_eq!(code, 0, "reid_write refused: see stderr");

        let rows = read(&graph);
        let row = rows
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(NEW))
            .expect("renamed row present");
        let aliases: Vec<&str> = row
            .get("aliases")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(aliases, vec![OLD], "old id kept as the alias");
        assert!(
            !rows
                .iter()
                .any(|e| e.get("id").and_then(Value::as_str) == Some(OLD)),
            "no row keeps the old id"
        );
        let kid = rows
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some("ab-bbbbbbbb"))
            .unwrap();
        assert_eq!(
            kid.get("parent").and_then(Value::as_str),
            Some(NEW),
            "sibling parent pointer rewritten"
        );
        // Sessions survive the fold under the new key.
        let sessions = row
            .get("sessions")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(sessions, 1, "session rows ride the rename");
        // The edge endpoint moved with the row.
        let conn = rusqlite::Connection::open(&graph).unwrap();
        let stale_edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE src_id=?1 OR dst_id=?1",
                rusqlite::params![OLD],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale_edges, 0, "no edge endpoint names the old id");
        let moved_edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE src_id=?1",
                rusqlite::params![NEW],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(moved_edges, 1, "the endpoint names the new id");
    }

    #[test]
    fn reid_refuses_collision_and_leaves_the_store_unchanged() {
        let state = temp_state();
        let graph = state.path().join("graph.db");
        seed(
            &graph,
            &[
                json!({"id": OLD, "title": "t", "type": "bug", "status": "idea", "priority": "p2"}),
                json!({"id": NEW, "title": "taken", "type": "bug", "status": "idea", "priority": "p2"}),
            ],
        );
        let before = read(&graph);
        let code = reid_write(&graph, OLD, NEW);
        assert_eq!(code, 2, "collision refuses with exit 2");
        let after = read(&graph);
        assert_eq!(
            graph_store::serialize_entries(&before),
            graph_store::serialize_entries(&after),
            "the store is unchanged"
        );
    }

    #[test]
    fn alias_pool_carries_retired_ids_into_the_mint_exclusion() {
        let state = temp_state();
        let graph = state.path().join("graph.db");
        seed(
            &graph,
            &[json!({
                "id": "x-cccc", "title": "t", "type": "bug", "status": "idea", "priority": "p2",
                "aliases": [OLD]
            })],
        );
        let pool = crate::backlog::create_cli::alias_pool();
        assert!(pool.contains(OLD), "the alias pool carries the retired id");
    }
}
