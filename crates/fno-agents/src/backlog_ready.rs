//! The dispatch admission decision: which backlog nodes may be dispatched
//! right now, and in what order.
//!
//! Ported from the Python leg it retires: `cmd_ready`'s inline filters
//! (cli/src/fno/graph/cli.py) over `fno.backlog.explain`'s cascade, with the
//! shared helpers the two spellings consumed by hand - `selection_guards`,
//! `plan_rung`, `is_stale_ready`, `make_selection_sort_key`,
//! `filter_by_project`. One leg, in Rust, served over the keeper socket;
//! the Python callers become clients and the old leg is deleted in the same
//! change: a port that leaves a compatibility shell is not finished, so
//! every caller with no surviving consumer moved onto this leg.
//!
//! Three behaviors are contracts, not incidental, and are kept exactly:
//! `selection_guards` fails OPEN on missing/malformed data except the plan
//! hold, which fails CLOSED (an unreadable hold must never release one);
//! `plan_rung` resolves a relative `plan_path` against the NODE's `cwd`,
//! never the process cwd, and answers UNREADABLE - which fails open - when
//! there is no anchor; staleness degrades to 21 days on any config problem.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Lower priority number = more urgent (graph._constants.PRIORITY_ORDER).
const PRIORITY_ORDER: &[(&str, i64)] = &[("p0", 0), ("p1", 1), ("p2", 2), ("p3", 3)];
const VALID_STATUSES: &[&str] = &[
    "done",
    "deferred",
    "superseded",
    "in_review",
    "blocked",
    "in_progress",
    "idea",
    "design",
    "ready",
];
const EPIC_TERMINAL_EXCEPT_DONE: &[&str] = &["superseded", "deferred"];
const ROLLUP_TYPES: &[&str] = &["feature", "task"];
const CLOSED_STATUSES: &[&str] = &["done", "superseded", "deferred"];
/// The rungs the autonomous drain must NOT pick up (ladder.UNSELECTABLE_RUNGS).
const UNSELECTABLE_RUNGS: &[&str] = &["idea", "design"];
const PRIORITY_WEIGHT: &[(&str, i64)] = &[("p0", 1), ("p1", 2), ("p2", 3), ("p3", 4)];
const AGE_CAP_DAYS: f64 = 90.0;
const AGE_DIVISOR: f64 = 100.0;
/// Default staleness window when config is missing or unparseable
/// (`_guard_staleness_days`'s fail-open contract).
const DEFAULT_STALENESS_DAYS: i64 = 21;
/// Bound so a pathological parent chain can never spin the walks
/// (advance._MAX_ANCESTOR_WALK).
const MAX_ANCESTOR_WALK: usize = 64;

/// One `[backlog]` integer read from the `config.toml` in `config_dir`
/// (the graph's own `.fno` directory). `None` on a missing file, a missing
/// key, or a non-integer value.
pub fn backlog_config_int(config_dir: &std::path::Path, key: &str) -> Option<i64> {
    let raw = std::fs::read_to_string(config_dir.join("config.toml")).ok()?;
    let table = raw.parse::<toml::Table>().ok()?;
    table.get("backlog")?.as_table()?.get(key)?.as_integer()
}

/// `config.backlog.staleness_days`, read from the `config.toml` in
/// `config_dir` (the graph's own `.fno` directory). `None` on a missing,
/// malformed, or non-positive value, so every caller degrades to
/// [`DEFAULT_STALENESS_DAYS`] exactly as `_guard_staleness_days` did.
pub fn configured_staleness_days(config_dir: &std::path::Path) -> Option<i64> {
    let days = backlog_config_int(config_dir, "staleness_days")?;
    (days > 0).then_some(days)
}

// ---------------------------------------------------------------------------
// Options + reply
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ReadyOpts {
    pub project: Option<String>,
    pub all: bool,
    pub roadmap_id: Option<String>,
    pub parent: Option<String>,
    pub mission: Option<String>,
    pub include_ideas: bool,
    pub include_deferred: bool,
    /// The canonical checkout root, the anchor `detect_project` normalizes
    /// entry `cwd`s against when neither `--project` nor `--all` narrows the
    /// read. The caller resolves it (the Python client passes `repo_root()`).
    pub repo_root: Option<String>,
    /// Node ids holding a LIVE `node:<id>` claim, resolved by the caller from
    /// the claims store (`claims::list` + liveness) so the decision stays a
    /// pure function of entries + options.
    pub claimed: BTreeSet<String>,
    /// `config.backlog.staleness_days`, resolved by the caller. `None` (or a
    /// non-positive value) means "no config surface reached me" and the
    /// selection degrades to [`DEFAULT_STALENESS_DAYS`], the same fail-open
    /// contract `_guard_staleness_days` had.
    pub staleness_days: Option<i64>,
    /// The selection instant, epoch milliseconds UTC. now()-stamps never
    /// reach the projection; this only drives staleness and encounter age.
    pub now_ms: i64,
}

/// One narrowed-out candidate: the first cascade filter that removed it plus
/// the inner reason where the filter carries one (the selection-guard drops
/// name `dead-ancestor:<id>`, `design-stage`, `idea-stage`,
/// `stale-quarantine`, `contained:<id>`, `no-difficulty` or the hold
/// verdict's guard reason; every other drop reasons with its filter name).
#[derive(Debug, Clone)]
pub struct Drop {
    pub id: String,
    pub filter: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct ReadyReply {
    /// Dispatch summaries in selection order (21 keys, key order preserved).
    pub rows: Vec<Value>,
    pub drops: Vec<Drop>,
}

/// The parent scope resolved to nothing: the verb refuses (AC2-ERR of
/// `next`, same contract here).
#[derive(Debug)]
pub struct NoSuchParent(pub String);

// ---------------------------------------------------------------------------
// JSON helpers (Python-truthiness semantics)
// ---------------------------------------------------------------------------

fn is_dict(v: &Value) -> bool {
    v.is_object()
}

/// Python truthiness for the value shapes a graph row carries.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn get_str<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    e.get(key).and_then(Value::as_str)
}

fn entry_id(e: &Value) -> Option<&str> {
    e.get("id").and_then(Value::as_str)
}

fn priority_rank(name: &str) -> i64 {
    PRIORITY_ORDER
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
        .unwrap_or(2)
}

fn priority_name(e: &Value) -> String {
    match get_str(e, "priority") {
        Some(p) if PRIORITY_ORDER.iter().any(|(k, _)| *k == p) => p.to_string(),
        _ => "p2".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Timestamps (maintain._parse_ts / demand._parse_ts semantics)
// ---------------------------------------------------------------------------

/// ISO-8601 reader: `Z` or numeric offset, else naive reads UTC; an
/// unparseable stamp is no signal.
fn parse_iso_ms(value: &Value) -> Option<i64> {
    let s = match value {
        Value::String(s) => s.as_str(),
        _ => return None,
    };
    parse_iso_str(s)
}

fn parse_iso_str(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let normalized = s
        .strip_suffix('Z')
        .map(|rest| format!("{rest}+00:00"))
        .unwrap_or_else(|| s.to_string());
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.timestamp_millis());
    }
    let naive_formats = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d",
    ];
    for fmt in naive_formats {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&normalized, fmt) {
            return Some(dt.and_utc().timestamp_millis());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    None
}

/// Whole days from `ts` to `now`, flooring like Python's `timedelta.days`.
fn days_between(now_ms: i64, ts_ms: i64) -> i64 {
    (now_ms - ts_ms).div_euclid(86_400_000)
}

// ---------------------------------------------------------------------------
// Plan-document reading (ladder.py)
// ---------------------------------------------------------------------------

/// Filesystem path for a node's plan doc, or None when it has no usable one
/// (`ladder.resolve_plan_probe`): strip a `#anchor`, expand `~`, resolve a
/// relative path against the NODE's own `cwd`; with no anchor, refuse to
/// guess (the caller fails open).
pub(crate) fn resolve_plan_probe(entry: &Value) -> Option<PathBuf> {
    let plan_path = get_str(entry, "plan_path")?;
    if plan_path.is_empty() {
        return None;
    }
    let probe = plan_path.split('#').next().unwrap_or(plan_path);
    if probe.is_empty() {
        return None;
    }
    let expanded: String = if let Some(rest) = probe.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => format!("{home}/{rest}"),
            _ => return None,
        }
    } else if probe == "~" {
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => home,
            _ => return None,
        }
    } else {
        probe.to_string()
    };
    let p = PathBuf::from(&expanded);
    if p.is_absolute() {
        return Some(p);
    }
    let cwd = get_str(entry, "cwd").filter(|c| !c.is_empty())?;
    Some(PathBuf::from(cwd).join(p))
}

