//! Typed classification for the `fno` front door.
//!
//! One clap declaration set owns the native-vs-forward boundary and the
//! already-typed native verbs; the carry-verbatim mux families (pane, block,
//! tab, layout, rows, where, thread, reseat, retire-session, view, workspace,
//! serve, web) still parse inside `mux_cli` until their cutover wave, so
//! `MuxCmd::Other` carries their argv byte-verbatim. The classifier keeps
//! Footnote's explicit compatibility rules at the command edge: the Locked-7
//! leading `--server`/`--session` pair, the socket-path role for a `--server`
//! value containing `/`, and the deprecation note that names the flag.

use std::ffi::OsString;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

/// What the front door decided to do with one invocation.
#[derive(Debug, PartialEq, Eq)]
pub enum FrontDoor {
    /// Attach (spawn the server if absent). `Some(name)` pins the session;
    /// `explicit_socket` marks a `--server` value containing `/` (the
    /// internal role `client.rs` spawns with an absolute path).
    Attach {
        name: Option<String>,
        explicit_socket: bool,
    },
    /// `fno version [--json]`: the mux self-report.
    Version { json: bool },
    /// A native mux invocation, typed as far as this wave cutover reaches.
    Mux(MuxParsed),
    /// Native-shaped but malformed: `message` prints on stderr, exit 2. The
    /// text is either clap's rendered help (an explicit help request on a
    /// group) or one command-qualified refusal line naming the bad token.
    Usage { message: String },
    /// Everything else: forward byte-verbatim to the Python CLI.
    Forward,
}

/// The `mux` subcommand's typed parse.
#[derive(Debug, PartialEq, Eq)]
pub struct MuxParsed {
    pub cmd: MuxCmd,
}

/// The mux verbs typed in this wave, plus the carry-verbatim `Other` arm the
/// later mux wave consumes.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum MuxCmd {
    /// `mux server [--server <n>] [--session <n>]`
    Server(ServerArgs),
    /// `mux attach <name>`
    Attach { name: String },
    /// `mux ls [--json]`
    Ls {
        #[command(flatten)]
        json: JsonOnly,
    },
    /// `mux doctor [--json]`
    Doctor {
        #[command(flatten)]
        json: JsonOnly,
    },
    /// `mux stats [--json]` (hidden, read-only)
    #[command(hide = true)]
    Stats {
        #[command(flatten)]
        json: JsonOnly,
    },
    /// `mux kill-server [<name>] [--end-unkept | --stale-idle | --all] [--json]`
    KillServer {
        /// Optional server name (default resolution: flag > env > default)
        name: Option<String>,
        #[command(flatten)]
        json: JsonOnly,
        /// End live panes no keeper holds (otherwise a kill refuses)
        #[arg(long)]
        end_unkept: bool,
        /// Kill every live stale-wire session hosting no live pane
        #[arg(long)]
        stale_idle: bool,
        /// Kill every live session
        #[arg(long)]
        all: bool,
    },
    /// `mux shell-init [<shell>] [--json]`
    ShellInit {
        /// Shell to print the snippet for (the verb validates the spelling)
        shell: Option<String>,
        #[command(flatten)]
        json: JsonOnly,
    },
    /// `mux command <selector> --text <native-command> --proof <kind>`
    Command(MuxCommandArgs),
    /// `mux serve --web|--stop|--status ...`: the read-only web bridge
    /// `--web`, `--stop` or `--status` is required; the flags are
    /// the bridge's own (parse_web_args keeps them exact).
    #[command(disable_help_flag = true)]
    Serve(MuxTail),
    /// `mux web reap [--json]`: the bridge marker's corpse sweep. A bare
    /// `mux web` refuses: clap requires the `reap` operation.
    Web {
        #[command(subcommand)]
        op: WebOp,
    },
    /// `mux pane <verb> ...`: the v4 pane script API (nothing under
    /// `mux pane` ever forwards to Python).
    Pane {
        #[command(subcommand)]
        op: PaneOp,
    },
    /// `mux block pipe|annotate ...`: the block porcelain. A bare
    /// `mux block` refuses: clap requires the operation word.
    Block {
        #[command(subcommand)]
        op: BlockOp,
    },
    /// `mux tab ls|create|rename|join|move|close ...`: the
    /// layout-tab script verbs.
    Tab {
        #[command(subcommand)]
        op: TabOp,
    },
    /// `mux layout get|apply|graft ...`: nested layout trees and specs.
    /// The [`MuxCommon`] flags ride the group too, so a flag before the
    /// operation word still resolves it (`mux layout --json apply ...`).
    Layout {
        #[command(flatten)]
        common: MuxCommon,
        #[command(subcommand)]
        op: LayoutOp,
    },
    /// `mux rows [--json] ...`: the one row-set receipt (hidden).
    #[command(hide = true, disable_help_flag = true)]
    Rows(MuxTail),
    /// `mux where <fno_id-or-tab>`: resolve an fno session id to its
    /// location. The selector is required; a bare `mux where`
    /// refuses.
    #[command(disable_help_flag = true)]
    Where(MuxTailReq),
    /// `mux thread <name> [--portal N]` (hidden): show a thread
    /// row through a portal; `mux thread reseat` (v72) moves a live
    /// pane-hosted worker into a portal seat instead.
    #[command(hide = true)]
    Thread {
        #[command(subcommand)]
        op: ThreadOp,
    },
    /// `mux retire-session ...`: the exact-session retirement door
    /// (v75, hidden). The selector is required.
    #[command(hide = true, disable_help_flag = true)]
    RetireSession(MuxTailReq),
    /// `mux view <selector> [--url] [--fzf] [--json]`: point the
    /// operator's view at the pane hosting an agent. The selector is
    /// required; `-h` inside the tail stays the family's.
    #[command(disable_help_flag = true)]
    View(MuxTailReq),
    /// `mux workspace prune|restore ...`: workspace-store maintenance
    /// A bare `mux workspace` refuses: clap requires the
    /// operation word.
    Workspace {
        #[command(subcommand)]
        op: WorkspaceOp,
    },
}

