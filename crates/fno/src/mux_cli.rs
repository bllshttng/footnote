//! The scriptable `fno mux` verbs: `ls`, `kill-server`, `shell-init`, `doctor`,
//! and the `pane` control-verb family. Plain client-process code - they run and
//! exit, no TUI, no raw mode, no attach - so every probe is bounded by a
//! read/write timeout and a bad session can never hang the listing (AC4-ERR).
//!
//! ## Exit-code table (US6, the one authority; asserted by tests)
//!
//! Every verb returns one of [`EXIT_OK`] (0), [`EXIT_ERROR`] (1, an io/dead/skew
//! failure), [`EXIT_USAGE`] (2, malformed args), or - for `pane wait`/`read` -
//! the distinct `EXIT_WAIT_*`/`EXIT_BLOCK_UNAVAILABLE` codes (10-14). The
//! constants below carry the canonical comment per code.
//!
//! ## `--json` envelope (US6; every verb accepts `--json`)
//!
//! Each verb emits a stable, documented JSON shape on stdout under `--json`;
//! errors stay one-line on stderr (never mixed into the json). The shapes:
//! - `ls`: array of `{session, state: live|stale|unqueryable|unprobeable,
//!   clients?, squads?, panes?, error?}` (`[]` when empty).
//! - `kill-server`: `{session, killed: true, note}` on success.
//! - `shell-init`: `{shell, snippet}` (the raw snippet without `--json`).
//! - `doctor`: `{ok: bool, checks: [{check, verdict: ok|warn|fail|n/a, detail,
//!   remedy}]}`.
//! - `pane ls|read|run|send|wait|kill|claim|release`: the per-reply shapes in
//!   [`render_reply`].

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::proto::{
    self, err_code, read_msg_sync, write_msg_sync, BlockSel, ClientMsg, ControlVerb, ServerMsg,
    WaitOutcome, BUILD_VERSION, DEFAULT_SESSION, PROTO_VERSION,
};

/// Bound every probe: a wedged server counts as alive-but-unqueryable, never
/// a hang. Generous next to a socket round-trip, tight next to a human.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolve the target session: explicit flag/arg > `FNO_SESSION` (set in
/// every pane the server spawns) > the default. Pure, so precedence is
/// unit-testable (Locked 7).
pub fn resolve_session(explicit: Option<&str>, env: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| env.filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_else(|| DEFAULT_SESSION.to_string())
}

/// What one socket probe learned.
enum Probe {
    /// The server answered `Query`.
    Live {
        clients: u32,
        squads: u32,
        panes: u32,
    },
    /// Something accepts connections but never answered a parseable `Info`
    /// (an older build, a wedged server): listed, never unlinked, and one
    /// bad session never breaks the listing (AC4-ERR).
    Unqueryable,
    /// Nothing listens: a leftover socket from a dead server.
    Stale,
    /// The probe itself failed CLIENT-side (fd exhaustion, permissions):
    /// says nothing about the server, so it must never read as `Stale` -
    /// "stale" steers the operator toward kill-server's unlink.
    Unprobeable(String),
}

fn probe(sock: &Path) -> Probe {
    let stream = match proto::connect_unix_timeout(sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        // Only a refused connection proves nothing listens. A connect TIMEOUT
        // means something holds the socket but never accepted (wedged server):
        // alive-but-unqueryable, never stale. Every other error (EMFILE,
        // EACCES, ...) is OUR failure, not the server's.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Probe::Stale,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Probe::Unqueryable,
        Err(e) => return Probe::Unprobeable(e.to_string()),
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return Probe::Unqueryable,
    };
    if write_msg_sync(&mut w, &ClientMsg::Query).is_err() {
        return Probe::Unqueryable;
    }
    let mut r = stream;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    // The server answers Query with exactly one Info then closes; tolerate
    // (skip) anything else a confused peer might emit until the deadline.
    while Instant::now() < deadline {
        match read_msg_sync::<_, ServerMsg>(&mut r) {
            Ok(ServerMsg::Info {
                clients,
                squads,
                panes,
                ..
            }) => {
                return Probe::Live {
                    clients,
                    squads,
                    panes,
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    Probe::Unqueryable
}

/// The zsh OSC 133 shell-integration snippet: `precmd` emits `D;<exit>` (the
/// just-finished command) then `A` (new prompt); `B` (command start) rides at
/// the END of `PROMPT` so it fires when the prompt finishes drawing - the
/// user's keystrokes are then echoed in the B..C window, which is what x-38c4's
/// rerun byte-captures as the command line (emitting B in `preexec` alongside C
/// leaves that window empty: readline has already echoed by then). `preexec`
/// emits `C` (Enter pressed, output begins). Idempotent (guarded +
/// double-eval-safe), no absolute paths (AC4-UI). A pane that eval's this
/// captures blocks (US1) and their command lines (x-38c4).
pub(crate) const ZSH_SHELL_INIT: &str = r#"if [ -z "${_FNO_OSC133:-}" ]; then
  _FNO_OSC133=1
  autoload -Uz add-zsh-hook
  _fno_osc133_precmd() { local e=$?; printf '\033]133;D;%s\a\033]133;A\a' "$e" }
  _fno_osc133_preexec() { printf '\033]133;C\a' }
  add-zsh-hook precmd _fno_osc133_precmd
  add-zsh-hook preexec _fno_osc133_preexec
  PROMPT="${PROMPT}%{"$'\033]133;B\a'"%}"
fi
"#;

/// The bash OSC 133 snippet: `_fno_osc133_prompt` (LAST in `PROMPT_COMMAND`)
/// emits `D;<exit>`/`A` and arms; `B` (command start) rides at the END of `PS1`
/// (`\[...\]` non-counting) so it fires when the prompt finishes drawing - the
/// user's keystrokes then echo in the B..C window that x-38c4's rerun
/// byte-captures as the command line. The `DEBUG` trap emits `C` on the FIRST
/// command after the prompt, then disarms - so a pipeline emits it once, and
/// the commands inside `PROMPT_COMMAND` (and a bare Enter) do not trip it. The
/// `_fno_osc133_prompt` guard skips the trap firing on the hook itself.
/// Idempotent, no absolute paths. (A pre-existing `PROMPT_COMMAND` entry plus a
/// bare Enter is the one residual edge - a rare phantom block; zsh has none.)
pub(crate) const BASH_SHELL_INIT: &str = r#"if [ -z "${_FNO_OSC133:-}" ]; then
  _FNO_OSC133=1
  _fno_osc133_armed=""
  _fno_osc133_prompt() {
    local e=$?
    printf '\033]133;D;%s\a\033]133;A\a' "$e"
    _fno_osc133_armed=1
  }
  _fno_osc133_preexec() {
    [ -z "$_fno_osc133_armed" ] && return
    [ -n "$COMP_LINE" ] && return
    case "$BASH_COMMAND" in _fno_osc133_prompt) return ;; esac
    _fno_osc133_armed=""
    printf '\033]133;C\a'
  }
  case "$PROMPT_COMMAND" in
    *_fno_osc133_prompt*) ;;
    *) PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND;}_fno_osc133_prompt" ;;
  esac
  PS1="${PS1}"'\[\033]133;B\a\]'
  trap '_fno_osc133_preexec' DEBUG
fi
"#;

/// `fno mux shell-init <zsh|bash>`: print an eval-able OSC 133 snippet so a
/// pane's shell emits command-block markers. A pane that never runs it still
/// captures ONE implicit block; a terminal whose shell already emits OSC 133
/// (Warp/iTerm) works with zero setup (AC4-EDGE) - the scanner does not care
/// who emits. An unsupported / missing shell is a one-line error (AC4-ERR).
pub fn shell_init(shell: Option<&str>, json: bool) -> i32 {
    let snippet = match shell {
        Some("zsh") => ZSH_SHELL_INIT,
        Some("bash") => BASH_SHELL_INIT,
        Some(other) => {
            eprintln!("fno mux shell-init: unsupported shell {other:?}; supported: zsh, bash");
            return EXIT_USAGE;
        }
        None => {
            eprintln!("fno mux shell-init: needs a shell argument: zsh|bash");
            return EXIT_USAGE;
        }
    };
    // --json wraps the snippet in the stable envelope (a script reads `.snippet`);
    // the default prints the raw snippet for `eval "$(fno mux shell-init zsh)"`.
    if json {
        println!(
            "{}",
            serde_json::json!({ "shell": shell.unwrap_or(""), "snippet": snippet })
        );
    } else {
        print!("{snippet}");
    }
    EXIT_OK
}

/// One enumerated session: its name and what a probe learned. Shared by
/// `ls` and the pre-attach picker (Locked 8) so both see the same live/stale
/// verdict from the same code.
struct SessionRow {
    name: String,
    probe: Probe,
}

impl SessionRow {
    fn is_live(&self) -> bool {
        matches!(self.probe, Probe::Live { .. })
    }
}

/// Enumerate `*.sock` stems in the mux dir, sorted. `Err` is an unreadable dir
/// the caller must distinguish from "no sessions" (a permissions/IO error must
/// never read as empty); `NotFound` returns an empty list (no session ever
/// started here, AC6-FR). Shared by `ls`/picker (which then probe) and `doctor`
/// (which version-probes) so both see the same session set.
fn session_names() -> Result<Vec<String>, String> {
    let dir = proto::mux_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("sock"))
                .then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .flatten()
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Enumerate sessions and probe each (`Query`/`Info`) for the live/stale verdict
/// `ls` and the picker render.
fn session_rows() -> Result<Vec<SessionRow>, String> {
    let dir = proto::mux_dir();
    Ok(session_names()?
        .into_iter()
        .map(|name| {
            let sock = dir.join(format!("{name}.sock"));
            let probe = probe(&sock);
            SessionRow { name, probe }
        })
        .collect())
}

