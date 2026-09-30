//! Windowed mechanical eval of a Claude king's reign transcript.

use crate::paths::AgentsHome;
use crate::provenance::BusIndex;
use crate::session_activity::{Activity, ActivityFold};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const REIGN_EVAL_INTERVAL_S: u64 = 600;

struct Window {
    n: usize,
    start: Option<String>,
    end: Option<String>,
    tool_calls: u64,
    tools: BTreeMap<String, u64>,
    buckets: BTreeMap<String, u64>,
    wakes: BTreeMap<String, u64>,
    activity: Activity,
    activity_fold: ActivityFold,
    summary: Option<String>,
}

impl Window {
    fn new(n: usize, start: Option<String>) -> Self {
        Self {
            n,
            start,
            end: None,
            tool_calls: 0,
            tools: BTreeMap::new(),
            buckets: BTreeMap::new(),
            wakes: BTreeMap::new(),
            activity: Activity::default(),
            activity_fold: ActivityFold::default(),
            summary: None,
        }
    }

    fn json(&self) -> Value {
        let errors = self.activity.tool_errors.values().sum::<u64>();
        let hours = span_hours(self.start.as_deref(), self.end.as_deref());
        let mut typed = 0u64;
        let mut machine = 0u64;
        for (class, count) in &self.wakes {
            match class.as_str() {
                "operator" | "unknown" => typed += count,
                _ => machine += count,
            }
        }
        json!({
            "n": self.n,
            "start": self.start,
            "end": self.end,
            "hours": hours,
            "tool_calls": self.tool_calls,
            "tools": self.tools,
            "errors": errors,
            "error_classes": self.activity.tool_errors,
            "buckets": self.buckets,
            "wakes": { "typed": typed, "machine": machine, "by_class": self.wakes },
            "tokens": { "output": self.activity.tokens.output, "cache_read": self.activity.tokens.cache_read },
            "requests": self.activity.assistant_ts.len(),
        })
    }
}

#[derive(Debug, Clone)]
struct TypedTurn {
    ts: String,
    class: String,
    text: String,
}

#[derive(Debug, Clone)]
struct RepeatedRefusal {
    lead: String,
    count: u64,
    windows: BTreeSet<usize>,
}

struct Fold {
    session: String,
    transcript: PathBuf,
    since: Option<String>,
    until: Option<String>,
    compaction_ceiling: i64,
    windows: Vec<Window>,
    entries: Vec<crate::reign_hygiene::Entry>,
    typed_turns: Vec<TypedTurn>,
    repeated_refusals: BTreeMap<String, RepeatedRefusal>,
    merges: Vec<Value>,
    spawns: Vec<Value>,
    crown_moves: Vec<Value>,
    verbs: BTreeMap<String, u64>,
    checkins: Vec<Value>,
    subagents: Value,
    first_ts: Option<String>,
    last_ts: Option<String>,
}

fn timestamp_epoch(value: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis() as f64 / 1000.0)
}

fn span_hours(start: Option<&str>, end: Option<&str>) -> f64 {
    match (
        start.and_then(timestamp_epoch),
        end.and_then(timestamp_epoch),
    ) {
        (Some(start), Some(end)) => ((end - start) / 3600.0).max(0.0),
        _ => 0.0,
    }
}

fn normalized_lead(text: &str) -> String {
    let lead: String = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    let mut out = String::with_capacity(lead.len());
    let mut in_digits = false;
    for ch in lead.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('N');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(ch);
        }
    }
    out
}

fn content_text(content: &Value) -> Cow<'_, str> {
    match content {
        Value::String(s) => Cow::Borrowed(s),
        Value::Array(parts) => Cow::Owned(
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => Cow::Borrowed(""),
    }
}

fn row_text(row: &Value) -> String {
    let Some(content) = row.pointer("/message/content") else {
        return String::new();
    };
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn add(map: &mut BTreeMap<String, u64>, key: &str) {
    *map.entry(key.to_string()).or_default() += 1;
}

fn fno_verb(target: &str) -> Option<(String, Option<String>)> {
    static VERB: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let regex = VERB.get_or_init(|| {
        regex::Regex::new(r"\bfno(?:-agents|-py)? ([a-z-]+)(?: ([a-z-]+))?")
            .expect("fno verb matcher is valid")
    });
    let captures = regex.captures(target)?;
    Some((
        captures.get(1)?.as_str().to_string(),
        captures.get(2).map(|m| m.as_str().to_string()),
    ))
}

fn events_for_session(home: &AgentsHome, session: &str) -> Result<Vec<Value>, String> {
    let report = crate::king_history::scan_scopes(&[home.events_jsonl()], None)?;
    let events = report
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| "reign check-in scan returned no event list".to_string())?;
    Ok(events
        .iter()
        .filter(|row| {
            row.get("type").and_then(Value::as_str) == Some(crate::king_history::REIGN_CHECKIN)
                && row.pointer("/data/session_id").and_then(Value::as_str) == Some(session)
        })
        .cloned()
        .collect())
}

fn bus_index() -> BusIndex {
    let home = AgentsHome::from_env();
    let fno_dir = home
        .events_jsonl()
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new(".fno"));
    let path = crate::intel::bus_log_path(fno_dir);
    BusIndex::load(&path)
}