/// The identity-pinned native action door. Keep this declaration typed so the
/// flag registry, help, and front-door classifier all share one contract.
#[derive(Args, Debug, PartialEq, Eq, Clone)]
pub struct MuxCommandArgs {
    /// Agent name, node id, or full harness session id.
    pub selector: String,
    /// Native command to execute, for example `/compact` or `/rc`.
    #[arg(long)]
    pub text: String,
    /// Postcondition recipe: compact, goal-active, or screen.
    #[arg(long, value_parser = ["compact", "goal-active", "screen"])]
    pub proof: String,
    /// Optional bounded screen assertion for the screen recipe.
    #[arg(long)]
    pub expect: Option<String>,
    /// Maximum seconds spent waiting for the command-specific proof.
    #[arg(long, default_value_t = 30)]
    pub timeout_seconds: u64,
    /// Stable id used to make retries idempotent.
    #[arg(long)]
    pub request_id: Option<String>,
}

/// The kill-server request the role carries: a NAME with the break-glass
/// flags, or one selector over the whole session table.
#[derive(Debug, PartialEq, Eq)]
pub struct KillRequest {
    pub name: Option<String>,
    pub json: bool,
    pub end_unkept: bool,
    pub stale_idle: bool,
    pub all: bool,
}

impl KillRequest {
    /// The named form with every selector off.
    pub fn simple(name: Option<&str>, json: bool) -> Self {
        KillRequest {
            name: name.map(str::to_string),
            json,
            end_unkept: false,
            stale_idle: false,
            all: false,
        }
    }
}

/// `mux block`'s operation set.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum BlockOp {
    /// `mux block pipe`: pipe a completed block into another pane's input
    #[command(disable_help_flag = true)]
    Pipe(MuxTail),
    /// `mux block annotate`: record a completed block as an operator finding
    #[command(disable_help_flag = true)]
    Annotate(MuxTail),
}

/// `mux tab`'s operation set.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum TabOp {
    /// List tabs
    #[command(disable_help_flag = true)]
    Ls(MuxTail),
    /// Create a tab
    #[command(disable_help_flag = true)]
    Create(MuxTail),
    /// Rename a tab
    #[command(disable_help_flag = true)]
    Rename(MuxTail),
    /// Join a tab to another tab
    #[command(disable_help_flag = true)]
    Join(MuxTail),
    /// Move a tab to a new position
    #[command(disable_help_flag = true)]
    Move(MuxTail),
    /// Close a tab
    #[command(disable_help_flag = true)]
    Close(MuxTail),
}

/// `mux layout`'s operation set. The re-sliced tail keeps the operation
/// word in place (`mux layout --json apply spec.toml` slices to
/// `["--json","apply","spec.toml"]`), because a common flag may sit
/// before the verb and [`MuxCommon::take`] strips it from any position.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum LayoutOp {
    /// Dump the nested layout tree + geometry (the default read)
    #[command(disable_help_flag = true)]
    Get(MuxTail),
    /// Apply a layout spec (--template/--slot or a .toml spec file)
    #[command(disable_help_flag = true)]
    Apply(MuxTail),
    /// Graft a spec onto a fresh tab anchored at a pane
    #[command(disable_help_flag = true)]
    Graft(MuxTail),
}

/// `mux workspace`'s operation set.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum WorkspaceOp {
    /// Reap dead-origin residue from the workspace store
    #[command(disable_help_flag = true)]
    Prune(MuxTail),
    /// Bring the stored workspaces' worker members back
    #[command(disable_help_flag = true)]
    Restore(MuxTail),
}

/// `mux thread`'s operation set: `reseat` is the one declared operation;
/// anything else is the row's name or attach id (external arm).
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum ThreadOp {
    /// `mux thread reseat <agent-name|pane-id> [--portal N]`: move a live
    /// pane-hosted worker into a portal seat, keeping its PTY
    #[command(hide = true, disable_help_flag = true)]
    Reseat(MuxTailReq),
    /// A thread's name or attach id: `mux thread <name> [--portal N]`
    #[command(external_subcommand, hide = true)]
    Name(Vec<OsString>),
}

impl ThreadOp {
    /// The re-sliced family argv (see `mux_with_tail`).
    pub fn tail(&self) -> Vec<OsString> {
        match self {
            ThreadOp::Reseat(MuxTailReq { tail }) => tail.clone(),
            ThreadOp::Name(words) => words.clone(),
        }
    }
}

/// The untyped family tail: declared ONLY so clap swallows the family's
/// own argv without parsing it. The dispatch tail is re-sliced from the
/// original argv (`mux_with_tail`); this captured copy is never read.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct MuxTail {
    /// The family's own argv, byte-verbatim
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub tail: Vec<OsString>,
}

/// [`MuxTail`] with at least one token: the leaves whose bare form is the
/// usage refusal (`mux where`, `mux view`, `mux retire-session`,
/// `mux thread reseat`).
#[derive(Args, Debug, PartialEq, Eq)]
pub struct MuxTailReq {
    /// The family's own argv, byte-verbatim (at least one token)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    pub tail: Vec<OsString>,
}

impl PaneOp {
    /// The re-sliced family argv (see `mux_with_tail`).
    pub fn tail(&self) -> Vec<OsString> {
        match self {
            PaneOp::Ls(t)
            | PaneOp::Read(t)
            | PaneOp::Run(t)
            | PaneOp::Send(t)
            | PaneOp::Wait(t)
            | PaneOp::Kill(t)
            | PaneOp::Claim(t)
            | PaneOp::Release(t)
            | PaneOp::Split(t)
            | PaneOp::Break(t)
            | PaneOp::Focus(t)
            | PaneOp::Keeper {
                op: KeeperOp::List(t),
            } => t.tail.clone(),
        }
    }

    /// Same operation, dispatch tail re-sliced (see `mux_with_tail`).
    fn with_tail(self, t: MuxTail) -> Self {
        match self {
            PaneOp::Ls(_) => PaneOp::Ls(t),
            PaneOp::Read(_) => PaneOp::Read(t),
            PaneOp::Run(_) => PaneOp::Run(t),
            PaneOp::Send(_) => PaneOp::Send(t),
            PaneOp::Wait(_) => PaneOp::Wait(t),
            PaneOp::Kill(_) => PaneOp::Kill(t),
            PaneOp::Claim(_) => PaneOp::Claim(t),
            PaneOp::Release(_) => PaneOp::Release(t),
            PaneOp::Split(_) => PaneOp::Split(t),
            PaneOp::Break(_) => PaneOp::Break(t),
            PaneOp::Focus(_) => PaneOp::Focus(t),
            PaneOp::Keeper { .. } => unreachable!(),
        }
    }
}

impl BlockOp {
    pub fn tail(&self) -> Vec<OsString> {
        match self {
            BlockOp::Pipe(t) | BlockOp::Annotate(t) => t.tail.clone(),
        }
    }

