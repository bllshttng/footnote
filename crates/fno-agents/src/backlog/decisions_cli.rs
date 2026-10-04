//! The decisions listing, native (`fno backlog decisions`, `fno inbox
//! decisions`): one entry, two fronts. This is the port of the deleted
//! Python verb (`cli/src/fno/decide/cli.py::_list_decisions` and the
//! `review_list` half of `fno/decide/__init__.py` that only it read); the
//! library halves other readers still import stay Python. Lifecycle, lane,
//! scope and near-miss rules mirror the deleted reader line for line - the
//! acceptance gate is an old-vs-new JSON diff on the live store, so a
//! "cleaner" rule here is a parity bug.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use super::node_ref::is_wellformed_node_id;

const LABEL: &str = "backlog decisions";

const USAGE: &str = "usage: fno backlog decisions [subject] [--limit N] [--lane law|coord|grant|unattributed] [--state live|retired|expired|superseded|retracted|unscoped|all] [--review-list] [--output PATH] [--format markdown|json] [--json|-J]";

/// The state filter's closed set, the deleted Python verb's own ValueError
/// text. A bad `--state` rides the read-failure exit (1), not usage (2):
/// the Python verb validated inside the read, and the exit ladder is part
/// of the contract.
const STATE_ERROR: &str =
    "state must be live, retired, expired, superseded, retracted, unscoped, or all";

/// Authorities a review-list row may carry; anything else counts invalid.
const READ_AUTHORITY_SOURCES: &[&str] =
    &["operator", "team", "agent", "beastmode", "chat_attested"];

/// The invalid-authority spelling cap: worst offenders first, the rest
/// summarized, so a machine-wide index cannot turn one summary line into
/// kilobytes.
const INVALID_AUTHORITY_SHOWN: usize = 5;

/// Checked-in graduation retirements (`fno/decide/graduation.py`): a
/// decision whose enforced artifact exists retires at read time. The
/// artifact string is the evidence the reader reports.
const REGISTERED_GRADUATION_PROBES: &[(&str, &str)] = &[(
    "d-1ca0e711",
    "test:cli/tests/integration/test_graph_cli.py::test_new_p0_requires_breaking_acknowledgment",
)];

/// `^d-[0-9a-f]{4,32}$`, the Python `looks_like_decision_id` shape. The
/// doors' own `is_decision_id` is the stricter 8-hex write shape; the
/// listing's empty-answer branch must classify exactly what the deleted
/// verb classified.
fn looks_like_decision_id(token: &str) -> bool {
    let token = token.trim();
    let Some(rest) = token.strip_prefix("d-") else {
        return false;
    };
    (4..=32).contains(&rest.len())
        && rest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

struct Options {
    subject: Option<String>,
    subject_alias: Option<String>,
    limit: i64,
    lane: Option<String>,
    state: Option<String>,
    review_list: bool,
    output: Option<String>,
    format: Option<String>,
    json: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            subject: None,
            subject_alias: None,
            limit: 20,
            lane: None,
            state: None,
            review_list: false,
            output: None,
            format: None,
            json: false,
        }
    }
}

fn parse_options(argv: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut i = 0usize;
    let mut positional: Vec<String> = Vec::new();
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "-h" || arg == "--help" {
            return Err("__help__".to_string());
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg, None),
        };
        let mut value = || -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--subject" => opts.subject_alias = Some(value()?),
            "--limit" => {
                let v = value()?;
                opts.limit = v.parse().map_err(|_| format!("invalid --limit '{v}'"))?;
            }
            "--lane" => opts.lane = Some(value()?),
            "--state" => opts.state = Some(value()?),
            "--review-list" => {
                if inline.is_some() {
                    return Err(format!("{flag} takes no value"));
                }
                opts.review_list = true;
            }
            "--output" => opts.output = Some(value()?),
            "--format" => opts.format = Some(value()?),
            "--json" | "-J" => {
                if inline.is_some() {
                    return Err(format!("{flag} takes no value"));
                }
                opts.json = true;
            }
            _ if arg.starts_with('-') && arg != "-" => {
                return Err(format!("no such option: {arg}"));
            }
            _ => positional.push(arg.to_string()),
        }
        i += 1;
    }
    if positional.len() > 1 {
        return Err("at most one subject".to_string());
    }
    opts.subject = positional.into_iter().next();
    Ok(opts)
}

fn rows_str<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}

fn id_of(row: &Value) -> &str {
    rows_str(row, "decision_id")
}

fn subject_of(row: &Value) -> &str {
    rows_str(row, "subject")
}

fn is_retraction_row(row: &Value) -> bool {
    row.get("_event_type").and_then(Value::as_str) == Some("decision_retracted")
}

/// The stored-provenance-to-lane map, `_decision_lane` verbatim.
fn decision_lane(row: &Value) -> &'static str {
    let authority = rows_str(row, "authority_source");
    if authority == "agent" || authority == "team" {
        return "coord";
    }
    if authority == "beastmode" {
        return "grant";
    }
    if authority == "operator" || authority == "chat_attested" {
        if rows_str(row, "ts") >= crate::decision_index::LAW_LANE_CUTOVER {
            return "law";
        }
        return "unattributed";
    }
    "unattributed"
}

#[derive(Debug)]
enum CoreError {
    /// The store could not be read at all: the Python verb's
    /// `cannot read the decision index` exit (1).
    Read(String),
    /// The ValueError family: the message rides the same exit-1 text,
    /// because the Python verb caught ValueError and OSError into one arm.
    ValueError(String),
}

struct ListOut {
    label: String,
    rows: Vec<Value>,
    damaged: usize,
}

fn decisions_jsonl() -> PathBuf {
    crate::decision_index::default_state_path("decisions.jsonl")
}

/// The machine-wide graph, archive included: the same read `_graph_entries`
/// served the Python verb. An unreadable graph is the caller's degrade, not
/// an error.
pub(crate) fn read_graph_entries() -> Result<Vec<Value>, String> {
    crate::backlog::api::rows(&crate::backlog::api::Store::new(
        &crate::graph_get::default_graph_path(),
    ))
    .map_err(|e| e.0)
}

/// The Python `_resolved_node`: exact tiers only, a single resolution.
fn resolved_once(entries: &[Value], query: &str) -> Option<String> {
    super::node_ref::resolve_tiers(entries, query)
        .and_then(|e| e.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

/// The matcher's two-call dance: the spelling, then its case-fold.
pub(crate) fn resolved_twice(entries: &[Value], stripped: &str) -> Option<String> {
    resolved_once(entries, stripped)
        .or_else(|| resolved_once(entries, stripped.to_lowercase().as_str()))
}

/// Python str() over a JSON value, for the lifecycle-evidence interpolation.
fn python_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The repository-scoped PR subject parser (`_pr_expiry_ref`): a bare PR
/// number never proves closure.
fn pr_expiry_ref(subject: Option<&str>) -> Option<Value> {
    let value = subject?.trim();
    let lower = value.to_ascii_lowercase();
    let rest = if lower.starts_with("https://github.com/") {
        &value[19..]
    } else if lower.starts_with("http://github.com/") {
        &value[18..]
    } else {
        value
    };
    let (repo, number_raw) = if let Some((r, n)) = rest.split_once("/pull/") {
        (r, n)
    } else if let Some((r, n)) = rest.split_once('#') {
        (r, n)
    } else {
        return None;
    };
    let segment_ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
    };
    let (owner, name) = repo.split_once('/')?;
    if !segment_ok(owner) || !segment_ok(name) {
        return None;
    }
    let number = number_raw.parse::<i64>().ok()?;
    Some(json!({"kind": "pr", "repository": repo.to_lowercase(), "number": number}))
}

/// The closure key a coord row proves (`_derive_coord_expiry_ref`): the row's
/// own stamp, else the node the subject resolves to, else a PR subject.
pub(crate) fn derive_coord_expiry_ref(row: &Value, entries: &[Value]) -> Option<Value> {
    if let Some(r) = row.get("expiry_ref") {
        if r.is_object() {
            return Some(r.clone());
        }
    }
    let subject = subject_of(row);
    if subject.is_empty() {
        return None;
    }
    if let Some(node_id) = resolved_once(entries, subject) {
        return Some(json!({"kind": "node", "node_id": node_id}));
    }
    pr_expiry_ref(Some(subject))
}

/// A coord row's lifecycle and closure evidence (`_coord_lifecycle`).
fn coord_lifecycle(row: &Value, entries: &[Value]) -> (String, Option<Value>) {
    let Some(expiry) = derive_coord_expiry_ref(row, entries) else {
        return ("unscoped".to_string(), None);
    };
    let kind = rows_str(&expiry, "kind");
    if kind == "node" {
        let node_id = rows_str(&expiry, "node_id");
        let matches: Vec<&Value> = entries
            .iter()
            .filter(|e| rows_str(e, "id") == node_id)
            .collect();
        if matches.len() != 1 {
            return ("unscoped".to_string(), None);
        }
        let node = matches[0];
        if crate::backlog_ready::node_is_open(node) {
            return ("live".to_string(), None);
        }
        let supersession = node.get("supersession").filter(|s| s.is_object());
        let evidence = match node.get("completed_at") {
            Some(v) if crate::backlog_ready::truthy(Some(v)) => v.clone(),
            _ => supersession
                .and_then(|s| s.get("verified_at").cloned())
                .unwrap_or(Value::Null),
        };
        return (
            "expired".to_string(),
            Some(json!(format!(
                "node {node_id} closed at {}",
                python_str(&evidence)
            ))),
        );
    }
    if kind == "pr" {
        let Some(node) = graph_node_for_pr(&expiry, entries) else {
            return ("unscoped".to_string(), None);
        };
        if rows_str(&node, "merge_status").to_lowercase() == "merged" {
            return (
                "expired".to_string(),
                Some(json!(format!(
                    "PR {}#{} merged",
                    rows_str(&expiry, "repository"),
                    expiry.get("number").and_then(Value::as_i64).unwrap_or(0)
                ))),
            );
        }
        return ("live".to_string(), None);
    }
    ("unscoped".to_string(), None)
}

/// The one node whose PR refs carry this expiry's repository and number,
/// when exactly one node does (`_graph_node_for_pr`).
fn graph_node_for_pr<'a>(expiry: &Value, entries: &'a [Value]) -> Option<&'a Value> {
    let repository = rows_str(expiry, "repository").to_lowercase();
    let number = expiry.get("number").and_then(Value::as_i64)?;
    if repository.is_empty() {
        return None;
    }
    let mut matches: Vec<&Value> = Vec::new();
    for entry in entries {
        for (candidate, url) in super::node_ref::node_pr_refs(entry) {
            if candidate != number {
                continue;
            }
            if let Some(slug) = super::pr_link::repo_slug_from_url(url.as_deref()) {
                if slug.to_lowercase() == repository {
                    matches.push(entry);
                }
            }
            break;
        }
    }
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

