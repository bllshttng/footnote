//! Native one-shot row migrations: legacy p0, difficulty, __updated_at.
//!
//! Each verb is a dry run by default and prints one `sort_keys` JSON receipt.
//! The dry run reads the live rows; `--apply` rewrites the whole store in one
//! locked mutation.

use serde_json::{json, Value};

use super::fields::normalize_difficulty;
use super::render::py_json_compact;
use super::settings;
use crate::graph_store;

const PRIORITIES_HELP: &str = "Usage: fno backlog migrate-priorities [OPTIONS]

  Dry-run or apply the explicit legacy p0 re-band migration.

Options:
  --apply     Apply the idempotent p0 to p1 migration.
  --rollback  Restore rows changed by the migration.
  -h, --help  Show this message and exit.";

const DIFFICULTY_HELP: &str = "Usage: fno backlog migrate-difficulty [OPTIONS]

  Dry-run or apply the difficulty migrations.

Options:
  --apply     Move each model_tier band onto difficulty and drop the retired
              key.
  --backfill  Backfill missing bands from size, priority, or type.
  -h, --help  Show this message and exit.";

const UPDATED_AT_HELP: &str = "Usage: fno backlog migrate-updated-at [OPTIONS]

  Dry-run or apply the one-shot __updated_at residue migration.

Options:
  --apply     Remove the proven-unread __updated_at field from candidate rows.
  -h, --help  Show this message and exit.";

/// Python `json.dumps(v, sort_keys=True)`.
fn sorted_json(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sort(&map[k])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    py_json_compact(&sort(v))
}

/// Python `repr` of a scalar, for the refusal that names a row's values.
fn py_repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(Value::String(s)) => {
            let quote = if s.contains('\'') && !s.contains('"') {
                '"'
            } else {
                '\''
            };
            let escaped = s.replace('\\', "\\\\");
            let escaped = if quote == '\'' {
                escaped.replace('\'', "\\'")
            } else {
                escaped
            };
            format!("{quote}{escaped}{quote}")
        }
        Some(other) => other.to_string(),
    }
}

/// Python `str(row.get("id", "?"))`.
fn id_text(row: &Value) -> String {
    match row.get("id") {
        None => "?".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_repr(Some(other)),
    }
}

fn present(row: &Value, key: &str) -> bool {
    row.get(key).is_some_and(|v| !v.is_null())
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Append one history item, replacing a null or non-list history.
fn push_history(row: &mut Value, key: &str, item: Value) {
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    let mut history = match obj.get(key) {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    history.push(item);
    obj.insert(key.into(), Value::Array(history));
}

fn set(row: &mut Value, key: &str, value: Value) {
    if let Some(obj) = row.as_object_mut() {
        obj.insert(key.into(), value);
    }
}

// -- legacy p0 -----------------------------------------------------------

fn migration_marker(row: &Value) -> bool {
    row.get("priority_history")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("source").and_then(Value::as_str) == Some("migrate-priorities")
                    && item.get("from").and_then(Value::as_str) == Some("p0")
                    && item.get("to").and_then(Value::as_str) == Some("p1")
            })
        })
}

/// Re-band unacknowledged legacy p0 rows to p1, once and audibly.
pub(crate) fn migrate_legacy_p0(rows: &mut [Value], apply: bool) -> Value {
    let pending: Vec<usize> = (0..rows.len())
        .filter(|i| {
            rows[*i].get("priority").and_then(Value::as_str) == Some("p0")
                && !truthy(rows[*i].get("blocks_everything"))
        })
        .collect();
    let already = rows.iter().filter(|r| migration_marker(r)).count();
    let mut receipt = json!({
        "already_migrated": already,
        "legacy_p0": pending.len(),
        "rebanded_to_p1": pending.len(),
        "remaining_unacknowledged_p0": pending.len(),
    });
    if !apply {
        return receipt;
    }
    let now = graph_store::now_isoformat();
    for i in pending {
        push_history(
            &mut rows[i],
            "priority_history",
            json!({"from": "p0", "to": "p1", "prior_priority": "p0",
                   "source": "migrate-priorities", "ts": now}),
        );
        set(&mut rows[i], "priority", json!("p1"));
    }
    receipt["remaining_unacknowledged_p0"] = json!(0);
    receipt
}