/// `fno mux ls`: one row per `*.sock` in the mux dir. Read-only - a stale
/// socket is REPORTED, never unlinked (kill-server owns removal). Exits 0
/// even when every row is stale or unqueryable; only "no sessions" is
/// distinguishable by text, not exit code, so scripts can `grep`.
pub fn ls(json: bool) -> i32 {
    let rows = match session_rows() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno: {e}");
            return EXIT_ERROR;
        }
    };
    if json {
        // Stable per-row envelope: `state` is always present; live rows carry
        // the counts. An empty listing is `[]` (never the "no sessions" prose).
        let arr: Vec<_> = rows
            .iter()
            .map(|SessionRow { name, probe }| match probe {
                Probe::Live {
                    clients,
                    squads,
                    panes,
                } => serde_json::json!({
                    "session": name, "state": "live",
                    "clients": clients, "squads": squads, "panes": panes,
                }),
                Probe::Unqueryable => {
                    serde_json::json!({ "session": name, "state": "unqueryable" })
                }
                Probe::Stale => serde_json::json!({ "session": name, "state": "stale" }),
                Probe::Unprobeable(e) => {
                    serde_json::json!({ "session": name, "state": "unprobeable", "error": e })
                }
            })
            .collect();
        println!("{}", serde_json::Value::Array(arr));
        return EXIT_OK;
    }
    if rows.is_empty() {
        println!("no sessions");
        return EXIT_OK;
    }
    for SessionRow { name, probe } in &rows {
        match probe {
            Probe::Live {
                clients,
                squads,
                panes,
            } => println!("{name}: {clients} clients, {squads} squads, {panes} panes"),
            Probe::Unqueryable => println!("{name}: alive (unqueryable - older server?)"),
            Probe::Stale => println!("{name}: stale"),
            Probe::Unprobeable(e) => println!("{name}: probe failed ({e})"),
        }
    }
    EXIT_OK
}

// ---------------------------------------------------------------------------
// Pre-attach session picker (US5, Locked 8)
// ---------------------------------------------------------------------------

/// Which session bare `fno` should attach, decided BEFORE any terminal
/// takeover. `None` means the user quit the picker - a clean exit 0, never a
/// spawn. Only reached when neither `--session` nor `FNO_SESSION` pinned a
/// session (those bypass the picker entirely, AC5-FR).
///
/// Zero live sessions -> attach the default (`connect_or_spawn` births it,
/// today's behavior); exactly one -> attach it whatever its name, no picker
/// and no implicit `main` (AC5-EDGE); two or more -> the interactive picker.
/// Stale sockets never count as live (AC5-ERR) but are shown, dimmed.
pub fn pick_session() -> Option<String> {
    let rows = match session_rows() {
        Ok(r) => r,
        // An unreadable mux dir is not "no sessions": fall back to the
        // default session rather than silently masking the error into a
        // pick. connect_or_spawn surfaces any real problem.
        Err(e) => {
            eprintln!("fno: {e}");
            return Some(DEFAULT_SESSION.to_string());
        }
    };
    let live = rows.iter().filter(|r| r.is_live()).count();
    match live {
        0 => Some(DEFAULT_SESSION.to_string()),
        1 => Some(
            rows.iter()
                .find(|r| r.is_live())
                .map(|r| r.name.clone())
                .unwrap(),
        ),
        _ => run_picker(rows),
    }
}

/// A logical picker keystroke, folded from raw bytes (arrows -> Up/Down).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickKey {
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    Char(u8),
}

/// Fold raw stdin bytes into logical keys, carrying escape state in `esc`
/// across reads so an arrow split at a read boundary still lands (and a
/// bare-Esc close is never confused with the start of an arrow). Mirrors the
/// client selector's fold; kept local because the key set differs.
fn fold_pick_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<PickKey> {
    let mut keys = Vec::new();
    for &b in bytes {
        if !esc.is_empty() {
            if esc.as_slice() == [0x1b] && b == b'[' {
                esc.push(b);
                continue;
            }
            if esc.as_slice() == [0x1b, b'['] {
                match b {
                    b'A' => keys.push(PickKey::Up),
                    b'B' => keys.push(PickKey::Down),
                    _ => {} // unknown escape sequence: swallowed whole
                }
                esc.clear();
                continue;
            }
            // Pending lone ESC + a non-'[' byte: that ESC was a real Esc press.
            esc.clear();
            keys.push(PickKey::Esc);
            if b == 0x1b {
                esc.push(0x1b);
                continue;
            }
        } else if b == 0x1b {
            esc.push(0x1b);
            continue;
        }
        match b {
            0x1b => unreachable!("ESC handled above"),
            b'\r' | b'\n' => keys.push(PickKey::Enter),
            0x7f | 0x08 => keys.push(PickKey::Backspace),
            other => keys.push(PickKey::Char(other)),
        }
    }
    keys
}

/// Fold one blocking-read chunk into keys, then flush a lone ESC left pending
/// at the chunk boundary as [`PickKey::Esc`]. A bare Esc press delivers just
/// `0x1b`; without this flush it lingers in `esc` (indistinguishable from the
/// start of an arrow) until the next keystroke, so Esc-to-quit does nothing
/// (codex P2). An arrow's `ESC [ A` arrives within a single read, so it is
/// already resolved and never lingers. Residual (accepted): an arrow whose
/// bytes are split across two reads reads the leading ESC as a quit - rare
/// (terminals emit an arrow as one write) and low-stakes pre-attach.
fn pick_keys_from_read(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<PickKey> {
    let mut keys = fold_pick_keys(esc, bytes);
    if esc.as_slice() == [0x1b] {
        esc.clear();
        keys.push(PickKey::Esc);
    }
    keys
}

/// Picker view state: the row list, the cursor, and (while naming a new
/// session) the typed buffer. Pure - [`Picker::step`] is exhaustively
/// unit-testable; the IO loop only renders and reads.
struct Picker {
    rows: Vec<SessionRow>,
    cursor: usize,
    naming: Option<String>,
}

/// What one keystroke asks the IO loop to do.
#[derive(Debug, PartialEq, Eq)]
enum PickAction {
    Redraw,
    Attach(String),
    Quit,
    Bell,
}

impl Picker {
    fn new(rows: Vec<SessionRow>) -> Self {
        // Anchor the cursor on the first LIVE row so the common Enter path
        // never opens on an unselectable stale row.
        let cursor = rows.iter().position(SessionRow::is_live).unwrap_or(0);
        Picker {
            rows,
            cursor,
            naming: None,
        }
    }

    fn step(&mut self, key: PickKey) -> PickAction {
        if let Some(buf) = self.naming.as_mut() {
            return match key {
                PickKey::Enter => {
                    // Validate at the trust boundary: a name with `..` or `/`
                    // is rejected by socket_path downstream, but catching it
                    // here BELs and lets the user retype instead of exiting the
                    // picker to a downstream launch error (gemini).
                    let name = buf.trim().to_string();
                    if name.is_empty() || proto::socket_path(&name).is_err() {
                        PickAction::Bell
                    } else {
                        PickAction::Attach(name)
                    }
                }
                PickKey::Esc => {
                    self.naming = None;
                    PickAction::Redraw
                }
                PickKey::Backspace => {
                    buf.pop();
                    PickAction::Redraw
                }
                // Printable ASCII only; control bytes are ignored so a stray
                // escape tail cannot poison the name.
                PickKey::Char(c) if (0x20..0x7f).contains(&c) => {
                    buf.push(c as char);
                    PickAction::Redraw
                }
                _ => PickAction::Bell,
            };
        }
        match key {
            PickKey::Down => {
                if self.cursor + 1 < self.rows.len() {
                    self.cursor += 1;
                }
                PickAction::Redraw
            }
            PickKey::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                PickAction::Redraw
            }
            PickKey::Enter => match self.rows.get(self.cursor) {
                // Only a live row attaches; a stale/unqueryable row BELs
                // (AC5-ERR: stale is unselectable).
                Some(r) if r.is_live() => PickAction::Attach(r.name.clone()),
                _ => PickAction::Bell,
            },
            PickKey::Char(b'n') => {
                self.naming = Some(String::new());
                PickAction::Redraw
            }
            PickKey::Char(b'j') => self.step(PickKey::Down),
            PickKey::Char(b'k') => self.step(PickKey::Up),
            PickKey::Char(b'q') | PickKey::Esc => PickAction::Quit,
            _ => PickAction::Bell,
        }
    }
}

