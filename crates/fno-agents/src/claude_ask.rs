//! Client-side `claude --bg` ask path (ab-cc926b4e).
//!
//! `claude` is a self-supervised `claude --bg` shellout, not a PTY-managed
//! agent: it runs its own background daemon, a rendezvous Unix socket, and a
//! transcript/state dir under `~/.claude/jobs/<short-id>/`. The fno daemon
//! cannot PTY-manage it, so the Rust **client** replicates Python's
//! `providers/claude.py` + `providers/_claude_session_registry.py` +
//! `dispatch.py` ask path directly, bypassing the daemon RPC.
//!
//! **Byte-parity is the contract.** Every observable (stdout reply, exit code,
//! the BG8 envelope bytes on the wire, events.jsonl fields) must match the
//! Python implementation. This module is a faithful port; divergences are
//! bugs. The MCP-channel transport (US6) and the auto-route flip belong to the
//! follow-up node ab-0429c6e1 (carveout cv-827faf2b) and are intentionally
//! absent here.
//!
//! Scope of this file: Wave 1 primitives (registry-read + socket + pure
//! helpers). `bg_create` / `wait_for_reply` / `ask_followup` orchestration is
//! layered on top in later waves within this module.

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ===========================================================================
// Constants (verbatim from providers/claude.py + _claude_session_registry.py)
// ===========================================================================

/// `_ARGV_OVERFLOW_THRESHOLD` — messages larger than this (in UTF-8 bytes) are
/// passed to `claude --bg` via stdin instead of as an argv token.
pub const ARGV_OVERFLOW_THRESHOLD: usize = 200 * 1024;

/// `_STDOUT_HEAD_LIMIT` — chars of stdout carried in a parse-error diagnostic.
pub const STDOUT_HEAD_LIMIT: usize = 200;

/// Terminal-or-needs-input states. The poll loop exits when `state.json`
/// transitions to one of these; the timeline tail picks `text` from rows with
/// these states (running rows are tool-call narration, deliberately excluded).
pub const TERMINAL_STATES: [&str; 4] = ["done", "completed", "failed", "needs-input"];

/// 250 ms liveness-probe connect timeout (`_LIVENESS_PROBE_TIMEOUT_SEC`).
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// 5 s send-socket timeout (`_SEND_SOCKET_TIMEOUT_SEC`).
const SEND_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Backoff between the two `read_state_json` attempts (`_RETRY_BACKOFF_SEC`),
/// clearing claude's ~1 ms atomic-rename window.
const RETRY_BACKOFF: Duration = Duration::from_millis(10);

/// Bounded best-effort window for resolving the full session UUID at spawn
/// (mirrors `providers.claude._SPAWN_UUID_RETRY_*`). The happy path resolves on
/// the first probe (claude writes `~/.claude/sessions/<pid>.json` before
/// `claude --bg` returns the short-id); the retry only covers the rare write-lag
/// window. `resolve_session_uuid_at_spawn` short-circuits when the sessions dir
/// is absent, so a fresh-HOME test never sleeps here.
const SPAWN_UUID_RETRY_ATTEMPTS: u32 = 6;
const SPAWN_UUID_RETRY_BACKOFF: Duration = Duration::from_millis(300);

fn is_terminal_state(state: &str) -> bool {
    TERMINAL_STATES.contains(&state)
}

// ===========================================================================
// Errors (mirror the Python provider exception taxonomy)
// ===========================================================================

/// Why a session could not be reached (`OrphanReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReason {
    /// Entry exists but `messagingSocketPath` is null (suspended session).
    SocketNull,
    /// No `~/.claude/sessions/*.json` entry with this jobId.
    NotFound,
    /// Socket exists but a connect probe failed.
    LivenessFailed,
    /// x-2681: the session is live in the daemon roster but the control.sock
    /// fallback inject did not confirm -- a delivery failure, NOT a dead
    /// session, so the orchestration layer must NOT stamp it orphaned.
    RosterLiveInjectFailed,
}

impl OrphanReason {
    /// The exact reason token Python uses in messages/events.
    pub fn as_str(self) -> &'static str {
        match self {
            OrphanReason::SocketNull => "socket-null",
            OrphanReason::NotFound => "not-found",
            OrphanReason::LivenessFailed => "liveness-failed",
            OrphanReason::RosterLiveInjectFailed => "roster-live-inject-failed",
        }
    }
}

/// Errors raised by the claude ask path. The orchestration layer maps each to
/// the Python exit code (1/12/13/15) and event payload.
#[derive(Debug)]
pub enum AskError {
    /// `claude --bg` stdout did not match the short-id contract.
    Parse { stdout_head: String },
    /// `claude --bg` exited non-zero, timed out (124), or was missing (127).
    Subprocess { exit_code: i32, stderr: String },
    /// Session not reachable (locate/probe failure).
    Orphan {
        reason: OrphanReason,
        short_id: String,
    },
    /// Socket connect/write/close failure during send.
    Socket { message: String },
    /// No reply within the poll timeout.
    Timeout { elapsed_sec: f64, short_id: String },
    /// A non-transient I/O error while reading state.json during polling
    /// (EACCES/EROFS/EISDIR). Python lets the OSError propagate rather than
    /// masking it as a 600s timeout; we surface it as a fatal exit-1 error.
    Io { message: String },
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskError::Parse { stdout_head } => write!(
                f,
                "unable to parse short-id from claude --bg output: first {} chars: {}",
                stdout_head.chars().count(),
                py_repr(stdout_head),
            ),
            AskError::Subprocess { exit_code, stderr } => {
                write!(f, "claude --bg exited {}: {}", exit_code, py_repr(stderr))
            }
            AskError::Orphan { reason, short_id } => write!(
                f,
                "agent short-id {} is not reachable (reason: {})",
                py_repr(short_id),
                reason.as_str()
            ),
            AskError::Socket { message } => write!(f, "{}", message),
            AskError::Io { message } => write!(f, "{}", message),
            AskError::Timeout {
                elapsed_sec,
                short_id,
            } => {
                write!(f, "timed out waiting for reply after {:.1}s", elapsed_sec)?;
                if !short_id.is_empty() {
                    write!(f, " (short_id={})", short_id)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AskError {}

// ===========================================================================
// Python-compatible string encoders (the byte-parity load-bearers)
// ===========================================================================

/// Mirror CPython `json.dumps` default string encoding (`ensure_ascii=True`):
/// emit a JSON string literal (surrounding quotes included) where every
/// non-ASCII scalar is `\uXXXX`-escaped (astral chars as a surrogate pair).
///
/// This is why the envelope is built from a fixed template plus this encoder
/// rather than `serde_json`: serde sorts object keys and emits raw UTF-8, so it
/// would not match Python byte-for-byte.
pub fn json_string_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xffff {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    // Surrogate pair, matching CPython.
                    let v = cp - 0x10000;
                    let hi = 0xd800 + (v >> 10);
                    let lo = 0xdc00 + (v & 0x3ff);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                }
            }
        }
    }
    out.push('"');
    out
}

/// Mirror Python `html.escape(s, quote=True)` for XML-attribute safety.
/// Order matters: `&` first.
pub fn html_escape_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// Approximate CPython `repr()` of a `str` for diagnostic messages: single
/// quotes (double if the string contains a single quote but no double quote),
/// with `\n`/`\r`/`\t`/`\\` and non-printable escapes. Used only in error text,
/// so this targets the common cases the parity tests exercise.
///
/// Public so the client's unresolvable-`ask` exit-2 surface (bin/client.rs) can
/// reproduce Python's `{name!r}` in `select_provider`'s error text byte-for-byte.
pub fn py_repr(s: &str) -> String {
    let use_double = s.contains('\'') && !s.contains('"');
    let quote = if use_double { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

// ===========================================================================
// claude home + bg-session paths (separate from fno's registry home)
// ===========================================================================

/// Resolver for claude's own session state under `$HOME/.claude`.
/// Mirrors `_claude_session_registry._sessions_dir` / `_jobs_dir_for`.
/// HOME is read from the environment so tests can pin it.
#[derive(Debug, Clone)]
pub struct ClaudeHome {
    home: PathBuf,
}

impl ClaudeHome {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self { home }
    }

    pub fn at(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.home.join(".claude").join("sessions")
    }

    pub fn jobs_dir_for(&self, short_id: &str) -> PathBuf {
        self.home.join(".claude").join("jobs").join(short_id)
    }

    /// The daemon roster path. Honors `FNO_CLAUDE_DAEMON_DIR` FIRST (a supported
    /// alt-home / alternate-daemon override, matching `claude_roster::daemon_dir`
    /// and the deliver path's `load_default`), else `<home>/.claude/daemon`. The
    /// env-first order keeps the ask-lane roster pre-check reading the SAME roster
    /// the deliver step resolves, so the fallback never skips in an alt-daemon
    /// setup; the home-relative fallback keeps it hermetic under a test
    /// `ClaudeHome`. Byte-parity with Python's `_daemon_dir` (env-first-else-home).
    pub fn daemon_roster_path(&self) -> PathBuf {
        if let Some(dir) = std::env::var_os(crate::claude_roster::DAEMON_DIR_ENV) {
            return PathBuf::from(dir).join("roster.json");
        }
        self.home.join(".claude").join("daemon").join("roster.json")
    }
}

/// Pointer into the claude session registry for one bg supervisor session
/// (`SessionLocator`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLocator {
    pub pid: i64,
    pub short_id: String,
    pub messaging_socket_path: String,
    pub jobs_dir: PathBuf,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

/// Parsed `state.json` snapshot (`StateSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StateSnapshot {
    pub state: String,
    pub updated_at: Option<String>,
    pub output_result: Option<String>,
    pub intent: Option<String>,
}

// ===========================================================================
// Pure helpers: parse_short_id, build_argv
// ===========================================================================

/// Extract the 8-hex short-id from `claude --bg`'s first stdout line.
/// Mirrors `_SHORT_ID_PATTERN = r"^backgrounded · ([0-9a-f]{8}) · "`.
/// The `·` is U+00B7 MIDDLE DOT.
pub fn parse_short_id(stdout: &str) -> Result<String, AskError> {
    if stdout.is_empty() {
        return Err(AskError::Parse {
            stdout_head: String::new(),
        });
    }
    let first_line = stdout.split('\n').next().unwrap_or("");
    if let Some(id) = match_short_id(first_line) {
        return Ok(id);
    }
    Err(AskError::Parse {
        stdout_head: head_chars(stdout, STDOUT_HEAD_LIMIT),
    })
}

/// `^backgrounded · ([0-9a-f]{8}) · ` matcher without a regex dependency.
fn match_short_id(line: &str) -> Option<String> {
    const PREFIX: &str = "backgrounded \u{b7} ";
    const SEP: &str = " \u{b7} ";
    // `claude --bg` colorizes the short-id when its stdout is colorized
    // (real bytes: `backgrounded · \x1b[36m<id>\x1b[39m · <name>`). Strip ANSI
    // CSI escapes before the byte checks below so the hex field is contiguous;
    // otherwise the leading `\x1b` fails the hexdigit test and the id is lost.
    let cleaned = strip_ansi_csi(line);
    let rest = cleaned.strip_prefix(PREFIX)?;
    let rb = rest.as_bytes();
    if rb.len() < 8 {
        return None;
    }
    // Validate the first 8 BYTES are ASCII lowercase hex BEFORE any char-index
    // slice. `split_at(8)` panics if byte 8 lands inside a multi-byte UTF-8
    // scalar; once the first 8 bytes are confirmed ASCII, byte 8 is guaranteed
    // a char boundary (Codex P2: malformed non-ASCII stdout must not crash).
    if !rb[..8]
        .iter()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    let (hex, after) = rest.split_at(8);
    if after.starts_with(SEP) {
        Some(hex.to_string())
    } else {
        None
    }
}

/// Remove ANSI CSI escape sequences (`ESC '[' params/intermediates final`) from
/// a line. Conservative: only well-formed CSI sequences are dropped; any other
/// byte, including a lone `ESC` that does not start a CSI, is preserved. CSI
/// control bytes are all single-byte ASCII, so iterating by `char` keeps
/// multi-byte scalars (e.g. the `·` separator) intact. The common case (no
/// `ESC` at all) borrows the input without allocating.
fn strip_ansi_csi(s: &str) -> Cow<'_, str> {
    if !s.contains('\u{1b}') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
                          // Parameter (0x30-0x3F) and intermediate (0x20-0x2F) bytes.
            while let Some(&p) = chars.peek() {
                if ('\u{20}'..='\u{3f}').contains(&p) {
                    chars.next();
                } else {
                    break;
                }
            }
            // Final byte (0x40-0x7E) terminates a well-formed CSI sequence.
            if let Some(&f) = chars.peek() {
                if ('\u{40}'..='\u{7e}').contains(&f) {
                    chars.next();
                }
            }
            continue;
        }
        out.push(c);
    }
    Cow::Owned(out)
}

/// First `n` chars (not bytes) of `s`, matching Python slice semantics.
fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// x-b6e2: the Tier-3 harness-native passthrough flags that claude maps but the
/// other providers largely don't. Bundled so the deep claude dispatch chain
/// threads ONE param, not four. Each is an opaque value forwarded to claude's
/// own flag; empty/None = the flag is omitted. codex/agy take only `add_dir`
/// individually (their sole real cell); `agent`/`tools`/`deny_tools` fail closed
/// for every non-claude provider at the client.rs guard, so a non-claude builder
/// never receives them.
#[derive(Clone, Copy, Default)]
pub struct HarnessFlags<'a> {
    pub add_dir: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub allowed_tools: Option<&'a str>,
    pub disallowed_tools: Option<&'a str>,
}

impl<'a> HarnessFlags<'a> {
    /// Append the mapped `--flag <value>` tokens (claude's own spellings) to an
    /// argv, skipping empty/None. Shared by the bg and headless claude builders
    /// so their token order stays identical (parity).
    fn push_onto(&self, argv: &mut Vec<String>) {
        for (flag, value) in [
            ("--add-dir", self.add_dir),
            ("--agent", self.agent),
            ("--allowedTools", self.allowed_tools),
            ("--disallowedTools", self.disallowed_tools),
        ] {
            if let Some(v) = value.filter(|v| !v.is_empty()) {
                argv.push(flag.to_string());
                argv.push(v.to_string());
            }
        }
    }
}

