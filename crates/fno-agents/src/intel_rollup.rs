//! The per-session rollup cache behind `fno-agents intel`: what one
//! transcript's fold produced so far, kept beside the fleet cursor cache at
//! `<agents-home>/intel/rollups.json` so the next run reads only the bytes
//! appended since the last pass. One entry per transcript path: a byte
//! offset, a sha256 head sample for replacement detection, the frozen
//! counters that never change on re-read, and the `bindable` turns that
//! replay through the operator witness every run because a submit row can
//! land after the turn was first folded.
//!
//! Scope: the claude and codex stores, whose transcripts are plain JSONL
//! already in the fold's row shape. Opencode renders a sqlite store (no
//! byte offsets) and the footnote store re-renders its rows on read; both
//! keep the whole-transcript path. The first intel run after this ships
//! pays one full backfill; every later run reads only tails. Two concurrent
//! intel runs serialize on the cache lock.

use crate::gh_budget::{lock_path, FileLock};
use crate::provenance::{BusIndex, SessionFile, TranscriptSource};
use crate::session_activity::{assistant_message_id, ActivityFold};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const CHUNK: u64 = crate::transcript_activity::CHUNK;
const HEAD_SAMPLE_BYTES: u64 = crate::transcript_activity::HEAD_SAMPLE_BYTES;
const RECENT_IDS: usize = 64;
const WORD_CONTRACT: usize = 80;

/// The stored fold of one transcript: everything that does not change when
/// the file only grows, plus the turns that replay.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct RollupEntry {
    offset: u64,
    head_sha: String,
    head_len: u64,
    session: String,
    /// Static labels only: `operator`, `unknown`, and
    /// `harness_command_invocation` are derived from the replay each run.
    counters: BTreeMap<String, u64>,
    /// Unknown and CommandInvocation turns in transcript order, replayed
    /// through the witness bind every run.
    bindable: Vec<BindableTurn>,
    /// Turns classify_turn shapes as operator outright: witnessed by row
    /// shape, not by the submit window.
    operator_direct: Vec<DirectTurn>,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
    tool_use: u64,
    tokens: crate::session_activity::Tokens,
    lines_added: u64,
    lines_removed: u64,
    tool_errors: BTreeMap<String, u64>,
    languages: BTreeMap<String, u64>,
    aborted_turns: u64,
    /// The running last assistant timestamp, so a tail turn's response gap
    /// reaches back across an offset boundary.
    last_assistant_ms: Option<i64>,
    /// Bus-row ids found in the text so far: the delivery check skips
    /// re-scanning stored bytes for them.
    relay_ids: BTreeSet<String>,
    /// The last assistant message ids: seeds the next pass's token dedupe.
    recent_message_ids: Vec<String>,
    /// Landing pad for the friction/smooth marker detectors; nothing
    /// writes it yet.
    #[serde(default)]
    #[allow(dead_code)]
    markers: BTreeMap<String, u64>,
}

/// One bindable turn: the ms the witness bind needs, whether the turn is a
/// command invocation (a bound command consumes the submit but stays a
/// command), the response-gap anchor before it, and the raw timestamp
/// string the report quotes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BindableTurn {
    ms: Option<i64>,
    command: bool,
    prev_assistant_ms: Option<i64>,
    ts: Option<String>,
}

/// One directly-operator turn: stored so its response gap survives without
/// keeping every assistant timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DirectTurn {
    ms: Option<i64>,
    ts: String,
    prev_assistant_ms: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RollupCache {
    version: u32,
    entries: BTreeMap<String, RollupEntry>,
}

/// What one intel run's advance pass read.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct AdvanceReceipt {
    pub(crate) files_advanced: u64,
    pub(crate) bytes_read: u64,
    /// Entries discarded because the file shrank or its head changed.
    pub(crate) replaced: u64,
    /// Files skipped because their head sample could not be read.
    pub(crate) pending: u64,
}

/// The loaded cache plus the receipt of this run's advances. The caller
/// holds the cache lock across load, every advance, and the save.
pub(crate) struct RollupStore {
    path: PathBuf,
    cache: RollupCache,
    pub(crate) receipt: AdvanceReceipt,
}

impl RollupStore {
    /// The production cache path: beside the fleet cursor cache. `None`
    /// when no agents home is declared (tests run the fold with no
    /// rollup cache at all).
    pub(crate) fn default_path() -> Option<PathBuf> {
        crate::paths::AgentsHome::from_env_opt()
            .map(|home| home.root().join("intel").join("rollups.json"))
    }