/// Render the picker to plain-terminal text (ANSI styling inline). Pure, so
/// the layout is unit-tested; the IO loop only writes what this returns.
/// Each line ends `\r\n` because the terminal is in raw mode (no implicit CR).
fn render_picker(p: &Picker) -> String {
    let mut out = String::new();
    out.push_str("fno sessions - \u{2191}\u{2193}/jk move, enter attach, n new, q quit\r\n");
    for (i, row) in p.rows.iter().enumerate() {
        let marker = if i == p.cursor { '>' } else { ' ' };
        let body = match &row.probe {
            Probe::Live {
                clients,
                squads,
                panes,
            } => format!(
                "{}  ({clients} clients, {squads} squads, {panes} panes)",
                row.name
            ),
            Probe::Unqueryable => format!("{}  (alive, unqueryable)", row.name),
            Probe::Stale => format!("{}  (stale)", row.name),
            Probe::Unprobeable(_) => format!("{}  (unprobeable)", row.name),
        };
        // The cursor reverses; stale/unselectable rows dim (AC5-ERR). Check
        // the cursor FIRST so it stays visible even when parked on a stale row
        // (combined reverse+dim), rather than vanishing under the dim (gemini).
        let (pre, post) = match (i == p.cursor, row.is_live()) {
            (true, true) => ("\x1b[7m", "\x1b[0m"),
            (true, false) => ("\x1b[7;2m", "\x1b[0m"),
            (false, false) => ("\x1b[2m", "\x1b[0m"),
            (false, true) => ("", ""),
        };
        out.push_str(&format!("{marker} {pre}{body}{post}\r\n"));
    }
    if let Some(buf) = &p.naming {
        out.push_str(&format!("new session name: {buf}\r\n"));
    }
    out
}

/// The interactive picker (2+ live sessions). Raw mode, NO alt screen (AC5-UI:
/// a clean exit leaves the terminal exactly as found, no alt-screen residue).
/// Returns the chosen/named session, or `None` on quit.
fn run_picker(rows: Vec<SessionRow>) -> Option<String> {
    use crossterm::terminal;
    use std::io::{Read, Write};

    // Restore raw mode on EVERY exit path, including the `?` early return.
    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    if let Err(e) = terminal::enable_raw_mode() {
        // No raw mode (piped stdin already screened out upstream): fall back
        // to the default session rather than fail the launch - but say so,
        // matching this file's failures-are-visible discipline. Silent here
        // would spawn a `main` the user never picked with no explanation.
        eprintln!(
            "fno: terminal raw mode unavailable ({e}); attaching default session {DEFAULT_SESSION:?}"
        );
        return Some(DEFAULT_SESSION.to_string());
    }
    let _guard = RawGuard;

    let mut picker = Picker::new(rows);
    let mut esc: Vec<u8> = Vec::new();
    let mut prev_lines = 0usize; // lines the last frame drew, to clear on redraw
    let mut stdout = std::io::stdout();
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];

    let redraw = |stdout: &mut std::io::Stdout, picker: &Picker, prev: &mut usize| {
        if *prev > 0 {
            // Move to the frame's top-left and clear it before repainting.
            let _ = write!(stdout, "\r\x1b[{}A\x1b[J", *prev);
        }
        let frame = render_picker(picker);
        *prev = frame.matches("\r\n").count();
        let _ = stdout.write_all(frame.as_bytes());
        let _ = stdout.flush();
    };
    redraw(&mut stdout, &picker, &mut prev_lines);

    let result = loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break None, // stdin closed: quit cleanly
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break None,
        };
        let mut action = PickAction::Redraw;
        for key in pick_keys_from_read(&mut esc, &buf[..n]) {
            action = picker.step(key);
            match &action {
                PickAction::Attach(_) | PickAction::Quit => break,
                PickAction::Bell => {
                    let _ = stdout.write_all(b"\x07");
                    let _ = stdout.flush();
                }
                PickAction::Redraw => {}
            }
        }
        match action {
            PickAction::Attach(name) => break Some(name),
            PickAction::Quit => break None,
            _ => redraw(&mut stdout, &picker, &mut prev_lines),
        }
    };
    // One clear on ANY exit path (attach or quit): nothing lingers above the
    // prompt on quit, nor above the client's first frame on attach (gemini).
    if prev_lines > 0 {
        let _ = write!(stdout, "\r\x1b[{prev_lines}A\x1b[J");
        let _ = stdout.flush();
    }
    result
}

/// `fno mux kill-server [<name>]`: shut one session down. A live server Byes
/// its clients, kills every pane child, and exits (its SocketGuard unlinks
/// the socket); a stale socket is unlinked here with a message (exit 0); no
/// socket at all is "no server" (exit 1).
pub fn kill_server(session: &str, json: bool) -> i32 {
    // On success `--json` prints `{"session":..,"killed":true,"note":..}`; errors
    // stay one-line on stderr (mirrors the pane verbs' json/error split).
    let killed_ok = |note: &str| -> i32 {
        if json {
            println!(
                "{}",
                serde_json::json!({ "session": session, "killed": true, "note": note })
            );
        } else {
            println!("{note}");
        }
        EXIT_OK
    };
    let sock = match proto::socket_path(session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno: {e}");
            return EXIT_USAGE;
        }
    };
    if !sock.exists() {
        eprintln!("fno: no server for session {session:?}");
        return EXIT_ERROR;
    }
    let stream = match proto::connect_unix_timeout(&sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        // Only a REFUSED connection (or a socket that vanished mid-race)
        // proves the server is dead. Any other connect error - including a
        // bounded-connect TIMEOUT (wedged server) - is not proof of death:
        // unlinking on it would orphan a LIVE server: still running,
        // unreachable by name, invisible to ls.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            // AC4-EDGE: dead server left its socket behind - take it out.
            return match std::fs::remove_file(&sock) {
                Ok(()) => killed_ok(&format!(
                    "removed stale socket for session {session:?} (server was dead)"
                )),
                Err(e) => {
                    eprintln!("fno: cannot remove stale socket {}: {e}", sock.display());
                    EXIT_ERROR
                }
            };
        }
        // A bounded-connect timeout means the server holds the socket but
        // never accepts: it is wedged. kill-server needs an ACCEPTED
        // connection to send KillServer, so it cannot recover this - and it
        // must NOT unlink (the socket may front a live-but-stuck server).
        // Name the real remedy instead of a bare io::Error, since every other
        // call site's advice points the operator here.
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            eprintln!(
                "fno: session {session:?} is wedged (connect timed out); the server holds \
                 {} but is not accepting. kill-server cannot recover it - kill the server \
                 process directly (its log is at {}).",
                sock.display(),
                sock.with_extension("log").display()
            );
            return EXIT_ERROR;
        }
        Err(e) => {
            eprintln!("fno: cannot connect to {}: {e}", sock.display());
            return EXIT_ERROR;
        }
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fno: socket setup failed: {e}");
            return EXIT_ERROR;
        }
    };
    if write_msg_sync(&mut w, &ClientMsg::KillServer).is_err() {
        eprintln!("fno: could not reach the server for session {session:?}");
        return EXIT_ERROR;
    }
    // Drain until the server closes the connection (bounded), then wait for
    // its SocketGuard unlink - the observable proof the process exited.
    let mut r = stream;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if read_msg_sync::<_, ServerMsg>(&mut r).is_err() {
            break; // EOF/timeout: the server is going down
        }
    }
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(30));
    }
    if sock.exists() {
        eprintln!("fno: session {session:?} did not shut down in time");
        return EXIT_ERROR;
    }
    killed_ok(&format!("killed session {session:?}"))
}

// ---------------------------------------------------------------------------
// `fno mux doctor` - read-only diagnostics (US6)
//
// Checks socket dir + perms, every session's server/client protocol match, and
// the terminal's copy/color capabilities. Read-only by construction: it probes
// with `Query` and the read-only `PaneLs` control verb and never unlinks a
// socket, kills a server, or scaffolds state (AC6-ERR "touches nothing",
// AC6-FR no side effects). Exit is `EXIT_ERROR` iff some check FAILs (a version
// skew); a degraded-but-usable state is a `Warn` and stays exit 0, so a box
// with no mux state exits clean (AC6-FR).
// ---------------------------------------------------------------------------

/// A doctor check's severity. Only `Fail` flips the exit non-zero; `Warn` is
/// advisory (degraded but usable), `Ok`/`Na` are clean. This is what keeps
/// AC6-ERR (skew -> non-zero) apart from AC6-FR (no state -> exit 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Warn,
    Fail,
    Na,
}

impl Verdict {
    fn word(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Warn => "warn",
            Verdict::Fail => "fail",
            Verdict::Na => "n/a",
        }
    }
}

/// One diagnostic line (AC6-UI: check name, verdict, detail, optional remedy).
struct Check {
    name: String,
    verdict: Verdict,
    detail: String,
    remedy: Option<String>,
}

/// What a version probe against one session's socket learned.
enum VersionVerdict {
    /// The server accepted our versioned control verb: protocols match.
    Ok,
    /// The server refused with `VERSION_SKEW`; `String` is its message, which
    /// already names both versions and the restart remedy (AC6-ERR).
    Skew(String),
    /// Connection refused: a leftover socket from a dead server.
    Stale,
    /// Connected but no usable control reply (an older pre-control server, or a
    /// client-side io error). Says nothing about state - never read as stale.
    Unqueryable(String),
}

/// Probe one session's socket over the VERSIONED control path (a plain `Query`
/// is frozen and version-independent, so it cannot detect skew). Sends the
/// read-only `PaneLs` verb - lists panes, mutates nothing - so doctor stays
/// side-effect free (AC6-ERR).
fn version_probe(sock: &Path) -> VersionVerdict {
    // Bounded connect: a wedged server (never accepts) reads as Unqueryable
    // via the generic arm below, never as Stale, and never hangs doctor.
    let stream = match proto::connect_unix_timeout(sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return VersionVerdict::Stale
        }
        Err(e) => return VersionVerdict::Unqueryable(e.to_string()),
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    match send_control(stream, ControlVerb::PaneLs, PROBE_TIMEOUT) {
        Ok(ServerMsg::PaneList { .. }) => VersionVerdict::Ok,
        Ok(ServerMsg::Err { code, msg }) if code == err_code::VERSION_SKEW => {
            VersionVerdict::Skew(msg)
        }
        Ok(_) => VersionVerdict::Unqueryable("unexpected control reply".into()),
        Err(e) => VersionVerdict::Unqueryable(e),
    }
}

