//! The insights layer over the fold's rows: the two drops, the substantive
//! gate, the populations block, the per-day and activity series, the
//! idle-gated blake3 sample, and the categories post-process. Pure functions
//! over [`crate::intel::SessionRow`] (at fold time) and over the saved fold
//! JSON (post-process). No transcript is read here, and no model runs here.

use crate::intel::SessionRow;
use chrono::{TimeZone, Timelike};
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
    // One stamp parser for every series: the fold's ts_secs, so a naive
    // stamp counts in hours and daily exactly where it counts in response
    // time.
    crate::intel::ts_secs(raw).map(|s| s as i64)
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
            // Two turns in one epoch second invert the range; an empty gap
            // overlaps nothing.
            if start >= end {
                continue;
            }
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

/// The idle, substantive, attended rows, sampled with witnessed-turn
/// sessions first and the rest by blake3 rank, marked on the rows. Returns
/// the `(eligible, sampled)` counts for the populations block; both `None`
/// when nothing was requested.
pub(crate) fn mark_sampled(
    rows: &mut [SessionRow],
    request: SampleRequest,
) -> (Option<usize>, Option<usize>) {
    let mut eligible_ids: Vec<&str> = rows
        .iter()
        .filter(|r| r.kind == "attended" && r.idle && r.substantive)
        .map(|r| r.session.as_str())
        .collect();
    // A codex resume writes several rollout files under one thread id; the
    // sample ranks sessions, not files.
    eligible_ids.sort_unstable();
    eligible_ids.dedup();
    let eligible = eligible_ids.len();
    // A hash rank over a population where most sessions hold no user speech
    // spends the sample on silent rows. Witnessed-turn sessions jump the
    // rank; the hash fills what is left.
    let witnessed: HashSet<&str> = rows
        .iter()
        .filter(|r| !r.operator_turns.is_empty())
        .map(|r| r.session.as_str())
        .collect();
    let (witnessed_ids, silent): (Vec<&str>, Vec<&str>) = eligible_ids
        .into_iter()
        .partition(|id| witnessed.contains(id));
    let mut picked: HashSet<String> = witnessed_ids.iter().map(|s| s.to_string()).collect();
    match request {
        SampleRequest::None => (None, None),
        SampleRequest::All => {
            picked.extend(pick_sample(&silent, None));
            let n = picked.len();
            for r in rows.iter_mut() {
                r.sampled |= picked.contains(&r.session);
            }
            (Some(eligible), Some(n))
        }
        SampleRequest::N(n) => {
            let fill = n.saturating_sub(picked.len());
            picked.extend(pick_sample(&silent, Some(fill)));
            let taken = picked.len();
            for r in rows.iter_mut() {
                r.sampled |= picked.contains(&r.session);
            }
            (Some(eligible), Some(taken))
        }
    }
}