    fn with_tail(self, t: MuxTail) -> Self {
        match self {
            BlockOp::Pipe(_) => BlockOp::Pipe(t),
            BlockOp::Annotate(_) => BlockOp::Annotate(t),
        }
    }
}

impl TabOp {
    pub fn tail(&self) -> Vec<OsString> {
        match self {
            TabOp::Ls(t)
            | TabOp::Create(t)
            | TabOp::Rename(t)
            | TabOp::Join(t)
            | TabOp::Move(t)
            | TabOp::Close(t) => t.tail.clone(),
        }
    }

    fn with_tail(self, t: MuxTail) -> Self {
        match self {
            TabOp::Ls(_) => TabOp::Ls(t),
            TabOp::Create(_) => TabOp::Create(t),
            TabOp::Rename(_) => TabOp::Rename(t),
            TabOp::Join(_) => TabOp::Join(t),
            TabOp::Move(_) => TabOp::Move(t),
            TabOp::Close(_) => TabOp::Close(t),
        }
    }
}

impl LayoutOp {
    pub fn tail(&self) -> Vec<OsString> {
        match self {
            LayoutOp::Get(t) | LayoutOp::Apply(t) | LayoutOp::Graft(t) => t.tail.clone(),
        }
    }

    fn with_tail(self, t: MuxTail) -> Self {
        match self {
            LayoutOp::Get(_) => LayoutOp::Get(t),
            LayoutOp::Apply(_) => LayoutOp::Apply(t),
            LayoutOp::Graft(_) => LayoutOp::Graft(t),
        }
    }
}

impl WorkspaceOp {
    pub fn tail(&self) -> Vec<OsString> {
        match self {
            WorkspaceOp::Prune(t) | WorkspaceOp::Restore(t) => t.tail.clone(),
        }
    }

    fn with_tail(self, t: MuxTail) -> Self {
        match self {
            WorkspaceOp::Prune(_) => WorkspaceOp::Prune(t),
            WorkspaceOp::Restore(_) => WorkspaceOp::Restore(t),
        }
    }
}

impl WebOp {
    pub fn tail(&self) -> Vec<OsString> {
        match self {
            WebOp::Reap(t) => t.tail.clone(),
        }
    }

    fn with_tail(self, t: MuxTail) -> Self {
        match self {
            WebOp::Reap(_) => WebOp::Reap(t),
        }
    }
}

/// The root declaration set: the leading attach pair plus the subcommands.
#[derive(Parser, Debug)]
#[command(name = "fno", disable_help_flag = true, disable_version_flag = true)]
pub struct FnoRoot {
    /// Attach to (or spawn) the named server; a value containing `/` names an explicit socket path (internal role)
    #[arg(long, value_name = "NAME", conflicts_with = "session")]
    pub server: Option<String>,
    /// Deprecated spelling of --server (warns; alias removed in a future release)
    #[arg(long, hide = true, value_name = "NAME")]
    pub session: Option<String>,
    #[command(subcommand)]
    pub cmd: Option<RootCmd>,
}

/// The root subcommand set.
#[derive(Subcommand, Debug)]
pub enum RootCmd {
    /// The native terminal multiplexer surface
    Mux(MuxRoot),
    /// Print the mux binary's baked-in build rev
    Version {
        /// Emit one JSON object on stdout
        #[arg(long)]
        json: bool,
    },
    /// Unclaimed argv: the forwarded Python surface, byte-verbatim.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// The mux subcommand's arg set.
#[derive(Args, Debug)]
pub struct MuxRoot {
    #[command(subcommand)]
    pub cmd: MuxCmd,
}

/// `mux web`'s operation set.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum WebOp {
    /// `mux web reap [--json]`: remove every bridge marker whose port refuses
    #[command(disable_help_flag = true)]
    Reap(MuxTail),
}

/// `mux pane`'s operation set (the order today's dispatcher taught, `keeper`
/// last). Every leaf carries its family argv untyped.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum PaneOp {
    /// List panes (`--fno-id <id>` filters by session id)
    #[command(disable_help_flag = true)]
    Ls(MuxTail),
    /// Print a pane's screen (all of it, or --lines / --block)
    #[command(disable_help_flag = true)]
    Read(MuxTail),
    /// Run a command in a new pane (the payload after -- is the command's own)
    #[command(disable_help_flag = true)]
    Run(MuxTail),
    /// Send text or keys to a pane
    #[command(disable_help_flag = true)]
    Send(MuxTail),
    /// Wait until a pane is idle, matches a pattern, or times out
    #[command(disable_help_flag = true)]
    Wait(MuxTail),
    /// Kill a pane
    #[command(disable_help_flag = true)]
    Kill(MuxTail),
    /// Claim a pane's writer lock
    #[command(disable_help_flag = true)]
    Claim(MuxTail),
    /// Release a pane's writer lock
    #[command(disable_help_flag = true)]
    Release(MuxTail),
    /// Split a pane (--direction is required)
    #[command(disable_help_flag = true)]
    Split(MuxTail),
    /// Break a pane out into its own named tab
    #[command(disable_help_flag = true)]
    Break(MuxTail),
    /// Focus a pane, a registry selector, or --fzf for the picker
    #[command(disable_help_flag = true)]
    Focus(MuxTail),
    /// Keeper-survival reads (direct socket scan, no server)
    #[command(hide = true)]
    Keeper {
        #[command(subcommand)]
        op: KeeperOp,
    },
}

/// `mux pane keeper`'s operation set.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum KeeperOp {
    /// List the keeper sockets directly (survives the server's death)
    #[command(disable_help_flag = true)]
    List(MuxTail),
}

/// Shared machine-output flag group for the scriptable no-positional verbs
/// (contracts build on this): one declaration, `-J` as the short alias.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct JsonOnly {
    /// Emit machine-readable JSON on stdout
    #[arg(short = 'J', long)]
    pub json: bool,
}

/// `mux server`'s flags.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct ServerArgs {
    /// Session name to serve
    #[arg(long, value_name = "NAME", conflicts_with = "session")]
    pub server: Option<String>,
    /// Deprecated spelling of --server (warns)
    #[arg(long, hide = true, value_name = "NAME")]
    pub session: Option<String>,
}