    pub(crate) fn acquire_lock(path: &Path) -> FileLock {
        FileLock::acquire(&lock_path(path))
    }

    pub(crate) fn open(path: PathBuf) -> Self {
        let cache = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<RollupCache>(&raw).ok())
            .filter(|c| c.version == VERSION)
            .unwrap_or(RollupCache {
                version: VERSION,
                entries: Default::default(),
            });
        RollupStore {
            path,
            cache,
            receipt: AdvanceReceipt::default(),
        }
    }

    /// Atomic save, mode 600. Best effort: an unwritable cache degrades the
    /// next run to a backfill, never fails this one. Entries whose
    /// transcript is gone ride out with the save.
    pub(crate) fn save(&mut self) {
        self.retain_existing();
        let mut doc = match serde_json::to_vec(&self.cache) {
            Ok(doc) => doc,
            Err(_) => return,
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)
            else {
                return;
            };
            if f.write_all(&mut doc).is_err() {
                return;
            }
        }
        let _ = std::fs::rename(&tmp, &self.path);
    }

    /// Drop entries whose transcript is gone.
    fn retain_existing(&mut self) {
        self.cache.entries.retain(|k, _| Path::new(k).exists());
    }

    /// One transcript's incremental pass: advance the entry over the bytes
    /// appended since the last run (the whole file when the entry is fresh
    /// or was replaced), then hand back a copy for the row build plus the
    /// text this run consumed for the relay scan.
    pub(crate) fn advance(
        &mut self,
        file: &SessionFile,
        source: &dyn TranscriptSource,
        bus: &BusIndex,
    ) -> (RollupEntry, String) {
        let key = file.path.display().to_string();
        self.receipt.files_advanced += 1;
        let (mut entry, fresh) = match self.cache.entries.get(&key) {
            Some(e) => {
                let sampled = crate::transcript_activity::head_sample(&file.path, e.head_len);
                let alive =
                    file.size >= e.offset && sampled.as_deref() == Some(e.head_sha.as_str());
                if alive {
                    (e.clone(), false)
                } else {
                    self.receipt.replaced += 1;
                    (RollupEntry::default(), true)
                }
            }
            None => (RollupEntry::default(), true),
        };
        if fresh {
            entry.head_len = file.size.min(HEAD_SAMPLE_BYTES);
            match crate::transcript_activity::head_sample(&file.path, entry.head_len) {
                Some(head) => entry.head_sha = head,
                None => {
                    // No head, no entry: the next run retries it fresh
                    // instead of carrying a head that reads as replaced
                    // forever.
                    self.receipt.pending += 1;
                    return (RollupEntry::default(), String::new());
                }
            }
            entry.session = file.session_id.clone();
            entry.offset = 0;
        }
        let mut text = String::new();
        if entry.offset < file.size {
            if let Ok(mut f) = std::fs::File::open(&file.path) {
                let start = entry.offset;
                let mut buf = vec![0u8; CHUNK as usize];
                let mut consumed = 0u64;
                let mut pass = TailPass::new(&mut entry, source);
                while start + consumed < file.size {
                    let want = (file.size - start - consumed).min(CHUNK) as usize;
                    if f.seek(SeekFrom::Start(start + consumed)).is_err() {
                        break;
                    }
                    let mut filled = 0;
                    while filled < want {
                        match f.read(&mut buf[filled..want]) {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            Err(_) => break,
                        }
                    }
                    if filled == 0 {
                        break;
                    }
                    let slice = &buf[..filled];
                    // Only complete lines fold: a partial trailing line
                    // waits for the next run, and a full chunk with no
                    // newline is an oversized line that skips forward
                    // uncounted so the fold stays live.
                    let keep = match slice.iter().rposition(|b| *b == b'\n') {
                        Some(pos) => pos + 1,
                        None => {
                            if filled == want && want == CHUNK as usize {
                                filled
                            } else {
                                0
                            }
                        }
                    };
                    if keep == 0 {
                        break;
                    }
                    let chunk = String::from_utf8_lossy(&slice[..keep]);
                    pass.line(&chunk, source, bus, file.session_id.as_str(), &mut text);
                    consumed += keep as u64;
                    self.receipt.bytes_read += keep as u64;
                }
                pass.finish();
                entry.offset = start + consumed;
            }
        }
        // Delivery ids: a bus-row id found in this run's consumed text stays
        // delivered on every later run, whose tail is empty.
        for row in bus.rows() {
            if row.to_session.as_deref() != Some(file.session_id.as_str())
                || row.id.is_empty()
                || entry.relay_ids.contains(&row.id)
            {
                continue;
            }
            if text.contains(row.id.as_str()) {
                entry.relay_ids.insert(row.id.clone());
            }
        }
        self.cache.entries.insert(key, entry.clone());
        (entry, text)
    }
}

