//! `fno` binary: role select.
//!
//! - bare `fno` on a TTY -> mux client (spawn a server if absent, attach)
//! - bare `fno` off a TTY -> a one-line notice, exit 0 (never a TUI into a pipe)
//! - `fno --server <socket>` -> mux server (internal; what the client spawns)
//! - `fno mux server [--session <name>]` -> mux server (public, scriptable)
//! - anything else -> forward to the provisioned Python CLI (`bootstrap`)

use std::env;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use fno::{bootstrap, proto};

/// What this invocation is, decided purely from args + TTY-ness.
#[derive(Debug, PartialEq, Eq)]
enum Role {
    /// Bare `fno` on a TTY: attach (spawning the server if absent).
    Client,
    /// `--server <socket>`: run the server on an explicit socket path.
    ServerSocket(OsString),
    /// `mux server [--session <name>]`: run the server for a named session.
    ServerSession(String),
    /// Bare `fno` with no TTY: print the notice, exit 0.
    NotTty,
    /// A malformed mux/server invocation: print usage, exit 2.
    MuxUsage,
    /// Any other args: the Python-CLI forwarding path.
    Forward,
}

fn decide_role(args: &[OsString], is_tty: bool) -> Role {
    let first = args.first().map(|a| a.to_str());
    match first {
        None => {
            if is_tty {
                Role::Client
            } else {
                Role::NotTty
            }
        }
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
            eprintln!("usage: fno mux server [--session <name>]");
            std::process::exit(2);
        }
        Role::Client => run_client(proto::DEFAULT_SESSION),
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

fn run_client(_session: &str) {
    // Task 1.3 wires the client TUI.
    eprintln!("fno mux: client not implemented yet");
    std::process::exit(1);
}

fn run_server(_socket: PathBuf) {
    // Task 1.2 wires the server spine.
    eprintln!("fno mux: server not implemented yet");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn proto_role_bare_tty_is_client() {
        assert_eq!(decide_role(&[], true), Role::Client);
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
