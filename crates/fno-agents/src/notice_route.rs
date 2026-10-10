//! The notice router: daemon arm that routes lead-scope warnings to the
//! session that owns them, plus the sent-fingerprint store the owner
//! ladder's load read counts.
//!
//! Two jobs live here: routing reconcile warnings (promise gate,
//! canonical sync, orphan plans) to the owning lead as one deduped mail,
//! and folding repeated failure events and repeated banners into one
//! owned node. The shared sent store is `~/.fno/notice-route/sent.json`:
//! fingerprint -> {scope, ts}, pruned on every write.

use serde_json::Value;
use sha2::Digest;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// A sent notice is "open" for the load read for this long.
pub(crate) const NOTICE_OPEN_SECS: i64 = 86_400;

/// `~/.fno/notice-route/`, beside `~/.fno` like `attention/`.
pub(crate) fn notice_route_dir() -> Result<PathBuf, String> {
    crate::paths::AgentsHome::from_env()
        .root()
        .parent()
        .map(|p| p.join("notice-route"))
        .ok_or_else(|| "notice_route: no agents home parent".to_string())
}

fn sent_path() -> Result<PathBuf, String> {
    Ok(notice_route_dir()?.join("sent.json"))
}

/// The sent store as `{fingerprint: {scope, ts}}`. A malformed file reads as
/// empty: the store is dedupe memory, never truth.
fn read_sent() -> BTreeMap<String, Value> {
    let Ok(path) = sent_path() else {
        return BTreeMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    serde_json::from_str::<BTreeMap<String, Value>>(&raw).unwrap_or_default()
}

fn write_sent(map: &BTreeMap<String, Value>) {
    let Ok(path) = sent_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if serde_json::to_string(map)
        .map(|body| std::fs::write(&tmp, body))
        .is_ok()
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Record one sent notice: the fingerprint and the owner scope it went to.
/// Older-than-open entries are pruned on the same write.
pub(crate) fn record_sent(fingerprint: &str, scope: &str, now: i64) {
    let mut map = read_sent();
    map.retain(|_, v| NOTICE_OPEN_SECS > now - v.get("ts").and_then(Value::as_i64).unwrap_or(0));
    map.insert(
        fingerprint.to_string(),
        serde_json::json!({"scope": scope, "ts": now}),
    );
    write_sent(&map);
}

/// Open notices per scope, from ONE read of the sent store: the second
/// load term the owner ladder's least-loaded rung reads. A repeat within
/// the open window counts again on purpose: a scope piling identical
/// warnings IS the loaded one.
pub(crate) fn open_notice_counts() -> BTreeMap<String, u32> {
    let now = now_epoch();
    let mut out: BTreeMap<String, u32> = BTreeMap::new();
    for v in read_sent().into_values() {
        let Some(scope) = v.get("scope").and_then(Value::as_str) else {
            continue;
        };
        if NOTICE_OPEN_SECS > now - v.get("ts").and_then(Value::as_i64).unwrap_or(0) {
            *out.entry(scope.to_string()).or_insert(0) += 1;
        }
    }
    out
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The arm's beat, matching its KNOWN_ARMS row.
pub const NOTICE_ROUTE_INTERVAL_S: u64 = 300;

/// The fold leg's own cadence: hourly, inside the same arm.
pub(crate) const FOLD_INTERVAL_S: u64 = 3_600;

/// The arm as the daemon holds it: cadence stamps plus one-in-flight
/// gate. The config cwd rides at construction, so the territory resolve
/// reads the same root the daemon declared.
pub struct Arm {
    config_cwd: std::path::PathBuf,
    last_tick: std::sync::Mutex<Option<std::time::Instant>>,
    last_fold: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    in_flight: Arc<std::sync::atomic::AtomicBool>,
}

impl Arm {
    pub fn new(config_cwd: std::path::PathBuf) -> Self {
        Self {
            config_cwd,
            last_tick: std::sync::Mutex::new(None),
            last_fold: Arc::new(std::sync::Mutex::new(None)),
            in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// The due-check and one-in-flight gate, the lead_wake shape; the body
/// runs off-loop. The routing pass beats every 300s, the fold hourly.
pub fn maybe_tick(arm: &Arm, home: crate::paths::AgentsHome) {
    let interval = std::time::Duration::from_secs(NOTICE_ROUTE_INTERVAL_S);
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < interval)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    let last_fold_gate = Arc::clone(&arm.last_fold);
    let cwd = arm.config_cwd.clone();
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let outcome = match run_pass(&home, &cwd) {
            Ok(o) => o,
            Err(e) => crate::lead_wake::Outcome {
                acted: 0,
                skip_reason: Some("unreadable".to_string()),
                detail: e.chars().take(160).collect(),
            },
        };
        let (fold_acted, fold_skip) = maybe_fold(&last_fold_gate, &home, &cwd);
        let acted = outcome.acted + fold_acted;
        let detail = match &fold_skip {
            Some(s) => format!("{}; fold: {}", outcome.detail, s),
            None => outcome.detail,
        };
        let skip = if acted > 0 {
            None
        } else {
            outcome.skip_reason.or(fold_skip)
        };
        emit_tick_row(&home, acted, skip.as_deref(), &detail);
    });
}

/// The hourly gate: past the fold cadence, read the world and run the
/// fold. A world read fault skips the leg without stamping.
fn maybe_fold(
    last_fold: &Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    home: &crate::paths::AgentsHome,
    config_cwd: &Path,
) -> (u64, Option<String>) {
    {
        let mut last = last_fold.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(FOLD_INTERVAL_S)) {
            return (0, None);
        }
        *last = Some(std::time::Instant::now());
    }
    let w = match crate::owner_ladder::world(home, config_cwd) {
        Ok(w) => w,
        Err(_) => return (0, Some("world_unreadable".to_string())),
    };
    fold_tick(&w)
}

fn emit_tick_row(
    home: &crate::paths::AgentsHome,
    acted: u64,
    skip_reason: Option<&str>,
    detail: &str,
) {
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    crate::tick_ledger::emit_tick(
        &journal,
        "notice_route",
        crate::tick_ledger::SCHED_DAEMON,
        acted,
        skip_reason,
        Some(detail),
        NOTICE_ROUTE_INTERVAL_S,
    );
}

/// One lead-scope warning lifted from a reconcile result file: the kind
/// (the bucket the hook used to render), the node it names when it has
/// one, and the cause text the hook printed, word for word.
pub(crate) struct Warning {
    pub(crate) kind: &'static str,
    pub(crate) node: Option<String>,
    pub(crate) cause: String,
}

/// Read `.fno/.reconcile-result.json` into warnings. The same buckets the
/// deleted hook render block surfaced, so the cause text the lead's mail
/// carries is the text the hook printed.
pub(crate) fn read_reconcile(raw: &str) -> Vec<Warning> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let node_list = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("node_id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for node in node_list("promise_unmet") {
        out.push(Warning {
            kind: "promise_unmet",
            node: Some(node.clone()),
            cause: format!(
                "last sweep held 1 node open on the promise gate ({node}). Run `fno backlog reconcile` for the per-node cause (unharvested carve-out / failed close_probe / short ship count); resolve, or close with --force --reason."
            ),
        });
    }
    for node in node_list("promise_unknown") {
        out.push(Warning {
            kind: "promise_unknown",
            node: Some(node.clone()),
            cause: format!(
                "last sweep could not read the ship count for 1 node ({node}); they stay open. The read failed retryably, so a later sweep clears them by itself. Do not force these closed - the count is unconfirmed, not short."
            ),
        });
    }
    let sync = v.get("sync_catchup");
    let outcome = sync
        .and_then(|s| s.get("outcome"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = sync
        .and_then(|s| s.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let stale = sync
        .and_then(|s| s.get("stale"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let needs_wording = outcome.contains("failed")
        || outcome.contains("unknown")
        || outcome.contains("error")
        || outcome.contains("marked")
        || outcome.contains("skipped")
        || (stale && outcome != "synced");
    if needs_wording {
        let tail = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        };
        out.push(Warning {
            kind: "sync_catchup",
            node: None,
            cause: format!(
                "canonical-sync catch-up {outcome}{tail}. The canonical checkout may be behind; run `fno doctor` for the outcome-keyed report."
            ),
        });
    }
    out
}

/// Read `.fno/.orphan-plans-result.json` into warnings: the bound and held
/// verdicts, plus the plans_dir the held line names.
pub(crate) fn read_orphan(raw: &str) -> (Vec<Warning>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return (Vec::new(), None);
    };
    let rows = v.get("rows").and_then(Value::as_array);
    let plans_dir = v
        .get("plans_dir")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut out: Vec<Warning> = Vec::new();
    let Some(rows) = rows else {
        return (out, plans_dir);
    };
    for row in rows {
        let Some(id) = row.get("node_id").and_then(Value::as_str) else {
            continue;
        };
        let verdict = row.get("verdict").and_then(Value::as_str).unwrap_or("");
        match verdict {
            "bound_now" => out.push(Warning {
                kind: "orphan_bound",
                node: Some(id.to_string()),
                cause: format!("bound 1 orphan plan(s) to their nodes ({id})."),
            }),
            "unfinalized" | "ambiguous" | "id_reuse" | "bind_failed" => out.push(Warning {
                kind: "orphan_held",
                node: Some(id.to_string()),
                cause: format!(
                    "1 plan(s) claim a node that is still unbound ({id}: {verdict}). Run fno-agents backlog-orphan-plans --plans-dir {} for the per-plan reason.",
                    plans_dir.as_deref().unwrap_or("?")
                ),
            }),
            _ => {}
        }
    }
    (out, plans_dir)
}

/// One owner's grouped warnings, routed as one mail or one question page.
struct OwnerGroup {
    owner: crate::owner_ladder::Owner,
    warnings: Vec<Warning>,
}

/// Route the warnings: group by owner, one deduped mail per live lead
/// (the cause text rides word for word), one question page when the rung
/// is the user. Returns the acted count plus the group keys whose
/// outcome is terminal: delivered now, or already delivered inside the
/// dedupe window. Only terminal groups may consume their source files.
fn route_groups(
    groups: BTreeMap<String, OwnerGroup>,
    now: i64,
    detail: &mut Vec<String>,
) -> (u64, std::collections::BTreeSet<String>) {
    let mut acted = 0u64;
    let mut terminal = std::collections::BTreeSet::new();
    for (key, group) in groups {
        let fingerprint = fingerprint_of(&group.warnings);
        if sent_recently(&fingerprint) {
            terminal.insert(key);
            continue;
        }
        let delivered = match group.owner.session.as_deref() {
            Some(sid) if !sid.trim().is_empty() => mail_owner(sid, &group, &fingerprint, now),
            _ => file_question_page(&group, &fingerprint, now),
        };
        if delivered {
            acted += 1;
            terminal.insert(key);
            // A least-loaded pick is a ruling: record it once per
            // delivered group, naming the scope and the load (AC3).
            if group.owner.rung == crate::owner_ladder::Rung::LeastLoaded {
                let node = group.warnings.iter().find_map(|w| w.node.clone());
                crate::owner_ladder::record_authority_pick(
                    node.as_deref(),
                    &group.owner,
                    &format!(
                        "notice router routed {} warning(s); scope load {}",
                        group.warnings.len(),
                        group.owner.scope.as_deref().unwrap_or("?")
                    ),
                );
            }
        } else {
            detail.push(format!(
                "undelivered: {} warning(s) for {}",
                group.warnings.len(),
                group.owner.scope.as_deref().unwrap_or("?")
            ));
        }
    }
    (acted, terminal)
}

/// The dedupe key: sha256 over the sorted kind:node pairs. A repeat inside
/// the open window is not resent (AC11).
fn fingerprint_of(warnings: &[Warning]) -> String {
    let mut pairs: Vec<String> = warnings
        .iter()
        .map(|w| format!("{}:{}", w.kind, w.node.as_deref().unwrap_or("")))
        .collect();
    pairs.sort();
    let joined = pairs.join("|");
    let digest = sha2::Sha256::digest(joined.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex
}

/// True when the fingerprint was sent inside the open window.
fn sent_recently(fingerprint: &str) -> bool {
    let now = now_epoch();
    let map = read_sent();
    match map.get(fingerprint) {
        Some(v) => NOTICE_OPEN_SECS > now - v.get("ts").and_then(Value::as_i64).unwrap_or(0),
        None => false,
    }
}

/// One mail per owner through the system-sender lane with the resume
/// fallback. Records the fingerprint only on a delivered mail.
fn mail_owner(sid: &str, group: &OwnerGroup, fingerprint: &str, now: i64) -> bool {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "Automatic notice routing from the fno daemon: {} reconcile warning(s) for your territory.",
        group.warnings.len()
    ));
    for w in &group.warnings {
        lines.push(format!("- [{}] {}", w.kind, w.cause));
    }
    let text = lines.join("\n");
    let mut runner: crate::burn_watch::Runner = &mut crate::burn_watch::run_command;
    let (sent, _lane) = crate::burn_watch::wake_with_text(
        sid,
        false,
        &text,
        crate::system_sender::system_name("notice-router").as_str(),
        &mut runner,
    );
    if sent {
        record_sent(fingerprint, group.owner.scope.as_deref().unwrap_or(""), now);
    }
    sent
}

/// The user-rung leg: one question page per warning group, filed through
/// `fno inbox outstanding ask --question-file`. The page carries the same
/// cause text and the required intake sections.
fn file_question_page(group: &OwnerGroup, fingerprint: &str, now: i64) -> bool {
    let dir = match notice_route_dir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let page_path = dir.join(format!("q-{}.md", &fingerprint[..8]));
    let mut body = String::new();
    body.push_str("---\nrecommend: 1\n---\n\n");
    body.push_str("Reconcile warnings your territory needs a ruling on.\n\n");
    body.push_str("## Options\n1. I will resolve the named nodes this beat.\n");
    body.push_str("    What happens next: the warnings stop re-surfacing\n");
    body.push_str("2. Route them to another owner.\n");
    body.push_str("    What happens next: the notice router re-ranks them\n\n");
    body.push_str("## Blocked because\n");
    body.push_str("reconcile held nodes open with no session to route them to\n\n");
    body.push_str("## Why these options\n");
    body.push_str("the owner ladder reached the user rung: no live lead qualifies\n\n");
    body.push_str("## Downside\n");
    body.push_str("the warnings repeat until a lead resolves them\n\n");
    body.push_str("## Recommendation\n");
    body.push_str("Option 1: resolve the named nodes.\n\n");
    body.push_str("## Not thought through\n");
    body.push_str("none\n\n");
    body.push_str("## Reversible\ncostly\n\n");
    body.push_str("## Cost if wrong\n");
    body.push_str("a held node stays open and its PR unmergable\n\n");
    body.push_str("## Meanwhile\nstops\n\n");
    let mut causes = String::new();
    for w in &group.warnings {
        causes.push_str("- [");
        causes.push_str(w.kind);
        causes.push_str("] ");
        causes.push_str(&w.cause);
        causes.push('\n');
    }
    body.push_str("## Warnings\n\n");
    body.push_str(&causes);
    if std::fs::write(&page_path, body).is_err() {
        return false;
    }
    let mut cmd = crate::loop_dispatch::fno_cmd("fno");
    cmd.args(["inbox", "outstanding", "ask", "--question-file"]);
    cmd.arg(&page_path);
    if let Some(node) = group.warnings.iter().find_map(|w| w.node.as_deref()) {
        cmd.arg("--node");
        cmd.arg(node);
    }
    let ran = crate::bounded_cmd::output_with_timeout_result(cmd, 30);
    let ok = ran.map(|o| o.status.success()).unwrap_or(false);
    if ok {
        record_sent(fingerprint, group.owner.scope.as_deref().unwrap_or(""), now);
    }
    ok
}

/// The pass: read the world once, walk the workspace project roots'
/// `.fno/.reconcile-result.json` and `.fno/.orphan-plans-result.json`,
/// group, route, dedupe, and rename each consumed file to `.shown` (the
/// rename the hook did before). A world read fault skips the pass.
pub(crate) fn run_pass(
    home: &crate::paths::AgentsHome,
    config_cwd: &Path,
) -> Result<crate::lead_wake::Outcome, String> {
    let w = crate::owner_ladder::world(home, config_cwd)?;
    let now = w.now;
    let mut groups: BTreeMap<String, OwnerGroup> = BTreeMap::new();
    let mut detail: Vec<String> = Vec::new();
    let mut consumed: Vec<(std::path::PathBuf, Vec<String>)> = Vec::new();
    let paths = crate::territory::workspace_paths(config_cwd);
    // Every workspace project root, plus the config root itself: a repo
    // outside the workspace map still sweeps, and its result must reach an
    // owner instead of sitting unconsumed.
    let mut roots: Vec<std::path::PathBuf> = paths.values().map(|p| PathBuf::from(p)).collect();
    roots.push(config_cwd.to_path_buf());
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for root in roots {
        let fno_dir = Path::new(&root).join(".fno");
        if !seen.insert(fno_dir.clone()) {
            continue;
        }
        if let Some((path, fed)) = consume_file(
            &fno_dir.join(".reconcile-result.json"),
            &w,
            &mut groups,
            ReconcileReader,
        ) {
            consumed.push((path, fed));
        }
        if let Some((path, fed)) = consume_file(
            &fno_dir.join(".orphan-plans-result.json"),
            &w,
            &mut groups,
            OrphanReader,
        ) {
            consumed.push((path, fed));
        }
    }
    let (acted, terminal) = route_groups(groups, now, &mut detail);
    // A result file is consumed only when every owner group it fed is
    // terminal (delivered now, or inside the dedupe window): one
    // undelivered group keeps the file on disk for the next tick, and the
    // groups that did deliver dedupe through sent_recently.
    for (path, fed) in consumed {
        if fed.iter().all(|k| terminal.contains(k)) {
            let shown = path.with_extension("json.shown");
            let _ = std::fs::rename(&path, shown);
        }
    }
    if acted == 0 && detail.is_empty() {
        return Ok(crate::lead_wake::Outcome {
            acted: 0,
            skip_reason: Some("nothing_to_route".to_string()),
            detail: "no unread reconcile results".to_string(),
        });
    }
    Ok(crate::lead_wake::Outcome {
        acted,
        skip_reason: None,
        detail: detail.join("; ").chars().take(160).collect(),
    })
}

/// The per-file reader seam, so the two result shapes share one consume
/// loop and tests drive both without disk.
struct ReconcileReader;
struct OrphanReader;

trait ResultReader {
    fn read(&self, raw: &str) -> Vec<Warning>;
}

impl ResultReader for ReconcileReader {
    fn read(&self, raw: &str) -> Vec<Warning> {
        read_reconcile(raw)
    }
}

impl ResultReader for OrphanReader {
    fn read(&self, raw: &str) -> Vec<Warning> {
        read_orphan(raw).0
    }
}

/// Read one result file, group its warnings under their owners, and return
/// the path plus the group keys it fed, for the caller to rename once
/// every fed group is terminal. A file that does not exist, or that
/// parses to nothing, contributes nothing.
fn consume_file(
    path: &Path,
    w: &crate::owner_ladder::World,
    groups: &mut BTreeMap<String, OwnerGroup>,
    reader: impl ResultReader,
) -> Option<(std::path::PathBuf, Vec<String>)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let warnings = reader.read(&raw);
    if warnings.is_empty() {
        return None;
    }
    let mut fed: Vec<String> = Vec::new();
    for warning in warnings {
        let owner = crate::owner_ladder::resolve(
            &crate::owner_ladder::Ask {
                node: warning.node.as_deref(),
                from_session: None,
                start: if warning.node.is_none() {
                    crate::owner_ladder::Rung::LeastLoaded
                } else {
                    crate::owner_ladder::Rung::NodeLead
                },
            },
            w,
        );
        let key = owner.scope.clone().unwrap_or_else(|| "user".to_string());
        if !fed.contains(&key) {
            fed.push(key.clone());
        }
        groups
            .entry(key)
            .or_insert(OwnerGroup {
                owner,
                warnings: Vec::new(),
            })
            .warnings
            .push(warning);
    }
    Some((path.to_path_buf(), fed))
}

// --- repeat-failure and banner fold (change 4) -----------------------------

/// One journal row the fold reads, normalized for the key. `node` is the
/// row's own `data.node` when the failure carried one.
pub(crate) struct FoldRow {
    pub(crate) ts: i64,
    pub(crate) etype: String,
    pub(crate) error: String,
    pub(crate) node: Option<String>,
}

/// The fold's memory: one entry per failure key, persisted at
/// `~/.fno/notice-route/fold.json`. It answers "is there already a node
/// for this key" without a graph search; the node still carries the
/// `failure-key:<sha8>` tag a human can grep.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct FoldState {
    pub(crate) keys: BTreeMap<String, KeyState>,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct KeyState {
    pub(crate) node: Option<String>,
    pub(crate) created_at: Option<i64>,
    pub(crate) completed_at: Option<i64>,
    pub(crate) last_encounter_ts: i64,
    pub(crate) last_question_ts: i64,
}

/// What the fold decided for one key this pass. The shell layer executes
/// these; the pure core decides them, and the tests read them.
#[derive(Debug, PartialEq)]
pub(crate) enum FoldAction {
    FileNode {
        key: String,
        first_ts: i64,
        last_ts: i64,
        count: u64,
        hint: Option<String>,
    },
    Encounter {
        key: String,
        node: String,
        count: u64,
    },
    Reopen {
        key: String,
        node: String,
        count: u64,
    },
    Question {
        key: String,
        node: String,
    },
}

/// The fold's key: event type plus the normalized error (digits, hex runs,
/// uuids and absolute paths replaced by one token, first 160 chars). A
/// banner row keys as `banner_repeated:<producer id>` (AC17).
pub(crate) fn fold_key(etype: &str, error: &str) -> String {
    if etype == "banner" {
        return format!("banner_repeated:{}", error.trim());
    }
    format!("{}:{}", etype, normalize_error(error))
}

pub(crate) fn normalize_error(error: &str) -> String {
    let trimmed = error.trim();
    let mut out = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            // Swallow the digit run, then a following hex run: an id like
            // 2727709ed7 or 0x9f3a collapses to one token either way.
            while chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                chars.next();
            }
            out.push('#');
            continue;
        }
        out.push(c);
    }
    out.chars().take(160).collect()
}

