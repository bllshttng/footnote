//! Per-file transcript cursors for the truth reader. An entry holds a byte
//! offset and a small derived [`TailSummary`], never transcript text, so a
//! daemon sweep parses only the bytes appended since the last read and the
//! daemon's memory stays bounded by the roster, not by transcript size.
//!
//! A rebuild (first sight of a file, a truncation, or a new inode) reads back
//! from EOF and stops at the newest compact boundary or at [`REBUILD_CAP`],
//! whichever comes first. It never replays a transcript from the start.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::TailSummary;

/// The rebuild window. Nothing before the newest compact boundary matters for
/// truth, and the window stops there when it meets one first.
pub(crate) const REBUILD_CAP: u64 = 64 * 1024;

/// One transcript line can outgrow the cap (a 3 MB tool result is ordinary),
/// so a window that holds no complete turn doubles until it holds one, up to
/// the Python reader's 4 MiB tail window. A window that meets a compact
/// boundary or the file start stops at once.
const REBUILD_MAX: u64 = 4 << 20;

/// An entry nobody asked about for this long belongs to a session that left
/// the roster; the next read past it drops the entry.
const IDLE_TTL: Duration = Duration::from_secs(10 * 60);
const PRUNE_EVERY: Duration = Duration::from_secs(60);

/// One beat: how long a handle that resolved to nothing answers `not-found`
/// from memory before the resolver tries it again.
pub(crate) const MISS_BEAT: Duration = Duration::from_secs(60);

/// How one harness's lines fold into a summary.
pub(crate) struct Folder<'a> {
    pub fold: &'a dyn Fn(&mut TailSummary, &str),
    /// True for the harness's compact-boundary line. A rebuild reads only
    /// the lines after the newest one.
    pub is_boundary: &'a dyn Fn(&str) -> bool,
    /// Runs once after a rebuild, with the window's start offset, to find
    /// provenance (model, title) the window did not carry.
    pub backfill: &'a dyn Fn(&mut TailSummary, &Path, u64),
}

struct CursorEntry {
    dev: u64,
    ino: u64,
    len_read: u64,
    summary: TailSummary,
    touched: Instant,
}

struct StoreEntry {
    version: Option<i64>,
    summary: TailSummary,
    touched: Instant,
}

/// One cursor set per process. The daemon serves every sweep from one; a
/// CLI process starts empty by construction.
#[derive(Default)]
pub struct TruthCursors {
    entries: HashMap<PathBuf, CursorEntry>,
    stores: HashMap<String, StoreEntry>,
    // Resolved transcript paths by `agent:session`, so a sweep does not list
    // the whole projects store or the codex tree once per row.
    paths: HashMap<String, (PathBuf, Instant)>,
    // Handles that resolved to nothing, so an unresolvable session costs one
    // bounded attempt per beat however often a caller asks.
    misses: HashMap<String, Instant>,
    last_bytes_read: u64,
    last_prune: Option<Instant>,
}

static CURSORS: OnceLock<Mutex<TruthCursors>> = OnceLock::new();

/// The process-global cursor set the daemon serves every sweep from.
pub(crate) fn global() -> &'static Mutex<TruthCursors> {
    CURSORS.get_or_init(|| Mutex::new(TruthCursors::default()))
}

fn file_identity(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino(), meta.len()))
}

impl TruthCursors {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes the last [`Self::summary`] call parsed: the cold window on a
    /// rebuild, only the appended lines after that.
    pub fn last_bytes_read(&self) -> u64 {
        self.last_bytes_read
    }