/// Restore rows changed by the migration, preserving the audit trail.
pub(crate) fn rollback_legacy_p0(rows: &mut [Value]) -> Value {
    let now = graph_store::now_isoformat();
    let mut restored = 0;
    for row in rows.iter_mut() {
        if row.get("priority").and_then(Value::as_str) != Some("p1") || !migration_marker(row) {
            continue;
        }
        push_history(
            row,
            "priority_history",
            json!({"from": "p1", "to": "p0", "prior_priority": "p1",
                   "source": "migrate-priorities-rollback", "ts": now}),
        );
        set(row, "priority", json!("p0"));
        restored += 1;
    }
    json!({ "restored_to_p0": restored })
}

// -- difficulty ------------------------------------------------------------

/// The row's model_tier as a canonical band, or None if it is not one.
fn retired_band(row: &Value) -> Option<String> {
    row.get("model_tier")
        .and_then(Value::as_str)
        .and_then(|s| normalize_difficulty(s).ok())
}

/// (drainable ids, needs-decision ids) for rows still carrying model_tier.
/// Drainable: a valid band with no canonical field, a same-band pair (what
/// the retired handler's leftovers look like), or a garbage retired key
/// under a live difficulty. Needs a decision: a divergent pair, a garbage
/// band with no canonical field, or a garbage canonical band, all fixed by
/// `fno backlog update <id> difficulty=<band>`.
pub(crate) fn split_retired_tier_rows(rows: &[Value]) -> (Vec<String>, Vec<String>) {
    let mut drainable = Vec::new();
    let mut needs_decision = Vec::new();
    for row in rows.iter().filter(|r| present(r, "model_tier")) {
        let band = retired_band(row);
        let current = row.get("difficulty").filter(|v| !v.is_null());
        let canonical = current
            .and_then(Value::as_str)
            .and_then(|s| normalize_difficulty(s).ok());
        let decision = match (current, &canonical, &band) {
            // A garbage canonical band: draining would bless it under a
            // success receipt, and no later sweep names the row again.
            (Some(_), None, _) => true,
            (None, _, None) => true,
            (Some(_), Some(current), Some(band)) => current != band,
            _ => false,
        };
        if decision {
            needs_decision.push(id_text(row));
        } else {
            drainable.push(id_text(row));
        }
    }
    (drainable, needs_decision)
}

/// Move the retired `model_tier` band onto `difficulty`, once, audibly. A
/// row needing a hand-picked band refuses the whole batch, named with its
/// values and the one command that fixes it.
pub(crate) fn migrate_model_tier(rows: &mut [Value], apply: bool) -> Result<Value, String> {
    let pending: Vec<usize> = (0..rows.len())
        .filter(|i| present(&rows[*i], "model_tier"))
        .collect();
    let (_drainable, needs_decision) = split_retired_tier_rows(rows);
    if !needs_decision.is_empty() {
        let named: Vec<String> = pending
            .iter()
            .map(|i| &rows[*i])
            .filter(|r| needs_decision.contains(&id_text(r)))
            .map(|r| {
                format!(
                    "{} model_tier={} difficulty={}",
                    id_text(r),
                    py_repr(r.get("model_tier")),
                    py_repr(r.get("difficulty"))
                )
            })
            .collect();
        let more = if named.len() > 8 {
            format!(" ... (+{} more)", named.len() - 8)
        } else {
            String::new()
        };
        return Err(format!(
            "rows need a hand-picked band before migrating; pick it with \
             `fno backlog update <id> difficulty=<band>` (which also clears \
             the retired key): {}{more}",
            named[..named.len().min(8)].join(", ")
        ));
    }
    let candidates: Vec<Value> = pending
        .iter()
        .map(|i| rows[*i].get("id").cloned().unwrap_or(Value::Null))
        .collect();
    let mut receipt = json!({
        "apply": apply,
        "candidate_count": pending.len(),
        "candidates": candidates,
    });
    if !apply {
        return Ok(receipt);
    }
    let now = graph_store::now_isoformat();
    for i in &pending {
        let band = retired_band(&rows[*i]);
        if let Some(obj) = rows[*i].as_object_mut() {
            obj.remove("model_tier");
        }
        if let Some(band) = band {
            if !present(&rows[*i], "difficulty") {
                set(&mut rows[*i], "difficulty", json!(band));
            }
            push_history(
                &mut rows[*i],
                "difficulty_history",
                json!({"value": band, "source": "migration", "ts": now}),
            );
        }
    }
    receipt["migrated"] = json!(pending.len());
    Ok(receipt)
}

