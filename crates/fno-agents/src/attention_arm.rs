//! The `attention` daemon arm: deliver attention items to `[[attention]]`
//! `md` sinks, read answers back after the settle window, record
//! `attention_answer` rows, and flip delivered blocks closed. Zero sinks
//! configured means the arm never opens a file.

use crate::attention::AttentionItem;
use crate::attention_file::{self, FileAnswer, FileBlock, FileSinkConfig};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The arm's beat, matching its KNOWN_ARMS row.
pub const ATTENTION_INTERVAL_S: u64 = 30;

/// How long an edit must hold still before it records. The sync client, the
/// editor and the arm share the file; the wait is insurance against reading
/// a half-settled edit.
pub const DEFAULT_SETTLE_SECS: u64 = 120;

/// How many times one recorded answer retries its failed clear before the
/// arm gives up and leaves the durable row for a human to finish.
pub const CLEAR_RETRY_CAP: u32 = 5;

/// One `[[attention]]` sink row. Unknown types and bad rows arrive as
/// errors, never silently dropped (AC6-ERR).
#[derive(Debug, Clone, PartialEq)]
pub enum SinkOrErr {
    Ok(SinkConfig),
    Err(String),
}

/// One writable `md` sink.
#[derive(Debug, Clone, PartialEq)]
pub struct SinkConfig {
    pub name: String,
    pub path: PathBuf,
    pub tag: String,
    pub line: String,
    pub option_line: String,
    pub settle_secs: u64,
    pub ready_only: bool,
    pub kinds: Vec<String>,
    pub match_project: Option<String>,
}

fn default_line() -> String {
    crate::attention_file::FileSinkConfig::default().line
}

fn default_option_line() -> String {
    crate::attention_file::FileSinkConfig::default().option_line
}

impl SinkConfig {
    /// The pure render/read half keyed off this row.
    pub fn file_config(&self) -> FileSinkConfig {
        FileSinkConfig {
            name: self.name.clone(),
            path: self.path.clone(),
            tag: self.tag.clone(),
            line: self.line.clone(),
            option_line: self.option_line.clone(),
            ready_only: self.ready_only,
        }
    }
}

/// Read `[[attention]]` from the layered config, first-hit per key. The
/// retired `[[reach_me]]` name reads for one release; loading it warns and
/// names the new key (operator ruling, 2026-09-21).
pub fn attention_sinks(cwd: &Path) -> Vec<SinkOrErr> {
    if let Some(value) = crate::agents_config::config_lookup(cwd, &["attention"]) {
        return parse_sinks(&value);
    }
    match crate::agents_config::config_lookup(cwd, &["reach_me"]) {
        Some(value) => {
            eprintln!(
                "fno-agents attention: config key [[reach_me]] is now [[attention]]; rename it (the old name reads for one release)."
            );
            parse_sinks(&value)
        }
        None => vec![],
    }
}

