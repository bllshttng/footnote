//! Which pane hosts this row, and what a lifecycle gesture may act on.
//!
//! The sideline renders section 1 from the pane tree, so a row exists for
//! every live pane - including one whose registry row is gone (retired, or
//! never written). `stop` and `remove` used to resolve through the registry
//! alone and answered `no such agent` for exactly those rows, stranding a
//! live process behind a live pane (x-e763). This module owns the one
//! resolver both verbs route through, with the pane-table fallback.
//!
//! Resolution rules (fail-closed, one unambiguous target per keypress):
//! 1. A non-empty harness session id against the registry, identity first.
//! 2. The label against the registry. Absent falls through; any external row
//!    sharing the name refuses (never act on a registry agent an external
//!    shadows); a >1 non-external collision refuses as ambiguous.
//! 3. The pane id against the pane table, corroborated by label - pane ids
//!    recycle, so a bare id never acts on a guess. This leg is what makes a
//!    bare-pane row actionable: the pane carries the child pid, which is
//!    everything a remove needs.
//! Nothing answering names every lookup that was tried, so a refusal says
//! what was searched instead of implying the row does not exist.
//!
//! `StopAgent`/`RemoveAgent` on a registry target shell the fno-agents verbs
//! as before; on a pane target stop signals the child and leaves the pane,
//! remove kills it and drops the pane. Neither gesture is gated behind the
//! other, and remove never asks twice unless the operator turned the confirm
//! pref back on. (v26/x-76ea and v67 wire prose for the two commands moved
//! here from proto.rs, where this module now lives.)

use crate::agents_view::RegistryAgent;

/// The pane-table facts a lifecycle decision needs. The label is the same
/// `pane_label` derivation section 1 rendered the row with, so the
/// corroboration compares like with like.
pub(crate) struct PaneFacts {
    pub label: String,
}

/// What a resolved lifecycle gesture acts on: a registry row (the fno-agents
/// verbs own it) or a bare pane (this server owns the child directly).
#[derive(Debug)]
pub(crate) enum Target<'a> {
    Registry(&'a RegistryAgent),
    Pane(u64),
}

/// The borrowed [`Target`] in owned form, so a handler holds no catalog
/// borrow across its `&mut self` action calls.
pub(crate) enum LifecycleTarget {
    Registry(String),
    Pane(u64),
}

/// Resolve a sideline lifecycle target by identity, then label, then pane.
/// `pane_at` looks a pane id up in the caller's pane table; `None` there
/// means the pane is gone (reaped, or the row is stale).
pub(crate) fn resolve_target<'a>(
    agents: &'a [RegistryAgent],
    pane_at: impl Fn(u64) -> Option<PaneFacts>,
    name: &str,
    harness_session_id: Option<&str>,
    pane_id: Option<u64>,
) -> Result<Target<'a>, String> {
    if let Some(sid) = harness_session_id.filter(|s| !s.is_empty()) {
        let by_id: Vec<&RegistryAgent> = agents
            .iter()
            .filter(|a| !a.external && super::agent_harness_session_id(a) == Some(sid))
            .collect();
        if let [one] = by_id.as_slice() {
            return Ok(Target::Registry(one));
        }
    }
    let matches: Vec<&RegistryAgent> = agents.iter().filter(|a| a.name == name).collect();
    if matches.iter().any(|a| a.external) {
        return Err(format!(
            "{name} is external - manage it from its own session"
        ));
    }
    match matches.as_slice() {
        [one] => return Ok(Target::Registry(one)),
        [] => {}
        _ => return Err(format!("{name} is ambiguous - use the CLI")),
    }
    match pane_id {
        Some(pid) => match pane_at(pid) {
            Some(facts) if facts.label == name => Ok(Target::Pane(pid)),
            Some(_) => Err(nothing_answered(
                name,
                Some(pid),
                Some("it hosts a different label now"),
            )),
            None => Err(nothing_answered(name, Some(pid), Some("the pane is gone"))),
        },
        None => Err(nothing_answered(name, None, None)),
    }
}

