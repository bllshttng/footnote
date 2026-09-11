//! The `fno mux retire-session` verb (v75, x-7649): the thin transport for
//! the exact-session retirement. A child module of `mux_cli`: the file
//! budget gate keeps the parent shrink-only, and `use super::*` keeps every
//! helper in one place.

use super::*;

pub fn retire_session(args: &[OsString], env_session: Option<&str>) -> i32 {
    let _ = env_session;
    let (session_flag, json, rest) = match take_common_flags(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno mux retire-session: {e}");
            return EXIT_USAGE;
        }
    };
    // The host session is the first positional; --server stays the override
    // spelling every verb shares.
    let mut parsed = rest.iter();
    let host_session = match parsed
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(s) => s,
        None => {
            eprintln!("fno mux retire-session: needs a session name");
            eprintln!("usage: fno mux retire-session <session> --harness <name> --session-id <id> [--json]");
            return EXIT_USAGE;
        }
    };
    let mut harness: Option<String> = None;
    let mut session_id: Option<String> = None;
    while let Some(arg) = parsed.next() {
        match arg.as_str() {
            "--harness" => match parsed.next() {
                Some(v) => harness = Some(v.clone()),
                None => {
                    eprintln!("fno mux retire-session: --harness needs a value");
                    return EXIT_USAGE;
                }
            },
            "--session-id" => match parsed.next() {
                Some(v) => session_id = Some(v.clone()),
                None => {
                    eprintln!("fno mux retire-session: --session-id needs a value");
                    return EXIT_USAGE;
                }
            },
            other => {
                eprintln!("fno mux retire-session: unexpected argument {other:?}");
                return EXIT_USAGE;
            }
        }
    }
    let harness = match harness {
        Some(h) => h,
        None => {
            eprintln!("fno mux retire-session: --harness is required");
            return EXIT_USAGE;
        }
    };
    let session_id = match session_id {
        Some(s) => s,
        None => {
            eprintln!("fno mux retire-session: --session-id is required");
            return EXIT_USAGE;
        }
    };
    let host_session = match session_flag.as_deref() {
        Some(flag) => flag.to_string(),
        None => host_session,
    };
    let sock = match proto::socket_path(&host_session) {
        Ok(sock) => sock,
        Err(e) => {
            eprintln!("fno mux retire-session: {e}");
            return EXIT_USAGE;
        }
    };
    let reply = control_roundtrip(
        &sock,
        &host_session,
        ControlVerb::RetireSession {
            harness,
            session_id,
        },
    );
    match reply {
        Ok(ServerMsg::SessionRetired {
            retired,
            panes_closed,
            closed_panes,
            tabs_removed,
        }) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "session": host_session,
                        "retired": retired,
                        "panes_closed": panes_closed,
                        "closed_panes": closed_panes,
                        "tabs_removed": tabs_removed,
                    })
                );
            } else {
                let names = if closed_panes.is_empty() {
                    String::new()
                } else {
                    format!(": {}", closed_panes.join(", "))
                };
                let tabs = if tabs_removed.is_empty() {
                    String::new()
                } else {
                    format!("; removed empty tab(s): {}", tabs_removed.join(", "))
                };
                println!(
                    "retire-session {host_session}: retired {retired} member(s), closed {panes_closed} pane(s){names}{tabs}"
                );
            }
            EXIT_OK
        }
        Ok(_) => {
            eprintln!("fno mux retire-session: unexpected reply shape (build skew?)");
            EXIT_ERROR
        }
        Err(ControlError::Unanswered(e)) => {
            eprintln!("fno mux retire-session: {e}");
            EXIT_CONTROL_UNANSWERED
        }
        Err(ControlError::Fatal(e)) => {
            eprintln!("fno mux retire-session: {e}");
            EXIT_ERROR
        }
        Err(ControlError::FatalCode { msg, .. }) => {
            eprintln!("fno mux retire-session: {msg}");
            EXIT_ERROR
        }
    }
}