/// Classify a full argv (post-argv[0]) into a front-door role.
///
/// Only a claimed first token (`mux`, `version`, `--server`, `--session`)
/// enters the parser; everything else IS the forwarded Python surface,
/// byte-verbatim (AC1-FORWARD). A claimed shape that fails to parse is
/// usage, exit 2. The mapping reads the raw `ArgMatches` (not the derive's
/// typed layer) on purpose: the typed layer misroutes subcommand capture
/// once the enum also carries a non-subcommand arm.
pub fn classify(args: &[OsString]) -> FrontDoor {
    match front_command()
        .no_binary_name(true)
        .try_get_matches_from(args)
    {
        Ok(m) => match FnoRoot::from_arg_matches(&m) {
            Ok(root) => front_door_from(root, args),
            Err(_) => FrontDoor::Usage {
                message: "fno: unclassifiable native invocation".into(),
            },
        },
        Err(e) => {
            if !native_first(args) {
                return FrontDoor::Forward;
            }
            // `fno mux thread -h`/`--help` is the addressing door's one
            // self-teaching surface. The thread group keeps its help flag OFF
            // (its tail is a row key, and clap refuses a hyphen spelling
            // before the external arm can read it as one), so the help
            // request arrives here as a parse error: render the group's help
            // instead of reciting the refusal.
            if path_words(args).as_slice() == ["mux", "thread"]
                && args.len() == 3
                && args[2]
                    .to_str()
                    .map(|v| v == "-h" || v == "--help")
                    .unwrap_or(false)
            {
                return FrontDoor::Usage {
                    message: render_path_help(&["mux", "thread"]),
                };
            }
            // `fno mux -h` (or any explicit help request on a mux group)
            // renders clap's help for that group: the banner's replacement.
            let message = if args.first().and_then(|a| a.to_str()) == Some("mux")
                && e.kind() == clap::error::ErrorKind::DisplayHelp
            {
                e.render().to_string()
            } else {
                // One command-qualified line naming the bad token; the
                // deepest declared path becomes the prefix.
                let prefix = format!(
                    "fno{}",
                    path_words(args)
                        .iter()
                        .fold(String::new(), |a, w| format!("{a} {w}"))
                );
                refusal_line(&prefix, &e)
            };
            FrontDoor::Usage { message }
        }
    }
}

/// The front-door command, assembled once: the derive-built tree with the
/// pane group's after_help injected (the four pane help texts clap renders
/// on `fno mux pane -h`). classify, the inventory and completion all read
/// this one command, so the surfaces cannot drift.
pub fn front_command() -> clap::Command {
    FnoRoot::command().mut_subcommand("mux", |mux| {
        mux.mut_subcommand("pane", |pane| pane.after_help(pane_group_help()))
            .mut_subcommand("thread", |thread| thread.after_help(thread_group_help()))
    })
}

/// The pane verb words, kept in sync with [`PaneOp`] by the tree test.
pub const PANE_VERBS_LIST: &str =
    "ls|read|run|send|wait|kill|claim|release|split|break|focus|keeper";

/// The pane group's self-teaching help: the reference line plus the three
/// long help texts. clap renders it on `fno mux pane -h`; the run verb
/// reuses it for the `-h` inside its tail.
pub fn pane_group_help() -> String {
    format!(
        "{}; verbs: {}
{}
{}
{}",
        crate::mux_cli::PANE_REFERENCE_USAGE,
        PANE_VERBS_LIST,
        crate::mux_cli::PANE_SEND_RAW_HELP,
        crate::mux_cli::PANE_RUN_WORKER_HELP,
        crate::mux_cli::PANE_LS_IDENTITY_HELP
    )
}

/// The thread group's self-teaching help: the addressing contract the two
/// portal documents quote. classify routes `fno mux thread -h`/`--help` here
/// (the group keeps its help flag off, so the spelling arrives as a parse
/// error); `render_path_help(["mux", "thread"])` reads it.
pub fn thread_group_help() -> String {
    "\
The addressing door onto a live thread row: show it through a portal (or focus the portal it already has), and never create, resume, or duplicate a worker. The key matches one live row exactly, not by the prefix or substring tiers `fno mux view` uses; zero matches, or several rows answering the same key, refuse and spawn no worker.

  fno mux thread w2
  fno mux thread 3f9d3c55-1c2b-4e8a-9a3f-7b2c5d6e8f90
  fno agents whoami prints both values: `name` for either harness, the full `session` id for Codex, and the `short_id` job id for Claude - a full Claude transcript UUID is not accepted here.

Move a live pane-hosted worker into a portal seat, keeping its terminal:
  fno mux thread reseat <name> --portal N"
        .to_string()
}

/// The deepest declared command path in `args` (e.g. `["mux", "pane"]` for
/// `fno mux pane bogus`), used to qualify parse-error refusals. Walks the
/// declaration, stopping at the first token no level names.
fn path_words(args: &[OsString]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut cmd = front_command();
    for a in args {
        let Some(w) = a.to_str() else { break };
        if w.starts_with('-') {
            break;
        }
        let Some(sub) = cmd.find_subcommand(w) else {
            break;
        };
        words.push(w);
        cmd = sub.clone();
    }
    words
}

/// clap's rendered help for a resolved path (e.g. `["mux", "view"]`), for
/// the shapes that ask for help on a leaf that keeps its tail (`mux view -h`).
pub fn render_path_help(words: &[&str]) -> String {
    let mut cmd = front_command();
    for w in words {
        cmd = match cmd.find_subcommand(w) {
            Some(sub) => sub.clone(),
            None => return String::new(),
        };
    }
    cmd.render_help().to_string()
}