/// Map one session's version verdict to a check line (pure, so AC6-ERR skew and
/// the stale/unqueryable advisories are unit-testable without a live server).
fn session_check(name: &str, v: VersionVerdict) -> Check {
    let cname = format!("session {name}");
    match v {
        VersionVerdict::Ok => Check {
            name: cname,
            verdict: Verdict::Ok,
            detail: format!("live, protocol v{PROTO_VERSION}"),
            remedy: None,
        },
        // The skew message already names both versions and the restart remedy.
        VersionVerdict::Skew(msg) => Check {
            name: cname,
            verdict: Verdict::Fail,
            detail: msg,
            remedy: None,
        },
        VersionVerdict::Stale => Check {
            name: cname,
            verdict: Verdict::Warn,
            detail: "stale socket (dead server)".into(),
            remedy: Some(format!("fno mux kill-server {name}")),
        },
        VersionVerdict::Unqueryable(e) => Check {
            name: cname,
            verdict: Verdict::Warn,
            detail: format!("alive but unqueryable ({e}); older server?"),
            remedy: Some("stop it and re-run fno to refresh".into()),
        },
    }
}

/// One check per session, or a single `Na` line when there is nothing to check
/// (AC6-FR). `probe` is injected so the empty and skew aggregations are testable.
fn session_checks(names: &[String], probe: impl Fn(&str) -> VersionVerdict) -> Vec<Check> {
    if names.is_empty() {
        return vec![Check {
            name: "sessions".into(),
            verdict: Verdict::Na,
            detail: "no sessions".into(),
            remedy: None,
        }];
    }
    names.iter().map(|n| session_check(n, probe(n))).collect()
}

/// Copy capability (AC6-EDGE): a local clipboard tool is reliable; its absence
/// leaves only the unverifiable OSC 52 fallback, which doctor flags as degraded.
fn clipboard_check(tool: Option<&str>) -> Check {
    match tool {
        Some(t) => Check {
            name: "copy".into(),
            verdict: Verdict::Ok,
            detail: format!("local clipboard tool `{t}` on PATH"),
            remedy: None,
        },
        None => Check {
            name: "copy".into(),
            verdict: Verdict::Warn,
            detail: "no clipboard tool on PATH; OSC 52 fallback only (unverifiable)".into(),
            remedy: Some("install pbcopy/wl-copy/xclip/xsel, or use an OSC 52 terminal".into()),
        },
    }
}

/// 24-bit color (best-effort env sniff; a true probe needs an interactive DA
/// round-trip doctor deliberately avoids). `COLORTERM` is the de-facto signal.
fn truecolor_check(colorterm: &str) -> Check {
    if colorterm == "truecolor" || colorterm == "24bit" {
        Check {
            name: "truecolor".into(),
            verdict: Verdict::Ok,
            detail: "COLORTERM advertises 24-bit".into(),
            remedy: None,
        }
    } else {
        Check {
            name: "truecolor".into(),
            verdict: Verdict::Warn,
            detail: format!("COLORTERM={colorterm:?} (24-bit not advertised)"),
            remedy: Some("set COLORTERM=truecolor if your terminal supports it".into()),
        }
    }
}

/// The outer terminal (`TERM`): mouse SGR and OSC 52 acceptance are not
/// env-derivable (both need a live query), so doctor reports `TERM` as context
/// rather than faking a capability probe. `dumb`/unset means no mouse routing.
fn terminal_check(term: &str) -> Check {
    if term.is_empty() || term == "dumb" {
        Check {
            name: "terminal".into(),
            verdict: Verdict::Warn,
            detail: format!("TERM={term:?} (no mouse/color support)"),
            remedy: Some("run inside a real terminal (xterm/tmux/alacritty/...)".into()),
        }
    } else {
        Check {
            name: "terminal".into(),
            verdict: Verdict::Ok,
            detail: format!("TERM={term}"),
            remedy: None,
        }
    }
}

/// Socket dir presence + perms. Absent is `Na` (AC6-FR: no state, not an error);
/// present-but-loose is a `Warn` (other users could reach the sockets).
fn socket_dir_check() -> Check {
    let dir = proto::mux_dir();
    match std::fs::metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Check {
            name: "socket-dir".into(),
            verdict: Verdict::Na,
            detail: format!("{} absent (no sessions started yet)", dir.display()),
            remedy: None,
        },
        Err(e) => Check {
            name: "socket-dir".into(),
            verdict: Verdict::Fail,
            detail: format!("{}: {e}", dir.display()),
            remedy: Some("check permissions on the parent directory".into()),
        },
        // A non-directory at the mux path (a stray regular file) blocks every
        // session - flag it, don't read its file mode as a dir verdict (gemini).
        Ok(md) if !md.is_dir() => Check {
            name: "socket-dir".into(),
            verdict: Verdict::Fail,
            detail: format!("{} exists but is not a directory", dir.display()),
            remedy: Some(format!(
                "remove {} (must be the mux socket dir)",
                dir.display()
            )),
        },
        Ok(md) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = md.permissions().mode() & 0o777;
            if mode == 0o700 {
                Check {
                    name: "socket-dir".into(),
                    verdict: Verdict::Ok,
                    detail: format!("{} (0700)", dir.display()),
                    remedy: None,
                }
            } else {
                Check {
                    name: "socket-dir".into(),
                    verdict: Verdict::Warn,
                    detail: format!("{} has mode {mode:o} (want 0700)", dir.display()),
                    remedy: Some(format!("chmod 700 {}", dir.display())),
                }
            }
        }
    }
}

/// Run every check. The fs/net/env reads live here; the verdict logic each one
/// calls is pure and unit-tested.
fn gather_checks() -> Vec<Check> {
    let mut checks = vec![socket_dir_check()];
    match session_names() {
        Ok(names) => {
            let dir = proto::mux_dir();
            checks.extend(session_checks(&names, |name| {
                version_probe(&dir.join(format!("{name}.sock")))
            }));
        }
        Err(e) => checks.push(Check {
            name: "sessions".into(),
            verdict: Verdict::Fail,
            detail: e,
            remedy: Some("fix the mux dir permissions".into()),
        }),
    }
    checks.push(terminal_check(&std::env::var("TERM").unwrap_or_default()));
    checks.push(truecolor_check(
        &std::env::var("COLORTERM").unwrap_or_default(),
    ));
    checks.push(clipboard_check(crate::clipboard::available_tool()));
    checks
}

/// Render every check and return the exit code: `EXIT_ERROR` iff any check
/// FAILed (a version skew), else `EXIT_OK`. Pure over the check list so the
/// exit mapping and both output shapes are testable without a live server.
fn render_doctor(checks: &[Check], json: bool) -> i32 {
    let failed = checks.iter().any(|c| c.verdict == Verdict::Fail);
    if json {
        let arr: Vec<_> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "check": c.name,
                    "verdict": c.verdict.word(),
                    "detail": c.detail,
                    "remedy": c.remedy,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "ok": !failed, "checks": arr }));
    } else {
        for c in checks {
            match &c.remedy {
                Some(r) => println!("{}: {} - {} [{}]", c.name, c.verdict.word(), c.detail, r),
                None => println!("{}: {} - {}", c.name, c.verdict.word(), c.detail),
            }
        }
    }
    if failed {
        EXIT_ERROR
    } else {
        EXIT_OK
    }
}

/// `fno mux doctor [--json]`: read-only environment diagnostics.
pub fn doctor(json: bool) -> i32 {
    render_doctor(&gather_checks(), json)
}

// ---------------------------------------------------------------------------
// `fno mux pane ls | read | run | send | wait | kill` - the v4 script API
// ---------------------------------------------------------------------------

/// The one exit-code table for the pane verbs (asserted by tests). `wait`
/// outcomes are distinct so a script can tell a settle from a match from a
/// timeout from an exit (AC4-EDGE); everything else is the usual ok/error/usage
/// trio. The server's [`WaitOutcome`] maps here in [`wait_exit_code`].
pub const EXIT_OK: i32 = 0; // ls/read/run/send/kill ok; wait settled quiet
pub const EXIT_ERROR: i32 = 1; // dead pane, io failure, version skew, server error
pub const EXIT_USAGE: i32 = 2; // malformed arguments
pub const EXIT_WAIT_MATCHED: i32 = 10; // wait: --pattern matched
pub const EXIT_WAIT_TIMEOUT: i32 = 11; // wait: deadline elapsed
pub const EXIT_WAIT_EXITED: i32 = 12; // wait: the pane's child exited
pub const EXIT_WAIT_COMMAND_DONE: i32 = 13; // wait: --command-done, OSC 133 D fired (v6)
pub const EXIT_BLOCK_UNAVAILABLE: i32 = 14; // read --block: evicted/nonexistent/markerless (v6)

/// Default `pane wait` deadline when `--timeout` is omitted. There is never an
/// infinite wait (Failure Modes: every wait is bounded).
const DEFAULT_WAIT_TIMEOUT_S: u64 = 30;

