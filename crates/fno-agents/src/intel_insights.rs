//! The insights layer over the fold's rows: the two drops, the substantive
//! gate, the populations block, the per-day and activity series, the
//! idle-gated blake3 sample, and the categories post-process. Pure functions
//! over [`crate::intel::SessionRow`] (at fold time) and over the saved fold
//! JSON (post-process). No transcript is read here, and no model runs here.

use crate::intel::SessionRow;
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// A session is idle when its transcript has been untouched this long.
pub(crate) const IDLE_SECS: u64 = 1800;

/// True when the only turns a session saw are keepalive pings and harness
/// machinery: the cache warmer and nothing else.
pub(crate) fn is_keepalive_only(counters: &BTreeMap<&'static str, u64>) -> bool {
    if counters.get("keepalive").copied().unwrap_or(0) == 0 {
        return false;
    }
    counters
        .iter()
        .all(|(label, count)| *label == "keepalive" || label.starts_with("harness_") || *count == 0)
}

/// The rows the fold read but does not report: keepalive-only sessions and
/// the intel run's own session.
#[derive(Debug, Default, Serialize)]
pub(crate) struct Dropped {
    pub(crate) keepalive_only: usize,
    pub(crate) self_dropped: usize,
}

pub(crate) fn drop_rows(rows: Vec<SessionRow>, own: Option<&str>) -> (Vec<SessionRow>, Dropped) {
    let mut dropped = Dropped::default();
    let kept = rows
        .into_iter()
        .filter(|row| {
            if is_keepalive_only(&row.counters) {
                dropped.keepalive_only += 1;
                return false;
            }
            if let Some(own) = own {
                if crate::claims::same_session_id(own, &row.session) {
                    dropped.self_dropped += 1;
                    return false;
                }
            }
            true
        })
        .collect();
    (kept, dropped)
}

/// The substantive gate: `operator + unknown` turns of 2 or more and a
/// duration of 60 s or more. Turn counts include `unknown` because history
/// from before the mux witness has no witnessed turns at all; every
/// per-person measure elsewhere uses witnessed turns only.
pub(crate) fn is_substantive(operator_plus_unknown: u64, duration_s: Option<i64>) -> bool {
    operator_plus_unknown >= 2 && duration_s.is_some_and(|d| d >= 60)
}

pub(crate) fn populations(
    transcripts: usize,
    dropped: &Dropped,
    rows: &[SessionRow],
    eligible: Option<usize>,
    sampled: Option<usize>,
) -> Value {
    json!({
        "transcripts": transcripts,
        "dropped": {
            "keepalive_only": dropped.keepalive_only,
            "self": dropped.self_dropped,
        },
        "scanned": rows.len(),
        "attended": rows.iter().filter(|r| r.kind == "attended").count(),
        "substantive": rows.iter().filter(|r| r.substantive).count(),
        "eligible": eligible,
        "sampled": sampled,
        "judged": Value::Null,
    })
}

/// Summed activity over the scanned rows. A harness whose rows carry null
/// activity (no parser) is named under `unmeasured`, never folded in as 0.
pub(crate) fn activity_totals(rows: &[SessionRow]) -> Value {
    let mut tokens = crate::session_activity::Tokens::default();
    let mut lines = (0u64, 0u64);
    let mut tool_errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut languages: BTreeMap<String, u64> = BTreeMap::new();
    let mut interruptions = 0u64;
    let mut unmeasured: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows {
        match (&row.tokens, &row.tool_errors, &row.languages) {
            (Some(t), Some(errs), Some(langs)) => {
                tokens.input += t.input;
                tokens.output += t.output;
                tokens.cache_read += t.cache_read;
                tokens.cache_write += t.cache_write;
                for (class, n) in errs {
                    *tool_errors.entry((*class).to_string()).or_insert(0) += n;
                }
                for (ext, n) in langs {
                    *languages.entry(ext.clone()).or_insert(0) += n;
                }
            }
            _ => {
                *unmeasured.entry(row.harness.to_string()).or_insert(0) += 1;
            }
        }
        if let Some(l) = &row.lines {
            lines.0 += l.added;
            lines.1 += l.removed;
        }
        interruptions += row.interruptions;
    }
    json!({
        "tokens": {
            "input": tokens.input,
            "output": tokens.output,
            "cache_read": tokens.cache_read,
            "cache_write": tokens.cache_write,
        },
        "lines": {"added": lines.0, "removed": lines.1},
        "tool_errors": tool_errors,
        "interruptions": interruptions,
        "languages": languages,
        "unmeasured": unmeasured,
    })
}

fn turn_epoch(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp())
}

