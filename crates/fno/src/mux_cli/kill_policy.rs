//! The kill-server session policy: the unkept-pane refusal, the preserved
//! and ended receipt, and the `--stale-idle` / `--all` selectors. Child of
//! `mux_cli` (the parent is shrink-only) - the row partition is a PURE
//! function over `mux ls` rows, so the restart verb consumes the same logic
//! and the two never disagree about who is spared.

use super::*;

/// One live pane measured for the kill decision: what the refusal names and
/// what the receipt reports.
pub(crate) struct PaneSnapshot {
    pub(crate) pane_id: u64,
    pub(crate) child_pid: Option<u32>,
    pub(crate) name: Option<String>,
    pub(crate) session_id: Option<String>,
    /// Live keeper pid at this pane's id. `None` = unkept: nothing holds
    /// this pane's pty master across a server death.
    pub(crate) keeper_pid: Option<u32>,
}

impl PaneSnapshot {
    pub(crate) fn is_kept(&self) -> bool {
        self.keeper_pid.is_some()
    }

    /// The one-line refusal form: pane id, child pid, command.
    pub(crate) fn describe(&self) -> String {
        format!(
            "pane {} child {} ({})",
            self.pane_id,
            self.child_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".into()),
            self.name.as_deref().unwrap_or("unnamed"),
        )
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "pane_id": self.pane_id,
            "child_pid": self.child_pid,
            "keeper_pid": self.keeper_pid,
            "name": self.name,
            "session_id": self.session_id,
        })
    }
}

/// Measure the session's panes: one `pane ls` control round trip + the
/// keeper sockets behind it. `Err` = UNMEASURABLE (the server holds its
/// socket but answers no control verb): the caller keeps the escalation
/// ladder and says the pane set was unmeasured - a recovery verb must not
/// depend on the subsystem it recovers.
pub(crate) fn measure_panes(session: &str) -> Result<Vec<PaneSnapshot>, String> {
    let sock = proto::socket_path(session)?;
    let panes = match control_roundtrip(&sock, session, ControlVerb::PaneLs) {
        Ok(ServerMsg::PaneList { panes }) => panes,
        _ => return Err("the server answered no pane ls".into()),
    };
    // Live keeper sockets by pane id. A socket nobody lives behind is not a
    // keeper: it is a dead leftover, and its pane reads unkept.
    let mut keepers: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    for (key, path) in crate::pty::keeper_sockets(session) {
        if let Some(reply) = crate::pty::keeper_identify(&path) {
            if let Some(pid) = reply.get("keeper_pid").and_then(serde_json::Value::as_u64) {
                keepers.insert(key, pid as u32);
            }
        }
    }
    Ok(panes
        .into_iter()
        .map(|p| PaneSnapshot {
            keeper_pid: keepers.get(&p.pane_id).copied(),
            session_id: p.harness_session_id.clone().or_else(|| p.fno_id.clone()),
            child_pid: p.child_pid,
            name: p.name,
            pane_id: p.pane_id,
        })
        .collect())
}

/// The refusal for a session with live unkept panes: nothing is signalled,
/// every pane is named, and the flag that ends them is named. Returns the
/// refusal text for the JSON summary's `refused` array.
pub(crate) fn unkept_refusal(session: &str, unkept: &[&PaneSnapshot]) -> String {
    let mut text = format!(
        "session {session:?} hosts {} unkept pane(s); killing the server ends them:",
        unkept.len()
    );
    for pane in unkept {
        text.push_str(&format!("\n  {}", pane.describe()));
    }
    text.push_str(&format!(
        "\nend them deliberately with: fno mux kill-server {session} --end-unkept"
    ));
    text
}

/// One `mux ls` row's policy facts, decoded from the JSON row shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LsRow {
    pub(crate) session: String,
    pub(crate) state: String,
    pub(crate) stale: bool,
    pub(crate) panes: u64,
    pub(crate) log: Option<String>,
}

