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

/// The attended hold refresh throttle: an arm older than this re-arms on
/// the next keystroke, so a burst that outlasts the five-minute clock gets
/// its deadline moved before the release timer can lift the hold under a
/// still-typing operator. One spawn a minute under continuous typing; a
/// sub-minute burst stays one spawn.
const ATTENDED_HOLD_REFRESH: Duration = Duration::from_secs(60);

/// Arm now? Pure so tests drive the burst arithmetic: the first keystroke
/// ever, or one past the refresh throttle since the last arm.
fn hold_arm_due(last: Option<Instant>, now: Instant) -> bool {
    match last {
        None => true,
        Some(t) => now.saturating_duration_since(t) >= ATTENDED_HOLD_REFRESH,
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

/// The class of a submitted line, after escape sequences and leading
/// whitespace are stripped. The attended auto-hold arms on `Message` only
/// (x-5198 R2/R3): a slash command, a `!` shell line, and an empty Enter
/// never count as conversation, and any of them resets the streak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Empty,
    Slash,
    Shell,
    Message,
}

fn line_kind(line: &[u8]) -> LineKind {
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            0x1b => {
                let skip = terminal_reply_len(&line[i..]);
                // A reply shape skips whole; any other escape skips ESC
                // plus one byte (a meta pair or a two-byte sequence).
                i += if skip > 0 {
                    skip
                } else if line[i..].starts_with(b"\x1b[") {
                    let fin = line[i + 2..]
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                        .map(|p| i + 2 + p + 1)
                        .unwrap_or(line.len());
                    fin
                } else {
                    (i + 2).min(line.len())
                };
            }
            b if b.is_ascii_whitespace() => i += 1,
            b'/' => return LineKind::Slash,
            b'!' => return LineKind::Shell,
            _ => return LineKind::Message,
        }
    }
    LineKind::Empty
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

    /// Arm (or, on a submit, re-arm) the pane session's mail hold.
    /// The one arm source that sees the keystrokes: a session the registry
    /// carries holds delivery while its operator types and drains as one
    /// digest at the idle clock. The arm re-arms past the refresh throttle
    /// (`hold_arm_due`), so a long burst keeps its clock ahead of the
    /// release timer; a pane with no session mapping does nothing. The
    /// spawn runs off-loop and never blocks the keystroke.
    pub(super) fn arm_attended_hold(&mut self, pane: u64, force: bool) {
        let now = Instant::now();
        if !force && !hold_arm_due(self.hold_arm_last.get(&pane).copied(), now) {
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
        // A portal-hosted thread has no mux/attach mapping; the same
        // fallback the submit witness uses resolves it.
        let portal_bound = bound
            .is_none()
            .then(|| crate::thread_viewer::row_for_pane(&self.portals, pane, &self.agents))
            .flatten();
        let Some(session) = bound
            .or(portal_bound)
            .and_then(|a| a.harness_session_id.clone())
        else {
            return;
        };
        self.hold_arm_last.insert(pane, now);
        spawn_hold_arm(&session);
    }

    /// The tail of the `CoreMsg::Input` arm, one call from `handle_msg` so
    /// server.rs only shrinks: touch telemetry, the attended hold, the
    /// submit witness. A keystroke here is past the relay guard - PaneSend
    /// and relay writes never reach this - and past the reply classifier:
    /// terminal-generated loopback (focus reports, query replies, mouse
    /// encodings) is not steering, so a viewed row arms nothing. Under the
    /// attended auto-hold ruling (x-5198 R2) the hold arms only on a real
    /// conversation: the SECOND consecutive Enter on a non-empty text line.
    /// Slash, shell and empty lines break the streak; focus alone never arms.
    pub(super) fn input_tail(&mut self, focus: u64, bytes: &[u8]) {
        if !has_human_keystroke(bytes) {
            return;
        }
        self.touch(focus, "inject", true);
        if !is_submit(bytes) {
            self.line_buf
                .entry(focus)
                .or_default()
                .extend_from_slice(bytes);
            return;
        }
        self.witness_submit(focus);
        let mut line = self.line_buf.remove(&focus).unwrap_or_default();
        let cr = bytes.iter().position(|b| *b == 0x0d).unwrap_or(bytes.len());
        line.extend_from_slice(&bytes[..cr]);
        if let Some(rest) = bytes.get(cr + 1..) {
            self.line_buf
                .entry(focus)
                .or_default()
                .extend_from_slice(rest);
        }
        if line_kind(&line) == LineKind::Message {
            let streak = self.real_streak.entry(focus).or_default();
            *streak = (*streak).min(1) + 1;
            if *streak >= 2 {
                self.arm_attended_hold(focus, true);
            }
        } else {
            self.real_streak.remove(&focus);
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
            "a keystroke inside the throttle coalesces into the burst the arm covers"
        );
        assert!(
            hold_arm_due(Some(t0), t0 + ATTENDED_HOLD_REFRESH),
            "one past the refresh throttle a new burst re-arms"
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
    fn two_real_messages_arm_and_later_ones_re_arm() {
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
        assert!(
            core.hold_arm_last.is_empty(),
            "composition alone never arms: only a real conversation does"
        );
        // The first real message: no arm yet (the R2 two-message rule).
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b" it\r".to_vec(),
        });
        assert!(
            core.hold_arm_last.is_empty(),
            "one real message never arms: the hold wants a conversation"
        );
        // The second consecutive real message arms.
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"ship it\r".to_vec(),
        });
        let first = *core
            .hold_arm_last
            .get(&7)
            .expect("the second consecutive real message armed the hold");
        // The next real message re-arms at once (force), so a running
        // conversation keeps its clock ahead of the five-minute lift.
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"onward\r".to_vec(),
        });
        assert!(
            *core.hold_arm_last.get(&7).unwrap() > first,
            "a later real message re-armed the hold"
        );
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replies_and_one_message_arm_nothing_two_consecutive_ones_arm() {
        use crate::server::CoreMsg;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-hold-replies-{}-{}",
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
            core.hold_arm_last.is_empty(),
            "terminal-generated replies never arm the attended hold"
        );
        // One real message does not arm either (the R2 two-message rule).
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"a\r".to_vec(),
        });
        assert!(
            core.hold_arm_last.is_empty(),
            "one real message never arms the hold"
        );
        // The second consecutive real message arms.
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"b\r".to_vec(),
        });
        assert!(
            core.hold_arm_last.contains_key(&7),
            "two consecutive real messages arm the hold"
        );
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
    fn a_portal_bound_pane_arms_through_the_portal_fallback() {
        use crate::server::CoreMsg;
        use crate::thread_viewer::Portal;
        let mut core = witness_test_core(7);
        core.agents = vec![crate::agents_view::RegistryAgent {
            name: "thread".into(),
            cwd: "/fixture".into(),
            mux: None,
            harness: Some("codex".into()),
            session_id: Some("thread-id".into()),
            harness_session_id: Some("dddddddd-1111-2222-3333-444455556666".into()),
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
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"world\r".to_vec(),
        });
        assert!(
            core.hold_arm_last.contains_key(&7),
            "a portal-seated pane arms its row's hold through the same fallback the witness uses"
        );
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
            bytes: b"hello\r".to_vec(),
        });
        core.handle(CoreMsg::Input {
            id: 1,
            bytes: b"world\r".to_vec(),
        });
        assert!(
            core.hold_arm_last.is_empty(),
            "a pane no registry row binds arms nothing"
        );
        drop(guard);
    }

    #[test]
    fn a_slash_shell_or_empty_line_resets_the_streak() {
        use crate::server::CoreMsg;
        let guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-hold-streak-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FNO_AGENTS_HOME", &dir);
        // Each case runs two real messages with the breaking line between
        // them; none may arm, because the streak never reaches two.
        for breaker in [&b"/compact\r"[..], b"!ls\r", b"\r"] {
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
                bytes: b"hello\r".to_vec(),
            });
            core.handle(CoreMsg::Input {
                id: 1,
                bytes: breaker.to_vec(),
            });
            core.handle(CoreMsg::Input {
                id: 1,
                bytes: b"world\r".to_vec(),
            });
            assert!(
                core.hold_arm_last.is_empty(),
                "a breaking line reset the streak; breaker: {breaker:?}"
            );
        }
        std::env::remove_var("FNO_AGENTS_HOME");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_kind_classes_the_submitted_line() {
        assert_eq!(super::line_kind(b""), super::LineKind::Empty);
        assert_eq!(super::line_kind(b"   "), super::LineKind::Empty);
        assert_eq!(super::line_kind(b"/compact"), super::LineKind::Slash);
        assert_eq!(super::line_kind(b"  /compact"), super::LineKind::Slash);
        assert_eq!(super::line_kind(b"!ls -la"), super::LineKind::Shell);
        assert_eq!(super::line_kind(b"hello there"), super::LineKind::Message);
        assert_eq!(super::line_kind(b"\x1b[Ihello"), super::LineKind::Message);
        assert_eq!(
            super::line_kind(b"\x1b[12;34Rworld"),
            super::LineKind::Message
        );
        assert_eq!(
            super::line_kind(b"\x1bOA big idea"),
            super::LineKind::Message
        );
    }
}