fn fold_transcript(
    session: &str,
    transcript: &Path,
    since: Option<String>,
    until: Option<String>,
    compaction_ceiling: i64,
    bus: &BusIndex,
) -> Result<Fold, String> {
    let file = std::fs::File::open(transcript)
        .map_err(|e| format!("{}: unreadable transcript: {e}", transcript.display()))?;
    let mut fold = Fold {
        session: session.to_string(),
        transcript: transcript.to_path_buf(),
        since,
        until,
        compaction_ceiling,
        windows: vec![Window::new(1, None)],
        entries: Vec::new(),
        typed_turns: Vec::new(),
        repeated_refusals: BTreeMap::new(),
        merges: Vec::new(),
        spawns: Vec::new(),
        crown_moves: Vec::new(),
        verbs: BTreeMap::new(),
        checkins: Vec::new(),
        subagents: json!({"files": 0, "tool_calls": 0}),
        first_ts: None,
        last_ts: None,
    };
    let since_epoch = fold.since.as_deref().and_then(timestamp_epoch);
    let until_epoch = fold.until.as_deref().and_then(timestamp_epoch);
    let mut first_after_boundary = false;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("{}: read error: {e}", transcript.display()))?;
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = row.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let ts_epoch = timestamp_epoch(ts);
        if until_epoch.is_some_and(|until| ts_epoch.is_some_and(|at| at > until)) {
            break;
        }
        if let Some(boundary_ts) = crate::compaction::boundary_ts(&row) {
            let window_no = fold.windows.len() + 1;
            fold.windows
                .push(Window::new(window_no, Some(boundary_ts.to_string())));
            first_after_boundary = true;
        }
        if !ts.is_empty() {
            fold.first_ts.get_or_insert_with(|| ts.to_string());
            fold.last_ts = Some(ts.to_string());
        }
        if since_epoch.is_some_and(|since| ts_epoch.is_some_and(|at| at < since)) {
            continue;
        }
        let window_idx = fold.windows.len() - 1;
        {
            let window = &mut fold.windows[window_idx];
            if window.start.is_none() && !ts.is_empty() {
                window.start = Some(ts.to_string());
            }
            if !ts.is_empty() {
                window.end = Some(ts.to_string());
            }
        }
        if first_after_boundary
            && (row.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
                || row.get("subtype").and_then(Value::as_str) == Some("isCompactSummary"))
        {
            fold.windows[window_idx].summary = Some(row_text(&row));
            first_after_boundary = false;
        }
        fold.windows[window_idx].activity_fold.row(&row);
        let prior_entries = fold.entries.len();
        crate::reign_hygiene::claude_row_entries(&row, &mut fold.entries);
        for entry in &fold.entries[prior_entries..] {
            if entry.kind != "tool_use" {
                continue;
            }
            fold.windows[window_idx].tool_calls += 1;
            add(
                &mut fold.windows[window_idx].tools,
                entry.tool.as_deref().unwrap_or(""),
            );
            if entry.tool.as_deref() == Some("Bash") {
                if let Some((verb, subverb)) = fno_verb(&entry.target) {
                    let key = subverb
                        .map(|s| format!("{verb} {s}"))
                        .unwrap_or(verb.clone());
                    add(&mut fold.verbs, &key);
                    if entry.target.contains("fno do pr merge") {
                        fold.merges.push(json!({"ts": ts, "command": entry.target}));
                    }
                    if entry.target.contains("agents spawn") && entry.target.contains("--name") {
                        fold.spawns.push(json!({"ts": ts, "command": entry.target}));
                    }
                    if [
                        "org checkin",
                        "org term",
                        "org done",
                        "--crown",
                        "--promote",
                        "--succeed",
                    ]
                    .iter()
                    .any(|needle| entry.target.contains(needle))
                    {
                        fold.crown_moves
                            .push(json!({"ts": ts, "command": entry.target}));
                    }
                }
            } else if entry.tool.as_deref() == Some("Agent") {
                fold.spawns.push(json!({"ts": ts, "target": entry.target}));
            }
        }
        if row.get("type").and_then(Value::as_str) == Some("user") {
            let text = row_text(&row);
            if !text.is_empty() {
                let provenance = crate::provenance::classify_turn(&row, bus, session);
                if let Some(machine) = crate::wake_meter::wake_class(provenance.clone()) {
                    let class = provenance.label();
                    add(&mut fold.windows[window_idx].wakes, class);
                    if !machine {
                        fold.typed_turns.push(TypedTurn {
                            ts: ts.to_string(),
                            class: class.to_string(),
                            text: text.chars().take(200).collect(),
                        });
                    }
                }
            }
        }
        if let Some(content) = row.pointer("/message/content").and_then(Value::as_array) {
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let text = content_text(block.get("content").unwrap_or(&Value::Null));
                let buckets = crate::refusal_rate::buckets_of(&text).collect::<Vec<_>>();
                for bucket in &buckets {
                    add(&mut fold.windows[window_idx].buckets, bucket);
                }
                if !buckets.is_empty() {
                    let key = normalized_lead(&text);
                    let refusal = fold
                        .repeated_refusals
                        .entry(key.clone())
                        .or_insert_with(|| RepeatedRefusal {
                            lead: key,
                            count: 0,
                            windows: BTreeSet::new(),
                        });
                    refusal.count += 1;
                    refusal.windows.insert(fold.windows[window_idx].n);
                }
            }
        }
    }
    for window in &mut fold.windows {
        window.activity = std::mem::take(&mut window.activity_fold).finish();
    }
    fold.subagents = fold_subagents(transcript, session)?;
    Ok(fold)
}

