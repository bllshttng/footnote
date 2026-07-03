//! `fno` binary: role select.
//!
//! - bare `fno` on a TTY -> mux client (spawn a server if absent, attach)
//! - bare `fno` off a TTY -> a one-line notice, exit 0 (never a TUI into a pipe)
//! - `fno --session <name>` on a TTY -> mux client for a named session
//! - `fno --server <socket>` -> mux server (internal; what the client spawns)
//! - `fno mux server [--session <name>]` -> mux server (public, scriptable)
//! - `fno mux ls | attach <name> | kill-server [<name>]` -> session management
//! - anything else -> forward to the provisioned Python CLI (`bootstrap`)
//!
//! `--session` is intercepted ONLY as the exact leading pair
//! `["--session", <name>]` (Locked 7): every other leading `--session` shape
//! is MuxUsage, never a silent forward to Python - the Python namespace only
//! carries a deprecated per-subcommand alias, never a leading flag, so the
//! interception is collision-free.

use std::env;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use fno::{bootstrap, mux_cli, proto};

/// What this invocation is, decided purely from args + TTY-ness. Session
/// resolution (flag > env > default) happens in `main`, not here, so the
/// decision table stays pure.
#[derive(Debug, PartialEq, Eq)]
enum Role {
    /// Attach (spawning the server if absent). `Some(name)` when an explicit
    /// session was named (`--session <name>` / `mux attach <name>`).
    Client(Option<String>),
    /// `--server <socket>`: run the server on an explicit socket path.
    ServerSocket(OsString),
    /// `mux server [--session <name>]`: run the server for a named session.
    ServerSession(String),
    /// An attach invocation with no TTY: print the notice, exit 0.
    NotTty,
    /// `mux ls [--json]`: list sessions (no TTY needed). The bool is `--json`.
    MuxLs(bool),
    /// `mux kill-server [<name>] [--json]`: shut a session down (no TTY needed).
    MuxKill(Option<String>, bool),
    /// `mux doctor [--json]`: read-only environment diagnostics (US6). The bool
    /// is `--json`.
    MuxDoctor(bool),
    /// `mux pane <verb> ...`: the v4 script API. Carries the tokens after
    /// `mux pane` verbatim; `mux_cli::pane` parses the verb + flags. No TTY
    /// needed (control verbs are scriptable one-shots).
    MuxPane(Vec<OsString>),
    /// `mux block <verb> ...`: block porcelain (`block pipe`, x-fe8f). Same
    /// carry-verbatim shape as `MuxPane`; `mux_cli::block` parses.
    MuxBlock(Vec<OsString>),
    /// `mux shell-init <zsh|bash> [--json]`: print the OSC 133 shell-integration
    /// snippet (v6). `None` / an unsupported shell is an error in the verb.
    MuxShellInit(Option<String>, bool),
    /// `mux serve --web [--session <name>] [--bind <addr>] [--port <n>]`: the
    /// read-only web bridge (x-6a14). Attaches to a session as an observer and
    /// serves its frame stream to browsers over HTTP+WebSocket. No TTY needed.
    MuxWeb(fno::web::WebArgs),
    /// A malformed mux/server invocation: print usage, exit 2.
    MuxUsage,
    /// Any other args: the Python-CLI forwarding path.
    Forward,
}

/// Split a mux verb's trailing args into positionals plus a `--json` flag (US6:
/// every scriptable verb accepts `--json`). `--json` may appear once, anywhere;
/// a repeated `--json`, an unknown `--flag`, or a non-UTF-8 arg is `None` (the
/// caller maps that to `MuxUsage`, exit 2).
fn split_json(rest: &[OsString]) -> Option<(Vec<&str>, bool)> {
    let mut positionals = Vec::new();
    let mut json = false;
    let mut end_of_flags = false;
    for a in rest {
        let s = a.to_str()?; // non-UTF-8 -> usage
        if end_of_flags {
            positionals.push(s); // after `--` everything is positional (gemini)
        } else if s == "--" {
            end_of_flags = true; // the standard end-of-options delimiter
        } else if s == "--json" {
            if json {
                return None; // repeated flag
            }
            json = true;
        } else if s.starts_with("--") {
            return None; // unknown flag
        } else {
            positionals.push(s);
        }
    }
    Some((positionals, json))
}