/// The strongest named legacy signal on a row (size, priority, type), or no
/// band: a row with none stays unbanded rather than receiving a default.
fn backfill_band(row: &Value) -> Option<&'static str> {
    const RANK: [&str; 3] = ["low", "medium", "high"];
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::trim);
    let signals = [
        text("size").and_then(|s| match s.to_uppercase().as_str() {
            "S" => Some("low"),
            "M" => Some("medium"),
            "L" => Some("high"),
            _ => None,
        }),
        text("priority").and_then(|s| match s.to_lowercase().as_str() {
            "p0" | "p1" => Some("high"),
            "p2" => Some("medium"),
            "p3" => Some("low"),
            _ => None,
        }),
        text("type").and_then(|s| match s.to_lowercase().as_str() {
            "bug" => Some("high"),
            "epic" | "feature" | "roadmap" => Some("medium"),
            _ => None,
        }),
    ];
    signals
        .into_iter()
        .flatten()
        .max_by_key(|band| RANK.iter().position(|r| r == band))
}

/// Backfill missing difficulty from explicit legacy row signals. Existing
/// difficulty and history are untouched; a retired-key row is skipped.
pub(crate) fn backfill_difficulty(rows: &mut [Value], apply: bool) -> Value {
    let mut planned: Vec<(usize, &'static str)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if present(row, "difficulty") {
            continue;
        }
        match backfill_band(row) {
            Some(band) if !present(row, "model_tier") => planned.push((i, band)),
            _ => skipped.push(id_text(row)),
        }
    }
    let candidates: Vec<String> = planned.iter().map(|(i, _)| id_text(&rows[*i])).collect();
    let receipt = json!({
        "already_band": rows.iter().filter(|r| present(r, "difficulty")).count(),
        "apply": apply,
        "candidates": candidates,
        "skipped": skipped,
        "written": if apply { candidates.clone() } else { Vec::new() },
    });
    if apply {
        let now = graph_store::now_isoformat();
        for (i, band) in planned {
            set(&mut rows[i], "difficulty", json!(band));
            push_history(
                &mut rows[i],
                "difficulty_history",
                json!({"value": band, "source": "backfill", "ts": now}),
            );
        }
    }
    receipt
}

// -- __updated_at ----------------------------------------------------------

/// Remove the proven-unread `__updated_at` residue, once and audibly.
pub(crate) fn migrate_updated_at(rows: &mut [Value], apply: bool) -> Value {
    let pending: Vec<usize> = (0..rows.len())
        .filter(|i| rows[*i].get("__updated_at").is_some())
        .collect();
    let candidates: Vec<Value> = pending
        .iter()
        .map(|i| rows[*i].get("id").cloned().unwrap_or(Value::Null))
        .collect();
    let mut receipt = json!({
        "apply": apply,
        "candidate_count": pending.len(),
        "candidates": candidates,
        "removed": 0,
    });
    if apply {
        for i in &pending {
            if let Some(obj) = rows[*i].as_object_mut() {
                obj.remove("__updated_at");
            }
        }
        receipt["removed"] = json!(pending.len());
    }
    receipt
}

// -- verbs -----------------------------------------------------------------

/// The parsed boolean flags, or the exit code the parse already answered.
fn parse_flags(
    verb: &str,
    tail: &[String],
    known: &[&str],
    help: &str,
) -> Result<Vec<String>, i32> {
    let mut seen = Vec::new();
    for token in tail {
        match token.as_str() {
            "-h" | "--help" => {
                println!("{help}");
                return Err(0);
            }
            flag if known.contains(&flag) => seen.push(flag.to_string()),
            other => {
                let message = if other.starts_with('-') {
                    format!("No such option: {other}")
                } else {
                    format!("Got unexpected extra argument ({other})")
                };
                eprintln!(
                    "Usage: fno backlog {verb} [OPTIONS]\nTry 'fno backlog {verb} -h' for help.\n\nError: {message}"
                );
                return Err(2);
            }
        }
    }
    Ok(seen)
}

