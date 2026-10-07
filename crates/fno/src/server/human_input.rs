//! What did a human type into a pane? At its one human-keystroke choke point
//! (`CoreMsg::Input`), the mux writes `human_touch` telemetry and the
//! `operator_submit` / `operator_typing` witnesses.
//! Machine transports (pane send, control.sock mail, codex turn/start)
//! never reach that arm, so a row here is the positive marker that a
//! person typed.

use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::Ordering;
#[cfg(test)]
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

/// Length of the terminal-generated reply at the head of `b`, or 0 when `b`
/// is not one. The classes the loopback delivers as pane input through the
/// client's own terminal: DEC 1004 focus reports (the client mirrors the
/// pane's enablement to the real TTY on every view switch, so switching
/// rows answers a fresh focus-in), CPR `ESC[...R`, DA replies `ESC[?...c` /
/// `ESC[>...c`, DSR `ESC[...n`, kitty keyboard-query replies `ESC[?...u`,
/// OSC replies (BEL- or ST-terminated), and mouse encodings (SGR
/// `ESC[<b;x;yM|m`, X10 `ESC[M` + 3 coord bytes). A keyboard never emits
/// these finals with these param shapes: F1-F4 ride `ESC O P` (second byte
/// `O`, not `[`), kitty KEYBOARD events carry no `?`.
fn terminal_reply_len(b: &[u8]) -> usize {
    if b.len() < 2 || b[0] != 0x1b {
        return 0;
    }
    match b[1] {
        b'[' if b.len() >= 3 && (b[2] == b'I' || b[2] == b'O') => 3,
        b'[' => {
            // CSI: parameter bytes, then one final 0x40-0x7e.
            let mut j = 2;
            let mut saw_private = false;
            let mut saw_left = false;
            while j < b.len() && matches!(b[j], b'0'..=b'9' | b';' | b'?' | b'<' | b'=' | b'>') {
                saw_private |= matches!(b[j], b'?' | b'>');
                saw_left |= b[j] == b'<';
                j += 1;
            }
            if j >= b.len() {
                // Fragment cut at the chunk edge; the rest rides the next
                // chunk, so the tail reads as reply, not keystrokes.
                return b.len();
            }
            if !(0x40..=0x7e).contains(&b[j]) {
                return 0;
            }
            let n = j + 1;
            match b[j] {
                b'R' | b'c' | b'n' => n,
                b'u' if saw_private => n,
                b'M' | b'm' if saw_left => n,
                b'M' => n.saturating_add(3).min(b.len()),
                _ => 0,
            }
        }
        b']' => {
            // OSC reply, BEL- or ST-terminated; an unterminated body is a
            // fragment whose terminator rides the next chunk.
            let mut k = 2;
            while k < b.len() {
                if b[k] == 0x07 {
                    return k + 1;
                }
                if b[k] == 0x1b && b.get(k + 1) == Some(&b'\\') {
                    return k + 2;
                }
                k += 1;
            }
            b.len()
        }
        _ => 0,
    }
}

/// Does this input chunk carry at least one human keystroke? The client
/// forwards terminal-generated replies as pane input, so a chunk written by
/// a mere view switch carries nothing typed; those bytes never arm the
/// attended hold, never emit the touch telemetry, and never witness a
/// submit. The pane write (which already happened upstream) is untouched.
/// ponytail: a reply split across chunks can leave a fragment that reads
/// human and arms once; the fold's one-to-one binding bounds the effect,
/// same caveat `is_submit` carries for pastes.
pub(super) fn has_human_keystroke(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"\x1b[200~") {
            // A bracketed paste is human even when the pasted text itself
            // contains reply-shaped escapes.
            return true;
        }
        let skip = terminal_reply_len(&bytes[i..]);
        if skip == 0 {
            return true;
        }
        i += skip;
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

/// The `operator_typing` journal row (C11 feed): where and when a human typed
/// without pressing Enter, so mail can wait for a live edit instead of
/// landing mid-draft. Same envelope as [`submit_row`]; `typed_ms` replaces
/// `submit_ms`. No typed text is ever recorded. Pure so tests can assert the
/// envelope.
fn typing_row(
    mux_session: &str,
    pane: u64,
    via: &str,
    resolution: &str,
    harness_session: Option<&str>,
    harness: Option<&str>,
    fno_id: Option<&str>,
) -> serde_json::Value {
    let typed_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "operator_typing",
        "source": "daemon",
        "data": {
            "mux_session": mux_session,
            "pane": pane,
            "via": via,
            "typed_ms": typed_ms,
            "resolution": resolution,
            "harness_session": harness_session,
            "harness": harness,
            "fno_id": fno_id,
        }
    })
}