/// The nothing-answered refusal. It names every lookup that was tried - both
/// registry legs, and the pane leg when a pane id rode the gesture - so the
/// operator can tell "no such row" from "the row moved".
fn nothing_answered(name: &str, pane_id: Option<u64>, verdict: Option<&str>) -> String {
    let pane_part = match (pane_id, verdict) {
        (Some(pid), Some(v)) => format!(", and the pane table by pane id {pid} ({v})"),
        (Some(pid), None) => format!(", and the pane table by pane id {pid}"),
        (None, _) => String::new(),
    };
    format!("no such agent: {name} - searched the registry by session id and by label{pane_part}; nothing answered")
}

/// True for the session-id shapes harnesses mint: the hyphenated 36-char hex
/// form (claude UUIDv4, codex UUIDv7) and opencode's `ses_` + alphanumerics
/// (`harnesses/opencode.py:is_session_id` is the Python twin). A populated
/// `fno_id` of any other shape is a worker NAME the registry's identity slot
/// holds, never an id.
pub fn session_id_shaped(value: &str) -> bool {
    if let Some(tail) = value.strip_prefix("ses_") {
        return !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_alphanumeric());
    }
    let b = value.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// (x-b029) The `fno_id` column for one row: `(state, cell)`. Three states
/// where two lived before. A RESOLVED session id. UNRESOLVED with the reason
/// named, for a row carrying fno evidence but no resolvable id: `spawned-name`
/// (the spawn captured a worker `name`, so the row is fno's even though the
/// registry join found no session id) or `name-as-id` (the registry identity
/// slot holds a worker name). And the `-` sentinel, now reserved for rows with
/// NO fno evidence at all. Pure, so the self-contradicting row (a worker name
/// beside `-`) is unit-testable without a socket.
pub fn identity_cell(fno_id: Option<&str>, name: Option<&str>) -> (&'static str, String) {
    match fno_id {
        Some(id) if session_id_shaped(id) => ("resolved", id.to_string()),
        Some(_) => ("unresolved:name-as-id", "unresolved:name-as-id".into()),
        None => match name {
            Some(_) => ("unresolved:spawned-name", "unresolved:spawned-name".into()),
            None => ("untracked", "-".into()),
        },
    }
}

/// The registry row's identity state, for the lines a CLI prints about a row
/// it could not pane-host.
pub(crate) fn row_identity_state(row: &RegistryAgent) -> &'static str {
    identity_cell(row.session_id.as_deref(), Some(&row.name)).0
}

/// The pane-table side of the same question, for the CLI verbs: one pane in
/// a session's listing that hosts `row`. A session-id-shaped identity joins
/// on the pane's resolved fno_id; a row with no usable id can still join on
/// its name (the spawned-name state). `None` = no pane in this listing hosts
/// the row, which lets the caller report paneless with the state named.
pub(crate) fn scan_panes<'a>(
    panes: &'a [crate::proto::PaneInfo],
    identity: Option<&str>,
    name: &str,
) -> Option<&'a crate::proto::PaneInfo> {
    if let Some(id) = identity.filter(|s| session_id_shaped(s)) {
        if let Some(hit) = panes.iter().find(|p| p.fno_id.as_deref() == Some(id)) {
            return Some(hit);
        }
    }
    panes.iter().find(|p| p.name.as_deref() == Some(name))
}

impl super::Core {
    /// Resolve a sideline lifecycle target (x-76ea) by identity, then label.
    /// Fail-closed: absent, any external row sharing the name, or a >1
    /// non-external collision are all refused, so a keypress can only ever
    /// act on exactly one unambiguous registry agent. The registry-only
    /// callers (rename) resolve here; the lifecycle verbs take
    /// [`Self::resolve_lifecycle_full`], which also answers from the pane
    /// the row was drawn from (x-e763).
    pub(super) fn resolve_lifecycle_target(
        &self,
        name: &str,
        harness_session_id: Option<&str>,
    ) -> Result<&RegistryAgent, String> {
        match resolve_target(
            &self.agents,
            |pid| {
                self.panes.get(&pid).map(|e| PaneFacts {
                    label: super::pane_label(
                        e.name.as_deref(),
                        e.node.as_deref(),
                        &e.cwd,
                        e.cmd.as_deref(),
                    ),
                })
            },
            name,
            harness_session_id,
            None,
        )? {
            Target::Registry(a) => Ok(a),
            // Registry-only callers pass pane_id None, so this arm is
            // unreachable from here by construction.
            Target::Pane(_) => Err("pane rows are not renamable".into()),
        }
    }