fn parse_sinks(value: &toml::Value) -> Vec<SinkOrErr> {
    let Some(rows) = value.as_array() else {
        return vec![SinkOrErr::Err(
            "attention is not an array of tables".to_string(),
        )];
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    rows.iter()
        .enumerate()
        .map(|(i, row)| match parse_sink(row, i) {
            SinkOrErr::Ok(cfg) if !seen.insert(cfg.name.clone()) => SinkOrErr::Err(format!(
                "attention[{i}]: duplicate sink name {:?}; each row needs its own",
                cfg.name
            )),
            other => other,
        })
        .collect()
}

fn parse_sink(row: &toml::Value, index: usize) -> SinkOrErr {
    let get_str = |key: &str| -> Option<String> {
        row.get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    let name = match get_str("name").filter(|n| !n.is_empty()) {
        Some(n) => n,
        None => {
            // The documented setup writes only `type` and `path`; derive a
            // stable name from the file stem so the doc's claim stays true.
            let path_raw = get_str("path").unwrap_or_default();
            let stem = std::path::Path::new(path_raw.trim_start_matches("~/"))
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            match stem {
                Some(s) => s,
                None => {
                    return SinkOrErr::Err(format!(
                        "attention[{index}]: missing name (and no path to derive one from)"
                    ))
                }
            }
        }
    };
    let sink_type = get_str("type").unwrap_or_else(|| "md".to_string());
    if sink_type != "md" {
        return SinkOrErr::Err(format!(
            "attention[{index}] ({name}): unknown type {sink_type:?} (only \"md\" ships today)"
        ));
    }
    let Some(path_raw) = get_str("path").filter(|p| !p.is_empty()) else {
        return SinkOrErr::Err(format!(
            "attention[{index}] ({name}): missing path (the one required key)"
        ));
    };
    let tag = get_str("tag").unwrap_or("#fno".to_string());
    let line = get_str("line").unwrap_or_else(default_line);
    let option_line = get_str("option_line").unwrap_or_else(default_option_line);
    let settle_secs = row
        .get("settle_secs")
        .and_then(toml::Value::as_integer)
        .and_then(|i| u64::try_from(i).ok())
        .unwrap_or(DEFAULT_SETTLE_SECS);
    let ready_only = row
        .get("ready_only")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let kinds = row
        .get("kinds")
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec!["question".to_string(), "pin".to_string()]);
    let match_project = get_str("match_project");
    SinkOrErr::Ok(SinkConfig {
        name,
        path: expand_home_path(&path_raw),
        tag,
        line,
        option_line,
        settle_secs,
        ready_only,
        kinds,
        match_project,
    })
}

fn expand_home_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

/// The IO seam the tick body takes, so tests drive everything without disk
/// or a real `fno` shellout.
pub trait SinkIo {
    fn read(&mut self, path: &Path) -> std::io::Result<String>;
    /// Append-mode write; cannot drop another writer's text.
    fn append(&mut self, path: &Path, block: &str) -> std::io::Result<()>;
    /// Whole-file atomic rewrite (tmp + rename in the same directory).
    fn write_atomic(&mut self, path: &Path, content: &str) -> std::io::Result<()>;
    /// Write one `attention_answer` row. Returns the `Recorded:` receipt text.
    fn record(
        &mut self,
        item: &AttentionItem,
        sink_name: &str,
        answer: &FileAnswer,
    ) -> Result<String, String>;
    /// `fno inbox outstanding clear` with the answer text.
    fn clear(&mut self, id: &str, answer_text: &str) -> Result<(), String>;
    fn notify(&mut self, title: &str, body: &str);
}

/// Per-sink tick result.
#[derive(Debug, Default)]
pub struct SinkTick {
    pub delivered: u64,
    pub recorded: u64,
    pub flips: u64,
    pub skip: Option<String>,
    pub detail: Vec<String>,
}

/// The pure, IO-injected tick body over ONE sink, in the plan's order:
/// route + filter, refuse conflict markers, append missing blocks, settle +
/// record + flip, then close flips for items closed elsewhere.
pub fn tick_sink(
    items: &[AttentionItem],
    sink: &SinkConfig,
    state: &mut HashMap<String, BlockState>,
    now: u64,
    io: &mut dyn SinkIo,
) -> SinkTick {
    let routed: Vec<&AttentionItem> = items
        .iter()
        .filter(|i| sink.kinds.iter().any(|k| k == &i.kind))
        .filter(|i| sink.match_project.as_ref().is_none_or(|p| p == &i.project))
        .filter(|i| !sink.ready_only || i.ready)
        // Wave 1 has no door that closes an escalation note (`clear` only
        // knows question ids and no-ops on unknown ones), so note items stay
        // on the projection and the mux until the note-closing path ships.
        .filter(|i| !i.id.starts_with("note-"))
        .collect();
    // Every open item id, routed or not: a block whose item is merely
    // filtered away (wrong kind, not ready) is NOT closed elsewhere and
    // must never flip.
    let all_open: std::collections::HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let file_text = match io.read(&sink.path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return SinkTick {
                skip: Some("error".to_string()),
                detail: vec![format!("{}: read failed: {e}", sink.name)],
                ..Default::default()
            }
        }
    };
    if attention_file::has_conflict_markers(&file_text) {
        io.notify(
            "Attention sink has conflict markers",
            &format!(
                "{} will not be parsed or written until resolved",
                sink.path.display()
            ),
        );
        return SinkTick {
            skip: Some("conflict_markers".to_string()),
            ..Default::default()
        };
    }
    let existing = attention_file::blocks(&file_text);
    let mut tick = SinkTick::default();
    tick.delivered = deliver_missing(&routed, sink, &existing, io);
    // Settle + record + flip over the delivered file state.
    let file_text2 = match io.read(&sink.path) {
        Ok(t) => t,
        Err(_) => file_text,
    };
    let existing2 = attention_file::blocks(&file_text2);
    let open_by_id: HashMap<&str, &AttentionItem> =
        routed.iter().map(|i| (i.id.as_str(), *i)).collect();
    // Prune state for items no longer open+routed (keeps the file bounded).
    let mut ids: Vec<&str> = open_by_id.keys().copied().collect();
    ids.sort();
    let mut keep: HashMap<String, BlockState> = HashMap::new();
    for id in &ids {
        if let Some(s) = state.remove(*id) {
            keep.insert((*id).to_string(), s);
        }
    }
    *state = keep;
    let mut close_ids: Vec<(String, String)> = Vec::new();
    for block in &existing2 {
        let Some(item) = open_by_id.get(block.id.as_str()) else {
            // Not routed: flip only when the item truly closed (absent from
            // the whole open set), never when it was merely filtered away.
            // An already-flipped block is left alone, or every beat would
            // append another `Recorded:` receipt to it forever.
            let already_closed = block
                .text
                .lines()
                .next()
                .map(|l| l.trim_start().starts_with("- [x]"))
                .unwrap_or(false);
            if !already_closed
                && !all_open.contains(block.id.as_str())
                && block_was_ours(block, &sink.tag)
            {
                close_ids.push((
                    block.id.clone(),
                    "Recorded: the item closed away from the file".to_string(),
                ));
            }
            continue;
        };
        settle_block(
            block,
            item,
            sink,
            state,
            now,
            io,
            &mut tick.recorded,
            &mut close_ids,
        );
    }
    if !close_ids.is_empty() {
        apply_flips(sink, io, &file_text2, close_ids, &mut tick);
    }
    tick
}