fn fold_subagents(transcript: &Path, session: &str) -> Result<Value, String> {
    let parent = transcript.parent().ok_or_else(|| {
        format!(
            "{}: transcript has no parent directory",
            transcript.display()
        )
    })?;
    let dir = parent.join(session).join("subagents");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"files": 0, "tool_calls": 0, "tokens":{"output":0,"cache_read":0}}));
        }
        Err(error) => {
            return Err(format!(
                "{}: subagent directory unreadable: {error}",
                dir.display()
            ))
        }
    };
    let mut files = 0u64;
    let mut tool_calls = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read = 0u64;
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("{}: subagent entry unreadable: {error}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file = std::fs::File::open(&path).map_err(|error| {
            format!(
                "{}: unreadable subagent transcript: {error}",
                path.display()
            )
        })?;
        files += 1;
        let mut activity = ActivityFold::default();
        for (line_no, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|error| {
                format!("{}:{}: read error: {error}", path.display(), line_no + 1)
            })?;
            let row = serde_json::from_str::<Value>(&line).map_err(|error| {
                format!(
                    "{}:{}: malformed transcript JSON: {error}",
                    path.display(),
                    line_no + 1
                )
            })?;
            activity.row(&row);
            let mut parsed = Vec::new();
            crate::reign_hygiene::claude_row_entries(&row, &mut parsed);
            tool_calls += parsed
                .iter()
                .filter(|entry| entry.kind == "tool_use")
                .count() as u64;
        }
        let activity = activity.finish();
        output_tokens += activity.tokens.output;
        cache_read += activity.tokens.cache_read;
    }
    Ok(
        json!({"files": files, "tool_calls": tool_calls, "tokens":{"output":output_tokens,"cache_read":cache_read}}),
    )
}

fn totals(windows: &[Window]) -> Value {
    let mut tools = 0;
    let mut errors = 0;
    let mut typed = 0;
    let mut machine = 0;
    let mut tool_names = BTreeMap::<String, u64>::new();
    let mut buckets = BTreeMap::<String, u64>::new();
    let mut wakes = BTreeMap::<String, u64>::new();
    for window in windows {
        tools += window.tool_calls;
        errors += window.activity.tool_errors.values().sum::<u64>();
        for (name, count) in &window.tools {
            *tool_names.entry(name.clone()).or_default() += count;
        }
        for (name, count) in &window.buckets {
            *buckets.entry(name.clone()).or_default() += count;
        }
        for (name, count) in &window.wakes {
            *wakes.entry(name.clone()).or_default() += count;
            if name == "operator" || name == "unknown" {
                typed += count;
            } else {
                machine += count;
            }
        }
    }
    json!({"tool_calls": tools, "errors": errors, "typed": typed, "machine": machine, "tools": tool_names, "buckets": buckets, "wakes_by_class": wakes})
}

fn bound_counts<'a>(windows: impl IntoIterator<Item = &'a Window>) -> Value {
    let mut tools = 0u64;
    let mut errors = 0u64;
    for window in windows {
        tools += window.tool_calls;
        errors += window.activity.tool_errors.values().sum::<u64>();
    }
    json!({ "errors": errors, "tool_calls": tools })
}

fn fold_json(fold: &Fold) -> Value {
    let hygiene = crate::reign_hygiene::run_checks(&fold.entries, None)
        .into_iter()
        .map(|result| {
            json!({
                "check": result.check,
                "applicable": result.applicable,
                "status": result.status,
                "detail": result.detail,
                "index": result.index,
            })
        })
        .collect::<Vec<_>>();
    let checkins = &fold.checkins;
    let scopes = checkins
        .iter()
        .filter_map(|row| row.pointer("/data/scope").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let last = checkins.last().unwrap_or(&Value::Null);
    let repeated = fold
        .repeated_refusals
        .values()
        .map(|r| json!({"lead": r.lead, "count": r.count, "windows": r.windows}))
        .collect::<Vec<_>>();
    let typed = fold
        .typed_turns
        .iter()
        .map(|turn| json!({"ts": turn.ts, "class": turn.class, "text": turn.text}))
        .collect::<Vec<_>>();
    let windows = fold.windows.iter().map(Window::json).collect::<Vec<_>>();
    let all_start = fold
        .windows
        .iter()
        .find_map(|window| window.start.as_deref());
    let all_end = fold
        .windows
        .iter()
        .rev()
        .find_map(|window| window.end.as_deref());
    let inside = fold
        .windows
        .iter()
        .filter(|window| window.n <= fold.compaction_ceiling as usize + 1)
        .map(Window::json)
        .collect::<Vec<_>>();
    let after = fold
        .windows
        .iter()
        .filter(|window| window.n > fold.compaction_ceiling as usize + 1)
        .map(Window::json)
        .collect::<Vec<_>>();
    let inside_windows = fold
        .windows
        .iter()
        .filter(|window| window.n <= fold.compaction_ceiling as usize + 1)
        .collect::<Vec<_>>();
    let after_windows = fold
        .windows
        .iter()
        .filter(|window| window.n > fold.compaction_ceiling as usize + 1)
        .collect::<Vec<_>>();
    json!({
        "session": fold.session,
        "transcript": fold.transcript,
        "since": fold.since,
        "until": fold.until,
        "span_h": span_hours(all_start, all_end),
        "compaction_ceiling": fold.compaction_ceiling,
        "windows": windows,
        "totals": totals(&fold.windows),
        "inside_bound": inside,
        "after_bound": after,
        "bound_error_rates": {
            "inside": bound_counts(inside_windows.iter().copied()),
            "after": bound_counts(after_windows.iter().copied()),
        },
        "typed_turns": typed,
        "repeated_refusals": repeated,
        "hygiene": hygiene,
        "subagents": fold.subagents,
        "reign_checkin": {
            "count": checkins.len(),
            "first_ts": checkins.first().and_then(|row| row.get("ts")),
            "last_ts": last.get("ts"),
            "scopes": scopes,
            "refusal_rate": last.pointer("/data/refusal_rate"),
            "wake_ratio": last.pointer("/data/wake_ratio"),
            "readers_failed": last.pointer("/data/readers_failed"),
        },
        "merges": fold.merges,
        "spawns": fold.spawns,
        "crown_moves": fold.crown_moves,
        "verbs": fold.verbs,
    })
}

fn parse_date(value: &str) -> Result<String, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(dt.to_rfc3339());
    }
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("invalid date: {value}"))?;
    Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc().to_rfc3339())
}

