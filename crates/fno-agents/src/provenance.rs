//! Turn provenance: which of a session's user-shaped transcript rows did a
//! person actually type?
//!
//! The classifier was promoted from [`crate::operator_turns`] (the inbox
//! queue's private skip rules) so a second consumer can share it:
//! `fno-agents intel` folds every transcript into per-session provenance
//! counters. The queue-facing [`classify`] keeps its exact signature and
//! reason names; [`classify_turn`] extends it with the bus join - a user turn
//! whose text equals a bus row body addressed to this session is relay, not
//! operator, which is the only rule that catches daemon nudges and raw slash
//! payloads injected without an `<fno_mail>` envelope.
//!
//! The harness contract also lives here: a [`TranscriptSource`] is a store
//! path, a session listing, and a turn reader, nothing more, so the fold
//! never branches on harness name after the source yields a turn.

use crate::mail_inject::contains_fno_mail_tag_anywhere;
use crate::provider::parse_verb_token;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// `(prefix, reason)` matched against the reminder-stripped turn text, in
/// order. Every prefix is a harness-injected envelope a person cannot type.
pub(crate) const SKIP_RULES: &[(&str, &str)] = &[
    ("<command-name>", "command_invocation"),
    ("<command-message>", "command_invocation"),
    ("<local-command", "command_invocation"),
    ("<user_instructions>", "synthetic"),
    ("<environment_context>", "synthetic"),
    ("<task-notification>", "task_notification"),
    ("<bash-input>", "bash_echo"),
    ("<bash-stdout>", "bash_echo"),
    ("[Request interrupted by user", "interrupt_marker"),
    (
        "This session is being continued from a previous conversation",
        "compaction_preamble",
    ),
    ("Another Claude session sent a message:", "teammate_message"),
    // The two provenance rules the queue never met: a cross-session message
    // is a teammate shape by another transport, and a keepalive ping is the
    // session's own cache warmer, not a person.
    ("<cross-session-message", "cross_session"),
    ("[cache-keepalive]", "keepalive"),
];

pub(crate) fn system_reminder_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").expect("valid pattern")
    })
}

/// True for a row the transcript writes when a user (or mail) speaks: a claude
/// `type:"user"` row without a truthy `isMeta`, or a codex payload message.
pub(crate) fn is_user_turn(obj: &Value) -> bool {
    if obj.get("type").and_then(|v| v.as_str()) == Some("user") {
        return !json_truthy(obj.get("isMeta"));
    }
    match obj.get("payload") {
        Some(p) if p.is_object() => {
            p.get("type").and_then(|v| v.as_str()) == Some("message")
                && p.get("role").and_then(|v| v.as_str()) == Some("user")
        }
        _ => false,
    }
}

/// A claude `type:"user"` row carrying a truthy `isMeta`: the harness renders
/// it user-shaped, but no person typed it.
pub(crate) fn is_meta_row(obj: &Value) -> bool {
    obj.get("type").and_then(|v| v.as_str()) == Some("user") && json_truthy(obj.get("isMeta"))
}

pub(crate) fn json_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_none_or(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// The user-visible text of a row, `""` when it has none. Both content shapes
/// (string, block list) across the claude and codex row formats; a codex
/// payload wins when present.
pub(crate) fn turn_text(obj: &Value) -> String {
    let mut content = match obj.get("message") {
        Some(m) if m.is_object() => m.get("content"),
        _ => obj.get("content"),
    };
    if let Some(p) = obj.get("payload") {
        if p.is_object() {
            content = p.get("content");
        }
    }
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// RFC 3339 (Z or offset), else a naive stamp read as UTC: transcripts are
/// UTC by convention and a naive stamp must not skew by the local offset.
pub(crate) fn turn_ts_epoch(obj: &Value) -> Option<f64> {
    let raw = match obj.get("timestamp") {
        Some(Value::String(s)) if !s.trim().is_empty() => s,
        _ => match obj.get("ts") {
            Some(Value::String(s)) if !s.trim().is_empty() => s,
            _ => return None,
        },
    };
    let t = raw.trim();
    let secs = |ts: i64, micros: u32| ts as f64 + micros as f64 / 1_000_000.0;
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(secs(dt.timestamp(), dt.timestamp_subsec_micros()));
    }
    if let Ok(nd) = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(secs(
            nd.and_utc().timestamp(),
            nd.and_utc().timestamp_subsec_micros(),
        ));
    }
    // A date-only stamp reads as midnight UTC, as Python's fromisoformat did.
    let date_only = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))?;
    Some(secs(date_only.and_utc().timestamp(), 0))
}

