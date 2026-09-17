//! `fno` binary: role select.
//!
//! - bare `fno` on a TTY -> mux client (spawn a server if absent, attach)
//! - bare `fno` off a TTY -> a one-line notice, exit 0 (never a TUI into a pipe)
//! - `fno --server <name>` on a TTY -> mux client for a named server
//! - `fno --session <name>` -> deprecated spelling of `--server` (still works, warns)
//! - `fno --server <socket>` -> mux server (internal; what the client spawns)
//! - `fno mux server [--server <name>]` -> mux server (public, scriptable)
//! - `fno mux ls | attach <name> | kill-server [<name>]` -> server management
//! - anything else -> forward to the provisioned Python CLI (`bootstrap`)
//!
//! The leading `--server`/`--session` pair is intercepted ONLY as the exact
//! pair `[flag, <name>]` (Locked 7): every other shape is MuxUsage, never a
//! silent forward to Python - the Python namespace only carries a deprecated
//! per-subcommand alias, never a leading flag, so the interception is
//! collision-free. A `--server` value containing `/` keeps the internal
//! ServerSocket role (what `client.rs` spawns with an absolute path); any
//! other value is the attach.

use std::env;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use fno::{bootstrap, cli_args, mux_cli, proto};

/// Verbs removed from the mux front, and what replaced each one.
///
/// A removed verb that lands on the bare usage banner makes the caller re-read
/// the docs to discover a rename they could have been told about in one line,
/// and a hook that hits it fails with no idea why. A tombstone is cheaper than
/// a broken caller, so removal means moving the name here, not deleting it.
const MUX_TOMBSTONES: &[(&str, &str)] = &[(
    "squad",
    "`fno mux workspace <verb>` - `squad` was an unadvertised alias and is gone",
)];

fn mux_tombstone(verb: &str) -> Option<&'static str> {
    MUX_TOMBSTONES
        .iter()
        .find(|(name, _)| *name == verb)
        .map(|(_, replacement)| *replacement)
}