impl LsRow {
    pub(crate) fn parse(value: &serde_json::Value) -> Option<LsRow> {
        let session = value.get("session")?.as_str()?.to_string();
        let state = value.get("state")?.as_str()?.to_string();
        Some(LsRow {
            session,
            stale: value
                .get("stale")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            panes: value
                .get("panes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            log: value
                .get("log")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            state,
        })
    }
}

/// The one partition pass over `mux ls` rows. Buckets mirror the restart
/// verb's contract: a stale-wire live server heals automatically only when
/// it hosts no live pane; one with live panes is spared because killing it
/// closes their ptys (`--all` is the deliberate break-glass lever). A wedged
/// server holds its socket but never accepts: a broken server, NOT a benign
/// non-live row - named as a failure with its log path. Other non-live rows
/// are reported, never killed (killing a dead socket is meaningless and
/// could unlink a socket `kill-server` owns).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Partition {
    /// Stale-wire live sessions hosting no live pane: safe to kill.
    pub(crate) stale_idle: Vec<String>,
    /// Stale-wire live sessions WITH live panes: spared under `--stale-idle`.
    pub(crate) stale_with_panes: Vec<String>,
    /// Current-wire live sessions: only `--all` touches them.
    pub(crate) current_live: Vec<String>,
    /// Wedged rows: actionable failures carrying their log path.
    pub(crate) wedged: Vec<(String, Option<String>)>,
    /// Other non-live rows: reported, never killed.
    pub(crate) other: Vec<String>,
}

/// The pure partition: every bucket's invariant holds by construction
/// instead of by several comprehensions agreeing with each other.
pub(crate) fn partition(rows: &[LsRow]) -> Partition {
    let mut out = Partition::default();
    for row in rows {
        match row.state.as_str() {
            "wedged" => out.wedged.push((row.session.clone(), row.log.clone())),
            "live" if row.stale && row.panes == 0 => out.stale_idle.push(row.session.clone()),
            "live" if row.stale => out.stale_with_panes.push(row.session.clone()),
            "live" => out.current_live.push(row.session.clone()),
            _ => out.other.push(row.session.clone()),
        }
    }
    out
}