/// How long to wait for a non-`wait` reply. A `wait` reply gets its own
/// deadline (`timeout_ms` + slack) so the bounded server wait is never cut
/// short by the client's read timeout.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve `pane run --cwd` to an absolute path CLIENT-side. The server is a
/// detached daemon with its own cwd, so a RELATIVE path would resolve against
/// the wrong directory there (gemini review). Absolute passes through; a
/// relative path is joined onto `client_cwd`; omitted defaults to `client_cwd`.
/// `client_cwd` is passed in (not read here) so the branch is unit-testable.
fn resolve_run_cwd(cwd: Option<String>, client_cwd: Option<std::path::PathBuf>) -> String {
    let join = |rel: &str| -> Option<String> {
        client_cwd
            .as_ref()
            .map(|d| d.join(rel).to_string_lossy().into_owned())
    };
    match cwd {
        Some(c) if std::path::Path::new(&c).is_absolute() => c,
        Some(c) => join(&c).unwrap_or(c),
        None => client_cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

fn wait_exit_code(outcome: WaitOutcome) -> i32 {
    match outcome {
        WaitOutcome::Quiet => EXIT_OK,
        WaitOutcome::Matched => EXIT_WAIT_MATCHED,
        WaitOutcome::Timeout => EXIT_WAIT_TIMEOUT,
        WaitOutcome::PaneExited => EXIT_WAIT_EXITED,
        WaitOutcome::CommandDone { .. } => EXIT_WAIT_COMMAND_DONE,
    }
}

/// Where `pane send` gets its bytes.
#[derive(Debug, PartialEq, Eq)]
enum SendSource {
    Text(String),
    Stdin,
}

/// A parsed `pane` verb (the wire-facing subset; `--session`/`--json` ride
/// alongside on [`ParsedPane`]).
#[derive(Debug, PartialEq, Eq)]
enum PaneCmd {
    Ls,
    Read {
        pane: u64,
        lines: Option<u16>,
        block: Option<BlockSel>,
    },
    Run {
        cwd: Option<String>,
        argv: Vec<String>,
        claim: bool,
    },
    Send {
        pane: u64,
        source: SendSource,
    },
    Wait {
        pane: u64,
        quiet_ms: Option<u64>,
        pattern: Option<String>,
        timeout_ms: u64,
        command_done: bool,
    },
    Kill {
        pane: u64,
    },
    Claim {
        pane: u64,
        pid: u32,
    },
    Release {
        pane: u64,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedPane {
    session: Option<String>,
    json: bool,
    cmd: PaneCmd,
}

/// Read the value of a `--flag value` pair, advancing `i` past the value.
fn flag_value(args: &[OsString], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .and_then(|a| a.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_u64(s: &str, flag: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|_| format!("{flag} needs a number, got {s:?}"))
}

/// `--block last` or `--block <seq>`.
fn parse_block_sel(s: &str) -> Result<BlockSel, String> {
    match s {
        "last" => Ok(BlockSel::Last),
        n => n
            .parse::<u64>()
            .map(BlockSel::Seq)
            .map_err(|_| format!("--block takes `last` or a seq number, got {s:?}")),
    }
}

/// Parse the tokens after `mux pane` into a [`ParsedPane`]. Pure, so the whole
/// grammar (verbs, flags, the exit-code-bearing outcomes) is unit-testable
/// without a socket.
fn parse_pane_args(args: &[OsString]) -> Result<ParsedPane, String> {
    let verb = args
        .first()
        .and_then(|a| a.to_str())
        .ok_or_else(|| "pane needs a verb: ls|read|run|send|wait|kill|claim|release".to_string())?;

    // `run` is special: leading flags, then the command argv verbatim (its own
    // flags are NOT ours to parse), optionally after a `--` separator.
    if verb == "run" {
        let mut session = None;
        let mut json = false;
        let mut cwd = None;
        let mut claim = false;
        let mut i = 1;
        while i < args.len() {
            let tok = args[i]
                .to_str()
                .ok_or_else(|| "non-UTF-8 argument".to_string())?;
            match tok {
                "--" => {
                    i += 1;
                    break;
                }
                "--json" => json = true,
                "--claim" => claim = true,
                "--session" => session = Some(flag_value(args, &mut i, "--session")?),
                "--cwd" => cwd = Some(flag_value(args, &mut i, "--cwd")?),
                t if t.starts_with("--") => return Err(format!("unknown flag: {t}")),
                _ => break, // first bare token begins the command argv
            }
            i += 1;
        }
        let argv = args[i..]
            .iter()
            .map(|a| {
                a.to_str()
                    .map(str::to_string)
                    .ok_or_else(|| "non-UTF-8 argv".to_string())
            })
            .collect::<Result<Vec<String>, String>>()?;
        if argv.is_empty() {
            return Err("pane run needs a command".to_string());
        }
        return Ok(ParsedPane {
            session,
            json,
            cmd: PaneCmd::Run { cwd, argv, claim },
        });
    }

    // Every other verb: a single flag/positional pass (no embedded argv).
    let mut session = None;
    let mut json = false;
    let mut lines = None;
    let mut text = None;
    let mut stdin = false;
    let mut quiet_ms = None;
    let mut pattern = None;
    let mut timeout_s = None;
    let mut pid = None;
    let mut block = None;
    let mut command_done = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let tok = args[i]
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match tok {
            "--json" => json = true,
            "--session" => session = Some(flag_value(args, &mut i, "--session")?),
            "--pid" => pid = Some(parse_u64(&flag_value(args, &mut i, "--pid")?, "--pid")? as u32),
            "--lines" => {
                lines = Some(parse_u64(&flag_value(args, &mut i, "--lines")?, "--lines")? as u16)
            }
            "--block" => block = Some(parse_block_sel(&flag_value(args, &mut i, "--block")?)?),
            "--command-done" => command_done = true,
            "--text" => text = Some(flag_value(args, &mut i, "--text")?),
            "--stdin" => stdin = true,
            "--quiet-ms" => {
                quiet_ms = Some(parse_u64(
                    &flag_value(args, &mut i, "--quiet-ms")?,
                    "--quiet-ms",
                )?)
            }
            "--pattern" => pattern = Some(flag_value(args, &mut i, "--pattern")?),
            "--timeout" => {
                timeout_s = Some(parse_u64(
                    &flag_value(args, &mut i, "--timeout")?,
                    "--timeout",
                )?)
            }
            t if t.starts_with("--") => return Err(format!("unknown flag: {t}")),
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    let pane_arg = |what: &str| -> Result<u64, String> {
        let raw = positionals
            .first()
            .ok_or_else(|| format!("pane {what} needs a pane id"))?;
        parse_u64(raw, "pane id")
    };

    let cmd = match verb {
        "ls" => PaneCmd::Ls,
        "read" => PaneCmd::Read {
            pane: pane_arg("read")?,
            lines,
            block,
        },
        "send" => {
            let pane = pane_arg("send")?;
            let source = match (text, stdin) {
                (Some(_), true) => return Err("pane send takes --text OR --stdin, not both".into()),
                (Some(t), false) => SendSource::Text(t),
                (None, true) => SendSource::Stdin,
                (None, false) => return Err("pane send needs --text <s> or --stdin".into()),
            };
            PaneCmd::Send { pane, source }
        }
        "wait" => PaneCmd::Wait {
            pane: pane_arg("wait")?,
            quiet_ms,
            pattern,
            timeout_ms: timeout_s.unwrap_or(DEFAULT_WAIT_TIMEOUT_S) * 1000,
            command_done,
        },
        "kill" => PaneCmd::Kill {
            pane: pane_arg("kill")?,
        },
        "claim" => PaneCmd::Claim {
            pane: pane_arg("claim")?,
            // The holder is the CALLER (it outlives this one-shot CLI); the
            // parent pid is the honest default when --pid is not passed.
            pid: pid.unwrap_or_else(std::os::unix::process::parent_id),
        },
        "release" => PaneCmd::Release {
            pane: pane_arg("release")?,
        },
        other => {
            return Err(format!(
                "unknown pane verb: {other} (ls|read|run|send|wait|kill|claim|release)"
            ))
        }
    };
    Ok(ParsedPane { session, json, cmd })
}

/// `fno mux pane <verb> ...`: parse, resolve the session, run the verb over a
/// one-shot v4 control connection, print machine-readable output, return the
/// exit code. `env_session` is `FNO_SESSION` (set in every pane).
pub fn pane(args: &[OsString], env_session: Option<&str>) -> i32 {
    let parsed = match parse_pane_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux pane: {e}");
            return EXIT_USAGE;
        }
    };
    let session = resolve_session(parsed.session.as_deref(), env_session);
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux pane: {e}");
            return EXIT_USAGE;
        }
    };
    dispatch(&session, &sock, parsed.json, parsed.cmd)
}

/// Resolve `PaneCmd` -> a control verb + the read deadline, then run it.
fn dispatch(session: &str, sock: &Path, json: bool, cmd: PaneCmd) -> i32 {
    // `pane run` self-spawns a server for a script-only session (AC1-EDGE);
    // every other verb operates on an existing server. `pane ls` against no
    // server is "no panes" (exit 0); the rest are an error (nothing to act on).
    let (verb, read_timeout) = match cmd {
        PaneCmd::Ls => (ControlVerb::PaneLs, CONTROL_TIMEOUT),
        PaneCmd::Read { pane, lines, block } => (
            ControlVerb::PaneRead { pane, lines, block },
            CONTROL_TIMEOUT,
        ),
        PaneCmd::Run { cwd, argv, claim } => {
            let cwd = resolve_run_cwd(cwd, std::env::current_dir().ok());
            (
                ControlVerb::PaneRun {
                    cwd,
                    argv,
                    cols: None,
                    rows: None,
                    claim,
                },
                CONTROL_TIMEOUT,
            )
        }
        PaneCmd::Send { pane, source } => {
            let bytes = match source {
                SendSource::Text(t) => t.into_bytes(),
                SendSource::Stdin => {
                    let mut buf = Vec::new();
                    if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
                        eprintln!("fno mux pane: reading stdin: {e}");
                        return EXIT_ERROR;
                    }
                    buf
                }
            };
            (ControlVerb::PaneSend { pane, bytes }, CONTROL_TIMEOUT)
        }
        PaneCmd::Wait {
            pane,
            quiet_ms,
            pattern,
            timeout_ms,
            command_done,
        } => (
            ControlVerb::PaneWait {
                pane,
                quiet_ms,
                pattern,
                timeout_ms,
                command_done,
            },
            Duration::from_millis(timeout_ms) + Duration::from_secs(2),
        ),
        PaneCmd::Kill { pane } => (ControlVerb::PaneKill { pane }, CONTROL_TIMEOUT),
        PaneCmd::Claim { pane, pid } => (
            ControlVerb::PaneClaim {
                pane,
                holder_pid: pid,
            },
            CONTROL_TIMEOUT,
        ),
        PaneCmd::Release { pane } => (ControlVerb::PaneRelease { pane }, CONTROL_TIMEOUT),
    };

    let is_run = matches!(verb, ControlVerb::PaneRun { .. });
    let is_ls = matches!(verb, ControlVerb::PaneLs);
    let stream = if is_run {
        match crate::client::connect_or_spawn(sock) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fno mux pane: {e}");
                return EXIT_ERROR;
            }
        }
    } else {
        match proto::connect_unix_timeout(sock, PROBE_TIMEOUT) {
            Ok(s) => s,
            // Only a refused/absent socket proves "no server" (nothing to act
            // on): `ls` -> empty listing (exit 0), the rest -> error. Any
            // OTHER connect error (fd exhaustion, permissions, a bounded-
            // connect timeout on a wedged server) is a real failure that must
            // never read as a clean empty result - mirrors the sibling
            // `ls`/`kill_server` split, which keeps a bad session from
            // looking like zero panes.
            Err(e) => {
                let no_server = matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                );
                if is_ls && no_server {
                    if json {
                        println!("[]");
                    }
                    return EXIT_OK;
                }
                eprintln!("fno mux pane: cannot reach session {session:?}: {e}");
                return EXIT_ERROR;
            }
        }
    };

    // Whether the caller asked for --command-done: used to note the markerless
    // degradation when the server answers Quiet/Timeout instead of CommandDone.
    let command_done_requested = matches!(
        &verb,
        ControlVerb::PaneWait {
            command_done: true,
            ..
        }
    );
    match send_control(stream, verb, read_timeout) {
        Ok(reply) => render_reply(reply, json, command_done_requested),
        Err(e) => {
            eprintln!("fno mux pane: {e}");
            EXIT_ERROR
        }
    }
}

