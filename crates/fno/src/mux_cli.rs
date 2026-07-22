//! The scriptable `fno mux` verbs: `ls`, `kill-server`, `shell-init`, `doctor`,
//! the `pane` control-verb family, and the `block` porcelain (`block pipe`).
//! Plain client-process code - they run and exit, no TUI, no raw mode, no
//! attach - so every probe is bounded by a read/write timeout and a bad
//! session can never hang the listing (AC4-ERR).
//!
//! ## Exit-code table (US6, the one authority; asserted by tests)
//!
//! Every verb returns one of [`EXIT_OK`] (0), [`EXIT_ERROR`] (1, an io/dead/skew
//! failure), [`EXIT_USAGE`] (2, malformed args), or - for `pane wait`/`read`
//! and `block pipe` - the distinct `EXIT_WAIT_*`/`EXIT_BLOCK_UNAVAILABLE`/
//! [`EXIT_TARGET_NOT_IDLE`] codes (10-15). The constants below carry the
//! canonical comment per code.
//!
//! ## `--json` envelope (US6; every verb accepts `--json`)
//!
//! Each verb emits a stable, documented JSON shape on stdout under `--json`;
//! errors stay one-line on stderr (never mixed into the json). The shapes:
//! - `ls`: array of `{session, state: live|stale|unqueryable|unprobeable,
//!   clients?, squads?, panes?, error?}` (`[]` when empty). A `live` row also
//!   carries `stale` (x-1a85: on a wire version this binary can't handshake -
//!   `fno restart` auto-restarts these) and `wire_version` (the server's
//!   `.ver` sidecar value, `null` for a pre-sidecar server).
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
    self, err_code, read_msg_sync, write_msg_sync, BlockSel, ClientMsg, ControlVerb, LayoutScope,
    PanePlacement, PaneTarget, ServerMsg, TabSel, WaitOutcome, BUILD_VERSION, DEFAULT_SESSION,
    PROTO_VERSION,
};
use crate::tree::Dir;

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
    /// Accepts connections but never answered a parseable `Info` (an older
    /// build): listed, never unlinked, and one bad session never breaks the
    /// listing (AC4-ERR). Distinct from `Wedged` -- an accepted connection
    /// proves the server is alive and reachable, just proto-old.
    Unqueryable,
    /// Holds the socket but never ACCEPTS a connection (connect times out): a
    /// wedged server, alive-but-stuck. Split from `Unqueryable` (x-82c6) so
    /// `restart --mux` stops reporting ok while a wedged server keeps running.
    /// Because a dead server releases its socket (connect REFUSED -> `Stale`),
    /// a connect-timeout implies a LIVE holder that is not accepting.
    Wedged,
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
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Probe::Wedged,
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
    /// (x-1a85) The wire version the server stamped in its `.ver` sidecar, or
    /// `None` when absent (a pre-sidecar server, i.e. an older build). Only
    /// meaningful for a `Live` row.
    wire_version: Option<u32>,
}

impl SessionRow {
    fn is_live(&self) -> bool {
        matches!(self.probe, Probe::Live { .. })
    }

    /// (x-1a85) A LIVE server whose wire version is not the running binary's:
    /// a new client's handshake would be REJECTED (`check_attach_version`), so
    /// the server is unreachable by a current client and safe to auto-restart.
    /// A missing sidecar (`None`) is treated as stale - it predates the feature,
    /// so it is necessarily an older wire (the exact pair-deploy case this
    /// heals). Non-live rows are never "wire stale" (they are dead/wedged).
    fn wire_stale(&self) -> bool {
        self.is_live() && self.wire_version != Some(proto::PROTO_VERSION)
    }
}

/// Read a session socket's `.ver` sidecar (x-1a85) and parse the stamped wire
/// version. `None` on any read/parse failure (absent sidecar = older server).
fn read_wire_version(sock: &Path) -> Option<u32> {
    std::fs::read_to_string(proto::version_sidecar_path(sock))
        .ok()?
        .trim()
        .parse()
        .ok()
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
            let wire_version = read_wire_version(&sock);
            SessionRow {
                name,
                probe,
                wire_version,
            }
        })
        .collect())
}