/// Render the argv for `claude --bg` (`_build_argv`). When `use_stdin`, the
/// message is omitted from argv and fed via stdin instead. A non-empty `model`
/// (x-571f per-node pin) appends `--model <m>` between `--name` and the
/// message, scoping the pin to this session; empty/None means today's argv
/// byte-for-byte (parity with Python's falsy-`model` check).
pub fn build_argv(
    name: &str,
    message: &str,
    use_stdin: bool,
    model: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    flags: HarnessFlags,
) -> Vec<String> {
    let mut argv = vec![
        "claude".to_string(),
        "--bg".to_string(),
        "--name".to_string(),
        name.to_string(),
    ];
    // x-dfa4: exact passthrough to claude's own --permission-mode. The caller
    // resolves --yolo -> bypassPermissions before this point; empty/None = the
    // claude default (unchanged argv).
    if let Some(m) = permission_mode.filter(|m| !m.is_empty()) {
        argv.push("--permission-mode".to_string());
        argv.push(m.to_string());
    }
    if let Some(value) = effort.filter(|v| !v.is_empty()) {
        argv.push("--effort".to_string());
        argv.push(value.to_string());
    }
    // x-b6e2: Tier-3 passthrough (--add-dir/--agent/--allowedTools/
    // --disallowedTools). Kept identical to the Python _build_argv (parity).
    flags.push_onto(&mut argv);
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.to_string());
    }
    if !use_stdin {
        argv.push(message.to_string());
    }
    argv
}

/// True iff the message must be sent via stdin (exceeds the argv threshold).
pub fn use_stdin_for(message: &str) -> bool {
    message.len() > ARGV_OVERFLOW_THRESHOLD
}

// ===========================================================================
// BG8 envelope + socket primitives
// ===========================================================================

/// Render the BG8 envelope bytes (`_build_envelope`), byte-for-byte with
/// Python's `json.dumps(separators=(",",":"))` over the fixed dict shape, plus
/// the trailing newline. Key order is fixed: type, message{role, content},
/// priority. `from_name` is html-attribute-escaped; `message` is inserted raw
/// into the wrapper then JSON-string-encoded.
/// Wrap `message` in the cross-session-message container that marks it as a peer
/// turn (`build_cross_session_container`). `from_name` is html-attribute-escaped;
/// `message` is inserted raw. Shared by the BG8 envelope ([`build_envelope`]) and
/// the x-2681 control.sock ask fallback, so both frame a peer turn identically.
pub fn build_cross_session_container(message: &str, from_name: &str) -> String {
    format!(
        "<cross-session-message from-name=\"{}\">\n{}\n</cross-session-message>",
        html_escape_quote(from_name),
        message
    )
}

pub fn build_envelope(message: &str, from_name: &str) -> Vec<u8> {
    let wrapped = build_cross_session_container(message, from_name);
    let content = json_string_ascii(&wrapped);
    let line = format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}},\"priority\":\"next\"}}\n",
        content
    );
    line.into_bytes()
}

/// Connect to an AF_UNIX SOCK_STREAM path with a bounded timeout. std's
/// `UnixStream::connect` has no connect-timeout knob, so a wedged listener (or
/// a full accept backlog) can block it indefinitely — Python sets the socket
/// timeout BEFORE connect (Codex P2). This does a nonblocking connect + `poll`
/// for writability, then restores blocking mode, so connect can never outlast
/// `timeout`.
fn connect_unix_timeout(path: &str, timeout: Duration) -> std::io::Result<UnixStream> {
    use std::os::unix::io::FromRawFd;
    let c_path = std::ffi::CString::new(path).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket path contains NUL")
    })?;
    // SAFETY: a standard libc socket/connect/poll sequence. The fd is closed on
    // every error path and wrapped into a UnixStream (which owns + closes it) on
    // success.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = c_path.as_bytes();
        if bytes.len() >= std::mem::size_of_val(&addr.sun_path) {
            libc::close(fd);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path too long",
            ));
        }
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr(),
            bytes.len(),
        );
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let addr_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let rc = libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len);
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINPROGRESS) {
                libc::close(fd);
                return Err(err);
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
            let pr = libc::poll(&mut pfd, 1, ms);
            if pr < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            if pr == 0 {
                libc::close(fd);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connect timed out",
                ));
            }
            let mut soerr: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            if libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut soerr as *mut _ as *mut libc::c_void,
                &mut len,
            ) < 0
            {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            if soerr != 0 {
                libc::close(fd);
                return Err(std::io::Error::from_raw_os_error(soerr));
            }
        }
        // Restore blocking mode for the subsequent read/write timeouts.
        if libc::fcntl(fd, libc::F_SETFL, flags) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(UnixStream::from_raw_fd(fd))
    }
}

/// Single-shot send of the BG8 envelope over the messaging socket
/// (`send_to_session`). A close-time error after a successful write is
/// propagated as a send failure (AF_UNIX: the only reliable "bytes didn't
/// land" signal).
pub fn send_to_session(sock_path: &str, content: &str, from_name: &str) -> Result<(), AskError> {
    let payload = build_envelope(content, from_name);
    let mut stream =
        connect_unix_timeout(sock_path, SEND_SOCKET_TIMEOUT).map_err(|e| AskError::Socket {
            message: e.to_string(),
        })?;
    let _ = stream.set_write_timeout(Some(SEND_SOCKET_TIMEOUT));
    let _ = stream.set_read_timeout(Some(SEND_SOCKET_TIMEOUT));
    let write_res = stream.write_all(&payload);
    // Explicitly flush+shutdown to surface a peer-reject as a close error,
    // mirroring Python's reliance on close() raising on AF_UNIX.
    let close_res = stream
        .flush()
        .and_then(|_| stream.shutdown(std::net::Shutdown::Both));
    if let Err(e) = write_res {
        return Err(AskError::Socket {
            message: e.to_string(),
        });
    }
    if let Err(e) = close_res {
        return Err(AskError::Socket {
            message: format!("close after send failed: {}", e),
        });
    }
    Ok(())
}

/// Return true iff a 250 ms connect to `sock_path` succeeds (`liveness_probe`).
/// Connect-then-close; no read/write. Any error (including a connect that
/// doesn't complete within the 250 ms bound) → false. Uses the timeout-bounded
/// connect so a wedged listener can't hang the probe (Python sets the timeout
/// before connect).
pub fn liveness_probe(sock_path: &str) -> bool {
    connect_unix_timeout(sock_path, LIVENESS_PROBE_TIMEOUT).is_ok()
}

// ===========================================================================
// Registry-read primitives: locate_session, read_state_json, read_timeline_tail
// ===========================================================================

/// Find the bg session whose `jobId` matches `short_id` (`locate_session`).
/// Requires `kind == "bg"` and a non-empty `messagingSocketPath`. Two-pass in
/// spirit (a respawn can leave a dead pid's file with a null socket): we scan
/// sorted entries and return the first live match; null-socket entries are
/// skipped. Corrupt JSON files are skipped silently. Returns `None` when no
/// live match exists or the sessions dir is absent.
pub fn locate_session(home: &ClaudeHome, short_id: &str) -> Option<SessionLocator> {
    let sessions = home.sessions_dir();
    if !sessions.exists() {
        return None;
    }
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(&sessions) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect(),
        Err(_) => return None,
    };
    entries.sort();

    for entry_path in entries {
        let raw = match std::fs::read_to_string(&entry_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !v.is_object() {
            continue;
        }
        if v.get("jobId").and_then(|x| x.as_str()) != Some(short_id) {
            continue;
        }
        if v.get("kind").and_then(|x| x.as_str()) != Some("bg") {
            continue;
        }
        let sock = match v.get("messagingSocketPath").and_then(|x| x.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue, // null/empty socket: respawn artifact, keep scanning
        };
        // pid is the file stem (`<pid>.json`).
        let pid = match entry_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(p) => p,
            None => continue,
        };
        return Some(SessionLocator {
            pid,
            short_id: short_id.to_string(),
            messaging_socket_path: sock,
            jobs_dir: home.jobs_dir_for(short_id),
            session_id: v
                .get("sessionId")
                .and_then(|x| x.as_str())
                .map(String::from),
            cwd: v.get("cwd").and_then(|x| x.as_str()).map(String::from),
        });
    }
    None
}

/// Resolve the FULL session UUID for a bg session by its 8-hex `jobId`
/// (`resolve_session_uuid`). The stream-json `--resume` lane keys on the full
/// `sessionId`; the jobId is only a 32-bit prefix. Unlike `locate_session`,
/// this does NOT require a live `messagingSocketPath` (an idle bg session is
/// exactly the resume target): it prefers a supervisor whose socket is live but
/// falls back to any `kind == "bg"` match carrying a non-empty `sessionId`.
/// Returns `None` when no such match exists or the sessions dir is absent.
pub fn resolve_session_uuid(home: &ClaudeHome, short_id: &str) -> Option<String> {
    let sessions = home.sessions_dir();
    if !sessions.exists() {
        return None;
    }
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(&sessions) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect(),
        Err(_) => return None,
    };
    entries.sort();

    let mut fallback: Option<String> = None;
    for entry_path in entries {
        let raw = match std::fs::read_to_string(&entry_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !v.is_object() {
            continue;
        }
        if v.get("jobId").and_then(|x| x.as_str()) != Some(short_id) {
            continue;
        }
        if v.get("kind").and_then(|x| x.as_str()) != Some("bg") {
            continue;
        }
        let sid = match v.get("sessionId").and_then(|x| x.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        match v.get("messagingSocketPath").and_then(|x| x.as_str()) {
            Some(s) if !s.is_empty() => return Some(sid), // live supervisor wins
            _ => {
                if fallback.is_none() {
                    fallback = Some(sid);
                }
            }
        }
    }
    fallback
}

/// Best-effort full session-UUID resolution at spawn
/// (`resolve_session_uuid_at_spawn`). Returns the full `sessionId` for
/// `short_id`, or `None` within the bounded retry window. NEVER blocks the
/// short-id report past that window: an unresolved UUID is a tolerated miss (the
/// live `chat` lane then opens a fresh pipe rather than adopting a guessed
/// UUID). Short-circuits when the sessions dir is absent (claude never wrote
/// one), so there is no point retrying — and a fresh-HOME test never sleeps.
pub fn resolve_session_uuid_at_spawn(home: &ClaudeHome, short_id: &str) -> Option<String> {
    if short_id.is_empty() || !home.sessions_dir().exists() {
        return None;
    }
    for attempt in 0..SPAWN_UUID_RETRY_ATTEMPTS {
        if let Some(uuid) = resolve_session_uuid(home, short_id) {
            return Some(uuid);
        }
        if attempt + 1 < SPAWN_UUID_RETRY_ATTEMPTS {
            std::thread::sleep(SPAWN_UUID_RETRY_BACKOFF);
        }
    }
    None
}

/// Classify why a `locate_session` miss occurred (`_classify_orphan_reason`):
/// re-walk the sessions dir; if a bg entry with this jobId exists but its
/// socket is null → `SocketNull`, otherwise `NotFound`.
pub fn classify_orphan_reason(home: &ClaudeHome, short_id: &str) -> OrphanReason {
    let sessions = home.sessions_dir();
    if let Ok(rd) = std::fs::read_dir(&sessions) {
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().map(|x| x != "json").unwrap_or(true) {
                continue;
            }
            let raw = match std::fs::read_to_string(&p) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("jobId").and_then(|x| x.as_str()) == Some(short_id)
                && v.get("kind").and_then(|x| x.as_str()) == Some("bg")
            {
                let sock = v.get("messagingSocketPath").and_then(|x| x.as_str());
                if sock.map(|s| s.is_empty()).unwrap_or(true) {
                    return OrphanReason::SocketNull;
                }
            }
        }
    }
    OrphanReason::NotFound
}

/// Error returned by `read_state_json`. `NotFound` (absent) and `Parse`
/// (present-but-unreadable JSON / empty / atomic-rename window) are transient:
/// the poll loop retries. `Io` is a non-transient filesystem fault
/// (EACCES/EROFS/EISDIR) that Python lets propagate rather than mask as a
/// timeout — the poll loop surfaces it as a fatal error.
#[derive(Debug)]
pub enum StateReadError {
    NotFound,
    Parse,
    Io(std::io::Error),
}

/// Parse `<jobs_dir>/state.json` into a `StateSnapshot` (`read_state_json`),
/// retrying once on a parse error to absorb claude's atomic-rename window. A
/// non-transient I/O fault (`Io`) is returned immediately, not retried.
pub fn read_state_json(jobs_dir: &Path) -> Result<StateSnapshot, StateReadError> {
    let state_path = jobs_dir.join("state.json");
    match parse_state(&state_path) {
        Ok(s) => Ok(s),
        Err(StateReadError::Parse) => {
            std::thread::sleep(RETRY_BACKOFF);
            parse_state(&state_path)
        }
        Err(e) => Err(e),
    }
}

fn parse_state(state_path: &Path) -> Result<StateSnapshot, StateReadError> {
    let raw_text = match std::fs::read_to_string(state_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StateReadError::NotFound),
        // EACCES/EROFS/EISDIR etc. are NOT retryable — polling 600s would mask
        // the cause as a timeout. Surface them (Python lets the OSError fly).
        Err(e) => return Err(StateReadError::Io(e)),
    };
    if raw_text.trim().is_empty() {
        return Err(StateReadError::Parse);
    }
    let v: serde_json::Value =
        serde_json::from_str(&raw_text).map_err(|_| StateReadError::Parse)?;
    let output = v.get("output");
    let output_result = output
        .and_then(|o| if o.is_object() { o.get("result") } else { None })
        .and_then(|r| r.as_str())
        .map(String::from);
    Ok(StateSnapshot {
        state: v
            .get("state")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        updated_at: v
            .get("updatedAt")
            .and_then(|x| x.as_str())
            .map(String::from),
        output_result,
        intent: v.get("intent").and_then(|x| x.as_str()).map(String::from),
    })
}

/// Read `<jobs_dir>/timeline.jsonl` from `offset` and concatenate `text` fields
/// from terminal-or-needs-input rows (`read_timeline_tail`). Missing file,
/// read error, or non-UTF-8 tail → empty string. Unparseable lines skipped.
pub fn read_timeline_tail(jobs_dir: &Path, offset: u64) -> String {
    use std::io::{Seek, SeekFrom};
    let timeline = jobs_dir.join("timeline.jsonl");
    if !timeline.exists() {
        return String::new();
    }
    let mut file = match std::fs::File::open(&timeline) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return String::new();
    }
    let mut tail = Vec::new();
    if file.read_to_end(&mut tail).is_err() {
        return String::new();
    }
    let text = match String::from_utf8(tail) {
        Ok(t) => t,
        Err(_) => return String::new(),
    };
    let mut chunks = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: serde_json::Value = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !row.is_object() {
            continue;
        }
        let state = row.get("state").and_then(|x| x.as_str()).unwrap_or("");
        if !is_terminal_state(state) {
            continue;
        }
        if let Some(piece) = row.get("text").and_then(|x| x.as_str()) {
            if !piece.is_empty() {
                chunks.push_str(piece);
            }
        }
    }
    chunks
}

