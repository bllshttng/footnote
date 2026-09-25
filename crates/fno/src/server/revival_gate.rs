//! May this mux relaunch of a dead worker session spawn? Every in-process
//! revival - the sideline resume gesture, the held-pane focus, the bulk
//! workspace restore - asks the one spawn gate the way every non-Rust door
//! does, over the `fno-agents spawn-gate` verb, charged to the revived
//! row's own parent. `crates/fno` never links `fno-agents`, so this module
//! owns that subprocess crossing. An unanswered gate never admits: a
//! missing verb, a timeout, or an unparseable answer is a refusal.

use std::time::Duration;

use serde_json::{json, Value};

use super::*;

/// The gate's own queue timeout is 600s; the ask bounds past it so the
/// verb's exit, not the transport, decides the wait.
const GESTURE_TIMEOUT: Duration = Duration::from_secs(660);
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// A staged admission survives the claude-plan and codex-argv replays of
/// the SAME gesture and is consumed at the pane spawn; past this it is
/// stale and the next gesture asks again.
const ADMISSION_TTL: Duration = Duration::from_secs(120);

#[cfg(test)]
thread_local! {
    /// Test override for the gate ask: `None` admits every check without a
    /// subprocess (the existing server and restore tests stand), a
    /// `Refuse` answers the ask with that refusal at once, and `Ask`
    /// records the ask instead of spawning a process.
    static GATE_OVERRIDE: std::cell::RefCell<Option<GateOverride>> =
        const { std::cell::RefCell::new(None) };
    /// The asks `GateOverride::Ask` recorded, taken by tests.
    static ASK_LOG: std::cell::RefCell<Vec<AskRecord>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(super) enum GateOverride {
    Refuse(String),
    Ask,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(super) struct AskRecord {
    pub(super) name: String,
    pub(super) caller_session: Option<String>,
    pub(super) account: Option<String>,
}

#[cfg(test)]
pub(super) fn set_gate_override(override_: GateOverride) {
    GATE_OVERRIDE.with(|p| *p.borrow_mut() = Some(override_));
}

#[cfg(test)]
pub(super) struct GateOverrideGuard;

#[cfg(test)]
impl Drop for GateOverrideGuard {
    fn drop(&mut self) {
        GATE_OVERRIDE.with(|p| *p.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(super) fn take_asks() -> Vec<AskRecord> {
    ASK_LOG.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// Whether the override makes a check answer in-process (a `Refuse`) or
/// record without a subprocess (an `Ask`). Tests without the override and
/// every production build read `false`.
#[cfg(test)]
pub(super) fn gate_ask_live() -> bool {
    GATE_OVERRIDE.with(|p| {
        p.borrow()
            .as_ref()
            .is_some_and(|o| matches!(o, GateOverride::Ask | GateOverride::Refuse(_)))
    })
}

/// The `gate`-mode payload one revival asks with. `hold: false` makes the
/// verb release the mutex before it answers: a long-lived server holds
/// nothing. `caller_session` and `account` come from the revived row
/// itself, never from whoever clicked.
pub(super) fn gate_payload(
    name: &str,
    caller_session: Option<&str>,
    account: Option<&str>,
    no_wait: bool,
    holder_pid: u32,
) -> Value {
    json!({
        "mode": "gate",
        "name": name,
        "substrate": "pane",
        "no_wait": no_wait,
        "hold": false,
        "account": account,
        "caller_session": caller_session,
        "holder_pid": holder_pid,
    })
}

/// The gate's answer: `admitted` passes; a refusal carries the verdict line
/// from stderr (the receipt's reason when no verdict line is present) plus
/// the one-run CLI escape; an unparseable answer is `spawn gate
/// unavailable`, never an admit.
pub(super) fn read_gate_answer(stdout: &str, stderr: &str, name: &str) -> Result<(), String> {
    let answer: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("spawn gate unavailable: unparseable answer: {e}"))?;
    match answer.get("status").and_then(Value::as_str) {
        Some("admitted") => Ok(()),
        Some("refused") => {
            let detail = crate::dispatch_launch::refusal_detail(stderr, "");
            let detail = if detail.is_empty() {
                answer
                    .pointer("/receipt/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("refused")
                    .to_string()
            } else {
                detail
            };
            Err(format!(
                "{detail}; resume it once past the gate with FNO_SPAWN_GATE=0 fno agents resume {name}"
            ))
        }
        other => Err(format!(
            "spawn gate unavailable: unexpected status {other:?}"
        )),
    }
}

/// The headroom a probe granted: how many workers may still spawn, and the
/// reading it came from, so a refusal at zero can name both numbers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProbeHeadroom {
    pub(crate) left: usize,
    pub(crate) slots: usize,
    pub(crate) cap: usize,
}

/// A probe answer with no slot reading, for a dry run and for the tests
/// that drive the apply half without a gate.
pub(super) fn unbounded_headroom() -> ProbeHeadroom {
    ProbeHeadroom {
        left: usize::MAX,
        slots: 0,
        cap: 0,
    }
}

/// The restore's one capacity reading: `accepted` yields the headroom,
/// `refused` yields the gate's own message (a RAM floor breach refuses the
/// whole restore with it), anything else is unreadable.
pub(super) fn read_probe_headroom(stdout: &str) -> Result<ProbeHeadroom, String> {
    let answer: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("spawn gate unreadable: unparseable answer: {e}"))?;
    match answer.get("verdict").and_then(Value::as_str) {
        Some("accepted") => {
            let slots = answer.get("slots").and_then(Value::as_u64).unwrap_or(0) as usize;
            let cap = answer.get("max_live").and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(ProbeHeadroom {
                left: cap.saturating_sub(slots),
                slots,
                cap,
            })
        }
        Some("refused") => Err(format!(
            "spawn gate: {}",
            answer
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("refused")
        )),
        other => Err(format!(
            "spawn gate unreadable: verdict {other:?}: {}",
            answer
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason given")
        )),
    }
}

/// One bounded `fno-agents spawn-gate` round trip OFF the core loop: the
/// payload on stdin, the answer on stdout, the gate's prose on stderr.
/// `Err` is a missing verb, a timeout, or a dead pipe - the caller refuses.
async fn ask(payload: &Value, timeout: Duration) -> Result<(String, String), String> {
    use tokio::io::AsyncWriteExt;
    let mut command = agent_actions::mux_command(crate::digest_overlay::fno_agents_bin());
    command.arg("spawn-gate");
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("fno-agents spawn-gate: {e}"))?;
    {
        let Some(mut stdin) = child.stdin.take() else {
            return Err("fno-agents spawn-gate: no stdin".into());
        };
        stdin
            .write_all(payload.to_string().as_bytes())
            .await
            .map_err(|e| format!("fno-agents spawn-gate: {e}"))?;
    }
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
        .map_err(|e| format!("fno-agents spawn-gate: {e}"))?;
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// The restore's one read-only probe.
pub(super) async fn ask_probe(timeout: Duration) -> Result<String, String> {
    let payload = json!({ "mode": "probe" });
    let (stdout, _) = ask(&payload, timeout).await?;
    Ok(stdout)
}

/// The restore's one capacity read: probe, then parse. A transport fault
/// reads `spawn gate unavailable`, a refusal reads `spawn gate: <message>`,
/// and both refuse the whole restore in the apply half.
pub(super) async fn probe_headroom() -> Result<ProbeHeadroom, String> {
    let stdout = ask_probe(PROBE_TIMEOUT)
        .await
        .map_err(|e| format!("spawn gate unavailable: {e}"))?;
    read_probe_headroom(&stdout)
}

impl super::Core {
    /// The revival gate check every in-process relaunch runs before it
    /// resolves a re-entry plan or an argv. `Some(Ok(()))` when a fresh
    /// admission for this worker is staged (the replays of the same gesture
    /// re-enter here); `Some(Err)` is the refusal; `None` means the ask
    /// fired off the core loop and the caller must wait for
    /// [`super::CoreMsg::RevivalGateAnswered`] to replay it.
    pub(super) fn revival_admitted(
        &mut self,
        client_id: u64,
        facts: &HeldWorker,
        replay: ResumeReplay,
    ) -> Option<Result<(), String>> {
        #[cfg(test)]
        match GATE_OVERRIDE.with(|p| p.borrow().clone()) {
            None => return Some(Ok(())),
            Some(GateOverride::Refuse(reason)) => return Some(Err(reason)),
            Some(GateOverride::Ask) => {}
        }
        if let Some((staged, at)) = &self.revival_admission {
            if *staged == facts.name && at.elapsed() < ADMISSION_TTL {
                return Some(Ok(()));
            }
        }
        // The charge rides the revived row: its own recorded parent and
        // account, whoever clicked.
        let row = self.agents.iter().find(|a| {
            a.harness.as_deref() == Some(facts.harness.as_str())
                && agent_harness_session_id(a) == Some(facts.harness_session_id.as_str())
        });
        let caller_session = row
            .and_then(|r| r.spawned_by_session.clone())
            .filter(|s| !s.is_empty());
        let account = row
            .and_then(|r| r.account.clone())
            .filter(|s| !s.is_empty());
        self.notice(
            client_id,
            format!("resume {}: waiting for the spawn gate", facts.name),
        );
        #[cfg(test)]
        if gate_ask_live() {
            ASK_LOG.with(|p| {
                p.borrow_mut().push(AskRecord {
                    name: facts.name.clone(),
                    caller_session,
                    account,
                })
            });
            return None;
        }
        self.ask_gate_off_loop(
            client_id,
            facts,
            caller_session.as_deref(),
            account.as_deref(),
            replay,
        );
        None
    }

    /// The off-loop `gate`-mode ask; the answer lands as
    /// [`super::CoreMsg::RevivalGateAnswered`].
    fn ask_gate_off_loop(
        &self,
        client_id: u64,
        facts: &HeldWorker,
        caller_session: Option<&str>,
        account: Option<&str>,
        replay: ResumeReplay,
    ) {
        let payload = gate_payload(
            &facts.name,
            caller_session,
            account,
            false,
            std::process::id(),
        );
        let core_tx = self.self_tx.clone();
        let name = facts.name.clone();
        tokio::spawn(async move {
            let verdict = match ask(&payload, GESTURE_TIMEOUT).await {
                Err(detail) => Err(format!("spawn gate unavailable: {detail}")),
                Ok((stdout, stderr)) => read_gate_answer(&stdout, &stderr, &name),
            };
            let _ = core_tx
                .send(super::CoreMsg::RevivalGateAnswered {
                    id: client_id,
                    name,
                    verdict,
                    replay: Box::new(replay),
                })
                .await;
        });
    }

    /// The gate answered: a refusal notices and stops (the held pane stays
    /// held, so a retry after a worker finishes is one click); an admission
    /// is staged and the SAME command re-dispatched, the
    /// `ResumeArgvReady` shape.
    pub(super) fn on_revival_gate_answered(
        &mut self,
        id: u64,
        name: String,
        verdict: Result<(), String>,
        replay: Box<ResumeReplay>,
    ) {
        match verdict {
            Err(reason) => {
                // The held seat itself names the refusal; it stays held, so
                // a retry after a worker finishes is one click.
                if let ResumeReplay::Held { pid } = *replay {
                    self.write_restore_message(pid, &format!("{name} was not resumed: {reason}"));
                }
                self.notice(id, format!("resume {name} refused: {reason}"));
            }
            Ok(()) => {
                self.revival_admission = Some((name, std::time::Instant::now()));
                match *replay {
                    ResumeReplay::Gesture { name } => {
                        self.command(id, super::Command::ResumeAgent { name });
                    }
                    ResumeReplay::Held { pid } => {
                        self.command(id, super::Command::FocusPane(pid));
                    }
                }
            }
        }
    }

    /// The one spawn site's admission check, so a future caller cannot skip
    /// the gate by reaching the spawn directly.
    pub(super) fn take_revival_admission(&mut self, name: &str) -> Result<(), String> {
        #[cfg(test)]
        if !gate_ask_live() {
            // Tests without the override admit; a staged admission is
            // still spent, so a staging test sees the consumption.
            self.revival_admission = None;
            return Ok(());
        }
        if let Some((staged, at)) = &self.revival_admission {
            if *staged == name && at.elapsed() < ADMISSION_TTL {
                self.revival_admission = None;
                return Ok(());
            }
        }
        Err(format!("spawn gate not asked for {name}"))
    }
}

#[cfg(test)]
#[path = "tests/revival_gate_tests.rs"]
mod tests;