/// One `fno mux ls --json` row. The `state` string is the stable contract
/// `fno restart --mux` reads (a `wedged` row is what makes restart report a
/// non-zero exit); a wedged row also carries its `log` path so the operator can
/// find the stuck server. Pure, so the state contract is unit-testable.
fn session_row_json(row: &SessionRow) -> serde_json::Value {
    let stale = row.wire_stale();
    let SessionRow {
        name,
        probe,
        wire_version,
    } = row;
    match probe {
        Probe::Live {
            clients,
            squads,
            panes,
        } => serde_json::json!({
            "session": name, "state": "live",
            "clients": clients, "squads": squads, "panes": panes,
            // (x-1a85) `stale` = live but on a wire version the running binary
            // can't handshake; `fno restart` auto-restarts these unconditionally.
            // `wire_version` is null for a pre-sidecar (older) server.
            "stale": stale, "wire_version": wire_version,
        }),
        Probe::Unqueryable => serde_json::json!({ "session": name, "state": "unqueryable" }),
        Probe::Wedged => {
            let log = proto::socket_path(name)
                .map(|p| p.with_extension("log").display().to_string())
                .unwrap_or_default();
            serde_json::json!({ "session": name, "state": "wedged", "log": log })
        }
        Probe::Stale => serde_json::json!({ "session": name, "state": "stale" }),
        Probe::Unprobeable(e) => {
            serde_json::json!({ "session": name, "state": "unprobeable", "error": e })
        }
    }
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
        let arr: Vec<_> = rows.iter().map(session_row_json).collect();
        println!("{}", serde_json::Value::Array(arr));
        return EXIT_OK;
    }
    if rows.is_empty() {
        println!("no sessions");
        return EXIT_OK;
    }
    for row in &rows {
        let stale = row.wire_stale();
        let SessionRow { name, probe, .. } = row;
        match probe {
            Probe::Live {
                clients,
                squads,
                panes,
            } => {
                // (x-1a85) A stale-wire live server is flagged: a current client
                // can't attach it, and `fno restart` will auto-restart it.
                let tail = if stale {
                    " [stale wire - restart to reconnect]"
                } else {
                    ""
                };
                println!("{name}: {clients} clients, {squads} squads, {panes} panes{tail}")
            }
            Probe::Unqueryable => println!("{name}: alive (unqueryable - older server?)"),
            Probe::Wedged => {
                println!(
                    "{name}: wedged (holds the socket but not accepting - kill the server process)"
                )
            }
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
            Probe::Wedged => format!("{}  (wedged)", row.name),
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
pub const EXIT_TARGET_NOT_IDLE: i32 = 15; // block pipe: receiving agent not idle (guard refused)
pub const EXIT_NOT_FOUND: i32 = 16; // where: the fno_id is not in the registry (x-d865)
pub const EXIT_NOT_PANE_HOSTED: i32 = 17; // where: in registry but hosts no live pane (x-d865)
pub const EXIT_REGISTRY_UNAVAILABLE: i32 = 18; // where: the registry could not be read (x-d865)

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
    Ls {
        /// (x-d865) `--fno-id <id>`: filter the listing to panes hosting this
        /// fno session id (client-side over `PaneInfo.fno_id`).
        fno_id: Option<String>,
    },
    Read {
        pane: u64,
        lines: Option<u16>,
        block: Option<BlockSel>,
    },
    /// (x-d865) `pane split <pane> --direction <dir> [--focus]`.
    Split {
        pane: u64,
        direction: Dir,
        focus: bool,
    },
    /// (x-d865) `pane break <pane> [--name <s>]`.
    Break {
        pane: u64,
        name: Option<String>,
    },
    Run {
        cwd: Option<String>,
        argv: Vec<String>,
        claim: bool,
        placement: PanePlacement,
    },
    Send {
        pane: u64,
        source: SendSource,
        /// `--guarded`: refuse the paste (TARGET_NOT_IDLE) unless the pane is
        /// provably idle, so a mid-turn recipient demotes to durable instead of
        /// swallowing bytes an external sender would miscall delivered. Default
        /// off: the raw channel is the writer-claim holder's own.
        guarded: bool,
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

/// A direction word: `left|right|up|down` (x-d865, shared by split/run).
fn parse_dir(s: &str, flag: &str) -> Result<Dir, String> {
    match s {
        "left" => Ok(Dir::Left),
        "right" => Ok(Dir::Right),
        "up" => Ok(Dir::Up),
        "down" => Ok(Dir::Down),
        _ => Err(format!(
            "{flag} must be left, right, up, or down (got {s:?})"
        )),
    }
}

/// A `--tab <spec>` selector (x-d865). Grammar: `active` | `new` | `id:<n>` (the
/// STABLE tab id, preferred in scripts) | `name:<s>` | a bare integer (an
/// ordinal Index, convenience only - ordinals renumber) | any other bare word
/// (a tab Name).
fn parse_tab_sel(s: &str) -> Result<TabSel, String> {
    if let Some(n) = s.strip_prefix("id:") {
        return parse_u64(n, "--tab id:").map(TabSel::Id);
    }
    if let Some(name) = s.strip_prefix("name:") {
        return Ok(TabSel::Name(name.to_string()));
    }
    match s {
        "active" => Ok(TabSel::Active),
        "new" => Ok(TabSel::New),
        _ => match s.parse::<usize>() {
            Ok(i) => Ok(TabSel::Index(i)),
            Err(_) => Ok(TabSel::Name(s.to_string())),
        },
    }
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

    // `run` is special: leading options/directives, then the command argv
    // verbatim (its own flags are NOT ours to parse), optionally after `--`.
    if verb == "run" {
        let mut session = None;
        let mut json = false;
        let mut cwd = None;
        let mut claim = false;
        let mut squad = None;
        let mut split = None;
        let mut tab = None;
        let mut at = None;
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
                "--squad" | "-s" | "squad" => {
                    let name = flag_value(args, &mut i, tok)?;
                    if name.trim().is_empty() {
                        return Err("squad/-s needs a nonblank squad name".into());
                    }
                    squad = Some(name);
                }
                "--split" | "-x" | "split" => {
                    split = Some(parse_dir(&flag_value(args, &mut i, tok)?, "split/-x")?);
                }
                // (x-d865) exact placement: land in a named tab, adjacent to an
                // anchor pane.
                "--tab" => tab = Some(parse_tab_sel(&flag_value(args, &mut i, "--tab")?)?),
                "--at" => at = Some(parse_u64(&flag_value(args, &mut i, "--at")?, "--at")?),
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
            cmd: PaneCmd::Run {
                cwd,
                argv,
                claim,
                placement: PanePlacement {
                    target: squad
                        .map(PaneTarget::SquadName)
                        .unwrap_or(PaneTarget::CurrentRoute),
                    split,
                    here: false,
                    tab,
                    at,
                },
            },
        });
    }

    // Every other verb: a single flag/positional pass (no embedded argv).
    let mut session = None;
    let mut json = false;
    let mut lines = None;
    let mut text = None;
    let mut stdin = false;
    let mut guarded = false;
    let mut quiet_ms = None;
    let mut pattern = None;
    let mut timeout_s = None;
    let mut pid = None;
    let mut block = None;
    let mut command_done = false;
    let mut direction = None;
    let mut focus = false;
    let mut name = None;
    let mut fno_id = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let tok = args[i]
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match tok {
            "--json" => json = true,
            "--session" => session = Some(flag_value(args, &mut i, "--session")?),
            // (x-d865) split/break/ls flags.
            "--direction" | "-d" => {
                direction = Some(parse_dir(&flag_value(args, &mut i, tok)?, tok)?)
            }
            "--focus" => focus = true,
            "--name" => name = Some(flag_value(args, &mut i, "--name")?),
            "--fno-id" => fno_id = Some(flag_value(args, &mut i, "--fno-id")?),
            "--pid" => pid = Some(parse_u64(&flag_value(args, &mut i, "--pid")?, "--pid")? as u32),
            "--lines" => {
                lines = Some(parse_u64(&flag_value(args, &mut i, "--lines")?, "--lines")? as u16)
            }
            "--block" => block = Some(parse_block_sel(&flag_value(args, &mut i, "--block")?)?),
            "--command-done" => command_done = true,
            "--text" => text = Some(flag_value(args, &mut i, "--text")?),
            "--stdin" => stdin = true,
            "--guarded" => guarded = true,
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
        "ls" => PaneCmd::Ls { fno_id },
        "read" => PaneCmd::Read {
            pane: pane_arg("read")?,
            lines,
            block,
        },
        "split" => PaneCmd::Split {
            pane: pane_arg("split")?,
            direction: direction
                .ok_or_else(|| "pane split needs --direction <left|right|up|down>".to_string())?,
            focus,
        },
        "break" => PaneCmd::Break {
            pane: pane_arg("break")?,
            name: name.filter(|n| !n.trim().is_empty()),
        },
        "send" => {
            let pane = pane_arg("send")?;
            let source = match (text, stdin) {
                (Some(_), true) => return Err("pane send takes --text OR --stdin, not both".into()),
                (Some(t), false) => SendSource::Text(t),
                (None, true) => SendSource::Stdin,
                (None, false) => return Err("pane send needs --text <s> or --stdin".into()),
            };
            PaneCmd::Send {
                pane,
                source,
                guarded,
            }
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
                "unknown pane verb: {other} (ls|read|run|send|wait|kill|claim|release|split|break)"
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

/// Resolve `--session`/env, connect to the EXISTING server, run one control
/// verb, render the reply. The shared spine of the `tab`/`layout` porcelains
/// (x-d865); `where` has its own registry-first path.
fn run_on_existing_server(
    session_flag: Option<&str>,
    env_session: Option<&str>,
    json: bool,
    verb: ControlVerb,
) -> i32 {
    let session = resolve_session(session_flag, env_session);
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux: {e}");
            return EXIT_USAGE;
        }
    };
    let stream = match proto::connect_unix_timeout(&sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fno mux: cannot reach session {session:?}: {e}");
            return EXIT_ERROR;
        }
    };
    match send_control(stream, verb, CONTROL_TIMEOUT) {
        Ok(reply) => render_reply(reply, json, false, None),
        Err(e) => {
            eprintln!("fno mux: {e}");
            EXIT_ERROR
        }
    }
}

/// Split off a leading `--session <s>` / `--json` prefix shared by the small
/// `tab`/`layout` verbs, returning the rest for verb-specific parsing.
fn take_common_flags(args: &[OsString]) -> Result<(Option<String>, bool, Vec<String>), String> {
    let mut session = None;
    let mut json = false;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let tok = args[i]
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match tok {
            "--json" => json = true,
            "--session" => session = Some(flag_value(args, &mut i, "--session")?),
            other => rest.push(other.to_string()),
        }
        i += 1;
    }
    Ok((session, json, rest))
}