/// The settle half for one block: hash-compare, wait out the settle window,
/// then record the answer and queue the flip. The tick row names the flow.
#[allow(clippy::too_many_arguments)]
fn settle_block(
    block: &FileBlock,
    item: &AttentionItem,
    sink: &SinkConfig,
    state: &mut HashMap<String, BlockState>,
    now: u64,
    io: &mut dyn SinkIo,
    recorded: &mut u64,
    close_ids: &mut Vec<(String, String)>,
) {
    let hash = hash_text(&block.text);
    let entry = state.get(&block.id).cloned();
    match entry {
        None => {
            state.insert(
                block.id.clone(),
                BlockState {
                    hash,
                    since: now,
                    recorded: false,
                    answer: String::new(),
                    tt: false,
                    retries: 0,
                },
            );
        }
        Some(bs) if bs.hash == hash && bs.recorded => {
            // The row is durable; the clear failed. Retry the clear only
            // (AC5-ERR: no second row), with a bounded number of attempts:
            // an answer the door refuses forever must not shell out on
            // every beat for the life of the block. The durable row stays
            // either way; a human or a terminal close can finish the job.
            if bs.retries >= CLEAR_RETRY_CAP {
                return;
            }
            if let Err(e) = io.clear(&block.id, &bs.answer) {
                eprintln!("fno-agents attention: clear retry failed: {e}");
                if let Some(s) = state.get_mut(&block.id) {
                    s.retries += 1;
                    if s.retries == CLEAR_RETRY_CAP {
                        io.notify(
                            "Answer could not be recorded",
                            &format!(
                                "{}: the clear kept failing; the answer row is durable, close it from a terminal.",
                                item.title
                            ),
                        );
                    }
                }
            }
        }
        Some(bs) if bs.hash == hash && now.saturating_sub(bs.since) < sink.settle_secs => {
            // Still settling; nothing records yet (AC4-EDGE).
        }
        Some(bs) if bs.hash == hash => match attention_file::read_answer(block) {
            FileAnswer::None => {}
            FileAnswer::TwoTicked => {
                if !bs.tt {
                    io.notify(
                        "Two options ticked",
                        &format!(
                            "You ticked two options on {}. Leave one ticked.",
                            item.title
                        ),
                    );
                    if let Some(s) = state.get_mut(&block.id) {
                        s.tt = true;
                    }
                }
            }
            other => {
                // A ticked top line means "done" only for a pin; on a
                // question it would close the ask without naming the option.
                if matches!(other, FileAnswer::Done) && item.kind != "pin" {
                    return;
                }
                match io.record(item, &sink.name, &other) {
                    Ok(receipt) => {
                        let answer_text = answer_text_of(item, &other);
                        *recorded += 1;
                        if let Some(s) = state.get_mut(&block.id) {
                            s.recorded = true;
                            s.answer = answer_text.clone();
                        }
                        if io.clear(&block.id, &answer_text).is_ok() {
                            close_ids.push((block.id.clone(), receipt));
                        }
                    }
                    Err(e) => {
                        // A failed append changed nothing durable: leave the
                        // state untouched so the next beat retries the whole
                        // record, and keep the notice out of the user's face.
                        eprintln!("fno-agents attention: record failed: {e}");
                    }
                }
            }
        },
        Some(bs) => {
            // The block changed: restart the settle window.
            state.insert(
                block.id.clone(),
                BlockState {
                    hash,
                    since: now,
                    recorded: bs.recorded,
                    answer: bs.answer,
                    tt: false,
                    retries: bs.retries,
                },
            );
        }
    }
}