/// `(frontmatter, readable)` for the plan at `probe`
/// (`ladder._read_frontmatter`): an empty file, a missing closing fence,
/// malformed YAML, or non-mapping frontmatter is UNREADABLE; a doc with no
/// frontmatter, or empty frontmatter, reads as an empty mapping.
pub(crate) fn read_frontmatter(probe: &std::path::Path) -> Option<Map<String, Value>> {
    let text = match std::fs::read_to_string(probe) {
        Ok(t) => t,
        Err(_) => return None,
    };
    if text.trim().is_empty() {
        return None;
    }
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or("").trim();
    if first != "---" {
        return Some(Map::new());
    }
    let mut block = Vec::new();
    let mut closed = false;
    for line in lines {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        block.push(line);
    }
    if !closed {
        return None;
    }
    match serde_yaml_ng::from_str::<Value>(&block.join("\n")) {
        Ok(Value::Null) => Some(Map::new()),
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// `(status_scalar, readable)` for the plan at `probe`
/// (`ladder._read_status_scalar`): readable=False only when the document
/// itself cannot be parsed; a readable doc with no `status` answers
/// `(None, true)`.
fn read_status_scalar(probe: &std::path::Path) -> (Option<String>, bool) {
    match read_frontmatter(probe) {
        None => (None, false),
        Some(fm) => match fm.get("status") {
            None => (None, true),
            Some(Value::Null) => (Some(String::new()), true),
            Some(v) => (Some(yaml_scalar_string(v)), true),
        },
    }
}

/// `str(value)` for a YAML scalar, the way PyYAML types it and Python
/// stringifies it (booleans lowercase in YAML spelling, numbers bare).
fn yaml_scalar_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Where a node's plan sits (`ladder.plan_rung`). Never refuses: every
/// failure mode maps to a rung, and UNREADABLE fails open at its callers.
/// The rung strings match `graph_store::supplied_plan_rung`'s vocabulary.
pub fn plan_rung(entry: &Value) -> &'static str {
    if !is_dict(entry) {
        return "none";
    }
    match get_str(entry, "plan_path") {
        Some(p) if !p.is_empty() => {}
        _ => return "none",
    }
    let probe = match resolve_plan_probe(entry) {
        Some(p) => p,
        // A plan_path IS declared but cannot be resolved to a file - "cannot
        // tell", not "nothing on disk": fail open, never collapse into none.
        None => return "unreadable",
    };
    let (raw, readable) = read_status_scalar(&probe);
    if !readable {
        return "unreadable";
    }
    let raw = match raw {
        Some(r) => r,
        // Readable, but declares no status: READY, which is what every
        // surface derived for status-less docs before ladder.py existed.
        None => return "ready",
    };
    let s = raw.trim().trim_matches(['\'', '"']).to_lowercase();
    crate::graph_store::plan_rung_from_status(&s)
}

// ---------------------------------------------------------------------------
// The lifecycle verb decision (harness_map.resolve_effective_verb, ported)
// ---------------------------------------------------------------------------

/// The frontmatter kinds a build rung may advance on, read across both
/// `kind` and `type` (migrated plans spell `type: quick-plan`).
const BLUEPRINT_DOC_KINDS: &[&str] = &["quick-plan", "plan", "implementation-plan", "blueprint"];
/// A declared non-blueprint kind is a hard no above every body marker.
const NON_BLUEPRINT_DOC_KINDS: &[&str] = &["research", "findings", "think", "stub"];

/// Whether the linked doc is a blueprint: an executable plan
/// (`ladder.is_blueprint_doc`, ported). Never panics; every unanswerable
/// shape (no probe, unreadable probe, missing file, unreadable frontmatter)
/// is false. Deliberately does NOT read `status`: the rung stays
/// [`plan_rung`]'s answer, and the rung-authority CI guards that read.
pub fn is_blueprint_doc(entry: &Value) -> bool {
    if !is_dict(entry) {
        return false;
    }
    let Some(probe) = resolve_plan_probe(entry) else {
        return false;
    };
    let Some(fm) = read_frontmatter(&probe) else {
        return false;
    };
    for key in ["kind", "type"] {
        let Some(raw) = fm.get(key) else {
            continue;
        };
        let kind = yaml_scalar_string(raw).trim().to_lowercase();
        if NON_BLUEPRINT_DOC_KINDS.contains(&kind.as_str()) {
            return false;
        }
        if BLUEPRINT_DOC_KINDS.contains(&kind.as_str()) {
            return true;
        }
    }
    // No declared kind on either key: the body marker decides.
    match std::fs::read_to_string(&probe) {
        Ok(text) => text
            .lines()
            .any(|line| line.trim() == "## Execution Strategy"),
        Err(_) => false,
    }
}

/// The ported lifecycle table (`harness_map.resolve_effective_verb`):
/// `(verb, note)` per node row, or a refusal string. Order: refusals fire
/// BEFORE the declared verb; a declared target-family verb WINS and is
/// returned as declared (d-834b6ff1); an out-of-family verb abstains
/// (`None`) to declared precedence, so the command still routes through the
/// allowlist-checked `verb` rung. The word "reconciled" never appears.
pub fn effective_verb(entry: &Value) -> Result<(Option<String>, String), String> {
    let node_id = get_str(entry, "id").unwrap_or("unknown");
    let raw = get_str(entry, "dispatch_verb")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let canonical = |tok: &str| match crate::provider::parse_verb_token(tok) {
        Some((verb, _namespaced)) => format!("/{verb}"),
        None => tok.to_string(),
    };
    let declared = raw.as_deref().map(canonical);
    if let Some(declared) = &declared {
        if declared != "/target" && declared != "/blueprint" {
            return Ok((
                None,
                format!("verb=lifecycle(out-of-family {declared}; declared precedence holds)"),
            ));
        }
    }
    let refusal = |rung: &str, difficulty: &str| {
        format!(
            "dispatch verb cannot be derived for node {node_id}: plan rung \
             {rung:?} with difficulty {difficulty:?} answers no lifecycle rung"
        )
    };
    let rung = plan_rung(entry);
    let difficulty = get_str(entry, "difficulty")
        .map(|d| d.trim().to_lowercase())
        .unwrap_or_default();
    let answer = match rung {
        "none" => match difficulty.as_str() {
            "low" => ("/target", format!("intake difficulty=low")),
            "medium" => ("/blueprint", format!("intake difficulty=medium")),
            "high" => ("/blueprint", format!("intake difficulty=high")),
            _ => return Err(refusal(rung, &difficulty)),
        },
        "idea" | "design" => ("/blueprint", format!("plan {rung}")),
        "ready" | "in_progress" | "in_review" => {
            if is_blueprint_doc(entry) {
                ("/target", format!("plan {rung}"))
            } else {
                ("/blueprint", format!("plan {rung} not a blueprint"))
            }
        }
        _ => return Err(refusal(rung, &difficulty)),
    };
    if let Some(declared) = &declared {
        if declared != answer.0 {
            return Ok((
                Some(declared.clone()),
                format!(
                    "verb=declared({declared}; lifecycle answers {}: {})",
                    answer.0, answer.1
                ),
            ));
        }
    }
    Ok((
        Some(answer.0.to_string()),
        format!("verb=lifecycle({} -> {})", answer.1, answer.0),
    ))
}

/// The keeper's `effective_verb` body: one row in, its ported lifecycle
/// verb decision out (`{"verb", "note"}`), the refusal as the error so the
/// Python client raises without an unwrapping layer. Pure over the shipped
/// row and its linked plan doc. Kept beside the table so the decision and
/// its serving live in one file.
pub(crate) fn serve_effective_verb(params: &Value) -> Result<Value, String> {
    let entry = params
        .get("entries")
        .and_then(Value::as_array)
        .and_then(|entries| entries.first())
        .ok_or_else(|| "effective_verb needs entries[0]".to_string())?;
    let (verb, note) = effective_verb(entry)?;
    Ok(json!({ "verb": verb, "note": note }))
}

/// A plan-less idea the autonomous drain may dispatch without a plan
/// (`ladder.is_cold_dispatchable`): `status == "idea"` AND rung `none`, so
/// a linked decompose stub (rung `idea`) stays behind --include-ideas.
fn is_cold_dispatchable(e: &Value) -> bool {
    get_str(e, "status") == Some("idea") && plan_rung(e) == "none"
}

/// The ported lifecycle table ([`effective_verb`]) answers a planless node
/// only from these bands, trimmed and lowercased; any other value refuses.
fn has_intake_difficulty(e: &Value) -> bool {
    matches!(
        get_str(e, "difficulty")
            .map(|d| d.trim().to_ascii_lowercase())
            .as_deref(),
        Some("low" | "medium" | "high")
    )
}

// ---------------------------------------------------------------------------
// Dispatch holds (ladder.dispatch_hold + dispatch_hold_verdict)
// ---------------------------------------------------------------------------

/// A non-ABSENT hold on a plan: HELD or INVALID, both of which PARK the node
/// (the one fail-closed policy in this selector).
pub(crate) struct HoldVerdict {
    pub(crate) guard_reason: String,
    /// True for a validated `HoldState::Held` block; false for `Invalid`
    /// (a broken ruling still parks the node, but is not held proof).
    pub(crate) held: bool,
}

/// One plan's hold state (`ladder.dispatch_hold`).
pub(crate) enum HoldState {
    /// No declaration: no plan, no anchor, no file under an absent root, or
    /// no `dispatch_hold` key in readable frontmatter.
    Absent,
    /// A present, validated `dispatch_hold` block: the hold is ACTIVE.
    Held,
    /// The hold state was reached but cannot be read: a missing file under an
    /// existing root, unreadable frontmatter, a non-mapping block, or any
    /// required field missing, blank, or unparseable. Fails CLOSED.
    Invalid,
}

/// Read one plan's hold declaration (`ladder.dispatch_hold`).
pub(crate) fn dispatch_hold(entry: &Value) -> HoldState {
    let Some(probe) = resolve_plan_probe(entry) else {
        return HoldState::Absent;
    };
    if !probe.exists() {
        // A missing file under an EXISTING root is INVALID (stale path, typo,
        // mid-fetch checkout); only a root that is itself absent stays ABSENT.
        if probe.parent().map(|d| d.is_dir()).unwrap_or(false) {
            return HoldState::Invalid;
        }
        return HoldState::Absent;
    }
    let Some(fm) = read_frontmatter(&probe) else {
        return HoldState::Invalid;
    };
    let Some(block) = fm.get("dispatch_hold") else {
        return HoldState::Absent;
    };
    let Some(obj) = block.as_object() else {
        // A non-mapping dispatch_hold is invalid, not absent.
        return HoldState::Invalid;
    };
    // DispatchHoldBlock shape: four required fields; missing, blank, or
    // unparseable is INVALID (refuse, never raise).
    let str_field = |k: &str| -> Option<String> {
        match obj.get(k) {
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        }
    };
    if str_field("reason").is_none()
        || str_field("release_when").is_none()
        || str_field("set_by").is_none()
    {
        return HoldState::Invalid;
    }
    match obj.get("review_on") {
        Some(Value::String(s)) => {
            if chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").is_err() {
                return HoldState::Invalid;
            }
        }
        _ => return HoldState::Invalid,
    }
    HoldState::Held
}