/// The typed front door: map the derive's own parse to a role. The family
/// tails re-slice from the ORIGINAL argv (`mux_with_tail`), never from
/// clap's captured values.
fn front_door_from(root: FnoRoot, args: &[OsString]) -> FrontDoor {
    // The one side effect classification keeps from the old cursor parser:
    // the `--session` deprecation note, which names the flag.
    if root.session.is_some() {
        crate::mux_cli::note_server_flag("--session");
    }
    let FnoRoot {
        server,
        session,
        cmd,
    } = root;
    if server.is_some() || session.is_some() {
        // Locked-7: only the exact pair attaches. A server/session flag
        // beside a subcommand is usage - never a forward, and never the
        // socket role, which would silently drop the trailing argv (AC3-ERR).
        return match cmd {
            None => {
                let from_server = server.is_some();
                let name = server.or(session).unwrap_or_default();
                let explicit_socket = from_server && name.contains('/');
                FrontDoor::Attach {
                    name: Some(name),
                    explicit_socket,
                }
            }
            Some(_) => FrontDoor::Usage {
                message: "fno: a leading --server/--session pair never combines with a subcommand"
                    .into(),
            },
        };
    }
    match cmd {
        // Bare `fno` (no flags, no subcommand) is the attach the client runs:
        // the picker when nothing is pinned, the nested guard otherwise.
        None if args.is_empty() => FrontDoor::Attach {
            name: None,
            explicit_socket: false,
        },
        // `fno --` (an escape with nothing after it) forwards, as before.
        None => FrontDoor::Forward,
        Some(RootCmd::Version { json }) => FrontDoor::Version { json },
        // Everything unclaimed IS the forwarded Python surface.
        Some(RootCmd::External(_)) => FrontDoor::Forward,
        Some(RootCmd::Mux(mux)) => {
            // The server subcommand carries its own deprecated `--session`
            // alias; it warns here, exactly where the root-level spelling
            // warned above (main.rs routes on the parsed fields only).
            if let MuxCmd::Server(s) = &mux.cmd {
                if s.session.is_some() {
                    crate::mux_cli::note_server_flag("--session");
                }
            }
            FrontDoor::Mux(MuxParsed {
                cmd: mux_with_tail(mux.cmd, args),
            })
        }
    }
}

/// Re-slice each family's dispatch tail from the ORIGINAL argv, after the
/// family's leading words. The captured copies ride the typed parse unused.
fn mux_with_tail(cmd: MuxCmd, args: &[OsString]) -> MuxCmd {
    match cmd {
        MuxCmd::Pane { op } => MuxCmd::Pane {
            op: match op {
                PaneOp::Keeper { .. } => PaneOp::Keeper {
                    op: KeeperOp::List(tail_from(args, 4)),
                },
                other => other.with_tail(tail_from(args, 3)),
            },
        },
        MuxCmd::Block { op } => MuxCmd::Block {
            op: op.with_tail(tail_from(args, 3)),
        },
        MuxCmd::Tab { op } => MuxCmd::Tab {
            op: op.with_tail(tail_from(args, 3)),
        },
        // layout keeps the operation word in its tail: a common flag may sit
        // before it, and MuxCommon::take strips flags from any position.
        MuxCmd::Layout { common, op } => MuxCmd::Layout {
            common,
            op: op.with_tail(tail_from(args, 2)),
        },
        MuxCmd::Web { op } => MuxCmd::Web {
            op: op.with_tail(tail_from(args, 3)),
        },
        MuxCmd::Workspace { op } => MuxCmd::Workspace {
            op: op.with_tail(tail_from(args, 3)),
        },
        MuxCmd::Serve(_) => MuxCmd::Serve(tail_from(args, 2)),
        MuxCmd::Rows(_) => MuxCmd::Rows(tail_from(args, 2)),
        MuxCmd::Where(_) => MuxCmd::Where(tail_req_from(args, 2)),
        MuxCmd::RetireSession(_) => MuxCmd::RetireSession(tail_req_from(args, 2)),
        MuxCmd::View(_) => MuxCmd::View(tail_req_from(args, 2)),
        MuxCmd::Thread { op } => MuxCmd::Thread {
            op: match op {
                ThreadOp::Reseat(_) => ThreadOp::Reseat(tail_req_from(args, 3)),
                ThreadOp::Name(words) => ThreadOp::Name(words),
            },
        },
        // The seven already-typed verbs carry no tail.
        typed => typed,
    }
}

/// Slice the ORIGINAL argv from `start` on, byte-exact.
fn tail_from(args: &[OsString], start: usize) -> MuxTail {
    MuxTail {
        tail: args[start.min(args.len())..].to_vec(),
    }
}

/// Same, for the leaves whose bare form is a usage refusal.
fn tail_req_from(args: &[OsString], start: usize) -> MuxTailReq {
    MuxTailReq {
        tail: args[start.min(args.len())..].to_vec(),
    }
}

/// The first tokens that can begin a native invocation.
fn native_first(args: &[OsString]) -> bool {
    matches!(
        args.first().and_then(|a| a.to_str()),
        Some("mux" | "version" | "--server" | "--session")
    )
}