/// The settle state for one delivered block, persisted per sink at
/// `~/.fno/attention/<name>.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockState {
    hash: u64,
    since: u64,
    recorded: bool,
    answer: String,
    tt: bool,
    #[serde(default)]
    retries: u32,
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Append blocks for routed open items the file lacks. Append-mode writes
/// cannot drop another writer's text.
fn deliver_missing(
    routed: &[&AttentionItem],
    sink: &SinkConfig,
    existing: &[FileBlock],
    io: &mut dyn SinkIo,
) -> u64 {
    let mut delivered = 0u64;
    for item in routed {
        if existing.iter().any(|b| b.id == item.id) {
            continue;
        }
        let rendered = attention_file::render_item(item, &sink.file_config());
        if io.append(&sink.path, &format!("{rendered}\n")).is_ok() {
            delivered += 1;
        }
    }
    delivered
}

/// A block is ours when its top line carries the sink's tag; the flip path
/// never touches the user's own anchored task lines.
fn block_was_ours(block: &FileBlock, tag: &str) -> bool {
    block
        .text
        .lines()
        .next()
        .map(|l| l.contains(tag))
        .unwrap_or(false)
}

/// The answer text `fno inbox outstanding clear` receives: the option's text
/// for a tick, the words for a written answer, `done` for a pin.
fn answer_text_of(item: &AttentionItem, answer: &FileAnswer) -> String {
    match answer {
        FileAnswer::Option(n) => item
            .options
            .iter()
            .find(|o| o.n == *n)
            .map(|o| o.text.clone())
            .unwrap_or_else(|| format!("option {n}")),
        FileAnswer::Words(w) => w.clone(),
        FileAnswer::Done => "done".to_string(),
        FileAnswer::None | FileAnswer::TwoTicked => String::new(),
    }
}

/// Apply the queued close flips in ONE guarded rewrite. The re-read compare
/// is the AC20-ERR gate: another writer's text between our read and write
/// skips the beat; the next beat flips the line.
fn apply_flips(
    sink: &SinkConfig,
    io: &mut dyn SinkIo,
    file_text2: &str,
    close_ids: Vec<(String, String)>,
    tick: &mut SinkTick,
) {
    let Some(cur) = io.read(&sink.path).ok() else {
        return;
    };
    if cur != file_text2 {
        tick.skip = Some("file_changed".to_string());
        return;
    }
    let mut new_text = cur.clone();
    let date = date_str();
    for (id, receipt) in &close_ids {
        new_text = attention_file::close_block(&new_text, id, receipt, &date);
    }
    let guard = io.read(&sink.path).ok();
    if guard.as_deref() != Some(cur.as_str()) {
        tick.skip = Some("file_changed".to_string());
        return;
    }
    match io.write_atomic(&sink.path, &new_text) {
        Ok(()) => tick.flips = close_ids.len() as u64,
        Err(e) => {
            eprintln!("fno-agents attention: close rewrite failed: {e}");
            let changed = io.read(&sink.path).ok().as_deref() != Some(cur.as_str());
            tick.skip = Some(
                if changed {
                    "file_changed"
                } else {
                    "write_failed"
                }
                .to_string(),
            );
        }
    }
}

