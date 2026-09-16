//! The `fno mux thread` verb: the outside-the-TUI portal reach. A child
//! module of `mux_cli` on purpose: the file-budget gate keeps the parent
//! shrink-only, and `use super::*` keeps every helper (`take_common_flags`,
//! `resolve_session`, the timeout constants, the exit codes, the proto
//! types, the selector parsers) in exactly one place.
use super::*;

/// `fno mux thread <name> [--portal N|new] [--tab SEL] [--split DIR]
/// [--workspace NAME] [--at PANE]` (hidden): the outside-the-TUI
/// reach behind `fno agents attach <name>`. Sends the ThreadPane control verb,
/// which runs the exact command a TUI reach runs, and prints where it landed.
/// A missing server is its own exit code so the CLI caller can fall through to
/// the inline attach instead of reading a generic failure as one.
///
/// `--portal N` names which portal to reach through; omitted is
/// portal 0. This is the addressing door: two calls naming 0 and 1 put two
/// threads in two panes, which the tab menu's Join actions then tile.
///
/// `--portal new` asks the server for a portal of its own in a new
/// tab: a MACHINE reach (retask, mail force) must never repoint a seat a
/// person is using, and portal 0 is usually the operator's own.
///
/// The placement flags reuse the pane path's spellings and ride the
/// verb's `placement` field. They steer a FRESH open; a portal that already
/// has a live seat keeps its geometry (the server says so) - same contract
/// the server holds for the TUI.
pub fn thread(args: &[OsString], env_session: Option<&str>) -> i32 {
    let (session_flag, parsed) = match parse_thread_args(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let mut placement = PanePlacement::default();
    let mut portal: Option<u8> = None;
    if let Some(value) = &parsed.portal {
        // if/else, not a match: an inner `"word" =>` arm reads as a phantom
        // verb to the Python parity parser's arm scan.
        let is_new = value == "new";
        if let Ok(n) = value.parse::<u8>() {
            portal = Some(n);
            // Last --portal flag wins, so an explicit index clears a `new`
            // spelled earlier.
            placement.portal_new = false;
        } else if is_new {
            placement.portal_new = true;
            portal = None;
        } else {
            eprintln!("fno mux thread: --portal takes an index 0-255 or new");
            return EXIT_USAGE;
        }
    }
    if let Some(ws) = &parsed.workspace {
        if ws.trim().is_empty() {
            eprintln!("fno mux thread: --workspace/-s needs a nonblank workspace name");
            return EXIT_USAGE;
        }
        placement.target = PaneTarget::SquadName(ws.clone());
    }
    if let Some(v) = &parsed.split {
        match parse_dir(v, "split/-x") {
            Ok(dir) => placement.split = Some(dir),
            Err(e) => {
                eprintln!("fno mux thread: {e}");
                return EXIT_USAGE;
            }
        }
    }
    if let Some(v) = &parsed.tab {
        match parse_tab_sel(v) {
            Ok(sel) => placement.tab = Some(sel),
            Err(e) => {
                eprintln!("fno mux thread: {e}");
                return EXIT_USAGE;
            }
        }
    }
    if let Some(v) = &parsed.at {
        if v == "current" {
            // `current` resolves a calling pane from FNO_PANE; this verb's
            // caller is a control client with no pane of its own.
            eprintln!(
                "fno mux thread: --at takes a pane id; there is no calling \
                 pane to resolve `current` from"
            );
            return EXIT_USAGE;
        }
        match parse_u64(v, "--at") {
            Ok(at) => placement.at = Some(at),
            Err(e) => {
                eprintln!("fno mux thread: {e}");
                return EXIT_USAGE;
            }
        }
    }
    let Some(name) = parsed
        .name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        eprintln!("fno mux thread: needs an agent name or attach id");
        return EXIT_USAGE;
    };
    // A paneless row owns no session routing: the operator's ambient server
    // (the flag, FNO_SERVER / FNO_SESSION, or the default) is the one whose
    // portal this drives. Flag and env stay separate so resolve_session can
    // tell an env-decided server from a flag-decided one.
    let session = resolve_session(
        session_flag.as_deref().filter(|s| !s.is_empty()),
        env_session,
    );
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux thread: {e}");
            return EXIT_USAGE;
        }
    };
    let stream = match proto::connect_unix_timeout(&sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fno mux thread: no live mux server ({e})");
            return EXIT_NO_SERVER;
        }
    };
    match send_control(
        stream,
        ControlVerb::ThreadPane {
            name,
            portal,
            placement,
        },
        CONTROL_TIMEOUT,
        CONTROL_REPLY_DEADLINE,
        &session,
    ) {
        Ok(ServerMsg::Notice { text }) => {
            println!("{text}");
            EXIT_OK
        }
        Ok(ServerMsg::Err { msg, .. }) => {
            eprintln!("fno mux thread: {msg}");
            EXIT_ERROR
        }
        Ok(other) => {
            eprintln!("fno mux thread: unexpected reply: {other:?}");
            EXIT_ERROR
        }
        Err(ControlError::Unanswered(e)) => {
            eprintln!("fno mux thread: {e}");
            EXIT_CONTROL_UNANSWERED
        }
        Err(e) => {
            eprintln!("fno mux thread: {e}");
            EXIT_ERROR
        }
    }
}

/// The parse `thread` runs: the shared common flags ride [`MuxCommon::take`],
/// and the verb's own grammar parses the REMAINDER. Parsing the original argv
/// instead would refuse `--server`/`--session`/`--json` - tokens ThreadArgs
/// does not declare - so the server-axis override every verb shares would die
/// on this verb. The returned error is the line the caller prints.
fn parse_thread_args(
    args: &[OsString],
) -> Result<(Option<String>, crate::cli_args::ThreadArgs), String> {
    let (common, rest) = MuxCommon::take(args).map_err(|e| format!("fno mux thread: {e}"))?;
    let parsed = crate::cli_args::ThreadArgs::try_parse_from(&rest)
        .map_err(|e| crate::cli_args::refusal_line("fno mux thread", &e))?;
    Ok((common.server.or(common.session), parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn thread_takes_the_shared_server_flags() {
        // The common flags are take()'s and the verb grammar parses the
        // remainder; parsing the original argv instead refused every
        // --server/--session/--json invocation (they are not ThreadArgs').
        let (session_flag, parsed) =
            parse_thread_args(&os(&["--server", "work", "--json", "myagent"]))
                .expect("common flags parse");
        assert_eq!(session_flag.as_deref(), Some("work"));
        assert_eq!(parsed.name.as_deref(), Some("myagent"));
        let (session_flag, parsed) =
            parse_thread_args(&os(&["--session", "legacy", "myagent"])).expect("alias parses");
        assert_eq!(session_flag.as_deref(), Some("legacy"));
        assert_eq!(parsed.name.as_deref(), Some("myagent"));
    }

    #[test]
    fn thread_still_refuses_its_own_unknown_flags() {
        let err = parse_thread_args(&os(&["--wat", "myagent"])).expect_err("unknown flag refuses");
        assert!(err.contains("--wat"), "{err}");
    }
}