/// A `--squad <name>` -> `PaneTarget`, defaulting to `CurrentRoute`.
fn squad_target(squad: Option<String>) -> PaneTarget {
    squad
        .map(PaneTarget::SquadName)
        .unwrap_or(PaneTarget::CurrentRoute)
}

/// `fno mux tab ls|create|rename|join ...` (x-d865).
pub fn tab(args: &[OsString], env_session: Option<&str>) -> i32 {
    let verb = match args.first().and_then(|a| a.to_str()) {
        Some(v) => v.to_string(),
        None => {
            eprintln!("fno mux tab: needs a verb: ls|create|rename|join");
            return EXIT_USAGE;
        }
    };
    // Flag pass over the tokens after the verb.
    let mut session = None;
    let mut json = false;
    let mut squad = None;
    let mut name = None;
    let mut tab_sel = None;
    let mut src = None;
    let mut at = None;
    let mut dir = None;
    let mut i = 1;
    while i < args.len() {
        let tok = match args[i].to_str() {
            Some(t) => t,
            None => {
                eprintln!("fno mux tab: non-UTF-8 argument");
                return EXIT_USAGE;
            }
        };
        let res = (|| -> Result<(), String> {
            match tok {
                "--json" => json = true,
                "--session" => session = Some(flag_value(args, &mut i, "--session")?),
                "--squad" | "-s" => squad = Some(flag_value(args, &mut i, tok)?),
                "--name" => name = Some(flag_value(args, &mut i, "--name")?),
                "--tab" => tab_sel = Some(parse_tab_sel(&flag_value(args, &mut i, "--tab")?)?),
                "--src" => src = Some(parse_tab_sel(&flag_value(args, &mut i, "--src")?)?),
                "--at" => at = Some(parse_u64(&flag_value(args, &mut i, "--at")?, "--at")?),
                "--dir" | "--direction" | "-d" => {
                    dir = Some(parse_dir(&flag_value(args, &mut i, tok)?, tok)?)
                }
                t => return Err(format!("unknown flag: {t}")),
            }
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("fno mux tab: {e}");
            return EXIT_USAGE;
        }
        i += 1;
    }

    let verb = match verb.as_str() {
        "ls" => ControlVerb::TabLs {
            squad: squad_target(squad),
        },
        "create" => ControlVerb::TabCreate {
            squad: squad_target(squad),
            name: name.filter(|n| !n.trim().is_empty()),
        },
        "rename" => {
            let (Some(tab), Some(name)) = (tab_sel, name) else {
                eprintln!("fno mux tab rename: needs --tab <sel> and --name <s>");
                return EXIT_USAGE;
            };
            ControlVerb::TabRename {
                squad: squad_target(squad),
                tab,
                name,
            }
        }
        "join" => {
            let (Some(src_tab), Some(anchor_pane), Some(direction)) = (src, at, dir) else {
                eprintln!("fno mux tab join: needs --src <sel> --at <pane> --dir <dir>");
                return EXIT_USAGE;
            };
            ControlVerb::TabJoin {
                src_tab,
                anchor_pane,
                direction,
            }
        }
        other => {
            eprintln!("fno mux tab: unknown verb {other} (ls|create|rename|join)");
            return EXIT_USAGE;
        }
    };
    run_on_existing_server(session.as_deref(), env_session, json, verb)
}

/// `fno mux layout get [--squad <s>] [--tab <sel>]` (x-d865).
pub fn layout(args: &[OsString], env_session: Option<&str>) -> i32 {
    let (session, json, rest) = match take_common_flags(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno mux layout: {e}");
            return EXIT_USAGE;
        }
    };
    // The only verb is `get`; a bare `layout` also means get.
    let flags = match rest.first().map(String::as_str) {
        Some("get") | None => args,
        Some(other) => {
            eprintln!("fno mux layout: unknown verb {other} (get)");
            return EXIT_USAGE;
        }
    };
    let mut squad = None;
    let mut tab_sel = None;
    let mut i = 0;
    while i < flags.len() {
        let Some(tok) = flags[i].to_str() else {
            eprintln!("fno mux layout: non-UTF-8 argument");
            return EXIT_USAGE;
        };
        let res = (|| -> Result<(), String> {
            match tok {
                "get" | "--json" | "--session" => {
                    if tok == "--session" {
                        let _ = flag_value(flags, &mut i, "--session")?;
                    }
                }
                "--squad" | "-s" => squad = Some(flag_value(flags, &mut i, tok)?),
                "--tab" => tab_sel = Some(parse_tab_sel(&flag_value(flags, &mut i, "--tab")?)?),
                t => return Err(format!("unknown flag: {t}")),
            }
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("fno mux layout: {e}");
            return EXIT_USAGE;
        }
        i += 1;
    }
    let scope = match (squad, tab_sel) {
        (None, None) => LayoutScope::Session,
        (sq, None) => LayoutScope::Squad(squad_target(sq)),
        (sq, Some(tab)) => LayoutScope::Tab {
            squad: squad_target(sq),
            tab,
        },
    };
    run_on_existing_server(
        session.as_deref(),
        env_session,
        json,
        ControlVerb::LayoutGet { scope },
    )
}

/// `fno mux where <fno_id>` (x-d865): resolve an fno session id to its live
/// location. Reads the registry to find the hosting mux session, connects to
/// THAT session's socket, and rounds-trips one `PaneWhere`. The three failure
/// modes get distinct exit codes (AC1-ERR); a registry read failure never reads
/// as "not found".
pub fn where_(args: &[OsString], _env_session: Option<&str>) -> i32 {
    // The caller's FNO_SESSION is irrelevant here: `where` resolves the HOST
    // session from the registry, so an explicit --session is the only override.
    let (session_flag, json, rest) = match take_common_flags(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno mux where: {e}");
            return EXIT_USAGE;
        }
    };
    let Some(fno_id) = rest
        .first()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        eprintln!("fno mux where: needs an fno_id");
        return EXIT_USAGE;
    };

    // Read the registry to locate the hosting mux session. A read failure is
    // REGISTRY_UNAVAILABLE, never a silent "not found" (Locked Decision 4).
    let raw = match std::fs::read_to_string(crate::agents_view::registry_path()) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("fno mux where: no agent registry");
            return EXIT_REGISTRY_UNAVAILABLE;
        }
        Err(e) => {
            eprintln!("fno mux where: registry unreadable: {e}");
            return EXIT_REGISTRY_UNAVAILABLE;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Some(rows) = crate::agents_view::derive_rows(&raw, now) else {
        eprintln!("fno mux where: registry malformed");
        return EXIT_REGISTRY_UNAVAILABLE;
    };
    let id_match = |s: &Option<String>| {
        s.as_deref()
            .is_some_and(|v| v == fno_id || v.starts_with(&fno_id))
    };
    let matched: Vec<_> = rows
        .iter()
        .filter(|a| id_match(&a.session_id) || id_match(&a.harness_session_id))
        .collect();
    if matched.is_empty() {
        eprintln!("fno mux where: no session matches {fno_id:?}");
        return EXIT_NOT_FOUND;
    }
    // The hosting mux session name from the first pane-hosted match.
    let Some(host_session) = matched
        .iter()
        .find_map(|a| a.mux.as_ref().map(|(s, _)| s.clone()))
    else {
        eprintln!("fno mux where: {fno_id:?} hosts no live pane");
        return EXIT_NOT_PANE_HOSTED;
    };
    // Prefer the explicit --session only if the caller gave one; else the host.
    let session = resolve_session(session_flag.as_deref(), Some(&host_session));
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux where: {e}");
            return EXIT_USAGE;
        }
    };
    let stream = match proto::connect_unix_timeout(&sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fno mux where: cannot reach hosting session {session:?}: {e}");
            return EXIT_ERROR;
        }
    };
    match send_control(stream, ControlVerb::PaneWhere { fno_id }, CONTROL_TIMEOUT) {
        Ok(reply) => render_reply(reply, json, false, None),
        Err(e) => {
            eprintln!("fno mux where: {e}");
            EXIT_ERROR
        }
    }
}