    /// Resolve with every leg, including the pane the client's row was drawn
    /// from (x-e763). Returns an owned target, so no catalog borrow is held
    /// across the handler's `&mut self` action calls.
    pub(super) fn resolve_lifecycle_full(
        &self,
        name: &str,
        harness_session_id: Option<&str>,
        pane_id: Option<u64>,
    ) -> Result<LifecycleTarget, String> {
        let target = resolve_target(
            &self.agents,
            |pid| {
                self.panes.get(&pid).map(|e| PaneFacts {
                    label: super::pane_label(
                        e.name.as_deref(),
                        e.node.as_deref(),
                        &e.cwd,
                        e.cmd.as_deref(),
                    ),
                })
            },
            name,
            harness_session_id,
            pane_id,
        )?;
        Ok(match target {
            Target::Registry(a) => LifecycleTarget::Registry(a.name.clone()),
            Target::Pane(pid) => LifecycleTarget::Pane(pid),
        })
    }

    /// Stop on a pane target (x-e763): signal the child, leave the pane. The
    /// row flips exited on the next render when the child died; the leftover
    /// pane is the operator's remove to make.
    pub(super) fn stop_pane_child(&self, client_id: u64, pane_id: u64, name: &str) {
        match self.panes.get(&pane_id) {
            Some(entry) => match entry.pty.child_pid() {
                Some(cpid) => {
                    let ok = unsafe { libc::kill(cpid as libc::pid_t, libc::SIGTERM) } == 0;
                    let msg = if ok {
                        format!("signalled {name} (pid {cpid}); pane stays open")
                    } else {
                        format!("signal to {name} (pid {cpid}) failed")
                    };
                    self.notice(client_id, msg);
                }
                None => self.notice(
                    client_id,
                    format!("{name}: pane {pane_id} has no live child"),
                ),
            },
            None => self.notice(
                client_id,
                format!("{name}: pane {pane_id} is gone - reopen the sideline"),
            ),
        }
    }

    /// Remove on a pane target (x-e763): kill the child, release the writer
    /// claim, drop the pane. `close_pane` is the cascade - reap_pane kills
    /// via the pty, so a keeper-hosted child dies correctly too.
    pub(super) fn remove_pane_row(
        &mut self,
        client_id: u64,
        pane_id: u64,
        name: &str,
    ) -> super::Flow {
        if !self.panes.contains_key(&pane_id) {
            self.notice(
                client_id,
                format!("{name}: pane {pane_id} is gone - reopen the sideline"),
            );
            return super::Flow::Continue;
        }
        let flow = self.close_pane(pane_id);
        self.notice(
            client_id,
            format!("removed {name}: child stopped, pane closed"),
        );
        flow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, sid: Option<&str>) -> RegistryAgent {
        let mut r = RegistryAgent {
            name: name.into(),
            ..Default::default()
        };
        r.harness_session_id = sid.map(str::to_owned);
        r
    }

    fn pane(label: &str) -> Option<PaneFacts> {
        Some(PaneFacts {
            label: label.into(),
        })
    }

    #[test]
    fn pane_leg_answers_when_the_registry_misses() {
        // The specimen: a live bare-pane row whose registry row is gone.
        let agents = vec![];
        let resolved = resolve_target(
            &agents,
            |_| pane("t-c574-rubric-gpt"),
            "t-c574-rubric-gpt",
            None,
            Some(1633),
        );
        assert!(matches!(resolved, Ok(Target::Pane(1633))), "{resolved:?}");
    }

    #[test]
    fn pane_leg_refuses_on_a_recycled_pane_id() {
        // Pane ids recycle: a bare pane id only acts when the label still
        // matches the row it was drawn from.
        let agents = vec![];
        let resolved = resolve_target(
            &agents,
            |_| pane("someone-else"),
            "t-c574-rubric-gpt",
            None,
            Some(1633),
        );
        let msg = resolved.unwrap_err();
        assert!(msg.contains("different label"), "{msg}");
        assert!(msg.contains("registry by session id and by label"), "{msg}");
        assert!(msg.contains("pane id 1633"), "{msg}");
    }