/// Parse `serve` flags into [`fno::web::WebArgs`]. `--web` is required; a missing
/// flag value, an unknown flag, a non-UTF-8 arg, or a bad `--port` is `None`
/// (the caller maps that to `MuxUsage`, exit 2).
fn parse_web_args(rest: &[OsString]) -> Option<fno::web::WebArgs> {
    let mut web = false;
    let mut args = fno::web::WebArgs::default();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.to_str()? {
            "--web" => web = true,
            "--session" => args.session = it.next()?.to_str()?.to_string(),
            "--bind" => args.bind = it.next()?.to_str()?.to_string(),
            "--port" => args.port = it.next()?.to_str()?.parse().ok()?,
            _ => return None,
        }
    }
    web.then_some(args)
}

fn decide_role(args: &[OsString], is_tty: bool) -> Role {
    let first = args.first().map(|a| a.to_str());
    match first {
        None => {
            if is_tty {
                Role::Client(None)
            } else {
                Role::NotTty
            }
        }
        Some(Some("--session")) => match args.get(1).and_then(|a| a.to_str()) {
            // Exactly ["--session", <name>]: an attach. Anything else
            // (bare flag, trailing args) is usage - never forwarded (AC3-ERR).
            Some(name) if args.len() == 2 => {
                if is_tty {
                    Role::Client(Some(name.to_string()))
                } else {
                    Role::NotTty
                }
            }
            _ => Role::MuxUsage,
        },
        Some(Some("--server")) => match args.get(1) {
            Some(p) if args.len() == 2 => Role::ServerSocket(p.clone()),
            _ => Role::MuxUsage,
        },
        Some(Some("mux")) => match args.get(1).and_then(|a| a.to_str()) {
            Some("server") => {
                let mut session = proto::DEFAULT_SESSION.to_string();
                let mut rest = args[2..].iter();
                while let Some(a) = rest.next() {
                    match a.to_str() {
                        Some("--session") => match rest.next().and_then(|s| s.to_str()) {
                            Some(s) => session = s.to_string(),
                            None => return Role::MuxUsage,
                        },
                        _ => return Role::MuxUsage,
                    }
                }
                Role::ServerSession(session)
            }
            // `mux serve --web ...`: the read-only web bridge (x-6a14). `--web`
            // is required (the `serve` verb reserves room for future modes).
            Some("serve") => match parse_web_args(&args[2..]) {
                Some(w) => Role::MuxWeb(w),
                None => Role::MuxUsage,
            },
            // `mux pane <verb> ...`: hand the rest to the pane verb family;
            // a bare `mux pane` (no verb) falls through to MuxUsage. Nothing
            // under `mux pane` ever forwards to Python (AC).
            Some("pane") if args.len() > 2 => Role::MuxPane(args[2..].to_vec()),
            // `mux block <verb> ...`: block porcelain; a bare `mux block`
            // falls through to MuxUsage. Never forwards to Python.
            Some("block") if args.len() > 2 => Role::MuxBlock(args[2..].to_vec()),
            // ls / doctor take no positional, an optional `--json` (US6).
            Some("ls") => match split_json(&args[2..]) {
                Some((pos, json)) if pos.is_empty() => Role::MuxLs(json),
                _ => Role::MuxUsage,
            },
            Some("doctor") => match split_json(&args[2..]) {
                Some((pos, json)) if pos.is_empty() => Role::MuxDoctor(json),
                _ => Role::MuxUsage,
            },
            Some("attach") => match args.get(2).and_then(|a| a.to_str()) {
                Some(name) if args.len() == 3 => {
                    if is_tty {
                        Role::Client(Some(name.to_string()))
                    } else {
                        Role::NotTty
                    }
                }
                _ => Role::MuxUsage,
            },
            Some("kill-server") => match split_json(&args[2..]) {
                Some((pos, json)) => match pos.as_slice() {
                    [] => Role::MuxKill(None, json),
                    [name] => Role::MuxKill(Some((*name).to_string()), json),
                    _ => Role::MuxUsage,
                },
                None => Role::MuxUsage,
            },
            // `mux shell-init [<shell>] [--json]`: the verb validates the shell
            // (an unsupported / missing one is its own one-line error, AC4-ERR).
            Some("shell-init") => match split_json(&args[2..]) {
                Some((pos, json)) => match pos.as_slice() {
                    [] => Role::MuxShellInit(None, json),
                    [shell] => Role::MuxShellInit(Some((*shell).to_string()), json),
                    _ => Role::MuxUsage,
                },
                None => Role::MuxUsage,
            },
            _ => Role::MuxUsage,
        },
        // Every other invocation (including a non-UTF-8 first arg) is the
        // existing CLI surface: forward it untouched.
        Some(_) => Role::Forward,
    }
}

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let env_session = env::var("FNO_SESSION").ok();
    match decide_role(&args, is_tty) {
        Role::Forward => bootstrap::forward(&args),
        Role::NotTty => {
            // AC1-EDGE: piped/CI bare `fno` gets a notice, not a TUI. Exit 0 -
            // this is a gate, not a failure.
            println!(
                "fno: not a tty - the fno mux needs an interactive terminal. \
                 Run `fno <subcommand>` for the CLI."
            );
        }
        Role::MuxUsage => {
            eprintln!(
                "usage: fno [--session <name>] | fno mux server [--session <name>] \
                 | fno mux ls [--json] | fno mux attach <name> \
                 | fno mux kill-server [<name>] [--json] \
                 | fno mux shell-init <zsh|bash> [--json] | fno mux doctor [--json] \
                 | fno mux serve --web [--session <name>] [--bind <addr>] [--port <n>] \
                 | fno mux pane ls|read|run|send|wait|kill|claim|release ... \
                 | fno mux block pipe --from <pane> --to <pane> [--block last|<seq>] [--json] [--force]"
            );
            std::process::exit(2);
        }
        Role::MuxLs(json) => std::process::exit(mux_cli::ls(json)),
        Role::MuxKill(name, json) => {
            let session = mux_cli::resolve_session(name.as_deref(), env_session.as_deref());
            std::process::exit(mux_cli::kill_server(&session, json));
        }
        Role::MuxShellInit(shell, json) => {
            std::process::exit(mux_cli::shell_init(shell.as_deref(), json))
        }
        Role::MuxDoctor(json) => std::process::exit(mux_cli::doctor(json)),
        Role::MuxWeb(web_args) => std::process::exit(fno::web::serve(web_args)),
        Role::MuxPane(rest) => std::process::exit(mux_cli::pane(&rest, env_session.as_deref())),
        Role::MuxBlock(rest) => std::process::exit(mux_cli::block(&rest, env_session.as_deref())),
        Role::Client(flag) => {
            let env = env_session.as_deref().filter(|s| !s.is_empty());
            // Bare `fno` with nothing pinned: the pre-attach picker decides
            // (Locked 8). `--session`/`mux attach`/`FNO_SESSION` all bypass it
            // (AC5-FR) - they name a session outright.
            if flag.is_none() && env.is_none() {
                match mux_cli::pick_session() {
                    Some(session) => run_client(&session),
                    None => {} // picker quit: clean exit 0, no spawn
                }
            } else {
                let session = mux_cli::resolve_session(flag.as_deref(), env);
                run_client(&session);
            }
        }
        Role::ServerSocket(p) => run_server(PathBuf::from(p)),
        Role::ServerSession(session) => match proto::socket_path(&session) {
            Ok(path) => run_server(path),
            Err(e) => {
                eprintln!("fno: {e}");
                std::process::exit(2);
            }
        },
    }
}