/// Resolve `PaneCmd` -> a control verb + the read deadline, then run it.
fn dispatch(session: &str, sock: &Path, json: bool, cmd: PaneCmd) -> i32 {
    // (x-d865) `pane ls --fno-id` filters the listing client-side over the
    // reply's PaneInfo.fno_id, so capture the filter before `cmd` is consumed.
    let ls_fno_id = match &cmd {
        PaneCmd::Ls { fno_id } => fno_id.clone(),
        _ => None,
    };
    // `pane run` self-spawns a server for a script-only session (AC1-EDGE);
    // every other verb operates on an existing server. `pane ls` against no
    // server is "no panes" (exit 0); the rest are an error (nothing to act on).
    let (verb, read_timeout) = match cmd {
        PaneCmd::Ls { .. } => (ControlVerb::PaneLs, CONTROL_TIMEOUT),
        PaneCmd::Split {
            pane,
            direction,
            focus,
        } => (
            ControlVerb::PaneSplit {
                pane,
                direction,
                no_focus: !focus,
            },
            CONTROL_TIMEOUT,
        ),
        PaneCmd::Break { pane, name } => (ControlVerb::PaneBreak { pane, name }, CONTROL_TIMEOUT),
        PaneCmd::Read { pane, lines, block } => (
            ControlVerb::PaneRead { pane, lines, block },
            CONTROL_TIMEOUT,
        ),
        PaneCmd::Run {
            cwd,
            argv,
            claim,
            placement,
        } => {
            let cwd = resolve_run_cwd(cwd, std::env::current_dir().ok());
            (
                ControlVerb::PaneRun {
                    cwd,
                    argv,
                    cols: None,
                    rows: None,
                    claim,
                    placement,
                },
                CONTROL_TIMEOUT,
            )
        }
        PaneCmd::Send {
            pane,
            source,
            guarded,
        } => {
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
            (
                ControlVerb::PaneSend {
                    pane,
                    bytes,
                    guarded,
                },
                CONTROL_TIMEOUT,
            )
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
        Ok(reply) => render_reply(reply, json, command_done_requested, ls_fno_id.as_deref()),
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
fn render_reply(
    reply: ServerMsg,
    json: bool,
    command_done_requested: bool,
    ls_fno_id: Option<&str>,
) -> i32 {
    match reply {
        ServerMsg::PaneList { mut panes } => {
            // (x-d865) `pane ls --fno-id <id>` filters to panes carrying that id.
            if let Some(want) = ls_fno_id {
                panes.retain(|p| p.fno_id.as_deref() == Some(want));
            }
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
                    let fno = p.fno_id.as_deref().unwrap_or("-");
                    println!(
                        "{} squad={} tab={} pid={} fno_id={} cwd={}",
                        p.pane_id, p.squad_id, p.tab_id, pid, fno, p.cwd
                    );
                }
            }
            EXIT_OK
        }
        ServerMsg::TabSpawned { tab_id } => {
            // pane break receipt: EXACTLY the machine-readable new tab id.
            if json {
                println!("{}", serde_json::json!({ "tab_id": tab_id }));
            } else {
                println!("{tab_id}");
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
        ServerMsg::TabList { tabs } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&tabs).unwrap_or_else(|_| "[]".into())
                );
            } else {
                for t in &tabs {
                    let mark = if t.active { "*" } else { " " };
                    let name = t.name.as_deref().unwrap_or("-");
                    let panes = t
                        .pane_ids
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    println!("{mark} {} name={} panes={}", t.tab_id, name, panes);
                }
            }
            EXIT_OK
        }
        ServerMsg::LayoutTree { squads } => {
            // Machine-first: the nested tree + geometry is only meaningful as
            // JSON (a consumer diffs topology), so emit JSON regardless of flag.
            println!(
                "{}",
                serde_json::to_string(&squads).unwrap_or_else(|_| "[]".into())
            );
            EXIT_OK
        }
        ServerMsg::PaneLocation {
            fno_id,
            squad_id,
            squad_name,
            tabs,
            panes,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "fno_id": fno_id,
                        "squad_id": squad_id,
                        "squad_name": squad_name,
                        "tabs": tabs.iter().map(|(id, n)| serde_json::json!({"tab_id": id, "name": n})).collect::<Vec<_>>(),
                        "panes": panes,
                    })
                );
            } else {
                let sq = squad_name.as_deref().unwrap_or("-");
                let tab_ids = tabs
                    .iter()
                    .map(|(id, _)| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let pane_ids = panes
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                println!("{fno_id} squad={squad_id} ({sq}) tabs={tab_ids} panes={pane_ids}");
            }
            EXIT_OK
        }
        ServerMsg::Err { code, msg } => {
            eprintln!("fno mux: {msg}");
            // Each error class the CLI can act on gets its OWN exit code so a
            // script can branch: BLOCK_UNAVAILABLE (AC2-ERR), TARGET_NOT_IDLE (a
            // guarded send bounced), and the three `where` outcomes (x-d865:
            // NOT_FOUND / NOT_PANE_HOSTED / REGISTRY_UNAVAILABLE stay distinct so
            // a script never conflates "no such id" with "id has no live pane").
            if code == err_code::BLOCK_UNAVAILABLE {
                EXIT_BLOCK_UNAVAILABLE
            } else if code == err_code::TARGET_NOT_IDLE {
                EXIT_TARGET_NOT_IDLE
            } else if code == err_code::NOT_FOUND {
                EXIT_NOT_FOUND
            } else if code == err_code::NOT_PANE_HOSTED {
                EXIT_NOT_PANE_HOSTED
            } else if code == err_code::REGISTRY_UNAVAILABLE {
                EXIT_REGISTRY_UNAVAILABLE
            } else {
                EXIT_ERROR
            }
        }
        // The server only ever answers a control connection with the replies
        // above; anything else is a protocol violation.
        other => {
            eprintln!("fno mux: unexpected server reply: {other:?}");
            EXIT_ERROR
        }
    }
}

// ---------------------------------------------------------------------------
// `fno mux block pipe` - cross-pane block piping porcelain (x-fe8f)
// ---------------------------------------------------------------------------

/// A parsed `block pipe` invocation. Pure-parse struct, mirrors [`ParsedPane`].
#[derive(Debug, PartialEq, Eq)]
struct ParsedBlockPipe {
    session: Option<String>,
    json: bool,
    from: u64,
    to: u64,
    block: BlockSel,
    force: bool,
}