/// The local hour of every witnessed operator turn, computed fresh each run
/// so a move across time zones cannot freeze an old bucket.
pub(crate) fn hours(rows: &[SessionRow]) -> Value {
    let mut buckets = [0u64; 24];
    for row in rows {
        for ts in &row.operator_turns {
            if let Some(secs) = turn_epoch(ts) {
                if let Some(dt) = chrono::Local.timestamp_opt(secs, 0).single() {
                    buckets[dt.hour() as usize] += 1;
                }
            }
        }
    }
    json!({
        "utc_offset": chrono::Local::now().offset().to_string(),
        "operator_turns": buckets,
    })
}

use chrono::Timelike;

/// Response time over every kept gap: count, median, p90, and the seven
/// named buckets.
pub(crate) fn response_time(rows: &[SessionRow]) -> Value {
    let mut gaps: Vec<u64> = rows
        .iter()
        .flat_map(|r| r.response_s.iter().copied())
        .collect();
    gaps.sort();
    let pick = |frac: f64| -> Option<u64> {
        let n = gaps.len();
        if n == 0 {
            return None;
        }
        let idx = ((frac * (n - 1) as f64).round()) as usize;
        Some(gaps[idx])
    };
    let mut buckets: BTreeMap<&str, u64> = BTreeMap::new();
    let names = [
        ("2-10s", 2, 10),
        ("10-30s", 10, 30),
        ("30-60s", 30, 60),
        ("1-2m", 60, 120),
        ("2-5m", 120, 300),
        ("5-15m", 300, 900),
        ("15-60m", 900, 3600),
    ];
    for (name, lo, hi) in names {
        let n = gaps.iter().filter(|g| **g >= lo && **g < hi).count() as u64;
        buckets.insert(name, n);
    }
    // response_s only holds 2..=3600, but a defensive tail keeps the bucket
    // sum equal to n if the bounds ever widen.
    let tail = gaps.iter().filter(|g| **g >= 3600).count() as u64;
    *buckets.get_mut("15-60m").unwrap() += tail;
    json!({
        "n": gaps.len(),
        "median_s": pick(0.5),
        "p90_s": pick(0.9),
        "buckets": buckets,
    })
}

/// Sessions a person drove in parallel: when two of one session's witnessed
/// turns land within the window and a second session's turn falls between
/// them, the pair overlaps once.
pub(crate) fn parallel(rows: &[SessionRow]) -> Value {
    const WINDOW_S: i64 = 1800;
    let mut stream: Vec<(i64, &str)> = Vec::new();
    for row in rows {
        for ts in &row.operator_turns {
            if let Some(secs) = turn_epoch(ts) {
                stream.push((secs, row.session.as_str()));
            }
        }
    }
    stream.sort();
    let mut pairs: HashSet<(String, String)> = HashSet::new();
    let mut by_session: HashMap<&str, Vec<i64>> = HashMap::new();
    for (secs, sid) in &stream {
        by_session.entry(sid).or_default().push(*secs);
    }
    for (sid, stamps) in &by_session {
        for pair in stamps.windows(2) {
            if pair[1] - pair[0] > WINDOW_S {
                continue;
            }
            let start = stream.partition_point(|(ts, _)| *ts <= pair[0]);
            let end = stream.partition_point(|(ts, _)| *ts < pair[1]);
            for (ts, other) in &stream[start..end] {
                if *other != *sid {
                    let key = if sid < other {
                        (sid.to_string(), other.to_string())
                    } else {
                        (other.to_string(), sid.to_string())
                    };
                    pairs.insert(key);
                    let _ = ts;
                }
            }
        }
    }
    let mut sessions: HashSet<&str> = HashSet::new();
    for (a, b) in &pairs {
        sessions.insert(a.as_str());
        sessions.insert(b.as_str());
    }
    json!({
        "window_s": WINDOW_S,
        "overlap_pairs": pairs.len(),
        "sessions": sessions.len(),
    })
}

/// Scanned sessions per local date and harness, plus the count of rows with
/// no `started` stamp, which land in no cell.
pub(crate) fn daily(rows: &[SessionRow]) -> (Vec<Value>, usize) {
    // output_tokens sums the present values; None means every row of the
    // cell was unmeasured, and the entry reads null.
    #[derive(Default)]
    struct Cell {
        sessions: u64,
        operator_turns: u64,
        tool_use: u64,
        output_tokens: Option<u64>,
    }
    let mut cells: BTreeMap<(String, String), Cell> = BTreeMap::new();
    let mut undated = 0usize;
    for row in rows {
        let Some(started) = &row.started else {
            undated += 1;
            continue;
        };
        let Some(secs) = turn_epoch(started) else {
            undated += 1;
            continue;
        };
        let Some(dt) = chrono::Local.timestamp_opt(secs, 0).single() else {
            undated += 1;
            continue;
        };
        let entry = cells
            .entry((dt.format("%Y-%m-%d").to_string(), row.harness.to_string()))
            .or_default();
        entry.sessions += 1;
        entry.operator_turns += row.operator_turns.len() as u64;
        entry.tool_use += row.tool_use as u64;
        if let Some(v) = row.tokens.as_ref().map(|t| t.output) {
            *entry.output_tokens.get_or_insert(0) += v;
        }
    }
    let series = cells
        .into_iter()
        .map(|((date, harness), cell)| {
            json!({
                "date": date,
                "harness": harness,
                "sessions": cell.sessions,
                "operator_turns": cell.operator_turns,
                "tool_use": cell.tool_use,
                "output_tokens": cell.output_tokens,
            })
        })
        .collect();
    (series, undated)
}