    /// Live entries, files and stores together.
    pub fn len(&self) -> usize {
        self.entries.len() + self.stores.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn has(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    pub(crate) fn has_store(&self, key: &str) -> bool {
        self.stores.contains_key(key)
    }

    /// A cached transcript path for `key`, when the file still exists.
    pub(crate) fn cached_path(&mut self, key: &str) -> Option<PathBuf> {
        let (path, touched) = self.paths.get_mut(key)?;
        if !path.exists() {
            self.paths.remove(key);
            return None;
        }
        *touched = Instant::now();
        Some(path.clone())
    }

    pub(crate) fn cache_path(&mut self, key: &str, path: &Path) {
        self.paths
            .insert(key.to_string(), (path.to_path_buf(), Instant::now()));
    }

    /// True when `handle` resolved to nothing within the last beat.
    pub(crate) fn recent_miss(&mut self, handle: &str) -> bool {
        match self.misses.get(handle) {
            Some(at) if at.elapsed() < MISS_BEAT => true,
            Some(_) => {
                self.misses.remove(handle);
                false
            }
            None => false,
        }
    }

    pub(crate) fn note_miss(&mut self, handle: &str) {
        self.misses.insert(handle.to_string(), Instant::now());
    }

    #[cfg(test)]
    pub(crate) fn expire_misses_for_test(&mut self) {
        for at in self.misses.values_mut() {
            *at -= MISS_BEAT;
        }
    }

    /// Drop a file's cursor. Called when the session behind it ended.
    pub(crate) fn forget(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    pub(crate) fn forget_store(&mut self, key: &str) {
        self.stores.remove(key);
    }

    /// The file's summary, advanced through every complete line on disk. A
    /// truncated or replaced (new inode) file rebuilds, so the answer always
    /// equals a fresh rebuild. `None` when the file cannot be stat'd or read.
    pub(crate) fn summary(&mut self, path: &Path, folder: &Folder) -> Option<TailSummary> {
        self.prune_idle();
        let (dev, ino, len) = file_identity(path)?;
        let same = self
            .entries
            .get(path)
            .is_some_and(|e| e.dev == dev && e.ino == ino && len >= e.len_read);
        if !same {
            self.entries.remove(path);
            let (summary, len_read, bytes) = rebuild(path, len, folder)?;
            self.last_bytes_read = bytes;
            self.entries.insert(
                path.to_path_buf(),
                CursorEntry {
                    dev,
                    ino,
                    len_read,
                    summary: summary.clone(),
                    touched: Instant::now(),
                },
            );
            return Some(summary);
        }
        let entry = self.entries.get_mut(path)?;
        entry.touched = Instant::now();
        let bytes = advance(entry, path, len, folder)?;
        self.last_bytes_read = bytes;
        Some(entry.summary.clone())
    }

    /// A summary for a store-backed session (opencode), rebuilt only when
    /// the store's `version` for it moved.
    pub(crate) fn store_summary(
        &mut self,
        key: &str,
        version: Option<i64>,
        build: impl FnOnce() -> TailSummary,
    ) -> TailSummary {
        self.prune_idle();
        if let Some(entry) = self.stores.get_mut(key) {
            if entry.version == version && version.is_some() {
                entry.touched = Instant::now();
                return entry.summary.clone();
            }
        }
        let summary = build();
        self.stores.insert(
            key.to_string(),
            StoreEntry {
                version,
                summary: summary.clone(),
                touched: Instant::now(),
            },
        );
        summary
    }

    fn prune_idle(&mut self) {
        let now = Instant::now();
        if self
            .last_prune
            .is_some_and(|at| now.duration_since(at) < PRUNE_EVERY)
        {
            return;
        }
        self.last_prune = Some(now);
        self.entries
            .retain(|_, e| now.duration_since(e.touched) < IDLE_TTL);
        self.stores
            .retain(|_, e| now.duration_since(e.touched) < IDLE_TTL);
        self.paths
            .retain(|_, (_, touched)| now.duration_since(*touched) < IDLE_TTL);
        self.misses
            .retain(|_, at| now.duration_since(*at) < MISS_BEAT);
    }

    #[cfg(test)]
    pub(crate) fn age_all_for_test(&mut self, by: Duration) {
        for e in self.entries.values_mut() {
            e.touched -= by;
        }
        for e in self.stores.values_mut() {
            e.touched -= by;
        }
        self.last_prune = None;
    }
}

/// Fold the complete lines appended since the last read. A trailing partial
/// line stays unread until its writer finishes it.
fn advance(entry: &mut CursorEntry, path: &Path, len: u64, folder: &Folder) -> Option<u64> {
    if len == entry.len_read {
        return Some(0);
    }
    let appended = read_range(path, entry.len_read, len)?;
    let consumed = match appended.iter().rposition(|b| *b == b'\n') {
        Some(pos) => pos + 1,
        None => 0,
    };
    for line in String::from_utf8_lossy(&appended[..consumed]).lines() {
        if !line.trim().is_empty() {
            (folder.fold)(&mut entry.summary, line);
        }
    }
    entry.len_read += consumed as u64;
    Some(consumed as u64)
}

/// The bounded rebuild. Returns the summary, the offset the cursor resumes
/// at (the end of the last complete line), and the bytes folded (the lines
/// after the boundary).
fn rebuild(path: &Path, len: u64, folder: &Folder) -> Option<(TailSummary, u64, u64)> {
    let mut window = REBUILD_CAP;
    loop {
        let start = len.saturating_sub(window);
        let bytes = read_range(path, start, len)?;
        // A mid-file start lands inside a line; drop through the next newline.
        let head = if start > 0 {
            bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |pos| pos + 1)
        } else {
            0
        };
        let end = bytes[head..]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(head, |pos| head + pos + 1);
        let text = String::from_utf8_lossy(&bytes[head..end]);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let boundary = lines.iter().rposition(|l| (folder.is_boundary)(l));
        let mut summary = TailSummary::default();
        let mut folded = 0u64;
        for line in &lines[boundary.map_or(0, |i| i + 1)..] {
            folded += line.len() as u64 + 1;
            (folder.fold)(&mut summary, line);
        }
        let done =
            boundary.is_some() || start == 0 || summary.has_record() || window >= REBUILD_MAX;
        if done {
            (folder.backfill)(&mut summary, path, start + head as u64);
            return Some((summary, start + end as u64, folded));
        }
        window = (window * 2).min(REBUILD_MAX);
    }
}

/// The newest complete line before `before` that contains `marker` and that
/// `accept` maps to a value, read backward in chunks and never past `limit`
/// bytes. Only a matching line is decoded; nothing else is parsed.
pub(crate) fn scan_back<T>(
    path: &Path,
    before: u64,
    marker: &[u8],
    limit: u64,
    accept: &dyn Fn(&str) -> Option<T>,
) -> Option<T> {
    const CHUNK: u64 = 256 * 1024;
    let finder =
        regex::bytes::Regex::new(&regex::escape(std::str::from_utf8(marker).ok()?)).ok()?;
    let floor = before.saturating_sub(limit);
    let mut end = before;
    // Bytes of a line that straddles the chunk below, carried down.
    let mut carry: Vec<u8> = Vec::new();
    while end > floor {
        let start = end.saturating_sub(CHUNK).max(floor);
        let mut chunk = read_range(path, start, end)?;
        chunk.extend_from_slice(&carry);
        // The bytes before the first newline may be a partial line, unless
        // the chunk starts the file. A chunk with no newline at all is the
        // middle of one long line: carry it whole.
        let carry_end = if start == 0 {
            0
        } else {
            chunk
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(chunk.len())
        };
        let body_from = if start == 0 {
            0
        } else {
            (carry_end + 1).min(chunk.len())
        };
        for line in chunk[body_from..].split(|b| *b == b'\n').rev() {
            if finder.is_match(line) {
                if let Some(found) = accept(&String::from_utf8_lossy(line)) {
                    return Some(found);
                }
            }
        }
        chunk.truncate(carry_end);
        carry = chunk;
        if carry.len() as u64 > REBUILD_MAX {
            return None;
        }
        end = start;
    }
    None
}

fn read_range(path: &Path, start: u64, end: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut out = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("truth-cursor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(kind: &str, ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"{kind}","timestamp":"{ts}","message":{{"role":"{kind}","content":"{text}"}}}}"#
        ) + "\n"
    }

    fn summary(cursors: &mut TruthCursors, path: &Path) -> TailSummary {
        cursors
            .summary(path, &super::super::folder_for("claude"))
            .unwrap()
    }

    /// AC2: a transcript that grows by one record between reads parses only
    /// the appended bytes on the second read and answers the new state.
    #[test]
    fn a_growing_transcript_parses_only_the_appended_bytes() {
        let dir = dir("grow");
        let path = dir.join("t.jsonl");
        let first = line("assistant", "2026-10-08T10:00:00Z", "reading");
        std::fs::write(&path, &first).unwrap();
        let mut cursors = TruthCursors::new();
        assert_eq!(summary(&mut cursors, &path).last_role(), Some("assistant"));
        assert_eq!(cursors.last_bytes_read(), first.len() as u64);
        let second = line("user", "2026-10-08T10:01:00Z", "next");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(second.as_bytes())
            .unwrap();
        assert_eq!(summary(&mut cursors, &path).last_role(), Some("user"));
        assert_eq!(cursors.last_bytes_read(), second.len() as u64);
        assert_eq!(summary(&mut cursors, &path).last_role(), Some("user"));
        assert_eq!(
            cursors.last_bytes_read(),
            0,
            "an unchanged file reads nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC3: truncation or an inode swap resets the cursor, and the answer
    /// equals a fresh rebuild.
    #[test]
    fn a_truncated_or_replaced_transcript_rebuilds() {
        let dir = dir("reset");
        let path = dir.join("t.jsonl");
        let a = line("assistant", "2026-10-08T10:00:00Z", "one");
        let b = line("user", "2026-10-08T10:01:00Z", "two");
        std::fs::write(&path, format!("{a}{b}")).unwrap();
        let mut cursors = TruthCursors::new();
        assert_eq!(summary(&mut cursors, &path).records(), 2);
        std::fs::write(&path, &a).unwrap();
        assert_eq!(
            summary(&mut cursors, &path).records(),
            1,
            "truncation resets"
        );
        std::fs::rename(&path, dir.join("old.jsonl")).unwrap();
        std::fs::write(&path, format!("{b}{b}{b}")).unwrap();
        assert_eq!(
            summary(&mut cursors, &path).records(),
            3,
            "a new inode resets"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The memory bound: a summary carries no transcript text past the
    /// 200-char last message, however long the turns are.
    #[test]
    fn a_summary_holds_no_transcript_text() {
        let dir = dir("bound");
        let path = dir.join("t.jsonl");
        let long = "x".repeat(50_000);
        std::fs::write(&path, line("assistant", "2026-10-08T10:00:00Z", &long)).unwrap();
        let mut cursors = TruthCursors::new();
        let s = summary(&mut cursors, &path);
        assert!(
            format!("{s:?}").len() < 1_000,
            "{} bytes",
            format!("{s:?}").len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rebuild stops at the cap: a long history ahead of the window is
    /// never read.
    #[test]
    fn a_rebuild_reads_no_more_than_the_cap() {
        let dir = dir("cap");
        let path = dir.join("t.jsonl");
        let mut body = String::new();
        while body.len() < 1 << 20 {
            body.push_str(&line("assistant", "2026-10-08T10:00:00Z", "old turn"));
        }
        body.push_str(&line(
            "assistant",
            "2026-10-08T11:00:00Z",
            "<promise>DONE</promise>",
        ));
        std::fs::write(&path, &body).unwrap();
        let mut cursors = TruthCursors::new();
        let s = summary(&mut cursors, &path);
        assert!(cursors.last_bytes_read() <= REBUILD_CAP);
        assert_eq!(s.last_role(), Some("assistant"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One turn longer than the cap still rebuilds to that turn: the window
    /// doubles until it holds a complete one.
    #[test]
    fn a_turn_longer_than_the_cap_still_rebuilds() {
        let dir = dir("long-line");
        let path = dir.join("t.jsonl");
        let big = "y".repeat(3 * REBUILD_CAP as usize);
        let body = line("user", "2026-10-08T10:00:00Z", "go")
            + &line("assistant", "2026-10-08T10:00:01Z", &big);
        std::fs::write(&path, &body).unwrap();
        let mut cursors = TruthCursors::new();
        assert_eq!(summary(&mut cursors, &path).last_role(), Some("assistant"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Eviction: an entry idle past the TTL drops on the next read, and
    /// `forget` drops one at once.
    #[test]
    fn idle_and_forgotten_cursors_drop() {
        let dir = dir("evict");
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        std::fs::write(&a, line("user", "2026-10-08T10:00:00Z", "a")).unwrap();
        std::fs::write(&b, line("user", "2026-10-08T10:00:00Z", "b")).unwrap();
        let mut cursors = TruthCursors::new();
        summary(&mut cursors, &a);
        summary(&mut cursors, &b);
        assert_eq!(cursors.len(), 2);
        cursors.forget(&a);
        assert_eq!(cursors.len(), 1);
        cursors.age_all_for_test(IDLE_TTL + Duration::from_secs(1));
        summary(&mut cursors, &a);
        assert!(
            cursors.has(&a) && !cursors.has(&b),
            "the idle entry dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_back_finds_the_newest_marker_line_across_chunks() {
        let dir = dir("scan");
        let path = dir.join("t.jsonl");
        let mut body = String::from("{\"k\":\"marker one\"}\n");
        body.push_str(&"z".repeat(600 * 1024));
        body.push_str("\n{\"k\":\"marker two\"}\n");
        body.push_str(&"q".repeat(300 * 1024));
        body.push('\n');
        std::fs::write(&path, &body).unwrap();
        let len = body.len() as u64;
        let any = |l: &str| Some(l.to_string());
        let hit = scan_back(&path, len, b"marker", 4 << 20, &any).unwrap();
        assert!(hit.contains("marker two"), "{hit}");
        let older = |l: &str| l.contains("one").then(|| l.to_string());
        let hit = scan_back(&path, len, b"marker", 4 << 20, &older).unwrap();
        assert!(hit.contains("marker one"), "a refused line scans on: {hit}");
        assert!(
            scan_back(&path, len, b"marker", 1024, &any).is_none(),
            "the limit binds"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