#[derive(Default)]
struct Args {
    session: Option<String>,
    crown: Option<String>,
    since: Option<String>,
    until: Option<String>,
    json: bool,
    write: Option<Option<PathBuf>>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--session" | "--crown" | "--since" | "--until" => {
                i += 1;
                let value = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                match flag {
                    "--session" => parsed.session = Some(value),
                    "--crown" => parsed.crown = Some(value),
                    "--since" => parsed.since = Some(parse_date(&value)?),
                    _ => parsed.until = Some(parse_date(&value)?),
                }
            }
            "--json" => parsed.json = true,
            "--write" => {
                let path = args
                    .get(i + 1)
                    .filter(|next| !next.starts_with("--"))
                    .map(|next| PathBuf::from(next));
                if path.is_some() {
                    i += 1;
                }
                parsed.write = Some(path);
            }
            other => return Err(format!("unknown flag {other}")),
        }
        i += 1;
    }
    if parsed.session.is_some() == parsed.crown.is_some() {
        return Err("exactly one of --session or --crown is required".to_string());
    }
    if parsed.write.is_some() && parsed.session.is_none() {
        return Err("--write requires --session".to_string());
    }
    Ok(parsed)
}

pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "fno-agents intel --windows --session <id>|--crown <scope> [--since DATE] [--until DATE] [--json] [--write [dir]]"
        );
        return 0;
    }
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("fno-agents intel --windows: {error}");
            return 2;
        }
    };
    let home = AgentsHome::from_env();
    let sessions = if let Some(session) = parsed.session.as_deref() {
        vec![session.to_string()]
    } else {
        let report =
            match crate::king_history::scan_scopes(&[home.events_jsonl()], parsed.crown.as_deref())
            {
                Ok(report) => report,
                Err(error) => {
                    eprintln!("fno-agents intel --windows: {error}");
                    return 3;
                }
            };
        report
            .get("events")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                row.pointer("/data/session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .fold(Vec::<String>::new(), |mut sessions, session| {
                if !sessions.contains(&session) {
                    sessions.push(session);
                }
                sessions
            })
    };
    let mut reports = Vec::new();
    let bus = bus_index();
    for session in sessions {
        let Some(transcript) =
            crate::king_history::hygiene_transcript_for_holder("claude", &session)
        else {
            if crate::king_history::hygiene_transcript_for_holder("codex", &session).is_some() {
                eprintln!(
                    "fno-agents intel --windows: the windows fold reads claude transcripts; codex rollout windows are not read yet"
                );
                return 2;
            }
            eprintln!(
                "fno-agents intel --windows: transcript not found for claude session {session}"
            );
            return 3;
        };
        let Some(cwd) = crate::provenance::first_cwd_row(&transcript).map(PathBuf::from) else {
            eprintln!("fno-agents intel --windows: transcript has no first cwd row");
            return 3;
        };
        let ceiling = match crate::king_verdict_inputs::compaction_ceiling(&cwd) {
            Ok(ceiling) => ceiling,
            Err(error) => {
                eprintln!("fno-agents intel --windows: {error}");
                return 2;
            }
        };
        let checkins = match events_for_session(&home, &session) {
            Ok(checkins) => checkins,
            Err(error) => {
                eprintln!("fno-agents intel --windows: {error}");
                return 3;
            }
        };
        let write_dir = match parsed.write.as_ref() {
            None => None,
            Some(Some(dir)) => Some(dir.clone()),
            Some(None) => match default_eval_dir_for(&session, &checkins, &cwd) {
                Ok(dir) => Some(dir),
                Err(error) => {
                    eprintln!("fno-agents intel --windows: {error}");
                    return 2;
                }
            },
        };
        let mut fold = match fold_transcript(
            &session,
            &transcript,
            parsed.since.clone(),
            parsed.until.clone(),
            ceiling,
            &bus,
        ) {
            Ok(fold) => fold,
            Err(error) => {
                if let Some(dir) = &write_dir {
                    if let Err(write_error) =
                        write_fold_failed_index(&session, &transcript, dir, &error)
                    {
                        eprintln!("fno-agents intel --windows: {write_error}");
                    }
                }
                eprintln!("fno-agents intel --windows: {error}");
                return 3;
            }
        };
        fold.checkins = checkins;
        let value = fold_json(&fold);
        if let Some(dir) = &write_dir {
            if let Err(error) = write_eval(&fold, &value, &dir) {
                eprintln!("fno-agents intel --windows: {error}");
                return 1;
            }
        }
        reports.push(value);
    }
    if parsed.json {
        if reports.len() == 1 {
            println!("{}", reports[0]);
        } else {
            println!("{}", json!({"sessions": reports}));
        }
    } else {
        for report in reports {
            print_text(&report);
        }
    }
    0
}