/// The blake3 rank: the same eligible set picks the same sample every run,
/// with no seed flag and no newest-day bias.
pub(crate) fn sample_rank(session: &str) -> [u8; 32] {
    *blake3::hash(session.as_bytes()).as_bytes()
}

pub(crate) fn pick_sample(eligible: &[&str], n: Option<usize>) -> HashSet<String> {
    let mut ranked: Vec<&str> = eligible.to_vec();
    ranked.sort_by(|a, b| sample_rank(a).cmp(&sample_rank(b)).then_with(|| a.cmp(b)));
    let keep = n.unwrap_or(ranked.len()).min(ranked.len());
    ranked.into_iter().take(keep).map(str::to_string).collect()
}

/// The `--sample` flag as parsed: nothing requested, N sessions, or all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleRequest {
    None,
    N(usize),
    All,
}

/// The idle, substantive, attended rows, sampled by blake3 rank and marked
/// on the rows. Returns the `(eligible, sampled)` counts for the
/// populations block; both `None` when nothing was requested.
pub(crate) fn mark_sampled(
    rows: &mut [SessionRow],
    request: SampleRequest,
) -> (Option<usize>, Option<usize>) {
    let eligible_ids: Vec<&str> = rows
        .iter()
        .filter(|r| r.kind == "attended" && r.idle && r.substantive)
        .map(|r| r.session.as_str())
        .collect();
    let eligible = eligible_ids.len();
    match request {
        SampleRequest::None => (None, None),
        SampleRequest::All => {
            let picked = pick_sample(&eligible_ids, None);
            let n = picked.len();
            for r in rows.iter_mut() {
                r.sampled |= picked.contains(&r.session);
            }
            (Some(eligible), Some(n))
        }
        SampleRequest::N(n) => {
            let picked = pick_sample(&eligible_ids, Some(n));
            let taken = picked.len();
            for r in rows.iter_mut() {
                r.sampled |= picked.contains(&r.session);
            }
            (Some(eligible), Some(taken))
        }
    }
}

/// The gap from each witnessed operator turn back to the last assistant
/// timestamp before it, kept when 2 to 3600 s: the floor drops tool-result
/// turns, the ceiling drops overnight gaps.
pub(crate) fn response_gaps(operator_epochs: &[i64], assistant_ts: &[f64]) -> Vec<u64> {
    operator_epochs
        .iter()
        .filter_map(|op| {
            let before = assistant_ts.partition_point(|a| *a <= *op as f64);
            if before == 0 {
                return None;
            }
            let gap = *op as f64 - assistant_ts[before - 1];
            ((2.0..=3600.0).contains(&gap)).then(|| gap as u64)
        })
        .collect()
}

// --- The categories post-process: the saved fold JSON plus the skill's run
// file become per-category metrics. Nothing here reads a transcript.

#[derive(Deserialize)]
pub(crate) struct FacetKey {
    #[allow(dead_code)]
    session: String,
    mtime: u64,
    size: u64,
}

