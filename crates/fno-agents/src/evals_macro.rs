//! `evals-macro`: the failure-pattern leaderboard over the events journals,
//! the native port of the Python `fno.evals.macro` fold.
//!
//! Same split as `king-history`: Python owns identity and paths - the
//! `fno doctor evals macro` shell resolves `paths.event_journals()` and
//! forwards the flags HERE - and the native side owns the fold, so the
//! file-budget Python-tree ratchet holds. Daemon-free read, not a routable
//! `fno agents` verb.
//!
//! What the fold does: one row per `(type, label)` pass over the ordered
//! journal rows. A pattern is a `(type, label)` pair where the label is the
//! row's first present `reason`/`outcome`/`verdict`/`termination_reason` and
//! the pair is neither a known-healthy label nor a noise type. For every
//! pattern the leaderboard carries count, distinct sessions and nodes,
//! first/last seen, and up to three suspects: the patterns that fired in the
//! `--window` events before it inside the same session, ranked by
//! `lift = P(suspect in the pre-window | pattern) / P(suspect in any row)`.
//! `--topic TYPE:LABEL` drills into one pattern and prints recent event
//! chains. No clustering: the journals' own labels are the topics.
//!
//! Fidelity notes against the retired Python module: the strict row-timestamp
//! parser accepts only `YYYY-MM-DDTHH:MM:SS[.f{1,6}](Z|+00:00)`; rows with
//! other shapes drop out of a `--since` window and count under
//! `invalid_timestamps`. Rows whose timestamps tie keep journal read order
//! (the Python fold tie-broke on a canonical-JSON signature; inside one
//! session's window the observable order is the same). A `context_snapshot`
//! row whose envelope is structurally fine is kept here without running the
//! Python-side schema validation over its payload, so a payload-only defect
//! counts as a row here where the Python fold counted it malformed.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Timelike, Utc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const EXIT_NO_SUCH_TOPIC: i32 = 1;
const EXIT_USAGE: i32 = 2;

/// Labels that describe success or a deliberate stop; never failures.
const HEALTHY: &[&str] = &[
    "pass",
    "found",
    "stamped",
    "graduated",
    "idempotent_noop",
    "allow",
    "DonePRGreen",
    "DoneAdvisory",
    "DoneDelivery",
    "DoneUnreviewed",
    "DoneAwaitingMerge",
    "DoneAwaitingReview",
    "DoneBatched",
    "DonePlanned",
];

/// Event types whose labels carry no failure signal at all.
const NOISE_TYPES: &[&str] = &["guard_decision", "control_plane_tick", "gh_probe"];

/// The `data` keys a label comes from, first present wins.
const LABEL_KEYS: &[&str] = &["reason", "outcome", "verdict", "termination_reason"];

const USAGE: &str = "evals-macro --events <journal.jsonl> [--events ...] [--since 30d] \
[--topic TYPE:LABEL] [--window 20] [--all] [--json]";

// -- Envelope view -----------------------------------------------------------

/// One journal row projected into the join shape the fold reads.
struct Row {
    /// The raw `ts` (or `timestamp`) value, kept verbatim for display.
    raw_ts: Value,
    ts: Option<DateTime<Utc>>,
    event_type: Option<String>,
    session: Option<String>,
    node: Option<String>,
    data: Value,
}

/// Python truthiness over a JSON value, flattened to a display string.
/// `null` is falsy, an empty string is falsy, everything else coerces.
fn coerce_str(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        Value::Bool(b) => Some(if *b { "True".into() } else { "False".into() }),
        other => Some(other.to_string()),
    }
}

/// `event.type or event.kind`, truthiness-flattened like the Python view.
fn envelope_type(event: &Value) -> Option<String> {
    coerce_str(event.get("type").unwrap_or(&Value::Null))
        .or_else(|| coerce_str(event.get("kind").unwrap_or(&Value::Null)))
}

/// First present of `reason`/`outcome`/`verdict`/`termination_reason`,
/// stringified and truncated to 60 characters.
fn label_of(data: &Value) -> Option<String> {
    let obj = data.as_object()?;
    for key in LABEL_KEYS {
        if let Some(value) = obj.get(*key) {
            if let Some(text) = coerce_str(value) {
                if !text.is_empty() {
                    return Some(text.chars().take(60).collect());
                }
            }
        }
    }
    None
}

/// The row's session: envelope, data, then the attester fallback the Python
/// view added for review rows that carry only the attester's id.
fn row_session(event: &Value, data: &Map<String, Value>) -> Option<String> {
    coerce_str(event.get("session_id").unwrap_or(&Value::Null))
        .or_else(|| coerce_str(data.get("session_id").unwrap_or(&Value::Null)))
        .or_else(|| coerce_str(data.get("attester_session_id").unwrap_or(&Value::Null)))
}

/// The row's node: envelope `node_id`/`graph_node_id`, then the same keys in
/// `data` plus `data.node`, then `data.key` spelled `node:<id>`.
fn row_node(event: &Value, data: &Map<String, Value>) -> Option<String> {
    for k in ["node_id", "graph_node_id"] {
        if let Some(v) = event.get(k).map(coerce_str).flatten() {
            return Some(v);
        }
    }
    for k in ["node_id", "graph_node_id", "node"] {
        if let Some(v) = data.get(k).map(coerce_str).flatten() {
            return Some(v);
        }
    }
    data.get("key")
        .and_then(Value::as_str)
        .filter(|k| k.starts_with("node:"))
        .map(|k| k["node:".len()..].to_string())
}