fn print_text(report: &Value) {
    println!("session {}", report["session"].as_str().unwrap_or(""));
    println!("# start end hours tools errors typed machine");
    for window in report["windows"].as_array().into_iter().flatten() {
        println!(
            "{} {} {} {:.1} {} {} {} {}",
            window["n"],
            window["start"],
            window["end"],
            window["hours"].as_f64().unwrap_or(0.0),
            window["tool_calls"],
            window["errors"],
            window["wakes"]["typed"],
            window["wakes"]["machine"]
        );
    }
    println!("totals {}", report["totals"]);
    println!(
        "bound ceiling={} inside={} after={}",
        report["compaction_ceiling"],
        report["inside_bound"].as_array().map(Vec::len).unwrap_or(0),
        report["after_bound"].as_array().map(Vec::len).unwrap_or(0)
    );
}

fn default_eval_dir_for(session: &str, checkins: &[Value], cwd: &Path) -> Result<PathBuf, String> {
    let scope = checkins
        .first()
        .and_then(|row| row.pointer("/data/scope"))
        .and_then(Value::as_str)
        .unwrap_or("session");
    let first_scope = scope.split(',').next().unwrap_or(scope).trim();
    let tag = first_scope
        .rsplit('-')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(first_scope);
    let plans = crate::plans_path::plans_content_dir(cwd).ok_or_else(|| {
        format!(
            "plans directory could not be resolved from {}",
            cwd.display()
        )
    })?;
    Ok(plans
        .join("..")
        .join("evals")
        .join("kings")
        .join(format!("king-{tag}-{}", &session[..session.len().min(8)])))
}

fn write_if_missing(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        println!(
            "kept {}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file")
        );
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file
            .write_all(contents.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            println!(
                "kept {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file")
            );
            Ok(())
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn write_fold_failed_index(
    session: &str,
    transcript: &Path,
    dir: &Path,
    reason: &str,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let base = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("eval");
    let path = dir.with_file_name(format!("{base}.md"));
    let body = format!(
        "---\nsession: {session}\nhandle: {}\ntranscript: {}\nstatus: fold-failed\n---\n\n# Reign eval\n\nThe mechanical fold failed: {}\n",
        &session[..session.len().min(8)], transcript.display(), reason.replace('\n', " ")
    );
    write_if_missing(&path, &body)
}