#[derive(Deserialize)]
pub(crate) struct Facet {
    key: FacetKey,
    friction: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct RunCategory {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    sessions: Vec<String>,
    #[serde(default)]
    subcategories: Vec<RunSubcategory>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct RunSubcategory {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    sessions: Vec<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Run {
    schema: u64,
    #[serde(default)]
    #[allow(dead_code)]
    question: String,
    #[serde(default)]
    #[allow(dead_code)]
    question_key: String,
    categories: Vec<RunCategory>,
}

#[derive(Deserialize)]
pub(crate) struct FoldRow {
    pub(crate) session: String,
    #[serde(default)]
    pub(crate) sampled: bool,
    pub(crate) tool_use: usize,
    #[serde(default)]
    pub(crate) commits: usize,
    pub(crate) duration_s: Option<i64>,
    pub(crate) tokens: Option<crate::session_activity::Tokens>,
    pub(crate) tool_errors: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    pub(crate) interruptions: u64,
    pub(crate) pr_number: Option<u64>,
    pub(crate) node: Option<String>,
    pub(crate) mtime: u64,
    pub(crate) size: u64,
    #[serde(default)]
    pub(crate) relay: Vec<RelayIn>,
}

#[derive(Deserialize)]
pub(crate) struct RelayIn {
    pub(crate) within_contract: bool,
}

/// The facets of the sampled rows, keyed by session id, plus every session
/// that cannot be judged with its reason. Files are parsed whole and never
/// deleted: a bad file stays on disk for its author to fix.
pub(crate) fn load_facets(
    facets_dir: &std::path::Path,
    sampled: &[FoldRow],
) -> (HashMap<String, Facet>, Vec<Value>) {
    let mut judged = HashMap::new();
    let mut unjudged = Vec::new();
    for row in sampled {
        let path = facets_dir.join(format!("{}.json", row.session));
        let reason = match std::fs::read_to_string(&path) {
            Err(_) => "missing".to_string(),
            Ok(raw) => match serde_json::from_str::<Facet>(&raw) {
                Err(err) => format!("invalid: {err}"),
                Ok(facet) => {
                    if facet.key.mtime != row.mtime || facet.key.size != row.size {
                        "stale key".to_string()
                    } else {
                        judged.insert(row.session.clone(), facet);
                        continue;
                    }
                }
            },
        };
        unjudged.push(json!({"session": row.session, "reason": reason}));
    }
    (judged, unjudged)
}

/// One run-file refusal, with the session id or field named. Nothing
/// partial ever reaches the metrics: every check runs before any output.
pub(crate) fn load_run(
    path: &std::path::Path,
    judged: &HashMap<String, Facet>,
) -> Result<Run, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))?;
    let run: Run = serde_json::from_str(&raw).map_err(|e| format!("invalid JSON: {e}"))?;
    if run.schema != 1 {
        return Err(format!("unsupported schema {}", run.schema));
    }
    let mut seen_top: HashMap<&str, &str> = HashMap::new();
    let mut categorized: HashSet<&str> = HashSet::new();
    for category in &run.categories {
        for id in &category.sessions {
            if !judged.contains_key(id) {
                return Err(format!("session {id} has no current facet"));
            }
            if let Some(_first) = seen_top.insert(id.as_str(), category.name.as_str()) {
                return Err(format!("session {id} is in two categories"));
            }
            categorized.insert(id.as_str());
        }
        let mut seen_sub: HashMap<&str, &str> = HashMap::new();
        for sub in &category.subcategories {
            for id in &sub.sessions {
                if !judged.contains_key(id) {
                    return Err(format!("session {id} has no current facet"));
                }
                if !category.sessions.contains(id) {
                    return Err(format!(
                        "session {id} is in subcategory {} but not in its parent {}",
                        sub.name, category.name
                    ));
                }
                if let Some(_first) = seen_sub.insert(id.as_str(), sub.name.as_str()) {
                    return Err(format!("session {id} is in two categories"));
                }
                categorized.insert(id.as_str());
            }
        }
    }
    for id in judged.keys() {
        if !categorized.contains(id.as_str()) {
            return Err(format!("session {id} is judged but in no category"));
        }
    }
    Ok(run)
}

fn median_of(mut values: Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort();
    Some(values[values.len() / 2])
}