fn live_rows() -> Result<Vec<Value>, String> {
    let rows = graph_store::read_rows(&settings::graph_path()).map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .filter(|r| !truthy(r.get("archived_at")))
        .collect())
}

/// One locked whole-store mutation that hands back the migration's receipt.
/// A refusal writes nothing; a `store: ` error is the write's own failure.
fn apply_receipt(
    mut migrate: impl FnMut(&mut [Value]) -> Result<Value, String>,
) -> Result<Value, String> {
    let mut receipt: Option<Result<Value, String>> = None;
    graph_store::mutate_rows(
        &settings::graph_path(),
        std::time::Duration::from_secs(30),
        None,
        None,
        |working| {
            // Archive residents are history: the migration sees the live
            // rows only, the same slice the dry run reads.
            let live_pos: Vec<usize> = (0..working.len())
                .filter(|i| !truthy(working[*i].get("archived_at")))
                .collect();
            let mut live: Vec<Value> = live_pos
                .iter()
                .map(|i| std::mem::take(&mut working[*i]))
                .collect();
            let outcome = migrate(&mut live);
            for (row, pos) in live.into_iter().zip(live_pos) {
                working[pos] = row;
            }
            let ok = outcome.is_ok();
            receipt = Some(outcome);
            Ok(ok)
        },
    )
    .map_err(|e| format!("store: {e}"))?;
    receipt.unwrap_or_else(|| Err("store: the migration never ran".into()))
}

fn finish(verb: &str, outcome: Result<Value, String>) -> i32 {
    match outcome {
        Ok(receipt) => {
            println!("{}", sorted_json(&receipt));
            0
        }
        Err(message) if message.starts_with("store: ") => {
            eprintln!("fno backlog {verb}: {}", &message["store: ".len()..]);
            1
        }
        Err(message) => {
            eprintln!("fno backlog {verb}: {message}");
            2
        }
    }
}

fn read_then(
    mut migrate: impl FnMut(&mut [Value]) -> Result<Value, String>,
) -> Result<Value, String> {
    let mut rows = live_rows().map_err(|e| format!("store: {e}"))?;
    migrate(&mut rows)
}

/// `fno backlog migrate-priorities [--apply | --rollback]`.
pub fn run_migrate_priorities(tail: &[String]) -> i32 {
    let verb = "migrate-priorities";
    let flags = match parse_flags(verb, tail, &["--apply", "--rollback"], PRIORITIES_HELP) {
        Ok(flags) => flags,
        Err(code) => return code,
    };
    let apply = flags.iter().any(|f| f == "--apply");
    let rollback = flags.iter().any(|f| f == "--rollback");
    if apply && rollback {
        eprintln!("Error: --apply and --rollback are mutually exclusive");
        return 2;
    }
    let outcome = if rollback {
        apply_receipt(|rows| Ok(rollback_legacy_p0(rows)))
    } else if apply {
        apply_receipt(|rows| Ok(migrate_legacy_p0(rows, true)))
    } else {
        read_then(|rows| Ok(migrate_legacy_p0(rows, false)))
    };
    finish(verb, outcome)
}

/// `fno backlog migrate-difficulty [--apply] [--backfill]`.
pub fn run_migrate_difficulty(tail: &[String]) -> i32 {
    let verb = "migrate-difficulty";
    let flags = match parse_flags(verb, tail, &["--apply", "--backfill"], DIFFICULTY_HELP) {
        Ok(flags) => flags,
        Err(code) => return code,
    };
    let apply = flags.iter().any(|f| f == "--apply");
    let backfill = flags.iter().any(|f| f == "--backfill");
    let migrate = |rows: &mut [Value]| {
        if backfill {
            Ok(backfill_difficulty(rows, apply))
        } else {
            migrate_model_tier(rows, apply)
        }
    };
    let outcome = if apply {
        apply_receipt(migrate)
    } else {
        read_then(migrate)
    };
    finish(verb, outcome)
}