fn date_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// The arm as the daemon holds it: cadence stamp plus one-in-flight gate.
pub struct Arm {
    config_cwd: PathBuf,
    last_tick: Mutex<Option<std::time::Instant>>,
    in_flight: Arc<AtomicBool>,
}

impl Arm {
    pub fn new(config_cwd: PathBuf) -> Self {
        Arm {
            config_cwd,
            last_tick: Mutex::new(None),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Load one sink's settle state from `~/.fno/attention/<name>.json`.
fn load_state(path: &Path) -> HashMap<String, BlockState> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Save one sink's settle state atomically.
fn save_state(path: &Path, state: &HashMap<String, BlockState>) {
    if let Some(parent) = attention_dir().ok() {
        let _ = std::fs::create_dir_all(&parent);
    }
    let tmp = path.with_extension("json.tmp");
    if serde_json::to_string(state)
        .map(|s| std::fs::write(&tmp, s))
        .unwrap_or(Err(std::io::Error::other("serialize")))
        .is_ok()
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// `~/.fno/attention/`, beside `~/.fno` like `questions.jsonl`.
pub fn attention_dir() -> Result<PathBuf, std::io::Error> {
    let home = crate::paths::AgentsHome::from_env();
    home.root()
        .parent()
        .map(|p| p.join("attention"))
        .ok_or_else(|| std::io::Error::other("no agents home parent"))
}

/// The not-ready bounce: one wrapped mail to a not-ready item's asker,
/// naming the id and the missing fields, once per item ever (the second
/// beat stays quiet via the persisted bounce set). AC9-HP.
fn bounce_not_ready(items: &[AttentionItem], dir: &Path) {
    bounce_not_ready_with(items, dir, &|asker, body| {
        let mut cmd = crate::loop_dispatch::fno_cmd("fno");
        cmd.args(["agents", "mail", "send", asker, body]);
        matches!(
            crate::loop_dispatch::retry_etxtbsy(move || cmd.output()),
            Ok(out) if out.status.success()
        )
    });
}

/// [`bounce_not_ready`] with the send injected, so a test counts sends
/// without a `fno` shellout.
fn bounce_not_ready_with(items: &[AttentionItem], dir: &Path, send: &dyn Fn(&str, &str) -> bool) {
    let path = dir.join("bounced.json");
    let mut bounced: std::collections::HashSet<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let mut dirty = false;
    for item in items {
        if item.ready || !matches!(item.kind.as_str(), "question" | "pin") {
            continue;
        }
        let Some(asker) = item
            .asker
            .as_ref()
            .map(|a| a.handle.clone())
            .filter(|h| !h.trim().is_empty())
        else {
            continue;
        };
        if bounced.contains(&item.id) {
            continue;
        }
        let body = format!(
            "Question {} is missing: {}. Re-ask with the fields (docs/architecture/attention-items.md) or clear it.",
            item.id,
            item.missing.join(", ")
        );
        if send(&asker, &body) {
            bounced.insert(item.id.clone());
            dirty = true;
        }
    }
    if dirty {
        if let Ok(s) = serde_json::to_string(&bounced) {
            let _ = std::fs::write(&path, s);
        }
    }
}

/// Fold `user_ask_answered` rows into every sink: one closed line per row,
/// appended once (the acked set persists beside the settle state). AC14-HP.
fn fold_user_ask_answered(
    index: &Path,
    sinks: &[SinkConfig],
    dir: &Path,
    io: &mut dyn SinkIo,
) -> u64 {
    let seen_path = dir.join("answered.json");
    let mut seen: std::collections::HashSet<String> = std::fs::read_to_string(&seen_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let raw = std::fs::read_to_string(index).unwrap_or_default();
    let mut appended = 0u64;
    let mut dirty = false;
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user_ask_answered") {
            continue;
        }
        let data = v.get("data");
        let session = data
            .and_then(|d| d.get("session_id"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let turn = data
            .and_then(|d| d.get("turn_id"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if session.is_empty() || turn.is_empty() {
            continue;
        }
        let key = format!("{session}:{turn}");
        if seen.contains(&key) {
            continue;
        }
        let field = |name: &str| -> String {
            data.and_then(|d| d.get(name))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        let (excerpt, answer) = (field("excerpt"), field("answer"));
        let date: String = v
            .get("ts")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .chars()
            .take(10)
            .collect();
        let closed_line =
            format!("- [x] You asked ({session}): \"{excerpt}\". Answer: {answer}. ✅ {date}\n");
        for sink in sinks {
            if io.append(&sink.path, &closed_line).is_ok() {
                appended += 1;
            }
        }
        seen.insert(key);
        dirty = true;
    }
    if dirty {
        if let Ok(s) = serde_json::to_string(&seen) {
            let _ = std::fs::write(&seen_path, s);
        }
    }
    appended
}

/// The daemon-facing wrapper: due-check plus one-in-flight gate (the
/// `merge_close::maybe_tick` shape). The body runs off-loop.
pub fn maybe_tick(arm: &Arm, home: crate::paths::AgentsHome) {
    let interval = std::time::Duration::from_secs(ATTENTION_INTERVAL_S);
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
    let cwd = arm.config_cwd.clone();
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let sinks = attention_sinks(&cwd);
        if sinks.is_empty() {
            emit_tick_row(&home, 0, Some("no_sinks"), "no [[attention]] configured");
            return;
        }
        let (items, unreadable) = read_items(&cwd);
        if !unreadable.is_empty() {
            // An incomplete projection must never drive delivery or the
            // close-elsewhere flip: absent ids would read as closed and
            // flip live blocks that the projection could not see.
            emit_tick_row(&home, 0, Some("source_unreadable"), &unreadable.join("; "));
            return;
        }
        write_items_cache(&home, &items);
        let dir = match attention_dir() {
            Ok(d) => d,
            Err(e) => {
                emit_tick_row(&home, 0, Some("error"), &format!("attention dir: {e}"));
                return;
            }
        };
        bounce_not_ready(&items, &dir);
        let ok_sinks: Vec<SinkConfig> = sinks
            .iter()
            .filter_map(|e| match e {
                SinkOrErr::Ok(s) => Some(s.clone()),
                SinkOrErr::Err(_) => None,
            })
            .collect();
        let answered = if ok_sinks.is_empty() {
            0
        } else {
            fold_user_ask_answered(
                &crate::provider_cap::questions_path(&home),
                &ok_sinks,
                &dir,
                &mut RealIo,
            )
        };
        let mut acted = answered;
        let mut skip: Option<String> = None;
        let mut detail: Vec<String> = Vec::new();
        for entry in sinks {
            match entry {
                SinkOrErr::Ok(sink) => {
                    let state_path = dir.join(format!("{}.json", sink.name));
                    let mut state = load_state(&state_path);
                    let t = tick_sink(&items, &sink, &mut state, now_secs(), &mut RealIo);
                    save_state(&state_path, &state);
                    acted += t.delivered + t.recorded + t.flips;
                    if skip.is_none() {
                        skip = t.skip.clone();
                    }
                    detail.extend(t.detail);
                }
                SinkOrErr::Err(e) => {
                    if skip.is_none() {
                        skip = Some("error".to_string());
                    }
                    detail.push(e);
                }
            }
        }
        if acted == 0 && skip.is_none() {
            skip = Some("no_open_items".to_string());
        }
        let summary = detail.join("; ");
        emit_tick_row(&home, acted, skip.as_deref(), &summary);
    });
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The projection read: question journals + escalation notes + user lane,
/// the same three stores `fno-agents needs --items` folds. Returns the items
/// plus the names of stores that exist but could not be read: an unreadable
/// store means an INCOMPLETE projection, and the caller must not treat its
/// absent ids as closed.
fn read_items(cwd: &Path) -> (Vec<AttentionItem>, Vec<String>) {
    let fno_dir = crate::paths::AgentsHome::from_env()
        .root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let mut journals_raw = String::new();
    let mut unreadable: Vec<String> = Vec::new();
    for path in crate::needs::question_journals(&fno_dir, cwd) {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                journals_raw.push_str(&content);
                if !content.ends_with('\n') {
                    journals_raw.push('\n');
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => unreadable.push(format!(
                "{}: {e}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("journal")
            )),
        }
    }
    let notes = read_notes(cwd);
    let lane_path = crate::king_board::scope::operator_lane_path(cwd);
    let lane_text = std::fs::read_to_string(lane_path).unwrap_or_default();
    let mut items = crate::attention::project(&journals_raw, &notes, &lane_text, now_secs());
    if let Ok(registry) =
        crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
    {
        crate::attention::attach_reach(&mut items, &registry);
    }
    (items, unreadable)
}

/// Escalation notes as (slug, text) pairs.
fn read_notes(cwd: &Path) -> Vec<(String, String)> {
    let dir = crate::escalation::dir(cwd);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    for path in paths {
        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("note")
            .to_string();
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.push((slug, text));
        }
    }
    out
}

/// The projection cache the prompt hook reads (wave 4). `{as_of, items}`.
fn write_items_cache(_home: &crate::paths::AgentsHome, items: &[AttentionItem]) {
    let Ok(dir) = attention_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let payload = json!({ "as_of": now_secs(), "items": items });
    let tmp = dir.join(".items.tmp");
    if serde_json::to_string(&payload)
        .map(|s| std::fs::write(&tmp, s))
        .unwrap_or(Err(std::io::Error::other("serialize")))
        .is_ok()
    {
        let _ = std::fs::rename(&tmp, dir.join("items.json"));
    }
}

/// The tick row: one per beat, the same shape every arm writes.
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
        "attention",
        crate::tick_ledger::SCHED_DAEMON,
        acted,
        skip_reason,
        Some(detail),
        ATTENTION_INTERVAL_S,
    );
}

/// The real IO: disk for the sink file, `questions.jsonl` for the answer row,
/// `fno` for the clear, the confirmed notice for the user.
struct RealIo;

impl SinkIo for RealIo {
    fn read(&mut self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn append(&mut self, path: &Path, block: &str) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(block.as_bytes())
    }

    fn write_atomic(&mut self, path: &Path, content: &str) -> std::io::Result<()> {
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, content)?;
        let mode = std::fs::metadata(path).ok().map(|m| m.permissions());
        let res = std::fs::rename(&tmp, path);
        if let (Ok(()), Some(mode)) = (&res, mode) {
            let _ = std::fs::set_permissions(path, mode);
        }
        res
    }

    fn record(
        &mut self,
        item: &AttentionItem,
        sink_name: &str,
        answer: &FileAnswer,
    ) -> Result<String, String> {
        let (option, words, done) = match answer {
            FileAnswer::Option(n) => (Some(*n as i64), String::new(), false),
            FileAnswer::Words(w) => (None, w.clone(), false),
            FileAnswer::Done => (None, String::new(), true),
            FileAnswer::None | FileAnswer::TwoTicked => (None, String::new(), false),
        };
        let answered_at = chrono::Utc::now().to_rfc3339();
        // First answer wins across sinks: an earlier unsuperseded row for
        // this item makes this one a superseded marker that changes nothing.
        let home = crate::paths::AgentsHome::from_env();
        let path = crate::provider_cap::questions_path(&home);
        let already_won = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .any(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .is_some_and(|v| {
                        v.get("type").and_then(serde_json::Value::as_str)
                            == Some("attention_answer")
                            && v.get("data")
                                .and_then(|d| d.get("item_id"))
                                .and_then(serde_json::Value::as_str)
                                == Some(item.id.as_str())
                            && v.get("data")
                                .and_then(|d| d.get("superseded"))
                                .and_then(serde_json::Value::as_bool)
                                != Some(true)
                    })
            });
        let receipt = match (answer, already_won) {
            (FileAnswer::Option(n), false) => format!("Recorded: option {n} (file)"),
            (FileAnswer::Words(_), false) => "Recorded: words (file)".to_string(),
            (FileAnswer::Done, false) => "Recorded: done (file)".to_string(),
            (FileAnswer::None | FileAnswer::TwoTicked, false) => String::new(),
            (_, true) => "Recorded: superseded by an earlier answer".to_string(),
        };
        let row = json!({
            "ts": answered_at,
            "type": "attention_answer",
            "source": "daemon",
            "data": {
                "item_id": item.id,
                "sink": sink_name,
                "option": option,
                "words": words,
                "done": done,
                "answered_at": answered_at,
                "authority": "file_edit",
                "attested_by": format!("file:{}", item.id),
                "mapped_by": "attention_arm",
                "superseded": already_won,
                "decision_id": null,
            }
        });
        // The row is the durable half of the contract: an append that fails
        // must fail the record so the settle state never marks it recorded.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        writeln!(f, "{row}").map_err(|e| e.to_string())?;
        Ok(receipt)
    }

    fn clear(&mut self, id: &str, answer_text: &str) -> Result<(), String> {
        let mut cmd = crate::loop_dispatch::fno_cmd("fno");
        cmd.args(["inbox", "outstanding", "clear", id, "--answer", answer_text])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::loop_dispatch::retry_etxtbsy(move || cmd.output())
            .map_err(|e| e.to_string())
            .and_then(|out| {
                if out.status.success() {
                    Ok(())
                } else {
                    Err(format!("clear exited {}", out.status.code().unwrap_or(-1)))
                }
            })
    }

    fn notify(&mut self, title: &str, body: &str) {
        crate::operator_notice::notify_operator_confirmed(title, body, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::project;

    /// One not-ready question with a live asker: every context field but
    /// `unknowns` is present, so `missing` names exactly one field.
    fn not_ready_items() -> Vec<AttentionItem> {
        let row = r#"{"ts":"2026-09-18T12:00:00Z","type":"operator_question","source":"test","data":{"question_id":"q-nr","question":"Which?","ask":"pick","session_id":"s1","cwd":"/repo/fno","node":"x-aaaa","asker":"worker-1","options":[{"n":1,"text":"A","next":"x"},{"n":2,"text":"B","next":"y"}],"context":{"blocked_because":"two repairs are Python edits","options_rationale":"the readings kings acted on","recommendation":{"option":1,"why":"narrowest"},"reversible":"costly","cost_if_wrong":"allowance drops","meanwhile":"stops"}}}"#;
        project(row, &[], "", 0)
    }

    fn bounce_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "attention-bounce-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn ac9_hp_the_bounce_mails_once_then_stays_quiet() {
        let items = not_ready_items();
        assert!(!items[0].ready);
        let dir = bounce_dir("once");
        std::fs::create_dir_all(&dir).unwrap();
        let sends = std::sync::atomic::AtomicUsize::new(0);
        {
            let send = |asker: &str, body: &str| {
                use std::sync::atomic::Ordering;
                assert_eq!(asker, "worker-1");
                assert!(body.contains("q-nr"), "names the item id: {body}");
                assert!(
                    body.contains("missing unknowns"),
                    "names the missing field: {body}"
                );
                sends.fetch_add(1, Ordering::SeqCst);
                true
            };
            bounce_not_ready_with(&items, &dir, &send);
        }
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        // Second beat: quiet.
        bounce_not_ready_with(&items, &dir, &|_a, _b| {
            panic!("a second beat must stay quiet");
        });
    }

    #[test]
    fn ac9_hp_a_failed_bounce_send_is_retried_next_beat() {
        let items = not_ready_items();
        let dir = bounce_dir("retry");
        std::fs::create_dir_all(&dir).unwrap();
        bounce_not_ready_with(&items, &dir, &|_a, _b| false);
        let retried = std::sync::atomic::AtomicUsize::new(0);
        bounce_not_ready_with(&items, &dir, &|a, b| {
            use std::sync::atomic::Ordering;
            assert_eq!(a, "worker-1");
            assert!(b.contains("q-nr"));
            retried.fetch_add(1, Ordering::SeqCst);
            true
        });
        assert_eq!(retried.load(Ordering::SeqCst), 1);
    }
}