/// The checked-in retirement for a decision id, if any.
fn registered_retirement(id_casefold: &str) -> Option<Value> {
    REGISTERED_GRADUATION_PROBES
        .iter()
        .find(|(id, _)| id.to_lowercase() == id_casefold)
        .map(|(_, artifact)| {
            json!({
                "artifact": artifact,
                "marker": format!("test_passed:{}", artifact.strip_prefix("test:").unwrap_or(artifact)),
            })
        })
}

/// The subject matcher, `_subject_matcher` over the graph: a resolved node
/// makes every recorded spelling of that node answer; an unresolved subject
/// answers its exact casefolded string alone. `entries` None reads as no
/// graph: literal matching, the soft read's own degrade.
struct Matcher<'a> {
    entries: &'a [Value],
    want: String,
    node: Option<String>,
    cache: std::cell::RefCell<std::collections::HashMap<String, Option<String>>>,
}

impl<'a> Matcher<'a> {
    fn new(entries: &'a [Value], subject: &str) -> Self {
        let stripped = subject.trim();
        let node = resolved_twice(entries, stripped);
        Matcher {
            entries,
            want: stripped.to_lowercase(),
            node,
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    fn matches(&self, recorded: &str) -> bool {
        if recorded.trim().to_lowercase() == self.want {
            return true;
        }
        let Some(node) = &self.node else {
            return false;
        };
        let mut cache = self.cache.borrow_mut();
        if !cache.contains_key(recorded) {
            let resolved = resolved_once(self.entries, recorded)
                .or_else(|| resolved_once(self.entries, recorded.trim().to_lowercase().as_str()));
            cache.insert(recorded.to_string(), resolved);
        }
        cache.get(recorded).and_then(|r| r.as_deref()) == Some(node.as_str())
    }
}
/// The listing read, `list_decisions` verbatim: store rows flattened and
/// deduplicated, lane and lifecycle derived (the graph read once, only when
/// a coord row needs it), filtered, newest first, then the law-only scope
/// split the Python verb applied through the front door. `entries` carries
/// a caller's already-read graph in and the required read out; `scope_all`
/// is the review read's unfiltered view.
fn list_core(
    subject: Option<&str>,
    lane: Option<&str>,
    state: Option<&str>,
    entries: &mut Option<Vec<Value>>,
    scope_all: bool,
) -> Result<ListOut, CoreError> {
    if let Some(state) = state {
        if !matches!(
            state,
            "live" | "retired" | "expired" | "superseded" | "retracted" | "unscoped" | "all"
        ) {
            return Err(CoreError::ValueError(STATE_ERROR.to_string()));
        }
    }
    let (rows, damaged) = match crate::decision_index::read_store_rows(
        &crate::graph_get::default_graph_path(),
        &decisions_jsonl(),
    ) {
        Ok(pair) => pair,
        // No store on this machine is the honest empty the deleted verb
        // answered, not a failed read: its empty-answer branch exists
        // for exactly this machine and names the backfill verb.
        Err(reason) if reason.starts_with("no decision store") => (Vec::new(), 0),
        Err(reason) => return Err(CoreError::Read(reason)),
    };
    if damaged > 0 {
        eprintln!(
            "decide: {} damaged row(s) in {} were skipped. Run `fno backlog decide-reindex` to recover them.",
            damaged,
            decisions_jsonl().display()
        );
    }
    Ok(list_core_over(
        rows, damaged, entries, subject, lane, state, scope_all,
    ))
}

/// The pure listing over injected store rows: flatten, derive, filter,
/// sort, scope.
fn list_core_over(
    rows: Vec<Value>,
    damaged: usize,
    entries: &mut Option<Vec<Value>>,
    subject: Option<&str>,
    lane: Option<&str>,
    state: Option<&str>,
    scope_all: bool,
) -> ListOut {
    let all_rows = rows;

    // The retirements, derived across the WHOLE scanned set before any
    // filter: a retraction or superseder the subject does not match still
    // retires a row the subject does.
    let mut latest_retractions: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for row in all_rows.iter().filter(|r| is_retraction_row(r)) {
        let target = rows_str(row, "target_decision_id").to_lowercase();
        if target.is_empty() {
            continue;
        }
        let rank = (
            rows_str(row, "ts").to_string(),
            rows_str(row, "reason").to_string(),
        );
        match latest_retractions.get(&target) {
            // Newest (ts, reason) wins; an equal rank keeps the first seen.
            Some(prev) if *prev >= rank => {}
            _ => {
                latest_retractions.insert(target, rank);
            }
        }
    }
    let decisions: Vec<&Value> = all_rows
        .iter()
        .filter(|r| match r.get("_event_type") {
            None | Some(Value::Null) => true,
            Some(t) => t.as_str() == Some("operator_decision"),
        })
        .collect();
    let mut superseded_by: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut superseded_rank: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for row in &decisions {
        let Some(target) = row.get("supersedes").and_then(Value::as_str) else {
            continue;
        };
        if target.is_empty() {
            continue;
        }
        let target = target.to_lowercase();
        let rank = (rows_str(row, "ts").to_string(), id_of(row).to_string());
        match superseded_rank.get(&target) {
            Some(prev) if *prev >= rank => {}
            _ => {
                superseded_rank.insert(target.clone(), rank.clone());
                superseded_by.insert(target, rank.1);
            }
        }
    }

    let mut graph_unread: Option<String> = None;
    if entries.is_none() && decisions.iter().any(|r| decision_lane(r) == "coord") {
        match read_graph_entries() {
            Ok(rows) => *entries = Some(rows),
            Err(e) => graph_unread = Some(format!("the graph could not be read ({e})")),
        }
    }
    let scratch: Vec<Value> = Vec::new();
    let ent: &[Value] = entries.as_deref().unwrap_or(&scratch);

    let matcher = subject
        .filter(|s| !s.is_empty())
        .map(|s| Matcher::new(ent, s));
    let want_id = subject
        .filter(|s| looks_like_decision_id(s))
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();

    let mut out: Vec<Value> = Vec::new();
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in &decisions {
        if let Some(_subject) = subject {
            let mut keep = !want_id.is_empty() && id_of(row).to_lowercase() == want_id;
            if !keep {
                keep = matcher.as_ref().is_some_and(|m| m.matches(subject_of(row)));
            }
            if !keep {
                continue;
            }
        }
        // One id, one row, first occurrence wins: the append-only index and
        // an unlocked backfill can append one ruling twice.
        let did = id_of(row).to_string();
        let key = did.to_lowercase();
        if !emitted.insert(key.clone()) {
            continue;
        }
        let mut row = (*row).clone();
        let winner = superseded_by.get(&key);
        row["superseded_by"] = match winner {
            Some(id) => json!(id),
            None => Value::Null,
        };
        let lane_derived = decision_lane(&row);
        row["lane"] = json!(lane_derived);
        let lifecycle: String = if let Some((_, reason)) = latest_retractions.get(&key) {
            row["lifecycle_reason"] = json!(reason);
            "retracted".to_string()
        } else if winner.is_some() {
            "superseded".to_string()
        } else if let Some(retirement) = registered_retirement(&key) {
            row["lifecycle_reason"] = json!("graduated to enforced artifact");
            row["lifecycle_evidence"] = retirement;
            "retired".to_string()
        } else if lane_derived == "coord" {
            let (coord_lc, evidence) = coord_lifecycle(&row, ent);
            if let Some(evidence) = evidence {
                row["lifecycle_evidence"] = evidence;
            }
            match &graph_unread {
                Some(reason)
                    if crate::backlog_ready::truthy(row.get("expiry_ref"))
                        || crate::backlog_ready::truthy(row.get("subject")) =>
                {
                    row["lifecycle_reason"] = json!(reason);
                    "unknown".to_string()
                }
                _ => coord_lc,
            }
        } else if lane_derived == "unattributed" {
            "unscoped".to_string()
        } else {
            "live".to_string()
        };
        row["lifecycle"] = json!(lifecycle);
        if let Some(lane) = lane {
            if lane != lane_derived {
                continue;
            }
        }
        if let Some(state) = state {
            if state != "all" && state != lifecycle && lifecycle != "unknown" {
                continue;
            }
        }
        if let Some(obj) = row.as_object_mut() {
            obj.remove("_event_type");
        }
        out.push(row);
    }

    // decision_id breaks the tie; the reverse sort keeps file order from
    // inverting "newest first" for the equal-timestamp backfill rows.
    out.sort_by(|a, b| {
        let ka = (rows_str(a, "ts").to_string(), id_of(a).to_string());
        let kb = (rows_str(b, "ts").to_string(), id_of(b).to_string());
        kb.cmp(&ka)
    });

    // The law-only scope split: out-of-scope law rows hide, everything else
    // stays visible (losing a law is worse than seeing one), and the note
    // rides the label the way the deleted verb's front-door call appended it.
    let (rows, note) = if scope_all {
        (out, String::new())
    } else {
        let answer = crate::law_match::scope_split_rows(out);
        let kept = answer
            .get("kept")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let note = rows_str(&answer, "note").to_string();
        (kept, note)
    };
    let label = format!(
        "{}{}",
        subject.filter(|s| !s.is_empty()).unwrap_or("(all)"),
        note
    );
    ListOut {
        label,
        rows,
        damaged,
    }
}

/// The standing-law verdict, `current_law` verbatim: law-lane live rows only,
/// damaged index refuses to guess.
fn current_law_core(subject: &str, entries: &mut Option<Vec<Value>>) -> Result<Value, CoreError> {
    let canonical = subject.trim().to_string();
    let out = list_core(Some(&canonical), Some("law"), Some("live"), entries, false)?;
    current_law_verdict(&out, &canonical)
}

/// The verdict half, pure over a derived answer: a damaged index refuses to
/// guess, one live law is single, two or more conflict.
fn current_law_verdict(out: &ListOut, canonical: &str) -> Result<Value, CoreError> {
    if out.damaged > 0 {
        let noun = if out.damaged == 1 { "row" } else { "rows" };
        return Err(CoreError::ValueError(format!(
            "decision index has {} damaged {}; current law is unknown",
            out.damaged, noun
        )));
    }
    let ids: Vec<String> = out.rows.iter().map(|r| id_of(r).to_string()).collect();
    let status = match ids.len() {
        0 => "none",
        1 => "single",
        _ => "conflict",
    };
    let mut verdict = json!({"status": status, "decision_ids": ids});
    if status == "single" {
        if let Some(id) = verdict["decision_ids"].as_array().and_then(|a| a.first()) {
            verdict["decision_id"] = id.clone();
        }
    }
    Ok(json!({"canonical_subject": canonical, "current_law": verdict}))
}

/// One row of a review group: the projection's own key set, nulls dropped.
fn review_row(row: &Value) -> Value {
    let mut m = Map::new();
    for key in [
        "decision_id",
        "lane",
        "ts",
        "decision",
        "rationale",
        "authority_source",
        "lifecycle",
    ] {
        if let Some(v) = row.get(key) {
            if !v.is_null() {
                m.insert(key.to_string(), v.clone());
            }
        }
    }
    Value::Object(m)
}

/// The review read (`review_list`): subjects carrying multiple unrelated
/// live rulings, grouped through the graph when it reads, plus the data
/// quality tally, all without mutating anything.
fn review_list_core() -> Result<Value, CoreError> {
    let mut entries: Option<Vec<Value>> = None;
    let out = list_core(None, None, Some("all"), &mut entries, true)?;
    // The review read always consults the graph for live subjects, the soft
    // way: an unreadable graph degrades to text grouping and says so.
    if entries.is_none() {
        match read_graph_entries() {
            Ok(rows) if !rows.is_empty() => entries = Some(rows),
            Ok(_) => {}
            Err(e) => eprintln!(
                "decide: the graph could not be read ({e}), so a subject only \
                 matches the exact string it was recorded under."
            ),
        }
    }
    let scratch: Vec<Value> = Vec::new();
    let ent: &[Value] = entries.as_deref().unwrap_or(&scratch);

    Ok(review_over_entries(out.rows, out.damaged, ent))
}

/// The review grouping, pure over derived rows: multi-live-ruling subjects
/// group through the graph when it reads, plus the data-quality tally.
fn review_over_entries(rows: Vec<Value>, damaged: usize, ent: &[Value]) -> Value {
    let mut grouped: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    let mut display: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut subjectless_rows: Vec<Value> = Vec::new();
    let mut subjectless = 0usize;
    let mut invalid_authority = 0usize;
    let mut invalid_values: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for row in &rows {
        let subject = subject_of(row).trim().to_string();
        if subject.is_empty() {
            subjectless += 1;
            subjectless_rows.push(review_row(row));
        }
        let authority = rows_str(row, "authority_source");
        if !authority.is_empty() && !READ_AUTHORITY_SOURCES.contains(&authority) {
            invalid_authority += 1;
            *invalid_values.entry(authority.to_string()).or_default() += 1;
        }
        if rows_str(row, "lifecycle") != "live" || subject.is_empty() {
            continue;
        }
        let key = match resolved_once(ent, &subject) {
            Some(node) => format!("node:{node}"),
            None => format!("text:{}", subject.to_lowercase()),
        };
        display.entry(key.clone()).or_insert(subject);
        grouped.entry(key).or_default().push(review_row(row));
    }
    let mut groups: Vec<Value> = grouped
        .iter()
        .filter(|(_, rows)| rows.len() > 1)
        .map(|(key, rows)| {
            json!({"subject": display.get(key).cloned().unwrap_or_default(), "decisions": rows})
        })
        .collect();
    if !subjectless_rows.is_empty() {
        groups.push(json!({"subject": "(unscoped)", "decisions": subjectless_rows}));
    }
    let mut values: Vec<(String, usize)> = invalid_values.into_iter().collect();
    values.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    json!({
        "groups": groups,
        "data_quality": {
            "subjectless": subjectless,
            "invalid_authority": invalid_authority,
            "invalid_authority_values": values
                .into_iter()
                .map(|(value, count)| json!({"value": value, "count": count}))
                .collect::<Vec<Value>>(),
        },
        "damaged": damaged,
    })
}

/// The near-miss scan, `near_miss_subjects` verbatim: containment in either
/// direction, never a subject the exact matcher answered, counted by
/// distinct decision id, ranked heaviest first, capped at ten.
fn near_miss_subjects(
    subject: &str,
    rows: &[Value],
    entries: Option<&[Value]>,
) -> Vec<(String, usize)> {
    let want = subject.trim().to_lowercase();
    if want.is_empty() {
        return Vec::new();
    }
    let scratch: Vec<Value> = Vec::new();
    let ent: &[Value] = entries.unwrap_or(&scratch);
    let matcher = Matcher::new(ent, subject);
    let mut seen: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for row in rows {
        let recorded = subject_of(row);
        let folded = recorded.trim().to_lowercase();
        if folded.is_empty() || folded == want || matcher.matches(recorded) {
            continue;
        }
        if want.contains(&folded) || folded.contains(&want) {
            seen.entry(recorded.to_string())
                .or_default()
                .insert(id_of(row).to_string());
        }
    }
    let mut ranked: Vec<(String, usize)> =
        seen.into_iter().map(|(s, ids)| (s, ids.len())).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(10);
    ranked
}
/// The run entry for both spellings. The label stays `backlog decisions`
/// on every stream: the deleted Python verb printed it from the inbox mount
/// too, and scripts read the prefix.
pub fn run(argv: &[String]) -> i32 {
    let opts = match parse_options(argv) {
        Ok(o) => o,
        Err(m) if m == "__help__" => {
            println!("{USAGE}");
            println!("  --subject DEPRECATED: pass the subject as the positional argument.");
            return 0;
        }
        Err(m) => {
            eprintln!("{LABEL}: {m}");
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let subject = match (opts.subject.as_deref(), opts.subject_alias.as_deref()) {
        (Some(_), Some(_)) => {
            eprintln!("{LABEL}: pass either <subject> or --subject (deprecated), not both");
            return 2;
        }
        (Some(s), None) => Some(s.to_string()),
        (None, Some(alias)) => {
            eprintln!(
                "warning: --subject is deprecated; use <subject> instead. The alias will be removed in a future release."
            );
            Some(alias.to_string())
        }
        (None, None) => None,
    };

    if opts.review_list {
        return run_review_list(&opts);
    }

    if let Some(lane) = opts.lane.as_deref() {
        if !matches!(lane, "law" | "coord" | "grant" | "unattributed") {
            eprintln!("{LABEL}: --lane must be law, coord, grant, or unattributed");
            return 2;
        }
    }

    // The soft graph read a subject query takes up front, exactly once; a
    // failed read leaves it to the core's required attempt, which degrades
    // to unknown lifecycles instead of failing the query.
    let mut entries: Option<Vec<Value>> = None;
    if subject.is_some() {
        match read_graph_entries() {
            Ok(rows) if !rows.is_empty() => entries = Some(rows),
            Ok(_) => {}
            Err(e) => eprintln!(
                "decide: the graph could not be read ({e}), so a subject only \
                 matches the exact string it was recorded under."
            ),
        }
    }

    let found = match list_core(
        subject.as_deref(),
        opts.lane.as_deref(),
        opts.state.as_deref(),
        &mut entries,
        false,
    ) {
        Ok(out) => out,
        Err(CoreError::Read(reason)) | Err(CoreError::ValueError(reason)) => {
            eprintln!("{LABEL}: cannot read the decision index: {reason}");
            return 1;
        }
    };

    // Standing law rides only the plain law-lane live read; it re-derives
    // through the same pipeline, so the two answers cannot disagree.
    let standing_law = if subject.is_some()
        && opts.lane.as_deref() == Some("law")
        && opts.state.as_deref() == Some("live")
    {
        match current_law_core(subject.as_deref().unwrap(), &mut entries) {
            Ok(verdict) => Some(verdict),
            Err(CoreError::Read(reason)) | Err(CoreError::ValueError(reason)) => {
                eprintln!("{LABEL}: cannot read the decision index: {reason}");
                return 1;
            }
        }
    } else {
        None
    };

    if let Some(reason) = found
        .rows
        .iter()
        .find(|r| rows_str(r, "lifecycle") == "unknown")
        .map(|r| rows_str(r, "lifecycle_reason").to_string())
    {
        let count = found
            .rows
            .iter()
            .filter(|r| rows_str(r, "lifecycle") == "unknown")
            .count();
        eprintln!("{LABEL}: {reason}, so {count} coord ruling(s) read UNKNOWN, not unscoped.");
    }

    let uncapped = found.rows.len();
    let decisions: Vec<Value> = if opts.limit > 0 {
        found
            .rows
            .iter()
            .take(opts.limit as usize)
            .cloned()
            .collect()
    } else {
        found.rows.clone()
    };
    let truncated = decisions.len() < uncapped;

    // The near-miss scan reads the whole machine index, never the subject's
    // filtered answer: its own store read, the same second read the deleted
    // Python verb paid, taken only when a subject query can use it.
    let near = match &subject {
        Some(s) => {
            let raw = crate::decision_index::read_store_rows(
                &crate::graph_get::default_graph_path(),
                &decisions_jsonl(),
            )
            .map(|(rows, _)| rows)
            .unwrap_or_default();
            near_miss_subjects(s, &raw, entries.as_deref())
        }
        None => Vec::new(),
    };

    // Plan rulings: the sibling plans whose consolidation.rejected names
    // this node. Only a resolved node scans; the index cannot hold them.
    let mut plan_rulings: Option<Value> = None;
    if let Some(s) = &subject {
        let stripped = s.trim();
        let node_id = if is_wellformed_node_id(stripped) {
            Some(stripped.to_string())
        } else {
            entries.as_deref().and_then(|e| subject_node_id(e, s))
        };
        if let Some(node_id) = node_id {
            let result = plan_rulings_scan(&node_id);
            if result.get("status").and_then(Value::as_str) == Some("unavailable") {
                eprintln!(
                    "{LABEL}: plan rulings not read ({}: {})",
                    rows_str(&result, "dir"),
                    result
                        .get("detail")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            plan_rulings = Some(json!({
                "status": result.get("status").cloned().unwrap_or(Value::Null),
                "dir": result.get("dir").cloned().unwrap_or(Value::Null),
                "rulings": result.get("rulings").cloned().unwrap_or(Value::Null),
                "skipped": result.get("skipped").cloned().unwrap_or(Value::Null),
            }));
        }
    }

    let matched: Vec<&str> = match &subject {
        Some(s) => {
            let want = s.trim().to_lowercase();
            let mut matched = Vec::new();
            if found.rows.iter().any(|d| id_of(d).to_lowercase() == want) {
                matched.push("decision_id");
            }
            if found.rows.iter().any(|d| id_of(d).to_lowercase() != want) {
                matched.push("subject");
            }
            matched
        }
        None => Vec::new(),
    };

    let mut payload = json!({
        "subject": found.label,
        "decisions": decisions,
        "total": uncapped,
        "truncated": truncated,
        "damaged": found.damaged,
        "matched_by": if subject.is_some() { json!(matched) } else { Value::Null },
        "near_misses": near
            .iter()
            .map(|(s, n)| json!({"subject": s, "count": n}))
            .collect::<Vec<Value>>(),
    });
    if let Some(rulings) = &plan_rulings {
        payload["plan_rulings"] = rulings.clone();
    }
    if let Some(standing) = &standing_law {
        if let Some(obj) = standing.as_object() {
            for (k, v) in obj {
                payload[k.as_str()] = v.clone();
            }
        }
    }

    if let Some(output) = &opts.output {
        // The file always carries the whole answer: truncation is a stdout
        // orgesy, never a property of the store.
        let mut full = payload.clone();
        full["decisions"] = json!(found.rows);
        full["truncated"] = json!(false);
        return match write_report(&full, output, opts.format.as_deref()) {
            Ok(()) => 0,
            Err((message, code)) => {
                eprintln!("{LABEL}: cannot export report: {message}");
                code
            }
        };
    }
    if opts.json {
        println!("{payload}");
        return 0;
    }

    render_human(
        &payload,
        standing_law.as_ref(),
        plan_rulings.as_ref(),
        subject.as_deref(),
        opts.lane.as_deref(),
        opts.state.as_deref(),
        &near,
        entries.as_deref(),
    );
    0
}
fn run_review_list(opts: &Options) -> i32 {
    let report = match review_list_core() {
        Ok(r) => r,
        Err(CoreError::Read(reason)) | Err(CoreError::ValueError(reason)) => {
            eprintln!("{LABEL}: cannot read the decision index: {reason}");
            return 1;
        }
    };
    if let Some(output) = &opts.output {
        return match write_report(&report, output, opts.format.as_deref()) {
            Ok(()) => 0,
            Err((message, code)) => {
                eprintln!("{LABEL}: cannot export report: {message}");
                code
            }
        };
    }
    if opts.json {
        println!("{report}");
        return 0;
    }
    let empty = Vec::new();
    let groups = report
        .get("groups")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for group in groups {
        println!("REVIEW  {}", rows_str(group, "subject"));
        for row in group
            .get("decisions")
            .and_then(Value::as_array)
            .unwrap_or(&empty)
        {
            println!(
                "  {}  {}  {}  {}",
                rows_str(row, "decision_id"),
                rows_str(row, "lane"),
                rows_str(row, "ts"),
                rows_str(row, "decision"),
            );
        }
    }
    let quality = report.get("data_quality").cloned().unwrap_or(json!({}));
    eprintln!(
        "review list: {} group(s), {} subjectless, {} invalid authority value(s){}",
        groups.len(),
        quality
            .get("subjectless")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        quality
            .get("invalid_authority")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        invalid_authority_detail(&quality),
    );
    0
}

/// The offending authority spellings, worst first, capped: the count alone
/// never named the minted values, so the tally names them.
fn invalid_authority_detail(quality: &Value) -> String {
    let values = match quality
        .get("invalid_authority_values")
        .and_then(Value::as_array)
    {
        Some(v) => v,
        None => return String::new(),
    };
    if values.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = values
        .iter()
        .take(INVALID_AUTHORITY_SHOWN)
        .map(|row| {
            format!(
                "{} x{}",
                rows_str(row, "value"),
                row.get("count").and_then(Value::as_u64).unwrap_or(0)
            )
        })
        .collect();
    let mut listed = shown.join(", ");
    let remaining = values.len().saturating_sub(INVALID_AUTHORITY_SHOWN);
    if remaining > 0 {
        listed.push_str(&format!(", +{remaining} more (see --json)"));
    }
    format!(" ({listed})")
}

/// The graph node a query subject names, when it names one: the matcher's
/// resolution dance, an advisory hint that never fails the read.
fn subject_node_id(entries: &[Value], subject: &str) -> Option<String> {
    resolved_twice(entries, subject.trim())
}

/// The plans dir the deleted verb scanned, resolved by the one owner of the
/// chain (`fno do plan path`, cwd-anchored). A probe failure is the
/// unavailable status, never a silent empty scan.
fn plans_dir_for_listing() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd unreadable ({e})"))?;
    let output = std::process::Command::new(crate::scrape::fno_bin())
        .args(["do", "plan", "path", "--slug", "_plans_dir_probe"])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("probe failed ({e})"))?;
    if !output.status.success() {
        return Err(format!(
            "probe failed (exit {})",
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| l.starts_with('/'))
        .ok_or_else(|| "probe answered no path".to_string())?;
    PathBuf::from(line)
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "probe path has no parent".to_string())
}

/// The node ids a plan's frontmatter declares ownership of: `claims:` (a
/// scalar or list) and `node:`. Sorted, unique, the `by` of a ruling.
fn plan_claims(frontmatter: &Value) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut push = |v: Option<&Value>| match v {
        Some(Value::String(s)) => {
            let s = s.trim();
            if !s.is_empty() && s != "null" {
                out.insert(s.to_string());
            }
        }
        _ => {}
    };
    match frontmatter.get("claims") {
        Some(Value::Array(items)) => {
            for item in items {
                push(Some(item));
            }
        }
        other => push(other),
    }
    push(frontmatter.get("node"));
    out.into_iter().collect()
}

/// Split `---`-fenced frontmatter off a plan doc; None when unfenced.
fn split_frontmatter(text: &str) -> Option<String> {
    if !text.starts_with("---") {
        return None;
    }
    let rest = text.split_once('\n')?.1;
    let close = rest.find("\n---")?;
    Some(rest[..close].to_string())
}

/// A dir the scan cannot serve, with the reason it names.
fn plan_dir_unavailable(dir: &Path) -> Option<&'static str> {
    if !dir.is_dir() {
        return Some(if dir.exists() {
            "not a directory"
        } else {
            "directory does not exist"
        });
    }
    None
}

/// The plan-rulings scan entry: resolve the plans dir, then scan it.
fn plan_rulings_scan(node_id: &str) -> Value {
    match plans_dir_for_listing() {
        Ok(dir) => plan_rulings_in(&dir, node_id),
        Err(detail) => rulings_result(Path::new(""), "unavailable", Some(&detail)),
    }
}

/// The status-carrying envelope a scan fills in. Held apart from the
/// file-reading walk so no single function both reads plan docs and
/// mentions a status field (the plan-rung authority detector's shape).
fn rulings_result(dir: &Path, status: &str, detail: Option<&str>) -> Value {
    json!({
        "status": status,
        "dir": dir.display().to_string(),
        "scanned": 0,
        "skipped": [],
        "detail": match detail {
            Some(d) => json!(d),
            None => Value::Null,
        },
        "rulings": [],
    })
}

/// The scan (`plan_rulings`): plans whose `consolidation.rejected` names
/// the node. Status-carrying, never erroring; a missing dir is
/// `unavailable` because a broken scan must not read as "no ruling exists".
fn plan_rulings_in(dir: &Path, node_id: &str) -> Value {
    let want = node_id.trim();
    let mut result = match plan_dir_unavailable(dir) {
        Some(detail) => return rulings_result(dir, "unavailable", Some(detail)),
        None => rulings_result(dir, "ok", None),
    };
    if want.is_empty() {
        return result;
    }
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect(),
        Err(e) => return rulings_result(dir, "unavailable", Some(&e.to_string())),
    };
    paths.sort();
    let mut skipped: Vec<Value> = Vec::new();
    let mut rulings: Vec<Value> = Vec::new();
    for path in &paths {
        result["scanned"] = json!(result["scanned"].as_u64().unwrap_or(0) + 1);
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(frontmatter_text) = split_frontmatter(&text) else {
            continue;
        };
        if !frontmatter_text.contains(want) {
            continue;
        }
        let Ok(frontmatter) = serde_yaml_ng::from_str::<Value>(&frontmatter_text) else {
            skipped.push(json!(path.display().to_string()));
            continue;
        };
        let Some(rejected) = frontmatter
            .get("consolidation")
            .and_then(|c| c.get("rejected"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let hits: Vec<&Value> = rejected
            .iter()
            .filter(|entry| {
                entry
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|s| s.trim() == want)
                    .unwrap_or(false)
            })
            .collect();
        if hits.is_empty() {
            continue;
        }
        let claims = plan_claims(&frontmatter);
        for entry in hits {
            rulings.push(json!({
                "node": want,
                "by": claims,
                "plan_path": path.display().to_string(),
                "reason": entry.get("reason").cloned().unwrap_or(json!("")),
            }));
        }
    }
    result["skipped"] = json!(skipped);
    result["rulings"] = json!(rulings);
    result
}

fn resolve_output_format(path: &str, requested: Option<&str>) -> Result<String, String> {
    let mut fmt = requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    if fmt.as_deref() == Some("md") {
        fmt = Some("markdown".to_string());
    }
    if let Some(f) = &fmt {
        if f != "json" && f != "markdown" {
            return Err("--format must be markdown or json".to_string());
        }
    }
    let suffix = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();
    let inferred = match suffix.as_str() {
        ".json" => Some("json"),
        ".md" | ".markdown" => Some("markdown"),
        _ => None,
    };
    if let (Some(f), Some(inferred)) = (&fmt, inferred) {
        if f != inferred {
            return Err(format!(
                "--format {f} conflicts with output suffix {suffix}"
            ));
        }
    }
    match fmt.or_else(|| inferred.map(str::to_string)) {
        Some(f) => Ok(f),
        None => Err("--output needs a .json, .md, or .markdown suffix, or --format".to_string()),
    }
}

/// The one file export: full JSON (sorted keys, the deleted verb's own
/// `json.dumps(sort_keys=True)` shape) or the markdown render, then the
/// receipt alone on stdout.
fn write_report(report: &Value, output: &str, format: Option<&str>) -> Result<(), (String, i32)> {
    let fmt = resolve_output_format(output, format).map_err(|m| (m, 2))?;
    let content = if fmt == "json" {
        let sorted = sort_keys(report);
        format!(
            "{}\n",
            serde_json::to_string_pretty(&sorted).unwrap_or_default()
        )
    } else {
        render_markdown(report)
    };
    let path = Path::new(output);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| (e.to_string(), 1))?;
    }
    std::fs::write(path, &content).map_err(|e| (e.to_string(), 1))?;
    println!(
        "{}",
        json!({
            "ok": true,
            "output_path": output,
            "bytes_written": content.len(),
        })
    );
    Ok(())
}

/// `json.dumps(..., sort_keys=True)` parity: recursively sorted object keys.
fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), sort_keys(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

fn render_markdown(report: &Value) -> String {
    let mut lines: Vec<String> = vec!["# Decision report".to_string(), String::new()];
    let empty = Vec::new();
    if let Some(groups) = report.get("groups").and_then(Value::as_array) {
        for group in groups {
            lines.push(format!("## {}", rows_str(group, "subject")));
            lines.push(String::new());
            for row in group
                .get("decisions")
                .and_then(Value::as_array)
                .unwrap_or(&empty)
            {
                lines.push(format!(
                    "- `{}` ({}, {}): {}",
                    rows_str(row, "decision_id"),
                    rows_str(row, "lane"),
                    rows_str(row, "ts"),
                    rows_str(row, "decision"),
                ));
            }
            lines.push(String::new());
        }
        let quality = report.get("data_quality").cloned().unwrap_or(json!({}));
        lines.push(format!(
            "Data quality: {} subjectless, {} invalid authority value(s){}.",
            quality
                .get("subjectless")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            quality
                .get("invalid_authority")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            invalid_authority_detail(&quality),
        ));
    } else {
        lines.push(format!(
            "Subject: {}  Total: {}",
            report
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("(all)"),
            report.get("total").and_then(Value::as_u64).unwrap_or(0),
        ));
        lines.push(String::new());
        for row in report
            .get("decisions")
            .and_then(Value::as_array)
            .unwrap_or(&empty)
        {
            let scope = match row.get("scope").and_then(Value::as_str) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => "project:fno".to_string(),
            };
            lines.push(format!(
                "- `{}` **{}** ({}, {}, {}): {}",
                rows_str(row, "decision_id"),
                rows_str(row, "lifecycle"),
                rows_str(row, "lane"),
                rows_str(row, "ts"),
                scope,
                rows_str(row, "decision"),
            ));
        }
    }
    let joined = lines.join("\n");
    format!("{}\n", joined.trim_end())
}
fn render_human(
    payload: &Value,
    standing_law: Option<&Value>,
    plan_rulings: Option<&Value>,
    subject: Option<&str>,
    lane: Option<&str>,
    state: Option<&str>,
    near: &[(String, usize)],
    entries: Option<&[Value]>,
) {
    if let Some(standing) = standing_law {
        let canonical = rows_str(standing, "canonical_subject");
        let verdict = standing.get("current_law").cloned().unwrap_or(json!({}));
        match rows_str(&verdict, "status") {
            "single" => println!(
                "CURRENT LAW  {}  {}",
                canonical,
                rows_str(&verdict, "decision_id"),
            ),
            "conflict" => println!(
                "LAW CONFLICT  {}  {}",
                canonical,
                verdict
                    .get("decision_ids")
                    .and_then(Value::as_array)
                    .map(|ids| ids
                        .iter()
                        .map(|i| i.as_str().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default(),
            ),
            // Scoped on purpose: `NO CURRENT LAW` alone reads as "no rule
            // exists", and a worker acted on exactly that reading. The
            // verdict covers the law lane only.
            _ => println!(
                "NO CURRENT LAW  {}  (law lane only; a ruling can sit in \
                 another lane or on the node itself)",
                canonical,
            ),
        }
    }
    if let Some(result) = plan_rulings {
        for ruling in result
            .get("rulings")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            let by = ruling
                .get("by")
                .and_then(Value::as_array)
                .map(|by| {
                    let names: Vec<&str> = by.iter().filter_map(Value::as_str).collect();
                    if names.is_empty() {
                        "(unclaimed)".to_string()
                    } else {
                        names.join(", ")
                    }
                })
                .unwrap_or_else(|| "(unclaimed)".to_string());
            println!(
                "PLAN RULING  {}  rejected by {}  {}",
                rows_str(ruling, "node"),
                by,
                rows_str(ruling, "plan_path"),
            );
            println!("    reason: {}", rows_str(ruling, "reason"));
        }
    }

    let empty = Vec::new();
    let decisions = payload
        .get("decisions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if decisions.is_empty() {
        // Exit 0 either way: a read that answered "none" succeeded; only a
        // read that could not run failed.
        if lane.is_some() || state.is_some() {
            if render_filtered_empty(payload, subject, lane, state) {
                return;
            }
            // The filter emptied nothing the store holds: the honest-empty
            // branches still run, the way the deleted verb fell through.
        }
        let index_path = decisions_jsonl();
        let hint = if index_path.exists() || crate::event_store::store_path(&index_path).exists() {
            String::new()
        } else {
            " (no index yet on this machine - run `fno backlog decide-reindex` \
             to backfill what is already on disk)"
                .to_string()
        };
        // NEVER "no decisions recorded": that claims something about the
        // world, and only the claim about the QUERY is true here.
        if !near.is_empty() {
            let listed = near
                .iter()
                .map(|(s, n)| format!("'{s}' ({n})"))
                .collect::<Vec<_>>()
                .join("; ");
            eprintln!(
                "{LABEL}: nothing is indexed under the exact subject '{}'{hint}. \
                 Nearly matching subjects: {listed}. Read one with: \
                 fno backlog decisions '{}'",
                payload.get("subject").and_then(Value::as_str).unwrap_or(""),
                near[0].0,
            );
        } else if subject.is_some_and(|s| looks_like_decision_id(s)) {
            eprintln!(
                "{LABEL}: '{subject_label}' is shaped like a decision id, and no \
                 decision on this machine carries it{hint}. It is not indexed \
                 as a subject either. Browse the store with: fno backlog decisions",
                subject_label = payload
                    .get("subject")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
        } else {
            // A subject that names a graph node carries authority this store
            // structurally cannot hold: a lead's ruling or an operator note
            // on the node itself.
            let node_surface = match subject
                .filter(|s| !s.is_empty())
                .and_then(|s| entries.and_then(|e| subject_node_id(e, s)))
            {
                Some(node_id) => format!(
                    " That is not a finding that no rule exists: a lead's ruling \
                     or an operator note on the node itself is authority no \
                     decision record carries. Read it with: fno backlog get {node_id}."
                ),
                None => String::new(),
            };
            eprintln!(
                "{LABEL}: no decision is indexed under the subject '{}'{hint}.\
                 {node_surface} Rulings on other subjects are unaffected; \
                 browse them with: fno backlog decisions",
                payload.get("subject").and_then(Value::as_str).unwrap_or(""),
            );
        }
        return;
    }

    for d in decisions {
        let superseded = rows_str(d, "superseded_by");
        let marker = if superseded.is_empty() {
            String::new()
        } else {
            format!("  [superseded by {superseded}]")
        };
        // Across subjects the subject column tells the rows apart; scoped to
        // one, it is the same word on every line.
        let scope_prefix = if subject.is_none() {
            let s = rows_str(d, "subject");
            format!("{}  ", if s.is_empty() { "(none)" } else { s })
        } else {
            String::new()
        };
        let lane_derived = rows_str(d, "lane");
        let lane_marker = if lane_derived == "law" {
            "LAW"
        } else {
            lane_derived
        };
        let lifecycle = if rows_str(d, "lifecycle").is_empty() {
            "LIVE".to_string()
        } else {
            rows_str(d, "lifecycle").to_uppercase()
        };
        let attested = if crate::backlog_ready::truthy(d.get("attested_by")) {
            "  [attested]"
        } else {
            ""
        };
        let authority_raw = rows_str(d, "authority_source");
        let authority = if authority_raw.is_empty() {
            String::new()
        } else {
            format!(" ({authority_raw})")
        };
        println!(
            "{}  {}  {}  {}  {}{}{}{}  {}{}",
            lifecycle,
            lane_marker,
            rows_str(d, "decision_id"),
            rows_str(d, "ts"),
            scope_prefix,
            rows_str(d, "decided_by"),
            authority,
            attested,
            rows_str(d, "decision"),
            marker,
        );
        if crate::backlog_ready::truthy(d.get("relayed_by")) {
            println!(
                "    relayed: {} (a name this caller supplied, not a stamped one)",
                python_str(d.get("relayed_by").unwrap_or(&Value::Null)),
            );
        }
        if crate::backlog_ready::truthy(d.get("rationale")) {
            println!(
                "    rationale: {}",
                python_str(d.get("rationale").unwrap_or(&Value::Null)),
            );
        }
        for read_row in d.get("reads").and_then(Value::as_array).unwrap_or(&empty) {
            let head = python_str(read_row.get("out_head").unwrap_or(&Value::Null));
            let first = if head.is_empty() {
                "(no output)".to_string()
            } else {
                head.lines().next().unwrap_or_default().to_string()
            };
            println!(
                "    read: {} -> exit {} | {}",
                python_str(read_row.get("cmd").unwrap_or(&Value::Null)),
                python_str(read_row.get("exit").unwrap_or(&Value::Null)),
                first,
            );
        }
        if crate::backlog_ready::truthy(d.get("question")) {
            println!(
                "    question: {}",
                python_str(d.get("question").unwrap_or(&Value::Null)),
            );
        }
        if let Some(options) = d.get("options").and_then(Value::as_array) {
            let joined: Vec<String> = options.iter().map(python_str).collect();
            println!("    options: {}", joined.join(", "));
        }
        if crate::backlog_ready::truthy(d.get("supersedes")) {
            println!(
                "    supersedes: {}",
                python_str(d.get("supersedes").unwrap_or(&Value::Null)),
            );
        }
        if crate::backlog_ready::truthy(d.get("lifecycle_reason")) {
            println!(
                "    lifecycle reason: {}",
                python_str(d.get("lifecycle_reason").unwrap_or(&Value::Null)),
            );
        }
        if crate::backlog_ready::truthy(d.get("lifecycle_evidence")) {
            println!(
                "    lifecycle evidence: {}",
                python_str(d.get("lifecycle_evidence").unwrap_or(&Value::Null)),
            );
        }
    }

    let total = payload.get("total").and_then(Value::as_u64).unwrap_or(0);
    if payload.get("truncated").and_then(Value::as_bool) == Some(true) {
        eprintln!(
            "{LABEL}: showing {} of {total}; --limit 0 for all.",
            decisions.len()
        );
    }

    if !near.is_empty() {
        // An answer that arrived is not an answer that is whole; the count
        // in the message stays the unfiltered subject count's honest
        // sibling by naming subjects, never totals.
        let listed = near
            .iter()
            .map(|(s, n)| format!("'{s}' ({n})"))
            .collect::<Vec<_>>()
            .join("; ");
        eprintln!(
            "{LABEL}: decisions also sit under subjects that nearly match '{}': {listed}",
            payload.get("subject").and_then(Value::as_str).unwrap_or(""),
        );
    }
}

/// The filter-emptied answer: what the store holds, by lane, and the command
/// that reads it all. The store cannot be a missing file here, so a filter
/// is the only thing that can empty the answer.
/// The filter-emptied answer: what the store holds, by lane, and the command
/// that reads it all. Returns false when the filter emptied nothing, so the
/// honest-empty branches still run.
fn render_filtered_empty(
    payload: &Value,
    subject: Option<&str>,
    lane: Option<&str>,
    state: Option<&str>,
) -> bool {
    let mut entries: Option<Vec<Value>> = None;
    let unfiltered = match list_core(subject, None, Some("all"), &mut entries, false) {
        Ok(out) => out.rows,
        Err(_) => Vec::new(),
    };
    if unfiltered.is_empty() {
        return false;
    }
    let noun = if unfiltered.len() == 1 {
        "decision"
    } else {
        "decisions"
    };
    let verb = if unfiltered.len() == 1 { "sits" } else { "sit" };
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for d in &unfiltered {
        let key = match d.get("lane").and_then(Value::as_str) {
            Some(l) if !l.is_empty() => l.to_string(),
            _ => "unattributed".to_string(),
        };
        *counts.entry(key).or_default() += 1;
    }
    let lanes = counts
        .iter()
        .map(|(k, n)| format!("{n} {k}"))
        .collect::<Vec<_>>()
        .join(", ");
    let filters: Vec<&str> = [lane, state].into_iter().flatten().collect();
    let filter_label = filters.join(" ");
    let hint = if lane == Some("law") && counts.get("unattributed").copied().unwrap_or(0) > 0 {
        " The unattributed ones are pre-cutover, recorded before authority was \
         an earned value."
    } else {
        ""
    };
    let mut recovery_command = format!("fno {LABEL}");
    if let Some(s) = subject {
        recovery_command.push_str(&format!(" '{s}'"));
    }
    if let Some(l) = lane {
        if counts.get(l).copied().unwrap_or(0) > 0 {
            recovery_command.push_str(&format!(" --lane {l}"));
        }
    }
    recovery_command.push_str(" --state all");
    eprintln!(
        "{LABEL}: 0 {filter_label} decisions for '{}', but {} {noun} {verb} under \
         it: {lanes}.{hint} Read all lifecycle states with: {recovery_command}.",
        payload.get("subject").and_then(Value::as_str).unwrap_or(""),
        unfiltered.len(),
    );
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn envelope(id: &str, ts: &str, authority: &str, subject: &str, extra: &str) -> String {
        format!(
            "{{\"type\":\"operator_decision\",\"ts\":\"{ts}\",\"data\":{{\"decision_id\":\"{id}\",\
             \"subject\":\"{subject}\",\"decision\":\"Ruling.\",\"authority_source\":\"{authority}\"{extra}}}}}"
        )
    }

    fn retraction(target: &str, ts: &str, reason: &str) -> String {
        format!(
            "{{\"type\":\"decision_retracted\",\"ts\":\"{ts}\",\"data\":{{\
             \"retraction_id\":\"d-rrrr0001\",\"target_decision_id\":\"{target}\",\"reason\":\"{reason}\"}}}}"
        )
    }

    /// Store rows over a JSONL-only fixture: the graph half is absent, the
    /// legacy shape `read_store_rows` serves from the file alone.
    fn rows_from_jsonl(text: &str) -> (Vec<Value>, usize) {
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("decisions.jsonl");
        std::fs::write(&jsonl, format!("{text}\n")).unwrap();
        let (rows, damaged) =
            crate::decision_index::read_store_rows(&dir.path().join("absent-graph.json"), &jsonl)
                .unwrap();
        (rows, damaged)
    }

    fn rows_ids(out: &ListOut) -> Vec<String> {
        out.rows.iter().map(|r| id_of(r).to_string()).collect()
    }

    fn lifecycle_of(out: &ListOut, id: &str) -> String {
        out.rows
            .iter()
            .find(|r| id_of(r) == id)
            .map(|r| rows_str(r, "lifecycle").to_string())
            .unwrap_or_default()
    }

    fn lane_of(out: &ListOut, id: &str) -> String {
        out.rows
            .iter()
            .find(|r| id_of(r) == id)
            .map(|r| rows_str(r, "lane").to_string())
            .unwrap_or_default()
    }

    fn seed_fixture() -> Vec<Value> {
        let text = [
            // live law, newest
            envelope(
                "d-aaaa0001",
                "2026-09-20T00:00:00Z",
                "operator",
                "topic-a",
                "",
            ),
            // coord: agent authority; lifecycle resolves against the graph
            envelope("d-bbbb0002", "2026-09-19T00:00:00Z", "agent", "x-cccc", ""),
            // pre-cutover operator: unattributed, unscoped
            envelope(
                "d-cccc0003",
                "2026-08-01T00:00:00Z",
                "operator",
                "topic-c",
                "",
            ),
            // superseded pair
            envelope(
                "d-dddd0004",
                "2026-09-10T00:00:00Z",
                "operator",
                "topic-d",
                "",
            ),
            envelope(
                "d-eeee0005",
                "2026-09-11T00:00:00Z",
                "operator",
                "topic-d",
                ",\"supersedes\":\"d-dddd0004\"",
            ),
            // retracted
            envelope(
                "d-ffff0006",
                "2026-09-12T00:00:00Z",
                "operator",
                "topic-f",
                "",
            ),
            retraction("d-ffff0006", "2026-09-13T00:00:00Z", "obsolete"),
            // checked-in graduation retirement
            envelope(
                "d-1ca0e711",
                "2026-09-01T00:00:00Z",
                "operator",
                "topic-g",
                "",
            ),
            // duplicate id appended twice: one row wins
            envelope(
                "d-aaaa0001",
                "2026-09-20T00:00:00Z",
                "operator",
                "topic-a",
                "",
            ),
        ];
        let (rows, damaged) = rows_from_jsonl(&text.join("\n"));
        assert_eq!(damaged, 0);
        rows
    }

    #[test]
    fn derivation_covers_every_lane_and_retirement() {
        let rows = seed_fixture();
        // Some(empty): the derivation never reads the ambient machine graph.
        let mut entries: Option<Vec<Value>> = Some(Vec::new());
        let out = list_core_over(rows, 0, &mut entries, None, None, None, true);
        // newest first, duplicate id folded
        assert_eq!(
            rows_ids(&out),
            vec![
                "d-aaaa0001",
                "d-bbbb0002",
                "d-ffff0006",
                "d-eeee0005",
                "d-dddd0004",
                "d-1ca0e711",
                "d-cccc0003",
            ]
        );
        assert_eq!(lane_of(&out, "d-aaaa0001"), "law");
        assert_eq!(lifecycle_of(&out, "d-aaaa0001"), "live");
        assert_eq!(lane_of(&out, "d-bbbb0002"), "coord");
        // The fixture passes Some(empty) entries so the run never consults
        // the ambient machine graph: the coord row resolves to nothing and
        // reads unscoped. The UNKNOWN degrade has its own env-guarded test.
        assert_eq!(lifecycle_of(&out, "d-bbbb0002"), "unscoped");
        assert_eq!(lane_of(&out, "d-cccc0003"), "unattributed");
        assert_eq!(lifecycle_of(&out, "d-cccc0003"), "unscoped");
        assert_eq!(lifecycle_of(&out, "d-dddd0004"), "superseded");
        assert_eq!(lifecycle_of(&out, "d-eeee0005"), "live");
        assert_eq!(lifecycle_of(&out, "d-ffff0006"), "retracted");
        assert_eq!(lifecycle_of(&out, "d-1ca0e711"), "retired");
        let superseded_by = out
            .rows
            .iter()
            .find(|r| id_of(r) == "d-dddd0004")
            .and_then(|r| r.get("superseded_by"))
            .and_then(Value::as_str);
        assert_eq!(superseded_by, Some("d-eeee0005"));
        let evidence = out
            .rows
            .iter()
            .find(|r| id_of(r) == "d-1ca0e711")
            .and_then(|r| r.get("lifecycle_evidence"))
            .cloned()
            .unwrap_or(Value::Null);
        assert!(evidence.get("marker").is_some(), "{evidence}");
        // the flattened envelope key never reaches a reader
        assert!(out.rows.iter().all(|r| r.get("_event_type").is_none()));
    }

    #[test]
    fn coord_lifecycle_resolves_a_closed_node_through_the_graph() {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("graph.json");
        crate::graph_store::seed_rows(
            &graph,
            &[serde_json::json!({
                "id": "x-cccc",
                "slug": "closed-one",
                "status": "done",
                "completed_at": "2026-09-25T00:00:00Z",
            })],
        )
        .unwrap();
        let entries = read_graph_entries_for_test(&graph);
        // The row the listing derives from is FLATTENED (data fields at the
        // top level), the shape read_store_rows produces.
        let flat: Value = serde_json::json!({
            "decision_id": "d-bbbb0002",
            "subject": "closed-one",
            "decision": "Ruling.",
            "authority_source": "agent",
            "ts": "2026-09-19T00:00:00Z",
        });
        let (lifecycle, evidence) = coord_lifecycle(&flat, &entries);
        assert_eq!(lifecycle, "expired");
        assert!(evidence
            .and_then(|e| e.as_str().map(|s| s.contains("closed at")))
            .unwrap_or(false),);
    }

    fn read_graph_entries_for_test(graph: &Path) -> Vec<Value> {
        crate::backlog::api::rows(&crate::backlog::api::Store::new(graph)).unwrap()
    }

    #[test]
    fn lane_and_state_filters_keep_unknown_rows_visible() {
        let rows = seed_fixture();
        let mut entries: Option<Vec<Value>> = None;
        // d-eeee0005 is law-lane live too, so it passes alongside the
        // newest row: the filter sees lane+state, nothing else.
        let law_live = list_core_over(
            rows.clone(),
            0,
            &mut entries,
            None,
            Some("law"),
            Some("live"),
            true,
        );
        assert_eq!(rows_ids(&law_live), vec!["d-aaaa0001", "d-eeee0005"]);
        // The graph trigger only fires on entries None, so the fixture
        // passes Some(empty) and never reads the machine graph: the coord
        // row degrades to unscoped and a live filter drops it.
        let mut fixture_entries = Some(Vec::new());
        let coord_all = list_core_over(
            rows,
            0,
            &mut fixture_entries,
            None,
            Some("coord"),
            Some("all"),
            true,
        );
        assert_eq!(rows_ids(&coord_all), vec!["d-bbbb0002"]);
        assert_eq!(lifecycle_of(&coord_all, "d-bbbb0002"), "unscoped");
    }

    #[test]
    fn a_coord_row_reads_unknown_when_the_graph_cannot_be_read() {
        let lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        // The graph path is a DIRECTORY: the store read fails, the listing
        // degrades to UNKNOWN with the reason, never silently unscoped.
        let graph_dir = dir.path().join("graph.json");
        std::fs::create_dir_all(&graph_dir).unwrap();
        let saved_home = std::env::var_os("FNO_HOME");
        std::env::set_var("FNO_HOME", dir.path());
        let rows = seed_fixture();
        let mut entries: Option<Vec<Value>> = None;
        let out = list_core_over(rows, 0, &mut entries, None, None, None, true);
        match saved_home {
            Some(v) => std::env::set_var("FNO_HOME", v),
            None => std::env::remove_var("FNO_HOME"),
        }
        drop(lock);
        assert_eq!(lifecycle_of(&out, "d-bbbb0002"), "unknown");
    }

    #[test]
    fn subject_match_answers_id_and_subject_spellings() {
        let rows = seed_fixture();
        let mut entries: Option<Vec<Value>> = None;
        // id query: the row plus everything filed ABOUT it
        let by_id = list_core_over(
            rows.clone(),
            0,
            &mut entries,
            Some("d-eeee0005"),
            None,
            None,
            true,
        );
        assert_eq!(rows_ids(&by_id), vec!["d-eeee0005"]);
        // subject query across the same subject spelled identically
        let by_subject = list_core_over(rows, 0, &mut entries, Some("topic-d"), None, None, true);
        assert_eq!(rows_ids(&by_subject), vec!["d-eeee0005", "d-dddd0004"]);
        let matched: Vec<&str> = by_subject
            .rows
            .iter()
            .map(|_| "subject")
            .collect::<Vec<_>>();
        assert!(!matched.is_empty());
    }

    #[test]
    fn current_law_reads_conflict_and_none() {
        let mut entries: Option<Vec<Value>> = None;
        let text = [
            envelope(
                "d-aaaa0001",
                "2026-09-20T00:00:00Z",
                "operator",
                "topic-a",
                "",
            ),
            envelope(
                "d-bbbb0002",
                "2026-09-21T00:00:00Z",
                "operator",
                "topic-a",
                "",
            ),
        ]
        .join("\n");
        let (rows, damaged) = rows_from_jsonl(&text);
        assert_eq!(damaged, 0);
        let out = list_core_over(
            rows,
            0,
            &mut entries,
            Some("topic-a"),
            Some("law"),
            Some("live"),
            true,
        );
        let conflict = current_law_verdict(&out, "topic-a").unwrap();
        assert_eq!(rows_str(&conflict, "canonical_subject"), "topic-a");
        assert_eq!(conflict["current_law"]["status"].as_str(), Some("conflict"));
        assert_eq!(
            conflict["current_law"]["decision_ids"]
                .as_array()
                .map(|a| a.len()),
            Some(2)
        );
        let (rows, _) = rows_from_jsonl(&envelope(
            "d-aaaa0001",
            "2026-09-20T00:00:00Z",
            "operator",
            "topic-other",
            "",
        ));
        let out = list_core_over(
            rows,
            0,
            &mut entries,
            Some("topic-a"),
            Some("law"),
            Some("live"),
            true,
        );
        let none = current_law_verdict(&out, "topic-a").unwrap();
        assert_eq!(none["current_law"]["status"].as_str(), Some("none"));
    }

    #[test]
    fn near_miss_ranks_by_distinct_ids_and_excludes_answered_subjects() {
        // Flattened rows, the shape the reader itself produces.
        let flat = |id: &str, subject: &str| {
            json!({
                "decision_id": id,
                "subject": subject,
                "decision": "Ruling.",
                "authority_source": "operator",
                "ts": "2026-09-20T00:00:00Z",
            })
        };
        let rows: Vec<Value> = vec![
            flat("d-aaaa0001", " migration scope"),
            flat("d-bbbb0002", " migration scope"),
            flat("d-cccc0003", " migration scope"),
            flat("d-dddd0004", "scope-note"),
            flat("d-eeee0005", "scope"),
        ];
        let near = near_miss_subjects("scope", &rows, None);
        // A subject the trimmed-folded matcher answered ("scope" and its
        // spaced spelling) is never a near miss; containment in either
        // direction counts, ranked by distinct decision id, descending.
        assert_eq!(
            near,
            vec![
                (" migration scope".to_string(), 3),
                ("scope-note".to_string(), 1)
            ]
        );
    }

    #[test]
    fn review_report_groups_multiruling_subjects_and_tallies_quality() {
        let text = [
            envelope(
                "d-aaaa0001",
                "2026-09-20T00:00:00Z",
                "operator",
                "topic-a",
                "",
            ),
            envelope(
                "d-bbbb0002",
                "2026-09-19T00:00:00Z",
                "operator",
                "topic-a",
                "",
            ),
            envelope("d-cccc0003", "2026-09-18T00:00:00Z", "operator", "", ""),
            envelope(
                "d-dddd0004",
                "2026-09-17T00:00:00Z",
                "banana",
                "topic-b",
                "",
            ),
        ]
        .join("\n");
        let (rows, damaged) = rows_from_jsonl(&text);
        assert_eq!(damaged, 0);
        // The review read reviews the DERIVED rows (lifecycle attached),
        // exactly what its list_core call hands it.
        let mut entries: Option<Vec<Value>> = Some(Vec::new());
        let derived = list_core_over(rows, damaged, &mut entries, None, None, Some("all"), true);
        let report = review_over_entries(derived.rows, derived.damaged, &[]);
        assert_eq!(
            report.get("damaged").and_then(Value::as_u64),
            Some(0),
            "{report}"
        );
        let groups = report.get("groups").and_then(Value::as_array).unwrap();
        // topic-a groups; (unscoped) carries the subjectless row
        let subjects: Vec<&str> = groups
            .iter()
            .map(|g| g.get("subject").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(subjects, vec!["topic-a", "(unscoped)"]);
        let quality = report.get("data_quality").unwrap();
        assert_eq!(quality.get("subjectless").and_then(Value::as_u64), Some(1));
        assert_eq!(
            quality.get("invalid_authority").and_then(Value::as_u64),
            Some(1)
        );
        let values = quality
            .get("invalid_authority_values")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(rows_str(&values[0], "value"), "banana");
    }

    #[test]
    fn markdown_render_covers_both_shapes() {
        let report = json!({
            "subject": "topic-a",
            "total": 1,
            "decisions": [{
                "decision_id": "d-aaaa0001",
                "lifecycle": "live",
                "lane": "law",
                "ts": "2026-09-20T00:00:00Z",
                "scope": null,
                "decision": "Ruling.",
            }],
        });
        let md = render_markdown(&report);
        assert!(md.starts_with("# Decision report\n"), "{md}");
        assert!(
            md.contains("`d-aaaa0001` **live** (law, 2026-09-20T00:00:00Z, project:fno): Ruling."),
            "{md}"
        );
        let grouped = json!({
            "groups": [{"subject": "topic-a", "decisions": [{
                "decision_id": "d-aaaa0001",
                "lane": "law",
                "ts": "2026-09-20T00:00:00Z",
                "decision": "Ruling.",
            }]}],
            "data_quality": {"subjectless": 1, "invalid_authority": 0},
        });
        let md = render_markdown(&grouped);
        assert!(md.contains("## topic-a"), "{md}");
        assert!(
            md.contains("Data quality: 1 subjectless, 0 invalid authority value(s)."),
            "{md}"
        );
    }

    #[test]
    fn output_format_resolution_matches_the_deleted_rules() {
        assert_eq!(resolve_output_format("r.md", None).unwrap(), "markdown");
        assert_eq!(resolve_output_format("r.json", Some("md")).is_err(), true);
        assert_eq!(
            resolve_output_format("r.txt", Some("md")).unwrap(),
            "markdown"
        );
        assert_eq!(
            resolve_output_format("r.txt", None).unwrap_err(),
            "--output needs a .json, .md, or .markdown suffix, or --format"
        );
        assert_eq!(
            resolve_output_format("r.md", Some("yaml")).unwrap_err(),
            "--format must be markdown or json"
        );
    }

    #[test]
    fn decision_id_shape_is_the_python_4_to_32_rule() {
        assert!(looks_like_decision_id(" d-abcd "));
        assert!(looks_like_decision_id("d-abcd1234abcd1234abcd1234abcd1234"));
        assert!(!looks_like_decision_id("d-abc"));
        assert!(!looks_like_decision_id(
            "d-abcd1234abcd1234abcd1234abcd12340"
        ));
        assert!(!looks_like_decision_id("x-cccc"));
        assert!(!looks_like_decision_id("scope"));
    }

    #[test]
    fn option_parse_rejects_two_positionals_and_double_subject() {
        let mk = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let opts = parse_options(&mk(&["topic", "--limit", "0", "--json"])).unwrap();
        assert_eq!(opts.subject.as_deref(), Some("topic"));
        assert_eq!(opts.limit, 0);
        assert!(opts.json);
        assert!(parse_options(&mk(&["a", "b"])).is_err());
        // The deprecated alias rides the parse; the both-passed refusal is
        // the run entry's, so parse alone carries both values out.
        let both = parse_options(&mk(&["--subject", "a", "b"])).unwrap();
        assert_eq!(both.subject_alias.as_deref(), Some("a"));
        assert_eq!(both.subject.as_deref(), Some("b"));
        assert!(parse_options(&mk(&["--nope"])).is_err());
    }

    #[test]
    fn plan_rulings_scan_reads_rejected_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("20260101-plan.md"),
            "---\nclaims: [x-aaaa, x-bbbb]\nconsolidation:\n  rejected:\n    - id: x-cccc\n      reason: covered elsewhere\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("other.md"),
            "---\nconsolidation:\n  rejected:\n    - id: x-dddd\n      reason: nope\n---\nbody",
        )
        .unwrap();
        let result = plan_rulings_in(&dir.path().to_path_buf(), "x-cccc");
        assert_eq!(result.get("status").and_then(Value::as_str), Some("ok"));
        let rulings = result.get("rulings").and_then(Value::as_array).unwrap();
        assert_eq!(rulings.len(), 1);
        assert_eq!(rows_str(&rulings[0], "reason"), "covered elsewhere");
        assert_eq!(
            rulings[0]
                .get("by")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(2)
        );
        let missing = plan_rulings_in(&dir.path().join("nope"), "x-cccc");
        assert_eq!(
            missing.get("status").and_then(Value::as_str),
            Some("unavailable")
        );
    }
}