/// Write the control verb, read exactly one reply. A closed connection with no
/// reply means the server could not parse a v4 Control - almost certainly a
/// pre-v4 server (AC4-FR): report it loudly, naming this client's proto.
fn send_control(
    stream: std::os::unix::net::UnixStream,
    verb: ControlVerb,
    read_timeout: Duration,
) -> Result<ServerMsg, String> {
    let mut w = stream
        .try_clone()
        .map_err(|e| format!("socket setup failed: {e}"))?;
    write_msg_sync(
        &mut w,
        &ClientMsg::Control {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            verb,
        },
    )
    .map_err(|e| format!("could not send the control verb: {e}"))?;
    let mut r = stream;
    let _ = r.set_read_timeout(Some(read_timeout));
    match read_msg_sync::<_, ServerMsg>(&mut r) {
        Ok(msg) => Ok(msg),
        Err(crate::proto::ProtoError::Closed) => Err(format!(
            "no response from the server; it may predate v4 control verbs \
             (this client speaks proto {PROTO_VERSION}). Restart the server \
             (fno mux kill-server) and retry."
        )),
        Err(e) => Err(format!("control read failed: {e}")),
    }
}

/// Turn one server reply into stdout + an exit code. `command_done_requested`
/// lets a `wait` note the markerless degradation (asked --command-done, got a
/// quiet/timeout settle because the pane emitted no OSC 133 `D`).
fn render_reply(reply: ServerMsg, json: bool, command_done_requested: bool) -> i32 {
    match reply {
        ServerMsg::PaneList { panes } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&panes).unwrap_or_else(|_| "[]".into())
                );
            } else {
                for p in &panes {
                    let pid = p
                        .child_pid
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{} squad={} tab={} pid={} cwd={}",
                        p.pane_id, p.squad_id, p.tab_id, pid, p.cwd
                    );
                }
            }
            EXIT_OK
        }
        ServerMsg::PaneText {
            pane_id,
            text,
            block,
        } => {
            if json {
                // A block read carries its metadata (seq/exit/complete/truncated/
                // implicit); a plain read omits `block`. Degradations are visible.
                let mut obj = serde_json::json!({ "pane_id": pane_id, "text": text });
                if let Some(m) = block {
                    obj["block"] = serde_json::json!({
                        "seq": m.seq,
                        "exit": m.exit,
                        "complete": m.complete,
                        "truncated": m.truncated,
                        "implicit": m.implicit,
                    });
                }
                println!("{obj}");
            } else {
                println!("{text}");
            }
            EXIT_OK
        }
        ServerMsg::PaneSpawned { pane_id } => {
            // AC4-UI: stdout is EXACTLY the machine-readable pane id.
            if json {
                println!("{}", serde_json::json!({ "pane_id": pane_id }));
            } else {
                println!("{pane_id}");
            }
            EXIT_OK
        }
        ServerMsg::Ok => {
            if json {
                println!("{}", serde_json::json!({ "ok": true }));
            }
            EXIT_OK
        }
        ServerMsg::WaitDone { outcome } => {
            let word = match outcome {
                WaitOutcome::Quiet => "quiet",
                WaitOutcome::Matched => "matched",
                WaitOutcome::Timeout => "timeout",
                WaitOutcome::PaneExited => "exited",
                WaitOutcome::CommandDone { .. } => "command-done",
            };
            // --command-done that settled some other way = the pane emitted no
            // OSC 133 D (markerless); surface the degradation, never silently.
            let degraded =
                command_done_requested && !matches!(outcome, WaitOutcome::CommandDone { .. });
            if json {
                let mut obj = serde_json::json!({ "outcome": word });
                if let WaitOutcome::CommandDone { exit } = outcome {
                    obj["exit"] = serde_json::json!(exit);
                }
                if degraded {
                    obj["degraded"] =
                        serde_json::json!("no OSC 133 markers; settled by quiet/timeout");
                }
                println!("{obj}");
            } else {
                println!("{word}");
            }
            wait_exit_code(outcome)
        }
        ServerMsg::Err { code, msg } => {
            eprintln!("fno mux pane: {msg}");
            // BLOCK_UNAVAILABLE gets its own exit code (AC2-ERR: a caller can
            // tell "no such block" apart from a dead pane); every other class
            // shares the generic error code.
            if code == err_code::BLOCK_UNAVAILABLE {
                EXIT_BLOCK_UNAVAILABLE
            } else {
                EXIT_ERROR
            }
        }
        // The server only ever answers a control connection with the replies
        // above; anything else is a protocol violation.
        other => {
            eprintln!("fno mux pane: unexpected server reply: {other:?}");
            EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_session_resolution_flag_beats_env_beats_default() {
        // Locked 7: --session flag > FNO_SESSION env > "main" (AC3-EDGE).
        assert_eq!(resolve_session(Some("other"), Some("work")), "other");
        assert_eq!(resolve_session(None, Some("work")), "work");
        assert_eq!(resolve_session(None, None), DEFAULT_SESSION);
        // An empty env var reads as unset, not as a session named "".
        assert_eq!(resolve_session(None, Some("")), DEFAULT_SESSION);
    }

    // -- pre-attach picker (US5) -------------------------------------------

    fn live(name: &str) -> SessionRow {
        SessionRow {
            name: name.into(),
            probe: Probe::Live {
                clients: 1,
                squads: 2,
                panes: 3,
            },
        }
    }

    fn stale(name: &str) -> SessionRow {
        SessionRow {
            name: name.into(),
            probe: Probe::Stale,
        }
    }

    #[test]
    fn mux_pick_fold_keys_arrows_and_bare_esc_split_across_reads() {
        let mut esc = Vec::new();
        // A plain Down arrow arriving one byte per read.
        let mut got = Vec::new();
        for chunk in [&b"\x1b"[..], &b"["[..], &b"B"[..]] {
            got.extend(fold_pick_keys(&mut esc, chunk));
        }
        assert_eq!(got, vec![PickKey::Down]);
        // A bare Esc (not followed by '[') resolves to Esc on the next byte.
        let mut esc = Vec::new();
        assert_eq!(
            fold_pick_keys(&mut esc, b"\x1bq"),
            vec![PickKey::Esc, PickKey::Char(b'q')]
        );
        // Enter, Backspace, printable.
        let mut esc = Vec::new();
        assert_eq!(
            fold_pick_keys(&mut esc, b"a\r\x7f"),
            vec![PickKey::Char(b'a'), PickKey::Enter, PickKey::Backspace]
        );
    }

    #[test]
    fn mux_pick_keys_from_read_flushes_a_lone_esc_as_quit() {
        // codex P2: a bare Esc press (single 0x1b byte in a read) must surface
        // as PickKey::Esc so Esc-to-quit works without a second keystroke.
        let mut esc = Vec::new();
        assert_eq!(
            pick_keys_from_read(&mut esc, b"\x1b"),
            vec![PickKey::Esc],
            "lone ESC flushes as Esc at the read boundary"
        );
        assert!(esc.is_empty(), "no ESC left pending after the flush");
        // An arrow arriving whole in one read is still Up, not a spurious quit.
        let mut esc = Vec::new();
        assert_eq!(pick_keys_from_read(&mut esc, b"\x1b[A"), vec![PickKey::Up]);
        // A char after ESC in the same read: the fold already resolves the ESC.
        let mut esc = Vec::new();
        assert_eq!(
            pick_keys_from_read(&mut esc, b"\x1bx"),
            vec![PickKey::Esc, PickKey::Char(b'x')]
        );
    }

    #[test]
    fn mux_picker_anchors_cursor_on_first_live_row() {
        // AC5-ERR: a leading stale row must not be where the cursor opens.
        let p = Picker::new(vec![stale("old"), live("work"), live("play")]);
        assert_eq!(p.cursor, 1);
    }

    #[test]
    fn mux_picker_enter_attaches_live_bells_on_stale() {
        let mut p = Picker::new(vec![live("work"), stale("dead")]);
        // Cursor starts on the live row -> Enter attaches it.
        assert_eq!(p.step(PickKey::Enter), PickAction::Attach("work".into()));
        // Move onto the stale row -> Enter BELs (unselectable, AC5-ERR).
        assert_eq!(p.step(PickKey::Down), PickAction::Redraw);
        assert_eq!(p.step(PickKey::Enter), PickAction::Bell);
        // Down clamps at the last row; Up returns.
        assert_eq!(p.step(PickKey::Down), PickAction::Redraw);
        assert_eq!(p.cursor, 1);
    }

    #[test]
    fn mux_picker_quit_on_q_and_esc() {
        let mut p = Picker::new(vec![live("a"), live("b")]);
        assert_eq!(p.step(PickKey::Char(b'q')), PickAction::Quit);
        assert_eq!(p.step(PickKey::Esc), PickAction::Quit);
    }

    #[test]
    fn mux_picker_naming_flow_builds_and_attaches() {
        // AC5 `n`: inline name prompt, printable chars accumulate, Enter
        // attaches the trimmed name, Esc cancels back to the list.
        let mut p = Picker::new(vec![live("a"), live("b")]);
        assert_eq!(p.step(PickKey::Char(b'n')), PickAction::Redraw);
        assert!(p.naming.is_some());
        for c in b"work" {
            assert_eq!(p.step(PickKey::Char(*c)), PickAction::Redraw);
        }
        assert_eq!(p.step(PickKey::Backspace), PickAction::Redraw); // "wor"
        assert_eq!(p.step(PickKey::Char(b'k')), PickAction::Redraw); // "work"
        assert_eq!(p.step(PickKey::Enter), PickAction::Attach("work".into()));
        // Empty name BELs rather than attaching "".
        let mut p = Picker::new(vec![live("a"), live("b")]);
        p.step(PickKey::Char(b'n'));
        assert_eq!(p.step(PickKey::Enter), PickAction::Bell);
        // An invalid name (path traversal) BELs at the trust boundary instead
        // of exiting to a downstream launch error.
        let mut p = Picker::new(vec![live("a"), live("b")]);
        p.step(PickKey::Char(b'n'));
        for c in b"../evil" {
            p.step(PickKey::Char(*c));
        }
        assert_eq!(p.step(PickKey::Enter), PickAction::Bell);
        assert!(
            p.naming.is_some(),
            "stays in naming mode to let the user retype"
        );
        // Esc from naming returns to the list (a following q quits).
        p.step(PickKey::Char(b'n'));
        assert_eq!(p.step(PickKey::Esc), PickAction::Redraw);
        assert!(p.naming.is_none());
        assert_eq!(p.step(PickKey::Char(b'q')), PickAction::Quit);
    }

    #[test]
    fn mux_render_picker_marks_cursor_and_dims_stale() {
        let mut p = Picker::new(vec![live("work"), stale("dead")]);
        let out = render_picker(&p);
        assert!(out.contains("work  (1 clients, 2 squads, 3 panes)"));
        assert!(out.contains("dead  (stale)"));
        assert!(out.contains("\x1b[2m"), "stale row dimmed");
        assert!(out.contains("\x1b[7m"), "live cursor row reversed");
        // Cursor parked on the stale row: reverse+dim so it stays visible.
        p.step(PickKey::Down);
        assert_eq!(p.cursor, 1);
        assert!(
            render_picker(&p).contains("\x1b[7;2m"),
            "selected stale row keeps a visible cursor"
        );
        // Naming mode renders the prompt.
        p.step(PickKey::Char(b'n'));
        p.step(PickKey::Char(b'x'));
        assert!(render_picker(&p).contains("new session name: x"));
    }

    #[test]
    fn mux_kill_server_missing_socket_is_no_server_exit_1() {
        // No env manipulation (unit tests share the process): a name no real
        // session uses resolves to a socket that does not exist -> exit 1.
        // The full live/stale matrix runs e2e against FNO_MUX_DIR-scoped
        // servers in 3.6.
        let code = kill_server(&format!("fno-test-absent-{}", std::process::id()), false);
        assert_eq!(code, EXIT_ERROR, "missing socket must exit 1");
    }

    #[test]
    fn mux_kill_server_invalid_name_is_usage_exit_2() {
        assert_eq!(
            kill_server("../evil", false),
            EXIT_USAGE,
            "validation precedes any I/O"
        );
    }

    // -- pane verb parsing (the socket-free grammar) -----------------------

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn mux_pane_parse_ls_read_kill() {
        assert_eq!(
            parse_pane_args(&os(&["ls"])).unwrap(),
            ParsedPane {
                session: None,
                json: false,
                cmd: PaneCmd::Ls
            }
        );
        assert_eq!(
            parse_pane_args(&os(&["read", "7", "--lines", "40", "--json"])).unwrap(),
            ParsedPane {
                session: None,
                json: true,
                cmd: PaneCmd::Read {
                    pane: 7,
                    lines: Some(40),
                    block: None,
                }
            }
        );
        // --block last | <seq> selects a command block (lines ignored server-side).
        assert_eq!(
            parse_pane_args(&os(&["read", "7", "--block", "last"]))
                .unwrap()
                .cmd,
            PaneCmd::Read {
                pane: 7,
                lines: None,
                block: Some(BlockSel::Last),
            }
        );
        assert_eq!(
            parse_pane_args(&os(&["read", "7", "--block", "3"]))
                .unwrap()
                .cmd,
            PaneCmd::Read {
                pane: 7,
                lines: None,
                block: Some(BlockSel::Seq(3)),
            }
        );
        assert!(parse_pane_args(&os(&["read", "7", "--block", "nope"])).is_err());
        assert_eq!(
            parse_pane_args(&os(&["kill", "3", "--session", "work"])).unwrap(),
            ParsedPane {
                session: Some("work".into()),
                json: false,
                cmd: PaneCmd::Kill { pane: 3 }
            }
        );
    }

    #[test]
    fn mux_pane_parse_run_takes_argv_verbatim_after_flags() {
        // Leading flags are ours; the command argv (incl. ITS flags) is not.
        let p = parse_pane_args(&os(&[
            "run",
            "--cwd",
            "/code/foo",
            "--",
            "claude",
            "--print",
            "hi",
        ]))
        .unwrap();
        assert_eq!(
            p,
            ParsedPane {
                session: None,
                json: false,
                cmd: PaneCmd::Run {
                    cwd: Some("/code/foo".into()),
                    argv: vec!["claude".into(), "--print".into(), "hi".into()],
                    claim: false,
                },
            }
        );
        // The `--` is optional: the first bare token begins the argv.
        let p = parse_pane_args(&os(&["run", "echo", "marker"])).unwrap();
        assert!(
            matches!(p.cmd, PaneCmd::Run { argv, .. } if argv == vec!["echo".to_string(), "marker".into()])
        );
        // An empty command is a usage error.
        assert!(parse_pane_args(&os(&["run", "--cwd", "/x"])).is_err());
    }

    #[test]
    fn mux_pane_parse_wait_defaults_and_units() {
        // --timeout is seconds -> ms; the default is bounded, never infinite.
        let p = parse_pane_args(&os(&["wait", "5", "--quiet-ms", "200"])).unwrap();
        assert_eq!(
            p.cmd,
            PaneCmd::Wait {
                pane: 5,
                quiet_ms: Some(200),
                pattern: None,
                timeout_ms: DEFAULT_WAIT_TIMEOUT_S * 1000,
                command_done: false,
            }
        );
        let p =
            parse_pane_args(&os(&["wait", "5", "--pattern", "done", "--timeout", "3"])).unwrap();
        assert_eq!(
            p.cmd,
            PaneCmd::Wait {
                pane: 5,
                quiet_ms: None,
                pattern: Some("done".into()),
                timeout_ms: 3000,
                command_done: false,
            }
        );
        // --command-done is a bare flag.
        let p = parse_pane_args(&os(&["wait", "5", "--command-done"])).unwrap();
        assert_eq!(
            p.cmd,
            PaneCmd::Wait {
                pane: 5,
                quiet_ms: None,
                pattern: None,
                timeout_ms: DEFAULT_WAIT_TIMEOUT_S * 1000,
                command_done: true,
            }
        );
    }

    #[test]
    fn mux_pane_parse_send_source_is_text_xor_stdin() {
        assert_eq!(
            parse_pane_args(&os(&["send", "2", "--text", "hi\r"]))
                .unwrap()
                .cmd,
            PaneCmd::Send {
                pane: 2,
                source: SendSource::Text("hi\r".into())
            }
        );
        assert_eq!(
            parse_pane_args(&os(&["send", "2", "--stdin"])).unwrap().cmd,
            PaneCmd::Send {
                pane: 2,
                source: SendSource::Stdin
            }
        );
        // Neither / both are usage errors.
        assert!(parse_pane_args(&os(&["send", "2"])).is_err());
        assert!(parse_pane_args(&os(&["send", "2", "--text", "x", "--stdin"])).is_err());
    }

    #[test]
    fn mux_pane_parse_rejects_bad_verbs_flags_and_ids() {
        assert!(parse_pane_args(&os(&["bogus"])).is_err());
        assert!(parse_pane_args(&os(&[])).is_err());
        assert!(parse_pane_args(&os(&["read", "notanumber"])).is_err());
        assert!(parse_pane_args(&os(&["read", "7", "--nope"])).is_err());
        assert!(
            parse_pane_args(&os(&["read"])).is_err(),
            "read needs a pane id"
        );
    }

    #[test]
    fn mux_pane_run_cwd_resolves_relative_client_side() {
        let base = std::path::PathBuf::from("/home/u/proj");
        // Absolute passes through untouched.
        assert_eq!(
            resolve_run_cwd(Some("/code/foo".into()), Some(base.clone())),
            "/code/foo"
        );
        // Relative is joined onto the client cwd (the daemon would otherwise
        // resolve it against ITS own cwd).
        assert_eq!(
            resolve_run_cwd(Some("sub/dir".into()), Some(base.clone())),
            "/home/u/proj/sub/dir"
        );
        // Omitted defaults to the client cwd.
        assert_eq!(resolve_run_cwd(None, Some(base)), "/home/u/proj");
    }

    #[test]
    fn mux_pane_wait_exit_codes_are_distinct() {
        // AC3-UI: CommandDone is tellable apart from quiet/matched/timeout/exited.
        let codes = [
            wait_exit_code(WaitOutcome::Quiet),
            wait_exit_code(WaitOutcome::Matched),
            wait_exit_code(WaitOutcome::Timeout),
            wait_exit_code(WaitOutcome::PaneExited),
            wait_exit_code(WaitOutcome::CommandDone { exit: Some(0) }),
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            5,
            "every wait outcome maps to a distinct code"
        );
        assert_eq!(wait_exit_code(WaitOutcome::Quiet), EXIT_OK);
        assert_ne!(EXIT_WAIT_TIMEOUT, EXIT_WAIT_MATCHED);
        // The exit code is independent of the reported command exit.
        assert_eq!(
            wait_exit_code(WaitOutcome::CommandDone { exit: None }),
            EXIT_WAIT_COMMAND_DONE
        );
    }

    #[test]
    fn mux_shell_init_snippets_are_marker_emitting_and_hygienic() {
        for snippet in [ZSH_SHELL_INIT, BASH_SHELL_INIT] {
            // Emits all four FinalTerm markers.
            for m in ["133;A", "133;B", "133;C", "133;D"] {
                assert!(snippet.contains(m), "missing {m} in {snippet:?}");
            }
            // Idempotent: guarded so a double-eval is a no-op (AC4-UI).
            assert!(snippet.contains("_FNO_OSC133"));
            // x-38c4: `B` rides in the prompt (PROMPT/PS1), not adjacent to `C`
            // in the run hook - else the B..C window is empty and rerun captures
            // no command (readline has already echoed by preexec/DEBUG time).
            assert!(
                !snippet.contains("133;B\\a\\033]133;C"),
                "B must not be emitted adjacent to C: {snippet:?}"
            );
            assert!(
                snippet.contains("PROMPT") || snippet.contains("PS1"),
                "B must ride in the prompt string: {snippet:?}"
            );
            // No absolute paths (AC4-UI): nothing references a `/...` path.
            assert!(
                !snippet.lines().any(|l| l.trim_start().starts_with('/')),
                "snippet must carry no absolute paths"
            );
        }
    }

    #[test]
    fn mux_shell_init_unsupported_shell_is_a_usage_error() {
        // AC4-ERR: an unsupported / missing shell exits non-zero.
        assert_eq!(shell_init(Some("zsh"), false), EXIT_OK);
        assert_eq!(shell_init(Some("bash"), false), EXIT_OK);
        assert_eq!(shell_init(Some("zsh"), true), EXIT_OK);
        assert_eq!(shell_init(Some("fish"), false), EXIT_USAGE);
        assert_eq!(shell_init(None, false), EXIT_USAGE);
    }

    // -- doctor (US6) --------------------------------------------------------

    fn check(verdict: Verdict) -> Check {
        Check {
            name: "t".into(),
            verdict,
            detail: "d".into(),
            remedy: None,
        }
    }

    #[test]
    fn doctor_exit_ok_when_no_check_fails() {
        // AC6-FR/HP: ok/warn/na only -> exit 0 (the exit table's OK row).
        let checks = [check(Verdict::Ok), check(Verdict::Warn), check(Verdict::Na)];
        assert_eq!(render_doctor(&checks, false), EXIT_OK);
        assert_eq!(render_doctor(&checks, true), EXIT_OK);
    }

    #[test]
    fn doctor_exit_error_when_any_check_fails() {
        // AC6-ERR: a single Fail (version skew) flips the exit non-zero.
        let checks = [check(Verdict::Ok), check(Verdict::Fail)];
        assert_eq!(render_doctor(&checks, false), EXIT_ERROR);
        assert_eq!(render_doctor(&checks, true), EXIT_ERROR);
    }

    #[test]
    fn doctor_version_skew_is_a_failing_check_naming_both_versions() {
        // AC6-ERR: the skew verdict carries the server's message (which names
        // both versions + the restart remedy) and renders as a Fail.
        let msg = "protocol version mismatch: client 0.3.0 speaks v7, \
                   server 0.2.0 speaks v6. ... re-run fno to start a fresh one.";
        let c = session_check("main", VersionVerdict::Skew(msg.into()));
        assert_eq!(c.verdict, Verdict::Fail);
        assert!(c.detail.contains("v7") && c.detail.contains("v6"));
    }

    #[test]
    fn doctor_no_sessions_is_na_not_a_finding() {
        // AC6-FR: nothing to check reports cleanly and never fails the run.
        let checks = session_checks(&[], |_| VersionVerdict::Ok);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].verdict, Verdict::Na);
        assert_eq!(render_doctor(&checks, false), EXIT_OK);
    }

    #[test]
    fn doctor_sessions_aggregate_each_verdict() {
        // A live session is Ok; a skewed one Fails the run.
        let names = vec!["good".to_string(), "bad".to_string()];
        let checks = session_checks(&names, |n| match n {
            "bad" => VersionVerdict::Skew("client v7 server v6".into()),
            _ => VersionVerdict::Ok,
        });
        assert_eq!(checks.len(), 2);
        assert_eq!(render_doctor(&checks, false), EXIT_ERROR);
    }

    #[test]
    fn doctor_stale_socket_is_advisory_not_a_failure() {
        // A leftover socket is worth cleaning but not a run failure; doctor
        // never unlinks it (read-only) - the remedy points at kill-server.
        let c = session_check("old", VersionVerdict::Stale);
        assert_eq!(c.verdict, Verdict::Warn);
        assert!(c.remedy.as_deref().unwrap().contains("kill-server old"));
    }

    #[test]
    fn doctor_copy_degraded_without_a_clipboard_tool() {
        // AC6-EDGE: no local tool -> copy flagged degraded (Warn), before the
        // user ever hits AC2-ERR. A present tool is Ok.
        assert_eq!(clipboard_check(None).verdict, Verdict::Warn);
        assert!(clipboard_check(None).detail.contains("OSC 52 fallback"));
        assert_eq!(clipboard_check(Some("pbcopy")).verdict, Verdict::Ok);
    }

    #[test]
    fn doctor_truecolor_and_terminal_env_sniffs() {
        assert_eq!(truecolor_check("truecolor").verdict, Verdict::Ok);
        assert_eq!(truecolor_check("24bit").verdict, Verdict::Ok);
        assert_eq!(truecolor_check("").verdict, Verdict::Warn);
        assert_eq!(terminal_check("xterm-256color").verdict, Verdict::Ok);
        assert_eq!(terminal_check("dumb").verdict, Verdict::Warn);
        assert_eq!(terminal_check("").verdict, Verdict::Warn);
    }

    #[test]
    fn doctor_text_lines_are_single_line_with_verdict() {
        // AC6-UI: every finding is one line carrying its verdict word.
        let c = Check {
            name: "socket-dir".into(),
            verdict: Verdict::Warn,
            detail: "mode 755".into(),
            remedy: Some("chmod 700".into()),
        };
        // Render captures stdout only in an integration harness; here assert the
        // verdict vocabulary the line is built from stays stable.
        assert_eq!(c.verdict.word(), "warn");
        assert_eq!(Verdict::Ok.word(), "ok");
        assert_eq!(Verdict::Fail.word(), "fail");
        assert_eq!(Verdict::Na.word(), "n/a");
    }
}
