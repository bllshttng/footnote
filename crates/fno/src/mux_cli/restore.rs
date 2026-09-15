//! `fno mux workspace restore`: the verb's CLI rendering. Split by
//! kind - `members` are squad members, `portals` are held portal seats - so
//! a refused portal is never hidden behind a member total. Lives beside the
//! verb it renders, out of the shrink-only mux_cli.rs (file budget).

use super::*;

pub(super) fn workspace_restore(args: &[OsString], env_session: Option<&str>) -> i32 {
    let mut dry_run = false;
    let mut json = false;
    let mut harness: Option<String> = None;
    let mut session: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--dry-run") => dry_run = true,
            Some("--json") | Some("-J") => json = true,
            Some("--harness") => {
                harness = Some(match it.next().and_then(|v| v.to_str()) {
                    Some(v) => v.to_string(),
                    None => {
                        eprintln!("fno mux workspace restore: --harness needs a value");
                        return EXIT_USAGE;
                    }
                });
            }
            Some(flag @ ("--server" | "--session")) => {
                note_server_flag(flag);
                session = Some(match it.next().and_then(|v| v.to_str()) {
                    Some(v) => v.to_string(),
                    None => {
                        eprintln!("fno mux workspace restore: {flag} needs a value");
                        return EXIT_USAGE;
                    }
                });
            }
            Some(other) => {
                eprintln!("fno mux workspace restore: unknown argument {other:?}");
                return EXIT_USAGE;
            }
            None => {
                eprintln!("fno mux workspace restore: non-UTF-8 argument");
                return EXIT_USAGE;
            }
        }
    }
    if let Some(h) = harness.as_deref().filter(|h| h.trim().is_empty()) {
        eprintln!("fno mux workspace restore: --harness needs a non-empty value, got {h:?}");
        return EXIT_USAGE;
    }
    let session = resolve_session(session.as_deref(), env_session);
    let sock = match proto::socket_path(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux workspace restore: {e}");
            return EXIT_ERROR;
        }
    };
    let verb = ControlVerb::WorkspaceRestore {
        dry_run,
        harness: harness.clone(),
    };
    match control_roundtrip(&sock, &session, verb) {
        Ok(ServerMsg::WorkspaceRestored { rows }) => {
            // The reply splits by kind: `members` are squad members,
            // `portals` are held portal seats. The counts sum both, so the
            // summary never hides a refused portal behind a member total.
            let (member_rows, portal_rows): (Vec<_>, Vec<_>) =
                rows.into_iter().partition(|r| r.portal.is_none());
            let count = |want: &str| {
                member_rows
                    .iter()
                    .chain(portal_rows.iter())
                    .filter(|r| r.outcome == want)
                    .count()
            };
            if json {
                let payload = serde_json::json!({
                    "session": session,
                    "dry_run": dry_run,
                    "harness": harness,
                    "resumed": count("resumed"),
                    "focused": count("focused"),
                    "refused": count("refused"),
                    "planned": count("planned"),
                    "members": member_rows,
                    "portals": portal_rows,
                });
                println!("{payload}");
            } else {
                for row in &member_rows {
                    match row.outcome.as_str() {
                        "resumed" => println!(
                            "resumed {}{} pane {} squad {}{}",
                            row.member,
                            row.harness
                                .as_deref()
                                .map(|h| format!(" ({h})"))
                                .unwrap_or_default(),
                            row.pane.map(|p| p.to_string()).unwrap_or_default(),
                            row.squad,
                            row.notice
                                .as_deref()
                                .map(|n| format!(" - {n}"))
                                .unwrap_or_default(),
                        ),
                        "focused" => println!(
                            "focused {} pane {} squad {}",
                            row.member,
                            row.pane.map(|p| p.to_string()).unwrap_or_default(),
                            row.squad,
                        ),
                        "planned" => println!(
                            "planned {}{}",
                            row.member,
                            row.harness
                                .as_deref()
                                .map(|h| format!(" ({h})"))
                                .unwrap_or_default(),
                        ),
                        _ => println!(
                            "refused {}: {}",
                            row.member,
                            row.reason.as_deref().unwrap_or("no reason given"),
                        ),
                    }
                }
                for row in &portal_rows {
                    let key = row.member.as_str();
                    match row.outcome.as_str() {
                        "focused" => println!(
                            "focused portal {} {key} pane {}",
                            row.portal.map(|p| p.to_string()).unwrap_or_default(),
                            row.pane.map(|p| p.to_string()).unwrap_or_default(),
                        ),
                        "planned" => println!("planned portal {} {key}", row.portal.unwrap_or(0)),
                        "resumed" => println!(
                            "resumed portal {} {key} pane {}{}",
                            row.portal.map(|p| p.to_string()).unwrap_or_default(),
                            row.pane.map(|p| p.to_string()).unwrap_or_default(),
                            row.notice
                                .as_deref()
                                .map(|n| format!(" - {n}"))
                                .unwrap_or_default(),
                        ),
                        _ => println!(
                            "refused portal {} {key}: {}",
                            row.portal.unwrap_or(0),
                            row.reason.as_deref().unwrap_or("no reason given"),
                        ),
                    }
                }
                println!(
                    "restore: {} resumed, {} focused, {} refused, {} planned{}",
                    count("resumed"),
                    count("focused"),
                    count("refused"),
                    count("planned"),
                    if dry_run {
                        " (dry run: nothing started)"
                    } else {
                        ""
                    },
                );
            }
            EXIT_OK
        }
        Ok(ServerMsg::Err { msg, .. }) => {
            eprintln!("fno mux workspace restore: {msg}");
            EXIT_ERROR
        }
        Ok(other) => {
            eprintln!("fno mux workspace restore: unexpected reply: {other:?}");
            EXIT_ERROR
        }
        Err(e) => {
            eprintln!("fno mux workspace restore: {e}");
            match e {
                ControlError::Unanswered(_) => EXIT_CONTROL_UNANSWERED,
                _ => EXIT_ERROR,
            }
        }
    }
}