/// The native command inventory: one tab-separated row per declared path
/// (without the `fno` word), sorted - path, kind (root|group|leaf),
/// visibility, aliases, flags. Generated artifact of record for the Python
/// ratchets (`scripts/ci/native-command-tree.txt`); the freshness test below
/// fails `cargo test` the moment the artifact lags this tree.
pub fn render_inventory() -> String {
    fn flags_of(cmd: &clap::Command) -> Vec<String> {
        let mut flags: Vec<String> = Vec::new();
        for arg in cmd.get_arguments() {
            if arg.is_positional() || arg.get_id() == "help" || arg.get_id() == "version" {
                continue;
            }
            let mut spellings: Vec<String> = Vec::new();
            if let Some(long) = arg.get_long() {
                spellings.push(format!("--{long}"));
            }
            if let Some(short) = arg.get_short() {
                spellings.push(format!("-{short}"));
            }
            if spellings.is_empty() {
                continue;
            }
            if arg.is_hide_set() {
                spellings[0] = format!("!{}", spellings[0]);
            }
            flags.push(spellings.join(","));
        }
        flags.sort();
        flags
    }

    fn walk(cmd: &clap::Command, prefix: &str, rows: &mut Vec<String>) {
        let name = cmd.get_name().to_string();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix} {name}")
        };
        let kind = if cmd.has_subcommands() {
            "group"
        } else {
            "leaf"
        };
        let visibility = if cmd.is_hide_set() {
            "hidden"
        } else {
            "visible"
        };
        let aliases = cmd.get_aliases().collect::<Vec<_>>().join(",");
        let aliases = if aliases.is_empty() {
            "-".to_string()
        } else {
            aliases
        };
        let flags = flags_of(cmd).join(",");
        let flags = if flags.is_empty() {
            "-".to_string()
        } else {
            flags
        };
        rows.push(format!("{path}\t{kind}\t{visibility}\t{aliases}\t{flags}"));
        for sub in cmd.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            walk(sub, &path, rows);
        }
    }

    let mut rows: Vec<String> = Vec::new();
    let mut root_flags = flags_of(&front_command()).join(",");
    if root_flags.is_empty() {
        root_flags = "-".into();
    }
    rows.push(format!("(root)\troot\tvisible\t-\t{root_flags}"));
    for sub in front_command().get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        walk(sub, "", &mut rows);
    }
    rows.sort();
    let header = concat!(
        "# Native fno command inventory, generated from the typed clap tree.\n",
        "# Regenerate with:\n",
        "#   cargo run --quiet --manifest-path crates/fno/Cargo.toml --example native_command_tree > scripts/ci/native-command-tree.txt\n",
        "# Columns: path, kind (root|group|leaf), visibility, aliases, flags\n",
        "# (hidden flags prefixed !; a hidden FLAG ALIAS is not listed).\n"
    );
    format!("{header}{}\n", rows.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::os::unix::ffi::OsStringExt;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn os_raw(bytes: &[u8]) -> OsString {
        OsString::from_vec(bytes.to_vec())
    }

    #[test]
    fn registry_root_and_mux_have_help_and_no_duplicates() {
        let cmd = FnoRoot::command();
        let mut seen = std::collections::BTreeSet::new();
        for arg in cmd.get_arguments() {
            if let Some(long) = arg.get_long() {
                assert!(seen.insert(long.to_string()), "--{long} declared twice");
                if arg.is_hide_set() {
                    continue;
                }
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                assert!(!help.trim().is_empty(), "--{long}: empty help");
            }
        }
        for sub in cmd.get_subcommands() {
            assert!(
                sub.get_about()
                    .map(|a| !a.to_string().trim().is_empty())
                    .unwrap_or(false),
                "subcommand {} lacks an about line",
                sub.get_name()
            );
        }
    }

    #[test]
    fn bare_fno_is_the_attach() {
        // No argv at all is the bare attach (the picker or the nested
        // guard decides downstream); a bare `--` still forwards.
        assert_eq!(
            classify(&[]),
            FrontDoor::Attach {
                name: None,
                explicit_socket: false
            }
        );
        assert_eq!(classify(&os(&["--"])), FrontDoor::Forward);
    }

    #[test]
    fn forward_keeps_the_python_surface_byte_verbatim() {
        assert_eq!(classify(&os(&["backlog", "list"])), FrontDoor::Forward);
        assert_eq!(classify(&os(&["--help"])), FrontDoor::Forward);
        assert_eq!(classify(&os(&["--version"])), FrontDoor::Forward);
        assert_eq!(classify(&os(&["--wat"])), FrontDoor::Forward);
        // A non-UTF-8 payload inside the forwarded tail must survive as raw
        // bytes, never a parse error.
        let mut raw = os(&["backlog", "list"]);
        raw.push(os_raw(&[0xff]));
        assert_eq!(classify(&raw), FrontDoor::Forward);
    }

    #[test]
    fn locked7_leading_pair_attaches_socket_or_usage() {
        assert_eq!(
            classify(&os(&["--server", "work"])),
            FrontDoor::Attach {
                name: Some("work".into()),
                explicit_socket: false
            }
        );
        assert_eq!(
            classify(&os(&["--server", "/tmp/x.sock"])),
            FrontDoor::Attach {
                name: Some("/tmp/x.sock".into()),
                explicit_socket: true
            }
        );
        // Only the exact pair attaches: a bare flag or trailing argv is
        // usage, never a silent forward (AC3-ERR).
        assert!(matches!(
            classify(&os(&["--server"])),
            FrontDoor::Usage { .. }
        ));
        assert!(matches!(
            classify(&os(&["--server", "work", "backlog", "list"])),
            FrontDoor::Usage { .. }
        ));
        // The socket spelling obeys the same exact-pair rule: trailing argv
        // after `--server <path>` is usage, never a socket role that drops
        // the command.
        assert!(matches!(
            classify(&os(&["--server", "/tmp/x.sock", "version"])),
            FrontDoor::Usage { .. }
        ));
        assert!(matches!(
            classify(&os(&["--server", "/tmp/x.sock", "backlog", "list"])),
            FrontDoor::Usage { .. }
        ));
        assert!(matches!(
            classify(&os(&["--session"])),
            FrontDoor::Usage { .. }
        ));
        assert!(matches!(
            classify(&os(&["--session", "work", "backlog", "list"])),
            FrontDoor::Usage { .. }
        ));
    }

    #[test]
    fn version_is_typed_and_bounded() {
        assert_eq!(
            classify(&os(&["version"])),
            FrontDoor::Version { json: false }
        );
        assert_eq!(
            classify(&os(&["version", "--json"])),
            FrontDoor::Version { json: true }
        );
        assert!(matches!(
            classify(&os(&["version", "x"])),
            FrontDoor::Usage { .. }
        ));
        assert!(matches!(
            classify(&os(&["version", "--help"])),
            FrontDoor::Usage { .. }
        ));
    }

    #[test]
    fn mux_simple_verbs_parse_typed() {
        assert_eq!(
            classify(&os(&["mux", "ls", "--json"])),
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Ls {
                    json: JsonOnly { json: true }
                }
            })
        );
        assert_eq!(
            classify(&os(&["mux", "ls", "-J"])),
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Ls {
                    json: JsonOnly { json: true }
                }
            })
        );
        assert_eq!(
            classify(&os(&["mux", "doctor"])),
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Doctor {
                    json: JsonOnly { json: false }
                }
            })
        );
        assert_eq!(
            classify(&os(&["mux", "kill-server", "--json", "work"])),
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::KillServer {
                    name: Some("work".into()),
                    json: JsonOnly { json: true },
                    end_unkept: false,
                    stale_idle: false,
                    all: false,
                }
            })
        );
        assert_eq!(
            classify(&os(&["mux", "kill-server", "--", "--weird"])),
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::KillServer {
                    name: Some("--weird".into()),
                    json: JsonOnly { json: false },
                    end_unkept: false,
                    stale_idle: false,
                    all: false,
                }
            })
        );
    }

    #[test]
    fn native_command_door_accepts_identity_proof_and_boundary_flags() {
        let args = os(&[
            "mux",
            "command",
            "codex-thread-full-id",
            "--text",
            "/compact",
            "--proof",
            "compact",
            "--expect",
            "contextCompaction",
            "--timeout-seconds",
            "9",
            "--request-id",
            "req-1",
        ]);
        match classify(&args) {
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Command(parsed),
            }) => {
                assert_eq!(parsed.selector, "codex-thread-full-id");
                assert_eq!(parsed.text, "/compact");
                assert_eq!(parsed.proof, "compact");
                assert_eq!(parsed.timeout_seconds, 9);
                assert_eq!(parsed.request_id.as_deref(), Some("req-1"));
            }
            other => panic!("native command door parsed incorrectly: {other:?}"),
        }
    }

    #[test]
    fn inventory_artifact_is_fresh() {
        // The committed artifact must equal what the tree renders NOW. A new
        // operation added to FnoRoot without regenerating fails here, and the
        // message names the regenerate command (AC3-HP).
        let committed = include_str!("../../../scripts/ci/native-command-tree.txt");
        assert_eq!(
            committed,
            render_inventory(),
            "scripts/ci/native-command-tree.txt is stale - regenerate with: \
             cargo run --quiet --manifest-path crates/fno/Cargo.toml --example native_command_tree \
             > scripts/ci/native-command-tree.txt"
        );
    }

    #[test]
    fn inventory_paths_classify_as_positive_controls() {
        // Every leaf row classifies without an unrecognized-subcommand
        // refusal; every group row plus the probe verb refuses BY NAME. The
        // thread group is the one exception: its external arm accepts a bare
        // name, so the probe resolves as the thread's name argument.
        for row in render_inventory().lines().filter(|l| !l.starts_with('#')) {
            let cols: Vec<&str> = row.split('\t').collect();
            let path = cols[0];
            if path == "(root)" {
                continue;
            }
            let tokens: Vec<OsString> = path.split(' ').map(OsString::from).collect();
            if cols[1] == "group" {
                let mut probe = tokens.clone();
                probe.push(OsString::from("__fno_verb_probe__"));
                match classify(&probe) {
                    FrontDoor::Usage { message } => assert!(
                        message.contains("__fno_verb_probe__"),
                        "group {path} must name the probe in its refusal"
                    ),
                    FrontDoor::Mux(_) if path == "mux thread" => {
                        // `thread <name>` is the row's own argument surface.
                    }
                    other => panic!("group {path} accepted the probe without naming it: {other:?}"),
                }
            } else {
                assert!(
                    !matches!(classify(&tokens), FrontDoor::Usage { message }
                        if message.contains("unrecognized subcommand")),
                    "leaf path {path} must classify without an unrecognized-subcommand refusal"
                );
            }
        }
        // Hidden paths are IN the artifact, marked hidden (AC3-EDGE).
        for (path, kind) in [
            ("mux stats", "leaf"),
            ("mux rows", "leaf"),
            ("mux thread", "group"),
            ("mux thread reseat", "leaf"),
            ("mux retire-session", "leaf"),
            ("mux pane keeper", "group"),
        ] {
            let hit = render_inventory().lines().any(|l| {
                l.starts_with(path) && l.contains('\t') && l.split('\t').nth(2) == Some("hidden")
            });
            assert!(
                hit,
                "{path} must appear in the inventory as hidden ({kind})"
            );
        }
        // The root's hidden deprecated flag is marked with `!` (AC3-EDGE).
        assert!(
            render_inventory().contains("!--session"),
            "the root row must mark --session hidden"
        );
        // Operation arguments are in the artifact (AC3-EDGE).
        for path in ["mux pane keeper list", "mux thread reseat", "mux web reap"] {
            assert!(render_inventory().contains(path), "{path} must be listed");
        }
    }

    #[test]
    fn thread_group_help_renders_the_addressing_contract() {
        // The help body is the contract the two portal documents quote:
        // both address forms, the discovery verb, the exact-match boundary,
        // and the no-spawn refusal. `attach_id` stays internal; the help
        // teaches `name`, `session` and `short_id` (the whoami fields).
        let help = render_path_help(&["mux", "thread"]);
        assert!(help.contains("fno mux thread w2"), "{help}");
        assert!(
            help.contains("3f9d3c55-1c2b-4e8a-9a3f-7b2c5d6e8f90"),
            "{help}"
        );
        assert!(help.contains("fno agents whoami"), "{help}");
        assert!(help.contains("exact"), "{help}");
        assert!(help.contains("refuse and spawn no worker"), "{help}");
        assert!(help.contains("short_id"), "{help}");
        assert!(!help.contains("attach_id"), "{help}");
    }

    #[test]
    fn mux_malformed_shapes_are_named_refusals() {
        // Every malformed mux shape is a command-qualified one-line refusal
        // (exit 2 downstream) - never a forward, never a silent accept.
        for bad in [
            vec!["mux"],
            vec!["mux", "bogus"],
            vec!["mux", "ls", "x"],
            vec!["mux", "doctor", "--wat"],
            vec!["mux", "server", "--session"],
            vec!["mux", "attach"],
            vec!["mux", "ls", "--json", "--json"],
        ] {
            let argv: Vec<OsString> = bad.iter().map(OsString::from).collect();
            match classify(&argv) {
                FrontDoor::Usage { message } => {
                    assert!(message.starts_with("fno"), "refusal names the command path");
                }
                other => panic!("expected a refusal for {bad:?}, got {other:?}"),
            }
        }
        // The unknown verb is NAMED in its refusal (AC1-ERR).
        let msg = match classify(&os(&["mux", "bogus"])) {
            FrontDoor::Usage { message } => message,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(msg.contains("bogus"), "refusal names the verb: {msg}");
        let msg = match classify(&os(&["mux", "pane", "bogus"])) {
            FrontDoor::Usage { message } => message,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(
            msg.contains("fno mux pane") && msg.contains("bogus"),
            "refusal names the path and the verb: {msg}"
        );
    }

    #[test]
    fn typed_families_resolve_with_re_sliced_tails() {
        // The carry families ride their typed op enums; the payload tail is
        // re-sliced from the ORIGINAL argv, so a leading `--` survives
        // (clap would drop the escape) and flags precede the payload.
        let r = classify(&os(&["mux", "pane", "run", "--cwd", "/x", "--", "claude"]));
        match r {
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Pane { op },
            }) => {
                assert!(matches!(op, PaneOp::Run(_)));
                let got: Vec<String> = op
                    .tail()
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                assert_eq!(got, ["--cwd", "/x", "--", "claude"]);
            }
            other => panic!("typed pane expected, got {other:?}"),
        }
        let r = classify(&os(&["mux", "workspace", "prune", "--dry-run"]));
        match r {
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Workspace { op },
            }) => {
                assert!(matches!(op, WorkspaceOp::Prune(_)));
                assert_eq!(op.tail().len(), 1, "--dry-run rides the tail");
            }
            other => panic!("typed workspace expected, got {other:?}"),
        }
        // layout: a common flag before the verb still resolves the verb, and
        // the tail keeps the flag for the family's own MuxCommon::take.
        let r = classify(&os(&["mux", "layout", "--json", "apply", "spec.toml"]));
        match r {
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Layout { common, op },
            }) => {
                assert!(common.json, "the group parsed --json");
                assert!(matches!(op, LayoutOp::Apply(_)));
                let got: Vec<String> = op
                    .tail()
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                assert_eq!(got, ["--json", "apply", "spec.toml"]);
            }
            other => panic!("typed layout expected, got {other:?}"),
        }
        // thread: the declared reseat op routes; a bare name rides the
        // external arm.
        let r = classify(&os(&["mux", "thread", "reseat", "7"]));
        match r {
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Thread { op },
            }) => {
                assert!(matches!(op, ThreadOp::Reseat(_)));
                assert_eq!(op.tail(), os(&["7"]));
            }
            other => panic!("typed thread expected, got {other:?}"),
        }
        let r = classify(&os(&["mux", "thread", "wk"]));
        match r {
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Thread { op },
            }) => {
                assert!(matches!(op, ThreadOp::Name(_)));
                assert_eq!(op.tail(), os(&["wk"]));
            }
            other => panic!("typed thread expected, got {other:?}"),
        }
    }
}