impl Row {
    fn from_event(event: &Value) -> Row {
        let data = event
            .get("data")
            .filter(|d| d.is_object())
            .or_else(|| event.get("payload").filter(|d| d.is_object()))
            .cloned()
            .unwrap_or(Value::Null);
        let empty = Map::new();
        let data_obj = data.as_object().unwrap_or(&empty);
        let raw_ts = event
            .get("ts")
            .filter(|v| !v.is_null())
            .or_else(|| event.get("timestamp").filter(|v| !v.is_null()))
            .cloned()
            .unwrap_or(Value::Null);
        Row {
            ts: strict_utc_ts(&raw_ts),
            raw_ts,
            event_type: envelope_type(event),
            session: row_session(event, data_obj),
            node: row_node(event, data_obj),
            data,
        }
    }

    /// The `(type, label)` pattern, filtered by the healthy/noise sets unless
    /// `--all`.
    fn pattern(&self, include_all: bool) -> Option<String> {
        let event_type = self.event_type.as_deref()?;
        let label = label_of(&self.data)?;
        if !include_all && (HEALTHY.contains(&label.as_str()) || NOISE_TYPES.contains(&event_type))
        {
            return None;
        }
        Some(format!("{event_type}:{label}"))
    }
}

// -- Timestamps --------------------------------------------------------------

/// The strict row-timestamp shape the Python fold accepts:
/// `YYYY-MM-DDTHH:MM:SS[.f{1,6}](Z|+00:00)`, always UTC. Anything else is
/// None and counts as an invalid timestamp under `--since`.
fn strict_utc_ts(value: &Value) -> Option<DateTime<Utc>> {
    let s = value.as_str()?;
    let body = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (main, frac) = match body.split_once('.') {
        Some((m, f)) => {
            if f.is_empty() || f.len() > 6 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (m, Some(f))
        }
        None => (body, None),
    };
    let b = main.as_bytes();
    if b.len() != 19 {
        return None;
    }
    for (i, &c) in b.iter().enumerate() {
        let ok = match i {
            4 | 7 => c == b'-',
            10 => c == b'T',
            13 | 16 => c == b':',
            _ => c.is_ascii_digit(),
        };
        if !ok {
            return None;
        }
    }
    let mut ndt = NaiveDateTime::parse_from_str(main, "%Y-%m-%dT%H:%M:%S").ok()?;
    if let Some(f) = frac {
        let nanos: u32 = format!("{f:0<9}").parse().ok()?;
        ndt = ndt.with_nanosecond(nanos)?;
    }
    Some(DateTime::from_naive_utc_and_offset(ndt, Utc))
}

/// The loose `--since` stamp parser: ISO-8601 (naive reads as UTC) or a bare
/// date, mirroring the Python `datetime.fromisoformat` acceptance. One
/// implementation, shared with the trace verb's matcher.
use crate::client_verbs::parse_iso8601 as parse_loose_ts;

/// `--since`: a `Nd`/`Nh`/`Nm`/`Ns` duration back from now, else a loose ISO
/// stamp. A refusal names the offending value like the Python shell did.
/// The duration arm splits on CHARS: a byte-index slice would panic on a
/// non-ASCII final character.
fn parse_since(raw: &str) -> Result<DateTime<Utc>, String> {
    let t = raw.trim().to_lowercase();
    let mut chars = t.chars();
    let unit = chars.next_back();
    let digits = chars.as_str();
    if let Some(unit) = unit {
        if !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && matches!(unit, 's' | 'm' | 'h' | 'd')
        {
            let amount: i64 = digits.parse().unwrap_or(0);
            let dur = match unit {
                's' => chrono::Duration::seconds(amount),
                'm' => chrono::Duration::minutes(amount),
                'h' => chrono::Duration::hours(amount),
                _ => chrono::Duration::days(amount),
            };
            return Ok(Utc::now() - dur);
        }
    }
    parse_loose_ts(raw)
        .ok_or_else(|| format!("--since must be ISO-8601 or a duration such as 7d: {raw:?}"))
}

// -- Journal reader ----------------------------------------------------------

/// Sorted-key compact serialization: the dedup signature. Deterministic
/// within one run, which is all the seen-set needs.
fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{k}:{}", canonical_json(&m[k.as_str()])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

struct Coverage {
    complete: bool,
    malformed_lines: usize,
    unreadable_paths: usize,
    paths: Vec<Value>,
    invalid_timestamps: Option<usize>,
}

impl Coverage {
    fn to_json(&self) -> Value {
        let mut obj = json!({
            "complete": self.complete,
            "malformed_lines": self.malformed_lines,
            "unreadable_paths": self.unreadable_paths,
            "paths": self.paths,
        });
        if let Some(invalid) = self.invalid_timestamps {
            obj["invalid_timestamps"] = json!(invalid);
        }
        obj
    }
}

