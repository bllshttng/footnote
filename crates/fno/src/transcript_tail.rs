//! The transcript tail reader behind the sideline's last-msg column.
//!
//! One question: what did this session last SAY? A registry row carries only
//! `last_message_at`; the text lives in the harness's transcript file, and
//! every harness lays those down differently. The reader answers in layers,
//! each harness-blind:
//!
//! 1. A row that names its own transcript (`log_path`) is read directly.
//! 2. Everything else goes through ONE shape-tolerant filename scan over the
//!    known transcript roots (claude projects dirs, isolated accounts, the
//!    codex sessions tree), matching a `.jsonl` stem that IS or ENDS in a
//!    wanted session uuid at up to three directory levels.
//!
//! Pure over the parsed text, batch-wide single scan, blocking I/O: callers
//! run it off the UI loop and degrade per-uuid, never fatally.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Read the last `budget` bytes of `path` as a lossy UTF-8 string, dropping the
/// partial first line. Missing/unreadable -> None (caller degrades).
pub(crate) fn read_tail(path: &Path, budget: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(budget);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(budget).read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(nl) = text.find('\n') {
            text = text[nl + 1..].to_string();
        }
    }
    Some(text)
}

/// Env override for the claude transcript base dir, mirroring fno-agents'
/// `claude_drive::PROJECTS_DIR_ENV` so one variable redirects both crates.
pub const CLAUDE_PROJECTS_DIR_ENV: &str = "FNO_CLAUDE_PROJECTS_DIR";

/// A transcript's newest turns are its last few KB; the extended table wants one
/// line, so this is deliberately far smaller than the events budget.
const TRANSCRIPT_TAIL_BYTES: u64 = 64 * 1024;

/// Cap on a composed tail. The table truncates to its column anyway; this keeps
/// a pathological single-line turn from riding the wire at full length.
const TAIL_MAX_CHARS: usize = 160;

#[cfg(test)]
thread_local! {
    static TEST_PROJECTS_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point this thread's transcript lookup at a scratch dir (test-only). A
/// thread-local, like the squad/view stores: this module's suite runs in
/// parallel, so a process-global env var would race across tests.
#[cfg(test)]
fn set_test_projects_dir(dir: Option<&Path>) {
    TEST_PROJECTS_DIR.with(|c| *c.borrow_mut() = dir.map(Path::to_path_buf));
}

/// The claude projects (transcript) base dir. Mirrors fno-agents'
/// `claude_drive::claude_projects_dir` - mirrored, not imported: the crates
/// share no types and the FILE is the contract (same rule as `roster_path`).
fn claude_projects_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = TEST_PROJECTS_DIR.with(|c| c.borrow().clone()) {
        return p;
    }
    if let Some(v) = std::env::var_os(CLAUDE_PROJECTS_DIR_ENV) {
        return PathBuf::from(v);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

#[cfg(test)]
thread_local! {
    static TEST_SESSIONS_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point this thread's sessions-tree lookup at a scratch dir (test-only),
/// mirroring `set_test_projects_dir`.
#[cfg(test)]
fn set_test_sessions_dir(dir: Option<&Path>) {
    TEST_SESSIONS_DIR.with(|c| *c.borrow_mut() = dir.map(Path::to_path_buf));
}

/// The codex sessions tree (`CODEX_HOME`, default `~/.codex`), whose
/// year/month/day layout holds rollout transcripts. Mirrored, not imported:
/// same rule as `claude_projects_dir` above.
fn codex_sessions_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = TEST_SESSIONS_DIR.with(|c| c.borrow().clone()) {
        return p;
    }
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".codex")
        })
        .join("sessions")
}

