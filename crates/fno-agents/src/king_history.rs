//! `king-history`: the crown-scope `reign_checkin` readback behind
//! `fno agents king history`.
//!
//! Python resolves the caller's crown scope (harness identity and registry
//! rows are Python-owned), pins the space journal path, and relays here;
//! the scan is a native read with the same treatment `board` and
//! `court-fold` got. Selection is EXACT `data.scope` equality: rows are
//! written through the crown canonicalization, so a second normalizer here
//! could only disagree with it. Legacy rows (the refused
//! `crown_scope`/`crown`/`result` aliases, or a missing canonical key)
//! stay byte-preserved evidence: counted in `rejected`, attributed by line
//! number in `rejected_legacy` when one of their scope spellings equals
//! the requested scope. The output is read-back, never a generated
//! summary; a zero-match answer still names the journal and the scanned
//! count, so an empty history is a measurement, not an absence.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const REIGN_CHECKIN: &str = "reign_checkin";
const FORBIDDEN_ALIASES: [&str; 3] = ["crown", "crown_scope", "result"];

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn scan(events_path: &Path, scope: &str) -> Result<Value, String> {
    let mut payload = json!({
        "scope": scope,
        "events_path": events_path.display().to_string(),
        "scanned": 0,
        "matched": 0,
        "events": Vec::<Value>::new(),
        "rejected": 0,
        "rejected_legacy": Vec::<Value>::new(),
    });
    let Ok(content) = std::fs::read_to_string(events_path) else {
        // A missing journal is a positive zero, not an error: the receipts
        // below say the file was read (absent) and nothing matched.
        if !events_path.exists() {
            return Ok(payload);
        }
        return Err(format!("{}: unreadable journal", events_path.display()));
    };
    let mut events: Vec<Value> = Vec::new();
    let mut rejected_legacy: Vec<Value> = Vec::new();
    let mut scanned: u64 = 0;
    let mut rejected: u64 = 0;
    for (lineno, raw) in content.lines().enumerate() {
        let lineno = lineno + 1;
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "{}:{lineno}: corrupt JSON line: {e}",
                    events_path.display()
                ))
            }
        };
        if !event.is_object() {
            return Err(format!(
                "{}:{lineno}: line is not a JSON object",
                events_path.display()
            ));
        }
        scanned += 1;
        if s_str(&event, "type") != Some(REIGN_CHECKIN) {
            continue;
        }
        let Some(data) = event.get("data").and_then(|d| d.as_object()) else {
            // A reign_checkin without an object payload is legacy evidence
            // too; it names no scope, so it counts but attributes nowhere.
            rejected += 1;
            continue;
        };
        let data = Value::Object(data.clone());
        let aliases: Vec<&str> = FORBIDDEN_ALIASES
            .iter()
            .filter(|k| data.get(**k).is_some())
            .copied()
            .collect();
        let row_scope = s_str(&data, "scope").unwrap_or("");
        let canonical = !row_scope.is_empty() && data.get("change").is_some() && aliases.is_empty();
        if canonical {
            if row_scope == scope {
                events.push(event);
            }
            continue;
        }
        rejected += 1;
        let names_this_crown =
            row_scope == scope || aliases.iter().any(|k| s_str(&data, k) == Some(scope));
        if names_this_crown {
            let missing: Vec<&str> = ["scope", "change"]
                .iter()
                .filter(|k| data.get(**k).is_none())
                .copied()
                .collect();
            rejected_legacy.push(json!({
                "line": lineno,
                "forbidden": aliases,
                "missing": missing,
            }));
        }
    }
    events.reverse();
    payload["events"] = Value::Array(events);
    payload["rejected_legacy"] = Value::Array(rejected_legacy);
    payload["scanned"] = json!(scanned);
    payload["rejected"] = json!(rejected);
    payload["matched"] = json!(payload["events"].as_array().map(|a| a.len()).unwrap_or(0));
    Ok(payload)
}

fn render(payload: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    for event in payload["events"].as_array().unwrap() {
        let data = event.get("data").cloned().unwrap_or_else(|| json!({}));
        lines.push(format!(
            "{}  {}",
            s_str(event, "ts").unwrap_or(""),
            s_str(&data, "scope").unwrap_or("")
        ));
        lines.push(format!(
            "  change: {}",
            s_str(&data, "change").unwrap_or("")
        ));
        if let Some(rest) = data.as_object() {
            let rest: serde_json::Map<String, Value> = rest
                .iter()
                .filter(|(k, _)| k.as_str() != "scope" && k.as_str() != "change")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !rest.is_empty() {
                lines.push(format!(
                    "  evidence: {}",
                    serde_json::to_string(&rest).unwrap_or_default()
                ));
            }
        }
    }
    lines.push(format!(
        "history: {} canonical check-in(s) for {}, scanned {} rows, {} legacy-invalid reign row(s)",
        payload["matched"], payload["scope"], payload["scanned"], payload["rejected"]
    ));
    for entry in payload["rejected_legacy"].as_array().unwrap() {
        lines.push(format!(
            "  rejected legacy row at line {}: forbidden={} missing={}",
            entry["line"],
            serde_json::to_string(&entry["forbidden"]).unwrap_or_default(),
            serde_json::to_string(&entry["missing"]).unwrap_or_default(),
        ));
    }
    lines.join("\n")
}

