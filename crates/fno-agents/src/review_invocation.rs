//! The `review-level invocations` door: the actions the deleted Python
//! `fno do review invocations` leaf served, served natively beside the
//! capability and findings actions. `fno` claims the leaf by its literal
//! three-token prefix and execs here, so the shared parse, sidecar and mint
//! have one owner (`fno::review_invocation`) and one subprocess surface.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// crate-visible: codex_inject paths through `review_invocation::owner` for
// the shared parse, sidecar and mint.
pub(crate) mod owner {
    // The pure core, vendored byte-identical from crates/fno (the parity
    // test pins the equality). The crates do not link in production: fno
    // shells this binary, so a shared module travels as a pinned copy.
    include!("review_invocation_owner.rs");
}

use owner::{
    adopt_pending_invocation, mint_invocation_id, parse_review_invocation, write_pending_invocation,
};

pub const USAGE: &str = "usage: fno-agents review-level invocations <action> [flags]

actions:
  parse --raw PAYLOAD            print the parse JSON, or null
  join --session S               adopt the sender's pending id, or mint
  reserve --session S [--json]   mint and write, adopt-and-retry on a busy slot
  emit --source S --data JSON    drop nulls, append one review_invocation row
  settle [--json]                settle every lost sent invocation";

/// Route the `review-level invocations` tail: everything after
/// `invocations`.
pub fn run_invocations_door(rest: &[String]) -> i32 {
    let Some(action) = rest.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let tail = &rest[1..];
    if tail.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    match action {
        "parse" => run_parse(tail),
        "join" => run_join(tail),
        "reserve" => run_reserve(tail),
        "emit" => run_emit(tail),
        "settle" => run_settle(tail),
        other => {
            eprintln!("{USAGE} (unknown action: {other})");
            2
        }
    }
}

/// The `--session S` value of a tail, or None when the flag is absent.
fn session_value(tail: &[String]) -> Option<&str> {
    tail.iter()
        .position(|a| a == "--session")
        .and_then(|i| tail.get(i + 1).map(String::as_str))
}

/// The `--raw PAYLOAD` value of a tail, or None when absent.
fn raw_value(tail: &[String]) -> Option<&str> {
    tail.iter()
        .position(|a| a == "--raw")
        .and_then(|i| tail.get(i + 1).map(String::as_str))
}

fn run_parse(tail: &[String]) -> i32 {
    let Some(raw) = raw_value(tail) else {
        eprintln!("{USAGE} (parse takes exactly --raw PAYLOAD)");
        return 2;
    };
    match parse_review_invocation(raw) {
        Some(parsed) => {
            println!("{}", serde_json::to_string(&parsed).unwrap_or_default())
        }
        None => println!("null"),
    }
    0
}

fn run_join(tail: &[String]) -> i32 {
    let Some(session) = session_value(tail) else {
        eprintln!("{USAGE} (join takes exactly --session S)");
        return 2;
    };
    // The hook's join, verbatim: adopt, else mint, write only when a session
    // exists. A bad session id is a miss, and the mint answers anyway.
    let joined = adopt_pending_invocation(session, None).unwrap_or_else(|| {
        let id = mint_invocation_id();
        if !session.is_empty() {
            write_pending_invocation(session, &id, None);
        }
        id
    });
    println!("{joined}");
    0
}

fn run_reserve(tail: &[String]) -> i32 {
    let json_out = tail.iter().any(|a| crate::json_output::is_flag(a));
    let Some(session) = session_value(tail) else {
        eprintln!("{USAGE} (reserve emits --session S [--json])");
        return 2;
    };
    // The mail send's reserve: mint, write, and on a busy slot adopt the
    // stale id and retry the write once with this send's own id.
    let invocation_id = mint_invocation_id();
    if !write_pending_invocation(session, &invocation_id, None) {
        adopt_pending_invocation(session, None);
        write_pending_invocation(session, &invocation_id, None);
    }
    if json_out {
        println!("{}", serde_json::json!({ "invocation_id": invocation_id }));
    } else {
        println!("{invocation_id}");
    }
    0
}