/// Which sessions the selector kills: `--stale-idle` takes only the
/// pane-less stale-wire live set; `--all` takes every live session.
pub(crate) fn selector_targets(selector: Selector, p: &Partition) -> Vec<String> {
    match selector {
        Selector::StaleIdle => p.stale_idle.clone(),
        Selector::All => {
            let mut all = p.stale_idle.clone();
            all.extend(p.stale_with_panes.iter().cloned());
            all.extend(p.current_live.iter().cloned());
            all
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    StaleIdle,
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: &str, state: &str, stale: bool, panes: u64) -> LsRow {
        LsRow {
            session: session.into(),
            state: state.into(),
            stale,
            panes,
            log: None,
        }
    }

    #[test]
    fn partition_sorts_the_five_row_shapes() {
        let rows = vec![
            row("idle", "live", true, 0),
            row("busy", "live", true, 2),
            row("cur", "live", false, 1),
            row("stuck", "wedged", false, 0),
            row("dead", "stale", false, 0),
        ];
        let p = partition(&rows);
        assert_eq!(p.stale_idle, vec!["idle".to_string()]);
        assert_eq!(p.stale_with_panes, vec!["busy".to_string()]);
        assert_eq!(p.current_live, vec!["cur".to_string()]);
        assert_eq!(p.wedged.len(), 1);
        assert_eq!(p.other, vec!["dead".to_string()]);
    }

    #[test]
    fn stale_idle_selects_only_paneless_stale_live_sessions() {
        let rows = vec![
            row("idle", "live", true, 0),
            row("busy", "live", true, 2),
            row("cur", "live", false, 0),
            row("stuck", "wedged", false, 0),
            row("dead", "stale", false, 0),
        ];
        let p = partition(&rows);
        assert_eq!(
            selector_targets(Selector::StaleIdle, &p),
            vec!["idle".to_string()]
        );
        assert!(
            !selector_targets(Selector::StaleIdle, &p).contains(&"busy".to_string()),
            "a stale-wire server with live panes is spared by name"
        );
    }

    #[test]
    fn all_selects_every_live_session_and_never_a_wedged_one() {
        let rows = vec![
            row("idle", "live", true, 0),
            row("busy", "live", true, 2),
            row("cur", "live", false, 1),
            row("stuck", "wedged", false, 0),
            row("dead", "stale", false, 0),
        ];
        let p = partition(&rows);
        let mut targets = selector_targets(Selector::All, &p);
        targets.sort();
        assert_eq!(
            targets,
            vec!["busy".to_string(), "cur".to_string(), "idle".to_string()]
        );
        assert!(
            !targets.contains(&"stuck".to_string()),
            "wedged rows are failures, not targets"
        );
        assert!(
            !targets.contains(&"dead".to_string()),
            "non-live rows are reported, never killed"
        );
    }

    #[test]
    fn wedged_row_carries_its_log_path_for_the_failure_line() {
        let value = serde_json::json!({
            "session": "stuck", "state": "wedged", "log": "/tmp/mux/stuck.log"
        });
        let parsed = LsRow::parse(&value).expect("the row parses");
        let p = partition(&[parsed]);
        assert_eq!(
            p.wedged,
            vec![("stuck".to_string(), Some("/tmp/mux/stuck.log".to_string()))]
        );
    }
}

// The verb bodies: the named kill and the selector kill. They live beside
// the policy they enforce; `use super::*` reaches the consts and the
// escalation ladder they share.

use super::*;

/// `fno mux kill-server [<name>]`: shut one session down. A live server Byes
/// its clients, kills every pane child, and exits (its SocketGuard unlinks
/// the socket); a stale socket is unlinked here with a message (exit 0); no
/// socket at all is "no server" (exit 1). A wedged holder - one that never
/// accepted, or accepted and never answered - is escalated through SIGTERM
/// and SIGKILL to unlink: a recovery verb must not depend on the
/// subsystem it recovers. Every run prints which rung ended it, and an
/// unrecoverable state names the next action instead of leaving a dead `&&`
/// chain with no hint.
pub fn kill_server(session: &str, json: bool, end_unkept: bool) -> i32 {
    let sock = match proto::socket_path(session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno: {e}");
            return EXIT_USAGE;
        }
    };
    if !sock.exists() {
        eprintln!("fno: no server for session {session:?}");
        return EXIT_ERROR;
    }
    // Measure BEFORE any signal: the refusal must fire while every pane is
    // still live. A wedged server answers no `pane ls` and cannot be
    // measured - keep today's escalation ladder for it and say the pane set
    // was unmeasured. A recovery verb must not depend on the subsystem it
    // recovers.
    let measured = kill_policy::measure_panes(session).ok();
    if let Some(panes) = &measured {
        let unkept: Vec<&kill_policy::PaneSnapshot> =
            panes.iter().filter(|p| !p.is_kept()).collect();
        if !unkept.is_empty() && !end_unkept {
            let refusal = kill_policy::unkept_refusal(session, &unkept);
            eprintln!("fno: {refusal}");
            return EXIT_ERROR;
        }
    }
    let outcome = kill_server_inner(session, &sock);
    match outcome.path {
        KillPath::Unrecoverable => eprintln!("{}", outcome.note),
        _ => {
            // The receipt names who was preserved and who ended: a kept pane
            // outlives the kill (its keeper re-adopts it into the next
            // server); an unkept one died with it. Empty arrays and an
            // unmeasured note keep the wedge case honest.
            let (preserved, ended, unmeasured) = match &measured {
                Some(panes) => {
                    let preserved: Vec<_> = panes
                        .iter()
                        .filter(|p| p.is_kept())
                        .map(|p| p.to_json())
                        .collect();
                    let ended: Vec<_> = panes
                        .iter()
                        .filter(|p| !p.is_kept())
                        .map(|p| p.to_json())
                        .collect();
                    (preserved, ended, false)
                }
                None => (Vec::new(), Vec::new(), true),
            };
            let mut note = outcome.note.clone();
            if unmeasured {
                note.push_str(" (the pane set was unmeasured)");
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "session": session,
                        "killed": true,
                        "note": note,
                        "path": outcome.path.json_tag(),
                        "preserved": preserved,
                        "ended": ended,
                    })
                );
            } else {
                println!("{}", note);
                for pane in preserved {
                    println!(
                        "preserved: pane {} child {} keeper {} ({})",
                        pane["pane_id"].as_u64().unwrap_or(0),
                        pane["child_pid"]
                            .as_u64()
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".into()),
                        pane["keeper_pid"]
                            .as_u64()
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".into()),
                        pane["name"].as_str().unwrap_or("unnamed"),
                    );
                }
                for pane in ended {
                    println!(
                        "ended: pane {} child {} ({})",
                        pane["pane_id"].as_u64().unwrap_or(0),
                        pane["child_pid"]
                            .as_u64()
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".into()),
                        pane["name"].as_str().unwrap_or("unnamed"),
                    );
                }
            }
        }
    }
    outcome.exit_code()
}

