//! The proven drive primitive: inject a turn into an adopted `claude --bg`
//! session and confirm delivery by reading the session transcript.
//!
//! G1 substrate (epic x-07c1, node x-26df). The Phase-0 spike found the drive
//! half is the real, architecture-independent win: `control.sock op:'reply'
//! {short,text,auth}` lands a turn, and delivery is confirmed by a new assistant
//! turn in the session transcript JSONL -- NEVER by socket-write success. This is
//! independent of the (retired) keepalive question.
//!
//! Wire contracts pinned to claude-code **2.1.195** (readiness brief):
//!   - inject: `control.sock op:'reply' {short, text, auth}`. `[corroborated]`
//!   - confirm: a new assistant turn referencing a marker in
//!     `~/.claude/projects/<cwd-enc>/<session_uuid>.jsonl`. The filename IS the
//!     full session uuid, so we locate it by globbing the uuid across project
//!     dirs (mirrors `cli/src/fno/doctor.py` `_find_transcript_for`), sidestepping
//!     the lossy cwd-encoding.
//!   - auth = the daemon `control.key` (32-hex), NOT the per-worker `ptyAuth`.
//!   - NEVER inject the detach sentinels (they would tear the session off the
//!     daemon). `[corroborated]`
//!
//! Addressing keys on the FULL `session_uuid`; `short` is a wire-derived value
//! (`sessionId.split('-')[0]`) used only at the `control.sock` boundary.

use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use crate::claude_attach::{perform_attach, AttachError, AttachRequest, ControlTransport};

/// Detach sentinels Claude's client uses to pull a session off the daemon.
/// Injecting either would detach the session, so the drive primitive refuses any
/// text containing one. `[corroborated]`
pub const DETACH_SENTINELS: [&str; 2] = ["\x1b_cc-daemon-detach\x1b\\", "\x1b_cc-detach-msg;"];

/// Env override for the Claude projects (transcript) base dir (tests). When
/// unset, `$HOME/.claude/projects`.
pub const PROJECTS_DIR_ENV: &str = "FNO_CLAUDE_PROJECTS_DIR";

/// What can go wrong driving a turn.
#[derive(Debug)]
pub enum DriveError {
    /// The text contains a detach sentinel; refused before any write.
    UnsafeText,
    /// The attach handshake failed.
    Attach(AttachError),
    /// An I/O error (socket write, transcript read).
    Io(String),
    /// Injected, but no confirming assistant turn appeared within the budget.
    NotDelivered,
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::UnsafeText => write!(f, "refused: text contains a detach sentinel"),
            DriveError::Attach(e) => write!(f, "attach failed: {e}"),
            DriveError::Io(s) => write!(f, "io error: {s}"),
            DriveError::NotDelivered => {
                write!(f, "injected but no confirming assistant turn appeared")
            }
        }
    }
}

impl std::error::Error for DriveError {}