impl Core {
    /// Node id for a `human_touch` event on `pane`: the pane's `FNO_NODE`
    /// provenance, or a node-shaped owning-squad cwd basename. Missing
    /// provenance is preserved as a present-null event field.
    pub(super) fn pane_touch_node(&self, pane: u64) -> Option<String> {
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
        node
    }

    /// Append one coalesced touch row to the agents journal. A failed append
    /// is counted and reported without changing input handling.
    pub(super) fn touch(&mut self, pane: u64, source: &'static str, coalesced: bool) {
        if coalesced && !touch_coalesce(&mut self.touch_last_emit, pane, Instant::now()) {
            return;
        }
        // This emergency switch controls telemetry only, never input witnesses.
        if std::env::var_os("FNO_TOUCH_EMIT").is_some_and(|v| v == "0") {
            return;
        }
        let node = self.pane_touch_node(pane);
        let resolution = if node.is_some() { "ok" } else { "failed" };
        let event = serde_json::json!({
            "ts": crate::review_invocation::review_invocation_timestamp(),
            "type": "human_touch",
            "source": "daemon",
            "data": {
                "graph_node_id": node,
                "source": source,
                "resolution": resolution,
            }
        });
        crate::pane_send_audit::queue_agents_event(
            &crate::pane_send_audit::pane_send_audit_events_path(),
            event,
            Some(&self.touch_emit_failures),
        );
    }

    /// One `operator_submit` witness row for a human Enter on `pane`: bind
    /// the pane to its registry row (mux ref, then attach), else its portal
    /// row, and append one line to the agents journal. No row (a
    /// hand-started harness in a shell pane) writes `via: shell`,
    /// `resolution: unresolved`: the submit still counts, no transcript can
    /// join it. A failed append bumps `touch_emit_failures` and never
    /// touches the keystroke path.
    pub(super) fn witness_submit(&self, pane: u64) {
        let event = self.witness_row(pane, "operator_submit");
        crate::pane_send_audit::queue_agents_event(
            &crate::pane_send_audit::pane_send_audit_events_path(),
            event,
            Some(&self.touch_emit_failures),
        );
    }

    /// One `operator_typing` witness row for a burst of keystrokes with no
    /// Enter on `pane` (C11 feed): the same binding and journal append as
    /// [`Self::witness_submit`], throttled to the touch burst window by the
    /// caller. The human_touch kill switch does not suppress this witness.
    /// The touch map already holds this pane's keystroke instant; the 60s
    /// auto-close guard reads it from there.
    pub(super) fn witness_typing(&self, pane: u64) {
        let event = self.witness_row(pane, "operator_typing");
        crate::pane_send_audit::queue_agents_event(
            &crate::pane_send_audit::pane_send_audit_events_path(),
            event,
            Some(&self.touch_emit_failures),
        );
    }

    /// True when `pane` received operator keystrokes within the last 60
    /// seconds: the auto-close guard's question. The touch map is the
    /// signal (one entry per keystroke burst, purged with the pane), so no
    /// record answers false and a pane the operator never typed into
    /// retires exactly as before.
    pub(super) fn typed_recently(&self, pane: u64) -> bool {
        self.touch_last_emit
            .get(&pane)
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
    }

    /// The witness envelope for `pane` (`operator_submit` or
    /// `operator_typing`): bind the pane to its registry row (mux ref, then
    /// attach), else its portal row.
    fn witness_row(&self, pane: u64, kind: &str) -> serde_json::Value {
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
        match kind {
            "operator_submit" => submit_row(
                &self.session_name,
                pane,
                via,
                resolution,
                harness_session.as_deref(),
                harness.as_deref(),
                fno_id.as_deref(),
            ),
            _ => typing_row(
                &self.session_name,
                pane,
                via,
                resolution,
                harness_session.as_deref(),
                harness.as_deref(),
                fno_id.as_deref(),
            ),
        }
    }