/// Absolute path key for journal dedup: resolved when the file exists,
/// lexically absolute otherwise (the Python fold's `Path.resolve` shape).
fn path_key(p: &Path) -> PathBuf {
    if p.exists() {
        if let Ok(c) = p.canonicalize() {
            return c;
        }
    }
    std::env::current_dir()
        .map(|base| base.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// Expand a leading `~` like `Path.expanduser` did for the Python reader.
fn expand_tilde(p: &Path) -> PathBuf {
    match p.as_os_str().to_str() {
        Some(s) if s.starts_with('~') => {
            PathBuf::from(s.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1))
        }
        _ => p.to_path_buf(),
    }
}

/// Read every journal into raw envelope rows with the input-integrity
/// coverage the Python fold reported. A missing journal is a positive zero;
/// unreadable ones and malformed lines surface in the coverage.
fn read_journals(paths: &[PathBuf]) -> (Vec<Value>, Coverage) {
    let mut rows = Vec::new();
    let mut seen_events: HashSet<String> = HashSet::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut path_coverage = Vec::new();
    let mut malformed_lines = 0usize;
    let mut unreadable_paths = 0usize;
    for raw_path in paths {
        let expanded = expand_tilde(raw_path);
        let key = path_key(&expanded);
        if !seen_paths.insert(key.clone()) {
            continue;
        }
        let key_str = key.display().to_string();
        if !expanded.exists() {
            path_coverage.push(json!({"path": key_str, "status": "missing", "malformed_lines": 0}));
            continue;
        }
        let content = match std::fs::read_to_string(&expanded) {
            Ok(c) => c,
            Err(e) => {
                unreadable_paths += 1;
                path_coverage.push(json!({
                    "path": key_str,
                    "status": "unreadable",
                    "error": format!("{e}"),
                    "malformed_lines": 0,
                }));
                continue;
            }
        };
        let mut path_malformed = 0usize;
        for line in content.lines() {
            per_line(
                line,
                &mut seen_events,
                &mut rows,
                &mut path_malformed,
                &mut malformed_lines,
            );
        }
        path_coverage.push(json!({
            "path": key_str,
            "status": if path_malformed == 0 { "ok" } else { "malformed" },
            "malformed_lines": path_malformed,
        }));
    }
    let coverage = Coverage {
        complete: malformed_lines == 0 && unreadable_paths == 0,
        malformed_lines,
        unreadable_paths,
        paths: path_coverage,
        invalid_timestamps: None,
    };
    (rows, coverage)
}

/// One journal line: skip nothing here (blank lines are already split away by
/// the caller over `lines()` only for non-blank trims - see read_journals),
/// count malformed JSON and non-objects, dedup by canonical signature, keep
/// the rest. Blank lines are skipped by the reader loop itself.
fn per_line(
    line: &str,
    seen_events: &mut HashSet<String>,
    rows: &mut Vec<Value>,
    path_malformed: &mut usize,
    malformed_lines: &mut usize,
) {
    let event: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            *path_malformed += 1;
            *malformed_lines += 1;
            return;
        }
    };
    if !event.is_object() {
        *path_malformed += 1;
        *malformed_lines += 1;
        return;
    }
    if seen_events.insert(canonical_json(&event)) {
        rows.push(event);
    }
}

/// Apply the `--since` cutoff: rows without a strict timestamp count as
/// invalid and drop; rows at or after the cutoff stay.
fn filter_since(
    raw_rows: Vec<Value>,
    cutoff: DateTime<Utc>,
    mut coverage: Coverage,
) -> (Vec<Row>, Coverage) {
    let mut rows = Vec::new();
    let mut invalid = 0usize;
    for raw in raw_rows {
        let row = Row::from_event(&raw);
        match row.ts {
            None => invalid += 1,
            Some(ts) if ts >= cutoff => rows.push(row),
            _ => {}
        }
    }
    if invalid > 0 {
        coverage.complete = false;
        coverage.invalid_timestamps = Some(invalid);
    }
    (rows, coverage)
}

/// Chronological order, untimestamped rows last, stable so ties keep the
/// journal read order.
fn ordered(rows: &mut Vec<Row>) {
    rows.sort_by_key(|r| (r.ts.is_none(), r.ts));
}

// -- The fold ----------------------------------------------------------------

/// One pattern's accumulated leaderboard stats.
struct EntryAcc {
    count: usize,
    unassigned: usize,
    sessions: HashSet<String>,
    nodes: HashSet<String>,
    first_seen: Value,
    last_seen: Value,
}

/// Build the leaderboard: one entry per failure pattern. Sorted sessions
/// descending, then count descending, then `termination:` rows ahead, then
/// pattern name. Returns entries in that order with their pattern name.
fn build_leaderboard(rows: &[Row], include_all: bool) -> Vec<(String, EntryAcc)> {
    let mut order: Vec<String> = Vec::new();
    let mut entries: HashMap<String, EntryAcc> = HashMap::new();
    for row in rows {
        let Some(pattern) = row.pattern(include_all) else {
            continue;
        };
        let entry = entries.entry(pattern.clone()).or_insert_with(|| {
            order.push(pattern.clone());
            EntryAcc {
                count: 0,
                unassigned: 0,
                sessions: HashSet::new(),
                nodes: HashSet::new(),
                first_seen: Value::Null,
                last_seen: Value::Null,
            }
        });
        entry.count += 1;
        match &row.session {
            None => entry.unassigned += 1,
            Some(s) => {
                entry.sessions.insert(s.clone());
            }
        }
        if let Some(n) = &row.node {
            entry.nodes.insert(n.clone());
        }
        if entry.first_seen.is_null() {
            entry.first_seen = row.raw_ts.clone();
        }
        entry.last_seen = row.raw_ts.clone();
    }
    let mut leaderboard: Vec<(String, EntryAcc)> = order
        .into_iter()
        .map(|p| {
            let acc = entries.remove(&p).expect("pattern was inserted above");
            (p, acc)
        })
        .collect();
    leaderboard.sort_by(|(pa, a), (pb, b)| {
        b.sessions
            .len()
            .cmp(&a.sessions.len())
            .then(b.count.cmp(&a.count))
            .then(
                // termination: rows ahead of the rest on a full tie (the
                // Python key put them at 0, ascending).
                pb.starts_with("termination:")
                    .cmp(&pa.starts_with("termination:")),
            )
            .then(pa.cmp(pb))
    });
    leaderboard
}