fn write_eval(fold: &Fold, value: &Value, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let base = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("eval");
    let index = dir.with_file_name(format!("{base}.md"));
    let windows = value["windows"].as_array().cloned().unwrap_or_default();
    let crowned = fold
        .checkins
        .first()
        .and_then(|row| row.get("ts"))
        .and_then(Value::as_str)
        .or(fold.first_ts.as_deref())
        .unwrap_or("");
    let ended = fold
        .checkins
        .last()
        .and_then(|row| row.get("ts"))
        .and_then(Value::as_str)
        .or(fold.until.as_deref())
        .unwrap_or("");
    let mut index_md = format!(
        "---\nsession: {}\nhandle: {}\nscope: {}\ncrowned: {}\nended: {}\ntranscript: {}\nwindows: {}\ntool_calls: {}\ngraded_by: pending\nmethod: fno-agents intel --session {} --windows --write\n---\n\n# Reign eval\n\nThe one number candidate: {} errors of {} tool calls inside the compaction bound, then {} errors of {} after it.\n\n- [Part 1: metrics]({base}/part1-metrics.md)\n- [Part 2: timeline]({base}/part2-timeline.md)\n- [Part 3: failures]({base}/part3-failures.md)\n- [Part 4: reforms]({base}/part4-reforms.md)\n",
        fold.session,
        &fold.session[..fold.session.len().min(8)],
        value["reign_checkin"]["scopes"],
        crowned,
        ended,
        fold.transcript.display(),
        windows.len(),
        value["totals"]["tool_calls"],
        fold.session,
        value["bound_error_rates"]["inside"]["errors"],
        value["bound_error_rates"]["inside"]["tool_calls"],
        value["bound_error_rates"]["after"]["errors"],
        value["bound_error_rates"]["after"]["tool_calls"]
    );
    if index_md.is_empty() {
        index_md.push('\n');
    }
    write_if_missing(&index, &index_md)?;
    let source = "Source: `data/fold.json`.\n";
    let mut metrics = format!(
        "# Part 1: metrics\n\n{source}\n| # | Start (UTC) | Hours | Tools | Errors | Error % | Tools per hour | User lines | cache_read | Output |\n|---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n"
    );
    for row in &windows {
        let tools = row["tool_calls"].as_u64().unwrap_or(0);
        let errors = row["errors"].as_u64().unwrap_or(0);
        let hours = row["hours"].as_f64().unwrap_or(0.0);
        let rate = if tools == 0 {
            0.0
        } else {
            errors as f64 / tools as f64 * 100.0
        };
        let per_hour = if hours == 0.0 {
            0.0
        } else {
            tools as f64 / hours
        };
        metrics.push_str(&format!(
            "| {} | {} | {:.1} | {} | {} | {:.1} | {:.1} | {} | {} | {} |\n",
            row["n"],
            row["start"],
            hours,
            tools,
            errors,
            rate,
            per_hour,
            row["wakes"]["typed"],
            row["tokens"]["cache_read"],
            row["tokens"]["output"]
        ));
    }
    let transcript_bytes = std::fs::metadata(&fold.transcript)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    metrics.push_str(&format!(
        "\n## Size\n\nTranscript bytes: {transcript_bytes}. Windows: {}. Tool calls: {}.\n",
        windows.len(),
        value["totals"]["tool_calls"]
    ));
    metrics.push_str(&format!(
        "\n## Inside and after the compaction bound\n\nInside: {} errors over {} tool calls. After: {} errors over {} tool calls.\n",
        value["bound_error_rates"]["inside"]["errors"],
        value["bound_error_rates"]["inside"]["tool_calls"],
        value["bound_error_rates"]["after"]["errors"],
        value["bound_error_rates"]["after"]["tool_calls"]
    ));
    for (title, data) in [
        ("Error buckets", &value["totals"]["buckets"]),
        ("Tools", &value["totals"]["tools"]),
        ("Top fno verbs", &value["verbs"]),
        ("Wake economy by class", &value["totals"]["wakes_by_class"]),
        ("Merges", &value["merges"]),
        ("Spawns and subagents", &value["spawns"]),
        ("Check-in journal", &value["reign_checkin"]),
        ("Hygiene", &value["hygiene"]),
    ] {
        metrics.push_str(&format!("\n## {title}\n\n```json\n{data}\n```\n"));
    }
    metrics.push_str(&format!("\n## Fold\n\n```json\n{}\n```\n", value));
    write_if_missing(&dir.join("part1-metrics.md"), &metrics)?;
    let mut timeline = format!("# Part 2: timeline\n\n{source}\n");
    let events = timeline_events(value);
    for row in &windows {
        timeline.push_str(&format!("## Window {}\n\nStart: {}  \nEnd: {}  \nHours: {:.1}  \nTool calls: {}  \nErrors: {}\n\n", row["n"], row["start"], row["end"], row["hours"].as_f64().unwrap_or(0.0), row["tool_calls"], row["errors"]));
        timeline.push_str("### Timeline events\n\n| Time | Kind | Detail |\n|---|---|---|\n");
        for event in events.iter().filter(|event| {
            let ts = event["ts"].as_str().unwrap_or("");
            ts >= row["start"].as_str().unwrap_or("") && ts <= row["end"].as_str().unwrap_or("")
        }) {
            timeline.push_str(&format!(
                "| {} | {} | {} |\n",
                event["ts"], event["kind"], event["detail"]
            ));
        }
        timeline.push('\n');
    }
    write_if_missing(&dir.join("part2-timeline.md"), &timeline)?;
    let failures = format!(
        "---\nstatus: pending\n---\n\n# Part 3: failures\n\n{source}\n## Repeated refusals\n\n| Lead | Count | Windows |\n|---|---:|---|\n{}\n\n## Hygiene\n\n```json\n{}\n```\n",
        value["repeated_refusals"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| format!("| {} | {} | {} |", r["lead"], r["count"], r["windows"]))
            .collect::<Vec<_>>()
            .join("\n"),
        value["hygiene"]
    );
    write_if_missing(&dir.join("part3-failures.md"), &failures)?;
    write_if_missing(
        &dir.join("part4-reforms.md"),
        &format!(
            "---\nstatus: pending\n---\n\n# Part 4: reforms\n\nEach reform names its specimen and its home, a node filed with `fno backlog idea --parent <epic>`.\n"
        ),
    )?;
    write_if_missing(&dir.join("data/fold.json"), &format!("{}\n", value))?;
    for window in &fold.windows {
        if let Some(summary) = &window.summary {
            write_if_missing(
                &dir.join(format!("data/window-{:02}-summary.md", window.n)),
                summary,
            )?;
        }
    }
    Ok(())
}

fn timeline_events(value: &Value) -> Vec<Value> {
    let mut events = Vec::new();
    for turn in value["typed_turns"].as_array().into_iter().flatten() {
        events.push(json!({"ts":turn["ts"],"kind":"typed_turn","detail":turn["text"]}));
    }
    for (field, kind) in [
        ("merges", "merge"),
        ("spawns", "spawn"),
        ("crown_moves", "crown_move"),
    ] {
        for event in value[field].as_array().into_iter().flatten() {
            let detail = event
                .get("command")
                .or_else(|| event.get("target"))
                .cloned()
                .unwrap_or_else(|| event.clone());
            events.push(json!({"ts":event["ts"],"kind":kind,"detail":detail}));
        }
    }
    events.sort_by(|left, right| left["ts"].as_str().cmp(&right["ts"].as_str()));
    events
}