    /// The tail of the `CoreMsg::Input` arm, one call from `handle_msg` so
    /// server.rs only shrinks: touch telemetry, the typing witness and the
    /// submit witness. The mux arms no hold - a keystroke only witnesses a
    /// human Enter (or a live draft, `operator_typing`), and the
    /// conversation hold is armed by the harness prompt hook. A keystroke
    /// here is past the relay guard - PaneSend and relay writes never reach
    /// this - and past the reply classifier: terminal-generated loopback
    /// (focus reports, query replies, mouse encodings) is not steering, so
    /// a viewed row witnesses nothing.
    pub(super) fn input_tail(&mut self, focus: u64, bytes: &[u8]) {
        if !has_human_keystroke(bytes) {
            return;
        }
        // One verdict per burst drives touch, typing witness and the burst
        // window: a submit writes `operator_submit` only (the Enter IS the
        // end of the draft); a non-submit burst writes one `operator_typing`
        // row per [`TOUCH_COALESCE_WINDOW`].
        let submit = is_submit(bytes);
        let burst = touch_coalesce(&mut self.touch_last_emit, focus, Instant::now());
        if burst {
            self.touch(focus, "inject", false);
            if !submit {
                self.witness_typing(focus);
            }
        }
        if submit {
            self.witness_submit(focus);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pane_touch_node_cwd_fallback_and_none() {
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
        assert_eq!(core.pane_touch_node(7).as_deref(), Some("x-cccc"));
        // Unshaped basename: no node; the event carries a present null and failed resolution.
        assert!(core.pane_touch_node(8).is_none());
        assert!(core.pane_touch_node(99).is_none());
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
        journal_rows(dir, "operator_submit")
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
    fn a_focus_report_writes_no_witness_row_and_a_typed_enter_writes_one() {
        use crate::server::CoreMsg;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-witness-replies-{}-{}",
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
        // The loopback the client forwards when a row is merely viewed: the
        // DEC 1004 focus reports, a CPR reply, a DA1 reply, an OSC 11 color
        // reply, an SGR mouse press, a kitty keyboard-query reply.
        for reply in [
            &b"\x1b[I"[..],
            b"\x1b[O",
            b"\x1b[12;34R",
            b"\x1b[?62;1;6;9;15;22c",
            b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07",
            b"\x1b[<0;10;5M",
            b"\x1b[?1u",
        ] {
            core.handle(CoreMsg::Input {
                id: 1,
                bytes: reply.to_vec(),
            });
        }
        assert!(
            journal_submits(&dir).is_empty(),
            "terminal-generated replies witness nothing"
        );
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"hello\r".to_vec(),
        });
        let rows = journal_submits(&dir);
        assert_eq!(
            rows.len(),
            1,
            "one typed line with Enter writes exactly one witness row"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pinned-journal env for the typing-witness tests: the
    /// `FNO_AGENTS_HOME_GUARD` plus a fresh temp agents home.
    fn witness_env(tag: &str) -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-typing-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FNO_AGENTS_HOME", &dir);
        (guard, dir)
    }

    fn journal_rows(dir: &Path, event_type: &str) -> Vec<serde_json::Value> {
        assert!(crate::pane_send_audit::flush_agents_journal(
            Duration::from_secs(10)
        ));
        crate::event_store::query_events(
            &dir.join("events.jsonl"),
            &crate::event_store::EventQuery::of_types(&[event_type]),
        )
        .unwrap_or_default()
        .iter()
        .filter_map(|row| serde_json::from_str(&row.line).ok())
        .collect()
    }

    fn typing_client_core() -> super::super::Core {
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
        core
    }

    #[test]
    fn a_typing_burst_without_enter_writes_one_operator_typing_row() {
        use crate::server::CoreMsg;
        let (guard, dir) = witness_env("burst");
        let mut core = typing_client_core();
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"abc".to_vec(),
        });
        let rows = journal_rows(&dir, "operator_typing");
        assert_eq!(rows.len(), 1, "one non-submit burst, one typing row");
        let data = &rows[0]["data"];
        assert_eq!(data["pane"], 7);
        assert_eq!(data["via"], "pane");
        assert_eq!(data["resolution"], "ok");
        assert_eq!(
            data["harness_session"],
            "ccccdddd-1111-2222-3333-444455556666"
        );
        assert!(
            data["typed_ms"].as_u64().is_some(),
            "typed_ms is the millisecond join key"
        );
        assert!(
            journal_rows(&dir, "operator_submit").is_empty(),
            "no Enter, no submit row"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_key_of_a_burst_writes_one_human_touch_row() {
        use crate::server::CoreMsg;
        let (guard, dir) = witness_env("human-touch");
        let mut core = typing_client_core();
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"ship it\r".to_vec(),
        });

        let rows = journal_rows(&dir, "human_touch");
        assert_eq!(
            rows.len(),
            1,
            "one touch event for the first key in a burst"
        );
        assert_eq!(rows[0]["type"], "human_touch");
        assert_eq!(rows[0]["source"], "daemon");
        assert_eq!(rows[0]["data"]["source"], "inject");
        assert_eq!(rows[0]["data"]["resolution"], "failed");
        assert_eq!(rows[0]["data"]["graph_node_id"], serde_json::Value::Null);

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_human_touch_append_is_counted_without_panicking() {
        use crate::server::CoreMsg;
        let (guard, dir) = witness_env("human-touch-failure");
        let blocked_home = dir.join("file");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&blocked_home, b"not a directory").unwrap();
        std::env::set_var("FNO_AGENTS_HOME", &blocked_home);
        let mut core = typing_client_core();
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"ship it\r".to_vec(),
        });
        assert!(crate::pane_send_audit::flush_agents_journal(
            Duration::from_secs(10)
        ));

        assert_eq!(
            core.touch_emit_failures.load(Ordering::Relaxed),
            2,
            "both human_touch and operator_submit append failures are counted"
        );

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_bursts_within_the_window_write_one_typing_row() {
        use crate::server::CoreMsg;
        let (guard, dir) = witness_env("coalesce");
        let mut core = typing_client_core();
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"a".to_vec(),
        });
        std::thread::sleep(std::time::Duration::from_secs(1));
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"b".to_vec(),
        });
        assert_eq!(
            journal_rows(&dir, "operator_typing").len(),
            1,
            "bursts inside the coalesce window ride one row"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_burst_after_the_window_writes_a_second_typing_row() {
        use crate::server::CoreMsg;
        let (guard, dir) = witness_env("window");
        let mut core = typing_client_core();
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"a".to_vec(),
        });
        std::thread::sleep(super::TOUCH_COALESCE_WINDOW + std::time::Duration::from_secs(1));
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"b".to_vec(),
        });
        assert_eq!(
            journal_rows(&dir, "operator_typing").len(),
            2,
            "a burst past the window opens a new one"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_submit_writes_operator_submit_and_no_typing_row() {
        use crate::server::CoreMsg;
        let (guard, dir) = witness_env("submit");
        let mut core = typing_client_core();
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"abc\r".to_vec(),
        });
        assert_eq!(journal_rows(&dir, "operator_submit").len(), 1);
        assert!(
            journal_rows(&dir, "operator_typing").is_empty(),
            "the Enter ends the draft: submit rows only"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_focus_report_writes_neither_typing_nor_submit_rows() {
        use crate::server::CoreMsg;
        let (guard, dir) = witness_env("focus");
        let mut core = typing_client_core();
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"\x1b[I".to_vec(),
        });
        assert!(journal_rows(&dir, "operator_typing").is_empty());
        assert!(journal_rows(&dir, "operator_submit").is_empty());
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_replies_never_eat_human_keystrokes() {
        // The classifier's false-positive class: sequences a keyboard
        // legitimately produces that sit next to the reply shapes.
        assert!(super::has_human_keystroke(b"a"));
        assert!(super::has_human_keystroke(b"\x1b[A"), "up arrow");
        assert!(super::has_human_keystroke(b"\x1b[3~"), "delete");
        assert!(
            super::has_human_keystroke(b"\x1bOR"),
            "F3 rides ESC O, not ESC ["
        );
        assert!(
            super::has_human_keystroke(b"\x1b[97;5u"),
            "kitty keyboard input has no ?"
        );
        assert!(super::has_human_keystroke(b"\x1b"), "a lone Esc");
        assert!(
            super::has_human_keystroke(b"\x1b[200~line one\r\n\x1b[6n\x1b[201~"),
            "a paste is human even when its text contains reply shapes"
        );
        // The pure reply classes, whole-chunk and mixed with real keys.
        assert!(!super::has_human_keystroke(b"\x1b[I"));
        assert!(
            !super::has_human_keystroke(b"\x1b[M#*%"),
            "an X10 mouse encoding carries three coord bytes"
        );
        assert!(!super::has_human_keystroke(b"\x1b[12;34R\x1b[I"));
        assert!(
            super::has_human_keystroke(b"\x1b[Ia"),
            "a mixed chunk still carries the typed a"
        );
    }

    #[test]
    fn a_portal_seated_panes_enter_writes_the_thread_witness_row() {
        use crate::server::CoreMsg;
        use crate::thread_viewer::Portal;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-witness-portal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FNO_AGENTS_HOME", &dir);
        let mut core = witness_test_core(7);
        core.agents = vec![crate::agents_view::RegistryAgent {
            name: "thread".into(),
            cwd: "/fixture".into(),
            mux: None,
            harness: Some("codex".into()),
            session_id: Some("thread-id".into()),
            harness_session_id: Some("dddddddd-1112-2222-3333-444455556666".into()),
            ..Default::default()
        }];
        core.portals.insert(
            0,
            Portal {
                row_key: "thread".into(),
                seat: 7,
                tab: 5,
            },
        );
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
            bytes: b"hello\r".to_vec(),
        });
        let rows = journal_submits(&dir);
        assert_eq!(rows.len(), 1, "one Enter, one witness row");
        let data = &rows[0]["data"];
        assert_eq!(data["via"], "portal");
        assert_eq!(data["resolution"], "ok");
        assert_eq!(
            data["harness_session"], "dddddddd-1112-2222-3333-444455556666",
            "the thread's harness_session joins the portal seat"
        );

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