/// Upstream suspects for one pattern: distinct patterns in the `window` rows
/// before each occurrence in the same session, ranked by
/// `lift = P(suspect | pattern) / P(suspect in any pattern row)`, needing
/// support in at least two sessions, top three.
fn suspects_for_pattern(
    sessions: &std::collections::BTreeMap<String, Vec<usize>>,
    rows: &[Row],
    target: &str,
    window: usize,
    include_all: bool,
) -> Vec<Value> {
    let mut occurrences: Vec<(&String, usize)> = Vec::new();
    for (session, indices) in sessions {
        for (pos, &row_idx) in indices.iter().enumerate() {
            if rows[row_idx].pattern(include_all).as_deref() == Some(target) {
                occurrences.push((session, pos));
            }
        }
    }
    if occurrences.is_empty() {
        return Vec::new();
    }
    let target_sessions: HashSet<&&String> = occurrences.iter().map(|(s, _)| s).collect();
    let mut global_counts: HashMap<String, usize> = HashMap::new();
    let mut total_pattern_rows = 0usize;
    for row in rows {
        if let Some(p) = row.pattern(include_all) {
            *global_counts.entry(p).or_insert(0) += 1;
            total_pattern_rows += 1;
        }
    }
    let mut candidate_sessions: HashMap<String, HashSet<String>> = HashMap::new();
    let mut candidate_counts: HashMap<String, usize> = HashMap::new();
    for (session, pos) in &occurrences {
        let indices = &sessions[*session];
        let low = pos.saturating_sub(window);
        for &row_idx in &indices[low..*pos] {
            if let Some(p) = rows[row_idx].pattern(include_all) {
                if p == target {
                    continue;
                }
                candidate_sessions
                    .entry(p.clone())
                    .or_default()
                    .insert((*session).clone());
                *candidate_counts.entry(p).or_insert(0) += 1;
            }
        }
    }
    let mut suspects: Vec<Value> = Vec::new();
    for (pattern, support_sessions) in &candidate_sessions {
        let support = support_sessions.len();
        if support < 2 {
            continue;
        }
        let conditional = support as f64 / target_sessions.len() as f64;
        let prevalence = global_counts
            .get(pattern.as_str())
            .map(|c| *c as f64 / total_pattern_rows as f64)
            .unwrap_or(0.0);
        let lift = if prevalence > 0.0 {
            (conditional / prevalence * 10_000.0).round() / 10_000.0
        } else {
            0.0
        };
        suspects.push(json!({
            "pattern": pattern,
            "sessions": support,
            "count": candidate_counts[pattern],
            "lift": lift,
        }));
    }
    suspects.sort_by(|a, b| {
        let lift = |v: &Value| v["lift"].as_f64().unwrap_or(0.0);
        lift(b)
            .partial_cmp(&lift(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b["sessions"]
                    .as_u64()
                    .unwrap_or(0)
                    .cmp(&a["sessions"].as_u64().unwrap_or(0)),
            )
            .then(a["pattern"].as_str().cmp(&b["pattern"].as_str()))
    });
    suspects.truncate(3);
    suspects
}

/// Session name -> indices into `rows`, in the ordered row sequence. A
/// BTreeMap, not a HashMap: the drilldown walks it to build the fire list,
/// and equal-timestamp ties must resolve the same way on every run.
fn sessions_map(rows: &[Row]) -> std::collections::BTreeMap<String, Vec<usize>> {
    let mut map: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, row) in rows.iter().enumerate() {
        if let Some(s) = &row.session {
            map.entry(s.clone()).or_default().push(i);
        }
    }
    map
}

/// Drill into one pattern: the sessions it fired in with node id and ts, the
/// same suspect table the leaderboard carries, and for the five most recent
/// fires the event chain across the window before (and including) the fire.
/// Unassigned rows never chain: they have no session to chain within.
fn drilldown(
    rows: &[Row],
    sessions: &std::collections::BTreeMap<String, Vec<usize>>,
    board: &[(String, EntryAcc)],
    pattern: &str,
    window: usize,
    limit: usize,
    include_all: bool,
) -> Value {
    let mut fires: Vec<(String, usize)> = Vec::new();
    for (session, indices) in sessions {
        for (pos, &row_idx) in indices.iter().enumerate() {
            if rows[row_idx].pattern(include_all).as_deref() == Some(pattern) {
                fires.push((session.clone(), pos));
            }
        }
    }
    fires.sort_by(|a, b| {
        let ts = |loc: &(String, usize)| -> DateTime<Utc> {
            let idx = sessions[&loc.0][loc.1];
            rows[idx].ts.unwrap_or_else(min_ts)
        };
        ts(b).cmp(&ts(a))
    });
    let mut details: Vec<Value> = Vec::new();
    for (session, pos) in fires.iter().take(limit) {
        let indices = &sessions[session];
        let low = pos.saturating_sub(window);
        let mut chain: Vec<Value> = Vec::new();
        for &row_idx in &indices[low..=*pos] {
            let row = &rows[row_idx];
            chain.push(json!({
                "ts": row.raw_ts,
                "type": row.event_type.clone().map(Value::String).unwrap_or(Value::Null),
                "label": label_of(&row.data).map(Value::String).unwrap_or(Value::Null),
            }));
        }
        let fire_row = &rows[indices[*pos]];
        details.push(json!({
            "session_id": session,
            "node_id": fire_row.node.clone().map(Value::String).unwrap_or(Value::Null),
            "ts": fire_row.raw_ts,
            "chain": chain,
        }));
    }
    let summary = board.iter().find(|(p, _)| p == pattern);
    let suspects = match summary {
        Some((p, _)) => suspects_for_pattern(sessions, rows, p, window, include_all),
        None => Vec::new(),
    };
    json!({
        "pattern": pattern,
        "count": summary.map(|(_, a)| a.count).unwrap_or(0),
        "unassigned": summary.map(|(_, a)| a.unassigned).unwrap_or(0),
        "sessions": details,
        "suspects": suspects,
    })
}