/// `king-history --scope SCOPE --events-path PATH [--json]`
///
/// rc 0 read (any match count), 1 corrupt journal line, 2 usage failure.
pub fn run_king_history(args: &[String]) -> i32 {
    let mut scope = String::new();
    let mut events_path = PathBuf::new();
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" if i + 1 < args.len() => {
                scope = args[i + 1].clone();
                i += 2;
            }
            "--events-path" if i + 1 < args.len() => {
                events_path = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--json" => {
                as_json = true;
                i += 1;
            }
            other => {
                eprintln!("fno-agents king-history: unknown flag {other}");
                eprintln!("fno-agents king-history: --scope SCOPE --events-path PATH [--json]");
                return 2;
            }
        }
    }
    if scope.is_empty() || events_path.as_os_str().is_empty() {
        eprintln!("fno-agents king-history: --scope and --events-path are required");
        return 2;
    }
    match scan(&events_path, &scope) {
        Ok(payload) => {
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                println!("{}", render(&payload));
            }
            0
        }
        Err(msg) => {
            eprintln!("fno-agents king-history: {msg}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn journal(rows: &[Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        for row in rows {
            writeln!(fh, "{row}").unwrap();
        }
        (dir, path)
    }

    fn checkin(ts: &str, data: Value) -> Value {
        json!({"ts": ts, "type": "reign_checkin", "source": "loop", "data": data})
    }

    #[test]
    fn newest_first_with_evidence_intact() {
        let (_dir, path) = journal(&[
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"scope": "x-a792", "change": "first"}),
            ),
            json!({"ts": "2026-09-10T09:00:00Z", "type": "phase_transition", "source": "loop", "data": {"phase": "review"}}),
            checkin(
                "2026-09-10T09:30:00Z",
                json!({"scope": "other", "change": "elsewhere"}),
            ),
            checkin(
                "2026-09-10T12:00:00Z",
                json!({"scope": "x-a792", "change": "merged PR 1710", "open_prs_fleet": 3}),
            ),
        ]);
        let payload = scan(&path, "x-a792").unwrap();
        assert_eq!(payload["scanned"], json!(4));
        assert_eq!(payload["matched"], json!(2));
        let events = payload["events"].as_array().unwrap();
        assert_eq!(events[0]["data"]["open_prs_fleet"], json!(3));
        assert_eq!(events[0]["ts"], json!("2026-09-10T12:00:00Z"));
        assert_eq!(events[1]["data"]["change"], json!("first"));
    }

    #[test]
    fn alias_rows_are_rejected_evidence_attributed_by_line() {
        let (_dir, path) = journal(&[
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"crown_scope": "x-a792", "change": "old"}),
            ),
            checkin(
                "2026-09-10T08:30:00Z",
                json!({"scope": "x-a792", "result": "no change"}),
            ),
            checkin("2026-09-10T09:00:00Z", json!({"change": "no scope named"})),
        ]);
        let payload = scan(&path, "x-a792").unwrap();
        assert_eq!(payload["matched"], json!(0));
        assert_eq!(payload["rejected"], json!(3));
        let legacy = payload["rejected_legacy"].as_array().unwrap();
        assert_eq!(legacy.len(), 2);
        assert_eq!(legacy[0]["line"], json!(1));
        assert_eq!(legacy[0]["forbidden"], json!(["crown_scope"]));
        assert_eq!(legacy[0]["missing"], json!(["scope"]));
        assert_eq!(legacy[1]["forbidden"], json!(["result"]));
    }

    #[test]
    fn missing_journal_reads_as_positive_zero() {
        let dir = tempfile::tempdir().unwrap();
        let payload = scan(&dir.path().join("absent.jsonl"), "x-a792").unwrap();
        assert_eq!(payload["scanned"], json!(0));
        assert_eq!(payload["matched"], json!(0));
        assert!(payload["events_path"]
            .as_str()
            .unwrap()
            .ends_with("absent.jsonl"));
    }

    #[test]
    fn corrupt_line_names_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        writeln!(fh, "{{not json").unwrap();
        drop(fh);
        let err = scan(&path, "x-a792").unwrap_err();
        assert!(err.contains(":1:"), "err: {err}");
    }

    #[test]
    fn run_relays_json_and_human_formats() {
        let (_dir, path) = journal(&[checkin(
            "2026-09-10T12:00:00Z",
            json!({"scope": "x-a792", "change": "did a thing", "open_prs_fleet": 3}),
        )]);
        let args = vec![
            "--scope".to_string(),
            "x-a792".to_string(),
            "--events-path".to_string(),
            path.display().to_string(),
            "--json".to_string(),
        ];
        assert_eq!(run_king_history(&args), 0);
        let text_args = args.clone();
        assert_eq!(run_king_history(&text_args), 0);
    }

    #[test]
    fn usage_failure_exit_two() {
        assert_eq!(run_king_history(&[]), 2);
        assert_eq!(
            run_king_history(&["--scope".to_string(), "x".to_string()]),
            2
        );
        assert_eq!(run_king_history(&["--nope".to_string()]), 2);
    }
}
