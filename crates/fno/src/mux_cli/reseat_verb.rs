//! The `fno mux thread reseat` verb: move a live pane-hosted worker into a
//! portal seat, keeping its PTY. A child module of `mux_cli` on purpose: the
//! file-budget gate keeps the parent shrink-only, and `use super::*` keeps
//! every helper (take_common_flags, resolve_session, the timeout constants,
//! the exit codes, the proto types) in exactly one place.
use super::*;

<<<<<<< HEAD
/// `fno mux thread reseat <pane-id> [--portal N] [--session S]` (v70): the
=======
/// `fno mux thread reseat <pane-id> [--portal N] [--session S]` (v69): the
>>>>>>> 7ca6b6cf6 (feat(mux): fno mux thread reseat verb + ThreadReseat wire (proto v69))
/// outside-the-TUI door of the re-seat move. Sends the ThreadReseat control
/// verb, which detaches the live leaf keeping the PTY, seats the same pane in
/// a portal, and de-recruits its squad membership; the registry `mux` flip is
/// the CALLER's half (the Python `fno agents reseat` front door drives this
/// verb and writes the registry on the receipt). Idempotent: an already-seated
/// pane is answered, not moved twice.
///
/// The trade, named where the operator meets it: after the move the row is no
/// longer rebuilt by `mux workspace restore`, and `fno agents rm` removes the
/// row without killing the pane (thread semantics). The work survives; the
/// geometry does not.
pub fn reseat(args: &[OsString], env_session: Option<&str>) -> i32 {
    let (session_flag, _json, rest) = match take_common_flags(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno mux thread reseat: {e}");
            return EXIT_USAGE;
        }
    };
    let mut portal: Option<u8> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let text = arg.as_str();
        if text == "--portal" {
            let value = match it.next() {
                Some(v) => v.as_str().to_string(),
                None => {
                    eprintln!("fno mux thread reseat: --portal needs a value");
                    return EXIT_USAGE;
                }
            };
            match value.parse::<u8>() {
                Ok(n) => portal = Some(n),
                Err(_) => {
                    eprintln!("fno mux thread reseat: --portal takes an index 0-255");
                    return EXIT_USAGE;
                }
            }
            continue;
        }
        if let Some(value) = text.strip_prefix("--portal=") {
            match value.parse::<u8>() {
                Ok(n) => portal = Some(n),
                Err(_) => {
                    eprintln!("fno mux thread reseat: --portal takes an index 0-255");
                    return EXIT_USAGE;
                }
            }
            continue;
        }
        if text.starts_with("--") {
            eprintln!("fno mux thread reseat: unknown flag: {text}");
            return EXIT_USAGE;
        }
        positionals.push(text.to_string());
    }
    if positionals.len() != 1 {
        eprintln!("fno mux thread reseat: takes exactly one pane id");
        return EXIT_USAGE;
    }
    let pane = match positionals[0].parse::<u64>() {
        Ok(n) => n,
        Err(_) => {
            eprintln!(
                "fno mux thread reseat: the pane id is a number, got {:?}",
                positionals[0]
            );
            return EXIT_USAGE;
        }
    };
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
            eprintln!("fno mux thread reseat: {e}");
            return EXIT_USAGE;
        }
    };
    let stream = match proto::connect_unix_timeout(&sock, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fno mux thread reseat: no live mux server ({e})");
            return EXIT_NO_SERVER;
        }
    };
    match send_control(
        stream,
        ControlVerb::ThreadReseat { pane, portal },
        CONTROL_TIMEOUT,
        CONTROL_REPLY_DEADLINE,
        &session,
    ) {
        Ok(ServerMsg::Notice { text }) => {
            println!("{text}");
            EXIT_OK
        }
        Ok(ServerMsg::Err { msg, .. }) => {
            eprintln!("fno mux thread reseat: {msg}");
            EXIT_ERROR
        }
        Ok(other) => {
            eprintln!("fno mux thread reseat: unexpected reply: {other:?}");
            EXIT_ERROR
        }
        Err(ControlError::Unanswered(e)) => {
            eprintln!("fno mux thread reseat: {e}");
            EXIT_CONTROL_UNANSWERED
        }
        Err(e) => {
            eprintln!("fno mux thread reseat: {e}");
            EXIT_ERROR
        }
    }
}