/// What this invocation is, decided purely from args + TTY-ness. Session
/// resolution (flag > env > default) happens in `main`, not here; the one
/// side effect is the `--session` deprecation note, which names a flag and
/// stays silent on every `--server` shape.
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
    /// (v78) `mux stats [--json]`: server-instance telemetry (the human_touch
    /// emission-failure counter with its measurement window). Hidden, read-only.
    MuxStats(bool),
    /// `mux pane <verb> ...`: the v4 script API. The operation is the typed
    /// tree's verdict; the argv is the re-sliced family tail. No TTY needed
    /// (control verbs are scriptable one-shots).
    MuxPane(fno::cli_args::PaneOp),
    /// `mux block <verb> ...`: block porcelain (`block pipe`).
    MuxBlock(fno::cli_args::BlockOp),
    /// `mux tab <verb> ...`: the layout-tab script verbs.
    MuxTab(fno::cli_args::TabOp),
    /// `mux layout <get|apply|graft> ...`: nested layout trees and specs.
    MuxLayout(fno::cli_args::LayoutOp),
    ///  `mux rows [--json]`: the one row-set receipt - the last
    /// derived `layout.agents` with the paint verdict per row.
    MuxRows(Vec<OsString>),
    /// `mux where <fno_id>`: resolve an fno session id to its
    /// location; a selector naming no agent is retried as a tab location -
    /// ordinal, stable id, or name.
    MuxWhere(Vec<OsString>),
    /// (hidden) `mux thread <name> [--portal N]`: show a thread row
    /// through a portal. `--portal` names the index (default 0), so two calls
    /// naming 0 and 1 put two threads in two panes for the tab menu's Join
    /// actions to tile.
    MuxThread(Vec<OsString>),
    /// (v72) `mux thread reseat <agent-name | pane-id> [--portal N]`: move a
    /// live pane-hosted worker into a portal seat, keeping its PTY, and clear
    /// the row's registry `mux` ref on the receipt. One process owns the
    /// whole move (operator ruling, 2026-09-06); the former Python front
    /// door is deleted.
    MuxThreadReseat(Vec<OsString>),
    /// (v75) `mux retire-session <session> --harness <name> --session-id <id>`:
    /// the thin transport for the exact-session retirement. The server closes
    /// only the identity's attached panes and tombstones through the store.
    MuxRetireSession(Vec<OsString>),
    /// `mux view <selector> [--url] [--fzf] [--json]`: point the
    /// operator's view at the pane hosting an agent, selected by node id,
    /// slug, or name; a selector naming no agent focuses the tab at that
    /// location instead. Same carry-verbatim shape; `mux_cli::view`
    /// parses.
    MuxView(Vec<OsString>),
    /// `mux workspace prune|restore ...`: workspace-store maintenance.
    MuxWorkspace(fno::cli_args::WorkspaceOp),
    /// `mux shell-init <zsh|bash> [--json]`: print the OSC 133 shell-integration
    /// snippet (v6). `None` / an unsupported shell is an error in the verb.
    MuxShellInit(Option<String>, bool),
    /// `mux serve --web [--session <name>] [--bind <addr>] [--port <n>]`: the
    /// read-only web bridge. Attaches to a session as an observer and
    /// serves its frame stream to browsers over HTTP+WebSocket. No TTY needed.
    /// `mux serve --stop [--session <name>]` kills the running bridge: it reads
    /// the bridge's own state file, identity-checks the pid against its
    /// recorded start token, then SIGINTs (the bridge's graceful exit) with a
    /// SIGKILL escalation for a wedged one.
    MuxWeb(fno::web::WebArgs),
    /// `mux web reap [--json]`: the corpse sweep for the `--web` bridge marker.
    MuxWebCtl(fno::cli_args::WebOp),
    /// A verb named in [`MUX_TOMBSTONES`]: refuse, naming what replaced it.
    MuxRemoved(String),
    /// `version [--json]`: report the mux binary's own baked-in build rev so
    /// `fno doctor update` can detect a present-but-stale front door. The bool is
    /// `--json`. Additive: `fno version` had no Python command (it errored), so
    /// intercepting it here breaks nothing; `fno --version` still forwards.
    MuxVersion(bool),
    /// A malformed mux/server invocation: `message` prints on stderr, exit 2
    /// (clap's rendered help for an explicit help request, else one
    /// command-qualified refusal line naming the bad token).
    MuxUsage(String),
    /// Any other args: the Python-CLI forwarding path.
    Forward,
}

/// Exit an `fno mux` verb, surfacing any config warning recorded while the
/// verb resolved its socket dir. These roles are non-TUI, so stderr is safe
/// here; the interactive client routes the same warning to its client log
/// instead, and `mux doctor` carries it as a row.
fn exit_mux(code: i32) -> ! {
    if let Some((w, _remedy)) = fno::proto::pending_config_warning() {
        eprintln!("{w}");
    }
    std::process::exit(code)
}

/// Parse `serve` flags into [`fno::web::WebArgs`]. One of `--web`, `--stop`,
/// `--status` is required; a missing flag value, an unknown flag, a non-UTF-8
/// arg, or a bad `--port` is `None` (the caller maps that to `MuxUsage`, exit
/// 2).
fn parse_web_args(rest: &[OsString]) -> Option<fno::web::WebArgs> {
    let mut web = false;
    let mut args = fno::web::WebArgs::default();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.to_str()? {
            "--web" => web = true,
            "--stop" => args.stop = true,
            "--status" => args.status = true,
            tok @ ("--server" | "--session") => {
                mux_cli::note_server_flag(tok);
                args.session = it.next()?.to_str()?.to_string()
            }
            "--bind" => args.bind = it.next()?.to_str()?.to_string(),
            "--port" => args.port = it.next()?.to_str()?.parse().ok()?,
            _ => return None,
        }
    }
    (web || args.stop || args.status).then_some(args)
}