/// The journal the door reads and appends: `FNO_EVENTS_PATH` first (the
/// hook's hermeticity pin), else the space journal at `cwd`.
fn events_journal(cwd: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("FNO_EVENTS_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    crate::paths::events_path(cwd)
}

fn run_emit(tail: &[String]) -> i32 {
    let mut source: Option<&str> = None;
    let mut data: Option<&str> = None;
    let mut index = 0;
    while index < tail.len() {
        match tail.get(index).map(String::as_str) {
            Some("--source") => {
                source = tail.get(index + 1).map(String::as_str);
                index += 2;
            }
            Some("--data") => {
                data = tail.get(index + 1).map(String::as_str);
                index += 2;
            }
            _ => {
                index += 1;
            }
        }
    }
    let (Some(source), Some(data)) = (source, data) else {
        emit_usage();
        return 2;
    };
    let Ok(fields) = serde_json::from_str::<serde_json::Value>(data) else {
        eprintln!("invocations emit: --data is not valid JSON");
        return 2;
    };
    let Some(fields) = fields.as_object() else {
        eprintln!("invocations emit: --data must be a JSON object");
        return 2;
    };
    // Python's builder dropped only unmeasured (null) observations, one
    // level deep; the door keeps that shape.
    let fields: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let journal = events_journal(&std::env::current_dir().unwrap_or_default());
    let emitter = crate::events::EventEmitter::new(journal, source);
    match emitter.emit_fields("review_invocation", fields) {
        Ok(()) => {
            println!(r#"{{"committed": true}}"#);
            0
        }
        Err(error) => {
            eprintln!("invocations emit: {error}");
            3
        }
    }
}

fn emit_usage() {
    eprintln!("{USAGE} (emit takes exactly --source S --data JSON)");
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// `review.invocation_ttl_minutes` through the layered config candidates:
/// the first layer defining the leaf answers, an unreadable or sub-minute
/// value keeps the shipped default of 15.
fn invocation_ttl_minutes(cwd: &Path) -> i64 {
    for path in crate::agents_config::config_candidates(cwd) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(table) = content.parse::<toml::Table>() else {
            return 15;
        };
        if let Some(leaf) = table
            .get("review")
            .and_then(|v| v.get("invocation_ttl_minutes"))
        {
            return leaf.as_integer().filter(|v| *v >= 1).unwrap_or(15);
        }
    }
    15
}

/// `(head_sha, branch)` at `cwd`, both empty when either is unreadable (the
/// writer refuses a detached HEAD; the settle inherits that refusal).
fn settle_head_pin(cwd: Option<&Path>) -> (String, String) {
    fn git_out(cwd: Option<&Path>, args: &[&str]) -> String {
        let mut command = std::process::Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        match command.output() {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            }
            _ => String::new(),
        }
    }
    let head_sha = git_out(cwd, &["rev-parse", "HEAD"]);
    let branch = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if head_sha.is_empty() || branch.is_empty() || branch == "HEAD" {
        return (String::new(), String::new());
    }
    (head_sha, branch)
}

const UNJOINED: &str = "UNJOINED";

/// The settle sweep, ported from the deleted Python leg: every
/// `review_invocation` row with `stage: sent` and no answering
/// `review_attestation` (or `stage: refused` row) older than the TTL
/// becomes one `lost` attestation row, idempotently. Returns the receipt
/// rows in the deleted leaf's shape.
fn settle_lost(
    journal: &Path,
    cwd: Option<&Path>,
    ttl_minutes: i64,
    observed_ms: i64,
) -> Vec<serde_json::Value> {
    let cutoff = observed_ms - ttl_minutes * 60_000;
    let mut sent: BTreeMap<String, (i64, serde_json::Value)> = BTreeMap::new();
    let mut answered: HashSet<String> = HashSet::new();
    // A missing or unreadable store reads as empty: settling nothing is the
    // honest answer when no journal exists to read.
    let Ok(rows) = crate::event_store::query_events(
        journal,
        &crate::event_store::EventQuery::of_types(&["review_invocation", "review_attestation"]),
    ) else {
        return Vec::new();
    };
    for row in rows {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&row.line) else {
            continue;
        };
        let Some(data) = event.get("data").and_then(|d| d.as_object()) else {
            continue;
        };
        let Some(id) = data
            .get("invocation_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if id == UNJOINED {
            continue;
        }
        let stage = data.get("stage").and_then(|v| v.as_str());
        match event.get("type").and_then(|v| v.as_str()) {
            Some("review_invocation") if stage == Some("sent") => {
                // First sent row wins: the OLDER timestamp is the
                // conservative TTL input.
                sent.entry(id.to_string())
                    .or_insert((row.ts_ms, serde_json::Value::Object(data.clone())));
            }
            Some("review_invocation") if stage == Some("refused") => {
                answered.insert(id.to_string());
            }
            Some("review_attestation") => {
                answered.insert(id.to_string());
            }
            _ => {}
        }
    }
    let (head_sha, branch) = settle_head_pin(cwd);
    let mut out = Vec::new();
    for (id, (ts_ms, data)) in sent.iter() {
        if *ts_ms > cutoff || answered.contains(id) {
            continue;
        }
        if head_sha.is_empty() || branch.is_empty() {
            out.push(serde_json::json!({
                "invocation_id": id,
                "settled": false,
                "reason": "no readable head to pin (not a git repo or detached HEAD)",
            }));
            continue;
        }
        let reviewer = verb_reviewer(data);
        let mut fields = serde_json::Map::new();
        fields.insert("reviewer".into(), reviewer.into());
        fields.insert("head_sha".into(), head_sha.clone().into());
        fields.insert("verdict".into(), "fail".into());
        fields.insert("session_id".into(), "settle".into());
        fields.insert("output_contract".into(), "lost".into());
        fields.insert("invocation_id".into(), id.clone().into());
        fields.insert(
            "settle_note".into(),
            "invocation lost: sent, delivered, and never answered; emitted by the settle sweep"
                .into(),
        );
        let append = crate::events::EventEmitter::new(journal, "hook")
            .emit_fields("review_attestation", fields);
        match append {
            Ok(()) => out.push(serde_json::json!({
                "invocation_id": id,
                "settled": true,
                "head": head_sha,
            })),
            Err(error) => out.push(serde_json::json!({
                "invocation_id": id,
                "settled": false,
                "reason": error.to_string(),
            })),
        }
    }
    out
}