/// `fno backlog migrate-updated-at [--apply]`.
pub fn run_migrate_updated_at(tail: &[String]) -> i32 {
    let verb = "migrate-updated-at";
    let flags = match parse_flags(verb, tail, &["--apply"], UPDATED_AT_HELP) {
        Ok(flags) => flags,
        Err(code) => return code,
    };
    let apply = !flags.is_empty();
    let outcome = if apply {
        apply_receipt(|rows| Ok(migrate_updated_at(rows, true)))
    } else {
        read_then(|rows| Ok(migrate_updated_at(rows, false)))
    };
    finish(verb, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_p0_rebands_once_and_rollback_restores() {
        let mut rows = vec![
            json!({"id": "x-a", "priority": "p0"}),
            json!({"id": "x-b", "priority": "p0", "blocks_everything": true}),
            json!({"id": "x-c", "priority": "p2"}),
        ];
        let dry = migrate_legacy_p0(&mut rows.clone(), false);
        assert_eq!(
            sorted_json(&dry),
            r#"{"already_migrated": 0, "legacy_p0": 1, "rebanded_to_p1": 1, "remaining_unacknowledged_p0": 1}"#
        );
        let applied = migrate_legacy_p0(&mut rows, true);
        assert_eq!(applied["remaining_unacknowledged_p0"], json!(0));
        assert_eq!(rows[0]["priority"], json!("p1"));
        assert_eq!(rows[1]["priority"], json!("p0"), "acknowledged p0 stays");
        let again = migrate_legacy_p0(&mut rows, true);
        assert_eq!(
            (
                again["legacy_p0"].clone(),
                again["already_migrated"].clone()
            ),
            (json!(0), json!(1))
        );
        assert_eq!(rollback_legacy_p0(&mut rows), json!({"restored_to_p0": 1}));
        assert_eq!(rows[0]["priority"], json!("p0"));
        assert_eq!(rows[0]["priority_history"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn model_tier_drains_same_band_pairs_and_refuses_divergent_ones() {
        let mut rows = vec![
            json!({"id": "x-a", "model_tier": "High"}),
            json!({"id": "x-b", "model_tier": "low", "difficulty": "low"}),
            json!({"id": "x-c", "model_tier": "junk", "difficulty": "medium"}),
        ];
        let receipt = migrate_model_tier(&mut rows, true).unwrap();
        assert_eq!(receipt["migrated"], json!(3));
        assert_eq!(rows[0]["difficulty"], json!("high"));
        assert!(rows.iter().all(|r| r.get("model_tier").is_none()));
        assert_eq!(rows[2]["difficulty"], json!("medium"));

        let mut divergent = vec![
            json!({"id": "x-d", "model_tier": "low", "difficulty": "high"}),
            json!({"id": "x-e", "model_tier": "junk"}),
        ];
        let err = migrate_model_tier(&mut divergent, false).unwrap_err();
        assert!(
            err.contains("x-d model_tier='low' difficulty='high'"),
            "{err}"
        );
        assert!(
            err.contains("x-e model_tier='junk' difficulty=None"),
            "{err}"
        );
    }

    #[test]
    fn backfill_takes_the_strongest_signal_and_skips_unbanded_rows() {
        let mut rows = vec![
            json!({"id": "x-a", "size": "S", "type": "bug"}),
            json!({"id": "x-b", "priority": "p3"}),
            json!({"id": "x-c"}),
            json!({"id": "x-d", "size": "L", "model_tier": "low"}),
            json!({"id": "x-e", "difficulty": "low"}),
        ];
        let receipt = backfill_difficulty(&mut rows, true);
        assert_eq!(receipt["candidates"], json!(["x-a", "x-b"]));
        assert_eq!(receipt["written"], json!(["x-a", "x-b"]));
        assert_eq!(receipt["skipped"], json!(["x-c", "x-d"]));
        assert_eq!(receipt["already_band"], json!(1));
        assert_eq!(rows[0]["difficulty"], json!("high"));
        assert_eq!(
            rows[0]["difficulty_history"][0]["source"],
            json!("backfill")
        );
    }

    #[test]
    fn updated_at_residue_is_removed_only_on_apply() {
        let mut rows = vec![
            json!({"id": "x-a", "__updated_at": "t"}),
            json!({"id": "x-b"}),
        ];
        let dry = migrate_updated_at(&mut rows, false);
        assert_eq!(
            sorted_json(&dry),
            r#"{"apply": false, "candidate_count": 1, "candidates": ["x-a"], "removed": 0}"#
        );
        assert!(rows[0].get("__updated_at").is_some());
        assert_eq!(migrate_updated_at(&mut rows, true)["removed"], json!(1));
        assert!(rows[0].get("__updated_at").is_none());
    }
}