/// Per-category and per-subcategory metrics over the sampled rows. Every
/// number traces to the saved fold JSON or the facets; the fold's friction
/// string is counted as written, with no copy of the six words here.
pub(crate) fn category_metrics(
    run: &Run,
    rows: &[FoldRow],
    facets: &HashMap<String, Facet>,
    merged_nodes: &HashMap<String, bool>,
) -> Vec<Value> {
    let by_id: HashMap<&str, &FoldRow> = rows.iter().map(|r| (r.session.as_str(), r)).collect();
    let judged = by_id.len();
    let share = |count: usize, base: usize| -> Value {
        if base == 0 {
            json!(0.0)
        } else {
            json!((count as f64 * 1000.0 / base as f64).round() / 10.0)
        }
    };
    let metrics = |ids: &[String], base: usize| -> Value {
        let members: Vec<&FoldRow> = ids
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect();
        let tool_counts: Vec<u64> = members.iter().map(|r| r.tool_use as u64).collect();
        let tokens_output: Option<u64> = {
            let outs: Vec<u64> = members
                .iter()
                .filter_map(|r| r.tokens.as_ref().map(|t| t.output))
                .collect();
            if outs.len() == members.len() && !members.is_empty() {
                Some(outs.iter().sum())
            } else {
                None
            }
        };
        let with_pr = members.iter().filter(|r| r.pr_number.is_some()).count();
        let merged = members
            .iter()
            .filter(|r| {
                r.node
                    .as_deref()
                    .and_then(|n| merged_nodes.get(n))
                    .copied()
                    .unwrap_or(false)
            })
            .count();
        let mut friction: BTreeMap<String, u64> = BTreeMap::new();
        let mut tool_errors_total = 0u64;
        let mut interruptions_total = 0u64;
        let mut relay_breaches = 0u64;
        for r in &members {
            if let Some(f) = facets.get(&r.session) {
                *friction.entry(f.friction.clone()).or_insert(0) += 1;
            }
            if let Some(errs) = &r.tool_errors {
                tool_errors_total += errs.values().sum::<u64>();
            }
            interruptions_total += r.interruptions;
            relay_breaches += r.relay.iter().filter(|f| !f.within_contract).count() as u64;
        }
        let durations: Vec<u64> = members
            .iter()
            .filter_map(|r| r.duration_s.map(|d| d.max(0) as u64))
            .collect();
        json!({
            "sessions": members.len(),
            "share_pct": share(members.len(), base),
            "tool_use": {"total": tool_counts.iter().sum::<u64>(),
                         "median": median_of(tool_counts)},
            "commits": members.iter().map(|r| r.commits as u64).sum::<u64>(),
            "duration_s": {"median": median_of(durations)},
            "tokens": {"output": tokens_output},
            "tool_errors": tool_errors_total,
            "interruptions": interruptions_total,
            "prs": {"with_pr": with_pr, "merged": merged},
            "relay_breaches": relay_breaches,
            "friction": friction,
        })
    };
    let mut items = Vec::new();
    for category in &run.categories {
        let base = category.sessions.len();
        let subs: Vec<Value> = category
            .subcategories
            .iter()
            .map(|sub| {
                json!({
                    "name": sub.name,
                    "description": sub.description,
                    "sessions": sub.sessions.len(),
                    "share_pct": share(sub.sessions.len(), base),
                    "metrics": metrics(&sub.sessions, base),
                })
            })
            .collect();
        items.push(json!({
            "name": category.name,
            "description": category.description,
            "metrics": metrics(&category.sessions, judged),
            "subcategories": subs,
        }));
    }
    items.sort_by(|a, b| {
        let count = |v: &Value| v["metrics"]["sessions"].as_u64().unwrap_or(0);
        count(b).cmp(&count(a)).then_with(|| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        })
    });
    items
}

/// Merge state for the post-process: node id -> merged. A read error leaves
/// the map empty and names itself in the output's `merge_state`.
pub(crate) fn merged_nodes() -> (HashMap<String, bool>, Option<String>) {
    match crate::graph_store::read_rows(&crate::graph_get::default_graph_path()) {
        Ok(rows) => {
            let mut map = HashMap::new();
            for row in rows {
                let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let merged = row.get("merge_status").and_then(|v| v.as_str()) == Some("merged");
                if !id.is_empty() {
                    map.insert(id.to_string(), merged);
                }
            }
            (map, None)
        }
        Err(err) => (HashMap::new(), Some(format!("unavailable: {err}"))),
    }
}

/// The categories block the post-process appends to the saved fold JSON.
pub(crate) fn categories_block(
    question_key: &str,
    judged: usize,
    unjudged: Vec<Value>,
    items: Vec<Value>,
    merge_state: Option<String>,
) -> Value {
    let mut block = json!({
        "question_key": question_key,
        "judged": judged,
        "unjudged": unjudged,
        "items": items,
    });
    if let Some(state) = merge_state {
        block["merge_state"] = json!(state);
    }
    block
}

