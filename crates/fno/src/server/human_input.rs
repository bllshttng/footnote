//! What did a human type into a pane? The two rows the mux writes at its
//! one human-keystroke choke point (`CoreMsg::Input`): `human_touch`
//! steering telemetry (moved unchanged from server.rs) and the
//! `operator_submit` witness recorded when a person's Enter reaches a pane.
//! Machine transports (pane send, control.sock mail, codex turn/start)
//! never reach that arm, so a row here is the positive marker that a
//! person typed.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Core;

/// At most one `human_touch(inject)` per pane per window: the first keystroke
/// of a burst means "operator started steering this pane".
/// ponytail: fixed 5s window; tune only if real bursts split.
const TOUCH_COALESCE_WINDOW: Duration = Duration::from_secs(5);

/// Whether an inject emit should fire now for `pane` (recording `now`), or be
/// coalesced into the burst whose start time is already stored.
fn touch_coalesce(last: &mut HashMap<u64, Instant>, pane: u64, now: Instant) -> bool {
    match last.entry(pane) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            // saturating: a `now` behind the stored instant (clock quirks
            // under virtualization) coalesces instead of panicking.
            if now.saturating_duration_since(*e.get()) < TOUCH_COALESCE_WINDOW {
                false
            } else {
                e.insert(now);
                true
            }
        }
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(now);
            true
        }
    }
}

/// The attended hold window (x-0e09): a keystroke past this since the
/// pane's last arm re-arms the hold. One arm per 5-minute window; the idle
/// clock the arm writes lifts the hold and delivers the digest ~5 minutes
/// after the operator goes quiet. Same value as mail_hold::DEFAULT_MINUTES.
const ATTENDED_HOLD_WINDOW: Duration = Duration::from_secs(300);

/// Arm now? Pure so tests drive the burst arithmetic: the first keystroke
/// ever, or one past the window since the last arm. A keystroke inside the
/// window coalesces into the burst the arm already covers.
fn hold_arm_due(last: Option<Instant>, now: Instant) -> bool {
    match last {
        None => true,
        Some(t) => now.saturating_duration_since(t) >= ATTENDED_HOLD_WINDOW,
    }
}

/// One detached `fno-agents mail-hold --session <id>` per window (the arm
/// the Python verb cannot run for another session; see mail_hold.rs). Off
/// the core loop, stdio null, never awaited; a dropped Child handle leaves
/// the child running (no kill_on_drop). Test builds skip the spawn: the
/// arm decision and the pane-session bind are what the tests assert.
fn spawn_hold_arm(session: &str) {
    if cfg!(test) {
        return;
    }
    let mut cmd = crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    cmd.args(["mail-hold", "--session", session, "--minutes", "5"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    tokio::spawn(async move {
        if let Err(exc) = crate::process_admission::tokio_spawn(&mut cmd) {
            eprintln!("fno mux: attended-hold arm spawn failed: {exc}");
        }
    });
}

/// A human submit inside one input chunk: a CR (0x0d) that is neither the
/// meta-Enter escape (ESC CR, a newline inside the composers) nor inside a
/// bracketed paste (`ESC[200~` .. `ESC[201~`). ponytail: a paste split
/// across chunks can miss or add one submit; the fold's one-to-one binding
/// bounds the effect.
pub(super) fn is_submit(bytes: &[u8]) -> bool {
    let mut in_paste = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes[i..].starts_with(b"\x1b[200~") {
            in_paste = true;
            i += 6;
            continue;
        }
        if bytes[i] == 0x1b && bytes[i..].starts_with(b"\x1b[201~") {
            in_paste = false;
            i += 6;
            continue;
        }
        if bytes[i] == 0x0d && !in_paste && bytes.get(i.wrapping_sub(1)) != Some(&0x1b) {
            return true;
        }
        i += 1;
    }
    false
}

/// The `operator_submit` journal row: where and when a human pressed Enter.
/// No typed text is ever recorded. Pure so tests can assert the envelope.
fn submit_row(
    mux_session: &str,
    pane: u64,
    via: &str,
    resolution: &str,
    harness_session: Option<&str>,
    harness: Option<&str>,
    fno_id: Option<&str>,
) -> serde_json::Value {
    let submit_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "operator_submit",
        "source": "daemon",
        "data": {
            "mux_session": mux_session,
            "pane": pane,
            "via": via,
            "submit_ms": submit_ms,
            "resolution": resolution,
            "harness_session": harness_session,
            "harness": harness,
            "fno_id": fno_id,
        }
    })
}

