//! `fno-agents pane-rebind`: the mux workspace restore's registry rebind
//! door. Transport-only (the shrink law allows no new client action): the
//! mux server's restore walk is the only caller, and it shells this verb
//! OFF its core loop after a resume spawn so the row it just re-seated
//! names the new pane, carries the new pid, and reads live in the same
//! step its pane starts. Before this door a restored codex lead came back
//! with its context while its row stayed orphaned on the dead pane, so
//! mail, pane send, and `fno agents resume` all refused it.
//!
//! The caller owns the second-writer decision (its resume walk refuses a
//! live pane before spawning); this verb only joins the row.

use crate::paths::AgentsHome;
use crate::state::{self, MuxRef, RegistryEntry};
use crate::AgentStatus;

/// Usage: `pane-rebind --harness <h> --session <native-sid> --mux-session
/// <mux-session> --pane <id> --pid <child-pid> [--json]`. Exit 0 prints the
/// JSON receipt; exit 3 refuses (no matching row, or more than one) with the
/// reason on stderr - the reentry-refused code, the family this door joins.
pub fn run(args: &[String]) -> i32 {
    let home = AgentsHome::from_env();
    match rebind(args, &home) {
        Ok(receipt) => {
            println!("{receipt}");
            0
        }
        Err((code, reason)) => {
            eprintln!("pane-rebind: {reason}");
            code
        }
    }
}

struct RebindArgs<'a> {
    harness: &'a str,
    session: &'a str,
    mux_session: &'a str,
    pane: u64,
    pid: u32,
}

fn parse_args(args: &[String]) -> Result<RebindArgs<'_>, (i32, String)> {
    let mut harness = None;
    let mut session = None;
    let mut mux_session = None;
    let mut pane = None;
    let mut pid = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .map(String::as_str)
                .ok_or_else(|| (2, format!("--{name} needs a value")))
        };
        match a.as_str() {
            "--harness" => harness = Some(value("harness")?),
            "--session" => session = Some(value("session")?),
            "--mux-session" => mux_session = Some(value("mux-session")?),
            "--pane" => {
                pane = Some(
                    value("pane")?
                        .parse::<u64>()
                        .map_err(|_| (2, "--pane needs a number".to_string()))?,
                )
            }
            "--pid" => {
                pid = Some(
                    value("pid")?
                        .parse::<u32>()
                        .map_err(|_| (2, "--pid needs a number".to_string()))?,
                )
            }
            "--json" | "-J" => {}
            other => return Err((2, format!("unknown argument {other:?}"))),
        }
    }
    let (harness, session, mux_session, pane, pid) =
        match (harness, session, mux_session, pane, pid) {
            (Some(h), Some(s), Some(m), Some(p), Some(pid))
                if !h.trim().is_empty()
                    && !s.trim().is_empty()
                    && !m.trim().is_empty()
                    && p > 0
                    && pid > 1 =>
            {
                (h, s, m, p, pid)
            }
            _ => {
                return Err((
                    2,
                    "needs --harness, --session, --mux-session, --pane > 0 and --pid > 1"
                        .to_string(),
                ))
            }
        };
    Ok(RebindArgs {
        harness,
        session,
        mux_session,
        pane,
        pid,
    })
}

/// The pure half, on an injected home. The receipt line is the verb's own
/// report; the caller quotes it into the restore row's notice.
fn rebind(args: &[String], home: &AgentsHome) -> Result<String, (i32, String)> {
    let parsed = parse_args(args)?;
    let name = state::update_registry(&home.registry_json(), |registry| {
        let matches: Vec<&mut RegistryEntry> = registry
            .entries
            .iter_mut()
            .filter(|e| {
                e.harness_name() == parsed.harness
                    && e.harness_session_id.as_deref() == Some(parsed.session)
            })
            .collect();
        match matches.len() {
            0 => {
                return Err(format!(
                    "no row carries harness {} session {}",
                    parsed.harness, parsed.session
                ))
            }
            1 => {}
            _ => {
                return Err(format!(
                    "{} rows carry harness {} session {}; rebind by exact name instead",
                    matches.len(),
                    parsed.harness,
                    parsed.session
                ))
            }
        }
        let entry = matches.into_iter().next().expect("exactly one");
        entry.mux = Some(MuxRef {
            session: parsed.mux_session.to_string(),
            pane_id: parsed.pane,
        });
        entry.pid = Some(parsed.pid);
        // A recycled pid cannot pass `pid_is_ours`; recording the start time
        // now is what makes the next sweep's pid probe honest.
        entry.pid_start_time = crate::daemon::process_start_time(parsed.pid);
        entry.status = AgentStatus::Live;
        Ok(entry.name.clone())
    })
    .map_err(|e| (3, e.to_string()))?
    .map_err(|reason: String| (3, reason))?;
    Ok(serde_json::json!({
        "rebound": name,
        "mux": {"session": parsed.mux_session, "pane_id": parsed.pane},
        "pid": parsed.pid,
        "status": "live",
    })
    .to_string())
}