/// A single-line slash command or `$fno:` verb with flag-shaped args only.
/// A token ending in sentence punctuation means the turn carries prose, and
/// prose may carry a ruling. A filename dot is fine (over-counting is the
/// safe direction); `x-1.` is not.
pub(crate) fn is_bare_command(text: &str) -> bool {
    if text.contains('\n') {
        return false;
    }
    let mut parts = text.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    if parse_verb_token(first).is_none() {
        return false;
    }
    parts.all(is_arg_token)
}

fn is_arg_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    let shaped = tok.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '.' | '_' | '/' | ':' | '@' | '%' | '+' | '=' | '~' | '-')
    });
    shaped
        && !matches!(
            tok.chars().last(),
            Some('.') | Some('?') | Some('!') | Some(';') | Some(',')
        )
}

/// What one turn text is: the cleaned operator text, an injected shape with
/// its reason name, or a bus row delivered into this session.
#[derive(Debug, PartialEq)]
pub(crate) enum Verdict {
    Operator(String),
    Injected(&'static str),
    BusRow,
}

/// The text-level classifier both consumers share: mail envelopes first, then
/// the skip rules, then the bus join (before the bare-command check, because
/// the residual the join exists for IS raw slash payloads), then bare
/// commands. `bus` is `None` for the queue, which keeps today's behavior.
pub(crate) fn classify_text(text: &str, bus: Option<(&BusIndex, &str)>) -> Verdict {
    if contains_fno_mail_tag_anywhere(text) {
        return Verdict::Injected("fno_mail");
    }
    let cleaned = system_reminder_re()
        .replace_all(text.trim(), "")
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return Verdict::Injected("no_user_text");
    }
    for (prefix, reason) in SKIP_RULES {
        if cleaned.starts_with(prefix) {
            return Verdict::Injected(reason);
        }
    }
    if let Some((index, session)) = bus {
        if index.delivered_to(session, &cleaned) {
            return Verdict::BusRow;
        }
    }
    if is_bare_command(&cleaned) {
        return Verdict::Injected("bare_command");
    }
    Verdict::Operator(cleaned)
}

/// The queue-shaped verdict: the operator-shaped text of a turn, or its named
/// skip reason when it is not one. The skip reason names ARE the queue's
/// counters and must not drift.
pub(crate) fn classify(text: &str) -> Result<String, &'static str> {
    match classify_text(text, None) {
        Verdict::Operator(cleaned) => Ok(cleaned),
        Verdict::Injected(reason) => Err(reason),
        Verdict::BusRow => Err("bus_row"),
    }
}

/// Relay kinds: an agent or process speaking in a user-shaped row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum RelayKind {
    FnoMail,
    TeammateMessage,
    CrossSession,
    TaskNotification,
    BusRow,
}

impl RelayKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            RelayKind::FnoMail => "relay_fno_mail",
            RelayKind::TeammateMessage => "relay_teammate_message",
            RelayKind::CrossSession => "relay_cross_session",
            RelayKind::TaskNotification => "relay_task_notification",
            RelayKind::BusRow => "relay_bus_row",
        }
    }
}

/// Harness kinds: the transcript's own machinery rendered user-shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum HarnessKind {
    CommandInvocation,
    Synthetic,
    CompactionPreamble,
    BashEcho,
    InterruptMarker,
    StopHook,
    LoopWakeup,
    SkillBody,
}