/// The common mux verb flags: the server axis plus the machine-output flag
/// every scriptable verb parses. One declaration; the deprecation
/// note fires here when the legacy `--session` spelling binds. Verb-specific
/// tokens ride through in `rest`, order kept, so each verb's own grammar
/// still refuses its unknowns.
#[derive(Parser, Debug, Default, PartialEq, Eq)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct MuxCommon {
    /// Server (socket session) this verb addresses
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
    /// Deprecated spelling of --server (warns)
    #[arg(long, hide = true, value_name = "NAME")]
    pub session: Option<String>,
    /// Emit machine-readable JSON on stdout
    #[arg(long)]
    pub json: bool,
}

impl MuxCommon {
    /// The known flag spellings this group owns.
    pub const FLAGS: &'static [&'static str] = &["--server", "--session", "--json"];

    /// Split the common flags out of a small verb's argv. Non-UTF-8 input is
    /// the old refusal; a valueless server flag is the old `{flag} needs a
    /// value` error; a repeated common flag is refused so the verb's own
    /// grammar cannot disagree with this one.
    pub fn take(toks: &[OsString]) -> Result<(MuxCommon, Vec<String>), String> {
        let mut pairs: Vec<String> = Vec::new();
        let mut rest = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            let tok = toks[i]
                .to_str()
                .ok_or_else(|| "non-UTF-8 argument".to_string())?;
            if Self::FLAGS.contains(&tok) {
                if tok != "--json" {
                    if i + 1 >= toks.len() {
                        return Err(format!("{tok} needs a value"));
                    }
                    let value = toks[i + 1]
                        .to_str()
                        .ok_or_else(|| "non-UTF-8 argument".to_string())?;
                    pairs.push(tok.to_string());
                    pairs.push(value.to_string());
                    i += 2;
                } else {
                    pairs.push(tok.to_string());
                    i += 1;
                }
                continue;
            }
            rest.push(tok.to_string());
            i += 1;
        }
        let cmd = <Self as clap::CommandFactory>::command();
        let matches = cmd
            .try_get_matches_from(pairs)
            .map_err(|e| refusal_line("fno mux", &e))?;
        let session = matches.get_one::<String>("session").cloned();
        if session.is_some() {
            crate::mux_cli::note_server_flag("--session");
        }
        Ok((
            MuxCommon {
                server: matches.get_one::<String>("server").cloned(),
                session,
                json: matches.get_flag("json"),
            },
            rest,
        ))
    }
}