#[derive(Default)]
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    let interval = Duration::from_secs(REIGN_EVAL_INTERVAL_S);
    {
        let mut last = arm
            .last_tick
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last.is_some_and(|tick| tick.elapsed() < interval)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let gate = Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _sweep = crate::daemon::SweepGate(gate);
        let outcome = run_arm(&home);
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        crate::tick_ledger::emit_tick(
            &journal,
            "reign_eval",
            crate::tick_ledger::SCHED_DAEMON,
            outcome.0,
            outcome.1.as_deref(),
            Some(&outcome.2),
            REIGN_EVAL_INTERVAL_S,
        );
    });
}

fn run_arm(home: &AgentsHome) -> (u64, Option<String>, String) {
    let now = chrono::Utc::now();
    let rows = match crate::king_history::scan_scopes(&[home.events_jsonl()], None) {
        Ok(report) => report
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(error) => return (0, Some("checkin_read_failed".into()), error),
    };
    let checkins = rows
        .into_iter()
        .filter(|row| {
            row.get("type").and_then(Value::as_str) == Some(crate::king_history::REIGN_CHECKIN)
        })
        .collect::<Vec<_>>();
    let live = match crate::state::load_registry(&home.registry_json()) {
        Ok(registry) => registry
            .entries
            .into_iter()
            .filter(|entry| {
                entry.crown_scope.is_some()
                    && !matches!(
                        entry.status,
                        crate::AgentStatus::Exited | crate::AgentStatus::PermanentDead
                    )
            })
            .filter_map(|entry| entry.harness_session_id)
            .collect::<BTreeSet<_>>(),
        Err(error) => return (0, Some("registry_read_failed".into()), error.to_string()),
    };
    let written = session_has_eval;
    let due = due_sessions(&checkins, &live, written, now);
    let Some(session) = due.first() else {
        return (0, Some("not_due".into()), "none due".into());
    };
    let Some(transcript) = crate::king_history::hygiene_transcript_for_holder("claude", session)
    else {
        return (0, Some("no_eval_root".into()), "eval root not found".into());
    };
    let Some(cwd) = crate::provenance::first_cwd_row(&transcript).map(PathBuf::from) else {
        return (
            0,
            Some("transcript_cwd_missing".into()),
            "transcript has no first cwd row".into(),
        );
    };
    let session_checkins = checkins
        .iter()
        .filter(|row| row.pointer("/data/session_id").and_then(Value::as_str) == Some(session))
        .cloned()
        .collect::<Vec<_>>();
    let root = match default_eval_dir_for(session, &session_checkins, &cwd) {
        Ok(root) => root,
        Err(error) => return (0, Some("plans_directory_missing".into()), error),
    };
    let result = std::process::Command::new(
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fno-agents")),
    )
    .args([
        "intel",
        "--session",
        session,
        "--windows",
        "--write",
        root.to_string_lossy().as_ref(),
    ])
    .stdin(std::process::Stdio::null())
    .output();
    match result {
        Ok(output) if output.status.success() => (1, None, format!("wrote {}", root.display())),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr
                .lines()
                .next_back()
                .unwrap_or("fold failed")
                .to_string();
            if let Err(error) = write_fold_failed_index(session, &transcript, &root, &reason) {
                return (0, Some("failed_index_write".into()), error);
            }
            (0, Some("child_failed".into()), reason)
        }
        Err(error) => {
            let reason = error.to_string();
            if let Err(write_error) = write_fold_failed_index(session, &transcript, &root, &reason)
            {
                return (0, Some("failed_index_write".into()), write_error);
            }
            (0, Some("spawn_failed".into()), reason)
        }
    }
}

fn session_has_eval(session: &str) -> bool {
    let Some(transcript) = crate::king_history::hygiene_transcript_for_holder("claude", session)
    else {
        return true;
    };
    let Some(cwd) = crate::provenance::first_cwd_row(&transcript).map(PathBuf::from) else {
        return true;
    };
    let Some(plans) = crate::plans_path::plans_content_dir(&cwd) else {
        return true;
    };
    let evals = plans.join("..").join("evals").join("kings");
    let sid8 = &session[..session.len().min(8)];
    let entries = match std::fs::read_dir(evals) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        name.contains(sid8)
    })
}

