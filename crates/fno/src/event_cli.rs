//! Native storage-level verbs under `fno doctor event`.
//!
//! The Python `fno doctor event emit` keeps its rich surface (validation,
//! attestation stamping, mirrors, parent push) and delegates storage to
//! `emit-envelope` here. `find`/`audit`/`gc` routing follows in the reader
//! cutover wave, intercepted by the same predicate; until then the Python
//! implementations stay authoritative for those names. No top-level verb
//! exists (law d-fe66560a); the group stays nested under `doctor`.

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;

#[path = "state_recovery.rs"]
mod recovery;

/// The verbs the native surface serves. `find` joins natively only when the
/// caller names stores explicitly (`--events`); a bare `find` keeps
/// forwarding to Python, whose front door resolves the journals and calls
/// back in with one `--events` per store. `audit`/`gc` join when their
/// Python output contracts are ported (reader cutover wave).
pub const NATIVE_EVENT_SUBCOMMANDS: &[&str] =
    &["emit-envelope", "export", "import", "rows", "recover"];

/// Classify `fno doctor event <sub> ...` for the front door: `Some(rest)`
/// runs natively, `None` forwards to the Python CLI.
pub fn classify_doctor_event(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() < 3 {
        return None;
    }
    let a0 = args[0].to_str()?;
    let a1 = args[1].to_str()?;
    let a2 = args[2].to_str()?;
    if a0 != "doctor" || a1 != "event" {
        return None;
    }
    if NATIVE_EVENT_SUBCOMMANDS.contains(&a2) {
        return Some(args[2..].to_vec());
    }
    // `find` is dual-homed: explicit stores run native, a bare find stays
    // with the Python journal resolver.
    if a2 == "find" && args[3..].iter().any(|a| a.to_str() == Some("--events")) {
        return Some(args[2..].to_vec());
    }
    None
}

/// Parse and run one native event subcommand; returns the exit code.
pub fn run(args: &[OsString]) -> i32 {
    let sub = args.first().and_then(|a| a.to_str()).unwrap_or("");
    let rest: &[OsString] = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "emit-envelope" => run_emit_envelope(rest),
        "export" => run_export(rest),
        "import" => run_import(rest),
        "rows" => run_rows(rest),
        "find" => run_find(rest),
        "recover" => recovery::run(rest),
        _ => {
            eprintln!(
                "error: expected a subcommand (emit-envelope | export | import | rows | find)"
            );
            2
        }
    }
}