/// Parse the tokens after `mux block` into a [`ParsedBlockPipe`]. Pure, so the
/// grammar is unit-testable without a socket. `pipe` is the only block verb.
fn parse_block_args(args: &[OsString]) -> Result<ParsedBlockPipe, String> {
    let verb = args
        .first()
        .and_then(|a| a.to_str())
        .ok_or_else(|| "block needs a verb: pipe | annotate".to_string())?;
    if verb != "pipe" {
        return Err(format!("unknown block verb: {verb} (pipe | annotate)"));
    }
    let mut session = None;
    let mut json = false;
    let mut force = false;
    let mut from = None;
    let mut to = None;
    let mut block = BlockSel::Last;
    let mut i = 1;
    while i < args.len() {
        let tok = args[i]
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match tok {
            "--json" => json = true,
            "--force" => force = true,
            "--session" => session = Some(flag_value(args, &mut i, "--session")?),
            "--from" => from = Some(parse_u64(&flag_value(args, &mut i, "--from")?, "--from")?),
            "--to" => to = Some(parse_u64(&flag_value(args, &mut i, "--to")?, "--to")?),
            "--block" => block = parse_block_sel(&flag_value(args, &mut i, "--block")?)?,
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(ParsedBlockPipe {
        session,
        json,
        from: from.ok_or("block pipe needs --from <pane>")?,
        to: to.ok_or("block pipe needs --to <pane>")?,
        block,
        force,
    })
}

/// Data-integrity gate on the source block's metadata: an open (still
/// running) or byte-cap-truncated block must never pipe - partial text is
/// worse than no pipe (the verb's contract). Distinct from the idle guard:
/// `--force` does NOT bypass this (an incomplete block does not become
/// complete because a human insists; wait or use `pane read` directly).
fn pipe_block_gate(meta: Option<&proto::BlockMeta>) -> Result<(), String> {
    let Some(m) = meta else { return Ok(()) };
    let seq = m.seq.map(|s| format!("#{s}")).unwrap_or_else(|| "?".into());
    if !m.complete {
        return Err(format!(
            "source block {seq} is still running - wait for the command to finish"
        ));
    }
    if m.truncated {
        return Err(format!(
            "source block {seq} was truncated by the byte cap - refusing to pipe partial text"
        ));
    }
    // A markerless pane has no command boundaries, so `--block last` degrades
    // to the whole-scrollback implicit block - which reports complete=true
    // even while a command is still running (there is no D marker to prove
    // otherwise). block pipe is a TYPED-block verb (the node's contract), so
    // refuse the implicit block rather than pipe unbounded, possibly
    // mid-command screen text at exit 0.
    if m.implicit {
        return Err(
            "source pane emits no command markers - block pipe needs a typed block \
             (enable OSC 133 shell integration: `fno mux shell-init`)"
                .to_string(),
        );
    }
    Ok(())
}

/// One control round-trip on a fresh one-shot connection (the pane verbs'
/// connect + [`send_control`], factored so `block pipe` can do two in a row).
fn control_roundtrip(sock: &Path, session: &str, verb: ControlVerb) -> Result<ServerMsg, String> {
    let stream = std::os::unix::net::UnixStream::connect(sock)
        .map_err(|e| format!("cannot reach session {session:?}: {e}"))?;
    send_control(stream, verb, CONTROL_TIMEOUT)
}

/// `fno mux block pipe --from <pane> --to <pane> [--block last|<seq>] [--json]
/// [--force]`: read a COMPLETED block from the source pane and land its text
/// in the target pane's input. Porcelain over `pane read --block` + `pane
/// send` - no new capability, one verb. Trailing newlines are stripped so the
/// pipe fills the input line and never submits it.
fn block_pipe(args: &[OsString], env_session: Option<&str>) -> i32 {
    let parsed = match parse_block_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux block: {e}");
            return EXIT_USAGE;
        }
    };
    let session = resolve_session(parsed.session.as_deref(), env_session);
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux block: {e}");
            return EXIT_USAGE;
        }
    };

    // 1. Read the source block. EXIT_BLOCK_UNAVAILABLE propagates verbatim -
    //    an evicted/nonexistent block must never pipe wrong or truncated text.
    let (text, meta) = match control_roundtrip(
        &sock,
        &session,
        ControlVerb::PaneRead {
            pane: parsed.from,
            lines: None,
            block: Some(parsed.block),
        },
    ) {
        Ok(ServerMsg::PaneText { text, block, .. }) => (text, block),
        Ok(ServerMsg::Err { code, msg }) => {
            eprintln!("fno mux block: {msg}");
            return if code == err_code::BLOCK_UNAVAILABLE {
                EXIT_BLOCK_UNAVAILABLE
            } else {
                EXIT_ERROR
            };
        }
        Ok(other) => {
            eprintln!("fno mux block: unexpected server reply: {other:?}");
            return EXIT_ERROR;
        }
        Err(e) => {
            eprintln!("fno mux block: {e}");
            return EXIT_ERROR;
        }
    };
    // 1b. Only a completed, typed, untruncated block pipes: open, truncated,
    //     and markerless-implicit blocks all refuse here (partial or unbounded
    //     text is worse than no pipe).
    if let Err(why) = pipe_block_gate(meta.as_ref()) {
        eprintln!("fno mux block: {why}");
        return EXIT_BLOCK_UNAVAILABLE;
    }
    let seq = meta.as_ref().and_then(|m| m.seq);

    // 2. Land the text via the server-side atomic guarded send. The idle check
    //    (agent-busy + writer-claim interlock) now runs on the server under the
    //    pane lock immediately before the write, so there is no client-side
    //    read->send TOCTOU and no dependence on the client and server agreeing
    //    on HOME/registry path. `--force` sends the raw unguarded PaneSend.
    if parsed.force {
        eprintln!("fno mux block: --force: skipping the receive-side idle guard");
    }
    let bytes = text.trim_end_matches(['\r', '\n']).as_bytes().to_vec();
    let sent = bytes.len();
    match control_roundtrip(
        &sock,
        &session,
        ControlVerb::PaneSend {
            pane: parsed.to,
            bytes,
            guarded: !parsed.force,
        },
    ) {
        Ok(ServerMsg::Ok) => {}
        Ok(ServerMsg::Err { code, msg }) if code == err_code::TARGET_NOT_IDLE => {
            eprintln!("fno mux block: {msg} - rerun with --force to override");
            return EXIT_TARGET_NOT_IDLE;
        }
        Ok(ServerMsg::Err { msg, .. }) => {
            eprintln!("fno mux block: {msg}");
            return EXIT_ERROR;
        }
        Ok(other) => {
            eprintln!("fno mux block: unexpected server reply: {other:?}");
            return EXIT_ERROR;
        }
        Err(e) => {
            eprintln!("fno mux block: {e}");
            return EXIT_ERROR;
        }
    }

    if parsed.json {
        println!(
            "{}",
            serde_json::json!({
                "from": parsed.from,
                "to": parsed.to,
                "block_seq": seq,
                "bytes": sent,
                "forced": parsed.force,
            })
        );
    } else {
        let seq_s = seq.map(|s| format!("#{s}")).unwrap_or_else(|| "?".into());
        println!(
            "piped block {seq_s} ({sent} bytes) pane {} -> pane {}",
            parsed.from, parsed.to
        );
    }
    EXIT_OK
}

// ---------------------------------------------------------------------------
// `fno mux block annotate` - operator review-finding capture porcelain (x-f8d4)
// ---------------------------------------------------------------------------