pub(crate) fn due_sessions(
    checkin_rows: &[Value],
    live_sessions: &BTreeSet<String>,
    has_eval: impl Fn(&str) -> bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let mut last = HashMap::<String, chrono::DateTime<chrono::Utc>>::new();
    for row in checkin_rows {
        let Some(session) = row.pointer("/data/session_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(ts) = row
            .get("ts")
            .and_then(Value::as_str)
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|ts| ts.with_timezone(&chrono::Utc))
        else {
            continue;
        };
        last.entry(session.to_string())
            .and_modify(|seen| {
                if ts > *seen {
                    *seen = ts;
                }
            })
            .or_insert(ts);
    }
    let cutoff = now - chrono::Duration::minutes(30);
    let mut due = last
        .into_iter()
        .filter_map(|(session, ts)| {
            (!live_sessions.contains(&session) && ts <= cutoff && !has_eval(&session))
                .then_some((ts, session))
        })
        .collect::<Vec<_>>();
    due.sort_by(|a, b| b.0.cmp(&a.0));
    due.into_iter().map(|(_, session)| session).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn parse_time(value: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn transcript_row(ts: &str, text: &str) -> Value {
        json!({
            "type":"user", "timestamp":ts,
            "message":{"role":"user","content":[{"type":"text","text":text}]}
        })
    }

    #[test]
    fn windows_split_at_boundaries_with_buckets_wakes_and_until() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let rows = vec![
            json!({"type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"fno backlog get x"}}]}}),
            json!({"type":"user","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Usage: fno backlog get"}]}}),
            json!({"type":"assistant","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"tool_result","tool_use_id":"t1","content":"style-exception: missing condition"}]}}),
            json!({"subtype":"compact_boundary","timestamp":"2026-01-01T01:00:00Z"}),
            json!({"type":"assistant","timestamp":"2026-01-01T01:00:01Z","isCompactSummary":true,"message":{"role":"assistant","content":"compaction summary"}}),
            transcript_row("2026-01-01T01:10:00Z", "typed request"),
            json!({"type":"assistant","timestamp":"2026-01-01T01:15:00Z","message":{"role":"assistant","content":"Base directory for this skill: /tmp/skill"}}),
            json!({"type":"user","timestamp":"2026-01-01T01:20:00Z","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"reign check-in: inspect this territory"}]}}),
            transcript_row(
                "2026-01-01T01:25:00Z",
                "<task-notification><task-id>t</task-id><subagent_tokens>10</subagent_tokens></task-notification>",
            ),
            json!({"subtype":"compact_boundary","timestamp":"2026-01-01T02:00:00Z"}),
            json!({"type":"assistant","timestamp":"2026-01-01T02:31:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"late","name":"Read","input":{"file_path":"late.rs"}}]}}),
        ];
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
        let fold = fold_transcript(
            "test-session",
            file.path(),
            None,
            Some("2026-01-01T02:30:00Z".into()),
            3,
            &BusIndex::empty(),
        )
        .unwrap();
        assert_eq!(fold.windows.len(), 3);
        assert_eq!(fold.windows[0].tool_calls, 1);
        assert_eq!(fold.windows[0].activity.tool_errors.get("other"), Some(&1));
        assert_eq!(fold.windows[0].buckets.get("usage_error"), Some(&1));
        assert_eq!(fold.windows[0].buckets.get("style_lint"), Some(&1));
        assert_eq!(fold.windows[1].wakes.get("unknown"), Some(&1));
        assert_eq!(fold.windows[1].wakes.get("harness_loop_wakeup"), Some(&1));
        assert_eq!(
            fold.windows[1].wakes.get("relay_task_notification"),
            Some(&1)
        );
        assert_eq!(fold.windows[2].tool_calls, 0);
        assert!(fold
            .entries
            .iter()
            .any(|entry| entry.text.contains("Base directory for this skill")));
    }

    #[test]
    fn write_fills_missing_parts_and_keeps_edited_ones() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("king-fno-12345678");
        std::fs::create_dir_all(&dir).unwrap();
        let part1 = dir.join("part1-metrics.md");
        std::fs::write(&part1, "founder edit\n").unwrap();
        let fold = Fold {
            session: "12345678-0000-4000-8000-000000000000".into(),
            transcript: root.path().join("transcript.jsonl"),
            since: None,
            until: None,
            compaction_ceiling: 3,
            windows: vec![],
            entries: vec![],
            typed_turns: vec![],
            repeated_refusals: BTreeMap::new(),
            merges: vec![],
            spawns: vec![],
            crown_moves: vec![],
            verbs: BTreeMap::new(),
            checkins: vec![],
            subagents: json!({"files":0,"tool_calls":0}),
            first_ts: None,
            last_ts: None,
        };
        let value = json!({"windows":[],"totals":{"tool_calls":0},"bound_error_rates":{"inside":{"errors":0,"tool_calls":0},"after":{"errors":0,"tool_calls":0}},"reign_checkin":{"scopes":[]},"repeated_refusals":[],"hygiene":[]});
        write_eval(&fold, &value, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(part1).unwrap(), "founder edit\n");
        assert!(dir.join("part2-timeline.md").exists());
        assert!(dir.join("part3-failures.md").exists());
        assert!(dir.join("part4-reforms.md").exists());
        assert!(dir.join("data/fold.json").exists());
        assert!(root.path().join("king-fno-12345678.md").exists());
    }

    #[test]
    fn due_sessions_skip_live_written_and_fresh_reigns() {
        let rows = vec![
            json!({"ts":"2026-01-01T10:00:00Z","data":{"session_id":"live"}}),
            json!({"ts":"2026-01-01T10:00:00Z","data":{"session_id":"written"}}),
            json!({"ts":"2026-01-01T10:00:00Z","data":{"session_id":"due"}}),
            json!({"ts":"2026-01-01T10:30:00Z","data":{"session_id":"fresh"}}),
        ];
        let live = BTreeSet::from(["live".to_string()]);
        let due = due_sessions(
            &rows,
            &live,
            |session| session == "written",
            parse_time("2026-01-01T10:40:00Z"),
        );
        assert_eq!(due, vec!["due".to_string()]);
    }
}