/// Find a hold on a node, its parents, or its contained delivery owner
/// (`ladder.dispatch_hold_verdict`): bounded BFS, enqueue-time dedup, first
/// non-ABSENT verdict wins.
pub(crate) fn dispatch_hold_verdict(
    entry: &Value,
    by_id: &BTreeMap<String, Value>,
) -> Option<HoldVerdict> {
    if !is_dict(entry) {
        return None;
    }
    let mut queue: Vec<Value> = vec![entry.clone()];
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut enqueued: BTreeSet<String> = BTreeSet::new();
    let mut steps = 0usize;
    while !queue.is_empty() && steps < MAX_ANCESTOR_WALK {
        steps += 1;
        let current = queue.remove(0);
        let node_id = get_str(&current, "id").unwrap_or("unknown").to_string();
        if seen.contains(&node_id) {
            continue;
        }
        seen.insert(node_id.clone());
        let state = dispatch_hold(&current);
        if !matches!(state, HoldState::Absent) {
            let held = matches!(state, HoldState::Held);
            let prefix = if held {
                "dispatch-hold"
            } else {
                "dispatch-hold-invalid"
            };
            return Some(HoldVerdict {
                guard_reason: format!("{prefix}:{node_id}"),
                held,
            });
        }
        for relation in ["contained_in", "parent"] {
            if let Some(related) = get_str(&current, relation) {
                if !related.is_empty() && !seen.contains(related) && !enqueued.contains(related) {
                    if let Some(ancestor) = by_id.get(related).filter(|a| is_dict(a)) {
                        enqueued.insert(related.to_string());
                        queue.push(ancestor.clone());
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Staleness (maintain.py)
// ---------------------------------------------------------------------------

fn node_is_open(node: &Value) -> bool {
    if truthy(node.get("completed_at")) {
        return false;
    }
    let Some(successor) = node.get("superseded_by").filter(|s| truthy(Some(s))) else {
        return true;
    };
    let _ = successor;
    match node.get("supersession") {
        Some(rec) if rec.is_object() => !truthy(rec.get("verified_at")),
        _ => false,
    }
}

/// True when a ready node shows any sign of being live or recently worked
/// (`maintain.node_has_movement`).
fn node_has_movement(entry: &Value, now_ms: i64, staleness_days: i64) -> bool {
    if truthy(entry.get("sessions")) || truthy(entry.get("pr_number")) {
        return true;
    }
    if truthy(entry.get("locked_by")) || truthy(entry.get("locked_at")) {
        return true;
    }
    // An encounter inside the window is somebody saying this node cost them
    // time recently (`demand.recent_encounter`).
    if let Some(encounters) = entry.get("encounters").and_then(Value::as_array) {
        for r in encounters {
            if let Some(ts) = r.get("ts").and_then(parse_iso_ms) {
                if days_between(now_ms, ts) <= staleness_days {
                    return true;
                }
            }
        }
    }
    // The plan-file mtime probe is best-effort: a missing/unreadable plan is
    // simply no freshness signal, never an error.
    if let Some(probe) = resolve_plan_probe(entry) {
        if let Ok(meta) = std::fs::metadata(&probe) {
            if let Ok(mtime) = meta.modified() {
                let mt_ms = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as i64);
                if let Some(mt_ms) = mt_ms {
                    if days_between(now_ms, mt_ms) <= staleness_days {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Quarantine-eligible: abandoned, old, unmoved (`maintain.is_stale_ready`).
/// The caller guarantees ready status; containment, blockers, undesigned
/// rungs, movement and unprovable age each independently exempt.
fn is_stale_ready(entry: &Value, now_ms: i64, staleness_days: i64) -> bool {
    if truthy(entry.get("contained_in")) || truthy(entry.get("blocked_by")) {
        return false;
    }
    if UNSELECTABLE_RUNGS.contains(&plan_rung(entry)) {
        return false;
    }
    if node_has_movement(entry, now_ms, staleness_days) {
        return false;
    }
    let Some(created) = entry.get("created_at").and_then(parse_iso_ms) else {
        return false;
    };
    days_between(now_ms, created) > staleness_days
}

// ---------------------------------------------------------------------------
// Selection guards (advance.selection_guards)
// ---------------------------------------------------------------------------

/// Skip reason for a would-be-selected node, or None to select it. The plan
/// hold is checked FIRST and fails CLOSED; every later guard fails OPEN -
/// missing or malformed data selects normally, never starves live work.
fn selection_guards(
    entry: &Value,
    by_id: &BTreeMap<String, Value>,
    now_ms: i64,
    staleness_days: i64,
) -> Option<String> {
    if let Some(hold) = dispatch_hold_verdict(entry, by_id) {
        return Some(hold.guard_reason);
    }
    if let Some(owner) = get_str(entry, "contained_in") {
        if !owner.is_empty() {
            return Some(format!("contained:{owner}"));
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cur = get_str(entry, "parent").map(str::to_string);
    let mut steps = 0usize;
    while let Some(id) = cur {
        if steps >= MAX_ANCESTOR_WALK {
            break;
        }
        steps += 1;
        if seen.contains(&id) {
            break;
        }
        seen.insert(id.clone());
        let Some(anc) = by_id.get(&id) else {
            break;
        };
        if get_str(anc, "status")
            .map(|s| s == "superseded" || s == "deferred")
            .unwrap_or(false)
            || truthy(anc.get("superseded_by"))
            || truthy(anc.get("deferred_at"))
        {
            return Some(format!("dead-ancestor:{id}"));
        }
        cur = get_str(anc, "parent").map(str::to_string);
    }
    if get_str(entry, "status") == Some("ready") {
        let rung = plan_rung(entry);
        if UNSELECTABLE_RUNGS.contains(&rung) {
            return Some(if rung == "design" {
                "design-stage".to_string()
            } else {
                "idea-stage".to_string()
            });
        }
        if is_stale_ready(entry, now_ms, staleness_days) {
            return Some("stale-quarantine".to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Project scoping (_intake.filter_by_project / detect_project)
// ---------------------------------------------------------------------------

/// `os.path.normpath` + `expanduser` for the cwd strings detect_project
/// compares: collapse `//`, `.` and `..`, strip a trailing separator,
/// expand a leading `~`.
fn normalize_cwd(raw: &str) -> String {
    let expanded = if raw == "~" {
        std::env::var("HOME").unwrap_or_else(|_| raw.to_string())
    } else if let Some(rest) = raw.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => format!("{home}/{rest}"),
            _ => raw.to_string(),
        }
    } else {
        raw.to_string()
    };
    let absolute = expanded.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in expanded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if !parts.is_empty() && parts[parts.len() - 1] != ".." {
                    parts.pop();
                } else if absolute {
                    // /.. normalizes to /
                } else {
                    parts.push("..");
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Detect the active project from candidate `cwd`s
/// (`_intake.detect_project`): an exact-root node names the project
/// outright; else the first node under the root does; else None.
pub(crate) fn detect_project(entries: &[Value], repo_root: &str) -> Option<String> {
    let norm_root = normalize_cwd(repo_root);
    let root_prefix = format!("{}/", norm_root.trim_end_matches('/'));
    let mut fallback: Option<String> = None;
    for e in entries {
        let Some(cwd) = e.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if cwd.is_empty() {
            continue;
        }
        let norm = normalize_cwd(cwd);
        if norm == norm_root {
            return e.get("project").and_then(Value::as_str).map(str::to_string);
        }
        if fallback.is_none() && norm.starts_with(&root_prefix) {
            fallback = e.get("project").and_then(Value::as_str).map(str::to_string);
        }
    }
    fallback
}

/// `filter_by_project`'s narrowing rule, one row at a time against a project
/// already resolved by detection or flag: an explicit project filters,
/// --all shows everything, detection returning nothing shows everything.
fn row_matches_project(e: &Value, project: Option<&str>) -> bool {
    match project {
        Some(p) => e.get("project").and_then(Value::as_str) == Some(p),
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Graph navigation (_intake.descendants_of / _find_node)
// ---------------------------------------------------------------------------

/// Transitive children of `parent_id` via the `parent` field; cycle-safe;
/// the parent itself never included.
fn descendants_of(entries: &[Value], parent_id: &str) -> BTreeSet<String> {
    let mut children_by_parent: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in entries {
        if let (Some(id), Some(pid)) = (entry_id(e), get_str(e, "parent")) {
            children_by_parent.entry(pid).or_default().push(id);
        }
    }
    let mut result: BTreeSet<String> = BTreeSet::new();
    let mut frontier: Vec<&str> = children_by_parent
        .get(parent_id)
        .cloned()
        .unwrap_or_default();
    while let Some(current) = frontier.pop() {
        if result.contains(current) || current == parent_id {
            continue;
        }
        result.insert(current.to_string());
        if let Some(kids) = children_by_parent.get(current) {
            frontier.extend(kids.iter().copied());
        }
    }
    result
}

/// `_find_node`: exact id first, format-agnostic (resolve_id's first step:
/// fixtures and legacy data use non-hex ids), then a unique short `ab-`
/// prefix (4-7 hex in the suffix, `resolve_id`'s partial-prefix gate);
/// malformed prefixes, ambiguity, and absence all read as no match.
fn find_node<'a>(entries: &'a [Value], node_id: &str) -> Option<&'a Value> {
    if let Some(exact) = entries.iter().find(|e| entry_id(e) == Some(node_id)) {
        return Some(exact);
    }
    if node_id.starts_with("ab-") && node_id.len() < 11 {
        let suffix = &node_id[3..];
        let is_partial =
            (4..=7).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_hexdigit());
        if !is_partial {
            return None;
        }
        let matches: Vec<&Value> = entries
            .iter()
            .filter(|e| entry_id(e).map(|i| i.starts_with(node_id)).unwrap_or(false))
            .collect();
        return if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        };
    }
    None
}

// ---------------------------------------------------------------------------
// Rollup orphans (rollup.orphan_ids)
// ---------------------------------------------------------------------------

fn has_epic_ancestor(entry: &Value, by_id: &BTreeMap<String, Value>) -> bool {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut current = get_str(entry, "parent").map(str::to_string);
    while let Some(id) = current {
        if seen.contains(&id) {
            break;
        }
        seen.insert(id.clone());
        let Some(parent) = by_id.get(&id) else {
            return false;
        };
        if get_str(parent, "type") == Some("epic") {
            return true;
        }
        current = get_str(parent, "parent").map(str::to_string);
    }
    false
}

fn is_orphan(entry: &Value, by_id: &BTreeMap<String, Value>) -> bool {
    if !get_str(entry, "type")
        .map(|t| ROLLUP_TYPES.contains(&t))
        .unwrap_or(false)
    {
        return false;
    }
    if truthy(entry.get("orphan_ok")) {
        return false;
    }
    if get_str(entry, "status")
        .map(|s| CLOSED_STATUSES.contains(&s))
        .unwrap_or(false)
    {
        return false;
    }
    !has_epic_ancestor(entry, by_id)
}

fn orphan_ids(entries: &[Value], by_id: &BTreeMap<String, Value>) -> BTreeSet<String> {
    entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| entry_id(e))
        .filter(|id| by_id.get(*id).map(|e| is_orphan(e, by_id)).unwrap_or(false))
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Demand signal (demand.importance_score)
// ---------------------------------------------------------------------------

fn voter_key(record: &Value) -> String {
    record
        .get("voter_key")
        .and_then(Value::as_str)
        .or_else(|| record.get("session_id").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn encounter_voters(entry: &Value) -> BTreeSet<String> {
    entry
        .get("encounters")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|r| r.is_object())
                .map(voter_key)
                .filter(|k| !k.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn importance_score(entry: &Value, effective_priority: &str, now_ms: i64) -> f64 {
    let voters = encounter_voters(entry);
    if voters.is_empty() {
        return 0.0;
    }
    let weight = PRIORITY_WEIGHT
        .iter()
        .find(|(k, _)| *k == effective_priority)
        .map(|(_, v)| *v as f64)
        .unwrap_or(3.0);
    let weight = if !truthy(entry.get("sessions")) && !truthy(entry.get("pr_number")) {
        weight * 2.0
    } else {
        weight
    };
    let divergence = voters.len() as f64 * weight;
    let stamp = entry
        .get("touched_at")
        .and_then(parse_iso_ms)
        .or_else(|| entry.get("created_at").and_then(parse_iso_ms));
    let Some(stamp) = stamp else {
        return divergence;
    };
    let days = days_between(now_ms, stamp).max(0) as f64;
    divergence + days.min(AGE_CAP_DAYS) / AGE_DIVISOR
}

// ---------------------------------------------------------------------------
// Epics-first ordering (graph/_intake.py)
// ---------------------------------------------------------------------------

fn rank_band(entry: &Value) -> (i64, f64) {
    match entry.get("rank") {
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f.is_finite() => (0, f),
            _ => (1, 0.0),
        },
        _ => (1, 0.0),
    }
}

fn epics_with_child_progress(by_id: &BTreeMap<String, Value>) -> BTreeSet<String> {
    let mut progressing = BTreeSet::new();
    for child in by_id.values() {
        let Some(parent_id) = get_str(child, "parent") else {
            continue;
        };
        if progressing.contains(parent_id) {
            continue;
        }
        if truthy(child.get("completed_at"))
            || get_str(child, "status")
                .map(|s| s == "done" || s == "in_progress")
                .unwrap_or(false)
            || truthy(child.get("session_id"))
        {
            progressing.insert(parent_id.to_string());
        }
    }
    progressing
}

/// The node's live epic parent, or None (`_intake._live_epic_for`): an
/// epic-typed parent with a valid priority and status, not explicitly
/// terminal and not status-terminal without child progress.
fn live_epic_for(
    node: &Value,
    by_id: &BTreeMap<String, Value>,
    child_progress: &BTreeSet<String>,
) -> Option<Value> {
    let parent_id = get_str(node, "parent")?;
    let epic = by_id.get(parent_id)?;
    if get_str(epic, "type") != Some("epic") {
        return None;
    }
    let epic_priority = get_str(epic, "priority")?;
    if !PRIORITY_ORDER.iter().any(|(k, _)| *k == epic_priority) {
        return None;
    }
    if let Some(status) = get_str(epic, "status") {
        if !VALID_STATUSES.contains(&status) {
            return None;
        }
    }
    if let Some(created_at) = epic.get("created_at") {
        if !created_at.is_null() && !created_at.is_string() {
            return None;
        }
    }
    let explicitly_terminal = truthy(epic.get("completed_at"))
        || truthy(epic.get("superseded_by"))
        || truthy(epic.get("deferred_at"));
    let status = get_str(epic, "status");
    let status_terminal = status
        .map(|s| {
            EPIC_TERMINAL_EXCEPT_DONE.contains(&s)
                || (s == "done" && !child_progress.contains(parent_id))
        })
        .unwrap_or(false);
    if explicitly_terminal || status_terminal {
        return None;
    }
    Some(epic.clone())
}

fn in_progress_epic_ids(
    entries: &[Value],
    by_id: &BTreeMap<String, Value>,
    child_progress: &BTreeSet<String>,
    live_claimed: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    for child in entries {
        if !is_dict(child) {
            continue;
        }
        let moving = truthy(child.get("completed_at"))
            || get_str(child, "status")
                .map(|s| s == "done" || s == "in_progress")
                .unwrap_or(false)
            || truthy(child.get("session_id"))
            || entry_id(child)
                .map(|id| live_claimed.contains(id))
                .unwrap_or(false);
        if !moving {
            continue;
        }
        if let Some(epic) = live_epic_for(child, by_id, child_progress) {
            if let Some(id) = entry_id(&epic) {
                result.insert(id.to_string());
            }
        }
    }
    result
}

fn make_effective_priority(
    by_id: &BTreeMap<String, Value>,
    child_progress: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (id, e) in by_id {
        let child_priority = priority_name(e);
        let effective = match live_epic_for(e, by_id, child_progress) {
            Some(epic) => {
                let epic_priority = priority_name(&epic);
                if priority_rank(&epic_priority) < priority_rank(&child_priority) {
                    epic_priority
                } else {
                    child_priority
                }
            }
            None => child_priority,
        };
        out.insert(id.clone(), effective);
    }
    out
}

/// The one selection sort key (`_intake.make_selection_sort_key`, no
/// swimlane term): curated rank leads, then epic-children tier before loose,
/// then priority, fan-out, orphan, evidence, age. Terms are compared
/// position-wise like Python's tuples; the epic and loose branches only ever
/// diverge at the tier term, so the tail terms never compare across branches.
fn selection_sort_key(
    node: &Value,
    by_id: &BTreeMap<String, Value>,
    child_progress: &BTreeSet<String>,
    dependents: &BTreeMap<String, i64>,
    effective_priority: &BTreeMap<String, String>,
    orphans: &BTreeSet<String>,
    epic_in_progress: &BTreeSet<String>,
    now_ms: i64,
) -> Vec<Term> {
    let node = if is_dict(node) { node } else { &Value::Null };
    let band = rank_band(node);
    let child_prio = priority_rank(&priority_name(node));
    let node_id = entry_id(node).unwrap_or("");
    let child_orphan = !node_id.is_empty() && orphans.contains(node_id);
    let child_created = get_str(node, "created_at").unwrap_or("").to_string();
    let child_created_clone = child_created.clone();
    let fanout = -dependents.get(node_id).copied().unwrap_or(0);
    let score = {
        let prio = effective_priority
            .get(node_id)
            .cloned()
            .unwrap_or_else(|| priority_name(node));
        // Degrade like every ordering signal: a malformed encounter never
        // breaks selection.
        -importance_score(node, &prio, now_ms)
    };
    let epic = live_epic_for(node, by_id, child_progress);
    match epic {
        Some(epic) => {
            // Python parity: the membership probe and the leading band both
            // name the EPIC (the parent id, the epic's rank), never the
            // child; the child's band only orders among siblings.
            let parent_id = get_str(node, "parent").unwrap_or("");
            let in_progress_rank = if epic_in_progress.contains(parent_id) {
                0
            } else {
                1
            };
            let epic_band = rank_band(&epic);
            let epic_prio = priority_rank(&priority_name(&epic));
            let epic_created = get_str(&epic, "created_at").unwrap_or("").to_string();
            vec![
                Term::Band(epic_band.0, OrdF64(epic_band.1)),
                Term::I(0),
                Term::I(in_progress_rank),
                Term::I(epic_prio),
                Term::S(epic_created),
                Term::Band(band.0, OrdF64(band.1)),
                Term::I(child_prio),
                Term::I(fanout),
                Term::B(child_orphan),
                Term::F(OrdF64(score)),
                Term::S(child_created),
            ]
        }
        None => vec![
            Term::Band(band.0, OrdF64(band.1)),
            Term::I(1),
            Term::I(0),
            Term::I(child_prio),
            Term::I(fanout),
            Term::B(child_orphan),
            Term::F(OrdF64(score)),
            Term::S(child_created),
            Term::I(child_prio),
            Term::I(fanout),
            Term::S(child_created_clone),
        ],
    }
}

/// One comparable sort term. Cross-variant order is arbitrary by design:
/// the positions where two branches could meet are decided by the tier term
/// before any variant mismatch is reached.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Term {
    B(bool),
    I(i64),
    F(OrdF64),
    S(String),
    Band(i64, OrdF64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdF64(f64);

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

// ---------------------------------------------------------------------------
// The projection (cli._dispatch_node_summary)
// ---------------------------------------------------------------------------

/// The ONE projection a dispatcher sees when it picks work: the union of
/// every field either dispatch surface carried, key order preserved.
fn dispatch_node_summary(e: &Value) -> Value {
    let mut out = Map::new();
    let keys = [
        "slug",
        "id",
        "title",
        "priority",
        "domain",
        "project",
        "cwd",
        "parent",
        "size",
        "difficulty",
        "plan_path",
        "model",
        "dispatch_verb",
        "dispatch_brief",
        "mission_id",
        "mission_wave",
        "mission_slug",
        "mission_from_msg_id",
        "created_at",
        "touched_at",
        "rank",
    ];
    for key in keys {
        out.insert(key.to_string(), e.get(key).cloned().unwrap_or(Value::Null));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// Cascade filter names, in `build_selection_filters`' shipped order. The
/// survivor set is order-independent (every filter is a per-row predicate
/// over the admitted candidates); attribution is to the FIRST filter that
/// removes a node.
const FILTER_ROADMAP: &str = "roadmap";
const FILTER_MISSION: &str = "mission";
const FILTER_PARENT_SCOPE: &str = "parent-scope";
const FILTER_PROJECT: &str = "project";
const FILTER_LIVE_CLAIM: &str = "live-claim";
const FILTER_UNMERGED_OPEN_PR: &str = "unmerged-open-pr";
const FILTER_CONTAINER: &str = "container";
const FILTER_BATCHED: &str = "batched";
const FILTER_SELECTION_GUARD: &str = "selection-guard";

/// The guarded predicate a candidate must satisfy per filter, with the drop
/// reason when it fails. `detected_project` is resolved once per cascade
/// from the survivor list at that position (detection narrows a LIST).
fn drops_for_filter(
    filter: &str,
    e: &Value,
    ctx: &SelectCtx,
    detected_project: Option<&str>,
) -> Option<String> {
    match filter {
        FILTER_ROADMAP => {
            let want = ctx.opts.roadmap_id.as_deref()?;
            if get_str(e, "roadmap_id") == Some(want) {
                None
            } else {
                Some(FILTER_ROADMAP.to_string())
            }
        }
        FILTER_MISSION => {
            let want = ctx.opts.mission.as_deref()?;
            if get_str(e, "mission_id") == Some(want) {
                None
            } else {
                Some(FILTER_MISSION.to_string())
            }
        }
        FILTER_PARENT_SCOPE => {
            let scope = ctx.parent_scope.as_ref()?;
            match entry_id(e) {
                Some(id) if scope.contains(id) => None,
                _ => Some(FILTER_PARENT_SCOPE.to_string()),
            }
        }
        FILTER_PROJECT => {
            // No explicit project and not --all: detection narrowed the
            // list. Detection returning nothing shows everything.
            if row_matches_project(e, detected_project) {
                None
            } else {
                Some(FILTER_PROJECT.to_string())
            }
        }
        FILTER_LIVE_CLAIM => match entry_id(e) {
            Some(id) if ctx.opts.claimed.contains(id) => Some(FILTER_LIVE_CLAIM.to_string()),
            _ => None,
        },
        FILTER_UNMERGED_OPEN_PR => {
            // Scoped to ready status: an explicitly --include-deferred /
            // --ideas paused PR-bearing row still surfaces.
            if get_str(e, "status") == Some("ready")
                && !truthy(e.get("completed_at"))
                && truthy(e.get("pr_number"))
            {
                Some(FILTER_UNMERGED_OPEN_PR.to_string())
            } else {
                None
            }
        }
        FILTER_CONTAINER => match entry_id(e) {
            Some(id) if ctx.container_ids.contains(id) => Some(FILTER_CONTAINER.to_string()),
            _ => None,
        },
        FILTER_BATCHED => {
            if truthy(e.get("batch")) {
                Some(FILTER_BATCHED.to_string())
            } else {
                None
            }
        }
        FILTER_SELECTION_GUARD => {
            selection_guards(e, &ctx.by_id, ctx.opts.now_ms, ctx.staleness_days).or_else(|| {
                // A cold idea the verb derivation is certain to refuse must
                // not spend a drain tick: drop it where every autonomous
                // dispatcher reads, attributed for `advance --explain`.
                // A scoped call (--parent / --mission / --roadmap-id) is an
                // enumeration, not the drain head: the epic fan-out must
                // still surface cold ideas and decides per row, where one
                // refusal costs a failed spawn, never the tick.
                let scoped = ctx.opts.parent.is_some()
                    || ctx.opts.mission.is_some()
                    || ctx.opts.roadmap_id.is_some();
                (!scoped
                    && !ctx.opts.include_ideas
                    && is_cold_dispatchable(e)
                    && !has_intake_difficulty(e))
                .then(|| "no-difficulty".to_string())
            })
        }
        _ => None,
    }
}

struct SelectCtx {
    opts: ReadyOpts,
    repo_root: String,
    staleness_days: i64,
    parent_scope: Option<BTreeSet<String>>,
    container_ids: BTreeSet<String>,
    by_id: BTreeMap<String, Value>,
}

/// The decision, in one call: admit, narrow through the cascade, rank.
///
/// Returns survivors as dispatch summaries plus per-node drops. `next`'s
/// answer is `rows[0]` of the same call (AC3: the two surfaces cannot
/// drift).
pub fn select(entries: &[Value], opts: &ReadyOpts) -> Result<ReadyReply, NoSuchParent> {
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| entry_id(e).map(|id| (id.to_string(), e.clone())))
        .collect();

    // --parent: resolve the epic up-front; a missing node is a hard error.
    let parent_scope = match opts.parent.as_deref() {
        None => None,
        Some(parent) => {
            let target =
                find_node(entries, parent).ok_or_else(|| NoSuchParent(parent.to_string()))?;
            let target_id = entry_id(target).unwrap_or(parent).to_string();
            Some(descendants_of(entries, &target_id))
        }
    };

    let repo_root = opts.repo_root.clone().unwrap_or_default();
    let staleness_days = opts
        .staleness_days
        .filter(|days| *days > 0)
        .unwrap_or(DEFAULT_STALENESS_DAYS);
    let ctx = SelectCtx {
        opts: opts.clone(),
        repo_root,
        staleness_days,
        parent_scope,
        container_ids: container_ids(entries),
        by_id: by_id.clone(),
    };

    // Admission: persisted status in the allowed set (ready, plus idea /
    // deferred only on their flags) or cold-dispatchable, and not closed out
    // of band (read_graph does not recompute status).
    let mut allowed: BTreeSet<&str> = BTreeSet::from(["ready"]);
    if opts.include_ideas {
        allowed.insert("idea");
    }
    if opts.include_deferred {
        allowed.insert("deferred");
    }
    let candidates: Vec<Value> = entries
        .iter()
        .filter(|e| is_dict(e))
        .filter(|e| {
            (get_str(e, "status")
                .map(|s| allowed.contains(s))
                .unwrap_or(false)
                || is_cold_dispatchable(e))
                && !truthy(e.get("completed_at"))
        })
        .cloned()
        .collect();

    // The cascade, in its shipped order, with first-filter attribution.
    let filter_order = [
        FILTER_ROADMAP,
        FILTER_MISSION,
        FILTER_PARENT_SCOPE,
        FILTER_PROJECT,
        FILTER_LIVE_CLAIM,
        FILTER_UNMERGED_OPEN_PR,
        FILTER_CONTAINER,
        FILTER_BATCHED,
        FILTER_SELECTION_GUARD,
    ];
    let mut survivors: Vec<Value> = candidates;
    let mut drops: Vec<Drop> = Vec::new();
    for filter in filter_order {
        // The live-claim filter only exists when claims are held, matching
        // the cascade's conditional construction.
        if filter == FILTER_LIVE_CLAIM && ctx.opts.claimed.is_empty() {
            continue;
        }
        // Detection narrows a LIST, not a row: resolve the effective project
        // once from the survivor list at this cascade position, exactly
        // where `filter_by_project` runs its detection.
        let effective_project = if filter == FILTER_PROJECT {
            match ctx.opts.project.as_deref() {
                Some(p) => Some(p.to_string()),
                None if !ctx.opts.all => detect_project(&survivors, &ctx.repo_root),
                None => None,
            }
        } else {
            None
        };
        let mut kept = Vec::new();
        for e in survivors {
            match drops_for_filter(filter, &e, &ctx, effective_project.as_deref()) {
                Some(reason) => {
                    if let Some(id) = entry_id(&e) {
                        drops.push(Drop {
                            id: id.to_string(),
                            filter: filter.to_string(),
                            reason,
                        });
                    }
                }
                None => kept.push(e),
            }
        }
        survivors = kept;
    }

    // Epics-first, then flat priority: the key is built from the FULL graph
    // so epic parents resolve even when filtered out of the candidate set.
    let (ordered, _tables) =
        order_by_selection_key(survivors, entries, &by_id, &opts.claimed, opts.now_ms);

    Ok(ReadyReply {
        rows: ordered.iter().map(|e| dispatch_node_summary(e)).collect(),
        drops,
    })
}

/// The sort tables the board mode reads, built once per read from the full
/// graph. The key's other three inputs (child progress, fan-out, orphans)
/// stay local to [`order_by_selection_key`]: nothing reads them back.
struct KeyTables {
    effective_priority: BTreeMap<String, String>,
    epic_in_progress: BTreeSet<String>,
}

/// The one selection order: `rows` sorted by [`selection_sort_key`], with the
/// tables it was built from handed back so the board mode answers from the
/// same facts instead of rebuilding them.
fn order_by_selection_key(
    rows: Vec<Value>,
    entries: &[Value],
    by_id: &BTreeMap<String, Value>,
    claimed: &BTreeSet<String>,
    now_ms: i64,
) -> (Vec<Value>, KeyTables) {
    let child_progress = epics_with_child_progress(by_id);
    let dependents = dependents_fanout(entries);
    let effective_priority = make_effective_priority(by_id, &child_progress);
    let orphans = orphan_ids(entries, by_id);
    let epic_in_progress = in_progress_epic_ids(entries, by_id, &child_progress, claimed);
    let mut keyed: Vec<(Vec<Term>, Value)> = rows
        .into_iter()
        .map(|e| {
            let key = selection_sort_key(
                &e,
                by_id,
                &child_progress,
                &dependents,
                &effective_priority,
                &orphans,
                &epic_in_progress,
                now_ms,
            );
            (key, e)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let tables = KeyTables {
        effective_priority,
        epic_in_progress,
    };
    let ordered = keyed.into_iter().map(|(_, e)| e).collect();
    (ordered, tables)
}

/// The board's whole-graph facts, from the tables the ready order uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardFacts {
    /// Every entry id, in selection-key order, no admission, no cascade.
    pub ids: Vec<String>,
    /// Epics whose work is underway (in_progress_epic_ids).
    pub underway: BTreeSet<String>,
    /// Ids whose live epic promotes their priority, to the promoted value.
    pub effective_priority: BTreeMap<String, String>,
}

/// Whole-graph board facts: every dict entry with an id, in selection-key
/// order. No admission and no cascade - the board lists everything and its
/// renderer filters. `effective_priority` keeps only ids whose live epic
/// actually promotes them, so the reply stays small.
pub fn board_facts(entries: &[Value], claimed: &BTreeSet<String>, now_ms: i64) -> BoardFacts {
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| entry_id(e).map(|id| (id.to_string(), e.clone())))
        .collect();
    let sortables: Vec<Value> = by_id.values().cloned().collect();
    let (ordered, tables) = order_by_selection_key(sortables, entries, &by_id, claimed, now_ms);
    let ids = ordered
        .iter()
        .filter_map(|e| entry_id(e).map(str::to_string))
        .collect();
    let effective_priority: BTreeMap<String, String> = tables
        .effective_priority
        .into_iter()
        .filter(|(id, effective)| {
            by_id
                .get(id)
                .is_some_and(|e| &priority_name(e) != effective)
        })
        .collect();
    BoardFacts {
        ids,
        underway: tables.epic_in_progress,
        effective_priority,
    }
}

/// Ids of nodes that are some other node's `parent` (`cli._container_ids`):
/// the parent pointer must not be the node's own containment mark.
fn container_ids(entries: &[Value]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for e in entries {
        if !is_dict(e) {
            continue;
        }
        if let Some(p) = get_str(e, "parent") {
            if get_str(e, "contained_in") != Some(p) {
                ids.insert(p.to_string());
            }
        }
    }
    ids
}

/// How many OPEN nodes wait on each id (`make_selection_sort_key`'s fan-out).
fn dependents_fanout(entries: &[Value]) -> BTreeMap<String, i64> {
    let mut dependents: BTreeMap<String, i64> = BTreeMap::new();
    for e in entries {
        if !is_dict(e) || !node_is_open(e) {
            continue;
        }
        if let Some(blockers) = e.get("blocked_by").and_then(Value::as_array) {
            for b in blockers {
                if let Some(bid) = b.as_str() {
                    *dependents.entry(bid.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    dependents
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts(include_ideas: bool) -> ReadyOpts {
        ReadyOpts {
            all: true,
            include_ideas,
            ..Default::default()
        }
    }

    #[test]
    fn no_difficulty_cold_idea_drops_and_next_ranked_row_wins() {
        let entries = vec![
            json!({"id": "x-a", "status": "idea", "priority": "p1"}),
            json!({"id": "x-b", "status": "idea", "priority": "p2", "difficulty": "low"}),
        ];
        let reply = select(&entries, &opts(false)).unwrap();
        assert_eq!(reply.rows.len(), 1);
        assert_eq!(reply.rows[0].get("id"), Some(&json!("x-b")));
        let drop = reply
            .drops
            .iter()
            .find(|d| d.id == "x-a")
            .expect("x-a dropped");
        assert_eq!(drop.filter, "selection-guard");
        assert_eq!(drop.reason, "no-difficulty");
    }

    #[test]
    fn difficulty_with_surrounding_whitespace_survives() {
        let entries = vec![
            json!({"id": "x-a", "status": "idea", "priority": "p1", "difficulty": " Medium "}),
        ];
        let reply = select(&entries, &opts(false)).unwrap();
        assert_eq!(reply.rows.len(), 1);
        assert_eq!(reply.rows[0].get("id"), Some(&json!("x-a")));
        assert!(reply.drops.is_empty());
    }

    #[test]
    fn include_ideas_lists_the_no_difficulty_idea_without_a_drop() {
        let entries = vec![json!({"id": "x-a", "status": "idea", "priority": "p1"})];
        let reply = select(&entries, &opts(true)).unwrap();
        assert_eq!(reply.rows.len(), 1);
        assert_eq!(reply.rows[0].get("id"), Some(&json!("x-a")));
        assert!(reply.drops.iter().all(|d| d.reason != "no-difficulty"));
    }

    #[test]
    fn dead_ancestor_wins_over_no_difficulty() {
        let entries = vec![
            json!({"id": "x-a", "status": "idea", "priority": "p1", "parent": "x-p"}),
            json!({"id": "x-p", "status": "deferred", "priority": "p1"}),
        ];
        let reply = select(&entries, &opts(false)).unwrap();
        assert!(reply.rows.is_empty());
        let drop = reply
            .drops
            .iter()
            .find(|d| d.id == "x-a")
            .expect("x-a dropped");
        assert_eq!(drop.filter, "selection-guard");
        assert_eq!(drop.reason, "dead-ancestor:x-p");
    }

    #[test]
    fn scoped_parent_call_still_enumerates_cold_ideas_without_difficulty() {
        let entries = vec![
            json!({"id": "x-e", "status": "ready", "priority": "p1", "type": "epic"}),
            json!({"id": "x-c", "status": "idea", "priority": "p2", "parent": "x-e"}),
        ];
        let reply = select(
            &entries,
            &ReadyOpts {
                all: true,
                parent: Some("x-e".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reply.rows.len(), 1);
        assert_eq!(reply.rows[0].get("id"), Some(&json!("x-c")));
        assert!(reply.drops.iter().all(|d| d.reason != "no-difficulty"));
    }

    // -- the board mode: whole-graph facts in selection order --

    #[test]
    fn board_mode_orders_every_entry_like_selection() {
        let entries = vec![
            json!({"id": "x-e", "status": "ready", "priority": "p1", "type": "epic"}),
            json!({"id": "x-c1", "status": "ready", "priority": "p2", "parent": "x-e"}),
            json!({"id": "x-c2", "status": "done", "priority": "p2", "parent": "x-e",
                   "completed_at": "2026-09-01T00:00:00Z"}),
            json!({"id": "x-loose", "status": "ready", "priority": "p1"}),
        ];
        let reply = select(&entries, &opts(true)).unwrap();
        let facts = board_facts(&entries, &Default::default(), 0);
        // The board lists everything; the admitted subsequence must match
        // selection's own order exactly.
        let admitted: Vec<&str> = reply
            .rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        let in_board_order: Vec<&str> = facts
            .ids
            .iter()
            .filter(|id| admitted.contains(&id.as_str()))
            .map(|id| id.as_str())
            .collect();
        assert_eq!(in_board_order, admitted);
        assert!(facts.ids.contains(&"x-e".to_string()));
        assert!(facts.ids.contains(&"x-c2".to_string()));
        assert!(facts.underway.contains("x-e"));
        assert_eq!(
            facts.effective_priority.get("x-c1"),
            Some(&"p1".to_string())
        );
        assert_eq!(facts.effective_priority.get("x-loose"), None);
    }

    // -- the ported lifecycle table (effective_verb) + is_blueprint_doc --

    fn plan_at(dir: &std::path::Path, name: &str, body: &str) -> String {
        let probe = dir.join(name);
        std::fs::write(&probe, body).unwrap();
        probe.to_str().unwrap().to_string()
    }

    fn row_with(plan_path: &str, extra: serde_json::Map<String, Value>) -> Value {
        let mut row = serde_json::Map::new();
        row.insert("id".into(), json!("x-test"));
        row.insert("plan_path".into(), json!(plan_path));
        if let Some(cwd) = std::path::Path::new(plan_path)
            .parent()
            .map(|d| d.to_str().unwrap().to_string())
        {
            row.insert("cwd".into(), json!(cwd));
        }
        for (k, v) in extra {
            row.insert(k, v);
        }
        Value::Object(row)
    }

    #[test]
    fn blueprint_doc_answers_true_for_declared_blueprint_kinds() {
        let dir = tempfile::tempdir().unwrap();
        for (name, fm) in [
            ("qp-kind.md", "kind: quick-plan\nstatus: ready\n"),
            ("qp-type.md", "type: quick-plan\n"),
            ("plan.md", "kind: plan\n"),
            ("impl.md", "kind: implementation-plan\n"),
            ("bp.md", "type: blueprint\n"),
        ] {
            let probe = plan_at(dir.path(), name, &format!("---\n{fm}---\n\n# Doc\n"));
            let row = row_with(&probe, serde_json::Map::new());
            assert!(is_blueprint_doc(&row), "{name} should read as a blueprint");
        }
    }

    #[test]
    fn execution_strategy_heading_alone_is_true() {
        let dir = tempfile::tempdir().unwrap();
        let probe = plan_at(
            dir.path(),
            "body.md",
            "# No frontmatter\n\n## Execution Strategy\n\n- step\n",
        );
        let row = row_with(&probe, serde_json::Map::new());
        assert!(is_blueprint_doc(&row));
    }

    #[test]
    fn declared_research_kind_is_a_hard_no_above_the_heading() {
        let dir = tempfile::tempdir().unwrap();
        let probe = plan_at(
            dir.path(),
            "research.md",
            "---\nkind: research\nstatus: ready\n---\n\n## Execution Strategy\n\n- step\n",
        );
        let row = row_with(&probe, serde_json::Map::new());
        assert!(!is_blueprint_doc(&row));
    }

    #[test]
    fn type_feature_with_strategy_heading_is_true() {
        let dir = tempfile::tempdir().unwrap();
        let probe = plan_at(
            dir.path(),
            "feature.md",
            "---\ntype: feature\n---\n\n## Execution Strategy\n\n- step\n",
        );
        let row = row_with(&probe, serde_json::Map::new());
        assert!(is_blueprint_doc(&row));
    }

    #[test]
    fn unanswerable_docs_read_false_without_panicking() {
        // A non-dict row, a row with no plan_path, a relative probe with no
        // cwd, a missing file, and a non-UTF8 file all read false.
        assert!(!is_blueprint_doc(&json!("not-an-entry")));
        assert!(!is_blueprint_doc(&json!({"id": "x-test"})));
        assert!(!is_blueprint_doc(
            &json!({"id": "x-test", "plan_path": "d.md"})
        ));
        let dir = tempfile::tempdir().unwrap();
        let missing = row_with(
            dir.path().join("absent.md").to_str().unwrap(),
            serde_json::Map::new(),
        );
        assert!(!is_blueprint_doc(&missing));
        assert!(!is_blueprint_doc(&json!({"id": "x-test", "plan_path": 42})));
        let binary = dir.path().join("binary.md");
        std::fs::write(&binary, b"\xff\xfe\x00\x80 not utf-8").unwrap();
        let binary_row = row_with(binary.to_str().unwrap(), serde_json::Map::new());
        assert!(!is_blueprint_doc(&binary_row));
    }

    #[test]
    fn ready_rung_research_doc_derives_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        let probe = plan_at(
            dir.path(),
            "research.md",
            "---\nkind: research\nstatus: ready\n---\n# findings\n",
        );
        let row = row_with(&probe, serde_json::Map::new());
        let (verb, note) = effective_verb(&row).unwrap();
        assert_eq!(verb.as_deref(), Some("/blueprint"));
        assert!(
            note.contains("plan ready not a blueprint -> /blueprint"),
            "{note}"
        );
    }

    #[test]
    fn ready_rung_quick_plan_derives_target() {
        let dir = tempfile::tempdir().unwrap();
        let probe = plan_at(
            dir.path(),
            "qp.md",
            "---\nkind: quick-plan\nstatus: ready\n---\n# contract\n",
        );
        let row = row_with(&probe, serde_json::Map::new());
        let (verb, note) = effective_verb(&row).unwrap();
        assert_eq!(verb.as_deref(), Some("/target"));
        assert!(note.contains("plan ready -> /target"), "{note}");
    }

    #[test]
    fn declared_blueprint_on_ready_blueprint_doc_wins_naming_lifecycle_answer() {
        let dir = tempfile::tempdir().unwrap();
        let probe = plan_at(
            dir.path(),
            "qp.md",
            "---\nkind: quick-plan\nstatus: ready\n---\n# contract\n",
        );
        let row = row_with(
            &probe,
            serde_json::Map::from_iter([("dispatch_verb".to_string(), json!("/fno:blueprint"))]),
        );
        let (verb, note) = effective_verb(&row).unwrap();
        assert_eq!(verb.as_deref(), Some("/blueprint"));
        assert!(
            note.contains("verb=declared(/blueprint; lifecycle answers /target: plan ready)"),
            "{note}"
        );
    }

    #[test]
    fn declared_think_abstains_with_the_out_of_family_note() {
        let row = json!({"id": "x-test", "difficulty": "high", "dispatch_verb": "/fno:think"});
        let (verb, note) = effective_verb(&row).unwrap();
        assert!(verb.is_none());
        assert!(note.contains("out-of-family /think"), "{note}");
    }

    #[test]
    fn planless_node_without_valid_difficulty_refuses_naming_the_node() {
        let row = json!({"id": "x-noplan", "difficulty": "spicy"});
        let refusal = effective_verb(&row).unwrap_err();
        assert!(
            refusal.starts_with("dispatch verb cannot be derived for node x-noplan:"),
            "{refusal}"
        );
        assert!(!refusal.contains("(x-"), "{refusal}");
    }

    #[test]
    fn dollar_namespaced_declared_verb_canonicalizes_like_the_slash_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let probe = plan_at(dir.path(), "idea.md", "---\nstatus: idea\n---\n# idea\n");
        for spelling in ["/fno:blueprint", "$fno:blueprint"] {
            let row = row_with(
                &probe,
                serde_json::Map::from_iter([("dispatch_verb".to_string(), json!(spelling))]),
            );
            let (verb, _note) = effective_verb(&row).unwrap();
            assert_eq!(verb.as_deref(), Some("/blueprint"), "{spelling}");
        }
    }

    #[test]
    fn serve_effective_verb_answers_the_row_and_the_refusal_is_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let research = plan_at(
            dir.path(),
            "research.md",
            "---\nkind: research\nstatus: ready\n---\n# findings\n",
        );
        let qp = plan_at(
            dir.path(),
            "qp.md",
            "---\nkind: quick-plan\nstatus: ready\n---\n# contract\n",
        );
        let research_row = row_with(
            &research,
            serde_json::Map::from_iter([("dispatch_verb".to_string(), json!("/fno:blueprint"))]),
        );
        let qp_row = row_with(&qp, serde_json::Map::new());
        let reply = serve_effective_verb(&json!({"entries": [research_row, qp_row]})).unwrap();
        assert_eq!(reply["verb"], json!("/blueprint"));
        // Declared /blueprint agrees with the not-a-blueprint answer, so the
        // note takes the lifecycle form; the declared form is for a
        // disagreement (the declared_blueprint_on_ready test above).
        assert!(reply["note"]
            .as_str()
            .unwrap()
            .contains("verb=lifecycle(plan ready not a blueprint -> /blueprint)"));
        assert_eq!(
            serve_effective_verb(&json!({"entries": [qp_row]})).unwrap()["verb"],
            json!("/target")
        );
        let refusal = serve_effective_verb(&json!({"entries": [
            json!({"id": "x-planless", "difficulty": "spicy"})
        ]}))
        .unwrap_err();
        assert!(
            refusal.starts_with("dispatch verb cannot be derived for node x-planless:"),
            "{refusal}"
        );
    }
}