impl HarnessKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            HarnessKind::CommandInvocation => "harness_command_invocation",
            HarnessKind::Synthetic => "harness_synthetic",
            HarnessKind::CompactionPreamble => "harness_compaction_preamble",
            HarnessKind::BashEcho => "harness_bash_echo",
            HarnessKind::InterruptMarker => "harness_interrupt_marker",
            HarnessKind::StopHook => "harness_stop_hook",
            HarnessKind::LoopWakeup => "harness_loop_wakeup",
            HarnessKind::SkillBody => "harness_skill_body",
        }
    }
}

/// The provenance of one user-shaped transcript row. Operator is the residual
/// after every injected shape is named; claude records no positive typed-turn
/// marker (the mux `operator_submit` event is the close).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum Provenance {
    Operator,
    Relay(RelayKind),
    Harness(HarnessKind),
    Keepalive,
}

impl Provenance {
    /// The counter name the fold reports; stable across runs and harnesses.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Provenance::Operator => "operator",
            Provenance::Relay(k) => k.label(),
            Provenance::Harness(k) => k.label(),
            Provenance::Keepalive => "keepalive",
        }
    }

    /// Every counter the fold emits, including the ones that may stay zero.
    pub(crate) fn all_labels() -> Vec<&'static str> {
        let mut labels = vec![Provenance::Operator.label(), Provenance::Keepalive.label()];
        labels.extend(
            [
                RelayKind::FnoMail,
                RelayKind::TeammateMessage,
                RelayKind::CrossSession,
                RelayKind::TaskNotification,
                RelayKind::BusRow,
            ]
            .into_iter()
            .map(RelayKind::label),
        );
        labels.extend(
            [
                HarnessKind::CommandInvocation,
                HarnessKind::Synthetic,
                HarnessKind::CompactionPreamble,
                HarnessKind::BashEcho,
                HarnessKind::InterruptMarker,
                HarnessKind::StopHook,
                HarnessKind::LoopWakeup,
                HarnessKind::SkillBody,
            ]
            .into_iter()
            .map(HarnessKind::label),
        );
        labels
    }
}

/// The provenance of one user-shaped transcript row, bus join included.
/// Callers pass only rows where [`is_user_turn`] or [`is_meta_row`] holds;
/// other row kinds are not turns.
pub(crate) fn classify_turn(obj: &Value, bus: &BusIndex, session: &str) -> Provenance {
    if !is_user_turn(obj) {
        return meta_kind(&turn_text(obj));
    }
    match classify_text(&turn_text(obj), Some((bus, session))) {
        Verdict::Operator(_) => Provenance::Operator,
        Verdict::Injected(reason) => kind_from_reason(reason),
        Verdict::BusRow => Provenance::Relay(RelayKind::BusRow),
    }
}

fn kind_from_reason(reason: &str) -> Provenance {
    match reason {
        "fno_mail" => Provenance::Relay(RelayKind::FnoMail),
        "teammate_message" => Provenance::Relay(RelayKind::TeammateMessage),
        "cross_session" => Provenance::Relay(RelayKind::CrossSession),
        "task_notification" => Provenance::Relay(RelayKind::TaskNotification),
        "keepalive" => Provenance::Keepalive,
        "bare_command" | "command_invocation" => {
            Provenance::Harness(HarnessKind::CommandInvocation)
        }
        "compaction_preamble" => Provenance::Harness(HarnessKind::CompactionPreamble),
        "bash_echo" => Provenance::Harness(HarnessKind::BashEcho),
        "interrupt_marker" => Provenance::Harness(HarnessKind::InterruptMarker),
        "stop_hook" => Provenance::Harness(HarnessKind::StopHook),
        // synthetic, no_user_text, and any future reason: machine-shaped.
        _ => Provenance::Harness(HarnessKind::Synthetic),
    }
}