/// Cap for the block excerpt carried into the finding: the command line + head
/// of output. A truncated excerpt is worse than none for a reviewer (same
/// contract as `pipe_block_gate`), so the block itself must be complete; this
/// cap only bounds how much of a large completed block rides into the event.
const ANNOTATE_EXCERPT_CAP: usize = 2048;

/// A parsed `block annotate` invocation. Pure-parse struct, mirrors
/// [`ParsedBlockPipe`]. `node` carries the backlog node the finding is scoped
/// to (the caller supplies the pane's server-tracked `FNO_NODE`, surfaced to
/// the mux client as `Layout::focus_node`); the porcelain never guesses it.
#[derive(Debug, PartialEq, Eq)]
struct ParsedBlockAnnotate {
    session: Option<String>,
    from: u64,
    block: BlockSel,
    node: String,
    message: String,
}

/// Parse the tokens after `mux block annotate` into a [`ParsedBlockAnnotate`].
/// Pure, so the grammar is unit-testable without a socket. `--node` and `-m`
/// are required; a missing `--node` is the "specify the node" refusal (a
/// non-agent pane has no provenance to resolve, so the caller must name it).
fn parse_block_annotate(args: &[OsString]) -> Result<ParsedBlockAnnotate, String> {
    // args[0] is the "annotate" verb (block() already routed on it).
    let mut session = None;
    let mut from = None;
    let mut block = BlockSel::Last;
    let mut node = None;
    let mut message = None;
    let mut i = 1;
    while i < args.len() {
        let tok = args[i]
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match tok {
            "--session" => session = Some(flag_value(args, &mut i, "--session")?),
            "--from" => from = Some(parse_u64(&flag_value(args, &mut i, "--from")?, "--from")?),
            "--block" => block = parse_block_sel(&flag_value(args, &mut i, "--block")?)?,
            "--node" => node = Some(flag_value(args, &mut i, "--node")?),
            "--message" | "-m" => message = Some(flag_value(args, &mut i, "--message")?),
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    let message = message.ok_or("block annotate needs -m <text>")?;
    if message.trim().is_empty() {
        return Err("block annotate: --message is empty".to_string());
    }
    Ok(ParsedBlockAnnotate {
        session,
        from: from.ok_or("block annotate needs --from <pane>")?,
        block,
        node: node.ok_or(
            "block annotate needs --node <id> (a pane's node cannot be guessed; \
             pass the node whose work this pane holds)",
        )?,
        message,
    })
}

/// `fno mux block annotate --from <pane> [--block last|<seq>] -m <text> --node
/// <id> [--session]`: read a COMPLETED block from the source pane and record it
/// as an operator review finding against `--node` via `fno annotate add`.
/// Unlike `block pipe` there is NO target-idle guard (nothing enters a
/// recipient PTY - delivery is a mail inject the daemon queues); it reuses the
/// same typed-block gate (an open/truncated/markerless block refuses) and caps
/// the excerpt. Shells the Python core (Locked Decision 4: routing lives there,
/// not duplicated in Rust) and propagates its receipt + exit code.
fn block_annotate(args: &[OsString], env_session: Option<&str>) -> i32 {
    let parsed = match parse_block_annotate(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux block: {e}");
            return EXIT_USAGE;
        }
    };
    let session = resolve_session(parsed.session.as_deref(), env_session);
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux block: {e}");
            return EXIT_USAGE;
        }
    };

    // 1. Read the source block. EXIT_BLOCK_UNAVAILABLE propagates verbatim.
    let (text, meta) = match control_roundtrip(
        &sock,
        &session,
        ControlVerb::PaneRead {
            pane: parsed.from,
            lines: None,
            block: Some(parsed.block),
        },
    ) {
        Ok(ServerMsg::PaneText { text, block, .. }) => (text, block),
        Ok(ServerMsg::Err { code, msg }) => {
            eprintln!("fno mux block: {msg}");
            return if code == err_code::BLOCK_UNAVAILABLE {
                EXIT_BLOCK_UNAVAILABLE
            } else {
                EXIT_ERROR
            };
        }
        Ok(other) => {
            eprintln!("fno mux block: unexpected server reply: {other:?}");
            return EXIT_ERROR;
        }
        Err(e) => {
            eprintln!("fno mux block: {e}");
            return EXIT_ERROR;
        }
    };
    // 1b. AC1-ERR: only a completed, typed, untruncated block annotates; an
    //     open/truncated/markerless block refuses (a partial excerpt misleads
    //     the reviewer the same way a partial pipe does).
    if let Err(why) = pipe_block_gate(meta.as_ref()) {
        eprintln!("fno mux block: {why}");
        return EXIT_BLOCK_UNAVAILABLE;
    }

    // 2. Cap the excerpt (command line + head of output). Piped via stdin below,
    //    so no world-readable temp file is ever staged (CWE-377; gemini/codex
    //    review) and there is nothing to clean up.
    let excerpt = cap_excerpt(&text, ANNOTATE_EXCERPT_CAP);

    // 2b. Resolve the --from pane's cwd so `fno annotate add` records the finding
    //     into THAT worktree's .fno/events.jsonl - the one loop-check reads for
    //     the node - not the caller's cwd (codex P1: a mismatched cwd lands the
    //     durable finding in the wrong project and never gates). Best-effort: on
    //     a PaneLs miss inherit the caller cwd (the mail inject still lands).
    let pane_cwd = pane_cwd_via_ls(&sock, &session, parsed.from);

    // 3. Shell the Python core (`fno annotate add`) via this binary's own `fno`
    //    entrypoint (the Rust shim forwards `annotate` to the wheel CLI), so the
    //    finding recording + claim-holder delivery ladder lives in one place. The
    //    excerpt rides stdin (`--block-excerpt-file -`), never a temp file.
    let fno = std::env::current_exe().unwrap_or_else(|_| "fno".into());
    let mut cmd = std::process::Command::new(&fno);
    cmd.args([
        OsString::from("annotate"),
        OsString::from("add"),
        OsString::from("--node"),
        OsString::from(&parsed.node),
        OsString::from("--message"),
        OsString::from(&parsed.message),
        OsString::from("--block-excerpt-file"),
        OsString::from("-"),
    ])
    .stdin(std::process::Stdio::piped());
    if let Some(cwd) = pane_cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fno mux block: cannot run `fno annotate add`: {e}");
            return EXIT_ERROR;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        // A broken pipe (child exited early) is not fatal: the child's own exit
        // code below is the authority on what happened.
        let _ = stdin.write_all(excerpt.as_bytes());
    }
    match child.wait() {
        Ok(s) => s.code().unwrap_or(EXIT_ERROR),
        Err(e) => {
            eprintln!("fno mux block: cannot run `fno annotate add`: {e}");
            EXIT_ERROR
        }
    }
}

/// Resolve `pane`'s cwd via a `PaneLs` round-trip. Returns `None` on any miss
/// (unreachable server, pane absent, empty cwd) so the caller degrades to the
/// inherited cwd rather than failing the annotation outright.
fn pane_cwd_via_ls(sock: &Path, session: &str, pane: u64) -> Option<String> {
    match control_roundtrip(sock, session, ControlVerb::PaneLs) {
        Ok(ServerMsg::PaneList { panes }) => panes
            .into_iter()
            .find(|p| p.pane_id == pane)
            .map(|p| p.cwd)
            .filter(|c| !c.is_empty()),
        _ => None,
    }
}