/// The settle row's reviewer: the verb's tail after the last colon and any
/// leading slash, `code-review` when nothing survives.
fn verb_reviewer(data: &serde_json::Value) -> String {
    let verb = data
        .get("verb")
        .and_then(|v| v.as_str())
        .unwrap_or("/code-review")
        .to_string();
    let reviewer = verb
        .trim_start_matches('/')
        .rsplit(':')
        .next()
        .unwrap_or("");
    if reviewer.is_empty() {
        "code-review".to_string()
    } else {
        reviewer.to_string()
    }
}

fn run_settle(tail: &[String]) -> i32 {
    let json_out = tail.iter().any(|a| crate::json_output::is_flag(a));
    let cwd = std::env::current_dir().ok();
    let journal = events_journal(cwd.as_deref().unwrap_or(Path::new(".")));
    let ttl = invocation_ttl_minutes(cwd.as_deref().unwrap_or(Path::new(".")));
    let rows = settle_lost(&journal, cwd.as_deref(), ttl, now_ms());
    if json_out {
        println!(
            "{}",
            serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
        );
    } else {
        for line in receipt_lines(&rows, ttl) {
            println!("{line}");
        }
    }
    0
}

/// The deleted leaf's receipt, byte for byte: the count line, then one line
/// per row naming the id, the verdict of the run, and the pinned head.
fn receipt_lines(rows: &[serde_json::Value], ttl: i64) -> Vec<String> {
    if rows.is_empty() {
        return vec![format!("invocations: none lost beyond the {ttl}m TTL")];
    }
    let settled = rows
        .iter()
        .filter(|r| r.get("settled") == Some(&serde_json::json!(true)))
        .count();
    let mut out = vec![format!(
        "invocations: {} lost, {} settled now",
        rows.len(),
        settled
    )];
    for row in rows {
        let id = row
            .get("invocation_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if row.get("settled") == Some(&serde_json::json!(true)) {
            let head = row.get("head").and_then(|v| v.as_str()).unwrap_or("");
            let head9: String = head.chars().take(9).collect();
            out.push(format!("  settled {id} at {head9}"));
        } else {
            let reason = row.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            out.push(format!("  refused {id}: {reason}"));
        }
    }
    out
}