/// The work of `kill-server`, returning the rung taken instead of printing.
/// Both dead ends of the old verb - a connect that timed out, and a graceful
/// request the connected server never answered - enter the same escalation
/// ladder instead of refusing, because that is exactly the state the verb
/// exists for.
/// `fno mux kill-server --stale-idle | --all`: partition every live session
/// with the shared pure partition, then kill each target with the SAME
/// unkept refusal and receipt the named form carries. A refusal keeps the
/// run going to the next session; wedged rows are named failures with their
/// log paths, never targets. Under `--json` one object: `restarted`,
/// `spared`, `wedged`, `other`, `refused`, and per session the `preserved`
/// and `ended` arrays.
pub fn kill_selector(selector: kill_policy::Selector, json: bool) -> i32 {
    let rows: Vec<kill_policy::LsRow> = match session_rows() {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| {
                let stale = row.wire_stale();
                let json_row = session_row_json(row);
                kill_policy::LsRow::parse(&json_row)
                    // The LsRow::parse contract needs `state`; carry the
                    // probed staleness through for live rows.
                    .map(|mut parsed| {
                        if stale {
                            parsed.stale = true;
                        }
                        parsed
                    })
            })
            .collect(),
        Err(e) => {
            eprintln!("fno: {e}");
            return EXIT_ERROR;
        }
    };
    let partition = kill_policy::partition(&rows);
    let wedged: Vec<(String, Option<String>)> = partition.wedged.clone();
    let targets = kill_policy::selector_targets(selector, &partition);
    let spared: Vec<(String, u64)> = partition
        .stale_with_panes
        .iter()
        .filter_map(|name| {
            rows.iter()
                .find(|r| r.session == *name)
                .map(|r| (name.clone(), r.panes))
        })
        .collect();
    let mut summary = serde_json::json!({
        "restarted": [],
        "spared": partition.stale_with_panes,
        "wedged": partition.wedged.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(),
        "other": partition.other,
        "refused": [],
        "sessions": [],
    });
    let mut exit = EXIT_OK;
    if !spared.is_empty() {
        // A spared stale-wire server with live panes is an unrestored
        // fleet: the selector reports it and FAILS, exactly like the
        // restart verb's floor always did.
        exit = EXIT_ERROR;
    }
    for session in &targets {
        let sock = match proto::socket_path(session) {
            Ok(p) => p,
            Err(_) => {
                exit = EXIT_ERROR;
                continue;
            }
        };
        let measured = kill_policy::measure_panes(session).ok();
        if let Some(panes) = &measured {
            let unkept: Vec<&kill_policy::PaneSnapshot> =
                panes.iter().filter(|p| !p.is_kept()).collect();
            if !unkept.is_empty() {
                let refusal = kill_policy::unkept_refusal(session, &unkept);
                eprintln!("fno: {refusal}");
                summary["refused"]
                    .as_array_mut()
                    .expect("array")
                    .push(serde_json::json!(session));
                summary["sessions"]
                    .as_array_mut()
                    .expect("array")
                    .push(serde_json::json!({
                        "session": session,
                        "killed": false,
                        "refusal": refusal,
                    }));
                exit = EXIT_ERROR;
                continue;
            }
        }
        let outcome = kill_server_inner(session, &sock);
        if matches!(outcome.path, KillPath::Unrecoverable) {
            eprintln!("{}", outcome.note);
            exit = EXIT_ERROR;
        }
        let (preserved, ended) = match &measured {
            Some(panes) => (
                panes
                    .iter()
                    .filter(|p| p.is_kept())
                    .map(|p| p.to_json())
                    .collect::<Vec<_>>(),
                panes
                    .iter()
                    .filter(|p| !p.is_kept())
                    .map(|p| p.to_json())
                    .collect::<Vec<_>>(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        summary["restarted"]
            .as_array_mut()
            .expect("array")
            .push(serde_json::json!(session));
        summary["sessions"]
            .as_array_mut()
            .expect("array")
            .push(serde_json::json!({
                "session": session,
                "killed": true,
                "preserved": preserved,
                "ended": ended,
            }));
    }
    // The human narratives ride stderr in BOTH modes: --json keeps stdout
    // to one object, and the spared/wedged/refused lines are exactly what
    // an operator (or the restart verb relaying) must see.
    for (name, panes) in &spared {
        eprintln!(
            "fno: mux session '{name}' has {panes} live pane(s); its stale-wire server \
is spared. Use `fno agents restart --mux` (or kill-server --all) to force-kill it."
        );
    }
    if json {
        println!("{summary}");
    } else {
        for w in &wedged {
            let (name, log) = (
                w.0.as_str(),
                w.1.as_deref().unwrap_or("(server log path unknown)"),
            );
            eprintln!(
                "fno: mux session '{name}' is WEDGED (holds its socket but is not accepting connections). \
`fno mux kill-server {name}` recovers it (escalates to SIGTERM/SIGKILL; its log: {log})."
            );
        }
        if !partition.other.is_empty() {
            eprintln!(
                "fno: {} non-live mux row(s) (not restarted): {:?}",
                partition.other.len(),
                partition.other
            );
        }
        println!("done");
    }
    exit
}