fn run_emit_envelope(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut file: Option<PathBuf> = None;
    let mut requested_id: Option<String> = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        let tok = match tok.to_str() {
            Some(t) => t,
            None => continue,
        };
        match tok {
            "--events" => journal = it.next().map(PathBuf::from),
            "--id" => requested_id = it.next().map(|v| v.to_string_lossy().into_owned()),
            "--file" => file = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    let Some(journal) = journal else {
        eprintln!("error: --events <events.jsonl> is required (it names the sibling store)");
        return 2;
    };
    // The envelope arrives on stdin or from --file; exact bytes are stored
    // byte-for-byte, never re-serialized.
    let mut envelope = String::new();
    let read_result = match &file {
        Some(path) => std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut envelope)),
        None => std::io::stdin().read_to_string(&mut envelope),
    };
    if let Err(e) = read_result {
        eprintln!("error: could not read the envelope: {e}");
        return 1;
    }
    let requested = requested_id.as_deref();
    let result = crate::event_store::append_envelope(&journal, envelope.trim(), requested);
    match result {
        Ok(r) => {
            let receipt = serde_json::json!({
                "success": true,
                "store": r.store.display().to_string(),
                "event_id": r.event_id,
                "seq": r.seq,
                "retention_class": r.retention_class,
                "inserted": r.inserted,
                "suppressed": r.suppressed,
                "pending_occurrences": r.pending_occurrences,
            });
            println!("{receipt}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// One read pass for thin clients: import, then print every committed
/// envelope as a JSON array of lines. A missing store yields an empty array,
/// never an error, so callers keep one code path.
fn run_rows(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut types: Vec<String> = Vec::new();
    let mut include_rejected = false;
    let mut store_path_only = false;
    let mut legacy_fallback = false;
    let mut mode = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        let tok = match tok.to_str() {
            Some(t) => t,
            None => continue,
        };
        match tok {
            "--events" => journal = it.next().map(PathBuf::from),
            "--type" => {
                if let Some(v) = it.next() {
                    types.push(v.to_string_lossy().into_owned());
                }
            }
            "--include-rejected" => include_rejected = true,
            "--store-path-only" => store_path_only = true,
            // Pre-store journals have no store to query: answer the raw
            // bytes so the caller carries no legacy reader of its own.
            "--legacy-fallback" => legacy_fallback = true,
            "--query-json" => mode = Some("query"),
            "--status-stream" => mode = Some("status"),
            "--answered-questions" => mode = Some("answered"),
            "--next-answer" => mode = Some("next-answer"),
            _ => {}
        }
    }
    let journal = match journal {
        Some(j) => j,
        None => {
            eprintln!("error: --events is required");
            return 2;
        }
    };
    if store_path_only {
        println!(
            "{}",
            serde_json::json!({"store": crate::event_store::store_path(&journal)})
        );
        return 0;
    }
    if let Some(mode) = mode {
        let mut input = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut input) {
            eprintln!("read query: {e}");
            return 1;
        }
        let result = serde_json::from_str::<serde_json::Value>(&input)
            .map_err(|e| e.to_string())
            .and_then(|query| read_projection(&journal, mode, &query));
        return match result {
            Ok(value) => {
                println!(
                    "{}",
                    serde_json::json!({"projection":format!("--{}",match mode {"query"=>"query-json","status"=>"status-stream","answered"=>"answered-questions",_=>"next-answer"}),"rows":value})
                );
                0
            }
            Err(e) => {
                eprintln!("event read refused: {e}");
                1
            }
        };
    }
    // A journal that was never written must not gain a store as a side
    // effect of being read: absence is a fact callers distinguish. A
    // non-regular journal (a directory standing in for the index) is a
    // failed read, so the caller's own error path names it.
    if !journal.exists() && !crate::event_store::store_path(&journal).exists() {
        println!("[]");
        return 0;
    }
    if journal.exists() && !journal.is_file() {
        eprintln!("error: {} is not a regular file", journal.display());
        return 1;
    }
    if legacy_fallback && !crate::event_store::store_path(&journal).exists() {
        // No store: the raw journal is the whole history. Lines go back
        // unfiltered and unvalidated, exactly the pre-store read.
        let raw = std::fs::read_to_string(&journal).unwrap_or_default();
        let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        println!(
            "{}",
            serde_json::to_string(&lines).unwrap_or_else(|_| "[]".into())
        );
        return 0;
    }
    let _ = crate::event_store::import_all(&journal);
    let query = crate::event_store::EventQuery {
        types,
        include_rejected,
        ..Default::default()
    };
    match crate::event_store::query_events(&journal, &query) {
        Ok(rows) => {
            let lines: Vec<&str> = rows.iter().map(|r| r.line.as_str()).collect();
            println!(
                "{}",
                serde_json::to_string(&lines).unwrap_or_else(|_| "[]".into())
            );
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn read_projection(
    journal: &std::path::Path,
    mode: &str,
    input: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if mode == "next-answer" {
        return next_answer(input);
    }
    use crate::event_store::{query_events, recovery_envelope, EventQuery};
    let store = crate::event_store::store_path(journal);
    if mode == "status" && !store.exists() {
        let since = input["since_ts"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| {
                crate::event_store::parse_rfc3339_ms(s)
                    .ok_or_else(|| "invalid since timestamp".to_string())
            })
            .transpose()?;
        let read = |path: &std::path::Path| -> Result<(Vec<serde_json::Value>, usize), String> {
            use std::io::BufRead;
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
                Err(e) => return Err(e.to_string()),
            };
            let mut rows = Vec::new();
            let mut skipped = 0;
            for line in std::io::BufReader::new(file).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .filter(|v| {
                        v.is_object()
                            && v["ts"]
                                .as_str()
                                .and_then(crate::event_store::parse_rfc3339_ms)
                                .is_some()
                    }) {
                    Some(v) => rows.push(v),
                    None => skipped += 1,
                }
            }
            Ok((rows, skipped))
        };
        let (active, mut skipped) = read(journal)?;
        let first = active
            .first()
            .and_then(|v| v["ts"].as_str())
            .and_then(crate::event_store::parse_rfc3339_ms);
        let mut rows = if since.zip(first).is_some_and(|(s, f)| s > f) {
            Vec::new()
        } else {
            let (rows, n) = read(&journal.with_file_name(format!(
                "{}.1",
                journal.file_name().unwrap_or_default().to_string_lossy()
            )))?;
            skipped += n;
            rows
        };
        rows.extend(active);
        rows.retain(|v| {
            since.is_none_or(|s| {
                v["ts"]
                    .as_str()
                    .and_then(crate::event_store::parse_rfc3339_ms)
                    .is_some_and(|t| t >= s)
            })
        });
        return Ok(serde_json::json!([rows, skipped]));
    }
    if mode == "answered" && !store.exists() && journal.exists() {
        let raw = std::fs::read_to_string(journal).map_err(|e| e.to_string())?;
        return Ok(answer_records(
            raw.lines()
                .enumerate()
                .filter_map(|(i, line)| {
                    serde_json::from_str(line)
                        .ok()
                        .map(|v| (i as i64 + 1, false, v))
                })
                .collect(),
        ));
    }
    if !store.exists() {
        return Ok(if mode == "status" {
            serde_json::json!([[], 0])
        } else {
            serde_json::json!([])
        });
    }
    let query = EventQuery {
        types: input["types"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        session_id: input["session_id"].as_str().map(str::to_owned),
        since_ms: input["since_ms"].as_i64(),
        limit: input["limit"]
            .as_u64()
            .map(|n| u32::try_from(n).map_err(|_| "limit too large".to_string()))
            .transpose()?,
        include_rejected: input["include_rejected"].as_bool().unwrap_or(false),
        ..Default::default()
    };
    let rows = query_events(journal, &query)?;
    if mode == "answered" {
        return Ok(answer_records(
            rows.iter()
                .filter_map(|r| {
                    serde_json::from_str(&r.line)
                        .ok()
                        .map(|v| (r.seq, r.history_only, v))
                })
                .collect(),
        ));
    }

    let values: Vec<_> = rows
        .iter()
        .filter(|r| mode != "status" || !r.history_only)
        .map(|r| {
            if r.history_only {
                recovery_envelope(r)
            } else {
                serde_json::from_str(&r.line)
                    .unwrap_or_else(|_| serde_json::json!({"_corrupt":r.line}))
            }
        })
        .collect();
    if mode == "status" {
        let since = input["since_ts"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| {
                crate::event_store::parse_rfc3339_ms(s)
                    .ok_or_else(|| "invalid since timestamp".to_string())
            })
            .transpose()?;
        let values: Vec<_> = values
            .into_iter()
            .filter(|v| {
                v["ts"]
                    .as_str()
                    .and_then(crate::event_store::parse_rfc3339_ms)
                    .is_some_and(|t| since.is_none_or(|s| t >= s))
            })
            .collect();
        Ok(serde_json::json!([values, 0]))
    } else {
        Ok(serde_json::json!(values))
    }
}

fn answer_records(rows: Vec<(i64, bool, serde_json::Value)>) -> serde_json::Value {
    let asks: std::collections::HashMap<String, &(i64, bool, serde_json::Value)> = rows
        .iter()
        .filter(|(_, _, v)| v["type"] == "operator_question")
        .filter_map(|row| Some((row.2["data"]["question_id"].as_str()?.to_owned(), row)))
        .collect();
    let mut answers = Vec::new();
    for (seq, history, value) in &rows {
        if value["type"] != "operator_question_closed" || *history {
            continue;
        }
        let data = &value["data"];
        let Some(id) = data["question_id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(answer) = data["answer"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let ask = asks.get(id);
        if ask.is_some_and(|a| a.1 && *seq < a.0) {
            continue;
        }
        let origin = ask.map(|a| a.2["data"].clone()).unwrap_or_default();
        answers.push(serde_json::json!({"id":id,"asker":origin["asker"],"question":origin["question"].as_str().unwrap_or(""),"answer":answer,"closed_ts":value["ts"].as_str().unwrap_or(""),"closed_by":data["closed_by"].as_str().unwrap_or(""),"answer_seq":seq}));
    }
    answers.sort_by(|a, b| {
        (a["closed_ts"].as_str(), a["id"].as_str())
            .cmp(&(b["closed_ts"].as_str(), b["id"].as_str()))
    });
    serde_json::json!(answers)
}

fn next_answer(input: &serde_json::Value) -> Result<serde_json::Value, String> {
    let parse = |ts: &str| {
        chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|dt| (dt.timestamp(), dt.timestamp_subsec_nanos()))
    };
    let cursor = input["cursor"].as_str().unwrap_or("");
    let saved: Option<serde_json::Value> = serde_json::from_str(cursor).ok();
    let ts = saved
        .as_ref()
        .and_then(|v| v["ts"].as_str())
        .unwrap_or(cursor);
    let floor = if ts.is_empty() {
        (i64::MIN, 0)
    } else {
        parse(ts).ok_or("unreadable answer cursor")?
    };
    let saved_key = saved.as_ref().map(|v| {
        (
            floor,
            v["id"].as_str().unwrap_or("").to_string(),
            v["seq"].as_i64().unwrap_or(0),
        )
    });
    let addresses = input["addresses"]
        .as_array()
        .ok_or("answer addresses missing")?;
    let records = input["records"]
        .as_array()
        .ok_or("answer records missing")?;
    let mut candidates = Vec::new();
    for (i, record) in records.iter().enumerate() {
        if !addresses.iter().any(|v| v == &record["asker"]) {
            continue;
        }
        let Some(ts) = record["closed_ts"].as_str() else {
            continue;
        };
        let Some(ms) = parse(ts) else {
            continue;
        };
        let id = record["id"].as_str().ok_or("answer identity missing")?;
        let seq = record["answer_seq"].as_i64().unwrap_or(i as i64 + 1);
        let key = (ms, id.to_string(), seq);
        // Legacy cursors cannot distinguish peers at their saved timestamp.
        // New cursors retain identity so subsequent equal-time answers survive.
        if saved_key
            .as_ref()
            .map_or(ms <= floor, |saved| &key <= saved)
        {
            continue;
        }
        candidates.push((key, record));
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    let Some(((_, id, seq), record)) = candidates.first() else {
        return Ok(serde_json::json!({"prompt":null,"cursor":""}));
    };
    let prompt = format!(
        "Answer to your question {id} \"{}\": {}.",
        record["question"].as_str().unwrap_or(""),
        record["answer"].as_str().unwrap_or("")
    );
    let cursor = serde_json::json!({"ts":record["closed_ts"],"id":id,"seq":seq}).to_string();
    Ok(serde_json::json!({"prompt":prompt,"cursor":cursor}))
}

/// Ingest every uncommitted generation of the journal into the store, the
/// same sync the Rust readers run before their queries. Python readers call
/// this before reading so seeded fixtures and pre-cutover bytes are visible.
fn run_import(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        let tok = match tok.to_str() {
            Some(t) => t,
            None => continue,
        };
        if tok == "--events" {
            journal = it.next().map(PathBuf::from);
        }
    }
    let journal = match journal {
        Some(j) => j,
        None => {
            eprintln!("error: --events is required");
            return 2;
        }
    };
    match crate::event_store::import_all(&journal) {
        Ok(receipt) => {
            let payload = serde_json::json!({
                "success": true,
                "store": receipt.store.display().to_string(),
                "ingested": receipt.ingested,
                "corrupt": receipt.corrupt,
                "coalesced": receipt.coalesced,
            });
            println!("{payload}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_export(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        let tok = match tok.to_str() {
            Some(t) => t,
            None => continue,
        };
        match tok {
            "--events" => journal = it.next().map(PathBuf::from),
            "--out" => out = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    let journal = match journal {
        Some(j) => j,
        None => {
            eprintln!("error: --events is required");
            return 2;
        }
    };
    let out = match out {
        Some(o) => o,
        None => {
            eprintln!("error: --out is required");
            return 2;
        }
    };
    match crate::event_store::export_jsonl(&journal, &out) {
        Ok(n) => {
            let payload = serde_json::json!({
                "success": true,
                "store": crate::event_store::store_path(&journal).display().to_string(),
                "exported": n,
                "path": out.display().to_string(),
                "snapshot": true,
            });
            println!("{payload}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// The envelope fields a find kind may live in (`_QUERY_FIELDS` parity).
pub const FIND_QUERY_FIELDS: &[&str] = &["type", "kind", "event"];

/// `--since` accepts ISO-8601 or a duration such as `7d`.
fn parse_since_ms(raw: Option<&str>) -> Result<Option<i64>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    let amount_len = raw.chars().take_while(char::is_ascii_digit).count();
    if amount_len > 0 && amount_len == raw.len() - 1 {
        let (amount, unit) = raw.split_at(amount_len);
        let amount: i64 = amount.parse().map_err(|e| format!("--since: {e}"))?;
        let unit_ms: i64 = match unit {
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => {
                return Err(format!(
                    "--since must be ISO-8601 or a duration such as 7d: {raw:?}"
                ))
            }
        };
        return Ok(Some(now_ms() - amount * unit_ms));
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
        .map_err(|e| format!("--since must be ISO-8601 or a duration such as 7d: {raw:?}: {e}"))?;
    Ok(Some(parsed.timestamp_millis()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The first matching envelope field: top level, then `data`, then `payload`
/// (the same order Python find searches).
fn field_lookup<'a>(row: &'a serde_json::Value, field: &str) -> Option<&'a serde_json::Value> {
    if let Some(v) = row.get(field) {
        return Some(v);
    }
    for envelope in ["data", "payload"] {
        if let Some(nested) = row.get(envelope).and_then(|v| v.as_object()) {
            if let Some(v) = nested.get(field) {
                return Some(v);
            }
        }
    }
    None
}

/// Field-filter comparison, byte-compatible with Python find's
/// `_field_matches`: strings equal, booleans by lowercase name, null against
/// `null`, anything else by compact JSON.
fn field_matches(actual: &serde_json::Value, expected: &str) -> bool {
    match actual {
        serde_json::Value::String(s) => s == expected,
        serde_json::Value::Bool(b) => b.to_string() == expected.to_lowercase(),
        serde_json::Value::Null => expected.eq_ignore_ascii_case("null"),
        other => serde_json::to_string(other).is_ok_and(|compact| compact == expected),
    }
}

/// The kind of an envelope: first non-empty of type/kind/event.
fn envelope_kind(row: &serde_json::Value) -> Option<(&str, &'static str)> {
    for field in FIND_QUERY_FIELDS {
        match row.get(field).and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => return Some((s, field)),
            _ => {}
        }
    }
    None
}

/// Represented occurrences of one stored row: the summary rows a flushed
/// observation window writes carry `occurrence_count`; everything else is 1.
fn row_occurrence(line: &str) -> i64 {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("occurrence_count")
                .and_then(|c| c.as_i64())
                .or_else(|| {
                    v.get("data")
                        .and_then(|d| d.get("occurrence_count"))
                        .and_then(|c| c.as_i64())
                })
        })
        .unwrap_or(1)
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| ms.to_string())
}

/// `fno doctor event find`: query stores for envelopes by kind, fields,
/// session, and a `since` bound, with a coverage receipt serializing the
/// store's own receipt per store, and how far back the answer is proven.
/// Exit 3 refuses a zero that coverage cannot back: a missing row before the
/// proven start never reads as a confident zero.
fn run_find(args: &[OsString]) -> i32 {
    let mut journals: Vec<PathBuf> = Vec::new();
    let mut kind: Option<String> = None;
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut since_raw: Option<String> = None;
    let mut session: Option<String> = None;
    let mut limit: usize = 50;
    let mut kinds_mode = false;
    let mut json_mode = false;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        let Some(tok) = tok.to_str() else { continue };
        match tok {
            "--events" => {
                if let Some(v) = it.next() {
                    journals.push(PathBuf::from(v));
                }
            }
            "--field" => {
                if let Some(v) = it.next() {
                    if let Some((k, val)) = v.to_string_lossy().split_once('=') {
                        fields.push((k.to_string(), val.to_string()));
                    }
                }
            }
            "--session-id" | "--session" => {
                if let Some(v) = it.next() {
                    session = Some(v.to_string_lossy().into_owned());
                }
            }
            "--since" => {
                if let Some(v) = it.next() {
                    since_raw = Some(v.to_string_lossy().into_owned());
                }
            }
            "--limit" => {
                if let Some(v) = it.next() {
                    match v.to_string_lossy().parse::<usize>() {
                        Ok(n) if n >= 1 => limit = n,
                        _ => {
                            eprintln!("error: --limit must be a positive integer");
                            return 2;
                        }
                    }
                }
            }
            "--kinds" => kinds_mode = true,
            "--json" | "-J" => json_mode = true,
            other if other.starts_with('-') => {
                eprintln!("error: unknown flag {other}");
                return 2;
            }
            other => {
                if kind.is_some() {
                    eprintln!("error: pass KIND or --kinds, not both");
                    return 2;
                }
                kind = Some(other.to_string());
            }
        }
    }
    if kind.is_some() && kinds_mode {
        eprintln!("error: pass KIND or --kinds, not both");
        return 2;
    }
    let since_ms = match parse_since_ms(since_raw.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    if journals.is_empty() {
        eprintln!("error: --events <events.jsonl> is required (repeatable)");
        return 2;
    }
    // Rotations and mirrors of one store collapse to their live journal
    // before the fold; each distinct live journal answers once.
    journals.sort();
    journals.dedup();
    let mut file_entries: Vec<serde_json::Value> = Vec::new();
    let mut matches: Vec<serde_json::Value> = Vec::new();
    let mut match_count = 0u64;
    let mut row_count = 0u64;
    let mut occurrence_count = 0i64;
    let mut journal_count = 0usize;
    let mut rotated_count = 0usize;
    for journal in &journals {
        let name = journal
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let rotated = name
            .rsplit_once('.')
            .map(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false);
        if rotated {
            rotated_count += 1;
        } else {
            journal_count += 1;
        }
        let entry = fold_journal(
            journal,
            kind.as_deref(),
            &fields,
            since_ms,
            session.as_deref(),
            limit,
        );
        let unreadable = entry["status"] == "unreadable";
        file_entries.push(entry.clone());
        if unreadable {
            continue;
        }
        row_count += entry["rows"].as_u64().unwrap_or(0);
        match_count += entry["matches"].as_u64().unwrap_or(0);
        occurrence_count += entry["occurrence_count"].as_i64().unwrap_or(0);
        for row in entry["matching_rows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if (matches.len() as usize) < limit {
                matches.push(row);
            }
        }
    }
    // kind_counts fold across files, at output time, from per-file entries.
    let mut kind_counts = serde_json::Map::new();
    for entry in &file_entries {
        let path = entry["path"].as_str().unwrap_or("").to_string();
        for (name, counts) in entry["kind_counts"].as_object().into_iter().flatten() {
            let slot = kind_counts
                .entry(name.clone())
                .or_insert_with(|| {
                    serde_json::json!({
                        "count": 0,
                        "keys": { "type": 0, "kind": 0, "event": 0 },
                        "files": {},
                    })
                })
                .as_object_mut()
                .unwrap();
            slot["count"] = serde_json::json!(
                slot["count"].as_u64().unwrap_or(0) + counts["count"].as_u64().unwrap_or(0)
            );
            for f in FIND_QUERY_FIELDS {
                slot["keys"][f] = serde_json::json!(
                    slot["keys"][f].as_u64().unwrap_or(0) + counts["keys"][f].as_u64().unwrap_or(0)
                );
            }
            slot["files"][path.as_str()] = serde_json::json!(counts["count"].as_u64().unwrap_or(0));
        }
    }
    // Aggregate coverage: the worst per-store status wins, the proven start
    // is the latest across stores, and observed bounds span all stores.
    let unreadable: Vec<&serde_json::Value> = file_entries
        .iter()
        .filter(|e| e["status"] == "unreadable")
        .collect();
    let coverages: Vec<&serde_json::Value> = file_entries
        .iter()
        .filter_map(|e| e.get("coverage"))
        .collect();
    let status_rank = |s: &str| match s {
        "unreadable" => 3,
        "unknown" => 2,
        "partial" => 1,
        _ => 0,
    };
    let worst = coverages
        .iter()
        .map(|c| c["status"].as_str().unwrap_or("unknown"))
        .max_by_key(|s| status_rank(s))
        .unwrap_or("unknown");
    // The joint answer is proven only from the LATEST proven start across
    // the stores (RFC3339 strings order chronologically).
    let complete_since = coverages
        .iter()
        .filter_map(|c| c["complete_since"].as_str())
        .max();
    let observed_first = coverages
        .iter()
        .filter_map(|c| c["observed_first"].as_str())
        .min();
    let observed_last = coverages
        .iter()
        .filter_map(|c| c["observed_last"].as_str())
        .max();
    let reason = if worst == "unreadable" {
        unreadable
            .first()
            .and_then(|u| u["error"].as_str())
            .map(str::to_string)
    } else if worst == "unknown" {
        coverages
            .iter()
            .find(|c| c["status"] == "unknown")
            .and_then(|c| c["reason"].as_str())
            .map(str::to_string)
    } else {
        None
    };
    let complete = worst == "complete" && unreadable.is_empty();
    let aggregate = serde_json::json!({
        "status": if complete { "complete" } else { worst },
        "complete_since": complete_since,
        "requested_since": since_ms.map(rfc3339),
        "observed_first": observed_first,
        "observed_last": observed_last,
        "reason": reason,
    });
    let complete_count = if complete {
        serde_json::json!(match_count)
    } else {
        serde_json::Value::Null
    };
    let payload = serde_json::json!({
        "kind": kind,
        "kinds": kinds_mode,
        "match_count": match_count,
        "returned_count": matches.len(),
        "row_count": row_count,
        "file_count": file_entries.len(),
        "journal_count": journal_count,
        "rotated_count": rotated_count,
        "observed_span": if observed_first.is_some() || observed_last.is_some() {
            serde_json::json!({
                "earliest": observed_first,
                "latest": observed_last,
            })
        } else {
            serde_json::Value::Null
        },
        "fields_searched": FIND_QUERY_FIELDS,
        "files": file_entries,
        "matches": matches,
        "kind_counts": kind_counts,
        "unreadable_files": unreadable.iter().map(|u| serde_json::json!({
            "path": u["path"],
            "status": u["status"],
            "error": u["error"].as_str().unwrap_or(""),
        })).collect::<Vec<_>>(),
        "occurrence_count": occurrence_count,
        "complete_count": complete_count,
        "coverage": aggregate,
    });
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
        );
        return find_exit_code(&aggregate, match_count, &unreadable);
    }
    for entry in &file_entries {
        let path = entry["path"].as_str().unwrap_or("");
        if entry["status"] == "unreadable" {
            println!(
                "unreadable: {path}: {}",
                entry["error"].as_str().unwrap_or("")
            );
        }
    }
    if kinds_mode {
        if kind_counts.is_empty() {
            println!("event kinds: none");
        } else {
            println!("event kinds:");
            for (name, counts) in &kind_counts {
                println!("  {name}: {}", counts["count"]);
            }
        }
    } else {
        for row in &matches {
            let key = envelope_kind(row).map(|(_, f)| f).unwrap_or("unknown");
            println!(
                "match (key: {key}): {}",
                serde_json::to_string(row).unwrap_or_default()
            );
        }
        if matches.is_empty() {
            println!("no matches");
        }
    }
    let (status, reason) = (
        aggregate["status"].as_str().unwrap_or("unknown"),
        aggregate["reason"].as_str(),
    );
    let complete_since = aggregate["complete_since"].as_str();
    if match_count == 0 && status != "complete" {
        match status {
            "partial" => println!(
                "count refused: no rows, and nothing before {t} is proven",
                t = complete_since.unwrap_or("?")
            ),
            _ => println!(
                "count refused: no rows, and {r}",
                r = reason.unwrap_or("the store predates the coverage epoch")
            ),
        }
    } else if status == "complete" {
        println!(
            "complete since {t}; earlier unknown",
            t = complete_since.unwrap_or("?")
        );
    } else {
        println!("coverage {status}: {r}", r = reason.unwrap_or("unknown"));
    }
    println!(
        "{match_count} matches in {row_count} rows across {n} files",
        n = file_entries.len()
    );
    find_exit_code(&aggregate, match_count, &unreadable)
}

/// The verdict for one find run: 0 answered; 3 refuses a zero that coverage
/// cannot back (any unreadable store, or an empty result short of `complete`).
fn find_exit_code(
    aggregate: &serde_json::Value,
    match_count: u64,
    unreadable: &[&serde_json::Value],
) -> i32 {
    if !unreadable.is_empty() {
        return 3;
    }
    if match_count == 0 && aggregate["status"] != "complete" {
        return 3;
    }
    0
}

/// Query and fold one journal's store into its file entry: counts, per-key
/// hits, span, kind counts, matching rows, and the store's coverage receipt.
#[allow(clippy::too_many_arguments)]
fn fold_journal(
    journal: &PathBuf,
    kind: Option<&str>,
    fields: &[(String, String)],
    since_ms: Option<i64>,
    session: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let display = journal.display().to_string();
    let store = crate::event_store::store_path(journal);
    let absent = !journal.exists() && !store.exists();
    let mut entry = serde_json::json!({
        "path": display,
        "status": "readable",
        "rows": 0,
        "matches": 0,
        "keys": { "type": 0, "kind": 0, "event": 0 },
        "span": { "earliest": serde_json::Value::Null, "latest": serde_json::Value::Null },
        "kind_counts": {},
        "matching_rows": [],
    });
    if absent {
        entry["status"] = serde_json::Value::String("absent".into());
        entry["coverage"] = serde_json::json!({
            "status": "unreadable",
            "complete_since": serde_json::Value::Null,
            "requested_since": serde_json::Value::Null,
            "observed_first": serde_json::Value::Null,
            "observed_last": serde_json::Value::Null,
            "reason": format!("store {} does not exist", store.display()),
        });
        return entry;
    }
    if let Err(e) = crate::event_store::import_all(journal) {
        entry["status"] = serde_json::Value::String("unreadable".into());
        entry["error"] = serde_json::Value::String(e);
        return entry;
    }
    let query = crate::event_store::EventQuery {
        include_rejected: false,
        ..Default::default()
    };
    let rows = match crate::event_store::query_events(journal, &query) {
        Ok(rows) => rows,
        Err(e) => {
            entry["status"] = serde_json::Value::String("unreadable".into());
            entry["error"] = serde_json::Value::String(e);
            return entry;
        }
    };
    let mut earliest: Option<(i64, String)> = None;
    let mut latest: Option<(i64, String)> = None;
    let mut all_rows = 0u64;
    let mut matched = 0u64;
    let mut occurrence_total = 0i64;
    let mut all_matches: Vec<serde_json::Value> = Vec::new();
    let mut per_kind: Vec<(String, u64, [u64; 3])> = Vec::new();
    for row in rows {
        let parsed: serde_json::Value = serde_json::from_str(&row.line)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        all_rows += 1;
        // Span tracks every readable row, keyed by the raw ts spelling.
        if let Some(ts_raw) = parsed.get("ts").and_then(|v| v.as_str()) {
            if earliest.as_ref().map_or(true, |(ms, _)| row.ts_ms < *ms) {
                earliest = Some((row.ts_ms, ts_raw.to_string()));
            }
            if latest.as_ref().map_or(true, |(ms, _)| row.ts_ms >= *ms) {
                latest = Some((row.ts_ms, ts_raw.to_string()));
            }
        }
        let (k, field) = match envelope_kind(&parsed) {
            Some(v) => v,
            None => continue,
        };
        entry["keys"][field] = serde_json::json!(entry["keys"][field].as_u64().unwrap_or(0) + 1);
        let kind_match = match kind {
            Some(want) => k == want,
            None => true,
        };
        if !kind_match {
            continue;
        }
        if let Some(s) = since_ms {
            if row.ts_ms < s {
                continue;
            }
        }
        if let Some(session) = session {
            let hit = [
                "session_id",
                "target_session",
                "target_session_id",
                "to_session_id",
            ]
            .iter()
            .any(|f| {
                field_lookup(&parsed, f)
                    .map(|v| field_matches(v, session))
                    .unwrap_or(false)
            });
            if !hit {
                continue;
            }
        }
        if !fields.iter().all(|(f, want)| {
            field_lookup(&parsed, f)
                .map(|v| field_matches(v, want))
                .unwrap_or(false)
        }) {
            continue;
        }
        matched += 1;
        // kind_counts folds by winning field, per file, for the aggregate.
        let slot = per_kind.iter_mut().find(|(name, _, _)| name == k);
        match slot {
            Some((_, count, keys)) => {
                *count += 1;
                if let Some(idx) = FIND_QUERY_FIELDS.iter().position(|f| *f == field) {
                    keys[idx] += 1;
                }
            }
            None => {
                let mut keys = [0u64; 3];
                if let Some(idx) = FIND_QUERY_FIELDS.iter().position(|f| *f == field) {
                    keys[idx] += 1;
                }
                per_kind.push((k.to_string(), 1, keys));
            }
        }
        // Represented occurrences: a flushed window's summary row carries
        // the total, everything else represents one.
        let occ = row_occurrence(&row.line);
        occurrence_total += occ;
        if (all_matches.len() as u64) < limit as u64 {
            all_matches.push(parsed);
        }
    }
    // The store's own receipt: how far back a count over this store is
    // proven for the asked kinds.
    let cov_types: Vec<String> = kind.map(|k| vec![k.to_string()]).unwrap_or_default();
    let cov = crate::event_store::coverage(journal, since_ms, &cov_types);
    entry["coverage"] = serde_json::json!({
        "status": cov.status,
        "complete_since": cov.complete_since_ms.map(rfc3339),
        "requested_since": cov.requested_since_ms.map(rfc3339),
        "observed_first": cov.observed_first_ms.map(rfc3339),
        "observed_last": cov.observed_last_ms.map(rfc3339),
        "reason": cov.reason,
    });
    entry["rows"] = serde_json::json!(all_rows);
    entry["matches"] = serde_json::json!(matched);
    entry["occurrence_count"] = serde_json::json!(occurrence_total);
    entry["matching_rows"] = serde_json::Value::Array(all_matches);
    entry["span"] = serde_json::json!({
        "earliest": earliest.map(|(_, s)| s),
        "latest": latest.map(|(_, s)| s),
    });
    for (name, count, keys) in per_kind {
        let mut keys_obj = serde_json::Map::new();
        for (i, f) in FIND_QUERY_FIELDS.iter().enumerate() {
            keys_obj.insert(f.to_string(), serde_json::json!(keys[i]));
        }
        entry["kind_counts"][&name] = serde_json::json!({
            "count": count,
            "keys": keys_obj,
        });
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn classify_admits_only_native_subcommands() {
        assert!(classify_doctor_event(&mk(&["doctor", "event", "recover", "--dry-run"])).is_some());
        assert!(classify_doctor_event(&mk(&[
            "doctor",
            "event",
            "emit-envelope",
            "--events",
            "/tmp/x"
        ]))
        .is_some());
        assert!(
            classify_doctor_event(&mk(&["doctor", "event", "export", "--out", "/tmp/x"])).is_some()
        );
        // Python-owned names keep forwarding to the Python CLI.
        assert!(
            classify_doctor_event(&mk(&["doctor", "event", "emit", "-t", "blocked"])).is_none()
        );
        assert!(classify_doctor_event(&mk(&["doctor", "event", "gc", "--dry-run"])).is_none());
        assert!(classify_doctor_event(&mk(&["doctor", "event"])).is_none());
        assert!(classify_doctor_event(&mk(&["doctor"])).is_none());
    }

    #[test]
    fn find_routes_native_only_with_explicit_stores() {
        recovery_and_reader_contracts();
        // Explicit stores run natively.
        assert!(classify_doctor_event(&mk(&[
            "doctor",
            "event",
            "find",
            "guard_decision",
            "--events",
            "/tmp/x/events.jsonl"
        ]))
        .is_some());
        // A bare find stays with the Python journal resolver.
        assert!(
            classify_doctor_event(&mk(&["doctor", "event", "find", "guard_decision"])).is_none()
        );
        assert!(classify_doctor_event(&mk(&["doctor", "event", "find", "--kinds"])).is_none());
    }

    fn recovery_and_reader_contracts() {
        use crate::event_store::{append_envelope, ensure_schema, query_events, EventQuery};
        let root = std::env::temp_dir().join(format!(
            "fno-recovery-proof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("db")).unwrap();
        let stamp = root.join("backups/state-root-migration/first");
        std::fs::create_dir_all(&stamp).unwrap();
        let raw_question = root.join("legacy-questions.jsonl");
        std::fs::write(&raw_question,"{\"type\":\"operator_question\",\"data\":{\"question_id\":\"q-legacy\",\"asker\":\"alice\",\"question\":\"ask?\"}}\n{\"ts\":\"2026-09-30T01:00:00Z\",\"type\":\"operator_question_closed\",\"data\":{\"question_id\":\"q-legacy\",\"answer\":\"yes\",\"closed_by\":\"operator\"}}\n").unwrap();
        let legacy = read_projection(&raw_question, "answered", &serde_json::json!({})).unwrap();
        assert_eq!(legacy[0]["asker"], "alice");
        assert!(
            !crate::event_store::store_path(&raw_question).exists(),
            "legacy reads create no store"
        );
        for family in ["questions", "decisions", "events"] {
            let path = root.join("db").join(format!("{family}.db"));
            let mut c = rusqlite::Connection::open(&path).unwrap();
            ensure_schema(&mut c, &path).unwrap();
        }
        for family in ["approvals", "graph-archive"] {
            let c =
                rusqlite::Connection::open(root.join("db").join(format!("{family}.db"))).unwrap();
            c.execute_batch("CREATE TABLE facts(id TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
        }
        let ask=serde_json::json!({"ts":"2026-09-30T00:00:00Z","type":"operator_question","source":"test","data":{"question_id":"q-copy","asker":"alice","question":"restore?"}}).to_string();
        let close=serde_json::json!({"ts":"2026-09-30T00:00:00Z","type":"operator_question_closed","source":"test","data":{"question_id":"q-copy","answer":"old","closed_by":"operator"}}).to_string();
        let q = root.join("db/questions.jsonl");
        let src = stamp.join("questions.jsonl");
        append_envelope(&q, &close, Some("old-answer")).unwrap();
        append_envelope(&src, &ask, Some("missing-ask")).unwrap();
        let control=serde_json::json!({"ts":"2026-09-30T00:00:00Z","type":"status_control","source":"test","data":{"n":1}}).to_string();
        let historical=serde_json::json!({"ts":"2026-09-30T00:00:00Z","type":"status_control","source":"test","data":{"n":0}}).to_string();
        let events = root.join("db/events.jsonl");
        append_envelope(&events, &control, Some("control-one")).unwrap();
        append_envelope(
            &stamp.join("events.jsonl"),
            &historical,
            Some("missing-event"),
        )
        .unwrap();
        append_envelope(&stamp.join("decisions.jsonl"),&serde_json::json!({"ts":"2026-09-30T00:00:00Z","type":"operator_decision","source":"test","data":{"answer":"historical"}}).to_string(),Some("missing-decision")).unwrap();
        let before = recovery::audit(&root).unwrap();
        assert!(before.errors.is_empty(), "{:?}", before.errors);
        assert_eq!(before.missing_by_family["questions"], 1);
        assert_eq!(before.missing_by_family["events"], 1);
        let source_hashes: Vec<_> = before
            .records
            .iter()
            .map(|r| r["source_sha256"].clone())
            .collect();
        let wrong = recovery::apply(&root, "wrong-packet", &[]).unwrap_err();
        assert!(wrong.contains("packet changed"));
        let interrupted = rusqlite::Connection::open(root.join("db/decisions.db")).unwrap();
        interrupted.execute_batch("CREATE TRIGGER simulate_interruption BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'copy-interruption'); END;").unwrap();
        drop(interrupted);
        let failed = recovery::apply(&root, &before.packet_digest, &[]).unwrap_err();
        assert!(failed.contains("copy-interruption"));
        let committed = rusqlite::Connection::open(root.join("db/questions.db")).unwrap();
        assert_eq!(
            committed
                .query_row(
                    "SELECT count(*) FROM recovery_history WHERE event_id='missing-ask'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1,
            "the first family committed its marker with its row"
        );
        drop(committed);
        let interrupted = rusqlite::Connection::open(root.join("db/decisions.db")).unwrap();
        assert_eq!(
            interrupted
                .query_row(
                    "SELECT count(*) FROM events WHERE event_id='missing-decision'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0,
            "the interrupted family exposed no partial rows"
        );
        interrupted
            .execute_batch("DROP TRIGGER simulate_interruption")
            .unwrap();
        drop(interrupted);
        let receipt = recovery::apply(&root, &before.packet_digest, &[]).unwrap();
        assert_eq!(receipt["inserted"]["questions"], 0);
        assert_eq!(receipt["inserted"]["decisions"], 1);
        assert_eq!(receipt["inserted"]["events"], 1);
        let after = recovery::audit(&root).unwrap();
        assert!(after.zero_missing);
        assert_eq!(
            after
                .records
                .iter()
                .map(|r| r["source_sha256"].clone())
                .collect::<Vec<_>>(),
            source_hashes
        );
        let rows = query_events(&q, &EventQuery::default()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[1].seq > rows[0].seq);
        assert!(rows[1].history_only);
        assert_eq!(rows[1].line, ask);
        assert!(
            read_projection(&q, "answered", &serde_json::json!({}))
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty(),
            "a recovered ask must not wake an old answer"
        );
        let status = read_projection(
            &events,
            "status",
            &serde_json::json!({"since_ts":"2026-09-30T00:00:00Z"}),
        )
        .unwrap();
        assert_eq!(
            status[0].as_array().unwrap().len(),
            1,
            "historical equal-timestamp rows never enter occurrence counting"
        );
        assert_eq!(status[0][0]["data"]["n"], 1);
        let new_close = close.replace("\"old\"", "\"new\"");
        append_envelope(&q, &new_close, Some("new-answer")).unwrap();
        let answers = read_projection(&q, "answered", &serde_json::json!({})).unwrap();
        assert_eq!(answers.as_array().unwrap().len(), 1);
        assert_eq!(answers[0]["answer"], "new");
        assert_eq!(answers[0]["asker"], "alice");
        let repeated = recovery::apply(&root, &before.packet_digest, &[]).unwrap();
        assert_eq!(repeated["inserted"]["questions"], 0);
        let db = rusqlite::Connection::open(root.join("db/questions.db")).unwrap();
        db.execute(
            "DELETE FROM recovery_history WHERE event_id='missing-ask'",
            [],
        )
        .unwrap();
        db.execute("DELETE FROM events WHERE event_id='missing-ask'", [])
            .unwrap();
        drop(db);
        let resumed = recovery::apply(&root, &before.packet_digest, &[]).unwrap();
        assert_eq!(
            resumed["inserted"]["questions"], 1,
            "a family rollback safely resumes by approved identity"
        );
        let source = rusqlite::Connection::open(stamp.join("questions.db")).unwrap();
        source
            .execute("UPDATE events SET event_id='different-identity'", [])
            .unwrap();
        drop(source);
        let conflict = recovery::audit(&root).unwrap();
        assert!(conflict
            .errors
            .iter()
            .any(|e| e.contains("row_hash conflict")));
        assert!(!conflict.zero_missing);
        std::fs::remove_dir_all(root).unwrap();
    }
}
