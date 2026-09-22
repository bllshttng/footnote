//! One shared read of the machine decision index (`decisions.jsonl`), the
//! same file `fno inbox decisions` reads first. The Python verb
//! (`list_decisions`) stays the format owner; this module mirrors its
//! flatten and lifecycle derivation for the Rust readers that cannot shell
//! out under fleet load - `prove_it_verdicts` measured ~25s per Python call,
//! and the session-start variant measured 67.0s, which breaks every
//! hook budget the read runs inside.

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

/// The envelope flatten and the retirement pass. The index stores event
/// ENVELOPES (`{type, ts, data}`) that the Python reader flattens (data
/// fields at the top plus `_event_type` and the envelope's `ts`). It then
/// derives lifecycle: a `decision_retracted` row retires its
/// `target_decision_id` (newest `(ts, reason)` wins) and a decision whose
/// `supersedes` names another retires that one (newest `(ts, decision_id)`
/// wins). ids compare casefolded, the Python reader's own rule. A line that
/// does not parse, or a decision/retraction envelope whose required id field
/// is empty, counts in `damaged`. Rows carry whatever fields the row carried;
/// the `text`-present rule is prove_it's, applied at its call site, because
/// law rows carry `decision` and no `text` at all.
pub fn derive_live(text: &str) -> Index {
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

/// Read an index into its live rows: the committed rows plus the unseen live
/// lines of the journal. A journal with neither a live file nor a store is
/// `Err` naming the path: a caller that cannot read the index must say so,
/// never render an empty list that reads as "no rulings exist".
pub fn read_live(path: &Path) -> Result<Index, String> {
    if !path.exists() && !crate::event_store::store_path(path).exists() {
        return Err(format!(
            "{}: neither the journal nor its store exists",
            path.display()
        ));
    }
    let text = crate::event_store::journal_text_checked(
        path,
        &crate::event_store::EventQuery::of_types(&[]),
    )?;
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

/// The live laws, newest first.
pub fn live_laws(path: &Path) -> Result<Index, String> {
    let mut index = read_live(path)?;
    index.rows.retain(is_law);
    index.rows.sort_by(|a, b| {
        let at = a.get("ts").and_then(Value::as_str).unwrap_or("");
        let bt = b.get("ts").and_then(Value::as_str).unwrap_or("");
        bt.cmp(at)
    });
    Ok(index)
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
        let reason = missing.expect_err("missing path is an Err");
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

    #[test]
    fn read_live_reads_a_store_committed_decision() {
        // AC13-LIVE
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let row = envelope(
            "d-abcd1234",
            "2026-09-12T00:00:00Z",
            "operator",
            "topic-a",
            "",
        );
        crate::event_store::append_envelope(&path, &row, None).unwrap();
        let index = read_live(&path).unwrap();
        assert_eq!(index.rows.len(), 1);
        assert_eq!(
            index.rows[0].get("decision_id").and_then(Value::as_str),
            Some("d-abcd1234")
        );
        let missing = dir.path().join("absent.jsonl");
        let err = read_live(&missing).unwrap_err();
        assert!(err.contains("absent.jsonl"), "{err}");
    }
}