/// The kind of an `isMeta` row, read from its text prefix so the fold has a
/// counter for shapes `is_user_turn` drops today. They never count as
/// operator either way. Measured 2026-09-16 over 60 transcripts: stop-hook
/// feedback starts `Stop hook feedback:`, the loop self-wakeups say `reign
/// check-in` or `Check in on your territory`, skill bodies start `Base
/// directory for this skill`.
pub(crate) fn meta_kind(text: &str) -> Provenance {
    let t = text.trim_start();
    if t.starts_with("Stop hook feedback:") {
        Provenance::Harness(HarnessKind::StopHook)
    } else if t.starts_with("reign check-in") || t.starts_with("Check in on your territory") {
        Provenance::Harness(HarnessKind::LoopWakeup)
    } else if t.starts_with("Base directory for this skill") {
        Provenance::Harness(HarnessKind::SkillBody)
    } else {
        Provenance::Harness(HarnessKind::Synthetic)
    }
}

/// One bus row's join fields: the body a session receives, who sent it, and
/// the session it is addressed to (`meta.to_session`; the envelope `to` is a
/// mail handle, not a session id).
#[derive(Debug, Clone)]
pub(crate) struct BusRow {
    pub(crate) id: String,
    pub(crate) ts: String,
    pub(crate) from_session: Option<String>,
    pub(crate) to_session: Option<String>,
    pub(crate) body: String,
    pub(crate) words: usize,
}

/// The bus join index, built once per fold from `~/.fno/bus/messages.jsonl`
/// (or the `FNO_BUS_DIR` override, the one path bus readers share). An
/// unreadable or absent bus is an empty index: the join then matches nothing
/// and every row falls through to the text rules, which is today's behavior.
pub(crate) struct BusIndex {
    rows: Vec<BusRow>,
}

impl BusIndex {
    /// A join-free index: fixtures and the queue's no-bus path.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        BusIndex { rows: Vec::new() }
    }

    /// The live log plus every retained rotation segment
    /// (`messages.jsonl.1`, `.2`, ...): a relay row still inside the fold's
    /// window must not vanish because the bus rotated it out. Segments are
    /// read until one is missing, ten names max.
    pub(crate) fn load(path: &Path) -> Self {
        let mut raw = String::new();
        let mut missing = false;
        for suffix in ["", ".1", ".2", ".3", ".4", ".5", ".6", ".7", ".8", ".9"] {
            if missing {
                break;
            }
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("messages.jsonl");
            let segment = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("{file_name}{suffix}"));
            match std::fs::read_to_string(&segment) {
                Ok(text) => raw.push_str(&text),
                Err(_) => missing = true,
            }
        }
        let rows = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|row| {
                let body = row.get("body")?.as_str()?.to_string();
                if body.trim().is_empty() {
                    return None;
                }
                let to_session = row
                    .get("meta")
                    .and_then(|m| m.get("to_session"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let words = row
                    .get("word_count")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or_else(|| body.split_whitespace().count());
                Some(BusRow {
                    id: row
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    ts: row
                        .get("ts")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    from_session: row
                        .get("from_session")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    to_session,
                    body,
                    words,
                })
            })
            .collect();
        BusIndex { rows }
    }

    pub(crate) fn rows(&self) -> &[BusRow] {
        &self.rows
    }

    /// True when a row addressed to `session` has exactly this text as its
    /// body: the raw payload a transport typed into the pane on every
    /// harness, envelope or not.
    fn delivered_to(&self, session: &str, text: &str) -> bool {
        self.rows
            .iter()
            .any(|r| r.to_session.as_deref() == Some(session) && r.body.trim() == text.trim())
    }
}

/// One transcript file with its identity: the input a session listing hands
/// the fold. `session_id` is the id bus rows address (the claude filename
/// stem, the codex rollout uuid).
#[derive(Debug, Clone)]
pub(crate) struct SessionFile {
    pub(crate) path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) mtime: u64,
    pub(crate) size: u64,
}

/// One user-shaped turn: the raw row (the classifier reads the envelope
/// shapes off it), its text, and its timestamp when the row carries one.
pub(crate) struct Turn {
    pub(crate) obj: Value,
    pub(crate) text: String,
    pub(crate) ts_epoch: Option<f64>,
}

