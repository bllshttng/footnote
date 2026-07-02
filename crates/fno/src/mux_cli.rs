//! The `fno mux ls | kill-server` verbs: plain client-process code over the
//! frozen pre-Attach protocol pair (`Query`/`Info`, `KillServer`). These run
//! and exit - no TUI, no raw mode, no attach - so every probe is bounded by
//! a read/write timeout and a bad session can never hang the listing
//! (AC4-ERR).

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::proto::{
    self, read_msg_sync, write_msg_sync, ClientMsg, ControlVerb, ServerMsg, WaitOutcome,
    BUILD_VERSION, DEFAULT_SESSION, PROTO_VERSION,
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
    let stream = match std::os::unix::net::UnixStream::connect(sock) {
        Ok(s) => s,
        // Only a refused connection proves nothing listens; every other
        // error (EMFILE, EACCES, ...) is OUR failure, not the server's.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Probe::Stale,
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

/// `fno mux ls`: one row per `*.sock` in the mux dir. Read-only - a stale
/// socket is REPORTED, never unlinked (kill-server owns removal). Exits 0
/// even when every row is stale or unqueryable; only "no sessions" is
/// distinguishable by text, not exit code, so scripts can `grep`.
pub fn ls() -> i32 {
    let dir = proto::mux_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // A missing dir means no session ever started here. Any OTHER error
        // (permissions, I/O) must not read as an empty listing - a script
        // grepping "no sessions" would get a clean false negative.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no sessions");
            return 0;
        }
        Err(e) => {
            eprintln!("fno: cannot read {}: {e}", dir.display());
            return 1;
        }
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
    if names.is_empty() {
        println!("no sessions");
        return 0;
    }
    names.sort();
    for name in names {
        let sock = dir.join(format!("{name}.sock"));
        match probe(&sock) {
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
    0
}

/// `fno mux kill-server [<name>]`: shut one session down. A live server Byes
/// its clients, kills every pane child, and exits (its SocketGuard unlinks
/// the socket); a stale socket is unlinked here with a message (exit 0); no
/// socket at all is "no server" (exit 1).
pub fn kill_server(session: &str) -> i32 {
    let sock = match proto::socket_path(session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno: {e}");
            return 2;
        }
    };
    if !sock.exists() {
        eprintln!("fno: no server for session {session:?}");
        return 1;
    }
    let stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        // Only a REFUSED connection (or a socket that vanished mid-race)
        // proves the server is dead. Any other connect error is client-side
        // (fd exhaustion, permissions) - unlinking on it would orphan a LIVE
        // server: still running, unreachable by name, invisible to ls.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            // AC4-EDGE: dead server left its socket behind - take it out.
            return match std::fs::remove_file(&sock) {
                Ok(()) => {
                    println!("removed stale socket for session {session:?} (server was dead)");
                    0
                }
                Err(e) => {
                    eprintln!("fno: cannot remove stale socket {}: {e}", sock.display());
                    1
                }
            };
        }
        Err(e) => {
            eprintln!("fno: cannot connect to {}: {e}", sock.display());
            return 1;
        }
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fno: socket setup failed: {e}");
            return 1;
        }
    };
    if write_msg_sync(&mut w, &ClientMsg::KillServer).is_err() {
        eprintln!("fno: could not reach the server for session {session:?}");
        return 1;
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
        return 1;
    }
    println!("killed session {session:?}");
    0
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

/// Default `pane wait` deadline when `--timeout` is omitted. There is never an
/// infinite wait (Failure Modes: every wait is bounded).
const DEFAULT_WAIT_TIMEOUT_S: u64 = 30;

/// How long to wait for a non-`wait` reply. A `wait` reply gets its own
/// deadline (`timeout_ms` + slack) so the bounded server wait is never cut
/// short by the client's read timeout.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

