//! The `fno mux thread` verb: the outside-the-TUI portal reach. A child
//! module of `mux_cli` on purpose: the file-budget gate keeps the parent
//! shrink-only, and `use super::*` keeps every helper (`take_common_flags`,
//! `resolve_session`, the timeout constants, the exit codes, the proto
//! types, the selector parsers) in exactly one place.
use super::*;

/// `fno mux thread <name> [--portal N] [--tab SEL] [--split DIR]
/// [--workspace NAME] [--at PANE]` (x-07c2, hidden): the outside-the-TUI
/// reach behind `fno agents attach <name>`. Sends the ThreadPane control verb,
/// which runs the exact command a TUI reach runs, and prints where it landed.
/// A missing server is its own exit code so the CLI caller can fall through to
/// the inline attach instead of reading a generic failure as one.
///
/// (x-8f9d) `--portal N` names which portal to reach through; omitted is
/// portal 0. This is the addressing door: two calls naming 0 and 1 put two
/// threads in two panes, which the tab menu's Join actions then tile.
///
/// (x-9b60) The placement flags reuse the pane path's spellings and ride the
/// verb's `placement` field. They steer a FRESH open; a portal that already
/// has a live seat keeps its geometry (the server says so) - same contract
/// the server holds for the TUI.
pub fn thread(args: &[OsString], env_session: Option<&str>) -> i32 {
    let (session_flag, _json, rest) = match take_common_flags(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno mux thread: {e}");
            return EXIT_USAGE;
        }
    };
    // Split `--portal N` and the placement flags out of the positionals
    // before the one-name check, so a flag can sit on either side of the
    // name.
    let mut portal: Option<u8> = None;
    let mut placement = PanePlacement::default();
    let mut positionals: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let text = arg.as_str();
        // Reads the flag's value off the iterator, naming the flag in every
        // refusal.
        macro_rules! flag_value {
            ($flag:literal) => {
                match it.next() {
                    Some(v) => v.as_str().to_string(),
                    None => {
                        eprintln!("fno mux thread: {} needs a value", $flag);
                        return EXIT_USAGE;
                    }
                }
            };
        }
        if text == "--portal" {
            let value = flag_value!("--portal");
            match value.parse::<u8>() {
                Ok(n) => portal = Some(n),
                Err(_) => {
                    eprintln!("fno mux thread: --portal takes an index 0-255");
                    return EXIT_USAGE;
                }
            }
            continue;
        }
        if let Some(value) = text.strip_prefix("--portal=") {
            match value.parse::<u8>() {
                Ok(n) => portal = Some(n),
                Err(_) => {
                    eprintln!("fno mux thread: --portal takes an index 0-255");
                    return EXIT_USAGE;
                }
            }
            continue;
        }
        // (x-9b60) Same spellings the pane placement uses; a new vocabulary
        // for the same concepts is the drift this repo keeps paying for.
        match text {
            "--workspace" | "--squad" | "-s" => {
                let name = flag_value!("--workspace");
                if name.trim().is_empty() {
                    eprintln!("fno mux thread: --workspace/-s needs a nonblank workspace name");
                    return EXIT_USAGE;
                }
                placement.target = PaneTarget::SquadName(name);
            }
            "--split" | "-x" => {
                let value = flag_value!("--split");
                match parse_dir(&value, "split/-x") {
                    Ok(dir) => placement.split = Some(dir),
                    Err(e) => {
                        eprintln!("fno mux thread: {e}");
                        return EXIT_USAGE;
                    }
                }
            }
            "--tab" => {
                let value = flag_value!("--tab");
                match parse_tab_sel(&value) {
                    Ok(sel) => placement.tab = Some(sel),
                    Err(e) => {
                        eprintln!("fno mux thread: {e}");
                        return EXIT_USAGE;
                    }
                }
            }
            "--at" => {
                let value = flag_value!("--at");
                if value == "current" {
                    // `current` resolves a calling pane from FNO_PANE; this
                    // verb's caller is a control client with no pane of its
                    // own.
                    eprintln!(
                        "fno mux thread: --at takes a pane id; there is no calling \
                         pane to resolve `current` from"
                    );
                    return EXIT_USAGE;
                }
                match parse_u64(&value, "--at") {
                    Ok(at) => placement.at = Some(at),
                    Err(e) => {
                        eprintln!("fno mux thread: {e}");
                        return EXIT_USAGE;
                    }
                }
            }
            t if t.starts_with("--") => {
                // A typo'd flag must not read as the agent name.
                eprintln!("fno mux thread: unknown flag: {t}");
                return EXIT_USAGE;
            }
            _ => positionals.push(text.to_string()),
        }
    }
    let Some(name) = positionals
        .first()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        eprintln!("fno mux thread: needs an agent name or attach id");
        return EXIT_USAGE;
    };
    if positionals.len() > 1 {
        eprintln!("fno mux thread: takes exactly one name");
        return EXIT_USAGE;
    }
    // A paneless row owns no session routing: the operator's ambient session
    // (FNO_SESSION / the default) is the server whose portal this drives.
    let session = resolve_session(
        session_flag
            .as_deref()
            .or(env_session)
            .filter(|s| !s.is_empty()),
        None,
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