/// Which stores roll up: plain JSONL the fold already parses in place.
/// Opencode renders a sqlite store; footnote re-renders its rows on read;
/// both keep the whole-transcript path.
pub(crate) fn eligible(harness: &str) -> bool {
    harness == "claude" || harness == "codex"
}

/// One pass over consumed tail lines: the activity fold, the running
/// assistant anchor, the recent-id window, and the turn records, writing
/// straight into the entry.
struct TailPass<'a> {
    entry: &'a mut RollupEntry,
    fold: ActivityFold,
    recent: VecDeque<String>,
    last_assistant_ms: Option<i64>,
    cumulative: bool,
}

impl<'a> TailPass<'a> {
    fn new(entry: &'a mut RollupEntry, source: &dyn TranscriptSource) -> Self {
        let mut fold = ActivityFold::default();
        fold.seed_seen(entry.recent_message_ids.iter().cloned());
        let anchor = entry.last_assistant_ms;
        TailPass {
            recent: entry.recent_message_ids.iter().cloned().collect(),
            entry,
            fold,
            last_assistant_ms: anchor,
            cumulative: source.tokens_cumulative(),
        }
    }

    fn line(
        &mut self,
        chunk: &str,
        source: &dyn TranscriptSource,
        bus: &BusIndex,
        session: &str,
        text: &mut String,
    ) {
        text.push_str(chunk);
        for line in chunk.lines() {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            self.fold.row(&row);
            if let Some(ts) = self.fold.last_assistant_ts() {
                self.last_assistant_ms = Some((ts * 1000.0) as i64);
            }
            if let Some(id) = assistant_message_id(&row) {
                let id = id.to_string();
                if self.recent.back() != Some(&id) {
                    self.recent.push_back(id);
                    while self.recent.len() > RECENT_IDS {
                        self.recent.pop_front();
                    }
                }
            }
            for turn in source.turns(line) {
                self.turn(&turn, bus, session);
            }
            self.entry.tool_use += source.tool_uses(line) as u64;
        }
    }

