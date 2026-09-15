//! The pane argv grammar (`fno mux pane <verb> ...`): one parser, [`parse_pane_args`],
//! for every pane verb including `run`'s embedded command argv. A child
//! module of `mux_cli` on purpose: the file-budget gate keeps the parent
//! shrink-only, and `use super::*` keeps the proto types, the selector
//! parsers, and the shared `MuxCommon` group in one place.
use super::*;

/// The result of parsing the tokens after `mux pane`. Public for the same
/// reason as [`PaneCmd`]: the daemon-side refusal round-trip test reads it.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedPane {
    pub session: Option<String>,
    pub json: bool,
    pub cmd: PaneCmd,
}

pub const PANE_VERBS: &str = "ls|read|run|send|wait|kill|claim|release|split|break|focus|keeper";
pub const PANE_REFERENCE_USAGE: &str =
    "pane refs are <pane-id> or <session>:<pane-id>; --session overrides the prefix";

/// `pane send`'s one non-obvious flag, stated where an operator looks for it
/// (node x-3a64). The default is the surprising half, so name it first and show
/// the raw case with a real payload rather than a placeholder: an envelope
/// around the character `1` is the nonsense the flag exists to avoid.
pub const PANE_SEND_RAW_HELP: &str = "pane send wraps the text in an <fno_mail> envelope by \
default, so a worker can tell a peer's message from its operator's, and refuses a pane showing \
an option prompt. The enveloped body passes the same style gate as mail; \
--style-exception <reason> excepts one reasoned send. --raw types the bytes verbatim for \
genuine keystrokes: `fno mux pane send 45 --text 1 --raw --submit` answers a prompt with a digit. \
Every send writes one audit row to ~/.fno/agents/events.jsonl naming the pane, the recipient and \
the caller; --source <label> declares that provenance (the mail lane declares mail; a bare \
invocation reads unattributed:<pid>).";

/// `pane run --worker`'s one line, same posture as [`PANE_SEND_RAW_HELP`]: the
/// flag records the pane as a squad member joined to the registry row by name,
/// so a non-claude worker survives a mux restart as an idle, resumable row
/// instead of vanishing (x-5f7f). Without it nothing is recorded - a plain
/// `pane run` stays byte-identical.
pub const PANE_RUN_WORKER_HELP: &str = "pane run --worker <registry-name> records the pane as a \
squad member joined to that registry row: after a mux restart the member stays as an idle row in \
the agent panel, and selecting it resumes the session through its own harness. A keeper-hosted \
worker pane outlives the server outright and a fresh server re-adopts it in place (`fno mux pane \
keeper list` reads them); startup restore holds (default) or idles it by policy; `fno mux \
workspace restore` respawns it on demand. A run without --worker records no member.";

/// (x-b029) What the `fno_id` column answers, stated where the listing is
/// read: identity, never idleness or reusability. The dash is reserved for
/// panes with no fno evidence at all; `unresolved:<reason>` marks an fno pane
/// whose session id never resolved, so it can never share the dash.
pub const PANE_LS_IDENTITY_HELP: &str = "pane ls fno_id answers identity, not idleness: `-` means \
no fno evidence at all; `unresolved:spawned-name` / `unresolved:name-as-id` mean an fno pane whose \
session id never resolved. Whether a pane is idle or reusable is `fno mux pane wait --quiet-ms <n>` \
(or the `pristine_idle_shell` field in --json), never this column.";