/// Current byte size of `<jobs_dir>/timeline.jsonl`, or 0 if absent/unreadable.
/// Captured as the baseline offset before a send.
pub fn timeline_offset(jobs_dir: &Path) -> u64 {
    std::fs::metadata(jobs_dir.join("timeline.jsonl"))
        .map(|m| m.len())
        .unwrap_or(0)
}

// ===========================================================================
// Wave 2: bg_create (subprocess), wait_for_reply (poll), ask_followup
// ===========================================================================

/// Default poll interval for `wait_for_reply` (`poll_interval=0.5` in Python).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Result of a successful `claude --bg` create: the parsed short-id plus the
/// captured streams (mirrors the parts of Python's `ProviderResult` the
/// orchestration layer reads).
#[derive(Debug, Clone)]
pub struct CreateResult {
    pub short_id: String,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
}

/// Outcome of scanning `claude --bg` stdout for its launch-confirmation line.
enum ShortIdScan {
    /// The `backgrounded · <id> · <name>` line was seen; scanning STOPS here (we
    /// never read to EOF). `consumed` is the stdout read up to and including it.
    Found { short_id: String, consumed: String },
    /// stdout reached EOF (claude exited) with no confirmation line -- a launch
    /// failure. `consumed` is the full stdout, for the parse-error diagnostic.
    NoId { consumed: String },
}

/// Read `claude --bg` stdout line by line and return `Found` the instant the
/// launch-confirmation line yields a short-id, WITHOUT consuming the rest of the
/// stream; return `NoId` only at EOF. Pure over any `BufRead` so the early-return
/// contract is unit-testable without spawning `claude` (PR #544 / codex P1).
fn scan_stdout_for_short_id<R: BufRead>(mut reader: R) -> ShortIdScan {
    let mut consumed = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return ShortIdScan::NoId { consumed }, // EOF: no confirmation
            Ok(_) => {
                consumed.push_str(&line);
                if let Some(short_id) = match_short_id(line.trim_end()) {
                    return ShortIdScan::Found { short_id, consumed };
                }
            }
            // A mid-stream read fault is treated as no-confirmation (the caller
            // reaps the exit code for a precise error), never a panic.
            Err(_) => return ShortIdScan::NoId { consumed },
        }
    }
}

/// Invoke `claude --bg` for a brand-new supervisor session (`bg_create`).
/// `extra_env` carries the `FNO_AGENT_*` attribution vars the caller
/// injects. On argv overflow the message is fed via stdin. Failure modes map
/// to `AskError::Subprocess` with the Python exit codes (subprocess non-zero,
/// 124 timeout, 127 missing binary).
pub fn bg_create(
    name: &str,
    message: &str,
    cwd: &Path,
    timeout: Option<Duration>,
    extra_env: &[(&str, &str)],
    model: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    flags: HarnessFlags,
) -> Result<CreateResult, AskError> {
    use std::process::{Command, Stdio};

    let use_stdin = use_stdin_for(message);
    let argv = build_argv(name, message, use_stdin, model, permission_mode, effort, flags);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    cmd.env("FNO_AGENT_SELF", name);
    cmd.env("FNO_AGENT_PROVIDER", "claude");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(if use_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let start = std::time::Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AskError::Subprocess {
                exit_code: 127,
                stderr: format!("claude CLI not found: {}", e),
            });
        }
        Err(e) => {
            return Err(AskError::Subprocess {
                exit_code: 127,
                stderr: e.to_string(),
            });
        }
    };

    // Own each pipe directly. The scan thread reads stdout line by line and
    // returns the moment `claude --bg` prints its launch-confirmation line -- we
    // never read stdout to EOF. That is the fix for codex P1 on PR #544: the
    // detached agent claude forks inherits the stdout pipe and can hold it open
    // forever, so a read-to-EOF wait (`wait_with_output`) would hang; and a wait
    // that timed out AFTER the agent had launched would SIGKILL only the launcher,
    // orphaning the agent while the node became re-dispatchable. Returning on the
    // confirmation line means a launched worker is always captured (never
    // orphaned), and a timeout can only fire BEFORE the confirmation -- i.e.
    // before any agent forked -- so its SIGKILL has nothing to orphan.
    let stdin_handle = if use_stdin { child.stdin.take() } else { None };
    let stdout_handle = match child.stdout.take() {
        Some(h) => h,
        None => {
            return Err(AskError::Subprocess {
                exit_code: 1,
                stderr: "claude --bg exposed no stdout pipe".to_string(),
            })
        }
    };
    let stderr_handle = child.stderr.take();
    let pid = child.id();

    // Drain stderr on its own thread (a chatty claude mustn't deadlock on a full
    // stderr pipe). Its JoinHandle yields the captured stderr: the failure path
    // joins it (the launcher is exiting there, so it's bounded) for an accurate
    // message; the success path never needs stderr and never joins (it could
    // block on a stderr pipe the detached agent holds open).
    let stderr_join: Option<std::thread::JoinHandle<String>> = stderr_handle.map(|mut eh| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = eh.read_to_string(&mut s);
            s
        })
    });

    // Write the (possibly >200KB) message to stdin on its OWN detached thread. By
    // the time claude prints the confirmation it has consumed stdin, so this
    // write has completed on the success path; on the timeout path the SIGKILL
    // below unblocks it with a broken pipe.
    if let Some(mut sin) = stdin_handle {
        let msg = message.to_string();
        std::thread::spawn(move || {
            let _ = sin.write_all(msg.as_bytes());
            // drop closes stdin so claude sees EOF
        });
    }

    // Scan stdout for the confirmation line on its own thread; the main thread
    // bounds the wait with the timeout.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let scan = scan_stdout_for_short_id(BufReader::new(stdout_handle));
        let _ = tx.send(scan);
    });

    let scan = match timeout {
        Some(d) => match rx.recv_timeout(d) {
            Ok(s) => s,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // No confirmation within the deadline. claude prints the short-id
                // AFTER it backgrounds the agent, so no agent has launched yet --
                // SIGKILL the stalled launcher with nothing to orphan.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                let secs = d.as_secs_f64();
                let secs_str = if secs.fract() == 0.0 {
                    format!("{}", secs as u64)
                } else {
                    format!("{}", secs)
                };
                return Err(AskError::Subprocess {
                    exit_code: 124,
                    stderr: format!("claude --bg timed out after {}s", secs_str),
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(AskError::Subprocess {
                    exit_code: 1,
                    stderr: "claude --bg stdout scan thread disconnected".to_string(),
                });
            }
        },
        // No deadline: block until the scan resolves. Safe from the original hang
        // because the scan returns on the confirmation line, not at EOF.
        None => rx.recv().unwrap_or(ShortIdScan::NoId {
            consumed: String::new(),
        }),
    };

    let duration_ms = start.elapsed().as_millis();

    match scan {
        // A confirmed launch wins regardless of what the launcher does next: the
        // agent is backgrounded and registered by its short-id, so a later
        // nonzero exit of the parent (or a never-closing inherited pipe) is moot.
        // We do NOT wait for the exit code -- that is what makes the wait
        // unhangable. stderr is unused on success, so we never join its drain.
        ShortIdScan::Found { short_id, consumed } => Ok(CreateResult {
            short_id,
            stdout: consumed,
            stderr: String::new(),
            duration_ms,
        }),
        ShortIdScan::NoId { consumed } => {
            // stdout closed before any confirmation: a genuine launch failure.
            // stdout EOF means the launcher is terminating, so wait() and the
            // stderr join both return promptly for a precise code + message.
            let exit_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
            let stderr = stderr_join
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            if exit_code != 0 {
                Err(AskError::Subprocess { exit_code, stderr })
            } else {
                Err(AskError::Parse {
                    stdout_head: head_chars(&consumed, STDOUT_HEAD_LIMIT),
                })
            }
        }
    }
}

/// Poll `<jobs_dir>/state.json` until a fresh terminal state appears, then
/// return the reply (`wait_for_reply`). Exit condition: `state ∈ TERMINAL_STATES`
/// AND (`baseline` is None OR `updated_at` lexicographically `> baseline`).
/// Reply preference: non-empty `output.result`, else the timeline tail from
/// `timeline_offset`. `ProviderTimeoutError`-equivalent on deadline.
pub fn wait_for_reply(
    jobs_dir: &Path,
    baseline_updated_at: Option<&str>,
    timeline_offset: u64,
    timeout: Duration,
    poll_interval: Duration,
    short_id: &str,
) -> Result<String, AskError> {
    let deadline = std::time::Instant::now() + timeout;
    let final_snap = loop {
        // NotFound/Parse are transient (recipient hasn't written yet, or an
        // atomic-rename window) -> poll again. A non-transient Io fault is
        // fatal: surface it rather than spin to the timeout (Python parity).
        let snap = match read_state_json(jobs_dir) {
            Ok(s) => Some(s),
            Err(StateReadError::NotFound) | Err(StateReadError::Parse) => None,
            Err(StateReadError::Io(e)) => {
                return Err(AskError::Io {
                    message: e.to_string(),
                });
            }
        };
        if let Some(ref s) = snap {
            if is_terminal_state(&s.state) {
                let advanced = match (baseline_updated_at, &s.updated_at) {
                    (None, _) => true,
                    (Some(base), Some(cur)) => cur.as_str() > base,
                    (Some(_), None) => false,
                };
                if advanced {
                    break snap.unwrap();
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(AskError::Timeout {
                elapsed_sec: timeout.as_secs_f64(),
                short_id: short_id.to_string(),
            });
        }
        std::thread::sleep(poll_interval);
    };

    match final_snap.output_result {
        Some(ref r) if !r.is_empty() => Ok(r.clone()),
        _ => Ok(read_timeline_tail(jobs_dir, timeline_offset)),
    }
}

/// Orchestrate locate → probe → baseline → send → wait_for_reply for one
/// follow-up (`ask_followup`). `home` is injected for testability;
/// `jobs_dir_override` lets the caller pin the poll dir (tests). Returns the
/// reply text (`""` when the recipient produced none). The baseline is captured
/// BEFORE the send so a stale `output.result` cannot impersonate the reply.
#[allow(clippy::too_many_arguments)]
/// x-2681: true iff `short_id` is present in the daemon roster under `home`.
/// Lenient -- a missing/torn/type-drifted roster yields false, never an error. A
/// cheap pre-check for the control.sock ask fallback; the deliver step's own
/// connect is the authoritative liveness gate, so roster PRESENCE (not
/// pid-liveness) is enough. Mirrors `_claude_session_registry.roster_live`.
fn roster_live(home: &ClaudeHome, short_id: &str) -> bool {
    match crate::claude_roster::ClaudeRoster::load(&home.daemon_roster_path()) {
        Ok(roster) => roster.find(short_id).is_some(),
        Err(_) => false,
    }
}

/// x-2681 ask-lane fallback: deliver `message` to a roster-live but socket-null
/// session over the daemon `control.sock` (the single wire vehicle,
/// [`crate::mail_inject::deliver_via_control_sock`]), then collect the reply from
/// the bg jobs-dir. Mirrors Python's `_ask_via_control_sock`:
///   - inject not confirmed -> `Orphan(RosterLiveInjectFailed)` (a delivery
///     failure on a LIVE session; the caller must not stamp it orphaned).
///   - delivered, reply from the jobs-dir tail -> the reply text.
///   - delivered, no jobs-dir (operator session) -> `Timeout` (delivered, no
///     reply surface; never fabricate an empty reply -- Open Questions 1/3).
fn ask_via_control_sock(
    short_id: &str,
    message: &str,
    from_name: &str,
    timeout: Duration,
    poll_interval: Duration,
    target_jobs_dir: &Path,
) -> Result<String, AskError> {
    // Baseline the reply surface BEFORE inject so a pre-existing terminal state
    // cannot impersonate this turn's reply.
    let baseline_updated_at = read_state_json(target_jobs_dir)
        .ok()
        .and_then(|s| s.updated_at);
    let offset = timeline_offset(target_jobs_dir);

    let wrapped = build_cross_session_container(message, from_name);
    if crate::mail_inject::deliver_via_control_sock(
        short_id,
        &wrapped,
        crate::mail_inject::DEFAULT_ATTEMPTS,
        crate::mail_inject::DEFAULT_INTERVAL_MS,
    )
    .is_err()
    {
        return Err(AskError::Orphan {
            reason: OrphanReason::RosterLiveInjectFailed,
            short_id: short_id.to_string(),
        });
    }

    // Delivered. No jobs-dir (operator session) -> nothing to poll: report
    // delivered-no-reply rather than spin the full timeout or fabricate a reply.
    if !target_jobs_dir.exists() {
        return Err(AskError::Timeout {
            elapsed_sec: 0.0,
            short_id: short_id.to_string(),
        });
    }

    wait_for_reply(
        target_jobs_dir,
        baseline_updated_at.as_deref(),
        offset,
        timeout,
        poll_interval,
        short_id,
    )
}

pub fn ask_followup(
    home: &ClaudeHome,
    claude_short_id: &str,
    message: &str,
    from_name: &str,
    timeout: Duration,
    poll_interval: Duration,
    jobs_dir_override: Option<&Path>,
) -> Result<String, AskError> {
    let locator = match locate_session(home, claude_short_id) {
        Some(l) => l,
        None => {
            let reason = classify_orphan_reason(home, claude_short_id);
            // x-2681: a socket-null session that is live in the daemon roster is
            // reachable over the daemon control.sock. Fall back before orphaning.
            // not-found is genuinely dead and never falls back (Locked Decision 5).
            if reason == OrphanReason::SocketNull && roster_live(home, claude_short_id) {
                let jd = jobs_dir_override
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| home.jobs_dir_for(claude_short_id));
                return ask_via_control_sock(
                    claude_short_id,
                    message,
                    from_name,
                    timeout,
                    poll_interval,
                    &jd,
                );
            }
            return Err(AskError::Orphan {
                reason,
                short_id: claude_short_id.to_string(),
            });
        }
    };

    if !liveness_probe(&locator.messaging_socket_path) {
        // Socket exists but is dead. Same control.sock fallback when roster-live.
        if roster_live(home, claude_short_id) {
            let jd = jobs_dir_override
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| locator.jobs_dir.clone());
            return ask_via_control_sock(
                claude_short_id,
                message,
                from_name,
                timeout,
                poll_interval,
                &jd,
            );
        }
        return Err(AskError::Orphan {
            reason: OrphanReason::LivenessFailed,
            short_id: claude_short_id.to_string(),
        });
    }

    let target_jobs_dir: PathBuf = jobs_dir_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| locator.jobs_dir.clone());

    // Baseline BEFORE send (AC2-EDGE invariant).
    let baseline_updated_at = read_state_json(&target_jobs_dir)
        .ok()
        .and_then(|s| s.updated_at);
    let offset = timeline_offset(&target_jobs_dir);

    send_to_session(&locator.messaging_socket_path, message, from_name)?;

    wait_for_reply(
        &target_jobs_dir,
        baseline_updated_at.as_deref(),
        offset,
        timeout,
        poll_interval,
        claude_short_id,
    )
}