fn run_client(session: &str) {
    std::process::exit(fno::client::run(session));
}

fn run_server(socket: PathBuf) {
    std::process::exit(fno::server::run(socket));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn proto_role_bare_tty_is_client() {
        assert_eq!(decide_role(&[], true), Role::Client(None));
    }

    #[test]
    fn proto_role_session_flag_exact_pair_is_client() {
        // AC3-HP: ["--session", <name>] on a TTY attaches that session.
        assert_eq!(
            decide_role(&os(&["--session", "work"]), true),
            Role::Client(Some("work".into()))
        );
        // Off a TTY it is the notice, mirroring bare `fno`.
        assert_eq!(
            decide_role(&os(&["--session", "work"]), false),
            Role::NotTty
        );
    }

    #[test]
    fn proto_role_malformed_session_flag_is_usage_never_forward() {
        // AC3-ERR: a bare flag or trailing args must never silently reach
        // Python and never open a TUI.
        assert_eq!(decide_role(&os(&["--session"]), true), Role::MuxUsage);
        assert_eq!(
            decide_role(&os(&["--session", "work", "backlog"]), true),
            Role::MuxUsage
        );
        assert_eq!(
            decide_role(&os(&["--session", "work", "backlog", "list"]), false),
            Role::MuxUsage
        );
    }

    #[test]
    fn proto_role_mux_ls_and_kill_server_need_no_tty() {
        assert_eq!(decide_role(&os(&["mux", "ls"]), false), Role::MuxLs(false));
        assert_eq!(
            decide_role(&os(&["mux", "kill-server"]), false),
            Role::MuxKill(None, false)
        );
        assert_eq!(
            decide_role(&os(&["mux", "kill-server", "work"]), false),
            Role::MuxKill(Some("work".into()), false)
        );
        assert_eq!(
            decide_role(&os(&["mux", "kill-server", "a", "b"]), false),
            Role::MuxUsage
        );
        assert_eq!(decide_role(&os(&["mux", "ls", "x"]), false), Role::MuxUsage);
    }

    #[test]
    fn proto_role_mux_json_flag_on_scriptable_verbs() {
        // US6: every scriptable verb accepts `--json`, anywhere in its args.
        assert_eq!(
            decide_role(&os(&["mux", "ls", "--json"]), false),
            Role::MuxLs(true)
        );
        assert_eq!(
            decide_role(&os(&["mux", "kill-server", "--json", "work"]), false),
            Role::MuxKill(Some("work".into()), true)
        );
        assert_eq!(
            decide_role(&os(&["mux", "kill-server", "work", "--json"]), false),
            Role::MuxKill(Some("work".into()), true)
        );
        assert_eq!(
            decide_role(&os(&["mux", "doctor"]), false),
            Role::MuxDoctor(false)
        );
        assert_eq!(
            decide_role(&os(&["mux", "doctor", "--json"]), false),
            Role::MuxDoctor(true)
        );
        // A repeated flag or an unknown flag is usage, not a silent accept.
        assert_eq!(
            decide_role(&os(&["mux", "ls", "--json", "--json"]), false),
            Role::MuxUsage
        );
        assert_eq!(
            decide_role(&os(&["mux", "doctor", "--wat"]), false),
            Role::MuxUsage
        );
        // `--` ends flag parsing: a dashed session name passes as a positional.
        assert_eq!(
            decide_role(&os(&["mux", "kill-server", "--", "--weird"]), false),
            Role::MuxKill(Some("--weird".into()), false)
        );
        assert_eq!(
            decide_role(
                &os(&["mux", "kill-server", "--json", "--", "--weird"]),
                false
            ),
            Role::MuxKill(Some("--weird".into()), true)
        );
    }

    #[test]
    fn proto_role_mux_shell_init_carries_optional_shell() {
        assert_eq!(
            decide_role(&os(&["mux", "shell-init", "zsh"]), false),
            Role::MuxShellInit(Some("zsh".into()), false)
        );
        assert_eq!(
            decide_role(&os(&["mux", "shell-init", "zsh", "--json"]), false),
            Role::MuxShellInit(Some("zsh".into()), true)
        );
        assert_eq!(
            decide_role(&os(&["mux", "shell-init"]), false),
            Role::MuxShellInit(None, false)
        );
        assert_eq!(
            decide_role(&os(&["mux", "shell-init", "a", "b"]), false),
            Role::MuxUsage
        );
    }

    #[test]
    fn proto_role_mux_attach_is_client_on_tty_notice_off() {
        assert_eq!(
            decide_role(&os(&["mux", "attach", "work"]), true),
            Role::Client(Some("work".into()))
        );
        assert_eq!(
            decide_role(&os(&["mux", "attach", "work"]), false),
            Role::NotTty
        );
        assert_eq!(decide_role(&os(&["mux", "attach"]), true), Role::MuxUsage);
    }

    #[test]
    fn proto_role_bare_non_tty_is_notice() {
        assert_eq!(decide_role(&[], false), Role::NotTty);
    }

    #[test]
    fn proto_role_subcommands_forward_to_python_cli() {
        assert_eq!(decide_role(&os(&["backlog", "list"]), true), Role::Forward);
        assert_eq!(decide_role(&os(&["--help"]), false), Role::Forward);
    }

    #[test]
    fn proto_role_server_flag_takes_socket_path() {
        assert_eq!(
            decide_role(&os(&["--server", "/tmp/s.sock"]), false),
            Role::ServerSocket(OsString::from("/tmp/s.sock"))
        );
        assert_eq!(decide_role(&os(&["--server"]), false), Role::MuxUsage);
    }

    #[test]
    fn proto_role_mux_pane_routes_to_the_verb_family() {
        // A verb after `mux pane` routes to MuxPane carrying the rest; a bare
        // `mux pane` is usage; nothing under `mux pane` forwards to Python.
        assert_eq!(
            decide_role(&os(&["mux", "pane", "ls"]), false),
            Role::MuxPane(os(&["ls"]))
        );
        assert_eq!(
            decide_role(
                &os(&["mux", "pane", "run", "--cwd", "/x", "--", "claude"]),
                true
            ),
            Role::MuxPane(os(&["run", "--cwd", "/x", "--", "claude"]))
        );
        assert_eq!(decide_role(&os(&["mux", "pane"]), false), Role::MuxUsage);
    }

    #[test]
    fn proto_role_mux_block_routes_to_the_verb_family() {
        // Same carry-verbatim shape as `mux pane`; a bare `mux block` is usage.
        assert_eq!(
            decide_role(
                &os(&["mux", "block", "pipe", "--from", "4", "--to", "2"]),
                false
            ),
            Role::MuxBlock(os(&["pipe", "--from", "4", "--to", "2"]))
        );
        assert_eq!(decide_role(&os(&["mux", "block"]), false), Role::MuxUsage);
    }

    #[test]
    fn proto_role_mux_server_parses_session() {
        assert_eq!(
            decide_role(&os(&["mux", "server"]), false),
            Role::ServerSession("main".into())
        );
        assert_eq!(
            decide_role(&os(&["mux", "server", "--session", "work"]), false),
            Role::ServerSession("work".into())
        );
        assert_eq!(
            decide_role(&os(&["mux", "server", "--session"]), false),
            Role::MuxUsage
        );
        assert_eq!(decide_role(&os(&["mux", "bogus"]), false), Role::MuxUsage);
    }
}