pub fn parse_pane_args(args: &[OsString]) -> Result<ParsedPane, String> {
    let verb = args
        .first()
        .and_then(|a| a.to_str())
        .ok_or_else(|| format!("pane needs a verb: {PANE_VERBS}"))?;
    if matches!(verb, "-h" | "--help") {
        return Err(format!(
            "{PANE_REFERENCE_USAGE}; verbs: {PANE_VERBS}\n{PANE_SEND_RAW_HELP}\n{PANE_RUN_WORKER_HELP}\n{PANE_LS_IDENTITY_HELP}"
        ));
    }

    // Hidden verb subtree: `pane keeper list` reads the keeper sockets
    // directly (no server), so it parses here and dispatches before any
    // session resolution.
    if verb == "keeper" {
        let sub = args
            .get(1)
            .and_then(|a| a.to_str())
            .ok_or_else(|| "pane keeper needs a verb: list".to_string())?;
        if sub != "list" {
            return Err(format!("unknown pane keeper verb: {sub} (expected list)"));
        }
        let mut json = false;
        let mut stale_after = None;
        let mut i = 2;
        while i < args.len() {
            let tok = args[i]
                .to_str()
                .ok_or_else(|| "non-UTF-8 argument".to_string())?;
            match tok {
                "--json" => json = true,
                "--stale-after" => {
                    let Some(value) = args.get(i + 1).and_then(|a| a.to_str()) else {
                        return Err("--stale-after needs a value".into());
                    };
                    stale_after = Some(parse_duration(value)?);
                    i += 1;
                }
                other => return Err(format!("unknown flag: {other}")),
            }
            i += 1;
        }
        return Ok(ParsedPane {
            session: None,
            json,
            cmd: PaneCmd::KeeperList { json, stale_after },
        });
    }

    // `run` is special: leading options/directives, then the command argv
    // verbatim (its own flags are NOT ours to parse), optionally after `--`.
    if verb == "run" {
        let mut cwd = None;
        let mut claim = false;
        let mut worker = None;
        let mut squad = None;
        let mut split = None;
        let mut tab = None;
        let mut at = None;
        let mut at_current = false;
        let mut max_panes = None;
        let mut fit = false;
        // The common grammar (--server/--session/--json) is MuxCommon's; the
        // loop keeps run's own shape: leading flags, then the command argv
        // verbatim from the first bare token (or `--`).
        let (common, rest) = MuxCommon::take(args).map_err(|e| format!("pane run: {e}"))?;
        let session = common.server.or(common.session);
        let json = common.json;
        let mut i = 1;
        while i < rest.len() {
            let tok = rest[i].as_str();
            match tok {
                "-h" | "--help" => {
                    return Err(format!(
                        "{PANE_REFERENCE_USAGE}; verbs: {PANE_VERBS}\n{PANE_SEND_RAW_HELP}\n{PANE_RUN_WORKER_HELP}\n{PANE_LS_IDENTITY_HELP}"
                    ))
                }
                "--" => {
                    i += 1;
                    break;
                }
                "--claim" => claim = true,
                "--fit" => fit = true,
                "--cwd" => {
                    let Some(v) = rest.get(i + 1) else {
                        return Err("--cwd needs a value".into());
                    };
                    cwd = Some(v.clone());
                    i += 1;
                }
                // (x-5f7f) The registry name of the worker this pane hosts.
                // Validated here with the same rule the store's load gate
                // holds, so an unrecordable name refuses before any pane
                // exists rather than landing in the store and being dropped
                // at the next load.
                "--worker" => {
                    let Some(name) = rest.get(i + 1).cloned() else {
                        return Err("--worker needs a value".into());
                    };
                    if !crate::squad_store::valid_worker_name(&name) {
                        return Err(
                            "--worker needs a registry name ([A-Za-z0-9._-], <=64 chars)"
                                .into(),
                        );
                    }
                    worker = Some(name);
                    i += 1;
                }
                "--workspace" | "--squad" | "-s" | "workspace" | "squad" => {
                    let Some(name) = rest.get(i + 1).cloned() else {
                        return Err(format!("{tok} needs a value"));
                    };
                    if name.trim().is_empty() {
                        return Err("--workspace/-s needs a nonblank workspace name".into());
                    }
                    squad = Some(name);
                    i += 1;
                }
                "--split" | "-x" | "split" => {
                    let Some(v) = rest.get(i + 1) else {
                        return Err(format!("{tok} needs a value"));
                    };
                    split = Some(parse_dir(v, "split/-x")?);
                    i += 1;
                }
                // (x-d865) exact placement: land in a named tab, adjacent to an
                // anchor pane.
                "--tab" => {
                    let Some(v) = rest.get(i + 1) else {
                        return Err("--tab needs a value".into());
                    };
                    tab = Some(parse_tab_sel(v)?);
                    i += 1;
                }
                // Bare "at" mirrors bare "split" above: mux_spawn.py's
                // placement_args sends directives unprefixed (x-d865).
                "--at" | "at" => {
                    let Some(v) = rest.get(i + 1) else {
                        return Err("--at needs a value".into());
                    };
                    if v == "current" {
                        at_current = true;
                    } else {
                        at = Some(parse_u64(v, "--at")?);
                    }
                    i += 1;
                }
                "--max-panes" => {
                    let Some(value) = rest.get(i + 1) else {
                        return Err("--max-panes needs a value".into());
                    };
                    let parsed = value.parse::<usize>().map_err(|_| {
                        format!("--max-panes needs a positive integer, got {value:?}")
                    })?;
                    if parsed == 0 {
                        return Err("--max-panes needs a positive integer, got 0".into());
                    }
                    max_panes = Some(parsed);
                    i += 1;
                }
                t if t.starts_with("--") => return Err(format!("unknown flag: {t}")),
                _ => break, // first bare token begins the command argv
            }
            i += 1;
        }
        let argv = rest[i..].to_vec();
        if argv.is_empty() {
            return Err("pane run needs a command".to_string());
        }
        let mut fallback = PlacementFallback::NewTab;
        if at_current {
            // `--at current` resolves the calling pane from FNO_PANE here in the
            // one parser both high- and low-level callers route through, and opts
            // into strict placement (Refuse). FNO_SESSION selects the server via
            // resolve_session; a missing/invalid FNO_PANE is a usage error before
            // any process is spawned (AC1-ERR).
            if split.is_none() {
                return Err("--at current requires --split".to_string());
            }
            let fno_pane = std::env::var("FNO_PANE")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| {
                    "--at current requires a numeric FNO_PANE (run it inside a mux pane)"
                        .to_string()
                })?;
            at = Some(fno_pane);
            fallback = PlacementFallback::Refuse;
        }
        let placement = PanePlacement {
            target: squad
                .map(PaneTarget::SquadName)
                .unwrap_or(PaneTarget::CurrentRoute),
            split,
            tab,
            at,
            fallback,
            max_panes,
            fit,
            ..Default::default()
        };
        if let Some((_, msg)) = crate::server::placement_fit::refuse_fit_with_geometry(&placement) {
            return Err(msg);
        }
        return Ok(ParsedPane {
            session,
            json,
            cmd: PaneCmd::Run {
                cwd,
                argv,
                claim,
                worker,
                placement,
            },
        });
    }

    // Every other verb: a single flag/positional pass (no embedded argv).
    // The common grammar rides MuxCommon::take; this pass keeps the verbs'
    // own flags and positionals.
    let (common, verb_args) = MuxCommon::take(args)?;
    let mut session = common.server.or(common.session);
    let json = common.json;
    let mut lines = None;
    let mut text = None;
    let mut stdin = false;
    let mut guarded = false;
    let mut submit = false;
    let mut raw = false;
    let mut style_exception: Option<String> = None;
    let mut provenance: Option<String> = None;
    let mut quiet_ms = None;
    let mut pattern = None;
    let mut timeout_s = None;
    let mut pid = None;
    let mut block = None;
    let mut command_done = false;
    let mut direction = None;
    let mut focus = false;
    let mut fzf = false;
    let mut name = None;
    let mut fno_id = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 1;
    while i < verb_args.len() {
        let tok = verb_args[i].as_str();
        // One value read for a value-carrying flag; names the flag in the
        // refusal, the same voice the shared group uses.
        macro_rules! value_of {
            () => {
                match verb_args.get(i + 1) {
                    Some(v) => {
                        i += 1;
                        v.clone()
                    }
                    None => return Err(format!("{tok} needs a value")),
                }
            };
        }
        match tok {
            // (x-d865) split/break/ls flags.
            "--direction" | "-d" => {
                let v = value_of!();
                direction = Some(parse_dir(&v, tok)?);
            }
            "--focus" => focus = true,
            // (x-b80d) focus-only: open the interactive pane picker.
            "--fzf" => fzf = true,
            "--name" => name = Some(value_of!()),
            "--fno-id" => fno_id = Some(value_of!()),
            "--pid" => {
                let v = value_of!();
                pid = Some(parse_u64(&v, "--pid")? as u32);
            }
            "--lines" => {
                let v = value_of!();
                lines = Some(parse_u64(&v, "--lines")? as u16);
            }
            "--block" => {
                let v = value_of!();
                block = Some(parse_block_sel(&v)?);
            }
            "--command-done" => command_done = true,
            "--text" => text = Some(value_of!()),
            "--stdin" => stdin = true,
            "--guarded" => guarded = true,
            "--submit" => submit = true,
            "--raw" => raw = true,
            "--style-exception" => style_exception = Some(value_of!()),
            "--source" => provenance = Some(value_of!()),
            "--quiet-ms" => {
                let v = value_of!();
                quiet_ms = Some(parse_u64(&v, "--quiet-ms")?);
            }
            "--pattern" => pattern = Some(value_of!()),
            "--timeout" => {
                let v = value_of!();
                timeout_s = Some(parse_u64(&v, "--timeout")?);
            }
            t if t.starts_with("--") => return Err(format!("unknown flag: {t}")),
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    // A pane positional is a bare pane id (`76`) or a `<session>:<pane>`
    // selector (`main:76`) - the exact form the daemon's pane-worker
    // refusals print and the agents JSON contract documents as the kill
    // remedy. An explicit `--session` flag wins over the selector's
    // session, the same explicit-flag-first order `resolve_session`
    // applies to FNO_SESSION.
    let mut selector_session: Option<String> = None;
    let mut pane_arg = |what: &str| -> Result<u64, String> {
        let raw = positionals
            .first()
            .ok_or_else(|| format!("pane {what} needs a pane id"))?;
        let (session, pane) = parse_pane_ref(raw)?;
        if selector_session.is_none() {
            selector_session = session;
        }
        Ok(pane)
    };

    // Before the match: the Send arm below MOVES `style_exception`, and a
    // post-match validation would not compile. Same refusal shape as the
    // `--raw` check, which sits after only because bool is Copy.
    if style_exception.is_some() && verb != "send" {
        return Err("--style-exception pairs only with pane send".into());
    }
    if provenance.is_some() && verb != "send" {
        return Err("--source pairs only with pane send".into());
    }
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
        "focus" => {
            if fzf && !positionals.is_empty() {
                return Err("--fzf takes no pane id or selector".into());
            }
            let (target, session) = if fzf {
                (FocusTarget::Pick, None)
            } else {
                parse_focus_target(positionals.first())?
            };
            if selector_session.is_none() {
                selector_session = session;
            }
            PaneCmd::Focus { target }
        }
        "send" => {
            let pane = pane_arg("send")?;
            let source = match (text, stdin) {
                (Some(_), true) => return Err("pane send takes --text OR --stdin, not both".into()),
                (Some(t), false) => SendSource::Text(t),
                (None, true) => SendSource::Stdin,
                // The bare-submit keystroke the attribution refusal names as
                // `--raw`: an omitted payload is an empty text ONLY when both
                // flags are present. Every other source-less form keeps the
                // arity error, because it expresses no operation.
                (None, false) if raw && submit => SendSource::Text(String::new()),
                (None, false) => return Err("pane send needs --text <s> or --stdin".into()),
            };
            PaneCmd::Send {
                pane,
                source,
                guarded,
                submit,
                raw,
                expected_identity: fno_id,
                style_exception,
                provenance,
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
        other => return Err(format!("unknown pane verb: {other} ({PANE_VERBS})")),
    };
    if fzf && verb != "focus" {
        return Err("--fzf pairs only with pane focus".into());
    }
    if raw && verb != "send" {
        return Err("--raw pairs only with pane send".into());
    }
    if session.is_none() {
        session = selector_session;
    }
    Ok(ParsedPane { session, json, cmd })
}
