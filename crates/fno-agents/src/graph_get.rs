//! `fno-agents graph-get <id>...` -- batch read of `graph.json` rows.
//!
//! Client-side and daemon-free, like [`crate::wait`]: a batch read is a
//! filesystem read, not an agent-lifecycle operation, so it needs no daemon
//! RPC. Not a routable `fno agents` verb (it stays out of `CLIENT_VERB_USAGE` /
//! `RUST_CLIENT_VERBS`, the same rule `pr-heal` and `kill-check` follow) - the
//! only caller is `fno backlog get`'s Python forwarder, which reaches for this
//! binary only when it is handed more than one id (a single id keeps its
//! existing all-Python path byte for byte).
//!
//! The census this verb answers: `backlog get` was 1,516 single-node calls
//! over 21 days, one graph read each. A caller naming several ids in one
//! invocation pays that read once.

use crate::graph_store;
use serde_json::Value;
use std::path::PathBuf;

/// `graph.json`'s default location: `$FNO_HOME/graph.json`, else
/// `$HOME/.fno/graph.json`. Mirrors the FNO_HOME-first resolution every other
/// client-side verb in this crate uses (see `finalize::append_corrections_pointer`).
/// Does not read `config.paths.graph_json` - a batch convenience read is not
/// where a config-driven relocation belongs, and `--graph` covers a test or an
/// operator override in the meantime.
fn default_graph_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(v).join("graph.json");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".fno").join("graph.json")
}

/// Match one requested token against `id` first, then `slug`, case-insensitive
/// (mirrors `fuzzy.resolve_node`'s exact-match tiers for these two shapes).
fn find_entry<'a>(entries: &'a [Value], token: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|e| field_eq(e, "id", token))
        .or_else(|| entries.iter().find(|e| field_eq(e, "slug", token)))
}

fn field_eq(entry: &Value, field: &str, token: &str) -> bool {
    entry
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|v| v.eq_ignore_ascii_case(token))
}

/// The whole verdict for one invocation: the array in argument order, and
/// whether any token went unmatched. Separated from `run_graph_get` so the
/// test suite exercises the same function the binary does, rather than
/// capturing stdout to re-derive it (the `decide()`/`main()` split every other
/// guard and verb in this crate already uses).
fn get_rows(entries: &[Value], ids: &[String]) -> (Vec<Value>, bool) {
    let mut out = Vec::with_capacity(ids.len());
    let mut any_missing = false;
    for id in ids {
        match find_entry(entries, id) {
            Some(entry) => out.push(entry.clone()),
            None => {
                any_missing = true;
                out.push(serde_json::json!({"id": id, "error": "not found"}));
            }
        }
    }
    (out, any_missing)
}

pub fn run_graph_get(args: &[String]) -> i32 {
    let mut ids: Vec<String> = Vec::new();
    let mut graph_path = default_graph_path();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // Accepted and ignored: the Python forwarder always appends
            // --json, and this verb's only output shape IS a JSON array.
            "--json" => {}
            "--graph" => {
                i += 1;
                match args.get(i) {
                    Some(p) => graph_path = PathBuf::from(p),
                    None => {
                        eprintln!("fno-agents graph-get: --graph needs a path");
                        return 2;
                    }
                }
            }
            other if other.starts_with('-') => {
                eprintln!("fno-agents graph-get: unknown flag {other}");
                return 2;
            }
            other => ids.push(other.to_string()),
        }
        i += 1;
    }
    if ids.is_empty() {
        eprintln!("fno-agents graph-get: needs at least one <id>");
        return 2;
    }

    let mut entries = match graph_store::read_defaulted(&graph_path, false) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("fno-agents graph-get: {err}");
            return 1;
        }
    };
    graph_store::apply_readiness_overlay(&mut entries);

    let (out, any_missing) = get_rows(&entries, &ids);
    println!(
        "{}",
        serde_json::to_string(&out).unwrap_or_else(|_| "[]".to_string())
    );
    i32::from(any_missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_graph(entries: &[Value]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.json");
        let mut f = std::fs::File::create(&path).expect("create graph.json");
        write!(f, "{}", serde_json::json!({"entries": entries})).expect("write graph.json");
        dir
    }

    fn node(id: &str, slug: &str) -> Value {
        serde_json::json!({"id": id, "slug": slug, "status": "ready"})
    }

    #[test]
    fn two_known_ids_return_a_two_element_array_in_argument_order() {
        let entries = [
            node("x-997a", "fewer-gated"),
            node("x-374b", "dispatch-two-axes"),
        ];
        let (rows, missing) = get_rows(&entries, &["x-374b".to_string(), "x-997a".to_string()]);
        assert!(!missing);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], "x-374b");
        assert_eq!(rows[1]["id"], "x-997a");
    }

    #[test]
    fn one_unknown_id_among_two_carries_an_error_row_and_flags_missing() {
        let entries = [node("x-997a", "fewer-gated")];
        let (rows, missing) = get_rows(&entries, &["x-997a".to_string(), "x-0000".to_string()]);
        assert!(missing);
        assert_eq!(rows[0]["id"], "x-997a");
        assert_eq!(rows[1]["id"], "x-0000");
        assert_eq!(rows[1]["error"], "not found");
    }

    #[test]
    fn a_slug_resolves_when_the_id_does_not_match() {
        let entries = [node("x-997a", "fewer-gated-bash-calls")];
        let (rows, missing) = get_rows(&entries, &["fewer-gated-bash-calls".to_string()]);
        assert!(!missing);
        assert_eq!(rows[0]["id"], "x-997a");
    }

    #[test]
    fn no_ids_is_a_usage_error() {
        let args = vec!["--json".to_string()];
        assert_eq!(run_graph_get(&args), 2);
    }

    #[test]
    fn a_fixture_graph_file_round_trips_through_the_binary_entry_point() {
        let dir = write_graph(&[
            node("x-997a", "fewer-gated"),
            node("x-374b", "dispatch-two-axes"),
        ]);
        let graph = dir.path().join("graph.json").display().to_string();
        let args = vec![
            "x-997a".to_string(),
            "x-374b".to_string(),
            "--graph".to_string(),
            graph,
        ];
        assert_eq!(run_graph_get(&args), 0);
    }
}