/// A uuid safe to use as a path component. Registry content is untrusted and
/// lands in a path join, so anything but the transcript filename shape is
/// refused before it can escape the projects dir.
fn transcript_uuid_shaped(uuid: &str) -> bool {
    !uuid.is_empty() && uuid.len() <= 64 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Locate the transcripts for `uuids` in ONE pass over the transcript roots.
///
/// The cwd-derived directory name is not a usable key: a worktree session's
/// registry `cwd` and the dir claude encoded from its own launch cwd disagree
/// for most live rows, so deriving the path would silently blank the column. The
/// filename carries the uuid, so scan for it - but scan ONCE for the whole batch. A
/// per-uuid scan re-walks every project dir (hundreds of them) per row, which
/// turns a fixed ~60ms tick cost into a per-row one.
///
/// The match is filename-shape tolerant up to `TRANSCRIPT_SCAN_DEPTH` directory
/// levels: claude lands exactly at `{uuid}.jsonl` one level down, while a codex
/// rollout sits three levels down as `rollout-<timestamp>-{uuid}.jsonl`. Both
/// are matched without naming either harness: a file whose `.jsonl` stem IS a
/// wanted uuid, or whose stem ENDS in one, matches. The uuid's fixed 36-char
/// width makes the suffix window exact, so a longer hex tail cannot read as a
/// uuid match.
fn find_transcripts(uuids: &[&str]) -> HashMap<String, PathBuf> {
    let wanted: HashSet<&str> = uuids
        .iter()
        .filter(|u| transcript_uuid_shaped(u))
        .copied()
        .collect();
    let mut out = HashMap::new();
    if wanted.is_empty() {
        return out;
    }
    for root in transcript_roots() {
        scan_transcript_dir(&root, 0, &wanted, &mut out);
        if out.len() == wanted.len() {
            break; // every wanted transcript found; skip the rest
        }
    }
    out
}

/// The uuid suffix window: standard claude/codex session ids are 36 chars
/// (8-4-4-4-12). Any future harness rolling its transcripts under a prefix
/// with the same uuid shape matches; one with a different id shape is
/// expected to name its own `log_path` instead (the direct read above).
const UUID_LEN: usize = 36;

/// Deepest directory level (below a root) a transcript may sit at: claude's
/// `projects/<proj>/` is one, codex's `sessions/<year>/<month>/<day>/` is
/// three. Both covered without naming either harness.
const TRANSCRIPT_SCAN_DEPTH: usize = 3;

fn scan_transcript_dir(
    dir: &Path,
    depth: usize,
    wanted: &HashSet<&str>,
    out: &mut HashMap<String, PathBuf>,
) {
    if depth > TRANSCRIPT_SCAN_DEPTH || out.len() == wanted.len() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // an absent/unreadable dir degrades to no tails, never fatal
    };
    for e in entries.flatten() {
        match e.file_type() {
            Ok(ft) if ft.is_dir() => {
                scan_transcript_dir(&e.path(), depth + 1, wanted, out);
            }
            Ok(_) => {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                let Some(stem) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                let uuid = if wanted.contains(stem) {
                    stem
                } else if stem.len() > UUID_LEN {
                    &stem[stem.len() - UUID_LEN..]
                } else {
                    continue;
                };
                if wanted.contains(uuid) {
                    out.insert(uuid.to_string(), e.path());
                }
            }
            Err(_) => continue,
        }
        if out.len() == wanted.len() {
            return; // every wanted transcript found; skip the rest
        }
    }
}

/// Every directory that can hold a session transcript.
///
/// The ambient root first, then each ISOLATED account's `<config_dir>/projects`.
/// An isolated account relocates its whole config dir (`CLAUDE_CONFIG_DIR`), so
/// a worker launched under one writes its transcript there and not under the
/// server's own `~/.claude` - searching only the ambient root left the tail
/// column permanently empty for every such worker. Same account set the roster
/// union already reads, so the two agree on which accounts exist.
///
/// The codex sessions tree rides behind them: a codex rollout is findable by
/// uuid there and nowhere else. An absent tree degrades to no scan (the reader
/// below checks readability per root).
///
/// A test override replaces the whole list: a scratch dir is the only root.
fn transcript_roots() -> Vec<PathBuf> {
    #[cfg(test)]
    {
        if let Some(p) = TEST_PROJECTS_DIR.with(|c| c.borrow().clone()) {
            return vec![p];
        }
        if let Some(p) = TEST_SESSIONS_DIR.with(|c| c.borrow().clone()) {
            return vec![p];
        }
    }
    let mut roots = vec![claude_projects_dir()];
    roots.extend(
        crate::agents_view::isolated_account_dirs()
            .into_iter()
            .map(|(_, dir)| dir.join("projects")),
    );
    roots.push(codex_sessions_dir());
    roots
}

