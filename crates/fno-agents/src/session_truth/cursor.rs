//! Per-file transcript cursors for the truth reader: one entry per
//! transcript path, so a daemon sweep parses only the bytes appended since
//! the last read instead of re-reading the tail window every sweep.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use super::TruthRecord;

/// The cold-read window, matching the Python reader's 4 MiB tail window
/// (`_TAIL_BYTES` in `cli/src/fno/agents/peek.py`), which holds the last 40
/// records of every live lead transcript.
const TAIL_WINDOW: u64 = 4 << 20;

/// One file's cursor. `len_read` is the offset of the end of the last
/// complete line, so a writer's trailing partial line is never consumed and
/// the next read picks it up whole once the writer finishes it.
struct CursorEntry {
    dev: u64,
    ino: u64,
    len_read: u64,
    ring: VecDeque<TruthRecord>,
    last_bytes_read: u64,
}

/// One cursor set per process. The daemon serves every sweep from one; a
/// CLI process starts empty by construction.
#[derive(Default)]
pub struct TruthCursors {
    entries: HashMap<PathBuf, CursorEntry>,
}

static CURSORS: OnceLock<Mutex<TruthCursors>> = OnceLock::new();

/// The process-global cursor set. The daemon answers every sweep through
/// this; a CLI process holds an empty one for its short life.
pub(crate) fn global_cursors() -> MutexGuard<'static, TruthCursors> {
    CURSORS
        .get_or_init(|| Mutex::new(TruthCursors::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    /// The transcript tail as at most `n` parsed records, newest LAST, plus
    /// the byte count this call parsed. The first read walks the trailing
    /// window; every later read parses only appended complete lines. A
    /// truncated or replaced (new inode) file resets its cursor, so the
    /// answer always equals a cold read. `None` when the file cannot be
    /// stat'd or opened.
    pub(crate) fn read_tail(
        &mut self,
        path: &Path,
        n: usize,
        parse_line: &dyn Fn(&str) -> Option<TruthRecord>,
    ) -> Option<(Vec<TruthRecord>, u64)> {
        let (dev, ino, len) = file_identity(path)?;
        let fresh = !self.same_file(path, dev, ino, len);
        if fresh {
            self.entries.remove(path);
            let (records, len_read, bytes) = cold_read(path, len, n, parse_line)?;
            self.entries.insert(
                path.to_path_buf(),
                CursorEntry {
                    dev,
                    ino,
                    len_read,
                    ring: records.iter().cloned().collect(),
                    last_bytes_read: bytes,
                },
            );
            return Some((records, bytes));
        }
        self.warm_read(path, len, n, parse_line)
    }

    fn same_file(&self, path: &Path, dev: u64, ino: u64, len: u64) -> bool {
        match self.entries.get(path) {
            Some(entry) => entry.dev == dev && entry.ino == ino && len >= entry.len_read,
            None => false,
        }
    }

    fn warm_read(
        &mut self,
        path: &Path,
        len: u64,
        n: usize,
        parse_line: &dyn Fn(&str) -> Option<TruthRecord>,
    ) -> Option<(Vec<TruthRecord>, u64)> {
        let entry = self.entries.get_mut(path)?;
        if len == entry.len_read {
            let mut out: Vec<TruthRecord> = entry.ring.clone().into();
            drain_to_n(&mut out, n);
            return Some((out, 0));
        }
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(entry.len_read)).ok()?;
        let mut appended = Vec::new();
        file.take(len - entry.len_read)
            .read_to_end(&mut appended)
            .ok()?;
        let consumed = match appended.iter().rposition(|b| *b == b'\n') {
            Some(pos) => pos + 1,
            None => 0,
        };
        let mut parsed = 0u64;
        for line in std::str::from_utf8(&appended[..consumed])
            .unwrap_or("")
            .lines()
        {
            parsed += line.len() as u64 + 1;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(record) = parse_line(line) {
                entry.ring.push_back(record);
                while entry.ring.len() > n {
                    entry.ring.pop_front();
                }
            }
        }
        entry.len_read += consumed as u64;
        entry.last_bytes_read = parsed;
        let mut out: Vec<TruthRecord> = entry.ring.clone().into();
        drain_to_n(&mut out, n);
        Some((out, parsed))
    }
}