fn decide_role(args: &[OsString], is_tty: bool) -> Role {
    use cli_args::FrontDoor;
    match cli_args::classify(args) {
        FrontDoor::Forward => Role::Forward,
        FrontDoor::Usage { message } => {
            // A removed verb is refused BY NAME, before the catch-all
            // refusal turns it into an anonymous message (same order the
            // old carry router applied the tombstone table in).
            if args.first().and_then(|a| a.to_str()) == Some("mux") {
                if let Some(v) = args.get(1).and_then(|a| a.to_str()) {
                    if mux_tombstone(v).is_some() {
                        return Role::MuxRemoved(v.to_string());
                    }
                }
            }
            Role::MuxUsage(message)
        }
        FrontDoor::Version { json } => Role::MuxVersion(json),
        FrontDoor::Attach {
            name,
            explicit_socket,
        } => {
            // A `--server` value containing `/` keeps the internal
            // ServerSocket role (client.rs spawns it with an absolute path);
            // any other value is the attach that `fno --session <name>`
            // performs today.
            if explicit_socket {
                Role::ServerSocket(OsString::from(name.unwrap_or_default()))
            } else if is_tty {
                Role::Client(name)
            } else {
                Role::NotTty
            }
        }
        FrontDoor::Mux(m) => match m.cmd {
            cli_args::MuxCmd::Server(s) => {
                // Server/session resolution: explicit > env > default; the
                // deprecated spelling already warned in `classify`.
                Role::ServerSession(
                    s.server
                        .or(s.session)
                        .unwrap_or_else(|| proto::DEFAULT_SESSION.to_string()),
                )
            }
            cli_args::MuxCmd::Ls { json } => Role::MuxLs(json.json),
            cli_args::MuxCmd::Doctor { json } => Role::MuxDoctor(json.json),
            cli_args::MuxCmd::Stats { json } => Role::MuxStats(json.json),
            cli_args::MuxCmd::KillServer { name, json } => Role::MuxKill(name, json.json),
            cli_args::MuxCmd::ShellInit { shell, json } => Role::MuxShellInit(shell, json.json),
            cli_args::MuxCmd::Attach { name } => {
                if is_tty {
                    Role::Client(Some(name))
                } else {
                    Role::NotTty
                }
            }
            cli_args::MuxCmd::Pane { op } => Role::MuxPane(op),
            cli_args::MuxCmd::Block { op } => Role::MuxBlock(op),
            cli_args::MuxCmd::Tab { op } => Role::MuxTab(op),
            // layout keeps the operation word in its tail: a common flag may
            // sit before it, and layout()'s own MuxCommon::take strips it.
            cli_args::MuxCmd::Layout { common: _, op } => Role::MuxLayout(op),
            cli_args::MuxCmd::Web { op } => Role::MuxWebCtl(op),
            cli_args::MuxCmd::Workspace { op } => Role::MuxWorkspace(op),
            cli_args::MuxCmd::Serve(t) => match parse_web_args(&t.tail) {
                Some(w) => Role::MuxWeb(w),
                None => Role::MuxUsage("fno mux serve: needs --web, --stop, or --status".into()),
            },
            cli_args::MuxCmd::Rows(t) => Role::MuxRows(t.tail),
            cli_args::MuxCmd::Where(t) => Role::MuxWhere(t.tail),
            cli_args::MuxCmd::RetireSession(t) => Role::MuxRetireSession(t.tail),
            // An explicit -h/--help prints the view help (the verb family's
            // one self-teaching surface) rather than parsing as a selector.
            cli_args::MuxCmd::View(t)
                if t.tail
                    .first()
                    .and_then(|a| a.to_str())
                    .map(|v| v == "-h" || v == "--help")
                    .unwrap_or(false) =>
            {
                Role::MuxUsage(cli_args::render_path_help(&["mux", "view"]))
            }
            cli_args::MuxCmd::View(t) => Role::MuxView(t.tail),
            // Thread help routes in classify's error branch: clap refuses the
            // hyphen spelling before the external arm can carry it, so
            // ThreadOp::Name never sees `-h`/`--help`.
            cli_args::MuxCmd::Thread { op } => match op {
                cli_args::ThreadOp::Reseat(t) => Role::MuxThreadReseat(t.tail),
                cli_args::ThreadOp::Name(words) => Role::MuxThread(words),
            },
        },
    }
}

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let env_session = mux_cli::env_server();
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
        Role::MuxUsage(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        Role::MuxRemoved(verb) => {
            eprintln!(
                "fno mux {verb}: removed. Use {}.",
                mux_tombstone(&verb).unwrap_or("`fno mux` for the current surface")
            );
            std::process::exit(2);
        }
        Role::MuxVersion(json) => fno::version::print_version(json),
        Role::MuxLs(json) => exit_mux(mux_cli::ls(json)),
        Role::MuxKill(name, json) => {
            let session = mux_cli::resolve_session(name.as_deref(), env_session.as_deref());
            exit_mux(mux_cli::kill_server(&session, json));
        }
        Role::MuxShellInit(shell, json) => {
            std::process::exit(mux_cli::shell_init(shell.as_deref(), json))
        }
        Role::MuxDoctor(json) => std::process::exit(mux_cli::doctor(json)),
        Role::MuxStats(json) => std::process::exit(mux_cli::stats(json)),
        Role::MuxWeb(web_args) => {
            // The bridge serves for hours, so the warning its startup
            // resolution recorded must surface NOW: exit_mux would print it
            // only after the socket closes, and a signal kill never exits
            // through it at all. The exit-time repeat is the cheap cost of
            // the early word (run_server takes the same trade).
            if let Some((warning, _)) = proto::pending_config_warning() {
                eprintln!("{warning}");
            }
            exit_mux(fno::web::serve(web_args))
        }
        Role::MuxWebCtl(op) => {
            let tail = op.tail();
            exit_mux(mux_cli::web_ctl::web(op, &tail, env_session.as_deref()))
        }
        Role::MuxPane(op) => exit_mux(mux_cli::pane(op, env_session.as_deref())),
        Role::MuxBlock(op) => {
            let tail = op.tail();
            exit_mux(mux_cli::block(op, &tail, env_session.as_deref()))
        }
        Role::MuxTab(op) => {
            let tail = op.tail();
            exit_mux(mux_cli::tab(op, &tail, env_session.as_deref()))
        }
        Role::MuxLayout(op) => {
            let tail = op.tail();
            exit_mux(mux_cli::layout(op, &tail, env_session.as_deref()))
        }
        Role::MuxRows(args) => exit_mux(mux_cli::mux_rows::rows(&args, env_session.as_deref())),
        Role::MuxWhere(rest) => exit_mux(mux_cli::where_(&rest, env_session.as_deref())),
        Role::MuxThread(rest) => exit_mux(mux_cli::thread(&rest, env_session.as_deref())),
        Role::MuxThreadReseat(rest) => exit_mux(mux_cli::reseat(&rest, env_session.as_deref())),
        Role::MuxRetireSession(rest) => {
            exit_mux(mux_cli::retire_session(&rest, env_session.as_deref()))
        }
        Role::MuxView(rest) => exit_mux(mux_cli::view(&rest, env_session.as_deref())),
        Role::MuxWorkspace(op) => {
            let tail = op.tail();
            exit_mux(mux_cli::workspace(op, &tail, env_session.as_deref()))
        }
        Role::Client(flag) => {
            let env = env_session.as_deref().filter(|s| !s.is_empty());
            // Bare `fno` with nothing pinned: the pre-attach picker decides
            // (Locked 8). `--session`/`mux attach`/`FNO_SESSION` all bypass it
            // (AC5-FR) - they name a session outright.
            if flag.is_none() && env.is_none() {
                // A `None` means the picker quit: clean exit 0, no spawn.
                if let Some(session) = mux_cli::pick_session() {
                    run_client(&session);
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
    // The one mux role that never returns through `exit_mux`: the daemon
    // blocks until killed, so the config warning it recorded while resolving
    // the socket dir would otherwise never surface. Server stderr is a log
    // stream, not a PTY the harness scrapes, so printing here is safe (the
    // NEVER-stderr rule governs the TUI client).
    if let Some((warning, _)) = proto::pending_config_warning() {
        eprintln!("{warning}");
    }
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
        assert!(matches!(
            decide_role(&os(&["--session"]), true),
            Role::MuxUsage(_)
        ));
        assert!(matches!(
            decide_role(&os(&["--session", "work", "backlog"]), true),
            Role::MuxUsage(_)
        ));
        assert!(matches!(
            decide_role(&os(&["--session", "work", "backlog", "list"]), false),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_socket_pair_is_also_exact() {
        // The socket spelling obeys the same Locked-7 rule: trailing argv
        // after `--server <path>` is usage, never a ServerSocket role that
        // drops the command.
        assert!(matches!(
            decide_role(&os(&["--server", "/tmp/x.sock", "version"]), true),
            Role::MuxUsage(_)
        ));
        assert!(matches!(
            decide_role(&os(&["--server", "/tmp/x.sock", "backlog", "list"]), true),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_mux_server_alias_combo_is_usage() {
        // The old in-order loop resolved `--server a --session b` to the
        // last spelling; the typed layer holds no order, so the ambiguous
        // combination refuses loudly instead of silently picking one.
        assert!(matches!(
            decide_role(
                &os(&["mux", "server", "--server", "a", "--session", "b"]),
                false
            ),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn server_axis_top_level_server_flag_attaches_or_spawns_internal() {
        // AC4-HP/EDGE: `--server <name>` is the attach that
        // `--session <name>` performs today; a value containing `/` keeps the
        // internal ServerSocket role the client spawns.
        assert_eq!(
            decide_role(&os(&["--server", "work"]), true),
            Role::Client(Some("work".into()))
        );
        assert_eq!(decide_role(&os(&["--server", "work"]), false), Role::NotTty);
        assert_eq!(
            decide_role(&os(&["--server", "/tmp/x.sock"]), true),
            Role::ServerSocket("/tmp/x.sock".into())
        );
        assert!(
            matches!(decide_role(&os(&["--server"]), true), Role::MuxUsage(_)),
            "a bare flag is usage, never a forward"
        );
    }

    #[test]
    fn server_axis_mux_server_takes_server_flag() {
        // `mux server --server <name>`; --session keeps working.
        assert_eq!(
            decide_role(&os(&["mux", "server", "--server", "work"]), false),
            Role::ServerSession("work".into())
        );
        assert_eq!(
            decide_role(&os(&["mux", "server", "--session", "work"]), false),
            Role::ServerSession("work".into())
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
        assert!(matches!(
            decide_role(&os(&["mux", "kill-server", "a", "b"]), false),
            Role::MuxUsage(_)
        ));
        assert!(matches!(
            decide_role(&os(&["mux", "ls", "x"]), false),
            Role::MuxUsage(_)
        ));
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
        assert!(matches!(
            decide_role(&os(&["mux", "ls", "--json", "--json"]), false),
            Role::MuxUsage(_)
        ));
        assert!(matches!(
            decide_role(&os(&["mux", "doctor", "--wat"]), false),
            Role::MuxUsage(_)
        ));
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
        assert!(matches!(
            decide_role(&os(&["mux", "shell-init", "a", "b"]), false),
            Role::MuxUsage(_)
        ));
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
        assert!(matches!(
            decide_role(&os(&["mux", "attach"]), true),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_bare_non_tty_is_notice() {
        assert_eq!(decide_role(&[], false), Role::NotTty);
    }

    #[test]
    fn proto_role_subcommands_forward_to_python_cli() {
        assert_eq!(decide_role(&os(&["backlog", "list"]), true), Role::Forward);
        assert_eq!(decide_role(&os(&["--help"]), false), Role::Forward);
        // `fno --version` is a Python-forwarded callback, NOT the mux self-report.
        assert_eq!(decide_role(&os(&["--version"]), false), Role::Forward);
    }

    #[test]
    fn proto_role_version_is_mux_self_report() {
        // `version [--json]` reports the mux binary's own rev (no TTY needed);
        // a trailing positional is usage, never a silent forward.
        assert_eq!(
            decide_role(&os(&["version"]), false),
            Role::MuxVersion(false)
        );
        assert_eq!(
            decide_role(&os(&["version", "--json"]), true),
            Role::MuxVersion(true)
        );
        assert!(matches!(
            decide_role(&os(&["version", "x"]), false),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_server_flag_takes_socket_path() {
        assert_eq!(
            decide_role(&os(&["--server", "/tmp/s.sock"]), false),
            Role::ServerSocket(OsString::from("/tmp/s.sock"))
        );
        assert!(matches!(
            decide_role(&os(&["--server"]), false),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_mux_pane_routes_to_the_verb_family() {
        // A verb after `mux pane` routes to MuxPane carrying the rest; a bare
        // `mux pane` is usage; nothing under `mux pane` forwards to Python.
        assert_eq!(
            decide_role(&os(&["mux", "pane", "ls"]), false),
            Role::MuxPane(PaneOp::Ls(tail(vec![])))
        );
        assert_eq!(
            decide_role(
                &os(&["mux", "pane", "run", "--cwd", "/x", "--", "claude"]),
                true
            ),
            Role::MuxPane(PaneOp::Run(tail(os(&["--cwd", "/x", "--", "claude"]))))
        );
        assert!(matches!(
            decide_role(&os(&["mux", "pane"]), false),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_mux_block_routes_to_the_verb_family() {
        // Same carry-verbatim shape as `mux pane`; a bare `mux block` is usage.
        assert_eq!(
            decide_role(
                &os(&["mux", "block", "pipe", "--from", "4", "--to", "2"]),
                false
            ),
            Role::MuxBlock(BlockOp::Pipe(tail(os(&["--from", "4", "--to", "2"]))))
        );
        assert!(matches!(
            decide_role(&os(&["mux", "block"]), false),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_mux_view_carries_rest_verbatim() {
        // `mux view <selector>` and `mux view --fzf` route to the
        // shared-resolver focus door; a bare `mux view` is usage.
        assert_eq!(
            decide_role(&os(&["mux", "view", "x919"]), false),
            Role::MuxView(os(&["x919"]))
        );
        assert_eq!(
            decide_role(&os(&["mux", "view", "--fzf"]), false),
            Role::MuxView(os(&["--fzf"]))
        );
        assert_eq!(
            decide_role(&os(&["mux", "view", "x919", "--url", "--json"]), false),
            Role::MuxView(os(&["x919", "--url", "--json"]))
        );
        assert!(matches!(
            decide_role(&os(&["mux", "view"]), false),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn proto_role_mux_thread_help_routes_to_one_body_and_never_swallows_a_key() {
        // AC2-HP: both spellings render the SAME self-teaching body, which
        // carries the addressing contract the docs quote. AC2-EDGE lives in
        // the characterization test beside it: a bare name, a full Codex
        // session UUID and reseat keep their roles rather than routing here.
        let long = match decide_role(&os(&["mux", "thread", "--help"]), false) {
            Role::MuxUsage(body) => body,
            other => panic!("--help is usage, got {other:?}"),
        };
        assert_eq!(
            long,
            match decide_role(&os(&["mux", "thread", "-h"]), false) {
                Role::MuxUsage(body) => body,
                other => panic!("-h is usage, got {other:?}"),
            },
            "both spellings render one body"
        );
        for needle in [
            "fno agents whoami",
            "3f9d3c55-1c2b-4e8a-9a3f-7b2c5d6e8f90",
            "Claude",
            "exact",
            "refuse",
            "reseat",
        ] {
            assert!(long.contains(needle), "missing {needle}: {long}");
        }
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
        assert!(matches!(
            decide_role(&os(&["mux", "server", "--session"]), false),
            Role::MuxUsage(_)
        ));
        assert!(matches!(
            decide_role(&os(&["mux", "bogus"]), false),
            Role::MuxUsage(_)
        ));
    }

    // --- Characterization: the carry families' accepted argv, pinned
    // before the command-tree cutover (AC1-HP). Every assert here passed on
    // the pre-cutover main; the same roles and byte-exact tails must survive
    // the typed-tree dispatch. ---

    fn os_raw(bytes: &[u8]) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }

    use fno::cli_args::{BlockOp, KeeperOp, LayoutOp, MuxTail, PaneOp, TabOp, WebOp, WorkspaceOp};

    fn tail(v: Vec<OsString>) -> MuxTail {
        MuxTail { tail: v }
    }

    #[test]
    fn char_carry_families_keep_their_byte_exact_tails() {
        // pane run with an embedded command; a --help inside the payload is
        // the payload's, never ours.
        assert_eq!(
            decide_role(
                &os(&["mux", "pane", "run", "--cwd", "/x", "--", "claude", "--help"]),
                true
            ),
            Role::MuxPane(PaneOp::Run(tail(os(&[
                "--cwd", "/x", "--", "claude", "--help"
            ]))))
        );
        // A non-UTF-8 byte in the tail survives byte-exact.
        let mut raw = os(&["mux", "pane", "ls"]);
        raw.push(os_raw(&[0xff]));
        let mut expect = Vec::new();
        expect.push(os_raw(&[0xff]));
        assert_eq!(
            decide_role(&raw, false),
            Role::MuxPane(PaneOp::Ls(tail(expect)))
        );
        // The hidden keeper subtree rides the pane family.
        assert_eq!(
            decide_role(
                &os(&["mux", "pane", "keeper", "list", "--stale-after", "5s"]),
                false
            ),
            Role::MuxPane(PaneOp::Keeper {
                op: KeeperOp::List(tail(os(&["--stale-after", "5s"])))
            })
        );
        // layout: MuxCommon::take strips common flags from any position.
        assert_eq!(
            decide_role(
                &os(&["mux", "layout", "--json", "apply", "spec.toml"]),
                false
            ),
            Role::MuxLayout(LayoutOp::Apply(tail(os(&["--json", "apply", "spec.toml"]))))
        );
        assert_eq!(
            decide_role(
                &os(&["mux", "layout", "apply", "spec.toml", "--json"]),
                false
            ),
            Role::MuxLayout(LayoutOp::Apply(tail(os(&["apply", "spec.toml", "--json"]))))
        );
        // thread: reseat takes the rest after the verb; a bare name is a row.
        assert_eq!(
            decide_role(&os(&["mux", "thread", "reseat", "7"]), false),
            Role::MuxThreadReseat(os(&["7"]))
        );
        assert_eq!(
            decide_role(&os(&["mux", "thread", "wk"]), false),
            Role::MuxThread(os(&["wk"]))
        );
        // Help never swallows a row key: a full Codex session UUID and a
        // bare name keep their roles (AC2-EDGE).
        assert_eq!(
            decide_role(
                &os(&["mux", "thread", "3f9d3c55-1c2b-4e8a-9a3f-7b2c5d6e8f90"]),
                false
            ),
            Role::MuxThread(os(&["3f9d3c55-1c2b-4e8a-9a3f-7b2c5d6e8f90"]))
        );
        // view: -h is usage; a selector rides verbatim.
        assert!(matches!(
            decide_role(&os(&["mux", "view", "-h"]), false),
            Role::MuxUsage(_)
        ));
        assert_eq!(
            decide_role(&os(&["mux", "view", "x919", "--url", "--json"]), false),
            Role::MuxView(os(&["x919", "--url", "--json"]))
        );
        // serve --web parses into the bridge args; the tombstone refuses by name.
        assert!(matches!(
            decide_role(&os(&["mux", "serve", "--web"]), false),
            Role::MuxWeb(_)
        ));
        assert_eq!(
            decide_role(&os(&["mux", "squad"]), false),
            Role::MuxRemoved("squad".into())
        );
        // The remaining leaves and families carry their argv verbatim.
        assert_eq!(
            decide_role(&os(&["mux", "web", "reap", "--json"]), false),
            Role::MuxWebCtl(WebOp::Reap(tail(os(&["--json"]))))
        );
        assert_eq!(
            decide_role(
                &os(&[
                    "mux",
                    "retire-session",
                    "a",
                    "--harness",
                    "h",
                    "--session-id",
                    "i"
                ]),
                false
            ),
            Role::MuxRetireSession(os(&["a", "--harness", "h", "--session-id", "i"]))
        );
        assert_eq!(
            decide_role(&os(&["mux", "rows", "--json"]), false),
            Role::MuxRows(os(&["--json"]))
        );
        assert_eq!(
            decide_role(&os(&["mux", "where", "x919"]), false),
            Role::MuxWhere(os(&["x919"]))
        );
        assert_eq!(
            decide_role(&os(&["mux", "workspace", "prune", "--dry-run"]), false),
            Role::MuxWorkspace(WorkspaceOp::Prune(tail(os(&["--dry-run"]))))
        );
        assert_eq!(
            decide_role(
                &os(&["mux", "block", "pipe", "--from", "4", "--to", "2"]),
                false
            ),
            Role::MuxBlock(BlockOp::Pipe(tail(os(&["--from", "4", "--to", "2"]))))
        );
        assert_eq!(
            decide_role(&os(&["mux", "tab", "ls", "--json"]), false),
            Role::MuxTab(TabOp::Ls(tail(os(&["--json"]))))
        );
        // The pane group keeps no help flag (the root disables it and clap
        // propagates that); the explicit help door is `fno mux help pane`,
        // which renders after_help with the pane reference texts.
        assert!(matches!(
            decide_role(&os(&["mux", "pane", "-h"]), false),
            Role::MuxUsage(_)
        ));
    }

    #[test]
    fn char_bare_families_and_unknown_verbs_are_usage() {
        // Bare families and unknown family words land on the usage role
        // (exit 2 on stderr) - never a forward. An unknown verb INSIDE a
        // family is the family's refusal: today it rides the family role
        // (`mux pane bogus` -> MuxPane(["bogus"]), refused downstream);
        // after the cutover the classifier refuses it by name (AC1-ERR), so
        // the classifier-level pin covers the bare/unknown-family shapes.
        for bad in [
            vec!["mux"],
            vec!["mux", "bogus"],
            vec!["mux", "pane"],
            vec!["mux", "block"],
            vec!["mux", "layout"],
            vec!["mux", "workspace"],
            vec!["mux", "web"],
            vec!["mux", "where"],
            vec!["mux", "view"],
            vec!["mux", "retire-session"],
            vec!["mux", "ls", "x"],
        ] {
            let argv: Vec<OsString> = bad.iter().map(OsString::from).collect();
            assert!(
                matches!(decide_role(&argv, false), Role::MuxUsage(_)),
                "expected usage for {bad:?}"
            );
        }
        // The family-internal unknown verbs now refuse at the classifier,
        // naming the verb and its path (AC1-ERR).
        for bad in [["mux", "pane", "bogus"], ["mux", "tab", "bogus"]] {
            let argv: Vec<OsString> = bad.iter().map(OsString::from).collect();
            assert!(
                matches!(decide_role(&argv, false), Role::MuxUsage(m) if m.contains("bogus")),
                "expected a refusal naming bogus for {bad:?}"
            );
        }
    }

    #[test]
    fn serve_parses_the_status_flag_like_stop() {
        // `--status` is the read door beside `--stop`: it parses alone and
        // alongside --web/--port, and the mode-required check admits all
        // three modes.
        let parsed = parse_web_args(&os(&["--status"])).expect("--status parses");
        assert!(parsed.status);
        assert!(!parsed.stop);
        let parsed = parse_web_args(&os(&["--web", "--port", "9001", "--status"]))
            .expect("--web --status parses");
        assert!(parsed.status);
        assert_eq!(parsed.port, 9001);
        assert!(parse_web_args(&os(&["--server", "main"])).is_none());
    }
}