// ===========================================================================
// Wave 3: orchestration (dispatch_claude_ask) — validation, flock,
// create-vs-followup, registry stamping, exit codes, events.
// ===========================================================================

use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry, RegistryEntry};
use crate::AgentStatus;

/// `_NAME_MAX_LEN` / `_FROM_NAME_MAX_LEN`.
const NAME_MAX_LEN: usize = 128;
const FROM_NAME_MAX_LEN: usize = 128;
/// `_DEFAULT_FOLLOWUP_TIMEOUT_SEC`.
const DEFAULT_FOLLOWUP_TIMEOUT: Duration = Duration::from_secs(600);
/// Lock-acquisition ceiling (Python's `lock_timeout`); contention here is rare.
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound the `claude --bg` *launch* wait for a spawn (create). A spawn only
/// waits for claude to print its short-id and exit, but `bg_create`'s
/// `wait_with_output` reads stdout/stderr to EOF -- and the detached agent
/// claude forks inherits those pipe fds, so the read can block on EOF
/// indefinitely if that agent holds them open. Spawn callers (spawn.sh,
/// dispatch-node.sh) pass no --timeout, so without a default the create wait
/// falls into bg_create's unbounded `rx.recv()` arm and hangs the caller
/// forever. 120s is far beyond a normal sub-5s launch; on overrun the child is
/// SIGKILLed and the caller sees exit 124 instead of a wedged process.
const DEFAULT_SPAWN_TIMEOUT: Duration = Duration::from_secs(120);

/// How recent an inside-leg report must be for a worker to count as "provably
/// live" when a follow-up fails to route (x-c393). A bg `/target` worker reports
/// at least per turn, but a long turn can leave a multi-minute gap, so the
/// window is generous; `fno agents reconcile` (the `claude logs` probe) is the
/// eventual authority that orphans a genuinely dead worker. ponytail: a fixed
/// ceiling, not config -- reconcile is the backstop.
const PROVABLY_LIVE_WINDOW_SECS: u64 = 3600;

/// Whether a row is "provably live": it carries an inside-leg report recent
/// enough that a follow-up routing miss is a gap, not a death (x-c393). Checked
/// against the CURRENT row under the registry lock at stamp time -- not a
/// pre-ask snapshot -- so a report that landed during a long ask is not missed
/// (codex P2). A live row must NOT be stamped orphaned; that would mislead
/// `fno agents list`. reconcile's `claude logs` probe orphans a truly dead one.
fn is_provably_live_report(
    inside_leg: Option<&crate::state::InsideLegReport>,
    now_secs: u64,
) -> bool {
    inside_leg.is_some_and(|r| r.received_within(now_secs, PROVABLY_LIVE_WINDOW_SECS))
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The create-wait timeout for a spawn: an explicit `--timeout` wins, else the
/// bounded default. Never returns `None`, so a spawn launch can never fall into
/// `bg_create`'s unbounded wait arm. Pulled out as a pure fn so the defaulting
/// is unit-testable without spawning `claude`.
fn spawn_create_timeout(explicit: Option<Duration>) -> Duration {
    explicit.unwrap_or(DEFAULT_SPAWN_TIMEOUT)
}

/// Outcome of a claude ask: what to print to stdout/stderr and the process exit
/// code. `stdout` already carries any trailing newline (create) or none
/// (followup reply), matching Python's `sys.stdout.write`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl AskOutcome {
    fn ok_stdout(s: String) -> Self {
        Self {
            stdout: s,
            stderr: String::new(),
            exit_code: 0,
        }
    }
    /// Errors mirror Python's `print(str(exc), file=sys.stderr)`: the message
    /// plus a trailing newline. `stderr` holds the exact bytes the client
    /// writes verbatim (no added newline at the print site).
    fn err(msg: impl Into<String>, code: i32) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("{}\n", msg.into()),
            exit_code: code,
        }
    }
}

/// UTC `_utc_now_iso()` second-precision timestamp.
fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Append one `events.jsonl` line in the Python-agents envelope
/// (`{...fields, ts, kind}`, compact). Free function (not `.emit(`) so the
/// crate's daemon-emit-kind scanner ignores these Python-side audit kinds,
/// matching `client_verbs::append_agents_event`.
///
/// SCOPE NOTE (cv-022d74f9): success events here carry their explicit fields
/// only. Python's `emit_with_context` also flattens a 13-field `EventContext`
/// (from_*/to_*/caller_kind/transport/request_id/target_session_id) onto
/// success events; porting `build_context` is deferred to the observability
/// surface (ab-85119580 / the ab-0429c6e1 cutover). Failure events already
/// match Python's plain `events.emit` (no context).
pub fn emit_event(events_path: &Path, kind: &str, fields: &[(&str, serde_json::Value)]) {
    // ensure_ascii parity with Python's json.dumps: encode every string scalar
    // (keys + string values) via json_string_ascii so non-ASCII field content
    // (e.g. an accented agent name) escapes to \uXXXX identically. Non-string
    // values (numbers/bools/null) are ASCII already.
    fn enc_value(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::String(s) => json_string_ascii(s),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }
    let ts = now_iso();
    let mut parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{}:{}", json_string_ascii(k), enc_value(v)))
        .collect();
    parts.push(format!("\"ts\":{}", json_string_ascii(&ts)));
    parts.push(format!("\"kind\":{}", json_string_ascii(kind)));
    let line = format!("{{{}}}\n", parts.join(","));
    let res = (|| -> std::io::Result<()> {
        if let Some(parent) = events_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut fh = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_path)?;
        fh.write_all(line.as_bytes())
    })();
    if let Err(e) = res {
        // cv-b3f6c5a1: a failed events.jsonl write stays best-effort (it never
        // fails the ask -- parity with Python's best-effort agents emit), but is
        // surfaced ONCE per process instead of fully swallowed, so a broken /
        // unwritable events dir is observable. Mirrors the output.jsonl tee
        // warn-once in codex_ask.rs. Shared by claude and codex ask via this fn.
        if !EMIT_EVENT_WRITE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "fno-agents: failed to append {} event to {}: {} (further event-write failures this run suppressed)",
                kind,
                events_path.display(),
                e
            );
        }
    }
}

/// Set the first time an `events.jsonl` append fails, so the warn in
/// `emit_event` fires once per process rather than once per dropped event.
static EMIT_EVENT_WRITE_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Validate name/message/from_name (`_validate_inputs` + `_validate_from_name`).
/// Returns the exit-2 error message on failure.
///
/// Public so other provider ports (codex_ask, future gemini_ask) share the
/// same checks rather than carrying weaker duplicates. The function mirrors
/// Python's `dispatch.py::_validate_inputs` + `_validate_from_name` and is
/// the canonical pre-flight gate for any `ask` dispatch.
pub fn validate_inputs(name: &str, message: &str, from_name: &str) -> Result<(), String> {
    validate_spawn_inputs(name, from_name)?;
    if message.is_empty() || message.trim().is_empty() {
        return Err("message must be non-empty".into());
    }
    Ok(())
}

/// Validate name + from_name WITHOUT the message check. `spawn` allows an
/// empty initial message (Python `dispatch_spawn` parity: it validates name
/// and from_name inline but never rejects an empty message; the once paths
/// default an empty message to "hello" instead).
pub fn validate_spawn_inputs(name: &str, from_name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("agent name must not be empty".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!(
            "agent name must not contain path separators or '..': {}",
            py_repr(name)
        ));
    }
    if name.chars().count() > NAME_MAX_LEN {
        return Err(format!(
            "name must be <={} chars (got {})",
            NAME_MAX_LEN,
            name.chars().count()
        ));
    }
    if name.len() == 8
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(format!(
            "agent name {} must not match short-id shape ^[0-9a-f]{{8}}$ (prevents name/id collision)",
            py_repr(name)
        ));
    }
    if let Some(bad) = ['\u{0}', '\n', '\r', '=']
        .into_iter()
        .find(|c| name.contains(*c))
    {
        return Err(format!(
            "agent name {} contains a forbidden character ({} would corrupt subprocess env injection)",
            py_repr(name),
            py_repr(&bad.to_string())
        ));
    }
    if from_name.is_empty() {
        return Err("from-name must not be empty".into());
    }
    if from_name.chars().count() > FROM_NAME_MAX_LEN {
        return Err(format!(
            "from-name must be <={} chars (got {})",
            FROM_NAME_MAX_LEN,
            from_name.chars().count()
        ));
    }
    if from_name
        .chars()
        .any(|c| matches!(c, '"' | '<' | '>' | '&'))
    {
        return Err("from-name must not contain XML-unsafe characters (\", <, >, &)".into());
    }
    Ok(())
}

/// RAII per-agent flock at `<registry-dir>/locks/<name>.lock`, byte-compatible
/// with Python's `_agent_lock_path` (`fcntl.flock` ⇄ `fs2`). Held for the
/// duration of one ask so concurrent same-agent asks serialize (AC10).
struct AgentLock {
    _file: std::fs::File,
}

impl AgentLock {
    fn acquire(home: &AgentsHome, name: &str, timeout: Duration) -> Result<Self, ()> {
        let locks_dir = home.root().join("locks");
        let _ = std::fs::create_dir_all(&locks_dir);
        let path = locks_dir.join(format!("{}.lock", name));
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(_) => return Err(()),
        };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

/// Stable abi-side log path for `fno agents logs <name>` (`_derive_log_path`).
fn derive_log_path(home: &AgentsHome, name: &str) -> PathBuf {
    home.root()
        .join("agents")
        .join("logs")
        .join(format!("{}.log", name))
}

/// Orchestrate one claude `ask`: validate, lock, decide create-vs-followup,
/// stamp the registry, emit events, and return what to print plus the exit
/// code. `extra_env` is forwarded to `bg_create` (production passes `&[]` to
/// inherit the real environment; tests inject `PATH`/`FAKE_CLAUDE_*`).
#[allow(clippy::too_many_arguments)]
pub fn dispatch_claude_ask(
    home: &AgentsHome,
    claude_home: &ClaudeHome,
    name: &str,
    message: &str,
    from_name: &str,
    // Create-only inputs, retained for API stability after Task 1.3a removed
    // the create branch from `ask` (callers still pass them; `spawn` owns
    // creation now via `dispatch_claude_spawn`).
    _cwd: &Path,
    _yolo: bool,
    timeout: Option<Duration>,
    _extra_env: &[(&str, &str)],
) -> AskOutcome {
    if let Err(msg) = validate_inputs(name, message, from_name) {
        return AskOutcome::err(msg, 2);
    }

    let events = home.events_jsonl();
    let registry_path = home.registry_json();

    let _lock = match AgentLock::acquire(home, name, LOCK_ACQUIRE_TIMEOUT) {
        Ok(l) => l,
        Err(()) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[("stage", "lock-timeout".into()), ("name", name.into())],
            );
            return AskOutcome::err(
                // Python: f"lock timeout for agent {name!r} after {timeout}s"
                // with timeout=30.0 (float) -> "...after 30.0s".
                format!(
                    "lock timeout for agent {} after {:.1}s",
                    py_repr(name),
                    LOCK_ACQUIRE_TIMEOUT.as_secs_f64()
                ),
                11,
            );
        }
    };

    // A registry READ error (corrupt / schema-mismatched file) must fail BEFORE
    // any provider side effect: treating it as empty would route a known agent
    // down the create path and spawn an orphaned `claude --bg` supervisor that
    // only fails later on the write. Python's dispatcher exits 12 here (Codex
    // P2). A missing file is NOT an error (load_registry returns the default).
    let registry = match load_registry(&registry_path) {
        Ok(r) => r,
        Err(e) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-read".into()),
                    ("name", name.into()),
                    ("error", e.to_string().into()),
                ],
            );
            return AskOutcome::err(format!("registry read failed: {}", e), 12);
        }
    };
    let existing = registry.find(name).cloned();

    match existing {
        Some(entry) => followup(
            home,
            claude_home,
            &events,
            &registry_path,
            name,
            &entry,
            message,
            from_name,
            timeout,
        ),
        None => {
            // ask never creates (Task 1.3a): unknown-name -> exit 16, byte-parity
            // with Python's dispatch_ask after Task 1.1.
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "unknown-name".into()),
                    ("name", name.into()),
                    ("provider", "claude".into()),
                ],
            );
            AskOutcome::err(
                format!(
                    "unknown agent {}; spawn it first: fno agents spawn {} -p <provider>",
                    py_repr(name),
                    name
                ),
                16,
            )
        }
    }
}