/// Keep the newest `n` records of `out`.
fn drain_to_n(out: &mut Vec<TruthRecord>, n: usize) {
    if out.len() > n {
        let drop = out.len() - n;
        out.drain(..drop);
    }
}

/// A cold read: the trailing window, partial head line dropped, complete
/// lines parsed. Returns the records (newest last), the consumed offset, and
/// the parsed byte count.
fn cold_read(
    path: &Path,
    len: u64,
    n: usize,
    parse_line: &dyn Fn(&str) -> Option<TruthRecord>,
) -> Option<(Vec<TruthRecord>, u64, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let start = len.saturating_sub(TAIL_WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes).ok()?;
    let head = if start > 0 {
        // A mid-file seek lands inside a line; drop through the next newline.
        match bytes.iter().position(|b| *b == b'\n') {
            Some(pos) => pos + 1,
            None => bytes.len(),
        }
    } else {
        0
    };
    // A trailing partial line holds back: parse only through the last newline.
    let end = match bytes[head..].iter().rposition(|b| *b == b'\n') {
        Some(pos) => head + pos + 1,
        None => head,
    };
    let mut records: Vec<TruthRecord> = Vec::new();
    let mut parsed = 0u64;
    for line in std::str::from_utf8(&bytes[head..end]).unwrap_or("").lines() {
        parsed += line.len() as u64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(record) = parse_line(line) {
            records.push(record);
        }
    }
    drain_to_n(&mut records, n);
    Some((records, start + end as u64, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// AC2: a transcript that grows by one record between reads parses only
    /// the appended bytes on the second read, and answers from the ring.
    #[test]
    fn a_growing_transcript_reparses_only_the_appended_bytes() {
        let dir = std::env::temp_dir().join(format!("truth-cursor-grow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let first = concat!(
            r#"{"type":"assistant","timestamp":"2026-10-08T10:00:00Z","message":{"role":"assistant","content":"one"}}"#,
            "\n",
        );
        std::fs::write(&path, first).unwrap();
        let parse = |line: &str| {
            let v: Value = serde_json::from_str(line).ok()?;
            Some(TruthRecord {
                role: v.get("type")?.as_str()?.to_string(),
                text: "x".into(),
                timestamp: v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .map(String::from),
            })
        };
        let mut cursors = TruthCursors::new();
        let (records, bytes) = cursors.read_tail(&path, 40, &parse).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(bytes, first.len() as u64);
        let second = r#"{"type":"user","timestamp":"2026-10-08T10:01:00Z","message":{"role":"user","content":"two"}}"#;
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{}\n", second).as_bytes())
            .unwrap();
        let (records, bytes) = cursors.read_tail(&path, 41, &parse).unwrap();
        assert_eq!(records.len(), 2, "the ring serves the whole tail");
        assert_eq!(bytes, second.len() as u64 + 1, "only appended bytes parsed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC3: truncation or an inode swap resets the cursor; the answer equals
    /// a cold read.
    #[test]
    fn a_truncated_or_replaced_transcript_reads_cold() {
        let dir = std::env::temp_dir().join(format!("truth-cursor-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, "line-one\nline-two\n").unwrap();
        let parse = |line: &str| {
            Some(TruthRecord {
                role: line.into(),
                text: String::new(),
                timestamp: None,
            })
        };
        let mut cursors = TruthCursors::new();
        let (records, _) = cursors.read_tail(&path, 40, &parse).unwrap();
        assert_eq!(records.len(), 2);
        std::fs::write(&path, "only\n").unwrap();
        let (records, _) = cursors.read_tail(&path, 40, &parse).unwrap();
        assert_eq!(records.len(), 1, "truncation resets");
        let replaced = dir.join("t2.jsonl");
        std::fs::rename(&path, &replaced).unwrap();
        std::fs::write(&path, "new-1\nnew-2\nnew-3\n").unwrap();
        let (records, _) = cursors.read_tail(&path, 40, &parse).unwrap();
        assert_eq!(records.len(), 3, "a new inode resets");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