    /// One classified turn lands as a static counter, a directly-operator
    /// record, or a bindable record for the replay.
    fn turn(&mut self, turn: &crate::provenance::Turn, bus: &BusIndex, session: &str) {
        // Empty-text turns never counted: not in the counters, not in the
        // span, and they never reached the witness bind before.
        if turn.text.trim().is_empty() {
            return;
        }
        if let Some(ts) = turn.ts_epoch {
            let ms = (ts * 1000.0) as i64;
            self.entry.first_ms = Some(self.entry.first_ms.map_or(ms, |f| f.min(ms)));
            self.entry.last_ms = Some(self.entry.last_ms.map_or(ms, |l| l.max(ms)));
        }
        let p = crate::provenance::classify_turn(&turn.obj, bus, session);
        match p {
            crate::provenance::Provenance::Unknown
            | crate::provenance::Provenance::Harness(
                crate::provenance::HarnessKind::CommandInvocation,
            ) => self.entry.bindable.push(BindableTurn {
                ms: turn.ts_epoch.map(|ts| (ts * 1000.0) as i64),
                command: p
                    == crate::provenance::Provenance::Harness(
                        crate::provenance::HarnessKind::CommandInvocation,
                    ),
                prev_assistant_ms: self.last_assistant_ms,
                ts: turn
                    .obj
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .map(str::to_string),
            }),
            crate::provenance::Provenance::Operator => {
                self.entry.operator_direct.push(DirectTurn {
                    ms: turn.ts_epoch.map(|ts| (ts * 1000.0) as i64),
                    ts: turn
                        .obj
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    prev_assistant_ms: self.last_assistant_ms,
                });
            }
            other => {
                *self
                    .entry
                    .counters
                    .entry(other.label().to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    fn finish(self) {
        let act = self.fold.finish();
        // Token merge follows the source: a cumulative total replaces, a
        // per-pass sum adds. A tail with no token_count row reads zero, and
        // a zero total never replaces a stored one.
        if self.cumulative && act.tokens.input + act.tokens.output > 0 {
            self.entry.tokens = act.tokens;
        } else if !self.cumulative {
            self.entry.tokens.input += act.tokens.input;
            self.entry.tokens.output += act.tokens.output;
            self.entry.tokens.cache_read += act.tokens.cache_read;
            self.entry.tokens.cache_write += act.tokens.cache_write;
        }
        self.entry.lines_added += act.lines_added;
        self.entry.lines_removed += act.lines_removed;
        for (k, v) in act.tool_errors {
            *self.entry.tool_errors.entry(k.to_string()).or_insert(0) += v;
        }
        for (k, v) in act.extensions {
            *self.entry.languages.entry(k).or_insert(0) += v;
        }
        self.entry.aborted_turns += act.aborted_turns;
        self.entry.last_assistant_ms = self.last_assistant_ms;
        self.entry.recent_message_ids = self.recent.into_iter().collect();
    }
}

/// The response gap of one operator turn over stored ms, reproducing the
/// whole-transcript path exactly: the turn's epoch truncates to whole
/// seconds there, the assistant anchor stays f64, the window is the
/// inclusive 2.0 to 3600.0 f64 range, and the kept value truncates.
fn gap_s(ms: i64, prev_ms: Option<i64>) -> Option<u64> {
    let prev = prev_ms?;
    let gap = (ms.div_euclid(1000)) as f64 - prev as f64 / 1000.0;
    (2.0..=3600.0).contains(&gap).then(|| gap as u64)
}

/// The report row of a stored entry: the counters replay through the
/// witness bind each run, the fresh joins stay fresh, and this run's
/// consumed text serves the relay delivery check.
pub(crate) fn build_row(
    harness: &'static str,
    entry: &RollupEntry,
    file: &SessionFile,
    ctx: &mut crate::intel::FoldCtx,
    text: &str,
) -> crate::intel::SessionRow {
    let mut counters: BTreeMap<&'static str, u64> = crate::provenance::Provenance::all_labels()
        .into_iter()
        .map(|l| (l, 0u64))
        .collect();
    for label in crate::provenance::Provenance::all_labels() {
        if let Some(v) = entry.counters.get(label) {
            counters.insert(label, *v);
        }
    }
    let mut operator_turns: Vec<String> = Vec::new();
    let mut gaps: Vec<u64> = Vec::new();
    for d in &entry.operator_direct {
        *counters.entry("operator").or_insert(0) += 1;
        operator_turns.push(d.ts.clone());
        if let Some(ms) = d.ms {
            gaps.extend(gap_s(ms, d.prev_assistant_ms));
        }
    }
    for b in &entry.bindable {
        let bound =
            b.ms.and_then(|ms| ctx.witness.bind(&file.session_id, ms))
                .is_some();
        match (b.command, bound) {
            (true, _) => {
                *counters.entry("harness_command_invocation").or_insert(0) += 1;
            }
            (false, true) => {
                *counters.entry("operator").or_insert(0) += 1;
                if let Some(ts) = &b.ts {
                    operator_turns.push(ts.clone());
                }
                if let (Some(ms), Some(prev)) = (b.ms, b.prev_assistant_ms) {
                    gaps.extend(gap_s(ms, Some(prev)));
                }
            }
            (false, false) => {
                *counters.entry("unknown").or_insert(0) += 1;
            }
        }
    }
    // Relay facets: the delivered check reads the stored id set first,
    // then this run's consumed text; the body fallback scans the tail
    // only, so a row relayed long before its first intel sighting adds a
    // late match only from the tail.
    let mut relay: Vec<crate::intel::RelayFacet> = Vec::new();
    let mut addressed: Vec<&crate::provenance::BusRow> = ctx
        .bus
        .rows()
        .iter()
        .filter(|r| r.to_session.as_deref() == Some(file.session_id.as_str()))
        .collect();
    if !addressed.is_empty() {
        addressed.sort_by(|a, b| a.ts.cmp(&b.ts));
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for row in addressed {
            let delivered = (!row.id.is_empty() && entry.relay_ids.contains(&row.id))
                || text.contains(row.body.trim())
                || (!row.id.is_empty() && text.contains(row.id.as_str()));
            let row_ts = crate::intel::ts_secs(&row.ts);
            let mut answered = false;
            if row.from_session.is_some() {
                for other in ctx.bus.rows() {
                    let reply = other.from_session.as_deref() == row.to_session.as_deref()
                        && other.to_session.as_deref() == row.from_session.as_deref()
                        && crate::intel::ts_secs(&other.ts)
                            .is_some_and(|t2| row_ts.is_none_or(|t1| t2 > t1));
                    if reply {
                        answered = true;
                        break;
                    }
                }
            }
            let key = (
                row.to_session.clone().unwrap_or_default(),
                row.body.trim().to_string(),
            );
            let duplicate = seen.contains(&key);
            seen.insert(key);
            relay.push(crate::intel::RelayFacet {
                id: row.id.clone(),
                from_session: row.from_session.clone(),
                ts: row.ts.clone(),
                words: row.words,
                delivered,
                answered,
                within_contract: row.words <= WORD_CONTRACT,
                control: row
                    .body
                    .lines()
                    .any(|l| l.trim_start().starts_with("control:")),
                duplicate,
            });
        }
    }
    let (group, node, pr) = ctx
        .join
        .get(&file.session_id)
        .cloned()
        .unwrap_or_else(|| (Vec::new(), None, None));
    let interrupt_markers = counters
        .get("harness_interrupt_marker")
        .copied()
        .unwrap_or(0);
    let duration_s = entry
        .first_ms
        .zip(entry.last_ms)
        .map(|(f, l)| (l - f) / 1000);
    let started = entry
        .first_ms
        .and_then(|ms| chrono::DateTime::from_timestamp(ms.div_euclid(1000), 0))
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let operator_count = counters.get("operator").copied().unwrap_or(0);
    let unknown_count = counters.get("unknown").copied().unwrap_or(0);
    let relay_turns: u64 = counters
        .iter()
        .filter(|(k, _)| k.starts_with("relay_"))
        .map(|(_, v)| v)
        .sum();
    crate::intel::SessionRow {
        harness,
        session: file.session_id.clone(),
        path: file.path.display().to_string(),
        started,
        duration_s,
        kind: if operator_count > 0 || unknown_count > 0 || relay_turns > 0 {
            "attended".to_string()
        } else {
            "unattended".to_string()
        },
        counters,
        operator_turns,
        tool_use: entry.tool_use as usize,
        commits: ctx.commits_for(&group),
        mtime: file.mtime,
        size: file.size,
        node,
        pr_number: pr,
        relay,
        tokens: Some(entry.tokens),
        lines: Some(crate::intel::Lines {
            added: entry.lines_added,
            removed: entry.lines_removed,
        }),
        tool_errors: Some(entry.tool_errors.clone()),
        languages: Some(entry.languages.clone()),
        interruptions: interrupt_markers + entry.aborted_turns,
        response_s: gaps,
        substantive: crate::intel_insights::is_substantive(
            operator_count + unknown_count,
            duration_s,
        ),
        idle: ctx.now.saturating_sub(file.mtime) >= crate::intel_insights::IDLE_SECS,
        sampled: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator_witness::SubmitIndex;
    use crate::provenance::ClaudeSource;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-intel-rollup-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_lines(path: &Path, lines: &[String]) -> SessionFile {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut raw = String::new();
        for l in lines {
            raw.push_str(l);
            raw.push('\n');
        }
        std::fs::write(path, &raw).unwrap();
        let meta = std::fs::metadata(path).unwrap();
        SessionFile {
            session_id: "s1".to_string(),
            path: path.to_path_buf(),
            mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            size: meta.len(),
        }
    }

    fn user_turn(ts: &str, text: &str) -> String {
        json!({"type": "user", "uuid": "u1", "timestamp": ts,
               "message": {"role": "user", "content": text}})
        .to_string()
    }

    fn assistant(ts: &str, id: &str, out: u64) -> String {
        json!({"type": "assistant", "timestamp": ts,
               "message": {"id": id, "role": "assistant", "content": [],
                           "usage": {"output_tokens": out}}})
        .to_string()
    }

    fn source(dir: &Path) -> ClaudeSource {
        ClaudeSource {
            projects_dir: dir.to_path_buf(),
            roots: None,
        }
    }

    fn empty_ctx(witness: SubmitIndex) -> crate::intel::FoldCtx {
        crate::intel::FoldCtx {
            bus: BusIndex::empty(),
            join: std::collections::HashMap::new(),
            events: Vec::new(),
            witness,
            days: 30,
            now: 1_800_000_000,
            rollups: None,
        }
    }

    const T: &str = "2026-10-01T12:00";

    /// A transcript with one token-bearing assistant row, two unshaped
    /// turns, a second assistant row, an Edit, and a failing tool result.
    fn fixture_lines() -> Vec<String> {
        vec![
            assistant(&format!("{T}:00.000Z"), "msg-a", 100),
            user_turn(&format!("{T}:05.000Z"), "typed one"),
            assistant(&format!("{T}:10.000Z"), "msg-b", 50),
            user_turn(&format!("{T}:15.000Z"), "typed two"),
            json!({"type": "assistant", "timestamp": format!("{T}:20.000Z"),
                   "message": {"id": "msg-c", "role": "assistant",
                               "content": [{"type": "tool_use", "name": "Edit",
                                            "input": {"file_path": "a/b.rs",
                                                      "old_string": "one\ntwo",
                                                      "new_string": "one\nx\ny"}}]}})
            .to_string(),
            json!({"type": "user", "timestamp": format!("{T}:25.000Z"),
                   "message": {"role": "user",
                               "content": [{"type": "tool_result", "is_error": true,
                                            "content": "Exit code 1"}]}})
            .to_string(),
        ]
    }

    #[test]
    fn two_pass_fold_equals_one_whole_pass() {
        let dir = tmp_dir("twopass");
        let lines = fixture_lines();
        let joined = |ls: &[String]| ls.iter().map(|l| format!("{l}\n")).collect::<String>();
        let (half, rest) = (&lines[..3], &lines[3..]);
        let path = dir.join("s1.jsonl");
        std::fs::write(&path, joined(half)).unwrap();
        let src = source(&dir);
        let mut store = RollupStore::open(dir.join("rollups.json"));
        let meta = std::fs::metadata(&path).unwrap();
        let f1 = SessionFile {
            session_id: "s1".to_string(),
            path: path.clone(),
            mtime: 0,
            size: meta.len(),
        };
        store.advance(&f1, &src, &BusIndex::empty());
        store.save();
        std::fs::write(&path, joined(half) + &joined(rest)).unwrap();
        let meta2 = std::fs::metadata(&path).unwrap();
        let f2 = SessionFile {
            session_id: "s1".to_string(),
            path: path.clone(),
            mtime: 0,
            size: meta2.len(),
        };
        let bytes_after_second = {
            let (e, _) = store.advance(&f2, &src, &BusIndex::empty());
            store.save();
            let (e3, _) = store.advance(&f2, &src, &BusIndex::empty());
            assert_eq!(e.counters, e3.counters);
            e
        };
        // The whole-file pass in a fresh store over the same content.
        let dir2 = tmp_dir("whole");
        let whole = write_lines(&dir2.join("s1.jsonl"), &lines);
        let mut whole_store = RollupStore::open(dir2.join("rollups.json"));
        let (we, _) = whole_store.advance(&whole, &source(&dir2), &BusIndex::empty());
        assert_eq!(bytes_after_second.counters, we.counters);
        assert_eq!(bytes_after_second.tokens, we.tokens);
        assert_eq!(bytes_after_second.lines_added, we.lines_added);
        assert_eq!(bytes_after_second.lines_removed, we.lines_removed);
        assert_eq!(bytes_after_second.tool_errors, we.tool_errors);
        assert_eq!(bytes_after_second.languages, we.languages);
        assert_eq!(bytes_after_second.bindable.len(), we.bindable.len());
        assert_eq!(
            bytes_after_second.operator_direct.len(),
            we.operator_direct.len()
        );
        assert_eq!(bytes_after_second.first_ms, we.first_ms);
        assert_eq!(bytes_after_second.last_ms, we.last_ms);
    }

    #[test]
    fn a_rewritten_transcript_resets_and_an_appended_tail_counts_its_bytes() {
        let dir = tmp_dir("reset");
        let path = dir.join("s1.jsonl");
        let three = || {
            write_lines(
                &path,
                &[
                    user_turn(&format!("{T}:05.000Z"), "a"),
                    user_turn(&format!("{T}:06.000Z"), "b"),
                    user_turn(&format!("{T}:07.000Z"), "c"),
                ],
            )
        };
        let src = source(&dir);
        let mut store = RollupStore::open(dir.join("rollups.json"));
        let unknown = |e: &RollupEntry, file: &SessionFile| {
            let mut ctx = empty_ctx(SubmitIndex::empty());
            build_row("claude", e, file, &mut ctx, "")
                .counters
                .get("unknown")
                .copied()
        };
        let f1 = three();
        let (e1, _) = store.advance(&f1, &src, &BusIndex::empty());
        assert_eq!(unknown(&e1, &f1), Some(3));
        // A rewrite that shrinks the file and changes the head resets the
        // entry and recounts to the smaller content's numbers.
        let smaller = write_lines(&path, &[user_turn(&format!("{T}:05.000Z"), "v1")]);
        let (e2, _) = store.advance(&smaller, &src, &BusIndex::empty());
        assert_eq!(store.receipt.replaced, 1);
        assert_eq!(unknown(&e2, &smaller), Some(1));
        // An append keeps the head: the second advance reads exactly the
        // appended bytes and the counters carry over.
        let before = store.receipt.bytes_read;
        let grown = write_lines(
            &path,
            &[
                user_turn(&format!("{T}:05.000Z"), "v1"),
                assistant(&format!("{T}:09.000Z"), "m1", 7),
            ],
        );
        let appended = grown.size - smaller.size;
        let (e3, _) = store.advance(&grown, &src, &BusIndex::empty());
        assert_eq!(store.receipt.bytes_read - before, appended);
        assert_eq!(unknown(&e3, &grown), Some(1));
        assert!(e3.last_assistant_ms.is_some());
    }

    #[test]
    fn a_late_submit_flips_a_stored_unknown_and_one_submit_binds_one_turn() {
        let dir = tmp_dir("flip");
        let base = crate::intel::ts_secs("2026-10-01T12:00:00Z").unwrap() as i64 * 1000;
        let at = |ms: i64| -> String {
            chrono::DateTime::from_timestamp(ms.div_euclid(1000), 0)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let f = write_lines(
            &dir.join("s1.jsonl"),
            &[
                assistant(&at(base), "m0", 10),
                user_turn(&at(base + 5_000), "one"),
                user_turn(&at(base + 10_000), "two"),
                user_turn(&at(base + 15_000), "three"),
            ],
        );
        let src = source(&dir);
        let mut store = RollupStore::open(dir.join("rollups.json"));
        let (entry, _) = store.advance(&f, &src, &BusIndex::empty());
        store.save();
        // No journal: every unshaped turn reads unknown.
        let mut ctx = empty_ctx(SubmitIndex::empty());
        let row = build_row("claude", &entry, &f, &mut ctx, "");
        assert_eq!(row.counters.get("unknown"), Some(&3));
        assert_eq!(row.counters.get("operator"), Some(&0));

        // Two submits land later; the stored entry replays through the
        // fresh index without re-reading a byte: one submit binds exactly
        // one turn, the late rows flip operator, the middle turn stays
        // unknown, and the response gap rides the stored anchor.
        let journal = dir.join("witness").join("events.jsonl");
        write_journal(&journal, base + 4_000, base + 14_000);
        let mut store2 = RollupStore::open(dir.join("rollups.json"));
        let (entry2, tail) = store2.advance(&f, &src, &BusIndex::empty());
        assert_eq!(store2.receipt.bytes_read, 0);
        let mut ctx2 = empty_ctx(SubmitIndex::load(&journal));
        let row2 = build_row("claude", &entry2, &f, &mut ctx2, &tail);
        assert_eq!(row2.counters.get("operator"), Some(&2));
        assert_eq!(row2.counters.get("unknown"), Some(&1));
        assert_eq!(row2.response_s, vec![5, 15]);
        assert_eq!(row2.kind, "attended");
        assert_eq!(row2.operator_turns.len(), 2);
    }

    fn write_journal(path: &Path, ms_a: i64, ms_b: i64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let row = |ms: i64| {
            json!({"ts": "2026-10-01T12:00:00Z", "type": "operator_submit",
                   "source": "daemon",
                   "data": {"mux_session": "main", "pane": 7, "via": "pane",
                            "submit_ms": ms, "resolution": "ok",
                            "harness_session": "s1"}})
        };
        std::fs::write(path, format!("{}\n{}\n", row(ms_a), row(ms_b))).unwrap();
    }
}