/// Live sessions with witnessed operator turns the idle rule held out of
/// the sample: what judging could not see. One entry per session, the max
/// witnessed-turn count across a session's rollout files.
pub(crate) fn held_out_live(rows: &[SessionRow]) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for r in rows
        .iter()
        .filter(|r| !r.idle && !r.operator_turns.is_empty())
    {
        match out.iter_mut().find(|(s, _)| s == &r.session) {
            Some((_, n)) => *n = (*n).max(r.operator_turns.len()),
            None => out.push((r.session.clone(), r.operator_turns.len())),
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
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
    #[serde(default)]
    suggestions: Vec<SuggestionIn>,
}

/// One proposed fix: what the friction is, where it showed, and the open
/// node carrying the fix. The post-process validates it and stamps the
/// scored metric's baseline; the next run's `--score-prior` reads that back.
#[derive(Deserialize, Debug)]
pub(crate) struct SuggestionIn {
    pub(crate) friction: String,
    pub(crate) cause: String,
    pub(crate) example_sessions: Vec<String>,
    #[serde(default)]
    pub(crate) evidence: String,
    /// Event types that prove the friction (hook_blocked, spawn_refused,
    /// help_emitted, ...). Each must exist in the fold's events block; the
    /// stamped copy carries that block's counts.
    #[serde(default)]
    pub(crate) events: Vec<String>,
    pub(crate) fix: FixIn,
    /// The open backlog node the fix is filed or matched to.
    pub(crate) node: String,
    /// The scored metric: `friction:<word>`, `tool_errors`, `interruptions`,
    /// `unanswered`, `undelivered`, or `unjudged`. Counts: lower is better.
    pub(crate) metric: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct FixIn {
    /// law | hook | config | node
    pub(crate) kind: String,
    pub(crate) target: String,
    /// The copy-ready artifact: the law text, the hook row, the config
    /// line, or the node title with its one-line ask.
    pub(crate) text: String,
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
    // One verdict per session: a resumed codex thread leaves several rollout
    // rows under one id, and a session judged through any one of them is
    // judged, never also stranded.
    let mut judged: HashMap<String, Facet> = HashMap::new();
    let mut reasons: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for row in sampled {
        let path = facets_dir.join(format!("{}.json", row.session));
        let reason = match std::fs::read_to_string(&path) {
            Err(_) => "missing".to_string(),
            Ok(raw) => match serde_json::from_str::<Facet>(&raw) {
                Err(err) => format!("invalid: {err}"),
                Ok(facet) => {
                    if facet.key.mtime == row.mtime && facet.key.size == row.size {
                        judged.insert(row.session.clone(), facet);
                        continue;
                    }
                    "stale key".to_string()
                }
            },
        };
        if !reasons.contains_key(&row.session) {
            order.push(row.session.clone());
        }
        reasons.entry(row.session.clone()).or_insert(reason);
    }
    let unjudged = order
        .into_iter()
        .filter(|session| !judged.contains_key(session))
        .map(|session| json!({"session": session, "reason": reasons[&session]}))
        .collect();
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

/// Merge state and status for the post-process: node id -> merged, node id
/// -> status. A read error leaves both maps empty and names itself in the
/// returned error.
pub(crate) fn graph_maps() -> (
    HashMap<String, bool>,
    HashMap<String, String>,
    Option<String>,
) {
    match crate::graph_store::read_rows(&crate::graph_get::default_graph_path()) {
        Ok(rows) => {
            let mut merged = HashMap::new();
            let mut statuses = HashMap::new();
            for row in rows {
                let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                merged.insert(
                    id.to_string(),
                    row.get("merge_status").and_then(|v| v.as_str()) == Some("merged"),
                );
                if let Some(status) = row.get("status").and_then(|v| v.as_str()) {
                    statuses.insert(id.to_string(), status.to_string());
                }
            }
            (merged, statuses, None)
        }
        Err(err) => (
            HashMap::new(),
            HashMap::new(),
            Some(format!("unavailable: {err}")),
        ),
    }
}

/// Resolve one metric name against the post-processed fold document.
/// Closed namespace; every name is a count where lower is better.
pub(crate) fn resolve_metric(name: &str, doc: &Value) -> Option<u64> {
    if let Some(word) = name.strip_prefix("friction:") {
        return doc["categories"]["items"].as_array().map(|items| {
            items
                .iter()
                .filter_map(|it| it["metrics"]["friction"][word].as_u64())
                .sum()
        });
    }
    match name {
        "tool_errors" => doc["activity"]["tool_errors"]
            .as_object()
            .map(|m| m.values().filter_map(|v| v.as_u64()).sum()),
        "interruptions" => doc["activity"]["interruptions"].as_u64(),
        "unanswered" => doc["totals"]["unanswered"].as_u64(),
        "undelivered" => doc["totals"]["undelivered"].as_u64(),
        "unjudged" => doc["categories"]["unjudged"]
            .as_array()
            .map(|a| a.len() as u64),
        _ => None,
    }
}

/// Validate the run file's suggestions against the judged facets and the
/// graph's open nodes, and stamp each with its metric baseline. The refusal
/// names the suggestion, the session, the fix kind, or the node. A graph
/// read error skips the node checks and names itself in `node_state`.
pub(crate) fn stamp_suggestions(
    run: &Run,
    facets: &HashMap<String, Facet>,
    statuses: &HashMap<String, String>,
    status_err: &Option<String>,
    doc: &Value,
) -> Result<Vec<Value>, String> {
    const FIX_KINDS: [&str; 4] = ["law", "hook", "config", "node"];
    const CLOSED: [&str; 2] = ["done", "superseded"];
    let mut out = Vec::new();
    for (i, s) in run.suggestions.iter().enumerate() {
        let label = format!("suggestion {} ({})", i + 1, s.friction);
        if s.example_sessions.is_empty() {
            return Err(format!("{label}: no example session named"));
        }
        for example in &s.example_sessions {
            let facet = facets
                .get(example)
                .ok_or_else(|| format!("{label}: example session {example} is not judged"))?;
            if facet.friction != s.friction {
                return Err(format!(
                    "{label}: example session {example} is judged {}, not {}",
                    facet.friction, s.friction
                ));
            }
        }
        if !FIX_KINDS.contains(&s.fix.kind.as_str()) {
            return Err(format!(
                "{label}: fix kind {} (known: law, hook, config, node)",
                s.fix.kind
            ));
        }
        let node_state = match status_err {
            Some(err) => err.clone(),
            None => match statuses.get(&s.node) {
                None => {
                    return Err(format!(
                        "{label}: node {} is not in the graph; file it (fno backlog idea) or name an open node",
                        s.node
                    ))
                }
                Some(status) if CLOSED.contains(&status.as_str()) => {
                    return Err(format!(
                        "{label}: node {} is {}, not open; file or match an open node",
                        s.node, status
                    ))
                }
                Some(status) => status.clone(),
            },
        };
        let Some(baseline) = resolve_metric(&s.metric, doc) else {
            return Err(format!(
                "{label}: metric {} does not resolve (known: friction:<word>, tool_errors, interruptions, unanswered, undelivered, unjudged)",
                s.metric
            ));
        };
        let mut event_counts = serde_json::Map::new();
        for ev in &s.events {
            let count = doc["events"]
                .get(ev)
                .and_then(|c| c.as_u64())
                .ok_or_else(|| format!("{label}: event {ev} is not in the fold's events block"))?;
            event_counts.insert(ev.clone(), json!(count));
        }
        // Events carry no project or session selector: whatever the fold's
        // scope, the counts are machine-global, so the stamp names it.
        let events_scope = doc["events_scope"].as_str().unwrap_or("machine-global");
        out.push(json!({
            "friction": s.friction,
            "cause": s.cause,
            "example_sessions": s.example_sessions,
            "evidence": s.evidence,
            "events": event_counts,
            "events_scope": events_scope,
            "fix": {"kind": s.fix.kind, "target": s.fix.target, "text": s.fix.text},
            "node": s.node,
            "node_state": node_state,
            "metric": s.metric,
            "baseline": baseline,
        }));
    }
    Ok(out)
}

/// Whether the prior report describes the same population as the current
/// document: same window, scope, sample request, and question. A changed
/// period, project set, sample size, or question makes the raw counts
/// incomparable, and every verdict would lie.
pub(crate) fn populations_comparable(prior: &Value, doc: &Value) -> bool {
    prior["days"].as_u64() == doc["days"].as_u64()
        && prior["scope"] == doc["scope"]
        && prior["sample"]["requested"] == doc["sample"]["requested"]
        && prior["categories"]["question_key"] == doc["categories"]["question_key"]
}

/// The next run's verdicts over the prior report's stamped suggestions:
/// per suggestion, the prior baseline against the current value of the
/// same metric. Counts read lower is better. An incompatible prior
/// population (scope, period, sample, or question changed) reads
/// unmeasured, never moved.
pub(crate) fn scorecard(prior: &Value, doc: &Value) -> Vec<Value> {
    let Some(sugs) = prior.get("suggestions").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let comparable = populations_comparable(prior, doc);
    sugs.iter()
        .map(|s| {
            let metric = s["metric"].as_str().unwrap_or("");
            let current = if comparable {
                resolve_metric(metric, doc)
            } else {
                None
            };
            let verdict = match (s["baseline"].as_u64(), current) {
                (Some(p), Some(c)) if c < p => "moved",
                (Some(p), Some(c)) if c > p => "worse",
                (Some(_), Some(_)) => "unchanged",
                _ => "unmeasured",
            };
            let mut row = json!({
                "friction": s["friction"],
                "node": s["node"],
                "metric": s["metric"],
                "prior": s["baseline"],
                "current": current,
                "verdict": verdict,
            });
            if !comparable {
                row["reason"] =
                    json!("prior report population differs (scope, period, sample, or question)");
            }
            row
        })
        .collect()
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
/// block added, the run file's suggestions validated and baseline-stamped,
/// and `populations.judged` set. `--score-prior` adds a `scorecard` block
/// over the prior report's suggestions. Refusals exit 2 and name the
/// session id or field.
pub(crate) fn run_categories(
    run_path: &str,
    fold_path: &str,
    facets_dir: &std::path::Path,
    score_prior: Option<&str>,
) -> i32 {
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
    if !doc.get("sample").is_some_and(|f| f.is_object()) {
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
    let (merged, statuses, graph_err) = graph_maps();
    let items = category_metrics(&run, &sampled, &facets, &merged);
    let judged = facets.len();
    doc["categories"] = categories_block(
        &run.question_key,
        judged,
        unjudged,
        items,
        graph_err.clone(),
    );
    doc["populations"]["judged"] = json!(judged);
    // Baselines resolve against the fully stamped document, so the
    // suggestions block lands after categories and before the scorecard
    // resolves the same names against it.
    let stamped = match stamp_suggestions(&run, &facets, &statuses, &graph_err, &doc) {
        Ok(stamped) => stamped,
        Err(reason) => {
            eprintln!("fno-agents intel: --categories {run_path}: {reason}");
            return 2;
        }
    };
    doc["suggestions"] = json!(stamped);
    if let Some(prior_path) = score_prior {
        let prior_raw = match std::fs::read_to_string(prior_path) {
            Ok(raw) => raw,
            Err(err) => {
                eprintln!("fno-agents intel: --score-prior {prior_path}: unreadable: {err}");
                return 2;
            }
        };
        let prior: Value = match serde_json::from_str(&prior_raw) {
            Ok(prior) => prior,
            Err(err) => {
                eprintln!("fno-agents intel: --score-prior {prior_path}: invalid JSON: {err}");
                return 2;
            }
        };
        doc["scorecard"] = json!({"prior": prior_path, "verdicts": scorecard(&prior, &doc)});
    }
    // The judgment summary: every aggregate the report and the renderer
    // read, minus the per-session bulk. The raw rows stay in the sidecar
    // (.fold.json) beside the report.
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("sessions");
        obj.remove("nodes");
    }
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
    fn same_second_turns_scan_an_empty_gap() {
        // Two operator turns in one epoch second invert the partitioned
        // range; the pair must scan nothing, not panic.
        let mut a = row("aaaa", "claude");
        a.operator_turns = vec!["2026-09-16T12:00:00Z".into(), "2026-09-16T12:00:00Z".into()];
        let mut b = row("bbbb", "codex");
        b.operator_turns = vec!["2026-09-16T12:05:00Z".into()];
        let p = parallel(&[a, b]);
        assert_eq!(p["overlap_pairs"], 0);
        assert_eq!(p["sessions"], 0);
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
    fn mark_sampled_ranks_unique_sessions_not_files() {
        // A codex resume leaves several rollout rows under one thread id;
        // duplicates collapse before the rank.
        let mut a1 = row("aaaa", "codex");
        a1.kind = "attended".to_string();
        a1.substantive = true;
        a1.idle = true;
        let mut a2 = row("aaaa", "codex");
        a2.kind = "attended".to_string();
        a2.substantive = true;
        a2.idle = true;
        let mut b = row("bbbb", "claude");
        b.kind = "attended".to_string();
        b.substantive = true;
        b.idle = true;
        let mut rows = vec![a1, a2, b];
        let (eligible, sampled) = mark_sampled(&mut rows, SampleRequest::All);
        assert_eq!(eligible, Some(2));
        assert_eq!(sampled, Some(2));
        assert!(rows[0].sampled && rows[1].sampled);
        // held_out_live: live-with-turns rows only, rollout files merge at
        // max, sorted by count.
        let mut live = row("live", "claude");
        live.idle = false;
        live.operator_turns = vec!["t1".to_string(), "t2".to_string()];
        let mut live2 = row("live2", "codex");
        live2.idle = false;
        live2.operator_turns = vec!["t1".to_string()];
        let mut idle_with_turns = row("idleturns", "claude");
        idle_with_turns.idle = true;
        idle_with_turns.operator_turns = vec!["t1".to_string()];
        let mut live_silent = row("livesilent", "claude");
        live_silent.idle = false;
        let mut rollout_a = row("rollout", "codex");
        rollout_a.idle = false;
        rollout_a.operator_turns = vec!["t1".to_string(), "t2".to_string(), "t3".to_string()];
        let mut rollout_b = row("rollout", "codex");
        rollout_b.idle = false;
        rollout_b.operator_turns = vec!["t1".to_string()];
        let rows4 = vec![
            live,
            live2,
            idle_with_turns,
            live_silent,
            rollout_a,
            rollout_b,
        ];
        let out = held_out_live(&rows4);
        assert_eq!(
            out,
            vec![
                ("rollout".to_string(), 3),
                ("live".to_string(), 2),
                ("live2".to_string(), 1),
            ]
        );
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
        // Witnessed-turn sessions sample first; the hash fills the rest.
        let mut loud = row("loud", "claude");
        loud.substantive = true;
        loud.idle = true;
        loud.operator_turns = vec!["2026-09-28T10:00:00Z".to_string()];
        let mut silent = row("silent", "claude");
        silent.substantive = true;
        silent.idle = true;
        let mut rows3 = vec![loud, silent];
        // N(1): the witnessed-turn session wins regardless of hash rank.
        let (eligible3, sampled3) = mark_sampled(&mut rows3, SampleRequest::N(1));
        assert_eq!(eligible3, Some(2));
        assert_eq!(sampled3, Some(1));
        assert!(rows3.iter().find(|r| r.session == "loud").unwrap().sampled);
        assert!(
            !rows3
                .iter()
                .find(|r| r.session == "silent")
                .unwrap()
                .sampled
        );
        // All: both picked.
        let (eligible4, sampled4) = mark_sampled(&mut rows3, SampleRequest::All);
        assert_eq!(eligible4, Some(2));
        assert_eq!(sampled4, Some(2));
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

    #[test]
    fn a_session_judged_through_one_rollout_row_is_never_also_stranded() {
        let dir = std::env::temp_dir().join(format!("fno-ins-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_facet(&dir, "resumed", 100, 200, "tool_failure");
        let mut stale = fold_row("resumed");
        stale.mtime = 999;
        let mut fresh = fold_row("resumed");
        fresh.mtime = 100;
        let (judged, unjudged) = load_facets(&dir, &[stale, fresh]);
        assert_eq!(judged.len(), 1);
        assert!(unjudged.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn run_file(categories: Value) -> String {
        json!({"schema": 1, "question": "q", "question_key": "ab12cd34", "categories": categories})
            .to_string()
    }

    fn suggestion(friction: &str, example: &str, kind: &str, node: &str, metric: &str) -> Value {
        json!({
            "friction": friction,
            "cause": "the mail lane never surfaced the row",
            "example_sessions": [example],
            "evidence": "4559 unanswered bus rows",
            "events": ["hook_blocked"],
            "fix": {"kind": kind, "target": "mail lane", "text": "surface unanswered count in the relay digest"},
            "node": node,
            "metric": metric,
        })
    }

    fn out_doc(friction_counts: u64) -> Value {
        json!({
            "activity": {"tool_errors": {"hook_blocked": 7}, "interruptions": 5},
            "totals": {"unanswered": 11, "undelivered": 3},
            "events": {"hook_blocked": 12},
            "categories": {
                "judged": 4,
                "unjudged": [],
                "items": [{"metrics": {"friction": {"tool_failure": friction_counts}}}],
            },
        })
    }

    fn stamp_ok(run: Value, statuses: HashMap<String, String>, doc: &Value) -> Vec<Value> {
        let run: Run = serde_json::from_str(&run.to_string()).unwrap();
        let facet: Facet = serde_json::from_str(
            &json!({"key": {"session": "s1", "mtime": 1, "size": 2}, "friction": "tool_failure"})
                .to_string(),
        )
        .unwrap();
        let facets: HashMap<String, Facet> = [("s1".to_string(), facet)].into_iter().collect();
        stamp_suggestions(&run, &facets, &statuses, &None, doc).unwrap()
    }

    #[test]
    fn the_run_file_and_suggestion_refusals_name_the_fault() {
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
        // The suggestion stage: validation refusals, stamping, graph
        // tolerance, and the scorecard verdicts, at net-zero declarations.
        {
            let statuses: HashMap<String, String> = [
                ("x-open1".to_string(), "ready".to_string()),
                ("x-done9".to_string(), "done".to_string()),
                ("x-gone".to_string(), "superseded".to_string()),
            ]
            .into_iter()
            .collect();
            let doc = out_doc(3);
            let build = |s: Value| {
                json!({
                    "schema": 1, "question": "q", "question_key": "ab12cd34",
                    "categories": [{"name": "C", "sessions": ["s1"]}],
                    "suggestions": [s],
                })
            };
            let facet: Facet = serde_json::from_str(
                &json!({"key": {"session": "s1", "mtime": 1, "size": 2}, "friction": "tool_failure"})
                    .to_string(),
            )
            .unwrap();
            let facets: HashMap<String, Facet> = [("s1".to_string(), facet)].into_iter().collect();

            // Happy path: a valid suggestion stamps baseline, node state, and
            // the cited event counts.
            let run = json!({
                "schema": 1, "question": "q", "question_key": "ab12cd34",
                "categories": [{"name": "C", "sessions": ["s1"]}],
                "suggestions": [suggestion("tool_failure", "s1", "law", "x-open1", "friction:tool_failure")],
            });
            let stamped = stamp_ok(run, statuses.clone(), &doc);
            assert_eq!(stamped.len(), 1);
            assert_eq!(stamped[0]["baseline"], 3);
            assert_eq!(stamped[0]["node_state"], "ready");
            assert_eq!(stamped[0]["fix"]["kind"], "law");
            assert_eq!(
                stamped[0]["fix"]["text"],
                "surface unanswered count in the relay digest"
            );
            assert_eq!(stamped[0]["metric"], "friction:tool_failure");
            assert_eq!(stamped[0]["example_sessions"][0], "s1");
            assert_eq!(stamped[0]["events"]["hook_blocked"], 12);
            assert_eq!(stamped[0]["events_scope"], "machine-global");

            // No example named at all.
            let run: Run = serde_json::from_str(
                &json!({
                    "schema": 1, "question": "q", "question_key": "ab12cd34",
                    "categories": [{"name": "C", "sessions": ["s1"]}],
                    "suggestions": [{
                        "friction": "tool_failure", "cause": "c",
                        "example_sessions": [], "fix": {"kind": "law", "target": "t", "text": "x"},
                        "node": "x-open1", "metric": "interruptions",
                    }],
                })
                .to_string(),
            )
            .unwrap();
            let err = stamp_suggestions(&run, &facets, &statuses, &None, &doc).unwrap_err();
            assert!(err.contains("no example session named"), "err: {err}");

            // Example session not judged.
            let run: Run = serde_json::from_str(
                &build(suggestion(
                    "tool_failure",
                    "ghost",
                    "law",
                    "x-open1",
                    "friction:tool_failure",
                ))
                .to_string(),
            )
            .unwrap();
            let err = stamp_suggestions(&run, &facets, &statuses, &None, &doc).unwrap_err();
            assert!(err.contains("ghost is not judged"), "err: {err}");

            // Example judged with a different friction word.
            let wrong: Facet = serde_json::from_str(
                &json!({"key": {"session": "s1", "mtime": 1, "size": 2}, "friction": "missing_context"})
                    .to_string(),
            )
            .unwrap();
            let facets_wrong: HashMap<String, Facet> =
                [("s1".to_string(), wrong)].into_iter().collect();
            let run: Run = serde_json::from_str(
                &build(suggestion(
                    "tool_failure",
                    "s1",
                    "law",
                    "x-open1",
                    "friction:tool_failure",
                ))
                .to_string(),
            )
            .unwrap();
            let err = stamp_suggestions(&run, &facets_wrong, &statuses, &None, &doc).unwrap_err();
            assert!(
                err.contains("s1 is judged missing_context, not tool_failure"),
                "err: {err}"
            );

            // Unfiled node.
            let run: Run = serde_json::from_str(
                &build(suggestion(
                    "tool_failure",
                    "s1",
                    "hook",
                    "x-none",
                    "friction:tool_failure",
                ))
                .to_string(),
            )
            .unwrap();
            let err = stamp_suggestions(&run, &facets, &statuses, &None, &doc).unwrap_err();
            assert!(err.contains("x-none is not in the graph"), "err: {err}");

            // Closed nodes refuse; open ones pass.
            for (node, want) in [
                ("x-done9", "is done, not open"),
                ("x-gone", "is superseded, not open"),
            ] {
                let run: Run = serde_json::from_str(
                    &build(suggestion(
                        "tool_failure",
                        "s1",
                        "config",
                        node,
                        "unanswered",
                    ))
                    .to_string(),
                )
                .unwrap();
                let err = stamp_suggestions(&run, &facets, &statuses, &None, &doc).unwrap_err();
                assert!(err.contains(want), "err: {err}");
            }

            // Unknown fix kind.
            let run: Run = serde_json::from_str(
                &build(suggestion(
                    "tool_failure",
                    "s1",
                    "vibe",
                    "x-open1",
                    "unanswered",
                ))
                .to_string(),
            )
            .unwrap();
            let err = stamp_suggestions(&run, &facets, &statuses, &None, &doc).unwrap_err();
            assert!(err.contains("fix kind vibe"), "err: {err}");

            // Unresolvable metric.
            let run: Run = serde_json::from_str(
                &build(suggestion(
                    "tool_failure",
                    "s1",
                    "law",
                    "x-open1",
                    "no_such_metric",
                ))
                .to_string(),
            )
            .unwrap();
            let err = stamp_suggestions(&run, &facets, &statuses, &None, &doc).unwrap_err();
            assert!(
                err.contains("no_such_metric does not resolve"),
                "err: {err}"
            );

            // Event citation absent from the fold's events block.
            let run: Run = serde_json::from_str(
                &json!({
                    "schema": 1, "question": "q", "question_key": "ab12cd34",
                    "categories": [{"name": "C", "sessions": ["s1"]}],
                    "suggestions": [{
                        "friction": "tool_failure", "cause": "c",
                        "example_sessions": ["s1"], "events": ["no_such_event"],
                        "fix": {"kind": "law", "target": "t", "text": "x"},
                        "node": "x-open1", "metric": "interruptions",
                    }],
                })
                .to_string(),
            )
            .unwrap();
            let err = stamp_suggestions(&run, &facets, &statuses, &None, &doc).unwrap_err();
            assert!(
                err.contains("no_such_event is not in the fold's events block"),
                "err: {err}"
            );

            // Graph read failed: node checks skip, the error names itself.
            let run = json!({
                "schema": 1, "question": "q", "question_key": "ab12cd34",
                "categories": [{"name": "C", "sessions": ["s1"]}],
                "suggestions": [suggestion("tool_failure", "s1", "law", "x-open1", "interruptions")],
            });
            let run: Run = serde_json::from_str(&run.to_string()).unwrap();
            let statuses2: HashMap<String, String> = HashMap::new();
            let graph_err = "unavailable: graph read failed".to_string();
            let stamped =
                stamp_suggestions(&run, &facets, &statuses2, &Some(graph_err), &out_doc(3))
                    .unwrap();
            assert_eq!(stamped[0]["node_state"], "unavailable: graph read failed");
            assert_eq!(stamped[0]["baseline"], 5);

            // Scorecard: prior baselines against the current document.
            let prior = json!({"suggestions": [
                {"friction": "tool_failure", "node": "x-a", "metric": "friction:tool_failure", "baseline": 3},
                {"friction": "tool_failure", "node": "x-b", "metric": "unanswered", "baseline": 3},
                {"friction": "tool_failure", "node": "x-c", "metric": "interruptions", "baseline": 5},
                {"friction": "tool_failure", "node": "x-d", "metric": "nonexistent", "baseline": 1},
            ]});
            let doc1 = out_doc(1);
            let verdicts = scorecard(&prior, &doc1);
            assert_eq!(verdicts.len(), 4);
            assert_eq!(verdicts[0]["verdict"], "moved");
            assert_eq!(verdicts[0]["current"], 1);
            assert_eq!(verdicts[1]["verdict"], "worse");
            assert_eq!(verdicts[1]["current"], 11);
            assert_eq!(verdicts[2]["verdict"], "unchanged");
            assert_eq!(verdicts[2]["current"], 5);
            assert_eq!(verdicts[3]["verdict"], "unmeasured");
            assert!(verdicts[3]["current"].is_null());
            let empty = scorecard(&json!({}), &doc1);
            assert!(empty.is_empty());

            // A prior report over a different population (period, scope,
            // sample, or question) reads unmeasured, never moved.
            let prior2 = json!({
                "suggestions": prior["suggestions"].clone(),
                "days": 60,
                "scope": {"harnesses": ["claude"], "all_projects": false, "projects": [], "roots": []},
                "sample": {"requested": 50},
                "categories": {"question_key": "ab12cd34"},
            });
            let doc2 = json!({
                "days": 30,
                "scope": {"harnesses": ["claude"], "all_projects": false, "projects": [], "roots": []},
                "sample": {"requested": 50},
                "categories": {"question_key": "ab12cd34"},
                "activity": {"tool_errors": {}, "interruptions": 5},
                "totals": {"unanswered": 11, "undelivered": 3},
                "events": {"hook_blocked": 12},
                "items": [],
            });
            let guarded = scorecard(&prior2, &doc2);
            assert_eq!(guarded[0]["verdict"], "unmeasured");
            assert!(guarded[0]["reason"]
                .as_str()
                .unwrap()
                .contains("population differs"));
        }
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