/// The harness contract, explicit because the classifier must not know whose
/// file it reads: a harness is a store path, a session listing, and a turn
/// reader. Two impls ship (claude, codex); opencode is one more impl with no
/// new classification rule, and until it lands its sessions report under
/// `skipped.opencode` rather than as a guess.
pub(crate) trait TranscriptSource {
    fn harness(&self) -> &'static str;
    /// Sessions whose transcript mtime falls inside the last `days` days;
    /// `days == 0` means no window.
    fn sessions(&self, days: u64) -> Vec<SessionFile>;
    /// The user-shaped turns (typed, meta, or relayed) of one transcript's
    /// raw text. Takes the raw bytes, not a path: the fold reads each file
    /// once and every parser works from that single read.
    fn turns(&self, raw: &str) -> Vec<Turn>;
    /// Tool calls the assistant made, in the harness's own row shape, from
    /// the same raw text.
    fn tool_uses(&self, raw: &str) -> usize;
}

fn within_window(mtime: u64, days: u64, now: u64) -> bool {
    days == 0 || now.saturating_sub(mtime) <= days * 86_400
}

/// The claude transcript store: `<projects>/<cwd-slug>/*.jsonl`, or every
/// slug with `all_projects`. The projects root is resolved once by the
/// caller (via [`crate::claude_drive::claude_projects_dir`]) and injected,
/// so the fold reads the store `resume`/`adopt` already resolve and tests
/// never mutate process env.
pub(crate) struct ClaudeSource {
    pub(crate) cwd: PathBuf,
    pub(crate) all_projects: bool,
    pub(crate) projects_dir: PathBuf,
}

impl ClaudeSource {
    fn session_dirs(&self) -> Vec<PathBuf> {
        let base = self.projects_dir.clone();
        if self.all_projects {
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            dirs
        } else {
            vec![base.join(crate::claude_ask::claude_cwd_slug(&self.cwd))]
        }
    }
}

impl TranscriptSource for ClaudeSource {
    fn harness(&self) -> &'static str {
        "claude"
    }

    fn sessions(&self, days: u64) -> Vec<SessionFile> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut out = Vec::new();
        for dir in self.session_dirs() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if !within_window(mtime, days, now) {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                out.push(SessionFile {
                    session_id: name.to_string(),
                    path: path.clone(),
                    mtime,
                    size: meta.len(),
                });
            }
        }
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        out
    }

    fn turns(&self, raw: &str) -> Vec<Turn> {
        raw.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|obj| is_user_turn(obj) || is_meta_row(obj))
            .map(|obj| {
                let text = turn_text(&obj);
                let ts_epoch = turn_ts_epoch(&obj);
                Turn {
                    obj,
                    text,
                    ts_epoch,
                }
            })
            .collect()
    }

    fn tool_uses(&self, raw: &str) -> usize {
        raw.lines()
            .filter(|line| line.contains("\"tool_use\""))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|row| {
                row.get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                            .count()
                    })
                    .unwrap_or(0)
            })
            .sum()
    }
}

/// The codex transcript store: `$CODEX_HOME/sessions` rollout files through
/// [`crate::codex_store`], the listing liveness already walks. `sessions_dir`
/// is the injected root (None resolves the env home); tests pin a fixture
/// directory instead of mutating process env. `cwd` scopes the listing to
/// the requested project when set (matching claude's default view); a
/// rollout whose session metadata carries no cwd still reports.
pub(crate) struct CodexSource {
    pub(crate) sessions_dir: Option<PathBuf>,
    pub(crate) cwd: Option<PathBuf>,
}

/// The rollout uuid: the 36-char id after the last `-` in
/// `rollout-<ts>-<uuid>.jsonl`. A name that does not end in one falls back to
/// the whole stem, which still sorts and reports, just unjoinable to mail.
pub(crate) fn rollout_session_id(stem: &str) -> String {
    if stem.len() > 36 {
        let tail = &stem[stem.len() - 36..];
        if tail.bytes().filter(|b| *b == b'-').count() == 4 {
            return tail.to_string();
        }
    }
    stem.to_string()
}