    #[test]
    fn refusal_names_every_lookup_when_the_pane_is_gone() {
        let agents = vec![];
        let resolved = resolve_target(&agents, |_| None, "ghost", None, Some(7));
        let msg = resolved.unwrap_err();
        assert!(msg.contains("registry by session id and by label"), "{msg}");
        assert!(msg.contains("pane id 7"), "{msg}");
        assert!(msg.contains("the pane is gone"), "{msg}");
    }

    #[test]
    fn refusal_without_a_pane_id_names_the_registry_legs() {
        let agents = vec![];
        let msg = resolve_target(&agents, |_| None, "ghost", None, None).unwrap_err();
        assert!(msg.starts_with("no such agent: ghost"), "{msg}");
        assert!(msg.contains("registry by session id and by label"), "{msg}");
        assert!(!msg.contains("pane table"), "{msg}");
    }

    #[test]
    fn external_and_ambiguous_refusals_still_preempt_the_pane_leg() {
        let mut ext = row("t-win", None);
        ext.external = true;
        // Any external row sharing the label refuses before the pane leg.
        let agents = vec![ext.clone()];
        let msg = resolve_target(&agents, |_| pane("t-win"), "t-win", None, Some(1)).unwrap_err();
        assert!(msg.contains("external"), "{msg}");
        // Two non-external rows sharing the label stay ambiguous even with a
        // pane id riding the gesture.
        let agents = vec![row("t-win", None), row("t-win", None)];
        let msg = resolve_target(&agents, |_| pane("t-win"), "t-win", None, Some(1)).unwrap_err();
        assert!(msg.contains("ambiguous"), "{msg}");
    }

    #[test]
    fn identity_leg_still_wins_and_still_refuses_a_shadowed_name() {
        let mut a = row("label", Some("11111111-1111-4111-8111-111111111111"));
        a.external = false;
        let agents = vec![a];
        let resolved = resolve_target(
            &agents,
            |_| pane("label"),
            "stale",
            Some("11111111-1111-4111-8111-111111111111"),
            Some(5),
        );
        assert!(matches!(resolved, Ok(Target::Registry(_))), "{resolved:?}");
    }

    #[test]
    fn scan_panes_joins_by_resolved_id_then_by_name() {
        let hit: crate::proto::PaneInfo = serde_json::from_value(serde_json::json!({
            "pane_id": 1637,
            "squad_id": 0,
            "tab_id": 0,
            "cwd": "/tmp",
            "fno_id": "46c2b4a1-6fe2-4d2a-ab0b-b992674f8148"
        }))
        .unwrap();
        let panes = vec![hit];
        let found = scan_panes(
            &panes,
            Some("46c2b4a1-6fe2-4d2a-ab0b-b992674f8148"),
            "other",
        );
        assert_eq!(found.map(|p| p.pane_id), Some(1637));
        // A name-shaped identity is never treated as an id; the name tier answers.
        let by_name = scan_panes(&panes, None, "other");
        assert!(by_name.is_none());
    }
}
/// (x-e763) Ask one session's server for the pane hosting `row` when the
/// registry row carries no mux field. One `PaneLs` round-trip; a server that
/// does not answer is an honest `None`. The join is
/// `lifecycle_target::scan_panes` - the same question the sideline asks,
/// asked of the pane table that knows.
pub(crate) fn pane_table_host(
    row: &crate::agents_view::RegistryAgent,
    session: &str,
) -> Option<(String, u64)> {
    let sock = crate::proto::socket_path(session).ok()?;
    let stream = crate::proto::connect_unix_timeout(&sock, crate::mux_cli::PROBE_TIMEOUT).ok()?;
    let reply = crate::mux_cli::send_control(
        stream,
        crate::proto::ControlVerb::PaneLs,
        crate::mux_cli::CONTROL_TIMEOUT,
        crate::mux_cli::CONTROL_REPLY_DEADLINE,
        session,
    )
    .ok()?;
    match reply {
        crate::proto::ServerMsg::PaneList { panes } => {
            scan_panes(&panes, row.effective_identity(), &row.name)
                .map(|p| (session.to_string(), p.pane_id))
        }
        _ => None,
    }
}