/// True when the type is allowlisted: `*_failed` or `*_dropped`. Banner
/// keys (`banner_repeated:<id>`) count as failures too (AC17), and the
/// steady-noise types enter here so their own 3x-average gate can run.
pub(crate) fn foldable(etype: &str) -> bool {
    etype == "banner"
        || steady_noise(etype)
        || etype.ends_with("_failed")
        || etype.ends_with("_dropped")
}

/// The steady-noise types: they file only when the 24h count exceeds three
/// times the 7-day daily average.
pub(crate) fn steady_noise(etype: &str) -> bool {
    etype == "transition_rejected" || etype == "graph_status_drift"
}

/// The pure fold core over the trailing 7 days of rows. One decision per
/// key with enough rows to act on; the shell layer executes the actions
/// (create, encounter, reopen, mail, question) and the tests read them.
pub(crate) fn fold_pass(rows: &[FoldRow], state: &mut FoldState, now: i64) -> Vec<FoldAction> {
    let mut actions: Vec<FoldAction> = Vec::new();
    let mut by_key: BTreeMap<String, Vec<&FoldRow>> = BTreeMap::new();
    for row in rows {
        if !foldable(&row.etype) {
            continue;
        }
        by_key
            .entry(fold_key(&row.etype, &row.error))
            .or_default()
            .push(row);
    }
    for (key, key_rows) in &by_key {
        let etype = key.split(':').next().unwrap_or("");
        let c24: Vec<&&FoldRow> = key_rows
            .iter()
            .filter(|r| NOTICE_OPEN_SECS > now - r.ts && now >= r.ts)
            .collect();
        let prior: Vec<&&FoldRow> = key_rows
            .iter()
            .filter(|r| {
                let age = now - r.ts;
                NOTICE_OPEN_SECS <= age && 2 * NOTICE_OPEN_SECS > age
            })
            .collect();
        let week = key_rows.len() as u64;
        let n24 = c24.len() as u64;
        if n24 == 0 {
            continue;
        }
        if steady_noise(etype) && n24 <= 3 * (week / 7) {
            continue;
        }
        let first_ts = c24.iter().map(|r| r.ts).min().unwrap_or(now);
        let last_ts = c24.iter().map(|r| r.ts).max().unwrap_or(now);
        let entry = state.keys.get(key).cloned();
        // The row's own node hint: a failure naming its node routes to
        // that node's owner even before the fold's own node exists.
        let hint = c24.iter().find_map(|r| r.node.clone());
        match entry {
            None => {
                if n24 >= 3 {
                    actions.push(FoldAction::FileNode {
                        key: key.clone(),
                        first_ts,
                        last_ts,
                        count: n24,
                        hint,
                    });
                }
            }
            Some(ks) => match (&ks.node, ks.completed_at) {
                (Some(node), Some(done_at)) => {
                    if last_ts > done_at {
                        actions.push(FoldAction::Reopen {
                            key: key.clone(),
                            node: node.clone(),
                            count: n24,
                        });
                    }
                }
                (Some(node), None) => {
                    if n24 >= 2 {
                        actions.push(FoldAction::Encounter {
                            key: key.clone(),
                            node: node.clone(),
                            count: n24,
                        });
                    }
                }
                (None, _) => {
                    if n24 >= 3 {
                        actions.push(FoldAction::FileNode {
                            key: key.clone(),
                            first_ts,
                            last_ts,
                            count: n24,
                            hint,
                        });
                    }
                }
            },
        }
        // The question leg: the 24h count doubling over the prior 24h (or
        // a 3-day-old node still growing), once per day per key. The node
        // named is the filed one; an unfilled key questions nothing.
        let prior_n = prior.len() as u64;
        let doubling = prior_n > 0 && n24 >= 2 * prior_n;
        let aged_unclaimed = match &state.keys.get(key) {
            Some(ks) => {
                let age_ok = ks
                    .created_at
                    .is_some_and(|c| now - c >= 3 * NOTICE_OPEN_SECS);
                age_ok && n24 > prior_n
            }
            None => false,
        };
        let node_named = state.keys.get(key).and_then(|ks| ks.node.clone());
        if (doubling || aged_unclaimed) && n24 >= 3 {
            if let Some(node) = node_named {
                if now
                    - state
                        .keys
                        .get(key)
                        .map(|ks| ks.last_question_ts)
                        .unwrap_or(0)
                    >= NOTICE_OPEN_SECS
                {
                    actions.push(FoldAction::Question {
                        key: key.clone(),
                        node,
                    });
                }
            }
        }
    }
    actions
}