/// `fno mux thread <name>`'s flags (/): the portal reach and the
/// placement trio, one typed declaration replacing the verb's scan and its
/// flag-value macro.
#[derive(Parser, Debug, Default, PartialEq, Eq)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct ThreadArgs {
    /// Which portal to reach through: an index, or "new" for a dedicated portal in a new tab
    #[arg(long)]
    pub portal: Option<String>,
    /// Workspace whose tab hosts the thread
    #[arg(long, alias = "squad", short = 's', value_name = "NAME")]
    pub workspace: Option<String>,
    /// Split direction for a fresh open
    #[arg(long, short = 'x')]
    pub split: Option<String>,
    /// Tab selector for a fresh open
    #[arg(long)]
    pub tab: Option<String>,
    /// Anchor pane id for a fresh open
    #[arg(long)]
    pub at: Option<String>,
    /// The agent name or attach id
    pub name: Option<String>,
}

/// `fno mux block pipe`'s flags. Values stay strings here; the pure
/// validators (`parse_u64`, `parse_block_sel`) keep their exact refusals
/// when the parser converts.
#[derive(Parser, Debug, Default, PartialEq, Eq)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct BlockPipeArgs {
    /// Server (socket session) this verb addresses
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
    /// Deprecated spelling of --server (warns)
    #[arg(long, hide = true, value_name = "NAME")]
    pub session: Option<String>,
    /// Emit machine-readable JSON on stdout
    #[arg(long)]
    pub json: bool,
    /// Force when the target pane is not idle
    #[arg(long)]
    pub force: bool,
    /// Source pane id to read the completed block from
    #[arg(long)]
    pub from: Option<String>,
    /// Target pane id to pipe into
    #[arg(long)]
    pub to: Option<String>,
    /// Which block to read (last | <seq>)
    #[arg(long)]
    pub block: Option<String>,
}

/// `fno mux block annotate`'s flags; same string-then-validate shape as
/// [`BlockPipeArgs`].
#[derive(Parser, Debug, Default, PartialEq, Eq)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct BlockAnnotateArgs {
    /// Server (socket session) this verb addresses
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
    /// Deprecated spelling of --server (warns)
    #[arg(long, hide = true, value_name = "NAME")]
    pub session: Option<String>,
    /// Source pane id to read the block from
    #[arg(long)]
    pub from: Option<String>,
    /// Which block to read (last | <seq>)
    #[arg(long)]
    pub block: Option<String>,
    /// The node the finding is recorded against
    #[arg(long)]
    pub node: Option<String>,
    /// The finding text
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

/// One command-qualified refusal line for a parse failure. The caller prints
/// it to stderr and exits 2; clap's own multi-line usage block never reaches
/// the operator.
pub fn refusal_line(cmd: &str, err: &clap::Error) -> String {
    let rendered = err.render().to_string();
    let first = rendered
        .lines()
        .next()
        .unwrap_or("invalid arguments")
        .trim()
        .trim_start_matches("error: ")
        .trim();
    format!("{cmd}: {first}")
}