impl Core {
    /// (graph node id, squad cwd) for a `human_touch` emit on `pane`. Node id:
    /// the pane's `FNO_NODE` provenance; fallback, the owning squad's
    /// cwd basename when it is node-id shaped (the worktree-per-node
    /// convention). Neither -> None, and the event carries resolution=failed
    /// rather than being dropped (AC4-FR).
    pub(super) fn pane_touch_provenance(&self, pane: u64) -> (Option<String>, Option<String>) {
        let cwd = self
            .session
            .find_pane(pane)
            .and_then(|(sid, _)| self.session.squad(sid))
            .map(|sq| sq.canonical_cwd().to_string());
        let node = self
            .panes
            .get(&pane)
            .and_then(|e| e.node.clone())
            .or_else(|| {
                cwd.as_deref()
                    .and_then(|c| Path::new(c).file_name())
                    .and_then(|b| b.to_str())
                    .filter(|b| super::node_id_shaped(b))
                    .map(str::to_owned)
            });
        (node, cwd)
    }

    /// Emit `human_touch` for one steering action on `pane` (W4 touch
    /// telemetry). `coalesced` applies the per-pane window (inject bursts);
    /// answer submits are one emit per action. The write rides the Python
    /// `type` envelope via a fire-and-forget `fno doctor event emit` shell-out (the
    /// digest idiom) - no Rust-side `kind`, so the three-places rule
    /// never applies. The shell-out runs in the squad's cwd so the event
    /// lands in that project's events.jsonl. A failure bumps
    /// `touch_emit_failures` and never touches the steering path (AC4-ERR).
    pub(super) fn touch(&mut self, pane: u64, source: &'static str, coalesced: bool) {
        if coalesced && !touch_coalesce(&mut self.touch_last_emit, pane, Instant::now()) {
            return;
        }
        // cfg!(test): in unit tests current_exe is the test binary, and
        // exec'ing it with event-emit args would re-enter the test harness.
        // FNO_TOUCH_EMIT=0 is the operator kill switch.
        if cfg!(test) || std::env::var_os("FNO_TOUCH_EMIT").is_some_and(|v| v == "0") {
            return;
        }
        let (node, cwd) = self.pane_touch_provenance(pane);
        let failures = Arc::clone(&self.touch_emit_failures);
        tokio::spawn(async move {
            let resolution = if node.is_some() { "ok" } else { "failed" };
            let data = serde_json::json!({
                "graph_node_id": node,
                "source": source,
                "resolution": resolution,
            })
            .to_string();
            const TOUCH_EMIT_TIMEOUT: Duration = Duration::from_secs(10);
            let mut cmd = crate::process_admission::tokio_command(super::fno_bin());
            cmd.args([
                "doctor",
                "event",
                "emit",
                "--type",
                "human_touch",
                "--source",
                "daemon",
                "--data",
                &data,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            let ok = matches!(
                tokio::time::timeout(
                    TOUCH_EMIT_TIMEOUT,
                    crate::process_admission::tokio_status(&mut cmd),
                )
                .await,
                Ok(Ok(s)) if s.success()
            );
            if !ok {
                // Counted AND visible (never swallowed): a 100%-failing
                // emitter silently inflates the autonomy rate, so each miss
                // logs to the server's stderr alongside the running total.
                let n = failures.fetch_add(1, Ordering::Relaxed) + 1;
                eprintln!("fno mux: human_touch({source}) emit failed ({n} this session)");
            }
        });
    }

    /// One `operator_submit` witness row for a human Enter on `pane`: bind
    /// the pane to its registry row (mux ref, then attach), else its portal
    /// row, and append one line to the agents journal. No row (a
    /// hand-started harness in a shell pane) writes `via: shell`,
    /// `resolution: unresolved`: the submit still counts, no transcript can
    /// join it. A failed append bumps `touch_emit_failures` and never
    /// touches the keystroke path.
    pub(super) fn witness_submit(&self, pane: u64) {
        // The same operator kill switch `touch` honors.
        if std::env::var_os("FNO_TOUCH_EMIT").is_some_and(|v| v == "0") {
            return;
        }
        let bound = super::agent_rows_join::bind_agent_to_pane(
            &self.agents,
            &self.session_name,
            pane,
            &self.attached,
            &|a| self.worker_pane_for_agent(a),
        )
        .map(|i| &self.agents[i]);
        let portal_bound = bound
            .is_none()
            .then(|| crate::thread_viewer::row_for_pane(&self.portals, pane, &self.agents))
            .flatten();
        let (via, resolution, agent) = match bound {
            Some(a) => ("pane", "ok", Some(a)),
            None => match portal_bound {
                Some(a) => ("portal", "ok", Some(a)),
                None => ("shell", "unresolved", None),
            },
        };
        let (harness_session, harness, fno_id) = match agent {
            Some(a) => (
                a.harness_session_id
                    .clone()
                    .or_else(|| a.effective_identity().map(str::to_string)),
                a.harness.clone(),
                a.session_id.clone(),
            ),
            None => (None, None, None),
        };
        let event = submit_row(
            &self.session_name,
            pane,
            via,
            resolution,
            harness_session.as_deref(),
            harness.as_deref(),
            fno_id.as_deref(),
        );
        // ponytail: the append runs inline on the core loop, one O_APPEND
        // line per Enter; move it off-loop if keystroke latency ever shows it.
        if crate::pane_send_audit::append_agents_event(
            &crate::pane_send_audit::pane_send_audit_events_path(),
            &event,
        )
        .is_err()
        {
            let n = self.touch_emit_failures.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!("fno mux: operator_submit emit failed ({n} this session)");
        }
    }

    /// Arm (or, on a submit, re-arm) the pane session's mail hold (x-0e09).
    /// The one arm source that sees the keystrokes: a session the registry
    /// carries holds delivery while its operator types and drains as one
    /// digest at the idle clock. One spawn per window (`hold_arm_due`);
    /// a pane with no session mapping does nothing. The spawn runs
    /// off-loop and never blocks the keystroke.
    pub(super) fn arm_attended_hold(&mut self, pane: u64, force: bool) {
        let now = Instant::now();
        if !force && !hold_arm_due(self.hold_arm_last.get(&pane).copied(), now) {
            return;
        }
        let session = super::agent_rows_join::bind_agent_to_pane(
            &self.agents,
            &self.session_name,
            pane,
            &self.attached,
            &|a| self.worker_pane_for_agent(a),
        )
        .and_then(|i| self.agents[i].harness_session_id.clone());
        let Some(session) = session else {
            return;
        };
        self.hold_arm_last.insert(pane, now);
        spawn_hold_arm(&session);
    }

    /// The tail of the `CoreMsg::Input` arm, one call from `handle_msg` so
    /// server.rs only shrinks: touch telemetry, the attended hold, the
    /// submit witness. A keystroke here is past the relay guard - PaneSend
    /// and relay writes never reach this.
    pub(super) fn input_tail(&mut self, focus: u64, bytes: &[u8]) {
        self.touch(focus, "inject", true);
        let submitted = is_submit(bytes);
        self.arm_attended_hold(focus, submitted);
        if submitted {
            self.witness_submit(focus);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_arm_window_and_submit_force_the_burst_arithmetic() {
        let t0 = Instant::now();
        assert!(hold_arm_due(None, t0), "the first keystroke ever arms");
        assert!(
            !hold_arm_due(Some(t0), t0 + Duration::from_secs(30)),
            "a keystroke inside the window coalesces into the burst the arm covers"
        );
        assert!(
            hold_arm_due(Some(t0), t0 + ATTENDED_HOLD_WINDOW),
            "one past the window a new burst re-arms"
        );
    }

    #[test]
    fn touch_coalesce_per_pane() {
        let mut last = HashMap::new();
        let t0 = Instant::now();
        assert!(touch_coalesce(&mut last, 1, t0));
        assert!(
            touch_coalesce(&mut last, 2, t0),
            "panes coalesce independently"
        );
    }

    #[test]
    fn pane_touch_provenance_cwd_fallback_and_none() {
        use crate::tree::{Node, Tab};
        let mut core = super::super::tests::empty_core();
        // No PaneEntry exists for either pane (no FNO_NODE provenance), so
        // the squad-cwd-basename fallback decides the node id.
        core.session.add_squad(
            1,
            vec!["/tmp/worktrees/x-cccc".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Leaf(7),
                focus: 7,
            },
        );
        core.session.add_squad(
            2,
            vec!["/tmp/worktrees/footnote".into()],
            None,
            Tab {
                name: None,
                id: 2,
                root: Node::Leaf(8),
                focus: 8,
            },
        );
        let (node, cwd) = core.pane_touch_provenance(7);
        assert_eq!(node.as_deref(), Some("x-cccc"));
        assert_eq!(cwd.as_deref(), Some("/tmp/worktrees/x-cccc"));
        // Unshaped basename: no node (the emit carries resolution=failed,
        // never a drop - AC4-FR), but the squad cwd still routes the event.
        let (node, cwd) = core.pane_touch_provenance(8);
        assert!(node.is_none());
        assert_eq!(cwd.as_deref(), Some("/tmp/worktrees/footnote"));
        // Unknown pane: (None, None).
        assert_eq!(core.pane_touch_provenance(99), (None, None));
    }

    #[test]
    fn is_submit_reads_a_bare_cr_and_rejects_paste_and_meta_enter() {
        assert!(is_submit(b"ship it\r"), "a typed Enter is a submit");
        assert!(is_submit(b"ship it\r\n"), "CRLF still carries one CR");
        // A CR inside a bracketed paste is pasted text, not a submit.
        assert!(!is_submit(b"\x1b[200~a\rb\x1b[201~"));
        // ESC CR is meta-Enter, a newline in the composers.
        assert!(!is_submit(b"\x1b\r"));
        assert!(!is_submit(b"no enter here"));
        assert!(!is_submit(b""));
    }

    #[test]
    fn submit_row_carries_the_envelope_and_no_text() {
        let row = submit_row(
            "main",
            7,
            "portal",
            "ok",
            Some("ccccdddd-1111-2222-3333-444455556666"),
            Some("claude"),
            Some("t-cccc-worker"),
        );
        assert_eq!(row["type"], "operator_submit");
        assert_eq!(row["source"], "daemon");
        let data = &row["data"];
        assert_eq!(data["mux_session"], "main");
        assert_eq!(data["pane"], 7);
        assert_eq!(data["via"], "portal");
        assert_eq!(data["resolution"], "ok");
        assert_eq!(
            data["harness_session"],
            "ccccdddd-1111-2222-3333-444455556666"
        );
        assert_eq!(data["harness"], "claude");
        assert_eq!(data["fno_id"], "t-cccc-worker");
        assert!(
            data["submit_ms"].as_u64().is_some(),
            "submit_ms is the millisecond join key"
        );
        assert!(
            row.to_string().len() < 400,
            "the row names where and when, never what was typed"
        );
    }

    fn witness_test_core(pane: u64) -> super::super::Core {
        use crate::agents_view::RegistryAgent;
        use crate::tree::{Node, Tab};
        let mut core = super::super::tests::empty_core();
        core.session.add_squad(
            1,
            vec!["/fixture".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Leaf(pane),
                focus: pane,
            },
        );
        core.agents = vec![RegistryAgent {
            name: "worker".into(),
            cwd: "/fixture".into(),
            mux: Some(("test".into(), pane)),
            harness: Some("claude".into()),
            session_id: Some("t-cccc-worker".into()),
            harness_session_id: Some("ccccdddd-1111-2222-3333-444455556666".into()),
            ..Default::default()
        }];
        core
    }

    fn journal_submits(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "operator_submit")
            .collect()
    }

    #[test]
    fn a_human_enter_in_a_bound_pane_writes_one_operator_submit_row() {
        use crate::server::CoreMsg;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-witness-bound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FNO_AGENTS_HOME", &dir);

        let mut core = witness_test_core(7);
        core.clients.push(crate::server::Client {
            id: 1,
            reliable_tx: tokio::sync::mpsc::channel(1).0,
            dirty: Default::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
            synced_modes: Default::default(),
            view: (1, 1),
            visible: Default::default(),
            dims: (24, 80),
            passive: false,
            last_press: None,
        });
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"ship it\r".to_vec(),
        });
        let rows = journal_submits(&dir);
        assert_eq!(rows.len(), 1, "exactly one witness row for one Enter");
        let data = &rows[0]["data"];
        assert_eq!(data["pane"], 7);
        assert_eq!(data["via"], "pane");
        assert_eq!(data["resolution"], "ok");
        assert_eq!(
            data["harness_session"],
            "ccccdddd-1111-2222-3333-444455556666"
        );
        assert_eq!(data["harness"], "claude");
        assert_eq!(data["fno_id"], "t-cccc-worker");

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_relay_claim_bounce_writes_no_witness_row() {
        use crate::server::CoreMsg;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-witness-bounce-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FNO_AGENTS_HOME", &dir);

        let mut core = witness_test_core(7);
        core.clients.push(crate::server::Client {
            id: 1,
            reliable_tx: tokio::sync::mpsc::channel(1).0,
            dirty: Default::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
            synced_modes: Default::default(),
            view: (1, 1),
            visible: Default::default(),
            dims: (24, 80),
            passive: false,
            last_press: None,
        });
        // A live relay holder on the focused pane bounces the keystroke
        // before the touch/witness arm ever runs.
        core.claims.insert(7, std::process::id());
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"ship it\r".to_vec(),
        });
        assert!(
            journal_submits(&dir).is_empty(),
            "a bounced keystroke witnesses nothing"
        );

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unbound_pane_writes_an_unresolved_shell_row() {
        use crate::server::CoreMsg;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-witness-shell-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FNO_AGENTS_HOME", &dir);

        // Pane 9: a squad tab focuses it, but no registry row and no portal
        // bind it - a hand-started harness in a shell pane.
        let mut core = witness_test_core(7);
        core.agents.clear();
        core.session.add_squad(
            2,
            vec!["/fixture".into()],
            None,
            crate::tree::Tab {
                name: None,
                id: 2,
                root: crate::tree::Node::Leaf(9),
                focus: 9,
            },
        );
        core.clients.push(crate::server::Client {
            id: 1,
            reliable_tx: tokio::sync::mpsc::channel(1).0,
            dirty: Default::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
            synced_modes: Default::default(),
            view: (2, 2),
            visible: Default::default(),
            dims: (24, 80),
            passive: false,
            last_press: None,
        });
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"hello\r".to_vec(),
        });
        let rows = journal_submits(&dir);
        assert_eq!(rows.len(), 1);
        let data = &rows[0]["data"];
        assert_eq!(data["via"], "shell");
        assert_eq!(data["resolution"], "unresolved");
        assert!(data["harness_session"].is_null());
        assert!(data["harness"].is_null());
        assert!(data["fno_id"].is_null());

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_burst_arms_the_hold_once_and_a_submit_re_arms() {
        use crate::server::CoreMsg;
        // The final submit fires a real witness append: pin the journal like
        // every other Input-driving test (the FNO_AGENTS_HOME guard), or the
        // append races the guarded tests' env swaps.
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-hold-burst-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FNO_AGENTS_HOME", &dir);
        let mut core = witness_test_core(7);
        core.clients.push(crate::server::Client {
            id: 1,
            reliable_tx: tokio::sync::mpsc::channel(1).0,
            dirty: Default::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
            synced_modes: Default::default(),
            view: (1, 1),
            visible: Default::default(),
            dims: (24, 80),
            passive: false,
            last_press: None,
        });
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"ship".to_vec(),
        });
        let first = *core
            .hold_arm_last
            .get(&7)
            .expect("the first keystroke armed the hold");
        // Ten more bytes inside the window: no re-arm (one spawn per window).
        for _ in 0..10 {
            core.handle(CoreMsg::Input {
                id: 1,
                bytes: b"x".to_vec(),
            });
        }
        assert_eq!(
            *core.hold_arm_last.get(&7).unwrap(),
            first,
            "no re-arm inside the window: one spawn per burst"
        );
        // A submit forces the re-arm the plan gives notify-self's job.
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b" it\r".to_vec(),
        });
        assert!(
            *core.hold_arm_last.get(&7).unwrap() > first,
            "a submit re-armed the hold"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unmapped_pane_arms_no_hold() {
        use crate::server::CoreMsg;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Pane 9: a squad tab focuses it, but no registry row binds it.
        let mut core = witness_test_core(7);
        core.agents.clear();
        core.session.add_squad(
            2,
            vec!["/fixture".into()],
            None,
            crate::tree::Tab {
                name: None,
                id: 2,
                root: crate::tree::Node::Leaf(9),
                focus: 9,
            },
        );
        core.clients.push(crate::server::Client {
            id: 1,
            reliable_tx: tokio::sync::mpsc::channel(1).0,
            dirty: Default::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
            synced_modes: Default::default(),
            view: (2, 2),
            visible: Default::default(),
            dims: (24, 80),
            passive: false,
            last_press: None,
        });
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"hello".to_vec(),
        });
        assert!(
            core.hold_arm_last.is_empty(),
            "a pane no registry row binds arms nothing"
        );
        drop(guard);
    }
}