/// `~/.fno/spaces/` - every space journal the fold reads.
fn spaces_dir() -> Option<std::path::PathBuf> {
    notice_route_dir().ok()?.parent().map(|p| p.join("spaces"))
}

/// The trailing 7 days of foldable rows from every space journal
/// (advance_failed ran 2,705 times in a NON-default space, so the scan is
/// never one journal).
pub(crate) fn scan_rows(now: i64) -> Vec<FoldRow> {
    let Some(spaces) = spaces_dir() else {
        return Vec::new();
    };
    let week = 7 * NOTICE_OPEN_SECS;
    let mut out: Vec<FoldRow> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&spaces) else {
        return out;
    };
    let mut dirs: Vec<std::path::PathBuf> =
        entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    dirs.sort();
    for space in dirs {
        let journal = space.join("events.jsonl");
        let Ok(raw) = std::fs::read_to_string(&journal) else {
            continue;
        };
        for line in raw.lines() {
            let row = journal_row(&line, now, week);
            if let Some(row) = row {
                out.push(row);
            }
        }
    }
    out
}

/// One journal line as a fold row, when it is foldable and inside the
/// window. Out-of-window and unparseable lines contribute nothing.
fn journal_row(line: &str, now: i64, week: i64) -> Option<FoldRow> {
    let v = serde_json::from_str::<Value>(line).ok()?;
    let ts = v.get("ts").and_then(Value::as_str).and_then(parse_iso)?;
    if now < ts || now - ts >= week {
        return None;
    }
    let etype = v.get("type").and_then(Value::as_str)?;
    if !foldable(etype) {
        return None;
    }
    let error = v
        .get("data")
        .and_then(|d| d.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(FoldRow {
        ts,
        etype: etype.to_string(),
        error: error.to_string(),
        node: v
            .get("data")
            .and_then(|d| d.get("node"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn parse_iso(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.timestamp())
}

/// The banner leg (AC17): warning producers whose ONE content_hash reached
/// 3 distinct session_ids inside 24h fold as `banner` rows keyed
/// `banner_repeated:<producer id>`. The snapshot manifest is the source:
/// `data.source_manifest[]` carries source_id + content_hash per producer.
pub(crate) fn scan_banners(
    now: i64,
    warning_ids: &std::collections::HashSet<String>,
) -> Vec<FoldRow> {
    let mut out: Vec<FoldRow> = Vec::new();
    if warning_ids.is_empty() {
        return out;
    }
    let Some(spaces) = spaces_dir() else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(&spaces) else {
        return out;
    };
    let mut dirs: Vec<std::path::PathBuf> =
        entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    dirs.sort();
    // (producer id, content_hash) -> distinct sessions in 24h.
    let mut seen: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for space in dirs {
        let journal = space.join("events.jsonl");
        let Ok(raw) = std::fs::read_to_string(&journal) else {
            continue;
        };
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("context_snapshot") {
                continue;
            }
            let ts = v.get("ts").and_then(Value::as_str).and_then(parse_iso);
            let age = now - ts.unwrap_or(i64::MAX);
            if !(0..NOTICE_OPEN_SECS).contains(&age) {
                continue;
            }
            let sid = v
                .get("data")
                .and_then(|d| d.get("session_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let manifest = v
                .get("data")
                .and_then(|d| d.get("source_manifest"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for row in manifest {
                let id = row.get("source_id").and_then(Value::as_str).unwrap_or("");
                if !warning_ids.contains(id) {
                    continue;
                }
                let hash = row
                    .get("content_hash")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                seen.entry((id.to_string(), hash.to_string()))
                    .or_default()
                    .push(sid.clone());
            }
        }
    }
    for ((id, _hash), sids) in seen {
        let distinct: std::collections::HashSet<&String> = sids.iter().collect();
        // One row per distinct session: fold_pass counts rows, so the
        // group's real occurrence count is what crosses the 3-row bar.
        for _ in 0..distinct.len() {
            out.push(FoldRow {
                ts: now,
                etype: "banner".to_string(),
                error: id.clone(),
                node: None,
            });
        }
    }
    out
}

/// The warning producers: ids carrying `"warning": true` in the installed
/// plugin's context-hooks.json. A missing declaration reads as none.
pub(crate) fn warning_producer_ids() -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Some(root) = plugin_root() else {
        return out;
    };
    let Ok(raw) = std::fs::read_to_string(root.join("hooks").join("context-hooks.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return out;
    };
    let Some(groups) = v.get("groups").and_then(Value::as_object) else {
        return out;
    };
    for group in groups.values() {
        for producer in group
            .get("producers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if producer.get("warning").and_then(Value::as_bool) == Some(true) {
                if let Some(id) = producer.get("id").and_then(Value::as_str) {
                    out.insert(id.to_string());
                }
            }
        }
    }
    out
}

/// The installed plugin root: the env the harness sets, else the install
/// stamp. A missing root reads as no declaration.
fn plugin_root() -> Option<std::path::PathBuf> {
    if let Ok(root) = std::env::var("CLAUDE_PLUGIN_ROOT") {
        if !root.trim().is_empty() {
            return Some(std::path::PathBuf::from(root));
        }
    }
    let stamp = notice_route_dir()
        .ok()?
        .parent()?
        .join("install")
        .join("plugin-root");
    let raw = std::fs::read_to_string(stamp).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(trimmed))
    }
}

fn fold_state_path() -> Result<std::path::PathBuf, String> {
    Ok(notice_route_dir()?.join("fold.json"))
}

fn load_fold_state() -> FoldState {
    let Ok(path) = fold_state_path() else {
        return FoldState::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_fold_state(state: &FoldState) {
    let Ok(path) = fold_state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if serde_json::to_string(state)
        .map(|body| std::fs::write(&tmp, body))
        .is_ok()
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The tag a filed node carries, and the fold ledger's key: the first 8
/// hex of the sha256 over the failure key.
pub(crate) fn failure_tag(key: &str) -> String {
    let digest = sha2::Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("failure-key:{}", &hex[..8])
}

/// Append one encounter as the system component: the daemon holds no
/// session identity, so the voter key names the component instead. One
/// locked write, the creation vote's same shape and same-voter refusal.
fn record_system_encounter(node: &str, evidence: &str) -> bool {
    let graph = crate::backlog::settings::graph_path();
    let run = || -> Result<(), String> {
        let base_version = crate::graph_store::base_version(&graph).map_err(|e| e.to_string())?;
        let mut entries = crate::graph_store::read_rows(&graph).map_err(|e| e.to_string())?;
        let Some(row) = entries
            .iter_mut()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(node))
        else {
            return Err(format!("no node resolves to '{node}'"));
        };
        let key = "system:notice-router";
        let Some(obj) = row.as_object_mut() else {
            return Err(format!("node {node} is not an object"));
        };
        let existing = obj
            .get("encounters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if existing
            .iter()
            .any(|p| p.get("voter_key").and_then(Value::as_str) == Some(key))
        {
            return Err(format!(
                "voter {key} already recorded an encounter on {node}"
            ));
        }
        let mut record = serde_json::Map::new();
        record.insert(
            "created_at".into(),
            Value::String(crate::graph_store::now_isoformat()),
        );
        record.insert("evidence".into(), Value::String(evidence.to_string()));
        record.insert("voter_key".into(), Value::String(key.into()));
        record.insert("voter_kind".into(), Value::String("agent".into()));
        record.insert("harness".into(), Value::String("daemon".into()));
        let encounters = obj
            .entry("encounters".to_string())
            .or_insert_with(|| Value::Array(vec![]));
        if !encounters.is_array() {
            *encounters = Value::Array(vec![]);
        }
        encounters
            .as_array_mut()
            .expect("just made an array")
            .push(Value::Object(record));
        crate::graph_store::locked_mutate(
            &graph,
            crate::graph_store::MutateInput {
                entries,
                canonical_path: None,
                base_version,
                plan_rungs: None,
            },
            crate::graph_store::DEFAULT_LOCK_TIMEOUT,
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    };
    run().is_ok()
}

/// Run the fold's actions through the backlog doors. Each action mails its
/// owner one `fno/notice-router` mail naming the node. Best-effort: a
/// failed door costs the tick row a detail line, never the pass.
fn execute_fold(
    actions: Vec<FoldAction>,
    state: &mut FoldState,
    w: &crate::owner_ladder::World,
    now: i64,
    detail: &mut Vec<String>,
) -> u64 {
    let mut acted = 0u64;
    for action in actions {
        match action {
            FoldAction::FileNode { key, first_ts, last_ts, count, hint } => {
                let tag = failure_tag(&key);
                let title = format!("Repeated failure: {}", key.split(':').next().unwrap_or("event"));
                let details = format!(
                    "Folded by notice_route: {count} rows of {key} in 24h (first {first_ts}, last {last_ts}). Tag {tag}."
                );
                let mut cmd = crate::loop_dispatch::fno_cmd("fno");
                cmd.args([
                    "backlog", "idea", &title, "--type", "bug", "--difficulty", "low",
                    "--tag", &tag, "--source-kind", "from_observation",
                    "--origin-evidence", key.split(':').next().unwrap_or("event"),
                    // A daemon cannot answer the fold-or-separate choice
                    // prompt: --separate always mints its own node.
                    "--separate",
                    "-J", "--details", &details,
                ]);
                let new_node = crate::bounded_cmd::output_with_timeout_result(cmd, 30)
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| receipt_id(&String::from_utf8_lossy(&o.stdout)));
                let entry = state.keys.entry(key.clone()).or_default();
                entry.node = new_node.clone();
                entry.created_at = Some(now);
                entry.completed_at = None;
                if let Some(node) = new_node.or(hint) {
                    if mail_node_owner(w, Some(&node), &format!("Repeated failure filed: {count} rows of {key} in 24h. Node carries tag {tag}."), now) {
                        acted += 1;
                    } else {
                        detail.push(format!("fold: undelivered mail for {tag}"));
                    }
                }
                acted += 1;
            }
            FoldAction::Encounter { key, node, count } => {
                let evidence = format!("notice_route fold: {count} more rows of the key in 24h");
                // The daemon holds no session identity, so the encounter
                // verb's identity gate would refuse (or misattribute the
                // vote to a launcher session): append as the system
                // component through the locked write instead.
                if record_system_encounter(&node, &evidence) {
                    state.keys.get_mut(&key).map(|e| e.last_encounter_ts = now);
                    if mail_node_owner(w, Some(&node), &format!("The repeated failure keeps recurring: {count} rows in 24h (node {node})."), now) {
                        acted += 1;
                    }
                } else {
                    detail.push(format!("fold: encounter failed for {node}"));
                }
            }
            FoldAction::Reopen { key, node, count } => {
                let mut cmd = crate::loop_dispatch::fno_cmd("fno");
                cmd.args(["backlog", "update", &node, "--status", "triage"]);
                let ok = crate::bounded_cmd::output_with_timeout_result(cmd, 30)
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if ok {
                    let entry = state.keys.get_mut(&key);
                    if let Some(e) = entry {
                        e.completed_at = None;
                    }
                    if mail_node_owner(w, Some(&node), &format!("A done node recurred: {count} rows of its failure key in 24h; reopened to triage ({node})."), now) {
                        acted += 1;
                    }
                } else {
                    detail.push(format!("fold: reopen failed for {node}"));
                }
            }
            FoldAction::Question { key, node } => {
                if mail_node_owner(w, Some(&node), &format!("The failure rate doubled or the node is aging unclaimed: key {key}, node {node}. What should change?"), now) {
                    state.keys.get_mut(&key).map(|e| e.last_question_ts = now);
                    acted += 1;
                }
            }
        }
    }
    acted
}

/// The node id in a `fno backlog idea -J` receipt: the first `"id"` string.
fn receipt_id(stdout: &str) -> Option<String> {
    let v = serde_json::from_str::<Value>(stdout).ok()?;
    find_first_id(&v)
}

fn find_first_id(v: &Value) -> Option<String> {
    if let Some(id) = v.get("id").and_then(Value::as_str) {
        if !id.trim().is_empty() {
            return Some(id.to_string());
        }
    }
    for child in v.as_object().map(|m| m.values()).into_iter().flatten() {
        if let Some(found) = find_first_id(child) {
            return Some(found);
        }
    }
    None
}

/// One `fno/notice-router` mail to a node's owner through the ladder.
fn mail_node_owner(
    w: &crate::owner_ladder::World,
    node: Option<&str>,
    text: &str,
    now: i64,
) -> bool {
    let owner = crate::owner_ladder::resolve(
        &crate::owner_ladder::Ask {
            node,
            from_session: None,
            start: crate::owner_ladder::Rung::NodeLead,
        },
        w,
    );
    let Some(sid) = owner.session.as_deref().filter(|s| !s.trim().is_empty()) else {
        return false;
    };
    let mut runner: crate::burn_watch::Runner = &mut crate::burn_watch::run_command;
    let (sent, _lane) = crate::burn_watch::wake_with_text(
        sid,
        false,
        text,
        crate::system_sender::system_name("notice-router").as_str(),
        &mut runner,
    );
    if sent {
        let fingerprint = {
            let digest =
                sha2::Sha256::digest(format!("fold:{}:{text}", node.unwrap_or("none")).as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            hex
        };
        record_sent(&fingerprint, owner.scope.as_deref().unwrap_or(""), now);
    }
    sent
}

/// Pull each key's filed node's live status back into the state before the
/// pass decides: a node closed (or reopened) by another actor must flip
/// the key to the Reopen (or Encounter) branch the graph implies, never
/// the stale one the fold ledger cached. Best-effort: an unreadable store
/// leaves the cached state standing.
fn refresh_completion(state: &mut FoldState, now: i64) {
    let graph = crate::backlog::settings::graph_path();
    let Ok(rows) = crate::graph_store::read_rows(&graph) else {
        return;
    };
    for ks in state.keys.values_mut() {
        let Some(node) = &ks.node else { continue };
        let Some(row) = rows
            .iter()
            .find(|r| crate::graph_store::entry_id(r) == Some(node.as_str()))
        else {
            continue;
        };
        if row.get("status").and_then(Value::as_str) == Some("done") {
            if ks.completed_at.is_none() {
                ks.completed_at = row
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .and_then(parse_iso)
                    .or(Some(now));
            }
        } else {
            ks.completed_at = None;
        }
    }
}

/// The hourly fold: scan, decide, execute, persist. Returns (acted, skip).
pub(crate) fn fold_tick(w: &crate::owner_ladder::World) -> (u64, Option<String>) {
    let now = now_epoch();
    let mut state = load_fold_state();
    refresh_completion(&mut state, now);
    let mut rows = scan_rows(now);
    let warnings = warning_producer_ids();
    rows.extend(scan_banners(now, &warnings));
    let actions = fold_pass(&rows, &mut state, now);
    if actions.is_empty() {
        return (0, Some("no_fold_actions".to_string()));
    }
    let mut detail: Vec<String> = Vec::new();
    let acted = execute_fold(actions, &mut state, w, now, &mut detail);
    save_fold_state(&state);
    if detail.is_empty() {
        (acted, None)
    } else {
        (acted, Some(detail.join("; ").chars().take(160).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC10-AC17 over the pure pieces: the result-file readers, the
    /// fingerprint dedupe against the sent store, and the fold's
    /// decisions. The mail/page/door legs are shelled doors verified by
    /// composition; the hook leg is bash, checked by `bash -n` in verify.
    #[test]
    fn ac10_to_ac17_readers_dedupe_and_fold_decisions() {
        // AC10 shape: each bucket lifts into a Warning with the node and
        // the cause text the hook printed.
        let raw = serde_json::json!({
            "promise_unmet": [{"node_id": "n-1"}],
            "promise_unknown": [{"node_id": "n-2"}],
            "sync_catchup": {"outcome": "failed", "detail": "locked", "stale": false}
        })
        .to_string();
        let warnings = read_reconcile(&raw);
        assert_eq!(warnings.len(), 3);
        assert_eq!(warnings[0].kind, "promise_unmet");
        assert_eq!(warnings[0].node.as_deref(), Some("n-1"));
        assert!(warnings[0].cause.contains("promise gate"));
        assert_eq!(warnings[1].kind, "promise_unknown");
        assert!(warnings[1].cause.contains("Do not force"));
        assert_eq!(warnings[2].kind, "sync_catchup");
        assert!(warnings[2].cause.contains("canonical-sync"));

        let (orphan_warnings, dir) = read_orphan(
            serde_json::json!({
                "plans_dir": "/p",
                "rows": [
                    {"node_id": "n-3", "verdict": "bound_now"},
                    {"node_id": "n-4", "verdict": "bind_failed"}
                ]
            })
            .to_string()
            .as_str(),
        );
        assert_eq!(orphan_warnings.len(), 2);
        assert_eq!(orphan_warnings[0].kind, "orphan_bound");
        assert_eq!(orphan_warnings[1].kind, "orphan_held");
        assert_eq!(dir.as_deref(), Some("/p"));

        // A terminal-rung plan reads history: its verdict must ride the
        // same silent arm as bound and terminal, or a shipped plan mails
        // its lead asking for a rebind.
        let (history_warnings, _) = read_orphan(
            serde_json::json!({
                "plans_dir": "/p",
                "rows": [
                    {"node_id": "n-9", "verdict": "history"},
                    {"node_id": "n-8", "verdict": "unfinalized"}
                ]
            })
            .to_string()
            .as_str(),
        );
        assert_eq!(history_warnings.len(), 1);
        assert_eq!(history_warnings[0].node.as_deref(), Some("n-8"));

        // AC11: the same fingerprint is not resent inside the open window.
        let _root = crate::paths::DeclaredRoot::declare("notice_sent_dedupe");
        let fp = fingerprint_of(&warnings);
        assert!(!sent_recently(&fp));
        record_sent(&fp, "scope-a", now_epoch());
        assert!(sent_recently(&fp));
        let other = fingerprint_of(&orphan_warnings);
        assert!(!sent_recently(&other));

        // The key: digits and hex runs collapse to one token, so a count
        // or an id inside the error never splits one failure in two.
        assert_eq!(normalize_error("port 5432 refused"), "port # refused");
        assert_eq!(
            fold_key("advance_failed", "substrate pane 2727709ed7"),
            "advance_failed:substrate pane #"
        );
        // AC17 shape: a banner key counts as a failure and names the id.
        assert!(foldable("banner"));
        assert!(foldable("advance_failed"));
        assert!(!foldable("context_snapshot"));
        assert_eq!(
            fold_key("banner", "reconcile-session-start"),
            "banner_repeated:reconcile-session-start"
        );
        assert!(steady_noise("transition_rejected"));
        assert!(!steady_noise("advance_failed"));

        // AC13: three rows of one key in 24h and no filed node -> one
        // FileNode, and no action for the two-row key below the bar.
        let now = 1_700_000_000_i64;
        let day = NOTICE_OPEN_SECS;
        let row = |age: i64, etype: &str, error: &str| FoldRow {
            ts: now - age,
            etype: etype.to_string(),
            error: error.to_string(),
            node: None,
        };
        let mut state = FoldState::default();
        let rows = vec![
            row(day - 100, "advance_failed", "substrate pane 1"),
            row(day - 200, "advance_failed", "substrate pane 2"),
            row(day - 300, "advance_failed", "substrate pane 3"),
            row(day - 100, "session_finalize_failed", "boom 9"),
            row(day - 150, "session_finalize_failed", "boom 9"),
        ];
        let actions = fold_pass(&rows, &mut state, now);
        let k = fold_key("advance_failed", "substrate pane 1");
        assert_eq!(actions.len(), 1, "{actions:?}");
        match &actions[0] {
            FoldAction::FileNode { key, count, .. } => {
                assert_eq!(key, &k);
                assert_eq!(*count, 3);
            }
            other => panic!("expected FileNode, got {other:?}"),
        }

        // AC14: the key now has an open node; two more rows add ONE
        // Encounter and never a second node. The pass is pure and never
        // touches state, so the test plays the shell layer: persist the
        // KeyState the executed FileNode would have written back.
        state.keys.insert(
            k.clone(),
            KeyState {
                node: Some("fno-abcd".to_string()),
                created_at: Some(now - day),
                ..Default::default()
            },
        );
        let rows = vec![
            row(100, "advance_failed", "substrate pane 4"),
            row(200, "advance_failed", "substrate pane 5"),
        ];
        let actions = fold_pass(&rows, &mut state, now);
        assert_eq!(actions.len(), 1, "{actions:?}");
        match &actions[0] {
            FoldAction::Encounter { node, count, .. } => {
                assert_eq!(node, "fno-abcd");
                assert_eq!(*count, 2);
            }
            other => panic!("expected Encounter, got {other:?}"),
        }

        // AC15: a done node whose key recurs after completed_at reopens.
        state.keys.get_mut(&k).unwrap().completed_at = Some(now - 2 * day);
        let rows = vec![row(100, "advance_failed", "substrate pane 6")];
        let actions = fold_pass(&rows, &mut state, now);
        match &actions[0] {
            FoldAction::Reopen { node, .. } => assert_eq!(node, "fno-abcd"),
            other => panic!("expected Reopen, got {other:?}"),
        }

        // AC16: steady noise stays silent at its average and files past
        // three times it. week = 70 rows, so the bar is 3 * (70 / 7) = 30.
        // The spike must out-run a bar that grows with the week: 36 fresh
        // rows put n24 at 46 against a week of 106, bar 3 * 15 = 45.
        let mut steady: Vec<FoldRow> = Vec::new();
        for i in 0..10 {
            steady.push(row(3_600 + i as i64, "transition_rejected", "stale"));
        }
        for i in 0..60 {
            steady.push(row(day + 3_600 + i * 6_000, "transition_rejected", "stale"));
        }
        let mut fresh = FoldState::default();
        let actions = fold_pass(&steady, &mut fresh, now);
        assert!(
            actions.is_empty(),
            "steady noise at its average: {actions:?}"
        );
        let mut spike: Vec<FoldRow> = steady;
        for i in 0..36 {
            spike.push(row(100 + i as i64, "transition_rejected", "stale"));
        }
        let actions = fold_pass(&spike, &mut fresh, now);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FoldAction::FileNode { .. })),
            "a spike past three times the 7-day average files: {actions:?}"
        );
    }
}