/// The `--categories <run> --fold <saved fold JSON>` post-process: reads no
/// transcript, prints the input fold JSON unchanged with a `categories`
/// block added and `populations.judged` set. Refusals exit 2 and name the
/// session id or field.
pub(crate) fn run_categories(run_path: &str, fold_path: &str, facets_dir: &std::path::Path) -> i32 {
    let raw = match std::fs::read_to_string(fold_path) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("fno-agents intel: --fold {fold_path}: unreadable: {err}");
            return 2;
        }
    };
    let mut doc: Value = match serde_json::from_str(&raw) {
        Ok(doc) => doc,
        Err(err) => {
            eprintln!("fno-agents intel: --fold {fold_path}: invalid JSON: {err}");
            return 2;
        }
    };
    if !doc.get("sample").is_some_and(|s| s.is_object()) {
        eprintln!("fno-agents intel: --fold {fold_path} was folded without --sample");
        return 2;
    }
    let Some(sessions) = doc.get("sessions").and_then(|s| s.as_array()) else {
        eprintln!("fno-agents intel: --fold {fold_path}: no sessions array");
        return 2;
    };
    let rows: Vec<FoldRow> = sessions
        .iter()
        .filter_map(|s| serde_json::from_value(s.clone()).ok())
        .collect();
    let sampled: Vec<FoldRow> = rows.into_iter().filter(|r| r.sampled).collect();
    let (facets, unjudged) = load_facets(facets_dir, &sampled);
    let run = match load_run(std::path::Path::new(run_path), &facets) {
        Ok(run) => run,
        Err(reason) => {
            eprintln!("fno-agents intel: --categories {run_path}: {reason}");
            return 2;
        }
    };
    let (merged, merge_err) = merged_nodes();
    let items = category_metrics(&run, &sampled, &facets, &merged);
    let judged = facets.len();
    doc["categories"] = categories_block(&run.question_key, judged, unjudged, items, merge_err);
    doc["populations"]["judged"] = json!(judged);
    println!(
        "{}",
        serde_json::to_string(&doc).unwrap_or_else(|_| "{}".to_string())
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::SessionRow;

    fn row(session: &str, harness: &'static str) -> SessionRow {
        let mut r = SessionRow::test_row(session, harness);
        r.kind = "attended".to_string();
        r
    }

    fn counters(pairs: &[(&'static str, u64)]) -> BTreeMap<&'static str, u64> {
        crate::provenance::Provenance::all_labels()
            .into_iter()
            .map(|l| (l, 0u64))
            .collect::<BTreeMap<&'static str, u64>>()
            .into_iter()
            .chain(pairs.iter().map(|(k, v)| (*k, *v)))
            .collect()
    }

    #[test]
    fn keepalive_only_and_self_rows_are_dropped() {
        // AC5-HP
        let mut keepalive = row("ka", "claude");
        keepalive.counters = counters(&[("keepalive", 4), ("harness_stop_hook", 1)]);
        let mut noisy = row("noisy", "claude");
        noisy.counters = counters(&[("keepalive", 1), ("unknown", 2)]);
        let mut own = row("own-session", "claude");
        own.counters = counters(&[("operator", 1)]);
        let rows = vec![keepalive, noisy, own];
        let (kept, dropped) = drop_rows(rows, Some("own-session"));
        assert_eq!(dropped.keepalive_only, 1);
        assert_eq!(dropped.self_dropped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session, "noisy");
    }

    #[test]
    fn substantive_needs_two_turns_and_a_minute() {
        // AC6-HP
        assert!(!is_substantive(1, Some(59)));
        assert!(!is_substantive(1, Some(60)));
        assert!(!is_substantive(2, Some(59)));
        assert!(is_substantive(2, Some(60)));
        assert!(!is_substantive(2, None));
    }

    #[test]
    fn response_time_buckets_read_the_gaps() {
        // AC7-EDGE, bucket half: 40s lands in 30-60s.
        let mut r = row("s1", "claude");
        r.response_s = vec![1, 40, 7200];
        let rt = response_time(&[r]);
        assert_eq!(rt["n"], 3);
        assert_eq!(rt["buckets"]["30-60s"], 1);
    }

    #[test]
    fn interleaved_turns_inside_the_window_overlap_once() {
        // AC8-HP
        let mut a = row("aaaa", "claude");
        a.operator_turns = vec!["2026-09-16T12:00:00Z".into(), "2026-09-16T12:10:00Z".into()];
        let mut b = row("bbbb", "codex");
        b.operator_turns = vec!["2026-09-16T12:05:00Z".into()];
        let mut c = row("cccc", "codex");
        c.operator_turns = vec!["2026-09-16T13:00:00Z".into()];
        let p = parallel(&[a, b, c]);
        assert_eq!(p["overlap_pairs"], 1);
        assert_eq!(p["sessions"], 2);
    }

    #[test]
    fn daily_series_sorts_by_date_and_harness() {
        // AC9-HP
        let mut c1 = row("c1", "claude");
        c1.started = Some("2026-09-16T12:00:00Z".into());
        let mut c2 = row("c2", "claude");
        c2.started = Some("2026-09-16T23:30:00Z".into());
        let mut x1 = row("x1", "codex");
        x1.started = Some("2026-09-17T08:00:00Z".into());
        let mut nodate = row("nd", "claude");
        nodate.started = None;
        let (series, undated) = daily(&[x1, c1, c2, nodate]);
        assert_eq!(undated, 1);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0]["date"], "2026-09-16");
        assert_eq!(series[0]["harness"], "claude");
        assert_eq!(series[0]["sessions"], 2);
        assert_eq!(series[1]["date"], "2026-09-17");
        assert_eq!(series[1]["harness"], "codex");
        assert_eq!(series[1]["sessions"], 1);
    }

    #[test]
    fn output_tokens_read_null_only_when_every_row_is_unmeasured() {
        let mut measured = row("m", "claude");
        measured.started = Some("2026-09-16T12:00:00Z".into());
        measured.tokens = Some(crate::session_activity::Tokens {
            input: 1,
            output: 30,
            cache_read: 0,
            cache_write: 0,
        });
        let mut unmeasured = row("u", "claude");
        unmeasured.started = Some("2026-09-16T13:00:00Z".into());
        let (series, _) = daily(&[measured, unmeasured]);
        assert_eq!(series[0]["output_tokens"], 30);
        let mut solo = row("solo", "opencode");
        solo.started = Some("2026-09-16T14:00:00Z".into());
        let (series2, _) = daily(&[solo]);
        assert!(series2[0]["output_tokens"].is_null());
    }

    #[test]
    fn the_sample_is_stable_across_input_orders() {
        // AC10-HP
        let ids = ["c", "a", "b", "e", "d"];
        let first = pick_sample(&ids, Some(3));
        let second = pick_sample(&["d", "b", "e", "a", "c"], Some(3));
        assert_eq!(first, second);
        assert_eq!(first.len(), 3);
        let mut all: Vec<&str> = ids.to_vec();
        all.sort_by_key(|id| sample_rank(id));
        for kept in &first {
            assert!(all[..3].contains(&kept.as_str()));
        }
    }

    #[test]
    fn oversized_and_open_ended_samples_take_everything() {
        // AC11-EDGE
        let ids = vec!["x", "y"];
        assert_eq!(pick_sample(&ids, Some(5)).len(), 2);
        assert_eq!(pick_sample(&ids, None).len(), 2);
    }

    #[test]
    fn response_gaps_keep_only_the_2_to_3600_window() {
        // AC7-EDGE, fold half: 1 s and 7200 s drop, 40 s stays.
        let t0 = 1_800_000_000.0f64;
        let assistant = vec![t0];
        let operators = vec![t0 as i64 + 1, t0 as i64 + 40, t0 as i64 + 7200];
        let gaps = response_gaps(&operators, &assistant);
        assert_eq!(gaps, vec![40]);
    }

    #[test]
    fn mark_sampled_picks_only_idle_substantive_attended_rows() {
        // AC13-HP
        let mut early = row("early", "claude");
        early.kind = "attended".to_string();
        early.substantive = true;
        early.idle = false; // 10 s old
        let mut ready = row("ready", "claude");
        ready.kind = "attended".to_string();
        ready.substantive = true;
        ready.idle = true; // 3600 s old
        let mut thin = row("thin", "codex");
        thin.kind = "attended".to_string();
        thin.substantive = false;
        thin.idle = true;
        let mut rows = vec![early, ready, thin];
        let (eligible, sampled) = mark_sampled(&mut rows, SampleRequest::All);
        assert_eq!(eligible, Some(1));
        assert_eq!(sampled, Some(1));
        assert!(!rows[0].sampled);
        assert!(rows[1].sampled);
        assert!(!rows[2].sampled);
        // No request: nothing marked, null counts.
        let mut rows2 = vec![row("r", "claude")];
        let (eligible2, sampled2) = mark_sampled(&mut rows2, SampleRequest::None);
        assert_eq!(eligible2, None);
        assert_eq!(sampled2, None);
        assert!(!rows2[0].sampled);
    }

    fn write_facet(dir: &std::path::Path, sid: &str, mtime: u64, size: u64, friction: &str) {
        let body = json!({
            "key": {"session": sid, "mtime": mtime, "size": size},
            "friction": friction,
        });
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{sid}.json")), body.to_string()).unwrap();
    }

    fn fold_row(sid: &str) -> FoldRow {
        serde_json::from_str::<FoldRow>(
            &json!({
                "harness": "claude", "session": sid, "sampled": true,
                "tool_use": 3, "commits": 1, "duration_s": 120,
                "tokens": {"input": 10, "output": 20, "cache_read": 0, "cache_write": 0},
                "tool_errors": {"command_failed": 1},
                "interruptions": 2, "pr_number": 7, "node": serde_json::Value::Null,
                "mtime": 100, "size": 200, "relay": []
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn facets_load_by_key_and_name_their_failures() {
        // AC17-EDGE
        let dir = std::env::temp_dir().join(format!("fno-ins-facets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_facet(&dir, "fresh", 100, 200, "tool_failure");
        write_facet(&dir, "stale", 999, 200, "tool_failure");
        std::fs::write(
            dir.join("bad.json"),
            "{\"key\": {\"session\": \"bad\", \"mtime\": 100, \"size\": 200}, \"friction\": \"x\"}\ntrailing prose",
        )
        .unwrap();
        let rows = vec![fold_row("fresh"), fold_row("stale"), fold_row("bad")];
        let (judged, unjudged) = load_facets(&dir, &rows);
        assert_eq!(judged.len(), 1);
        assert!(judged.contains_key("fresh"));
        let stale = unjudged.iter().find(|u| u["session"] == "stale").unwrap();
        assert_eq!(stale["reason"], "stale key");
        let bad = unjudged.iter().find(|u| u["session"] == "bad").unwrap();
        assert!(bad["reason"].as_str().unwrap().starts_with("invalid:"));
        assert!(dir.join("stale.json").exists());
        assert!(dir.join("bad.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn run_file(categories: Value) -> String {
        json!({"schema": 1, "question": "q", "question_key": "ab12cd34", "categories": categories})
            .to_string()
    }

    #[test]
    fn the_run_file_refusals_name_the_fault() {
        // AC15-ERR, AC16-ERR
        let dir = std::env::temp_dir().join(format!("fno-ins-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let facet: Facet = serde_json::from_str(
            &json!({"key": {"session": "s1", "mtime": 1, "size": 2}, "friction": "tool_failure"})
                .to_string(),
        )
        .unwrap();
        let mut facet_map: HashMap<String, Facet> = HashMap::new();
        facet_map.insert("s1".to_string(), facet);
        let missing = dir.join("missing.json");
        std::fs::write(
            &missing,
            run_file(json!([{"name": "C", "sessions": ["ghost"]}])),
        )
        .unwrap();
        let err = load_run(&missing, &facet_map).unwrap_err();
        assert!(
            err.contains("ghost") && err.contains("no current facet"),
            "err: {err}"
        );
        let dup = dir.join("dup.json");
        std::fs::write(
            &dup,
            run_file(json!([
                {"name": "C1", "sessions": ["s1"]},
                {"name": "C2", "sessions": ["s1"]}
            ])),
        )
        .unwrap();
        let err = load_run(&dup, &facet_map).unwrap_err();
        assert!(err.contains("s1 is in two categories"));
        let uncovered = dir.join("uncovered.json");
        std::fs::write(&uncovered, run_file(json!([{"name": "C", "sessions": []}]))).unwrap();
        let err = load_run(&uncovered, &facet_map).unwrap_err();
        assert!(err.contains("s1 is judged but in no category"));
        let schema2 = dir.join("schema2.json");
        std::fs::write(
            &schema2,
            run_file(json!([])).replacen("\"schema\":1", "\"schema\":2", 1),
        )
        .unwrap();
        let err = load_run(&schema2, &facet_map).unwrap_err();
        assert!(err.contains("unsupported schema 2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn category_metrics_match_hand_computed_values() {
        // AC14-HP
        let rows = vec![fold_row("a1"), fold_row("a2"), fold_row("a3"), {
            let mut r = fold_row("b1");
            r.node = Some("n-merged".to_string());
            r.tool_errors = Some(BTreeMap::from([("command_failed".to_string(), 2)]));
            r.relay = vec![RelayIn {
                within_contract: false,
            }];
            r.tokens = None;
            r
        }];
        let facets: HashMap<String, Facet> = ["a1", "a2", "a3", "b1"]
            .iter()
            .map(|sid| {
                (
                    sid.to_string(),
                    serde_json::from_str::<Facet>(
                        &json!({"key": {"session": sid, "mtime": 100, "size": 200},
                                "friction": "tool_failure"})
                        .to_string(),
                    )
                    .unwrap(),
                )
            })
            .collect();
        let run: Run = serde_json::from_str(
            &run_file(json!([
                {"name": "Big", "sessions": ["a1", "a2", "a3"],
                 "subcategories": [{"name": "Sub", "sessions": ["a1"]}]},
                {"name": "Small", "sessions": ["b1"]},
            ]))
            .to_string(),
        )
        .unwrap();
        let merged: HashMap<String, bool> = [("n-merged".to_string(), true)].into_iter().collect();
        let items = category_metrics(&run, &rows, &facets, &merged);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["name"], "Big");
        let big = &items[0]["metrics"];
        assert_eq!(big["sessions"], 3);
        assert_eq!(big["share_pct"], 75.0);
        assert_eq!(big["tool_use"]["total"], 9);
        assert_eq!(big["tool_use"]["median"], 3);
        assert_eq!(big["commits"], 3);
        assert_eq!(big["duration_s"]["median"], 120);
        assert_eq!(big["tokens"]["output"], 60);
        assert_eq!(big["tool_errors"], 3);
        assert_eq!(big["interruptions"], 6);
        assert_eq!(big["prs"]["with_pr"], 3);
        assert_eq!(big["prs"]["merged"], 0);
        assert_eq!(big["relay_breaches"], 0);
        assert_eq!(big["friction"]["tool_failure"], 3);
        let small = &items[1]["metrics"];
        assert_eq!(small["share_pct"], 25.0);
        assert!(small["tokens"]["output"].is_null());
        assert_eq!(small["prs"]["merged"], 1);
        assert_eq!(small["relay_breaches"], 1);
        assert_eq!(small["tool_errors"], 2);
        let sub = &items[0]["subcategories"][0];
        assert_eq!(sub["share_pct"], 33.3);
    }
}