/// The Claude projects (transcript) base dir.
pub fn claude_projects_dir() -> PathBuf {
    if let Some(v) = std::env::var_os(PROJECTS_DIR_ENV) {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("projects")
}

/// Loose `8-4-4-4-12` hex session-uuid check (the transcript filename shape).
fn is_session_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&n, p)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Locate a session's transcript JSONL by its full uuid, across all project dirs.
/// Mirrors `doctor.py`: the filename is the uuid, so we never need the lossy
/// cwd-encoding. Returns `None` for a malformed uuid or when no transcript exists.
pub fn find_transcript(session_uuid: &str) -> Option<PathBuf> {
    if !is_session_uuid(session_uuid) {
        return None;
    }
    let base = claude_projects_dir();
    let entries = std::fs::read_dir(&base).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(format!("{session_uuid}.jsonl"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// True if `text` contains any detach sentinel.
pub fn contains_detach_sentinel(text: &str) -> bool {
    DETACH_SENTINELS.iter().any(|s| text.contains(s))
}

/// The sender of an a2a turn, rendered as a legible Claude-native tag. Mirrors
/// Claude's own `<IMPORTANT MESSAGE>` convention so an adopted session treats a
/// `<FNO ...>` line as structure natively (which closes most of the recipient-side
/// gap with no custom primer).
///
/// `fid` is the SHORT 8-hex sessionId of the driving session -- the identity, since
/// sessionIds ARE names (never a display name; the registry row + claim still key
/// on the FULL session_uuid underneath, the tag uses the short purely for
/// legibility). `harness`/`model` are context for later reference. This is legible
/// context, NOT unforgeable trust: the serde-escaped envelope is deliberately
/// deferred until a parser actually makes a trust decision on `fid` (nothing does
/// yet).
pub struct A2aFrom<'a> {
    /// Short 8-hex sessionId of the sender.
    pub fid: &'a str,
    /// Sender harness: `claude-code` / `codex` / `gemini`.
    pub harness: &'a str,
    /// Sender model id.
    pub model: &'a str,
}

/// Render the a2a tag prefix: `<FNO harness={h} model={m} fid={fid}>`.
pub fn a2a_tag(from: &A2aFrom) -> String {
    format!(
        "<FNO harness={} model={} fid={}>",
        from.harness, from.model, from.fid
    )
}

/// Build the `op:'reply'` inject line. When `from` is set, the turn text is
/// prefixed with the legible a2a tag (`<FNO ...> {text}`) so the recipient sees
/// it as agent-to-agent structure, not a human typing. Refuses text carrying a
/// detach sentinel. `auth` is omitted on the same-uid no-auth path.
pub fn build_reply_request(
    short: &str,
    text: &str,
    auth: Option<&str>,
    from: Option<&A2aFrom>,
) -> Result<String, DriveError> {
    if contains_detach_sentinel(text) {
        return Err(DriveError::UnsafeText);
    }
    let body = match from {
        Some(f) => format!("{} {}", a2a_tag(f), text),
        None => text.to_string(),
    };
    let mut obj = serde_json::Map::new();
    obj.insert("op".into(), "reply".into());
    obj.insert("short".into(), short.into());
    obj.insert("text".into(), body.into());
    if let Some(a) = auth {
        obj.insert("auth".into(), a.into());
    }
    let mut line = serde_json::Value::Object(obj).to_string();
    line.push('\n');
    Ok(line)
}

/// Inject a turn over `t` via `op:'reply'`, tagged as a2a from `from`. Writing
/// succeeded does NOT mean the turn landed -- confirm with [`confirm_marker_after`]
/// against the transcript.
pub fn inject_reply<T: ControlTransport>(
    t: &mut T,
    short: &str,
    text: &str,
    auth: Option<&str>,
    from: Option<&A2aFrom>,
) -> Result<(), DriveError> {
    let line = build_reply_request(short, text, auth, from)?;
    t.send_line(&line)
        .map_err(|e| DriveError::Io(e.to_string()))
}

/// Current byte length of `path` (the baseline to read new transcript lines from),
/// or 0 if absent/unreadable.
pub fn transcript_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Scan transcript lines appended after `since_byte` for an ASSISTANT turn
/// containing `marker`. The injected user turn also carries the marker, so we
/// only count assistant-role records -- that is what proves the model ACTED on
/// the inject, not that the socket echoed our text. `[corroborated]` (spike's
/// confirm rule)
pub fn confirm_marker_after(path: &Path, marker: &str, since_byte: u64) -> io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    use std::io::Seek;
    file.seek(io::SeekFrom::Start(since_byte))?;
    let reader = io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.contains(marker) && line_is_assistant(&line) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True if a transcript JSONL line is an assistant-role record. Claude's
/// transcript tags turns by a top-level `type` and/or a nested `message.role`;
/// accept either so a schema tweak does not silently break confirmation.
fn line_is_assistant(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if v.get("type").and_then(serde_json::Value::as_str) == Some("assistant") {
        return true;
    }
    v.get("message")
        .and_then(|m| m.get("role"))
        .and_then(serde_json::Value::as_str)
        == Some("assistant")
}

/// One a2a turn to drive: the `text` (which must embed `marker` for the transcript
/// confirm), and the `from` identity that becomes the legible `<FNO ...>` tag.
pub struct DriveTurn<'a> {
    pub text: &'a str,
    pub marker: &'a str,
    pub from: Option<&'a A2aFrom<'a>>,
}

/// Drive one turn end to end and confirm delivery: attach, baseline the
/// transcript, inject `turn.text` tagged as a2a, then poll the transcript for a
/// confirming assistant turn until `attempts` * `interval` elapses. Live glue
/// (real socket + real transcript); the unit-tested pieces are
/// [`build_reply_request`], [`confirm_marker_after`], [`find_transcript`].
///
/// ponytail: the poll loop sleeps for real; it is not unit-tested. Every decision
/// it makes is a tested function.
pub fn drive_and_confirm<T: ControlTransport>(
    transport: &mut T,
    attach: &AttachRequest,
    session_uuid: &str,
    turn: &DriveTurn,
    attempts: u32,
    interval: std::time::Duration,
) -> Result<(), DriveError> {
    perform_attach(transport, attach).map_err(DriveError::Attach)?;

    let transcript = find_transcript(session_uuid)
        .ok_or_else(|| DriveError::Io(format!("no transcript for session {session_uuid}")))?;
    let baseline = transcript_len(&transcript);

    inject_reply(
        transport,
        &attach.short,
        turn.text,
        attach.auth.as_deref(),
        turn.from,
    )?;

    for _ in 0..attempts.max(1) {
        if confirm_marker_after(&transcript, turn.marker, baseline)
            .map_err(|e| DriveError::Io(e.to_string()))?
        {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
    Err(DriveError::NotDelivered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Fake {
        sent: Vec<String>,
    }
    impl ControlTransport for Fake {
        fn send_line(&mut self, line: &str) -> io::Result<()> {
            self.sent.push(line.to_string());
            Ok(())
        }
        fn recv_line(&mut self) -> io::Result<Option<String>> {
            Ok(None)
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fno-drive-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    const UUID: &str = "a1b2c3d4-1111-2222-3333-444455556666";

    #[test]
    fn is_session_uuid_validates_shape() {
        assert!(is_session_uuid(UUID));
        assert!(!is_session_uuid("a1b2c3d4")); // short id, not a uuid
        assert!(!is_session_uuid("not-a-uuid"));
        assert!(!is_session_uuid("g1b2c3d4-1111-2222-3333-444455556666")); // non-hex
    }

    fn from_orchestrator() -> A2aFrom<'static> {
        A2aFrom {
            fid: "7d1f8bdc",
            harness: "claude-code",
            model: "opus-4.8",
        }
    }

    #[test]
    fn a2a_tag_is_legible_native_shape() {
        // <FNO harness= model= fid=short-sid>, fid is the SHORT 8-hex sessionId,
        // no JSON, no name field.
        assert_eq!(
            a2a_tag(&from_orchestrator()),
            "<FNO harness=claude-code model=opus-4.8 fid=7d1f8bdc>"
        );
    }

    #[test]
    fn build_reply_request_untagged_when_no_from() {
        let line = build_reply_request("a1b2c3d4", "hello world", Some("deadbeef"), None).unwrap();
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["op"], "reply");
        assert_eq!(v["short"], "a1b2c3d4");
        assert_eq!(v["text"], "hello world");
        assert_eq!(v["auth"], "deadbeef");
    }

    #[test]
    fn build_reply_request_prefixes_a2a_tag() {
        let from = from_orchestrator();
        let line = build_reply_request("a1b2c3d4", "ship it MARKER42", None, Some(&from)).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let text = v["text"].as_str().unwrap();
        // The turn is tagged legibly, and the marker survives for the confirm.
        assert_eq!(
            text,
            "<FNO harness=claude-code model=opus-4.8 fid=7d1f8bdc> ship it MARKER42"
        );
    }

    #[test]
    fn build_reply_request_omits_auth() {
        let line = build_reply_request("a1b2c3d4", "hi", None, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert!(v.get("auth").is_none());
    }

    #[test]
    fn build_reply_request_refuses_detach_sentinel() {
        let evil = format!("hi {}", DETACH_SENTINELS[0]);
        assert!(matches!(
            build_reply_request("a1b2c3d4", &evil, None, None),
            Err(DriveError::UnsafeText)
        ));
        assert!(matches!(
            build_reply_request("a1b2c3d4", DETACH_SENTINELS[1], None, None),
            Err(DriveError::UnsafeText)
        ));
    }

    #[test]
    fn inject_reply_writes_one_line_and_guards_sentinels() {
        let mut t = Fake { sent: Vec::new() };
        inject_reply(&mut t, "a1b2c3d4", "ping MARKER42", None, None).unwrap();
        assert_eq!(t.sent.len(), 1);
        assert!(t.sent[0].contains("\"op\":\"reply\""));
        assert!(t.sent[0].contains("MARKER42"));

        let mut t2 = Fake { sent: Vec::new() };
        assert!(inject_reply(&mut t2, "a1b2c3d4", DETACH_SENTINELS[0], None, None).is_err());
        assert!(t2.sent.is_empty(), "must not write unsafe text");
    }

    #[test]
    fn find_transcript_by_uuid_across_project_dirs() {
        let base = tmpdir("find");
        std::env::set_var(PROJECTS_DIR_ENV, &base);
        let proj = base.join("-Users-x-code-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let t = proj.join(format!("{UUID}.jsonl"));
        std::fs::write(&t, b"{}\n").unwrap();

        assert_eq!(find_transcript(UUID), Some(t));
        assert_eq!(
            find_transcript("ffffffff-0000-0000-0000-000000000000"),
            None
        );
        assert_eq!(find_transcript("bad-uuid"), None);
        std::env::remove_var(PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn confirm_marker_after_counts_only_assistant_turns() {
        let dir = tmpdir("confirm");
        let path = dir.join("t.jsonl");
        // Baseline content (pre-inject); the marker must NOT be sought here.
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"older"}}}}"#
        )
        .unwrap();
        let baseline = transcript_len(&path);

        // After inject: our own user echo carries the marker (must NOT match),
        // then the assistant turn carrying it (must match).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"drive MARKER42"}}}}"#
        )
        .unwrap();
        // Our own user echo carries the marker but is NOT an assistant turn -> not
        // yet delivered.
        assert!(!confirm_marker_after(&path, "MARKER42", baseline).unwrap());

        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"done MARKER42"}}}}"#
        )
        .unwrap();
        assert!(confirm_marker_after(&path, "MARKER42", baseline).unwrap());

        // An offset past everything sees nothing new.
        let end = transcript_len(&path);
        assert!(!confirm_marker_after(&path, "MARKER42", end).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn confirm_marker_absent_assistant_is_false() {
        let dir = tmpdir("absent");
        let path = dir.join("t.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"no token here"}}}}"#
        )
        .unwrap();
        assert!(!confirm_marker_after(&path, "MARKER42", 0).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