fn wait_exit_code(outcome: WaitOutcome) -> i32 {
    match outcome {
        WaitOutcome::Quiet => EXIT_OK,
        WaitOutcome::Matched => EXIT_WAIT_MATCHED,
        WaitOutcome::Timeout => EXIT_WAIT_TIMEOUT,
        WaitOutcome::PaneExited => EXIT_WAIT_EXITED,
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
    },
    Run {
        cwd: Option<String>,
        argv: Vec<String>,
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
    },
    Kill {
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

/// Parse the tokens after `mux pane` into a [`ParsedPane`]. Pure, so the whole
/// grammar (verbs, flags, the exit-code-bearing outcomes) is unit-testable
/// without a socket.
fn parse_pane_args(args: &[OsString]) -> Result<ParsedPane, String> {
    let verb = args
        .first()
        .and_then(|a| a.to_str())
        .ok_or_else(|| "pane needs a verb: ls|read|run|send|wait|kill".to_string())?;

    // `run` is special: leading flags, then the command argv verbatim (its own
    // flags are NOT ours to parse), optionally after a `--` separator.
    if verb == "run" {
        let mut session = None;
        let mut json = false;
        let mut cwd = None;
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
            cmd: PaneCmd::Run { cwd, argv },
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
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let tok = args[i]
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match tok {
            "--json" => json = true,
            "--session" => session = Some(flag_value(args, &mut i, "--session")?),
            "--lines" => {
                lines = Some(parse_u64(&flag_value(args, &mut i, "--lines")?, "--lines")? as u16)
            }
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
        },
        "kill" => PaneCmd::Kill {
            pane: pane_arg("kill")?,
        },
        other => {
            return Err(format!(
                "unknown pane verb: {other} (ls|read|run|send|wait|kill)"
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
        PaneCmd::Read { pane, lines } => (ControlVerb::PaneRead { pane, lines }, CONTROL_TIMEOUT),
        PaneCmd::Run { cwd, argv } => {
            let cwd = cwd
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            (
                ControlVerb::PaneRun {
                    cwd,
                    argv,
                    cols: None,
                    rows: None,
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
        } => (
            ControlVerb::PaneWait {
                pane,
                quiet_ms,
                pattern,
                timeout_ms,
            },
            Duration::from_millis(timeout_ms) + Duration::from_secs(2),
        ),
        PaneCmd::Kill { pane } => (ControlVerb::PaneKill { pane }, CONTROL_TIMEOUT),
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
        match std::os::unix::net::UnixStream::connect(sock) {
            Ok(s) => s,
            // Only a refused/absent socket proves "no server" (nothing to act
            // on): `ls` -> empty listing (exit 0), the rest -> error. Any
            // OTHER connect error (fd exhaustion, permissions) is a real
            // failure that must never read as a clean empty result - mirrors
            // the sibling `ls`/`kill_server` split, which keeps a bad session
            // from looking like zero panes.
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

    match send_control(stream, verb, read_timeout) {
        Ok(reply) => render_reply(reply, json),
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

/// Turn one server reply into stdout + an exit code.
fn render_reply(reply: ServerMsg, json: bool) -> i32 {
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
        ServerMsg::PaneText { pane_id, text } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "pane_id": pane_id, "text": text })
                );
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
            };
            if json {
                println!("{}", serde_json::json!({ "outcome": word }));
            } else {
                println!("{word}");
            }
            wait_exit_code(outcome)
        }
        ServerMsg::Err { code, msg } => {
            eprintln!("fno mux pane: {msg}");
            let _ = code; // one nonzero code for every control error class
            EXIT_ERROR
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

    #[test]
    fn mux_kill_server_missing_socket_is_no_server_exit_1() {
        // No env manipulation (unit tests share the process): a name no real
        // session uses resolves to a socket that does not exist -> exit 1.
        // The full live/stale matrix runs e2e against FNO_MUX_DIR-scoped
        // servers in 3.6.
        let code = kill_server(&format!("fno-test-absent-{}", std::process::id()));
        assert_eq!(code, 1, "missing socket must exit 1");
    }

    #[test]
    fn mux_kill_server_invalid_name_is_usage_exit_2() {
        assert_eq!(kill_server("../evil"), 2, "validation precedes any I/O");
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
                    lines: Some(40)
                }
            }
        );
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
    fn mux_pane_wait_exit_codes_are_distinct() {
        // AC4-EDGE: timeout is tellable apart from a match and a settle.
        let codes = [
            wait_exit_code(WaitOutcome::Quiet),
            wait_exit_code(WaitOutcome::Matched),
            wait_exit_code(WaitOutcome::Timeout),
            wait_exit_code(WaitOutcome::PaneExited),
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            4,
            "every wait outcome maps to a distinct code"
        );
        assert_eq!(wait_exit_code(WaitOutcome::Quiet), EXIT_OK);
        assert_ne!(EXIT_WAIT_TIMEOUT, EXIT_WAIT_MATCHED);
    }
}
