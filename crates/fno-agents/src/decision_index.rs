//! One shared read of the machine decision store: graph.db decisions plus
//! the legacy `decisions.jsonl` rows the db lacks (the same merge the
//! Python owner `_read_index` applies). The Python verb (`list_decisions`)
//! stays the format owner; this module mirrors its flatten and lifecycle
//! derivation for the Rust readers that cannot shell out under fleet load -
//! `prove_it_verdicts` measured ~25s per Python call, and the session-start
//! variant measured 67.0s, which breaks every hook budget the read runs
//! inside.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The Python reader's law-lane cutover (`AUTHORITY_LANE_CUTOVER`,
/// `cli/src/fno/decide/__init__.py`): an operator or chat_attested row at or
/// after this instant is law. Compared as a string, the same rule Python
/// applies.
pub const LAW_LANE_CUTOVER: &str = "2026-08-21T00:00:00Z";

/// The live flattened decisions plus the count of lines that could not join
/// them. `damaged` is a report for the reader, never a filter: the rows are
/// exactly what the Python reader would call live.
#[derive(Debug)]
pub struct Index {
    pub rows: Vec<Value>,
    pub damaged: usize,
}

/// `$FNO_HOME/<name>`, else `$HOME/.fno/<name>`: the same resolution
/// `graph_get::default_graph_path` applies to the graph store.
pub fn default_state_path(name: &str) -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(v).join(name);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".fno").join(name)
}

/// The envelope flatten: JSONL text into flattened rows plus the damaged
/// count. The index stores event ENVELOPES (`{type, ts, data}`) that the
/// Python reader flattens (data fields at the top plus `_event_type` and
/// the envelope's `ts`). A line that does not parse, or a
/// decision/retraction envelope whose required id field is empty, counts in
/// `damaged`.
fn flatten_envelopes(text: &str) -> (Vec<Value>, usize) {
    let mut rows: Vec<Value> = Vec::new();
    let mut damaged: usize = 0;
    for line in text.lines() {
        let Ok(env) = serde_json::from_str::<Value>(line) else {
            damaged += 1;
            continue;
        };
        let Some(data) = env.get("data").and_then(Value::as_object) else {
            damaged += 1;
            continue;
        };
        let Some(etype) = env.get("type").and_then(Value::as_str) else {
            damaged += 1;
            continue;
        };
        if etype != "operator_decision" && etype != "decision_retracted" {
            continue;
        }
        // The Python reader marks an envelope whose required data field is
        // empty as damaged (discarded), not as a live or retiring row.
        let required = if etype == "operator_decision" {
            "decision_id"
        } else {
            "target_decision_id"
        };
        if data
            .get(required)
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true)
        {
            damaged += 1;
            continue;
        }
        let mut flat = Value::Object(data.clone());
        let obj = flat.as_object_mut().expect("just built");
        obj.insert("_event_type".to_string(), json!(etype));
        obj.insert(
            "ts".to_string(),
            env.get("ts").cloned().unwrap_or(Value::Null),
        );
        rows.push(flat);
    }
    (rows, damaged)
}