/// Orchestrate one claude `spawn`: validate, lock, collision-check, create,
/// and return a compact JSON receipt.  The `create` helper machinery is
/// reused directly; only the output shape differs from `dispatch_claude_ask`.
///
/// Receipt (byte-parity with Python `cmd_spawn`):
/// `{"name": "<name>", "short_id": "<8hex>", "provider": "claude", "status": "live"}\n`
#[allow(clippy::too_many_arguments)]
pub fn dispatch_claude_spawn(
    home: &AgentsHome,
    claude_home: &ClaudeHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
    extra_env: &[(&str, &str)],
    model: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    flags: HarnessFlags,
) -> AskOutcome {
    // spawn allows an empty initial message (Python dispatch_spawn parity).
    if let Err(msg) = validate_spawn_inputs(name, from_name) {
        return AskOutcome::err(msg, 2);
    }

    let events = home.events_jsonl();
    let registry_path = home.registry_json();

    let _lock = match AgentLock::acquire(home, name, LOCK_ACQUIRE_TIMEOUT) {
        Ok(l) => l,
        Err(()) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "lock-timeout".into()),
                    ("name", name.into()),
                    ("provider", "claude".into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "lock timeout for agent {} after {:.1}s",
                    py_repr(name),
                    LOCK_ACQUIRE_TIMEOUT.as_secs_f64()
                ),
                11,
            );
        }
    };

    // Collision check INSIDE the lock (mirrors Python dispatch_spawn 4a).
    let registry = match load_registry(&registry_path) {
        Ok(r) => r,
        Err(e) => {
            return AskOutcome::err(format!("registry read failed: {}", e), 12);
        }
    };
    if registry.find(name).is_some() {
        // Python: f"agent {name!r} already exists; ..." -> py_repr, not {:?}.
        return AskOutcome::err(
            format!(
                "agent {} already exists; use 'fno agents rm {}' first or pick another name",
                py_repr(name),
                name
            ),
            2,
        );
    }

    // x-dfa4: --yolo now maps to bypassPermissions for claude (was a no-op); an
    // explicit --permission-mode wins (the two are mutually exclusive upstream).
    // Resolved once here so the receipt below can name the applied mode.
    let effective_mode: Option<&str> = match permission_mode {
        Some(m) => Some(m),
        None if yolo => Some("bypassPermissions"),
        None => None,
    };

    // Delegate to the retained create machinery. A spawn always bounds the
    // launch wait (DEFAULT_SPAWN_TIMEOUT when the caller passed no --timeout) so
    // a `claude --bg` that never EOFs its inherited stdout/stderr can't hang the
    // dispatcher forever; on overrun create() returns exit 124.
    let inner = create(
        home,
        claude_home,
        &events,
        &registry_path,
        name,
        message,
        from_name,
        cwd,
        yolo,
        Some(spawn_create_timeout(timeout)),
        extra_env,
        model,
        effective_mode,
        effort,
        flags,
    );
    if inner.exit_code != 0 {
        return inner;
    }

    // On success, `create` returns the 8-hex short_id in stdout (with trailing newline).
    // Build the JSON receipt expected by the CLI and parity tests.
    let short_id = inner.stdout.trim_end_matches('\n').to_string();
    // Create-path output contract trip-wire (sigma-review type-design
    // finding): the receipt format does not itself validate the id shape.
    // assert! (not debug_assert!) so release builds keep the guard - the
    // check is one 8-byte scan per spawn and a loud panic beats a malformed
    // receipt propagating to jq consumers (gemini review, PR #457).
    assert!(
        short_id.len() == 8 && short_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "create path produced non-8hex short_id: {short_id:?}"
    );
    // Escape `"` in the name so the receipt stays valid JSON for jq consumers
    // (name validation blocks backslash already; Python cmd_spawn parity).
    let safe_name = name.replace('"', "\\\"");
    // Locked Decision 5: name the applied mode (flag or yolo-derived) so an audit
    // of "why did this worker have edit rights" has a durable answer. Only when
    // set, so the unset receipt is byte-identical (AC7). Values are exact
    // passthrough, so escape `"` defensively.
    let perm_field = match effective_mode.filter(|m| !m.is_empty()) {
        Some(m) => format!(r#", "permission_mode": "{}""#, m.replace('"', "\\\"")),
        None => String::new(),
    };
    AskOutcome {
        stdout: format!(
            r#"{{"name": "{safe_name}", "short_id": "{short_id}", "provider": "claude", "status": "live"{perm_field}}}"#
        ) + "\n",
        stderr: inner.stderr,
        exit_code: 0,
    }
}