/// One rollout's session metadata, from its first line: the authoritative
/// session id and the cwd the session ran in. Codex treats the metadata row
/// as the identity, so a filename uuid that disagrees with it loses.
fn rollout_meta(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(file) = std::fs::File::open(path) else {
        return (None, None);
    };
    use std::io::BufRead;
    let first = std::io::BufReader::new(file)
        .lines()
        .next()
        .and_then(|l| l.ok());
    let Some(first) = first else {
        return (None, None);
    };
    let Ok(row) = serde_json::from_str::<Value>(&first) else {
        return (None, None);
    };
    let meta = row.get("payload");
    let id = meta
        .and_then(|p| p.get("session_id").or_else(|| p.get("id")))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let cwd = meta
        .and_then(|p| p.get("cwd"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    (id, cwd)
}

impl TranscriptSource for CodexSource {
    fn harness(&self) -> &'static str {
        "codex"
    }

    fn sessions(&self, days: u64) -> Vec<SessionFile> {
        let want_slug = self.cwd.as_deref().map(crate::claude_ask::claude_cwd_slug);
        crate::codex_store::codex_sessions(self.sessions_dir.as_deref(), days)
            .into_iter()
            .filter_map(|s| {
                let (meta_id, meta_cwd) = rollout_meta(&s.path);
                if let (Some(want), Some(actual)) = (&want_slug, &meta_cwd) {
                    if crate::claude_ask::claude_cwd_slug(Path::new(actual)) != *want {
                        return None;
                    }
                }
                Some(SessionFile {
                    session_id: meta_id.unwrap_or(s.session_id),
                    path: s.path,
                    mtime: s.mtime_secs,
                    size: s.size,
                })
            })
            .collect()
    }

    fn turns(&self, raw: &str) -> Vec<Turn> {
        raw.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(is_user_turn)
            .map(|obj| {
                let text = turn_text(&obj);
                let ts_epoch = turn_ts_epoch(&obj);
                Turn {
                    obj,
                    text,
                    ts_epoch,
                }
            })
            .collect()
    }

    fn tool_uses(&self, raw: &str) -> usize {
        raw.lines()
            .filter(|line| line.contains("\"function_call\""))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|row| {
                row.get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("function_call")
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A claude user row: the shape every fixture below rides.
    fn user_row(text: &str) -> Value {
        json!({"type": "user", "uuid": "u1", "timestamp": "2026-09-16T12:00:00.000Z",
               "message": {"role": "user", "content": text}})
    }

    fn bus_index(rows: Value) -> BusIndex {
        // Unique per call: cargo test runs this module's tests on parallel
        // threads of one process, and a shared path lets one fixture read
        // another's rows.
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fno-provenance-test-{}-{n}-bus.jsonl",
            std::process::id()
        ));
        let body = match rows {
            Value::Array(rows) => rows
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        };
        std::fs::write(&path, body + "\n").unwrap();
        let index = BusIndex::load(&path);
        let _ = std::fs::remove_file(&path);
        index
    }

    #[test]
    fn skip_rules_keep_the_queue_names() {
        assert_eq!(classify("plain prose"), Ok("plain prose".to_string()));
        assert_eq!(
            classify("<task-notification><t/></task-notification>"),
            Err("task_notification")
        );
        assert_eq!(
            classify("<fno_mail to=\"x\">hi</fno_mail>"),
            Err("fno_mail")
        );
        assert_eq!(classify("/fno:setup"), Err("bare_command"));
        assert_eq!(classify("   "), Err("no_user_text"));
    }

    #[test]
    fn the_two_new_rules_name_keepalive_and_cross_session() {
        assert_eq!(classify("[cache-keepalive] Ping 1/4"), Err("keepalive"));
        assert_eq!(
            classify("<cross-session-message from=\"other\">hello</cross-session-message>"),
            Err("cross_session")
        );
    }

    #[test]
    fn a_turn_matching_a_bus_row_addressed_here_is_relay() {
        let bus = bus_index(json!([{
            "v": 1, "id": "msg-1", "ts": "2026-09-16T11:00:00Z",
            "from_session": "aaaa-bbbb", "meta": {"to_session": "cccc-dddd"},
            "word_count": 3, "body": "status on x-1 now"
        }]));
        let row = user_row("status on x-1 now");
        assert_eq!(
            classify_turn(&row, &bus, "cccc-dddd"),
            Provenance::Relay(RelayKind::BusRow)
        );
        // Same body, different recipient: the operator typed it themselves.
        assert_eq!(classify_turn(&row, &bus, "eeee-ffff"), Provenance::Operator);
        // No bus at all: today's residual, counted as operator.
        assert_eq!(
            classify_turn(&row, &BusIndex { rows: Vec::new() }, "cccc-dddd"),
            Provenance::Operator
        );
    }

    #[test]
    fn the_fixture_counts_operator_relay_and_keepalive() {
        let bus = bus_index(json!([{
            "v": 1, "id": "msg-2", "ts": "2026-09-16T11:00:00Z",
            "meta": {"to_session": "s"}, "word_count": 2, "body": "drive it to merge"
        }]));
        let rows = [
            user_row("please widen the gate"),
            user_row("and keep the worktree clean"),
            user_row("ship it when green"),
            user_row("<fno_mail from=\"p\" to=\"s\">run the sweep</fno_mail>"),
            user_row(
                "Another Claude session sent a message:\n<teammate-message>hi</teammate-message>",
            ),
            user_row("[cache-keepalive] Ping 2/4"),
        ];
        let mut counters: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        for row in &rows {
            *counters
                .entry(classify_turn(row, &bus, "s").label().to_string())
                .or_insert(0) += 1;
        }
        assert_eq!(counters.get("operator"), Some(&3));
        assert_eq!(counters.get("relay_fno_mail"), Some(&1));
        assert_eq!(counters.get("relay_teammate_message"), Some(&1));
        assert_eq!(counters.get("keepalive"), Some(&1));
    }

    #[test]
    fn meta_rows_map_to_harness_kinds_never_operator() {
        for (text, label) in [
            ("Stop hook feedback: continue working", "harness_stop_hook"),
            ("reign check-in: scan the board", "harness_loop_wakeup"),
            ("Check in on your territory", "harness_loop_wakeup"),
            ("Base directory for this skill: /x", "harness_skill_body"),
        ] {
            let row = json!({"type": "user", "isMeta": true,
                             "message": {"role": "user", "content": text}});
            assert_eq!(
                classify_turn(&row, &BusIndex { rows: Vec::new() }, "s").label(),
                label
            );
        }
    }

    #[test]
    fn a_codex_payload_row_with_a_bus_body_is_relay_too() {
        let bus = bus_index(json!([{
            "v": 1, "id": "msg-3", "ts": "2026-09-16T11:00:00Z",
            "meta": {"to_session": "codex-s"}, "body": "run the review lane"
        }]));
        let row = json!({
            "payload": {"type": "message", "role": "user", "content": "run the review lane"},
            "timestamp": "2026-09-16T12:00:00.000Z"
        });
        assert_eq!(
            classify_turn(&row, &bus, "codex-s"),
            Provenance::Relay(RelayKind::BusRow)
        );
    }

    #[test]
    fn every_label_is_distinct_and_the_fold_can_zero_fill() {
        let labels = Provenance::all_labels();
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len());
        assert!(labels.contains(&"operator"));
        assert!(labels.contains(&"relay_bus_row"));
    }

    #[test]
    fn rollout_session_id_reads_the_trailing_uuid() {
        assert_eq!(
            rollout_session_id("rollout-2026-09-16T12-00-00-0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3"),
            "0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3"
        );
        assert_eq!(rollout_session_id("rollout-plain"), "rollout-plain");
    }
}