/// The datetime.min stand-in for a missing ts in the drilldown's
/// most-recent-first sort (Python sorted fires by `ts or datetime.min`).
fn min_ts() -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(
        NaiveDate::from_ymd_opt(1, 1, 1)
            .expect("valid epoch start")
            .and_hms_opt(0, 0, 0)
            .expect("valid midnight"),
        Utc,
    )
}

// -- The verb ----------------------------------------------------------------

/// One `--flag value` / `--flag=value` token pair, advancing the cursor. The
/// caller's loop already stepped `i` past the flag, so the value sits AT `i`.
fn take_value(args: &[String], i: &mut usize, inline: Option<String>) -> Option<String> {
    if inline.is_some() {
        return inline;
    }
    let value = args.get(*i).cloned();
    if value.is_some() {
        *i += 1;
    }
    value
}

/// Print the one-line usage and return the usage exit code.
fn usage(msg: &str) -> i32 {
    if !msg.is_empty() {
        eprintln!("{msg}");
    }
    eprintln!("usage: {USAGE}");
    EXIT_USAGE
}

/// `fno-agents evals-macro`: fold the journals into the leaderboard. The
/// Python shell resolves the default journal list; the binary needs at least
/// one `--events` path to read.
pub fn run_evals_macro(args: &[String]) -> i32 {
    let mut since = "30d".to_string();
    let mut topic: Option<String> = None;
    let mut window: usize = 20;
    let mut include_all = false;
    let mut json_output = false;
    let mut events: Vec<PathBuf> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].clone();
        i += 1;
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        match name.as_str() {
            "--since" => match take_value(args, &mut i, inline) {
                Some(v) => since = v,
                None => return usage("evals-macro: --since needs a value"),
            },
            "--topic" => match take_value(args, &mut i, inline) {
                Some(v) => topic = Some(v),
                None => return usage("evals-macro: --topic needs a value"),
            },
            "--window" => {
                match take_value(args, &mut i, inline).and_then(|v| v.parse::<usize>().ok()) {
                    Some(w) => window = w,
                    None => return usage("evals-macro: --window needs a positive integer"),
                }
            }
            "--events" => match take_value(args, &mut i, inline) {
                Some(v) => events.push(PathBuf::from(v)),
                None => return usage("evals-macro: --events needs a path"),
            },
            "--all" | "-A" => include_all = true,
            "--json" | "-J" => json_output = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other => return usage(&format!("evals-macro: unknown flag {other:?}")),
        }
    }
    if events.is_empty() {
        return usage(
            "evals-macro: at least one --events path is required; \
the Python surface resolves the journal defaults",
        );
    }
    if window == 0 {
        return usage("evals-macro: --window must be at least 1");
    }
    let cutoff = match parse_since(&since) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: {msg}");
            return EXIT_NO_SUCH_TOPIC;
        }
    };
    let (raw_rows, cov) = read_journals(&events);
    let (mut rows, coverage) = filter_since(raw_rows, cutoff, cov);
    ordered(&mut rows);
    let sessions = sessions_map(&rows);
    let board = build_leaderboard(&rows, include_all);

    if let Some(topic) = topic {
        let result = drilldown(&rows, &sessions, &board, &topic, window, 5, include_all);
        if result["count"].as_u64().unwrap_or(0) == 0 {
            let present: Vec<&str> = board.iter().map(|(p, _)| p.as_str()).collect();
            let msg = format!(
                "macro: no '{topic}' rows since {since}; patterns present: {}",
                if present.is_empty() {
                    "none".to_string()
                } else {
                    present.join(", ")
                }
            );
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "error": msg,
                        "patterns": present,
                    }))
                    .expect("serializes a plain object")
                );
            } else {
                println!("{msg}");
            }
            return EXIT_NO_SUCH_TOPIC;
        }
        if json_output {
            let mut payload = result.clone();
            payload["coverage"] = coverage.to_json();
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).expect("serializes")
            );
        } else {
            println!("macro: {topic}");
            if let Some(fired) = result["sessions"].as_array() {
                for session in fired {
                    let node = display_value(&session["node_id"]);
                    let ts = display_value(&session["ts"]);
                    println!(
                        "  session {} node {node} at {ts}",
                        session["session_id"].as_str().unwrap_or("-")
                    );
                    let chain: Vec<String> = session["chain"]
                        .as_array()
                        .map(|events| {
                            events
                                .iter()
                                .map(|e| {
                                    format!(
                                        "{} {} {}",
                                        display_value(&e["ts"]),
                                        display_value(&e["type"]),
                                        display_value(&e["label"])
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    println!("    chain: {}", chain.join(" -> "));
                }
            }
        }
        return 0;
    }

    if json_output {
        let payload = json!({
            "leaderboard": board
                .iter()
                .map(|(p, a)| entry_json(p, a, &sessions, &rows, window, include_all))
                .collect::<Vec<_>>(),
            "coverage": coverage.to_json(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("serializes")
        );
        return 0;
    }
    if !coverage.complete {
        let mut line = format!(
            "coverage: {} journals, {} malformed lines skipped",
            coverage.paths.len(),
            coverage.malformed_lines
        );
        if let Some(invalid) = coverage.invalid_timestamps {
            line.push_str(&format!(", {invalid} invalid timestamps skipped"));
        }
        println!("{line}");
    }
    if rows.is_empty() {
        println!("macro: no events since {since}");
        return 0;
    }
    println!("rank  sessions  count  nodes  first  last  pattern  top suspect (lift)");
    for (idx, (pattern, acc)) in board.iter().enumerate().take(20) {
        let rank = idx + 1;
        let suspects = suspects_for_pattern(&sessions, &rows, pattern, window, include_all);
        let suspect_text = suspects
            .first()
            .map(|s| {
                format!(
                    "{} ({:.2})",
                    s["pattern"].as_str().unwrap_or("-"),
                    s["lift"].as_f64().unwrap_or(0.0)
                )
            })
            .unwrap_or_else(|| "-".to_string());
        let first = display_value(&acc.first_seen);
        let last = display_value(&acc.last_seen);
        println!(
            "{rank:>4}  {:>8}  {:>5}  {:>5}  {first}  {last}  {pattern}  {suspect_text}",
            acc.sessions.len(),
            acc.count,
            acc.nodes.len(),
        );
    }
    0
}

/// Render a raw JSON value for the text tables: null prints as "-", Python
/// printed `str()` of anything else.
fn display_value(v: &Value) -> String {
    match v {
        Value::Null => "-".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One leaderboard entry in the Python fold's entry-dict key order:
/// identity and counts first, the suspect table last.
fn entry_json(
    pattern: &str,
    acc: &EntryAcc,
    sessions: &std::collections::BTreeMap<String, Vec<usize>>,
    rows: &[Row],
    window: usize,
    include_all: bool,
) -> Value {
    let suspects = suspects_for_pattern(sessions, rows, pattern, window, include_all);
    json!({
        "pattern": pattern,
        "count": acc.count,
        "sessions": acc.sessions.len(),
        "nodes": acc.nodes.len(),
        "unassigned": acc.unassigned,
        "first_seen": acc.first_seen,
        "last_seen": acc.last_seen,
        "suspects": suspects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Python test fixture's `_event(ts, type, session=..., node=..., **data)`.
    fn event(
        ts: &str,
        event_type: &str,
        session: Option<&str>,
        node: Option<&str>,
        data: Value,
    ) -> Value {
        let mut payload = data.as_object().cloned().unwrap_or_default();
        if let Some(s) = session {
            payload.insert("session_id".into(), json!(s));
        }
        if let Some(n) = node {
            payload.insert("node_id".into(), json!(n));
        }
        json!({"ts": ts, "type": event_type, "data": payload})
    }

    fn rows_of(values: &[Value]) -> Vec<Row> {
        values.iter().map(Row::from_event).collect()
    }

    #[test]
    fn leaderboard_ranks_upstream_suspect_for_repeated_failure() {
        let rows = rows_of(&[
            event(
                "2026-09-12T10:00:00Z",
                "loop_check_watch_idle",
                Some("s1"),
                Some("n1"),
                json!({"reason": "ci"}),
            ),
            event(
                "2026-09-12T10:01:00Z",
                "termination",
                Some("s1"),
                Some("n1"),
                json!({"reason": "Budget"}),
            ),
            event(
                "2026-09-12T11:00:00Z",
                "loop_check_watch_idle",
                Some("s2"),
                Some("n2"),
                json!({"reason": "ci"}),
            ),
            event(
                "2026-09-12T11:01:00Z",
                "termination",
                Some("s2"),
                Some("n2"),
                json!({"reason": "Budget"}),
            ),
            event(
                "2026-09-12T12:00:00Z",
                "loop_check_watch_idle",
                Some("s3"),
                Some("n3"),
                json!({"reason": "ci"}),
            ),
        ]);
        let board = build_leaderboard(&rows, false);
        let (_, acc) = board
            .iter()
            .find(|(p, _)| p == "termination:Budget")
            .unwrap();
        assert_eq!(acc.sessions.len(), 2);
        assert_eq!(acc.count, 2);
        assert_eq!(acc.nodes.len(), 2);
        let sessions = sessions_map(&rows);
        let suspects = suspects_for_pattern(&sessions, &rows, "termination:Budget", 20, false);
        assert_eq!(suspects[0]["pattern"], json!("loop_check_watch_idle:ci"));
        assert!((suspects[0]["lift"].as_f64().unwrap() - 1.6667).abs() < 1e-9);
    }

    #[test]
    fn leaderboard_keeps_unassigned_rows_and_filters_noise() {
        let values = [
            event(
                "2026-09-12T10:00:00Z",
                "termination",
                None,
                None,
                json!({"reason": "Budget"}),
            ),
            event(
                "2026-09-12T10:01:00Z",
                "guard_decision",
                Some("s1"),
                None,
                json!({"reason": "allow"}),
            ),
            event(
                "2026-09-12T10:02:00Z",
                "termination",
                Some("s1"),
                None,
                json!({"reason": "DonePRGreen"}),
            ),
            event(
                "2026-09-12T10:03:00Z",
                "termination",
                Some("s1"),
                None,
                json!({"reason": "DoneBatched"}),
            ),
            event(
                "2026-09-12T10:04:00Z",
                "termination",
                Some("s1"),
                None,
                json!({"reason": "DonePlanned"}),
            ),
        ];
        let rows = rows_of(&values);
        let board = build_leaderboard(&rows, false);
        let patterns: Vec<&str> = board.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(patterns, vec!["termination:Budget"]);
        let (_, acc) = board
            .iter()
            .find(|(p, _)| p == "termination:Budget")
            .unwrap();
        assert_eq!(acc.count, 1);
        assert_eq!(acc.unassigned, 1);

        let all_board = build_leaderboard(&rows_of(&values), true);
        let all_patterns: HashSet<&str> = all_board.iter().map(|(p, _)| p.as_str()).collect();
        assert!(all_patterns.contains("guard_decision:allow"));
        assert!(all_patterns.contains("termination:DonePRGreen"));
    }

    #[test]
    fn journals_report_malformed_lines_and_invalid_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        std::fs::write(
            &journal,
            format!(
                "{}\nnot json\n{}\n{}\n{}\n",
                event(
                    "2026-09-12T10:00:00Z",
                    "termination",
                    None,
                    None,
                    json!({"reason": "Budget"})
                ),
                event(
                    "2026-09-12T10:01:00Z",
                    "termination",
                    None,
                    None,
                    json!({"reason": "NoProgress"})
                ),
                event(
                    "not-a-timestamp",
                    "termination",
                    None,
                    None,
                    json!({"reason": "Interrupted"})
                ),
                event(
                    "2026-09-12T10:02:00",
                    "termination",
                    None,
                    None,
                    json!({"reason": "Naive"})
                ),
            ),
        )
        .unwrap();
        let (raw, cov) = read_journals(&[journal]);
        let cutoff = parse_since("2026-09-01").unwrap();
        let (rows, coverage) = filter_since(raw, cutoff, cov);
        assert_eq!(rows.len(), 2);
        assert_eq!(coverage.malformed_lines, 1);
        assert_eq!(coverage.invalid_timestamps, Some(2));
        assert!(!coverage.complete);
    }

    #[test]
    fn verb_exit_codes_match_the_python_surface() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let now = Utc::now();
        // The verb always applies a cutoff (default 30d), so the topic cases
        // need fresh rows; the stale journal covers the empty-window case.
        let fresh = dir.path().join("fresh.jsonl");
        std::fs::write(
            &fresh,
            format!(
                "{}\n{}\n",
                event(
                    &stamp(now - chrono::Duration::seconds(60)),
                    "loop_check_watch_idle",
                    Some("s1"),
                    Some("n1"),
                    json!({"reason": "ci"})
                ),
                event(
                    &stamp(now - chrono::Duration::seconds(30)),
                    "termination",
                    Some("s1"),
                    Some("n1"),
                    json!({"reason": "Budget"})
                ),
            ),
        )
        .unwrap();
        let stale = dir.path().join("stale.jsonl");
        std::fs::write(
            &stale,
            format!(
                "{}\n{}\n",
                event(
                    "2020-01-01T10:00:00Z",
                    "loop_check_watch_idle",
                    Some("s9"),
                    Some("n9"),
                    json!({"reason": "ci"})
                ),
                event(
                    "2020-01-01T10:01:00Z",
                    "termination",
                    Some("s9"),
                    Some("n9"),
                    json!({"reason": "Budget"})
                ),
            ),
        )
        .unwrap();
        let fresh_path = fresh.display().to_string();
        let stale_path = stale.display().to_string();
        let run = |args: &[&str]| -> i32 {
            run_evals_macro(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };

        assert_eq!(run(&["--events", &fresh_path, "--json"]), 0);
        assert_eq!(
            run(&["--events", &fresh_path, "--topic", "termination:Budget"]),
            0
        );
        assert_eq!(run(&["--events", &fresh_path, "--topic", "nope:never"]), 1);
        assert_eq!(run(&["--events", &stale_path, "--since", "1h"]), 0);
        assert_eq!(run(&[]), EXIT_USAGE);
        assert_eq!(run(&["--events", &fresh_path, "--window", "0"]), EXIT_USAGE);
        assert_eq!(run(&["--events", &fresh_path, "--bogus"]), EXIT_USAGE);
        assert_eq!(run(&["--events", &fresh_path, "--since", "x"]), 1);
    }

    #[test]
    fn strict_timestamp_accepts_only_utc_shapes() {
        assert!(strict_utc_ts(&json!("2026-09-12T10:00:00Z")).is_some());
        assert!(strict_utc_ts(&json!("2026-09-12T10:00:00+00:00")).is_some());
        assert!(strict_utc_ts(&json!("2026-09-12T10:00:00.1Z")).is_some());
        assert!(strict_utc_ts(&json!("2026-09-12T10:00:00.123456Z")).is_some());
        assert!(strict_utc_ts(&json!("2026-09-12T10:00:00.1234567Z")).is_none());
        assert!(strict_utc_ts(&json!("2026-09-12T10:00:00")).is_none());
        assert!(strict_utc_ts(&json!("2026-09-12")).is_none());
        assert_eq!(
            strict_utc_ts(&json!("2026-09-12T10:00:00.500Z"))
                .unwrap()
                .timestamp_millis()
                % 1000,
            500
        );
    }

    #[test]
    fn loose_since_parser_reads_naive_and_offsets() {
        assert!(parse_loose_ts("2026-09-13").is_some());
        assert!(parse_loose_ts("2026-09-13T05:00:00").is_some());
        assert_eq!(
            parse_loose_ts("2026-09-13T12:00:00+02:00").unwrap(),
            parse_loose_ts("2026-09-13T10:00:00Z").unwrap()
        );
        assert!(parse_loose_ts("x").is_none());
    }

    #[test]
    fn since_durations_parse_back_from_now() {
        assert!(parse_since("7d").is_ok());
        assert!(parse_since("2h").is_ok());
        assert!(parse_since("30d").is_ok());
        assert!(parse_since("x").is_err());
        assert!(parse_since("2026-09-13").is_ok());
        assert!(parse_since("2026-09-13T00:00:00Z").is_ok());
    }

    #[test]
    fn suspect_prevalence_damps_a_frequent_outside_candidate() {
        let mut values = Vec::new();
        // 98 sessions where B fires alone: globally prevalent, never before A.
        let start = strict_utc_ts(&json!("2026-01-03T00:00:00Z")).unwrap();
        for i in 0..98u32 {
            let ts = (start + chrono::Duration::minutes(i as i64))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            values.push(event(
                &ts,
                "err",
                Some(format!("b{i}").as_str()),
                None,
                json!({"reason": "B"}),
            ));
        }
        // Two target sessions: B, then the rare co-occurring C, then A.
        for (session, day) in [("t1", "01"), ("t2", "02")] {
            let base = format!("2026-02-{day}");
            values.push(event(
                &format!("{base}T10:00:00Z"),
                "err",
                Some(session),
                None,
                json!({"reason": "B"}),
            ));
            values.push(event(
                &format!("{base}T10:01:00Z"),
                "err",
                Some(session),
                None,
                json!({"reason": "C"}),
            ));
            values.push(event(
                &format!("{base}T10:02:00Z"),
                "err",
                Some(session),
                None,
                json!({"reason": "A"}),
            ));
        }
        let mut rows = rows_of(&values);
        ordered(&mut rows);
        let sessions = sessions_map(&rows);
        let suspects = suspects_for_pattern(&sessions, &rows, "err:A", 20, false);
        assert_eq!(suspects[0]["pattern"], json!("err:C"));
        let c_lift = suspects[0]["lift"].as_f64().unwrap();
        assert!(
            c_lift > 10.0,
            "rare co-occurring C should dominate: {c_lift}"
        );
        let b = suspects
            .iter()
            .find(|s| s["pattern"] == json!("err:B"))
            .expect("B still meets the two-session support floor");
        let b_lift = b["lift"].as_f64().unwrap();
        assert!(b_lift < 2.0, "global prevalence must damp B: {b_lift}");
    }

    #[test]
    fn leaderboard_breaks_a_full_tie_toward_termination_rows() {
        // The Python CLI fixture: all rows share one timestamp, both patterns
        // sit at sessions 2 / count 2, and termination:Budget must take rank
        // one.
        let rows = rows_of(&[
            event(
                "2026-09-12T10:00:00Z",
                "loop_check_watch_idle",
                Some("s1"),
                Some("n1"),
                json!({"reason": "ci"}),
            ),
            event(
                "2026-09-12T10:00:00Z",
                "termination",
                Some("s1"),
                Some("n1"),
                json!({"reason": "Budget"}),
            ),
            event(
                "2026-09-12T10:00:00Z",
                "loop_check_watch_idle",
                Some("s2"),
                Some("n2"),
                json!({"reason": "ci"}),
            ),
            event(
                "2026-09-12T10:00:00Z",
                "termination",
                Some("s2"),
                Some("n2"),
                json!({"reason": "Budget"}),
            ),
        ]);
        let board = build_leaderboard(&rows, false);
        assert_eq!(board.len(), 2);
        assert_eq!(
            board[0].0, "termination:Budget",
            "termination: rows lead on a full tie"
        );
    }

    #[test]
    fn since_with_a_non_ascii_unit_is_a_refusal_not_a_panic() {
        // The duration arm splits on chars: a byte-index slice used to panic
        // here (byte index N is not a char boundary).
        assert!(parse_since("7é").is_err());
        assert!(parse_since("3日").is_err());
        assert!(parse_since("7d").is_ok());
    }

    #[test]
    fn drilldown_breaks_equal_timestamp_ties_deterministically() {
        // Two sessions fire at the same instant; the five-slot detail list is
        // cut to one. The session order comes from the BTreeMap, so the pick
        // is the same on every run instead of HashMap's per-process order.
        let rows = rows_of(&[
            event(
                "2026-09-12T10:00:00Z",
                "termination",
                Some("s2"),
                Some("n2"),
                json!({"reason": "Budget"}),
            ),
            event(
                "2026-09-12T10:00:00Z",
                "termination",
                Some("s1"),
                Some("n1"),
                json!({"reason": "Budget"}),
            ),
        ]);
        let sessions = sessions_map(&rows);
        let board = build_leaderboard(&rows, false);
        let result = drilldown(&rows, &sessions, &board, "termination:Budget", 20, 1, false);
        let details = result["sessions"].as_array().expect("details list");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["session_id"], "s1", "sorted session order");
    }
}