/// Dispatch a `claude -p` truly-headless one-shot (x-2c27 `headless` substrate).
///
/// Unlike [`dispatch_claude_spawn`] (the detached `--bg` thread, which returns a
/// short-id receipt), this runs `claude -p` SYNCHRONOUSLY to completion, prints
/// the model's reply to stdout, and exits - no registry row, no short-id, no
/// driveable pane. `claude` never *defaults* to `-p`; this is the one lane that
/// shells it (Locked Decision 4). `ask` and the relay claude hop keep `--bg`.
///
/// A headless run cannot answer permission prompts, so it always passes
/// `--dangerously-skip-permissions` (mirrors the agy/codex once lanes); `yolo`
/// is therefore a no-op, accepted only for signature parity. `claude_home` is
/// unused (no session registry for an ephemeral one-shot) and kept for parity
/// with the bg path's signature.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_claude_headless(
    _claude_home: &ClaudeHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    _yolo: bool,
    timeout: Option<Duration>,
    model: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    flags: HarnessFlags,
) -> AskOutcome {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    if let Err(msg) = validate_spawn_inputs(name, from_name) {
        return AskOutcome::err(msg, 2);
    }

    // Python-truthiness parity with the once lanes: only an EMPTY message
    // becomes "hello"; a whitespace-only prompt passes through unchanged.
    let effective = if message.is_empty() { "hello" } else { message };
    let use_stdin = use_stdin_for(effective);

    // x-dfa4: an explicit --permission-mode replaces the hardcoded
    // --dangerously-skip-permissions for the headless lane; unset keeps the
    // skip (a headless one-shot cannot answer permission prompts).
    let mut argv: Vec<String> = vec!["claude".into(), "-p".into()];
    match permission_mode.filter(|m| !m.is_empty()) {
        Some(m) => {
            argv.push("--permission-mode".into());
            argv.push(m.into());
        }
        None => argv.push("--dangerously-skip-permissions".into()),
    }
    // x-c772: an explicit --model is forwarded to `claude -p --model <m>`
    // (empty/None = claude default). Exact passthrough, no fuzzy resolution.
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.to_string());
    }
    if let Some(value) = effort.filter(|v| !v.is_empty()) {
        argv.push("--effort".to_string());
        argv.push(value.to_string());
    }
    // x-b6e2: Tier-3 passthrough, same token order as the Python headless_create.
    flags.push_onto(&mut argv);
    if !use_stdin {
        argv.push(effective.to_string());
    }
    // QoS (x-c5cc): a headless one-shot is an fno-spawned child — exec-wrap it
    // at background priority (worker_qos=utility) so it never starves the
    // foreground. Identity when worker_qos=off.
    let argv = crate::spawn_gate::qos_wrap(cwd, argv);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    cmd.env("FNO_AGENT_SELF", name);
    cmd.env("FNO_AGENT_PROVIDER", "claude");
    cmd.env("FNO_AGENT_FROM", from_name);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(if use_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return AskOutcome::err(format!("claude CLI not found: {}", e), 127);
        }
        Err(e) => return AskOutcome::err(e.to_string(), 127),
    };

    // Drain stdout/stderr on their own threads so a filling pipe can never
    // deadlock the bounded wait below (we can't use wait_with_output: it blocks
    // with no timeout knob). The common argv path still uses these.
    let stdout_join = child.stdout.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = p.read_to_end(&mut s);
            s
        })
    });
    let stderr_join = child.stderr.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = p.read_to_end(&mut s);
            s
        })
    });

    // Large (>200KB) prompts go via stdin on a detached writer thread so a
    // filling stdout pipe can't deadlock a synchronous write. The common path
    // (argv) skips this entirely.
    let stdin_writer = if use_stdin {
        child.stdin.take().map(|mut s| {
            let data = effective.to_string();
            std::thread::spawn(move || {
                let _ = s.write_all(data.as_bytes());
            })
        })
    } else {
        None
    };

    // Bounded wait: a `-p` one-shot has no detached fork to orphan, but a hung
    // `claude -p` (startup/network stall) would otherwise wedge the caller
    // forever even when --timeout was supplied (codex P2). When `timeout` is
    // set, SIGKILL the child past the deadline and report exit 124 (parity with
    // the --bg launch timeout); with no --timeout the wait is unbounded, as the
    // user asked for no bound. The reader threads above keep the pipes drained.
    // `wait_result` is Some(exit_code) on a real exit, None on a timeout kill.
    let wait_result: Result<Option<i32>, String> = if let Some(limit) = timeout {
        let deadline = Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(Some(st.code().unwrap_or(1))),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => break Err(format!("claude -p wait failed: {}", e)),
            }
        }
    } else {
        child
            .wait()
            .map(|st| Some(st.code().unwrap_or(1)))
            .map_err(|e| format!("claude -p wait failed: {}", e))
    };

    // Collect output by JOINING the reader/writer threads ONLY on a clean exit.
    // On a timeout-kill we must NOT join: SIGKILL'ing `claude` does not reap a
    // grandchild it spawned (e.g. a shell's `sleep`), which keeps the inherited
    // stdout/stderr write end open, so `read_to_end` would block until THAT
    // grandchild exits - defeating the very timeout we just enforced. Abandon
    // the threads (they die when the OS finally closes the fds); the timed-out
    // one-shot has no useful deliverable anyway. (Same orphan-pipe hazard the
    // --bg launch path documents.)
    match wait_result {
        Err(msg) => AskOutcome::err(msg, 1),
        Ok(None) => AskOutcome::err(
            format!(
                "claude -p timed out after {:.1}s",
                timeout
                    .expect("timeout Some on the bounded path")
                    .as_secs_f64()
            ),
            124,
        ),
        Ok(Some(exit_code)) => {
            let stdout = stdout_join
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            let stderr = stderr_join
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            if let Some(h) = stdin_writer {
                let _ = h.join();
            }
            AskOutcome {
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit_code,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn followup(
    _home: &AgentsHome,
    claude_home: &ClaudeHome,
    events: &Path,
    registry_path: &Path,
    name: &str,
    entry: &RegistryEntry,
    message: &str,
    from_name: &str,
    timeout: Option<Duration>,
) -> AskOutcome {
    let short_id = match entry.claude_short_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return AskOutcome::err(
                format!(
                    "registry entry {} has no claude_short_id; cannot follow up. Remove with 'fno agents rm {}' and recreate.",
                    py_repr(name), name
                ),
                12,
            );
        }
    };

    emit_event(
        events,
        "agent_followup_started",
        &[
            ("name", name.into()),
            ("provider", entry.provider.clone().into()),
            ("short_id", short_id.clone().into()),
        ],
    );

    let wait = timeout.unwrap_or(DEFAULT_FOLLOWUP_TIMEOUT);
    match ask_followup(
        claude_home,
        &short_id,
        message,
        from_name,
        wait,
        DEFAULT_POLL_INTERVAL,
        None,
    ) {
        Ok(reply) => {
            // Stamp status=live + last_message_at under the registry flock.
            // A write failure here is FATAL (Python dispatch.py:537-556 parity):
            // the message was already delivered but the registry can't record
            // it, so withhold the reply from stdout and exit 12 to prevent a
            // double-send on retry.
            if let Err(e) = update_registry(registry_path, |reg| {
                if let Some(en) = reg.find_mut(name) {
                    en.status = AgentStatus::Live;
                    en.last_message_at = Some(now_iso());
                }
            }) {
                emit_event(
                    events,
                    "agent_followup_failed",
                    &[
                        ("stage", "registry-write".into()),
                        ("name", name.into()),
                        ("short_id", short_id.clone().into()),
                        ("error", e.to_string().into()),
                    ],
                );
                return AskOutcome::err(
                    format!(
                        "registry write failed: {}. NOTE: message was already delivered; do not retry.",
                        e
                    ),
                    12,
                );
            }
            emit_event(
                events,
                "agent_followup_done",
                &[
                    ("stage", "followup".into()),
                    ("name", name.into()),
                    ("provider", entry.provider.clone().into()),
                    ("short_id", short_id.clone().into()),
                    ("reply_chars", (reply.chars().count() as u64).into()),
                    ("backend", "socket".into()),
                ],
            );
            AskOutcome::ok_stdout(reply)
        }
        Err(AskError::Orphan { reason, .. }) => {
            // Decide orphan-vs-routing-gap against the CURRENT row under the same
            // registry lock that stamps it, so an inside-leg report that landed
            // during a long ask is not missed (x-c393; codex P2). A recent report
            // => routing gap (status untouched, `fno agents list` still shows the
            // live worker); else stamp orphaned. Best-effort stamp: a write
            // failure stays OBSERVABLE (stderr warning + agent_status_stamp_failed
            // event) like Python's, not a silent swallow.
            let now = now_epoch_secs();
            // x-2681: "roster-live-inject-failed" means the control.sock fallback
            // delivery failed on a session that IS live in the daemon roster --
            // a routing gap, never a death, so it takes the same no-stamp branch
            // as a recent inside-leg report (a roster-live session is never
            // stamped orphaned).
            let roster_live_gap = reason == OrphanReason::RosterLiveInjectFailed;
            let mut provably_live = false;
            let mut stamp_warning = String::new();
            if let Err(e) = update_registry(registry_path, |reg| {
                if let Some(en) = reg.find_mut(name) {
                    if roster_live_gap || is_provably_live_report(en.inside_leg.as_ref(), now) {
                        provably_live = true;
                    } else {
                        en.status = AgentStatus::Orphaned;
                    }
                }
            }) {
                stamp_warning = format!(
                    "fno agents: warning: failed to mark {} as orphaned: {}\n",
                    py_repr(name),
                    e
                );
                emit_event(
                    events,
                    "agent_status_stamp_failed",
                    &[
                        ("name", name.into()),
                        ("short_id", short_id.clone().into()),
                        ("target_status", "orphaned".into()),
                        ("error", e.to_string().into()),
                    ],
                );
            }
            if provably_live {
                emit_event(
                    events,
                    "agent_followup_failed",
                    &[
                        ("stage", "routing-gap".into()),
                        ("name", name.into()),
                        ("short_id", short_id.clone().into()),
                        ("reason", reason.as_str().into()),
                    ],
                );
                return AskOutcome {
                    stdout: String::new(),
                    stderr: format!(
                        "agent {} is live but not currently routable (reason: {}); message not delivered. Try 'claude attach {}'\n",
                        py_repr(name),
                        reason.as_str(),
                        short_id
                    ),
                    exit_code: 13,
                };
            }
            emit_event(
                events,
                "agent_followup_failed",
                &[
                    ("stage", "orphan".into()),
                    ("name", name.into()),
                    ("short_id", short_id.clone().into()),
                    ("reason", reason.as_str().into()),
                ],
            );
            let hint = match reason {
                OrphanReason::SocketNull => format!(
                    ". Run 'claude attach {}' to wake the session, or 'fno agents rm {}' to remove",
                    short_id, name
                ),
                OrphanReason::NotFound => format!(". Run 'fno agents rm {}' to clear the stale entry", name),
                OrphanReason::LivenessFailed => format!(
                    ". Socket exists but is unresponsive; try 'claude attach {}' or 'fno agents rm {}'",
                    short_id, name
                ),
                // Unreachable here: RosterLiveInjectFailed always routes to the
                // no-stamp routing-gap branch above. Kept for exhaustiveness with
                // the same defensive inspect hint Python's dispatch.py `else` uses.
                OrphanReason::RosterLiveInjectFailed => format!(
                    ". Inspect with 'fno agents logs {}' or remove via 'fno agents rm {}'",
                    name, name
                ),
            };
            let suspended = if reason == OrphanReason::SocketNull {
                "; session is suspended"
            } else {
                ""
            };
            // Warning (if any) precedes the orphan error on stderr, matching
            // Python's two separate print() calls.
            AskOutcome {
                stdout: String::new(),
                stderr: format!(
                    "{}agent {} is not running (reason: {}{}){}\n",
                    stamp_warning,
                    py_repr(name),
                    reason.as_str(),
                    suspended,
                    hint
                ),
                exit_code: 13,
            }
        }
        Err(AskError::Socket { message }) => {
            emit_event(
                events,
                "agent_followup_failed",
                &[
                    ("stage", "send".into()),
                    ("name", name.into()),
                    ("short_id", short_id.clone().into()),
                    ("reason", "socket-error".into()),
                ],
            );
            AskOutcome::err(message, 1)
        }
        Err(AskError::Timeout { elapsed_sec, .. }) => {
            emit_event(
                events,
                "agent_followup_failed",
                &[
                    ("stage", "poll-timeout".into()),
                    ("name", name.into()),
                    ("short_id", short_id.clone().into()),
                    ("elapsed_sec", (elapsed_sec as u64).into()),
                ],
            );
            AskOutcome::err(
                format!(
                    "message sent but no reply within {}s. Try 'fno agents logs {}' to read the transcript.",
                    elapsed_sec as u64, name
                ),
                15,
            )
        }
        // ask_followup raises Orphan/Socket/Timeout, plus Io for a fatal
        // state.json read fault (EACCES/EROFS). Io and any other variant map
        // to exit 1 (Python's uncaught-OSError path also exits 1).
        Err(other) => AskOutcome::err(other.to_string(), 1),
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn create(
    home: &AgentsHome,
    claude_home: &ClaudeHome,
    events: &Path,
    registry_path: &Path,
    name: &str,
    message: &str,
    _from_name: &str,
    cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
    extra_env: &[(&str, &str)],
    model: Option<&str>,
    // x-dfa4: the already-resolved permission mode (dispatch_claude_spawn folds
    // --yolo -> bypassPermissions before calling); None = the claude default.
    permission_mode: Option<&str>,
    effort: Option<&str>,
    flags: HarnessFlags,
) -> AskOutcome {
    let pre_stderr = String::new();

    // Python passes the raw CLI timeout (None when --timeout unset) to
    // bg_create, so an unset timeout means NO SIGKILL deadline on the
    // claude --bg create. Pass it through unchanged (don't default to 600s).
    let result = match bg_create(
        name,
        message,
        cwd,
        timeout,
        extra_env,
        model,
        permission_mode,
        effort,
        flags,
    ) {
        Ok(r) => r,
        Err(AskError::Subprocess { exit_code, stderr }) => {
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", "subprocess".into()),
                    ("name", name.into()),
                    ("provider", "claude".into()),
                    ("returncode", exit_code.into()),
                ],
            );
            // A 127 from bg_create means the `claude` binary was not on PATH
            // (spawn NotFound). Python checks provider availability before spawn
            // and exits 14 for that config error, distinct from exit 1 for a
            // `claude --bg` process that ran and failed (Codex P2). All other
            // subprocess failures collapse to exit 1, surfacing stderr.
            let code = if exit_code == 127 { 14 } else { 1 };
            return AskOutcome {
                stdout: String::new(),
                stderr: format!("{}{}\n", pre_stderr, stderr),
                exit_code: code,
            };
        }
        Err(AskError::Parse { stdout_head }) => {
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", "parse".into()),
                    ("name", name.into()),
                    ("provider", "claude".into()),
                    ("short_id_raw", stdout_head.clone().into()),
                ],
            );
            return AskOutcome {
                stdout: String::new(),
                stderr: format!(
                    "{}unable to parse short-id from claude --bg output: {}\n",
                    pre_stderr, stdout_head
                ),
                exit_code: 1,
            };
        }
        Err(other) => {
            return AskOutcome {
                stdout: String::new(),
                stderr: format!("{}{}\n", pre_stderr, other),
                exit_code: 1,
            };
        }
    };

    let short_id = result.short_id.clone();
    // Best-effort full session-UUID capture (ab-f1b0ccd1, AC1-HP): persist the
    // stream-json `--resume` target alongside the 8-hex short-id so the worker
    // is adoptable by the live `chat` lane. Runs after the receipt is captured;
    // a miss leaves the field None and never gates the launch. This is the Rust
    // (default installed) path's parity with providers/claude.py's resolution.
    let session_uuid = resolve_session_uuid_at_spawn(claude_home, &short_id);
    let log_path = derive_log_path(home, name);
    let new_entry = RegistryEntry {
        name: name.to_string(),
        short_id: String::new(),
        provider: "claude".to_string(),
        cwd: cwd.to_string_lossy().to_string(),
        project_root: String::new(),
        session_id: None,
        claude_short_id: Some(short_id.clone()),
        claude_session_uuid: session_uuid,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        host_mode: None, // claude ask = exec/shellout (not an interactive host)
        cc_session_id: None,
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: now_iso(),
        pid: None,
        pid_start_time: None,
        log_path: Some(log_path.to_string_lossy().to_string()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
    };

    // Re-check the name UNDER the registry lock before appending. The per-agent
    // flock serializes two Rust-client creates, but a concurrent daemon-PTY
    // create (forced-Rust run) uses a different lock domain and could insert the
    // same name while `claude --bg` was running. The daemon's spawn re-checks
    // under the registry lock; this closure must too, or we leave duplicate
    // rows that make later find(name) ambiguous (Codex P2). Returns Ok(true) on
    // append, Ok(false) on a collision.
    match update_registry(registry_path, |reg| {
        if reg.find(name).is_some() {
            false
        } else {
            reg.entries.push(new_entry.clone());
            true
        }
    }) {
        Ok(true) => {}
        Ok(false) => {
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", "name-collision".into()),
                    ("name", name.into()),
                    ("provider", "claude".into()),
                    ("short_id", short_id.clone().into()),
                ],
            );
            return AskOutcome {
                stdout: String::new(),
                stderr: format!(
                    "{}agent {} already exists (registered concurrently); orphaned supervisor session: claude rm {} (registry not updated)\n",
                    pre_stderr,
                    py_repr(name),
                    short_id
                ),
                exit_code: 12,
            };
        }
        Err(_) => {
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-write".into()),
                    ("name", name.into()),
                    ("provider", "claude".into()),
                    ("short_id", short_id.clone().into()),
                ],
            );
            return AskOutcome {
                stdout: String::new(),
                stderr: format!(
                    "{}registry write failed. orphaned supervisor session: claude rm {} (registry not updated)\n",
                    pre_stderr, short_id
                ),
                exit_code: 12,
            };
        }
    }

    emit_event(
        events,
        "agent_ask_done",
        &[
            ("stage", "dispatch".into()),
            ("name", name.into()),
            ("provider", "claude".into()),
            ("short_id", short_id.clone().into()),
            ("duration_ms", (result.duration_ms as u64).into()),
            ("yolo", yolo.into()),
        ],
    );
    // AC1-UI: stdout is exactly `<short_id>\n`.
    AskOutcome {
        stdout: format!("{}\n", short_id),
        stderr: pre_stderr,
        exit_code: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // A spawn must NEVER hand bg_create a `None` timeout: the create wait then
    // falls into the unbounded `rx.recv()` arm and a `claude --bg` that holds
    // its inherited stdout/stderr pipe fds open hangs the dispatcher forever
    // (the motivating incident). spawn_create_timeout guarantees a bound.
    #[test]
    fn spawn_create_timeout_defaults_when_unset() {
        assert_eq!(spawn_create_timeout(None), DEFAULT_SPAWN_TIMEOUT);
    }

    // An explicit --timeout still wins over the default.
    #[test]
    fn spawn_create_timeout_honors_explicit() {
        let explicit = Duration::from_secs(5);
        assert_eq!(spawn_create_timeout(Some(explicit)), explicit);
    }

    // --- is_provably_live_report (x-c393) ----------------------------------

    fn report_at(stamp: &str) -> crate::state::InsideLegReport {
        crate::state::InsideLegReport {
            state: crate::state::InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: stamp.into(),
            ttl_ms: None,
        }
    }

    #[test]
    fn provably_live_true_for_recent_inside_leg_report() {
        // AC2-HP: a live worker (recent report) is not orphaned on a routing miss.
        let stamp = "2026-07-06T20:00:00Z";
        let now = crate::state::rfc3339_like_to_secs(stamp).unwrap() + 30;
        assert!(is_provably_live_report(Some(&report_at(stamp)), now));
    }

    #[test]
    fn not_provably_live_without_inside_leg_report() {
        // No liveness signal -> a routing failure is a real orphan (AC2-ERR side).
        assert!(!is_provably_live_report(None, 9_999_999_999));
    }

    #[test]
    fn not_provably_live_when_inside_leg_is_stale() {
        // A report older than the window is not a liveness signal.
        let stamp = "2026-07-06T20:00:00Z";
        let now =
            crate::state::rfc3339_like_to_secs(stamp).unwrap() + PROVABLY_LIVE_WINDOW_SECS + 60;
        assert!(!is_provably_live_report(Some(&report_at(stamp)), now));
    }

    #[test]
    fn not_provably_live_for_future_stamp() {
        // codex P3: a future/corrupt stamp must not count as recent.
        let stamp = "2026-07-06T20:00:00Z";
        let now = crate::state::rfc3339_like_to_secs(stamp).unwrap() - 60;
        assert!(!is_provably_live_report(Some(&report_at(stamp)), now));
    }

    // PR #544 deeper fix (codex P1): the create wait returns on the launch
    // confirmation line, not at stdout EOF. A confirmation line yields the id.
    #[test]
    fn scan_returns_short_id_on_confirmation_line() {
        let input = "backgrounded \u{b7} abcd1234 \u{b7} my-worker\n";
        match scan_stdout_for_short_id(std::io::Cursor::new(input)) {
            ShortIdScan::Found { short_id, .. } => assert_eq!(short_id, "abcd1234"),
            ShortIdScan::NoId { .. } => panic!("expected Found"),
        }
    }

    // The scan must STOP at the confirmation line -- lines after it are never
    // read (in production they never arrive; the detached agent holds the pipe
    // open). Proven by the post-confirmation sentinel being absent from consumed.
    #[test]
    fn scan_stops_at_confirmation_does_not_drain_to_eof() {
        let input = "warming up\nbackgrounded \u{b7} 0011aabb \u{b7} w\nSENTINEL_AFTER_ID\n";
        match scan_stdout_for_short_id(std::io::Cursor::new(input)) {
            ShortIdScan::Found { short_id, consumed } => {
                assert_eq!(short_id, "0011aabb");
                assert!(consumed.contains("warming up"));
                assert!(
                    !consumed.contains("SENTINEL_AFTER_ID"),
                    "scan read past the confirmation line: {consumed:?}"
                );
            }
            ShortIdScan::NoId { .. } => panic!("expected Found"),
        }
    }

    // No confirmation anywhere -> NoId at EOF; the caller then reaps the exit code
    // and surfaces a precise failure rather than a fabricated success.
    #[test]
    fn scan_no_confirmation_is_noid_at_eof() {
        let input = "error: could not start\ngiving up\n";
        match scan_stdout_for_short_id(std::io::Cursor::new(input)) {
            ShortIdScan::NoId { consumed } => assert!(consumed.contains("giving up")),
            ShortIdScan::Found { .. } => panic!("expected NoId"),
        }
    }

    // A colorized confirmation line (claude wraps the id in ANSI when stdout is
    // colorized) still matches -- the scan reuses match_short_id's ANSI strip.
    #[test]
    fn scan_matches_colorized_confirmation_line() {
        let input = "backgrounded \u{b7} \u{1b}[36mdeadbeef\u{1b}[39m \u{b7} w\n";
        match scan_stdout_for_short_id(std::io::Cursor::new(input)) {
            ShortIdScan::Found { short_id, .. } => assert_eq!(short_id, "deadbeef"),
            ShortIdScan::NoId { .. } => panic!("expected Found on colorized line"),
        }
    }

    // READINESS HANDSHAKE (read before adding a socket/timing test here)
    // ------------------------------------------------------------------
    // These tests stand up a real AF_UNIX listener and a polled state.json to
    // drive ask_followup / wait_for_reply without a live `claude --bg` daemon.
    // They run multi-threaded under the default `cargo test` harness and have
    // to stay green even while a concurrent `cargo build` saturates every core.
    //
    // The rule: NEVER use a fixed `std::thread::sleep(...)` as a "the other side
    // is ready now" barrier. A sleep sized for an idle box (e.g. 60ms) becomes a
    // coin flip under load because wall-clock stretches relative to scheduled
    // work, so the awaited write/accept/join can miss the budget. That is the
    // exact flake this module was de-flaked to remove.
    //
    // Use an OBSERVED readiness signal instead, in order of preference:
    //  1. Restructure so the awaited state is already on disk before the call
    //     (wait_for_reply checks state before it sleeps, so a pre-written
    //     terminal state returns on the first poll iteration). Deterministic.
    //  2. Drive the state transition off RECEIVED bytes (the socket accept
    //     thread reacts to the send, not a timer) and join the thread to
    //     synchronize. Deterministic on the send side.
    //  3. Only as a last resort, widen a TEST-LOCAL poll/deadline budget as
    //     defense-in-depth -- generous enough to absorb scheduling latency,
    //     bounded enough that a real hang still fails in seconds (never
    //     minutes). Never widen production constants.

    fn tmpdir() -> PathBuf {
        // Unique per call BY CONSTRUCTION. The pid is identical for every test
        // in a single test binary, and `as_nanos()` is only as fine-grained as
        // the OS clock -- on a coarse clock under parallel load two tests can
        // read the SAME nanos and collide on this path, mixing their session
        // files into one dir. That collision was an observed load-flake: a
        // locate_session test would read another test's socket path. The
        // process-wide atomic sequence makes the path collision-proof regardless
        // of clock resolution or scheduling, so every socket/timing test gets an
        // isolated tree even under a saturating concurrent build.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "abi-claude-ask-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            seq
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    // --- parse_short_id ---

    #[test]
    fn parse_short_id_happy() {
        let out = "backgrounded \u{b7} 7c5dcf5d \u{b7} alice\n";
        assert_eq!(parse_short_id(out).unwrap(), "7c5dcf5d");
    }

    #[test]
    fn parse_short_id_only_first_line() {
        let out = "backgrounded \u{b7} 7c5dcf5d \u{b7} alice\nextra garbage\n";
        assert_eq!(parse_short_id(out).unwrap(), "7c5dcf5d");
    }

    #[test]
    fn parse_short_id_empty_is_error() {
        assert!(matches!(parse_short_id(""), Err(AskError::Parse { .. })));
    }

    #[test]
    fn parse_short_id_strips_ansi_color() {
        // Regression: the installed `claude --bg` wraps the short-id in SGR
        // color codes (`backgrounded · \x1b[36m<id>\x1b[39m · <name>`), which
        // the matcher used to reject (leading ESC is not a hexdigit). The id
        // must survive the colorization.
        let out = "backgrounded \u{b7} \u{1b}[36m441064a2\u{1b}[39m \u{b7} abigates\n";
        assert_eq!(parse_short_id(out).unwrap(), "441064a2");
    }

    #[test]
    fn parse_short_id_strips_truecolor_sgr() {
        // Truecolor SGR uses `;`-separated params (0x3B); the CSI stripper must
        // consume the whole parameter run, not just a single byte.
        let out = "backgrounded \u{b7} \u{1b}[38;2;215;119;87m7c5dcf5d\u{1b}[0m \u{b7} alice";
        assert_eq!(parse_short_id(out).unwrap(), "7c5dcf5d");
    }

    #[test]
    fn strip_ansi_csi_borrows_when_no_escape() {
        // Common path (no ESC) is zero-copy (gemini PR #403 review); a line
        // carrying CSI codes allocates and drops the escapes.
        assert!(matches!(
            strip_ansi_csi("backgrounded \u{b7} 7c5dcf5d \u{b7} alice"),
            std::borrow::Cow::Borrowed(_)
        ));
        let owned = strip_ansi_csi("a\u{1b}[31mb\u{1b}[0mc");
        assert!(matches!(owned, std::borrow::Cow::Owned(_)));
        assert_eq!(owned.as_ref(), "abc");
    }

    #[test]
    fn parse_short_id_no_panic_on_non_ascii_at_byte_8() {
        // Codex P2: a multibyte char straddling byte 8 used to panic split_at(8).
        // 'é' (2 bytes) at rest-bytes 7-8 makes byte 8 a non-char-boundary.
        let out = "backgrounded \u{b7} 1234567\u{e9} \u{b7} x";
        // Must return Err, not panic.
        assert!(parse_short_id(out).is_err());
    }

    #[test]
    fn parse_short_id_rejects_non_hex_and_uppercase() {
        assert!(parse_short_id("backgrounded \u{b7} 7C5DCF5D \u{b7} a").is_err());
        assert!(parse_short_id("backgrounded \u{b7} zzzzzzzz \u{b7} a").is_err());
        assert!(parse_short_id("nope \u{b7} 7c5dcf5d \u{b7} a").is_err());
        assert!(parse_short_id("backgrounded \u{b7} 7c5dcf5d done").is_err());
    }

    // --- build_argv / use_stdin_for ---

    #[test]
    fn build_argv_inline_vs_stdin() {
        assert_eq!(
            build_argv("a", "hi", false, None, None, None, HarnessFlags::default()),
            vec!["claude", "--bg", "--name", "a", "hi"]
        );
        assert_eq!(
            build_argv("a", "hi", true, None, None, None, HarnessFlags::default()),
            vec!["claude", "--bg", "--name", "a"]
        );
    }

    // x-dfa4: an explicit --permission-mode rides between --name and --model as
    // an exact passthrough; empty/None is byte-identical to today (AC1-HP/AC7).
    #[test]
    fn build_argv_appends_permission_mode() {
        assert_eq!(
            build_argv("a", "hi", false, None, Some("acceptEdits"), None, HarnessFlags::default()),
            vec![
                "claude",
                "--bg",
                "--name",
                "a",
                "--permission-mode",
                "acceptEdits",
                "hi"
            ]
        );
        // Empty mode == unset: no flag, byte-identical to the None case (AC7).
        assert_eq!(
            build_argv("a", "hi", false, None, Some(""), None, HarnessFlags::default()),
            build_argv("a", "hi", false, None, None, None, HarnessFlags::default())
        );
        // Does NOT stack with --dangerously-skip-permissions (bg never had it).
        assert!(
            !build_argv("a", "hi", false, None, Some("acceptEdits"), None, HarnessFlags::default())
                .iter()
                .any(|t| t == "--dangerously-skip-permissions")
        );
    }

    // x-571f: a per-node model pin appends `--model <m>` between --name and the
    // message; an empty/None pin is byte-identical to today (AC1-EDGE), and the
    // argv must match Python's `_build_argv` (AC2-FR parity).
    #[test]
    fn build_argv_appends_model_pin() {
        assert_eq!(
            build_argv("a", "hi", false, Some("fable"), None, None, HarnessFlags::default()),
            vec!["claude", "--bg", "--name", "a", "--model", "fable", "hi"]
        );
        assert_eq!(
            build_argv("a", "hi", true, Some("fable"), None, None, HarnessFlags::default()),
            vec!["claude", "--bg", "--name", "a", "--model", "fable"]
        );
        // Empty pin == unset: no flag, byte-identical to the None case.
        assert_eq!(
            build_argv("a", "hi", false, Some(""), None, None, HarnessFlags::default()),
            build_argv("a", "hi", false, None, None, None, HarnessFlags::default())
        );
    }

    #[test]
    fn build_argv_appends_effort() {
        assert_eq!(
            build_argv("a", "hi", false, None, None, Some("high"), HarnessFlags::default()),
            vec!["claude", "--bg", "--name", "a", "--effort", "high", "hi"]
        );
    }

    // x-b6e2: the Tier-3 passthrough bundle maps to claude's own spellings, in a
    // fixed order (--add-dir, --agent, --allowedTools, --disallowedTools), riding
    // after --effort and before the message. Empty/None fields are omitted. This
    // token order must match the Python _build_argv (AC2-EDGE parity).
    #[test]
    fn build_argv_appends_harness_flags() {
        let flags = HarnessFlags {
            add_dir: Some("/work"),
            agent: Some("reviewer"),
            allowed_tools: Some("Read,Edit"),
            disallowed_tools: Some("Bash"),
        };
        assert_eq!(
            build_argv("a", "hi", false, None, None, None, flags),
            vec![
                "claude",
                "--bg",
                "--name",
                "a",
                "--add-dir",
                "/work",
                "--agent",
                "reviewer",
                "--allowedTools",
                "Read,Edit",
                "--disallowedTools",
                "Bash",
                "hi"
            ]
        );
        // A partially-filled bundle emits only the set fields (empty == unset).
        let only_dir = HarnessFlags {
            add_dir: Some("/work"),
            allowed_tools: Some(""),
            ..Default::default()
        };
        assert_eq!(
            build_argv("a", "hi", false, None, None, None, only_dir),
            vec!["claude", "--bg", "--name", "a", "--add-dir", "/work", "hi"]
        );
    }

    #[test]
    fn use_stdin_threshold() {
        assert!(!use_stdin_for(&"x".repeat(ARGV_OVERFLOW_THRESHOLD)));
        assert!(use_stdin_for(&"x".repeat(ARGV_OVERFLOW_THRESHOLD + 1)));
    }

    // --- envelope byte-parity ---

    #[test]
    fn envelope_exact_bytes_ascii() {
        let env = build_envelope("hello", "bob");
        let expected = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<cross-session-message from-name=\\\"bob\\\">\\nhello\\n</cross-session-message>\"},\"priority\":\"next\"}\n";
        assert_eq!(String::from_utf8(env).unwrap(), expected);
    }

    #[test]
    fn envelope_escapes_from_name_html() {
        let env = build_envelope("hi", "a&b<c>\"d'e");
        let s = String::from_utf8(env).unwrap();
        assert!(
            s.contains("from-name=\\\"a&amp;b&lt;c&gt;&quot;d&#x27;e\\\""),
            "{}",
            s
        );
    }

    #[test]
    fn envelope_ensure_ascii_non_ascii() {
        // café -> café in the JSON string, matching Python ensure_ascii.
        let env = build_envelope("caf\u{e9}", "x");
        let s = String::from_utf8(env).unwrap();
        assert!(s.contains("caf\\u00e9"), "{}", s);
        assert!(!s.contains('\u{e9}'), "raw non-ascii leaked: {}", s);
    }

    #[test]
    fn envelope_astral_surrogate_pair() {
        // U+1F600 grinning face
        let env = build_envelope("\u{1F600}", "x");
        let s = String::from_utf8(env).unwrap();
        assert!(s.contains("\\ud83d\\ude00"), "{}", s);
    }

    #[test]
    fn json_string_escapes_control_chars() {
        assert_eq!(json_string_ascii("a\nb\tc"), "\"a\\nb\\tc\"");
        assert_eq!(json_string_ascii("\u{01}"), "\"\\u0001\"");
        assert_eq!(json_string_ascii("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    // --- locate_session ---

    fn write_session(dir: &Path, pid: &str, job: &str, kind: &str, sock: Option<&str>) {
        let sock_field = match sock {
            Some(s) => format!("\"{}\"", s),
            None => "null".to_string(),
        };
        let body = format!(
            "{{\"jobId\":\"{}\",\"kind\":\"{}\",\"messagingSocketPath\":{},\"sessionId\":\"sess-{}\",\"cwd\":\"/tmp\"}}",
            job, kind, sock_field, job
        );
        fs::write(dir.join(format!("{}.json", pid)), body).unwrap();
    }

    #[test]
    fn locate_session_happy() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(&sessions, "111", "7c5dcf5d", "bg", Some("/tmp/sock1"));
        let ch = ClaudeHome::at(&home);
        let loc = locate_session(&ch, "7c5dcf5d").unwrap();
        assert_eq!(loc.pid, 111);
        assert_eq!(loc.messaging_socket_path, "/tmp/sock1");
        assert_eq!(loc.session_id.as_deref(), Some("sess-7c5dcf5d"));
        assert_eq!(
            loc.jobs_dir,
            home.join(".claude").join("jobs").join("7c5dcf5d")
        );
    }

    #[test]
    fn locate_session_skips_null_socket_prefers_live() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        // dead pid with null socket sorts before the live one
        write_session(&sessions, "100", "abcd1234", "bg", None);
        write_session(&sessions, "200", "abcd1234", "bg", Some("/tmp/live"));
        let ch = ClaudeHome::at(&home);
        let loc = locate_session(&ch, "abcd1234").unwrap();
        assert_eq!(loc.messaging_socket_path, "/tmp/live");
        assert_eq!(loc.pid, 200);
    }

    #[test]
    fn locate_session_not_found_and_classify() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(&sessions, "100", "abcd1234", "bg", None);
        let ch = ClaudeHome::at(&home);
        assert!(locate_session(&ch, "abcd1234").is_none());
        assert_eq!(
            classify_orphan_reason(&ch, "abcd1234"),
            OrphanReason::SocketNull
        );
        assert_eq!(
            classify_orphan_reason(&ch, "ffffffff"),
            OrphanReason::NotFound
        );
    }

    #[test]
    fn locate_session_skips_corrupt_and_non_bg() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("1.json"), "{not json").unwrap();
        write_session(&sessions, "2", "abcd1234", "interactive", Some("/tmp/x"));
        write_session(&sessions, "3", "abcd1234", "bg", Some("/tmp/good"));
        let ch = ClaudeHome::at(&home);
        let loc = locate_session(&ch, "abcd1234").unwrap();
        assert_eq!(loc.messaging_socket_path, "/tmp/good");
    }

    #[test]
    fn locate_session_missing_dir_is_none() {
        let home = tmpdir();
        let ch = ClaudeHome::at(&home);
        assert!(locate_session(&ch, "abcd1234").is_none());
    }

    // --- resolve_session_uuid / resolve_session_uuid_at_spawn (ab-f1b0ccd1) ---

    #[test]
    fn resolve_session_uuid_resolves_idle_bg() {
        // Unlike locate_session, resolution does NOT require a live socket: an
        // idle (socket-null) bg session is exactly the resume target.
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(&sessions, "111", "7c5dcf5d", "bg", None); // null socket
        let ch = ClaudeHome::at(&home);
        assert!(locate_session(&ch, "7c5dcf5d").is_none()); // socket-null: locate misses
        assert_eq!(
            resolve_session_uuid(&ch, "7c5dcf5d").as_deref(),
            Some("sess-7c5dcf5d") // ... but resolve still returns the sessionId
        );
    }

    #[test]
    fn resolve_session_uuid_skips_non_bg_and_unmatched() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(&sessions, "1", "7c5dcf5d", "interactive", Some("/tmp/x")); // wrong kind
        write_session(&sessions, "2", "deadbeef", "bg", Some("/tmp/y")); // wrong jobId
        let ch = ClaudeHome::at(&home);
        assert!(resolve_session_uuid(&ch, "7c5dcf5d").is_none());
        assert!(resolve_session_uuid(&ch, "ffffffff").is_none());
    }

    #[test]
    fn resolve_session_uuid_missing_dir_is_none() {
        let home = tmpdir();
        let ch = ClaudeHome::at(&home);
        assert!(resolve_session_uuid(&ch, "7c5dcf5d").is_none());
    }

    #[test]
    fn resolve_at_spawn_happy_empty_and_missing() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(&sessions, "111", "7c5dcf5d", "bg", Some("/tmp/sock"));
        let ch = ClaudeHome::at(&home);
        // happy: first probe hits, no sleep
        assert_eq!(
            resolve_session_uuid_at_spawn(&ch, "7c5dcf5d").as_deref(),
            Some("sess-7c5dcf5d")
        );
        // empty short-id short-circuits (no probe)
        assert!(resolve_session_uuid_at_spawn(&ch, "").is_none());
        // absent sessions dir short-circuits without retrying (no sleep)
        let empty = tmpdir();
        let empty_ch = ClaudeHome::at(&empty);
        assert!(resolve_session_uuid_at_spawn(&empty_ch, "7c5dcf5d").is_none());
    }

    // --- read_state_json ---

    #[test]
    fn read_state_json_parses_fields() {
        let jobs = tmpdir();
        fs::write(
            jobs.join("state.json"),
            r#"{"state":"completed","updatedAt":"2026-05-27T10:00:00Z","output":{"result":"PONG"},"intent":"reply"}"#,
        )
        .unwrap();
        let snap = read_state_json(&jobs).unwrap();
        assert_eq!(snap.state, "completed");
        assert_eq!(snap.updated_at.as_deref(), Some("2026-05-27T10:00:00Z"));
        assert_eq!(snap.output_result.as_deref(), Some("PONG"));
    }

    #[test]
    fn read_state_json_missing_is_notfound() {
        let jobs = tmpdir();
        assert!(matches!(
            read_state_json(&jobs),
            Err(StateReadError::NotFound)
        ));
    }

    #[test]
    fn read_state_json_empty_is_parse_err() {
        let jobs = tmpdir();
        fs::write(jobs.join("state.json"), "   ").unwrap();
        assert!(matches!(read_state_json(&jobs), Err(StateReadError::Parse)));
    }

    #[test]
    fn read_state_json_output_not_dict() {
        let jobs = tmpdir();
        fs::write(jobs.join("state.json"), r#"{"state":"done","output":null}"#).unwrap();
        let snap = read_state_json(&jobs).unwrap();
        assert_eq!(snap.output_result, None);
    }

    // --- read_timeline_tail ---

    #[test]
    fn timeline_tail_concats_terminal_text_from_offset() {
        let jobs = tmpdir();
        let tl = jobs.join("timeline.jsonl");
        // pre-baseline content that must be ignored
        fs::write(&tl, "{\"state\":\"completed\",\"text\":\"OLD\"}\n").unwrap();
        let offset = timeline_offset(&jobs);
        // appended after baseline
        let mut f = fs::OpenOptions::new().append(true).open(&tl).unwrap();
        writeln!(f, "{{\"state\":\"running\",\"text\":\"tool call\"}}").unwrap();
        writeln!(f, "{{\"state\":\"completed\",\"text\":\"AB\"}}").unwrap();
        writeln!(f, "{{\"state\":\"done\",\"text\":\"CD\"}}").unwrap();
        writeln!(f, "not json").unwrap();
        assert_eq!(read_timeline_tail(&jobs, offset), "ABCD");
    }

    #[test]
    fn timeline_tail_missing_is_empty() {
        let jobs = tmpdir();
        assert_eq!(read_timeline_tail(&jobs, 0), "");
    }

    // --- socket round-trip ---

    #[test]
    fn send_to_session_delivers_envelope_bytes() {
        use std::os::unix::net::UnixListener;
        let dir = tmpdir();
        let sock = dir.join("s.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let sock_str = sock.to_str().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            conn.read_to_end(&mut buf).unwrap();
            buf
        });
        send_to_session(&sock_str, "ping", "tester").unwrap();
        let got = handle.join().unwrap();
        assert_eq!(got, build_envelope("ping", "tester"));
    }

    #[test]
    fn liveness_probe_true_when_listening_false_when_absent() {
        use std::os::unix::net::UnixListener;
        let dir = tmpdir();
        let sock = dir.join("live.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        assert!(liveness_probe(sock.to_str().unwrap()));
        assert!(!liveness_probe(dir.join("absent.sock").to_str().unwrap()));
    }

    #[test]
    fn send_to_session_errors_on_missing_socket() {
        let dir = tmpdir();
        let res = send_to_session(dir.join("nope.sock").to_str().unwrap(), "x", "y");
        assert!(matches!(res, Err(AskError::Socket { .. })));
    }

    // --- wait_for_reply ---

    fn write_state(jobs: &Path, state: &str, updated: &str, result: Option<&str>) {
        let res = match result {
            Some(r) => format!(",\"output\":{{\"result\":{}}}", json_string_ascii(r)),
            None => String::new(),
        };
        fs::write(
            jobs.join("state.json"),
            format!(
                "{{\"state\":\"{}\",\"updatedAt\":\"{}\"{}}}",
                state, updated, res
            ),
        )
        .unwrap();
    }

    #[test]
    fn wait_for_reply_prefers_output_result() {
        let jobs = tmpdir();
        write_state(&jobs, "completed", "2026-05-27T10:00:01Z", Some("PONG"));
        let r = wait_for_reply(
            &jobs,
            Some("2026-05-27T10:00:00Z"),
            0,
            Duration::from_secs(2),
            Duration::from_millis(10),
            "sid",
        )
        .unwrap();
        assert_eq!(r, "PONG");
    }

    #[test]
    fn wait_for_reply_baseline_invariant_then_advance() {
        // Deterministic by construction: no fixed-sleep barrier, no writer
        // thread racing a poll deadline (see READINESS HANDSHAKE note at the top
        // of this module). The old version spawned a thread that slept 60ms then
        // wrote the advance; under CPU saturation that 60ms-then-scheduled write
        // could miss the budget. We instead assert the two properties the test
        // name promises in sequence:
        let jobs = tmpdir();

        // (1) baseline invariant: a terminal state whose updatedAt EQUALS the
        // baseline has NOT advanced, so wait_for_reply must not return it. With
        // nothing ever advancing it, the call deterministically times out within
        // a short bounded budget (the outcome is the error *type*, independent of
        // wall-clock under load).
        write_state(&jobs, "completed", "2026-05-27T10:00:00Z", Some("STALE"));
        let r = wait_for_reply(
            &jobs,
            Some("2026-05-27T10:00:00Z"),
            0,
            Duration::from_millis(200),
            Duration::from_millis(10),
            "sid",
        );
        assert!(
            matches!(r, Err(AskError::Timeout { .. })),
            "stale state equal to baseline must not satisfy wait_for_reply; got {:?}",
            r
        );

        // (2) then advance: once updatedAt moves past the baseline, the state is
        // already terminal+advanced on disk before the call, so wait_for_reply
        // returns it on the FIRST poll iteration without sleeping a poll_interval
        // (the loop checks state before it sleeps). This is the zero-delay reply
        // path (AC1-EDGE) and is load-proof: the value is present before we poll.
        write_state(&jobs, "completed", "2026-05-27T10:00:05Z", Some("FRESH"));
        let r = wait_for_reply(
            &jobs,
            Some("2026-05-27T10:00:00Z"),
            0,
            Duration::from_secs(2),
            Duration::from_millis(10),
            "sid",
        )
        .unwrap();
        assert_eq!(r, "FRESH");
    }

    #[test]
    fn wait_for_reply_falls_back_to_timeline_when_result_empty() {
        let jobs = tmpdir();
        let offset = timeline_offset(&jobs); // 0, no file yet
        write_state(&jobs, "done", "2026-05-27T10:00:01Z", None);
        fs::write(
            jobs.join("timeline.jsonl"),
            "{\"state\":\"done\",\"text\":\"TAIL\"}\n",
        )
        .unwrap();
        let r = wait_for_reply(
            &jobs,
            None,
            offset,
            Duration::from_secs(2),
            Duration::from_millis(10),
            "sid",
        )
        .unwrap();
        assert_eq!(r, "TAIL");
    }

    #[test]
    fn read_state_json_eacces_is_fatal_io_not_transient() {
        // EACCES must surface as Io (fatal), not be masked as a transient Parse
        // that the poll loop spins on (Python lets the OSError propagate).
        // Skip as root (root bypasses permission bits).
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("SKIP: running as root; permission bits not enforced");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let jobs = tmpdir();
        let sp = jobs.join("state.json");
        fs::write(&sp, r#"{"state":"done","updatedAt":"t"}"#).unwrap();
        fs::set_permissions(&sp, fs::Permissions::from_mode(0o000)).unwrap();
        let got = read_state_json(&jobs);
        // restore so tmpdir cleanup is unhindered
        let _ = fs::set_permissions(&sp, fs::Permissions::from_mode(0o644));
        assert!(
            matches!(got, Err(StateReadError::Io(_))),
            "expected Io, got {:?}",
            got
        );

        // and wait_for_reply turns it into a fatal AskError::Io, not a 600s spin
        fs::set_permissions(&sp, fs::Permissions::from_mode(0o000)).unwrap();
        let r = wait_for_reply(
            &jobs,
            None,
            0,
            Duration::from_secs(30),
            Duration::from_millis(10),
            "sid",
        );
        let _ = fs::set_permissions(&sp, fs::Permissions::from_mode(0o644));
        assert!(
            matches!(r, Err(AskError::Io { .. })),
            "expected fatal Io, got {:?}",
            r
        );
    }

    #[test]
    fn wait_for_reply_times_out() {
        let jobs = tmpdir();
        write_state(&jobs, "running", "2026-05-27T10:00:00Z", None);
        let r = wait_for_reply(
            &jobs,
            None,
            0,
            Duration::from_millis(40),
            Duration::from_millis(10),
            "sid",
        );
        assert!(matches!(r, Err(AskError::Timeout { .. })));
    }

    // --- ask_followup (live socket + state.json) ---

    #[test]
    fn ask_followup_socket_to_reply() {
        use std::os::unix::net::UnixListener;
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        let jobs = home.join(".claude").join("jobs").join("abcd1234");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&jobs).unwrap();
        let sock = home.join("msg.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        write_session(
            &sessions,
            "999",
            "abcd1234",
            "bg",
            Some(sock.to_str().unwrap()),
        );

        let jobs_for_thread = jobs.clone();
        let handle = std::thread::spawn(move || {
            // ask_followup connects twice: a liveness probe (no bytes) then the
            // send. Accept until we get the connection carrying the envelope.
            loop {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let _ = conn.read_to_end(&mut buf);
                if buf.is_empty() {
                    continue; // liveness probe; wait for the real send
                }
                write_state(
                    &jobs_for_thread,
                    "completed",
                    "2026-05-27T10:00:09Z",
                    Some("REPLY!"),
                );
                break buf;
            }
        });

        let ch = ClaudeHome::at(&home);
        // Readiness model (no fixed-sleep barrier; see the note at the top of
        // this module):
        //  - The listener is bound BEFORE ask_followup runs, so the client's
        //    liveness probe and send connect into a ready accept backlog; a
        //    transient refused connect is not possible here (AC1-ERR holds by
        //    construction, not by a retry sleep).
        //  - The accept thread writes the terminal state.json in reaction to the
        //    RECEIVED send bytes (a real handshake), not a timer.
        //  - The only cross-thread observation is wait_for_reply polling for that
        //    state write. We give it a generous-but-bounded budget (10s) so a
        //    late-scheduled accept thread under a saturating `cargo build` is
        //    still observed, while a genuine hang still fails in seconds, not
        //    minutes (AC3-ERR). This budget is test-local; production constants
        //    are unchanged.
        let reply = ask_followup(
            &ch,
            "abcd1234",
            "ping",
            "tester",
            Duration::from_secs(10),
            Duration::from_millis(10),
            None,
        )
        .unwrap();
        let envelope = handle.join().unwrap();
        assert_eq!(reply, "REPLY!");
        assert_eq!(envelope, build_envelope("ping", "tester"));
    }

    #[test]
    fn ask_followup_orphan_socket_null() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(&sessions, "1", "abcd1234", "bg", None);
        let ch = ClaudeHome::at(&home);
        let err = ask_followup(
            &ch,
            "abcd1234",
            "x",
            "y",
            Duration::from_secs(1),
            Duration::from_millis(10),
            None,
        )
        .unwrap_err();
        match err {
            AskError::Orphan { reason, .. } => assert_eq!(reason, OrphanReason::SocketNull),
            other => panic!("expected orphan, got {:?}", other),
        }
    }

    #[test]
    fn ask_followup_orphan_not_found() {
        let home = tmpdir();
        fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
        let ch = ClaudeHome::at(&home);
        let err = ask_followup(
            &ch,
            "ffffffff",
            "x",
            "y",
            Duration::from_secs(1),
            Duration::from_millis(10),
            None,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            AskError::Orphan {
                reason: OrphanReason::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn ask_followup_liveness_failed_when_socket_dead() {
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        // points at a socket path that has no listener
        write_session(
            &sessions,
            "1",
            "abcd1234",
            "bg",
            Some(home.join("dead.sock").to_str().unwrap()),
        );
        let ch = ClaudeHome::at(&home);
        let err = ask_followup(
            &ch,
            "abcd1234",
            "x",
            "y",
            Duration::from_secs(1),
            Duration::from_millis(10),
            None,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            AskError::Orphan {
                reason: OrphanReason::LivenessFailed,
                ..
            }
        ));
    }

    // --- x-2681 ask-lane control.sock fallback ---

    // daemon_roster_path reads FNO_CLAUDE_DAEMON_DIR, a process-global. Serialize
    // the env-touching tests below (cargo runs tests in parallel threads; no
    // serial_test dep in this crate) so they never observe each other's mutation.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_roster(home: &Path, session_uuid: &str) {
        let daemon = home.join(".claude").join("daemon");
        fs::create_dir_all(&daemon).unwrap();
        let body = format!(
            "{{\"workers\":{{\"w\":{{\"sessionId\":\"{}\",\"pid\":5}}}}}}",
            session_uuid
        );
        fs::write(daemon.join("roster.json"), body).unwrap();
    }

    #[test]
    fn build_cross_session_container_wraps_peer_turn() {
        // Byte-parity with Python's build_cross_session_container.
        assert_eq!(
            build_cross_session_container("hello", "fno"),
            "<cross-session-message from-name=\"fno\">\nhello\n</cross-session-message>"
        );
    }

    #[test]
    fn orphan_reason_roster_live_inject_failed_token() {
        assert_eq!(
            OrphanReason::RosterLiveInjectFailed.as_str(),
            "roster-live-inject-failed"
        );
    }

    #[test]
    fn daemon_roster_path_honors_env_override_first() {
        // x-2681 / codex P2: the roster pre-check must honor FNO_CLAUDE_DAEMON_DIR
        // (a supported alt-daemon override) the SAME way the deliver path does, or
        // the fallback silently skips in an alt-daemon setup.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tmpdir();
        let ch = ClaudeHome::at(&home);
        std::env::remove_var(crate::claude_roster::DAEMON_DIR_ENV);
        assert_eq!(
            ch.daemon_roster_path(),
            home.join(".claude").join("daemon").join("roster.json")
        );
        let alt = tmpdir();
        std::env::set_var(crate::claude_roster::DAEMON_DIR_ENV, &alt);
        assert_eq!(ch.daemon_roster_path(), alt.join("roster.json"));
        std::env::remove_var(crate::claude_roster::DAEMON_DIR_ENV);
    }

    #[test]
    fn ask_followup_socket_null_roster_live_falls_back_to_control_sock() {
        // A socket-null session that is present in the daemon roster takes the
        // control.sock fallback. With no real control.sock the deliver fails and
        // surfaces the DISTINCT reason (not socket-null) -- which the dispatch
        // layer routes to the no-stamp branch (AC6-FR: never orphan a live row).
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(crate::claude_roster::DAEMON_DIR_ENV);
        let home = tmpdir();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(&sessions, "1", "abcd1234", "bg", None);
        write_roster(&home, "abcd1234-1111-2222-3333-444455556666");
        let ch = ClaudeHome::at(&home);
        let err = ask_followup(
            &ch,
            "abcd1234",
            "x",
            "y",
            Duration::from_millis(200),
            Duration::from_millis(10),
            None,
        )
        .unwrap_err();
        match err {
            AskError::Orphan { reason, .. } => {
                assert_eq!(reason, OrphanReason::RosterLiveInjectFailed)
            }
            other => panic!("expected roster-live-inject-failed orphan, got {:?}", other),
        }
    }

    #[test]
    fn ask_followup_not_found_never_falls_back_even_if_rostered() {
        // No session file -> not-found. Even with a matching roster entry,
        // not-found is genuinely dead and never falls back (Locked Decision 5).
        let home = tmpdir();
        fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
        write_roster(&home, "abcd1234-1111-2222-3333-444455556666");
        let ch = ClaudeHome::at(&home);
        let err = ask_followup(
            &ch,
            "abcd1234",
            "x",
            "y",
            Duration::from_secs(1),
            Duration::from_millis(10),
            None,
        )
        .unwrap_err();
        match err {
            AskError::Orphan { reason, .. } => assert_eq!(reason, OrphanReason::NotFound),
            other => panic!("expected not-found orphan, got {:?}", other),
        }
    }
}