/// The most recent assistant text in a transcript tail, as one display line.
///
/// Pure over the tail text so the scan is testable without a transcript. Walks
/// backward to the newest `assistant` turn carrying a `text` block and takes its
/// first non-empty line: a turn whose content is all tool-use has no prose to
/// show, so it is skipped rather than rendered as an empty cell that reads like
/// "no activity". Returns `None` when nothing qualifies - the caller renders an
/// empty cell, never a placeholder.
pub(crate) fn compose_tail(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // a torn or alien line is skipped, never fatal
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let blocks = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array());
        let prose = blocks.into_iter().flatten().filter_map(|b| {
            (b.get("type").and_then(|v| v.as_str()) == Some("text"))
                .then(|| b.get("text").and_then(|v| v.as_str()))
                .flatten()
        });
        for chunk in prose {
            let Some(first) = chunk.lines().map(str::trim).find(|l| !l.is_empty()) else {
                continue;
            };
            // A transcript is UNTRUSTED text that the client paints into the
            // terminal a character at a time, so an ESC sequence in an assistant
            // message would be replayed as terminal control rather than shown;
            // a tab or CR merely wrecks the table's column alignment. Strip
            // before the wire, matching `sanitize_name` / `sanitize_mail_text`
            // on the server side. The cap applies AFTER stripping, so control
            // padding cannot smuggle extra visible width past it.
            let clean: String = first
                .chars()
                .filter(|c| !c.is_control())
                .take(TAIL_MAX_CHARS)
                .collect();
            let clean = clean.trim();
            if !clean.is_empty() {
                return Some(clean.to_string());
            }
        }
    }
    None
}

