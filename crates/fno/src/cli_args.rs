//! Typed classification for the `fno` front door (x-861c).
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

use clap::{Args, CommandFactory, Parser, Subcommand};

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
    /// Native-shaped but malformed: banner + exit 2.
    Usage,
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
    /// `mux kill-server [<name>] [--json]`
    KillServer {
        /// Optional server name (default resolution: flag > env > default)
        name: Option<String>,
        #[command(flatten)]
        json: JsonOnly,
    },
    /// `mux shell-init [<shell>] [--json]`
    ShellInit {
        /// Shell to print the snippet for (the verb validates the spelling)
        shell: Option<String>,
        #[command(flatten)]
        json: JsonOnly,
    },
    /// A carry-verbatim family verb: `mux <verb> <tail>`, tail byte-exact.
    #[command(external_subcommand)]
    Other(Vec<OsString>),
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

/// Shared machine-output flag group for the scriptable no-positional verbs
/// (x-81b6 contracts build on this): one declaration, `-J` as the short alias.
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
    #[arg(long, value_name = "NAME")]
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
    match FnoRoot::command()
        .no_binary_name(true)
        .try_get_matches_from(args)
    {
        Ok(m) => map_matches(&m, args),
        Err(_) => {
            if !native_first(args) {
                return FrontDoor::Forward;
            }
            // `mux <family> <tail>`: the carry families are native even
            // though this wave does not type-parse them; the mux tail rides
            // the Other arm to the carry re-route in main.rs.
            if args.first().and_then(|a| a.to_str()) == Some("mux") {
                return FrontDoor::Mux(MuxParsed {
                    cmd: MuxCmd::Other(args[1..].to_vec()),
                });
            }
            FrontDoor::Usage
        }
    }
}

/// The first tokens that can begin a native invocation.
fn native_first(args: &[OsString]) -> bool {
    matches!(
        args.first().and_then(|a| a.to_str()),
        Some("mux" | "version" | "--server" | "--session")
    )
}

fn map_matches(m: &clap::ArgMatches, args: &[OsString]) -> FrontDoor {
    let server = m.get_one::<String>("server").cloned();
    let session = m.get_one::<String>("session").cloned();
    // The one side effect classification keeps from the old cursor parser:
    // the `--session` deprecation note, which names the flag. The bound
    // field IS the spelling record: only `--session` fills `session`.
    if session.is_some() {
        crate::mux_cli::note_server_flag("--session");
    }
    if server.is_some() || session.is_some() {
        let from_server = server.is_some();
        let name = server.or(session).unwrap_or_default();
        if from_server && name.contains('/') {
            return FrontDoor::Attach {
                name: Some(name),
                explicit_socket: true,
            };
        }
        if m.subcommand_name().is_none() {
            return FrontDoor::Attach {
                name: Some(name),
                explicit_socket: false,
            };
        }
        // A server/session flag beside a subcommand is not the exact pair:
        // Locked-7 says usage, never a forward (AC3-ERR).
        return FrontDoor::Usage;
    }
    match m.subcommand() {
        Some(("mux", mux_m)) => match mux_m.subcommand() {
            Some(("server", s)) => {
                let srv = s.get_one::<String>("server").cloned();
                if s.get_one::<String>("session").is_some() {
                    crate::mux_cli::note_server_flag("--session");
                }
                FrontDoor::Mux(MuxParsed {
                    cmd: MuxCmd::Server(ServerArgs {
                        server: srv,
                        session: s.get_one::<String>("session").cloned(),
                    }),
                })
            }
            Some(("attach", s)) => FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Attach {
                    name: s.get_one::<String>("name").cloned().unwrap_or_default(),
                },
            }),
            Some(("ls", s)) => FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Ls {
                    json: JsonOnly {
                        json: s.get_flag("json"),
                    },
                },
            }),
            Some(("doctor", s)) => FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Doctor {
                    json: JsonOnly {
                        json: s.get_flag("json"),
                    },
                },
            }),
            Some(("stats", s)) => FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Stats {
                    json: JsonOnly {
                        json: s.get_flag("json"),
                    },
                },
            }),
            Some(("kill-server", s)) => FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::KillServer {
                    name: s.get_one::<String>("name").cloned(),
                    json: JsonOnly {
                        json: s.get_flag("json"),
                    },
                },
            }),
            Some(("shell-init", s)) => FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::ShellInit {
                    shell: s.get_one::<String>("shell").cloned(),
                    json: JsonOnly {
                        json: s.get_flag("json"),
                    },
                },
            }),
            // clap's auto `help` subcommand and any unrecognized mux verb
            // ride the carry arm; the re-route in main.rs refuses unknown
            // verbs by name and hands the families their byte-exact tails.
            _ => FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Other(args[1..].to_vec()),
            }),
        },
        Some(("version", s)) => FrontDoor::Version {
            json: s.get_flag("json"),
        },
        _ => FrontDoor::Forward,
    }
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
        assert_eq!(classify(&os(&["--server"])), FrontDoor::Usage);
        assert_eq!(
            classify(&os(&["--server", "work", "backlog", "list"])),
            FrontDoor::Usage
        );
        assert_eq!(classify(&os(&["--session"])), FrontDoor::Usage);
        assert_eq!(
            classify(&os(&["--session", "work", "backlog", "list"])),
            FrontDoor::Usage
        );
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
        assert_eq!(classify(&os(&["version", "x"])), FrontDoor::Usage);
        assert_eq!(classify(&os(&["version", "--help"])), FrontDoor::Usage);
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
                    json: JsonOnly { json: true }
                }
            })
        );
        assert_eq!(
            classify(&os(&["mux", "kill-server", "--", "--weird"])),
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::KillServer {
                    name: Some("--weird".into()),
                    json: JsonOnly { json: false }
                }
            })
        );
    }

    #[test]
    fn mux_malformed_shapes_ride_the_carry_arm() {
        // The classify layer hands malformed mux shapes to the carry arm as
        // `Other`; `mux_carry_role` in main.rs refuses each one as
        // MuxUsage (pinned there), so nothing here can silently forward.
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
                FrontDoor::Mux(MuxParsed {
                    cmd: MuxCmd::Other(tail),
                }) => assert_eq!(tail, argv[1..].to_vec(), "tail must stay byte-exact"),
                other => panic!("expected carry Other for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn mux_carry_verbatim_families_survive_as_other() {
        // pane/block/tab/layout/rows/where/thread/view/workspace/serve/web
        // route through Other until their cutover wave; the tail must be
        // byte-exact, including dashes and the `--` delimiter.
        let r = classify(&os(&["mux", "pane", "run", "--cwd", "/x", "--", "claude"]));
        match r {
            FrontDoor::Mux(MuxParsed {
                cmd: MuxCmd::Other(rest),
            }) => {
                let got: Vec<String> = rest
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                assert_eq!(got, ["pane", "run", "--cwd", "/x", "--", "claude"]);
            }
            other => panic!("carry expected, got {other:?}"),
        }
    }
}

/// The common mux verb flags: the server axis plus the machine-output flag
/// every scriptable verb parses (x-861c). One declaration; the deprecation
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

/// `fno mux thread <name>`'s flags (x-07c2/x-9b60): the portal reach and the
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