/// The retirement pass over flattened rows. A `decision_retracted` row
/// retires its `target_decision_id` (newest `(ts, reason)` wins) and a
/// decision whose `supersedes` names another retires that one (newest
/// `(ts, decision_id)` wins). ids compare casefolded, the Python reader's
/// own rule. Rows carry whatever fields the row carried; the `text`-present
/// rule is prove_it's, applied at its call site, because law rows carry
/// `decision` and no `text` at all.
pub fn derive_rows(rows: Vec<Value>, damaged: usize) -> Index {
    let is_decision = |row: &Value| {
        matches!(
            row.get("_event_type").and_then(Value::as_str),
            Some("operator_decision")
        )
    };
    let rank = |row: &Value, tie: &str| {
        (
            row.get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            row.get(tie)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
    };
    let mut retired: std::collections::BTreeMap<String, (String, String)> = Default::default();
    for row in rows
        .iter()
        .filter(|r| r.get("_event_type").and_then(Value::as_str) == Some("decision_retracted"))
    {
        let target = row
            .get("target_decision_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if target.is_empty() {
            continue;
        }
        let r = rank(row, "reason");
        if retired.get(&target).map_or(true, |prev| *prev < r) {
            retired.insert(target, r);
        }
    }
    for row in rows.iter().filter(|r| is_decision(r)) {
        let target = row
            .get("supersedes")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if target.is_empty() {
            continue;
        }
        let r = rank(row, "decision_id");
        if retired.get(&target).map_or(true, |prev| *prev < r) {
            retired.insert(target, r);
        }
    }
    let rows = rows
        .into_iter()
        .filter(is_decision)
        .filter(|row| {
            let id = row
                .get("decision_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            id.is_empty() || !retired.contains_key(&id)
        })
        .collect();
    Index { rows, damaged }
}

/// `derive_live` composed of its two halves, kept as the seam the JSONL-file
/// readers and their tests call.
pub fn derive_live(text: &str) -> Index {
    let (rows, damaged) = flatten_envelopes(text);
    derive_rows(rows, damaged)
}

/// The row identity the Python merge uses (`decide/__init__.py:814-819`):
/// event type, then the first non-empty of `decision_id`,
/// `retraction_id`, `target_decision_id`.
fn row_key(row: &Value) -> (String, String) {
    let etype = row
        .get("_event_type")
        .and_then(Value::as_str)
        .unwrap_or("operator_decision")
        .to_string();
    let id = ["decision_id", "retraction_id", "target_decision_id"]
        .iter()
        .find_map(|k| row.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string();
    (etype, id)
}

/// The store rows: graph.db decisions first, then the JSONL rows the db
/// lacks, keyed the way `_read_index` (`cli/src/fno/decide/__init__.py:800`)
/// merges them. The JSONL half is load-bearing, not vestigial: ruling
/// d-0e9ed907 is in the JSONL and missing from the db. A missing JSONL reads
/// as zero rows, as Python does. No db AND no JSONL is `Err` naming both
/// paths: a caller that cannot read the store must say so, never render an
/// empty list that reads as "no rulings exist". `damaged` counts JSONL
/// lines only, the same source the Python reader reports.
pub fn read_store_rows(graph: &Path, jsonl: &Path) -> Result<(Vec<Value>, usize), String> {
    let db = crate::backlog::database_path(graph);
    let db_present = db.exists() || graph.exists();
    let jsonl_present = jsonl.exists();
    if !db_present && !jsonl_present {
        return Err(format!(
            "no decision store: nothing at {} and nothing at {}",
            db.display(),
            jsonl.display()
        ));
    }
    let mut rows = Vec::new();
    if db_present {
        match crate::backlog::api::decisions(&crate::backlog::api::Store::new(graph), None, None) {
            Ok(db_rows) => rows = db_rows,
            Err(e) => {
                if !jsonl_present {
                    return Err(format!(
                        "the decision db at {} is unreadable ({}) and no JSONL sits at {}",
                        db.display(),
                        e.0,
                        jsonl.display()
                    ));
                }
                // Degrade to the JSONL leg; the Err above covers the machine
                // with neither leg readable.
            }
        }
    }
    let known: std::collections::HashSet<(String, String)> = rows.iter().map(row_key).collect();
    let mut damaged = 0usize;
    if jsonl_present {
        let text =
            std::fs::read_to_string(jsonl).map_err(|e| format!("{}: {e}", jsonl.display()))?;
        let (jsonl_rows, jsonl_damaged) = flatten_envelopes(&text);
        damaged = jsonl_damaged;
        for row in jsonl_rows {
            if !known.contains(&row_key(&row)) {
                rows.push(row);
            }
        }
    }
    Ok((rows, damaged))
}

/// The store read through the lifecycle derivation: the rows a Python
/// `fno backlog decisions` call renders.
pub fn read_store_live(graph: &Path, jsonl: &Path) -> Result<Index, String> {
    let (rows, damaged) = read_store_rows(graph, jsonl)?;
    Ok(derive_rows(rows, damaged))
}

/// `read_store_live` over the machine's default paths, resolved the way
/// every other client-side verb resolves them.
pub fn default_store_live() -> Result<Index, String> {
    read_store_live(
        &crate::graph_get::default_graph_path(),
        &default_state_path("decisions.jsonl"),
    )
}

/// Read an index file into its live rows. A missing or unreadable file is
/// `Err` naming the path: a caller that cannot read the index must say so,
/// never render an empty list that reads as "no rulings exist".
pub fn read_live(path: &Path) -> Result<Index, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(derive_live(&text))
}

/// A port of `_decision_lane(row) == "law"`: authority `operator` or
/// `chat_attested`, `ts` at or after the cutover. Beastmode is the grant
/// lane and agent/crown are coordination; neither is law.
pub fn is_law(row: &Value) -> bool {
    let authority = row
        .get("authority_source")
        .and_then(Value::as_str)
        .unwrap_or("");
    if authority != "operator" && authority != "chat_attested" {
        return false;
    }
    row.get("ts").and_then(Value::as_str).unwrap_or("") >= LAW_LANE_CUTOVER
}

/// The laws of an index, newest first.
pub fn laws_of(mut index: Index) -> Index {
    index.rows.retain(is_law);
    index.rows.sort_by(|a, b| {
        let at = a.get("ts").and_then(Value::as_str).unwrap_or("");
        let bt = b.get("ts").and_then(Value::as_str).unwrap_or("");
        bt.cmp(at)
    });
    index
}

/// The live laws of an index FILE, newest first. The fixture/test path; the
/// store read is `read_store_live` + `laws_of`.
pub fn live_laws(path: &Path) -> Result<Index, String> {
    Ok(laws_of(read_live(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn envelope(id: &str, ts: &str, authority: &str, subject: &str, extra: &str) -> String {
        format!(
            "{{\"type\":\"operator_decision\",\"ts\":\"{ts}\",\"data\":{{\"decision_id\":\"{id}\",\
             \"subject\":\"{subject}\",\"decision\":\"Ruling.\",\"text\":\"Ruling.\",\
             \"authority_source\":\"{authority}\"{extra}}}}}"
        )
    }

    fn write_index(dir: &Path, lines: &[String]) -> PathBuf {
        let path = dir.join("decisions.jsonl");
        fs::write(&path, lines.join("\n") + "\n").expect("writes");
        path
    }

    #[test]
    fn ac1_hp_live_laws_keeps_the_three_law_rows_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_index(
            dir.path(),
            &[
                // live law, operator, newest of the three
                envelope(
                    "d-aaaa0001",
                    "2026-09-12T00:00:00Z",
                    "operator",
                    "topic-a",
                    "",
                ),
                // live law, chat_attested
                envelope(
                    "d-bbbb0002",
                    "2026-09-10T00:00:00Z",
                    "chat_attested",
                    "topic-b",
                    "",
                ),
                // agent lane: authority agent is coordination, never law
                envelope("d-cccc0003", "2026-09-11T00:00:00Z", "agent", "topic-c", ""),
                // operator but before the cutover: unattributed, not law
                envelope(
                    "d-dddd0004",
                    "2026-08-01T00:00:00Z",
                    "operator",
                    "topic-d",
                    "",
                ),
                // superseded by d-ffff0006 below
                envelope(
                    "d-eeee0005",
                    "2026-09-05T00:00:00Z",
                    "operator",
                    "topic-e",
                    "",
                ),
                // retraction of d-gggg0007 below
                format!(
                    "{{\"type\":\"decision_retracted\",\"ts\":\"2026-09-13T00:00:00Z\",\
                     \"data\":{{\"target_decision_id\":\"d-gggg0007\",\"reason\":\"r\"}}}}"
                ),
                envelope(
                    "d-gggg0007",
                    "2026-09-06T00:00:00Z",
                    "operator",
                    "topic-g",
                    "",
                ),
                // the superseding law, oldest of the three live ones
                envelope(
                    "d-ffff0006",
                    "2026-09-08T00:00:00Z",
                    "operator",
                    "topic-f",
                    ",\"supersedes\":\"d-eeee0005\"",
                ),
            ],
        );
        let index = live_laws(&path).expect("reads");
        let ids: Vec<&str> = index
            .rows
            .iter()
            .filter_map(|r| r.get("decision_id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["d-aaaa0001", "d-bbbb0002", "d-ffff0006"]);
    }

    #[test]
    fn ac1_err_missing_path_is_err_and_bad_lines_count_as_damaged() {
        let missing = live_laws(Path::new("/nonexistent/fno/decisions.jsonl"));
        let reason = missing.expect_err("missing path is the Err");
        assert!(
            reason.contains("/nonexistent/fno/decisions.jsonl"),
            "{reason}"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_index(
            dir.path(),
            &[
                "not json at all".to_string(),
                envelope(
                    "d-aaaa0001",
                    "2026-09-12T00:00:00Z",
                    "operator",
                    "topic-a",
                    "",
                ),
                "{\"type\":\"operator_decision\"}".to_string(),
                envelope(
                    "d-bbbb0002",
                    "2026-09-10T00:00:00Z",
                    "operator",
                    "topic-b",
                    "",
                ),
            ],
        );
        let index = live_laws(&path).expect("valid rows still return");
        assert_eq!(index.rows.len(), 2);
        assert_eq!(index.damaged, 2);
    }

    fn seed_db(graph: &Path, envelopes: &[String]) {
        let connection = crate::backlog::open(graph).expect("opens the db");
        for e in envelopes {
            let event: Value = serde_json::from_str(e).expect("parses");
            crate::backlog::decisions::record(&connection, &event).expect("records");
        }
    }

    // AC1-HP: db A,B + JSONL B,C reads as A,B,C with B once.
    #[test]
    fn ac1_hp_store_merge_keeps_db_and_jsonl_only_rows_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let graph = dir.path().join("graph.json");
        let jsonl = dir.path().join("decisions.jsonl");
        seed_db(
            &graph,
            &[
                envelope(
                    "d-aaaa0001",
                    "2026-09-12T00:00:00Z",
                    "operator",
                    "topic-a",
                    "",
                ),
                envelope(
                    "d-bbbb0002",
                    "2026-09-10T00:00:00Z",
                    "operator",
                    "topic-b",
                    "",
                ),
            ],
        );
        // Written after the db seed, so the one-shot decisions import never
        // pulls these rows into the db.
        write_index(
            dir.path(),
            &[
                envelope(
                    "d-bbbb0002",
                    "2026-09-10T00:00:00Z",
                    "operator",
                    "topic-b",
                    "",
                ),
                envelope(
                    "d-cccc0003",
                    "2026-09-11T00:00:00Z",
                    "operator",
                    "topic-c",
                    "",
                ),
            ],
        );
        let index = read_store_live(&graph, &jsonl).expect("reads");
        let mut ids: Vec<String> = index
            .rows
            .iter()
            .filter_map(|r| r.get("decision_id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["d-aaaa0001", "d-bbbb0002", "d-cccc0003"]);
    }

    // AC1-EDGE: a JSONL retraction retires a db row.
    #[test]
    fn ac1_edge_jsonl_retraction_retires_a_db_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let graph = dir.path().join("graph.json");
        let jsonl = dir.path().join("decisions.jsonl");
        seed_db(
            &graph,
            &[
                envelope(
                    "d-aaaa0001",
                    "2026-09-12T00:00:00Z",
                    "operator",
                    "topic-a",
                    "",
                ),
                envelope(
                    "d-bbbb0002",
                    "2026-09-10T00:00:00Z",
                    "operator",
                    "topic-b",
                    "",
                ),
            ],
        );
        write_index(
            dir.path(),
            &[
                "{\"type\":\"decision_retracted\",\"ts\":\"2026-09-13T00:00:00Z\",\
                 \"data\":{\"retraction_id\":\"d-rrrr0009\",\"target_decision_id\":\"d-aaaa0001\",\"reason\":\"r\"}}"
                    .to_string(),
                envelope(
                    "d-cccc0003",
                    "2026-09-11T00:00:00Z",
                    "operator",
                    "topic-c",
                    "",
                ),
            ],
        );
        let index = read_store_live(&graph, &jsonl).expect("reads");
        let ids: Vec<&str> = index
            .rows
            .iter()
            .filter_map(|r| r.get("decision_id").and_then(Value::as_str))
            .collect();
        assert!(!ids.contains(&"d-aaaa0001"), "{ids:?}");
        assert!(ids.contains(&"d-bbbb0002"), "{ids:?}");
        assert!(ids.contains(&"d-cccc0003"), "{ids:?}");
    }

    // AC1-ERR: no db and no JSONL is Err naming both paths, never empty.
    #[test]
    fn ac1_err_missing_store_names_both_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let graph = dir.path().join("graph.json");
        let jsonl = dir.path().join("decisions.jsonl");
        let reason = read_store_live(&graph, &jsonl).expect_err("no store is the Err");
        assert!(reason.contains("graph.db"), "{reason}");
        assert!(reason.contains("decisions.jsonl"), "{reason}");
    }
}