/// The most recent assistant line per session, for the extended table's
/// tail column: `{session_uuid: tail}`.
///
/// Each key is `(uuid, log_path)`: a row that names its own transcript is read
/// DIRECTLY (a rollout lives outside every claude projects dir, and a third
/// harness will name a third layout), and only the rest go to the one
/// filename-shape glob. A row whose log_path read yields no prose still joins
/// the glob, so a missing or tee-shaped file degrades instead of blanking.
///
/// A uuid with no transcript, or a transcript with no assistant prose, is simply
/// ABSENT from the map - the row then renders an empty cell (data honesty: never
/// a fabricated or placeholder value). Every failure degrades per-uuid and
/// recovers on the next tick (AC6-FR); nothing here can fail the batch.
///
/// Blocking I/O. Callers run it off the UI loop, gated on the row set moving.
pub fn session_tails(keys: &[(String, Option<String>)]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut fallback: Vec<&str> = Vec::new();
    for (uuid, log_path) in keys {
        let direct = log_path
            .as_deref()
            .and_then(|p| compose_tail(&read_tail(Path::new(p), TRANSCRIPT_TAIL_BYTES)?));
        match direct {
            Some(tail) => {
                out.insert(uuid.clone(), tail);
            }
            None => fallback.push(uuid.as_str()),
        }
    }
    for (uuid, path) in find_transcripts(&fallback) {
        let Some(text) = read_tail(&path, TRANSCRIPT_TAIL_BYTES) else {
            continue;
        };
        if let Some(tail) = compose_tail(&text) {
            out.insert(uuid, tail);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One transcript line for an assistant turn carrying `blocks`.
    fn turn(blocks: &str) -> String {
        format!(r#"{{"type":"assistant","message":{{"content":[{blocks}]}}}}"#)
    }

    #[test]
    fn compose_tail_takes_the_newest_assistant_prose_line() {
        let raw = [
            turn(r#"{"type":"text","text":"older turn"}"#),
            turn(r#"{"type":"text","text":"newest turn\nsecond line"}"#),
        ]
        .join("\n");
        // Newest wins, and only its FIRST line rides the wire (a table cell is
        // one line; wrapping is not an option in the sideline).
        assert_eq!(compose_tail(&raw).as_deref(), Some("newest turn"));
    }

    #[test]
    fn compose_tail_caps_visible_chars_after_stripping() {
        // The cap must apply to what RENDERS, so a line padded with control
        // characters cannot smuggle extra visible width past it.
        // `\u0007` here is a JSON escape in the fixture text, so the document
        // stays valid JSON and the control char appears after parsing.
        let noisy = format!("{}{}", "\\u0007".repeat(50), "y".repeat(TAIL_MAX_CHARS * 2));
        let got = compose_tail(&turn(&format!(r#"{{"type":"text","text":"{noisy}"}}"#))).unwrap();
        assert_eq!(got.chars().count(), TAIL_MAX_CHARS);
        assert!(got.chars().all(|c| c == 'y'));
    }

    #[test]
    fn compose_tail_caps_a_pathological_line() {
        let long = "x".repeat(TAIL_MAX_CHARS * 3);
        let got = compose_tail(&turn(&format!(r#"{{"type":"text","text":"{long}"}}"#))).unwrap();
        assert_eq!(got.chars().count(), TAIL_MAX_CHARS);
    }

    #[test]
    fn transcript_uuid_shape_refuses_traversal() {
        // Registry content is untrusted and lands in a path join.
        assert!(!transcript_uuid_shaped("../../etc/passwd"));
        assert!(!transcript_uuid_shaped(""));
        assert!(!transcript_uuid_shaped("a/b"));
        assert!(transcript_uuid_shaped(
            "346f5d0d-9840-473c-af21-eaf100ca9ec2"
        ));
    }

    #[test]
    fn session_tails_reads_a_transcript_and_omits_the_absent() {
        let dir =
            std::env::temp_dir().join(format!("fno-transcripts-{}-batch", std::process::id()));
        let proj = dir.join("-Users-someone-repo");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&proj).unwrap();
        let live = "346f5d0d-9840-473c-af21-eaf100ca9ec2";
        std::fs::write(
            proj.join(format!("{live}.jsonl")),
            format!(
                "{}\n{}\n",
                turn(r#"{"type":"text","text":"stale"}"#),
                turn(r#"{"type":"text","text":"latest line\nignored"}"#)
            ),
        )
        .unwrap();
        set_test_projects_dir(Some(&dir));
        let absent = "ffffffff-0000-0000-0000-000000000000";
        let got = session_tails(&[(live.to_string(), None), (absent.to_string(), None)]);
        set_test_projects_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.get(live).map(String::as_str), Some("latest line"));
        // Absent, not empty-string: the row renders no cell, never a placeholder.
        assert!(!got.contains_key(absent));
    }

    #[test]
    fn session_tails_reads_the_row_log_path_directly() {
        // A row that names its own transcript needs no glob: the rollout lives
        // outside every claude projects dir, and the direct read is what makes
        // its tail land.
        let dir = std::env::temp_dir().join(format!("fno-logtail-{}-direct", std::process::id()));
        let rollout = dir.join("2026").join("09").join("01");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&rollout).unwrap();
        let live = "01a0608a-1234-1234-1234-000000000001";
        let path = rollout.join(format!("rollout-2026-09-01T22-14-58-{live}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                turn(r#"{"type":"text","text":"stale"}"#),
                turn(r#"{"type":"text","text":"wake lane text"}"#)
            ),
        )
        .unwrap();
        // A claude projects dir that contains NOTHING for this uuid: the
        // direct read must win without the glob finding anything.
        let empty_projects = dir.join("projects");
        std::fs::create_dir_all(&empty_projects).unwrap();
        set_test_projects_dir(Some(&empty_projects));
        let got = session_tails(&[(live.to_string(), Some(path.to_string_lossy().into_owned()))]);
        set_test_projects_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.get(live).map(String::as_str), Some("wake lane text"));
    }

    #[test]
    fn session_tails_glob_finds_a_codex_rollout_by_uuid() {
        // No log_path on the row: the widened scan must still land the rollout
        // - three directory levels down, with the timestamp prefix before the
        // uuid - or the column stays blank for every row that lost its path.
        let dir = std::env::temp_dir().join(format!("fno-logtail-{}-glob", std::process::id()));
        let day = dir.join("2026").join("09").join("02");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&day).unwrap();
        let live = "01a0608a-1234-1234-1234-000000000002";
        std::fs::write(
            day.join(format!("rollout-2026-09-02T08-00-00-{live}.jsonl")),
            format!("{}\n", turn(r#"{"type":"text","text":"globbed rollout"}"#)),
        )
        .unwrap();
        set_test_sessions_dir(Some(&dir));
        let got = session_tails(&[(live.to_string(), None)]);
        set_test_sessions_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.get(live).map(String::as_str), Some("globbed rollout"));
    }

    #[test]
    fn session_tails_prefixed_match_refuses_a_partial_uuid_suffix() {
        // The `-` separator in the prefixed match keeps a LONGER hex tail from
        // reading as the wanted uuid: a file whose suffix merely extends the
        // uuid must not satisfy it.
        let dir = std::env::temp_dir().join(format!("fno-logtail-{}-suffix", std::process::id()));
        let day = dir.join("2026").join("09").join("02");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&day).unwrap();
        let wanted = "01a0608a-1234-1234-1234-000000000003";
        std::fs::write(
            day.join(format!("rollout-2026-09-02T08-00-00-{wanted}aaaa.jsonl")),
            format!("{}\n", turn(r#"{"type":"text","text":"wrong row"}"#)),
        )
        .unwrap();
        set_test_sessions_dir(Some(&dir));
        let got = session_tails(&[(wanted.to_string(), None)]);
        set_test_sessions_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!got.contains_key(wanted));
    }

    #[test]
    fn compose_tail_skips_tool_only_turns_and_user_turns() {
        let raw = [
            turn(r#"{"type":"text","text":"real prose"}"#),
            turn(r#"{"type":"tool_use","name":"Bash","input":{}}"#),
            r#"{"type":"user","message":{"content":[{"type":"text","text":"operator"}]}}"#
                .to_string(),
        ]
        .join("\n");
        // A trailing tool-use turn has no prose to show; falling back to the
        // last real assistant line beats rendering a blank that reads as idle.
        assert_eq!(compose_tail(&raw).as_deref(), Some("real prose"));
    }

    #[test]
    fn compose_tail_degrades_on_torn_and_empty_input() {
        // A bounded tail read starts mid-file, so a partial leading line is the
        // normal case, not an error (AC6-FR: degrade, never blank the sideline).
        let raw = format!(
            "{{\"type\":\"assis\n{}",
            turn(r#"{"type":"text","text":"ok"}"#)
        );
        assert_eq!(compose_tail(&raw).as_deref(), Some("ok"));
        assert_eq!(compose_tail(""), None);
        assert_eq!(compose_tail("not json at all"), None);
        // Present but prose-free: no cell rather than an empty-string cell.
        assert_eq!(compose_tail(&turn(r#"{"type":"text","text":"   "}"#)), None);
    }

    #[test]
    fn compose_tail_strips_control_characters() {
        // A transcript is untrusted text that lands in the terminal one char at
        // a time, so an ESC sequence in an assistant message would be REPLAYED
        // as terminal control - cursor moves, colour changes, or worse. Tabs and
        // CRs merely wreck the table's column alignment. Same posture as
        // `sanitize_name` / `sanitize_mail_text` on the server side.
        let raw = turn(r#"{"type":"text","text":"safe \u001b[31mred\u001b[0m and\ttabbed"}"#);
        let got = compose_tail(&raw).unwrap();
        assert!(
            !got.chars().any(char::is_control),
            "control chars must not reach the wire: {got:?}"
        );
        assert!(got.contains("safe"), "visible text survives: {got:?}");
        assert!(got.contains("red"), "{got:?}");
    }
}