/// Keep the head of `text` within `cap` bytes on a char boundary, appending a
/// truncation marker when it was cut. The head is the command line + start of
/// output - a reviewer needs the top of a block, not its tail.
fn cap_excerpt(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... [truncated to {cap} bytes]", &text[..end])
}

/// `fno mux block <verb> ...`: route the block verb family. `pipe` (x-fe8f)
/// pipes a completed block into another pane's input; `annotate` (x-f8d4)
/// records it as an operator review finding.
pub fn block(args: &[OsString], env_session: Option<&str>) -> i32 {
    match args.first().and_then(|a| a.to_str()) {
        Some("pipe") => block_pipe(args, env_session),
        Some("annotate") => block_annotate(args, env_session),
        Some(v) => {
            eprintln!("fno mux block: unknown block verb: {v} (pipe | annotate)");
            EXIT_USAGE
        }
        None => {
            eprintln!("fno mux block: block needs a verb: pipe | annotate");
            EXIT_USAGE
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
            // A live server on the current wire (not stale).
            wire_version: Some(proto::PROTO_VERSION),
        }
    }

    fn stale(name: &str) -> SessionRow {
        SessionRow {
            name: name.into(),
            probe: Probe::Stale,
            wire_version: None,
        }
    }

    fn wedged(name: &str) -> SessionRow {
        SessionRow {
            name: name.into(),
            probe: Probe::Wedged,
            wire_version: None,
        }
    }

    #[test]
    fn mux_ls_json_state_contract() {
        // The `state` string is the contract `fno restart --mux` reads; a wedged
        // row is what flips restart to a non-zero exit (x-82c6), split from the
        // (still-live) unqueryable old-build row.
        assert_eq!(session_row_json(&live("s"))["state"], "live");
        assert_eq!(session_row_json(&stale("s"))["state"], "stale");
        let w = session_row_json(&wedged("s"));
        assert_eq!(w["state"], "wedged");
        assert!(w.get("log").is_some(), "a wedged row carries its log path");
        // A wedged row is NOT live: it must never be auto-attached.
        assert!(!wedged("s").is_live());
    }

    #[test]
    fn mux_ls_flags_stale_wire_live_server() {
        // x-1a85: a live server whose sidecar version != the running binary's
        // (or is absent) is `stale` - a current client can't handshake it, so
        // `fno restart` auto-restarts it. A current-version live server is not.
        let current = live("cur"); // wire_version = PROTO_VERSION
        assert!(!current.wire_stale(), "same wire is not stale");
        assert_eq!(session_row_json(&current)["stale"], false);

        let mut older = live("old");
        older.wire_version = Some(proto::PROTO_VERSION - 1);
        assert!(older.wire_stale(), "an older wire is stale");
        assert_eq!(session_row_json(&older)["stale"], true);

        let mut unstamped = live("pre");
        unstamped.wire_version = None; // a pre-sidecar (older) build
        assert!(unstamped.wire_stale(), "a missing sidecar reads as stale");
        assert_eq!(
            session_row_json(&unstamped)["wire_version"],
            serde_json::Value::Null
        );

        // Non-live rows are never wire-stale (they are dead/wedged, not skewed).
        assert!(!stale("d").wire_stale());
        assert!(!wedged("w").wire_stale());
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
                cmd: PaneCmd::Ls { fno_id: None }
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
    fn mux_pane_parse_split_break_and_ls_fno_id() {
        // (x-d865) split needs a direction; --focus opts into focus.
        assert_eq!(
            parse_pane_args(&os(&["split", "5", "--direction", "right"]))
                .unwrap()
                .cmd,
            PaneCmd::Split {
                pane: 5,
                direction: Dir::Right,
                focus: false,
            }
        );
        assert_eq!(
            parse_pane_args(&os(&["split", "5", "-d", "up", "--focus"]))
                .unwrap()
                .cmd,
            PaneCmd::Split {
                pane: 5,
                direction: Dir::Up,
                focus: true,
            }
        );
        assert!(
            parse_pane_args(&os(&["split", "5"])).is_err(),
            "split without --direction is a usage error"
        );
        assert_eq!(
            parse_pane_args(&os(&["break", "9", "--name", "solo"]))
                .unwrap()
                .cmd,
            PaneCmd::Break {
                pane: 9,
                name: Some("solo".into()),
            }
        );
        assert_eq!(
            parse_pane_args(&os(&["ls", "--fno-id", "abc123"]))
                .unwrap()
                .cmd,
            PaneCmd::Ls {
                fno_id: Some("abc123".into()),
            }
        );
    }

    #[test]
    fn mux_pane_parse_run_tab_and_anchor() {
        // AC2-HP: `run --tab id:10 --at 2 --split down -- <argv>`.
        let p = parse_pane_args(&os(&[
            "run", "--tab", "id:10", "--at", "2", "--split", "down", "--", "/bin/cat",
        ]))
        .unwrap();
        let PaneCmd::Run { placement, .. } = p.cmd else {
            panic!("expected Run");
        };
        assert_eq!(placement.tab, Some(TabSel::Id(10)));
        assert_eq!(placement.at, Some(2));
        assert_eq!(placement.split, Some(Dir::Down));
    }

    #[test]
    fn parse_tab_sel_grammar() {
        assert_eq!(parse_tab_sel("active").unwrap(), TabSel::Active);
        assert_eq!(parse_tab_sel("new").unwrap(), TabSel::New);
        assert_eq!(parse_tab_sel("3").unwrap(), TabSel::Index(3));
        assert_eq!(parse_tab_sel("id:7").unwrap(), TabSel::Id(7));
        assert_eq!(
            parse_tab_sel("name:bee").unwrap(),
            TabSel::Name("bee".into())
        );
        assert_eq!(parse_tab_sel("bee").unwrap(), TabSel::Name("bee".into()));
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
                    placement: PanePlacement::default(),
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
    fn mux_pane_parse_run_accepts_typed_placement_before_argv() {
        let p = parse_pane_args(&os(&[
            "run", "squad", "review", "split", "left", "claude", "--print",
        ]))
        .unwrap();
        assert!(matches!(
            p.cmd,
            PaneCmd::Run {
                placement: PanePlacement {
                    tab: None,
                    at: None,
                    target: PaneTarget::SquadName(ref name),
                    split: Some(crate::tree::Dir::Left),
                    ..
                },
                ref argv,
                ..
            } if name == "review" && argv == &["claude", "--print"]
        ));
        let aliases =
            parse_pane_args(&os(&["run", "-s", "review", "-x", "right", "--", "echo"])).unwrap();
        assert!(matches!(
            aliases.cmd,
            PaneCmd::Run {
                placement: PanePlacement {
                    tab: None,
                    at: None,
                    target: PaneTarget::SquadName(ref name),
                    split: Some(crate::tree::Dir::Right),
                    ..
                },
                ..
            } if name == "review"
        ));
        let long = parse_pane_args(&os(&[
            "run", "--squad", "review", "--split", "up", "--", "echo",
        ]))
        .unwrap();
        assert!(matches!(
            long.cmd,
            PaneCmd::Run {
                placement: PanePlacement {
                    tab: None,
                    at: None,
                    target: PaneTarget::SquadName(ref name),
                    split: Some(crate::tree::Dir::Up),
                    ..
                },
                ..
            } if name == "review"
        ));
        assert!(parse_pane_args(&os(&["run", "squad", " ", "--", "echo"])).is_err());
        assert!(parse_pane_args(&os(&["run", "split", "diagonal", "--", "echo"])).is_err());
        assert!(parse_pane_args(&os(&["run", "--target", "review", "--", "echo"])).is_err());
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
                source: SendSource::Text("hi\r".into()),
                guarded: false,
            }
        );
        assert_eq!(
            parse_pane_args(&os(&["send", "2", "--stdin"])).unwrap().cmd,
            PaneCmd::Send {
                pane: 2,
                source: SendSource::Stdin,
                guarded: false,
            }
        );
        // --guarded opts the send into the server-side turn-taken interlock.
        assert_eq!(
            parse_pane_args(&os(&["send", "2", "--stdin", "--guarded"]))
                .unwrap()
                .cmd,
            PaneCmd::Send {
                pane: 2,
                source: SendSource::Stdin,
                guarded: true,
            }
        );
        // Neither / both are usage errors.
        assert!(parse_pane_args(&os(&["send", "2"])).is_err());
        assert!(parse_pane_args(&os(&["send", "2", "--text", "x", "--stdin"])).is_err());
    }

    #[test]
    fn mux_pane_guarded_send_not_idle_maps_to_target_not_idle_exit() {
        // A guarded send the server refuses (pane's turn not takeable) must
        // carry its own exit code so a mail sender demotes to durable rather
        // than the generic error, and never miscalls a stalled paste delivered.
        let refused = ServerMsg::Err {
            code: err_code::TARGET_NOT_IDLE,
            msg: "receiving agent not idle".into(),
        };
        assert_eq!(
            render_reply(refused, false, false, None),
            EXIT_TARGET_NOT_IDLE
        );
        // A different error class still collapses to the generic error code.
        let other = ServerMsg::Err {
            code: err_code::DEAD_PANE,
            msg: "no such pane".into(),
        };
        assert_eq!(render_reply(other, false, false, None), EXIT_ERROR);
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

    // -- block pipe (x-fe8f) -------------------------------------------------

    #[test]
    fn block_pipe_parses_flags_and_defaults_to_last() {
        assert_eq!(
            parse_block_args(&os(&["pipe", "--from", "4", "--to", "2"])),
            Ok(ParsedBlockPipe {
                session: None,
                json: false,
                from: 4,
                to: 2,
                block: BlockSel::Last,
                force: false,
            })
        );
        assert_eq!(
            parse_block_args(&os(&[
                "pipe",
                "--from",
                "4",
                "--to",
                "2",
                "--block",
                "7",
                "--session",
                "work",
                "--json",
                "--force",
            ])),
            Ok(ParsedBlockPipe {
                session: Some("work".into()),
                json: true,
                from: 4,
                to: 2,
                block: BlockSel::Seq(7),
                force: true,
            })
        );
    }

    // -- block annotate (x-f8d4) --------------------------------------------

    #[test]
    fn block_annotate_parses_required_flags() {
        assert_eq!(
            parse_block_annotate(&os(&[
                "annotate",
                "--from",
                "3",
                "--node",
                "x-1",
                "-m",
                "off-by-one",
            ])),
            Ok(ParsedBlockAnnotate {
                session: None,
                from: 3,
                block: BlockSel::Last,
                node: "x-1".into(),
                message: "off-by-one".into(),
            })
        );
        // --block seq + --session + long --message.
        assert_eq!(
            parse_block_annotate(&os(&[
                "annotate",
                "--from",
                "3",
                "--block",
                "7",
                "--session",
                "work",
                "--node",
                "x-2",
                "--message",
                "fix it",
            ])),
            Ok(ParsedBlockAnnotate {
                session: Some("work".into()),
                from: 3,
                block: BlockSel::Seq(7),
                node: "x-2".into(),
                message: "fix it".into(),
            })
        );
    }

    #[test]
    fn block_annotate_usage_errors() {
        // AC2-ERR: a missing --node is a refusal (no guessing the pane's node).
        assert!(parse_block_annotate(&os(&["annotate", "--from", "3", "-m", "x"])).is_err());
        // Missing --from, missing -m, empty -m, unknown flag all refuse.
        assert!(parse_block_annotate(&os(&["annotate", "--node", "x-1", "-m", "x"])).is_err());
        assert!(parse_block_annotate(&os(&["annotate", "--from", "3", "--node", "x-1"])).is_err());
        assert!(parse_block_annotate(&os(&[
            "annotate", "--from", "3", "--node", "x-1", "-m", "  "
        ]))
        .is_err());
        assert!(parse_block_annotate(&os(&[
            "annotate", "--from", "3", "--node", "x-1", "-m", "x", "--oops",
        ]))
        .is_err());
    }

    #[test]
    fn block_verb_dispatch_rejects_unknown() {
        // The verb enumeration now carries both verbs.
        let e = parse_block_args(&os(&["rerun"])).unwrap_err();
        assert!(
            e.contains("annotate"),
            "verb list must enumerate annotate: {e}"
        );
    }

    #[test]
    fn cap_excerpt_keeps_head_and_marks_truncation() {
        assert_eq!(cap_excerpt("short", 2048), "short"); // under cap: verbatim
        let big = "x".repeat(3000);
        let capped = cap_excerpt(&big, 2048);
        assert!(capped.starts_with(&"x".repeat(2048)));
        assert!(capped.contains("truncated to 2048 bytes"));
        // char-boundary safe on multibyte input.
        let multi = "é".repeat(2000); // 2 bytes each -> 4000 bytes
        let c = cap_excerpt(&multi, 2048);
        assert!(c.is_char_boundary(c.len().min(2048)));
    }

    #[test]
    fn block_pipe_usage_errors() {
        // Missing --from / --to, an unknown flag, and an unknown verb are all
        // parse errors (exit 2 at the verb), never a partial pipe.
        assert!(parse_block_args(&os(&["pipe", "--to", "2"])).is_err());
        assert!(parse_block_args(&os(&["pipe", "--from", "4"])).is_err());
        assert!(parse_block_args(&os(&["pipe", "--from", "4", "--to", "2", "--oops"])).is_err());
        assert!(parse_block_args(&os(&["rerun"])).is_err());
        assert!(parse_block_args(&os(&["pipe", "--from", "x", "--to", "2"])).is_err());
    }

    #[test]
    fn block_pipe_gate_refuses_open_truncated_and_implicit_blocks() {
        let meta =
            |seq: Option<u64>, complete: bool, truncated: bool, implicit: bool| proto::BlockMeta {
                seq,
                exit: Some(0),
                complete,
                truncated,
                implicit,
            };
        // Only a completed, untruncated, TYPED block pipes.
        assert_eq!(
            pipe_block_gate(Some(&meta(Some(7), true, false, false))),
            Ok(())
        );
        // No metadata (plain read shape) has nothing to refuse on.
        assert_eq!(pipe_block_gate(None), Ok(()));
        // Open (still running) and byte-cap-truncated blocks never pipe.
        let err = pipe_block_gate(Some(&meta(Some(7), false, false, false))).unwrap_err();
        assert!(err.contains("still running"), "{err}");
        let err = pipe_block_gate(Some(&meta(Some(7), true, true, false))).unwrap_err();
        assert!(err.contains("truncated"), "{err}");
        // A markerless implicit block (whole scrollback, complete=true even
        // mid-command) is refused: block pipe is a typed-block verb.
        let err = pipe_block_gate(Some(&meta(None, true, false, true))).unwrap_err();
        assert!(err.contains("no command markers"), "{err}");
    }
}
