//! What a family-1 worker is actually doing, read once per answer.
//!
//! The one transcript reader is [`crate::session_truth`], native in this
//! crate: the probe resolves the handle, reads the tail through per-file
//! cursors, and decodes the answer through the same [`parse_truth_payload`]
//! the wire always used, so the Rust list emitter reports the identical
//! reading the Python `fno agents truth` verb renders. No `fno-py` child
//! starts on this path.
//!
//! Moved out of `claude_ask` because it answers a different question. That file
//! is the `claude --bg` ask path; this one is "what is this session doing", and
//! its callers (`wait`, `needs`, `org_board`, the daemon's list rows) reach it
//! without going near an ask.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// One family-1 truth answer: the supervision state, plus the model the worker
/// is ACTUALLY answering as.
///
/// Both come from the SAME probe. `observed_model` is derived by the one
/// reader (`crate::session_truth`) from the worker's own transcript, so every
/// surface reports the identical reading instead of growing a second reader
/// that could drift from it.
#[derive(Debug, Clone)]
pub struct TruthProbe {
    pub state: String,
    /// The shared reachability verdict (`reachable` / `unreachable` /
    /// `unknown`), derived Python-side with the falsifiers applied. `None` on a
    /// truth build that predates the field.
    ///
    /// Prefer this over mapping [`Self::state`]: `state` is transcript activity
    /// alone, so a session whose process died minutes ago still reads
    /// `working` and renders live. That two-hour blind spot is the whole bug.
    pub reachability: Option<String>,
    /// The evidence [`Self::reachability`] was reached from (`transcript` /
    /// `process-gone` / `pane-gone` / `silent` / `no-evidence`), and how old that
    /// evidence is.
    ///
    /// Carried so the daemon's row can re-emit the whole triple. A verdict
    /// without its basis is the shape this module exists to retire: it renders
    /// as a bare word and a reader cannot tell a positive transcript reading
    /// from a fired falsifier.
    pub basis: Option<String>,
    pub last_activity_age_s: Option<f64>,
    /// The instrument the activity age came from (`last-entry` | `mtime` |
    /// `opencode-db`), or the resolver's reason word (`not-found` |
    /// `no-records` | `resolver-error`) when it could not resolve the handle
    /// at all. Those three are the difference between "this worker
    /// has no transcript" and "the resolver crashed", and both rendered as
    /// the same blank before. `None` on a truth build that predates the
    /// field: absence renders as absence.
    pub last_activity_basis: Option<String>,
    /// The absolute ISO8601 stamp of the newest transcript activity, and the
    /// flattened text of the LAST turn (compact `[tool_use: name]` markers
    /// included, capped at 200 chars Python-side). Derived by the same probe as
    /// the age; `None` when the probe did not answer, which is never the same
    /// claim as "nothing happened".
    pub last_event_at: Option<String>,
    pub last_message: Option<String>,
    pub observed_model: serde_json::Value,
    /// The error taxonomy's class for a last assistant turn that is a provider
    /// refusal (`provider_4xx_quota` and its siblings), classified Python-side
    /// by the one classifier recovery already trusts. `None` on a healthy row
    /// AND on a truth build that predates the field, so an older `fno` renders
    /// exactly as it did before.
    ///
    /// Needed because the refusal record is the NEWEST transcript entry: a
    /// worker killed by a usage-limit 429 dies writing that error, so
    /// [`Self::last_activity_age_s`] reads freshest at the moment it died and
    /// the row renders `writing` (measured 2026-09-11: 469 s, status writing).
    pub provider_refusal: Option<String>,
    /// The title the HARNESS carries for this session (claude's Ctrl+R
    /// agent-name record; codex/opencode's index title), read Python-side by
    /// the same probe so the list emitter never grows a second title reader.
    /// `None` = the harness carries none or the probe predates the field:
    /// absence renders as absence, never as the row's label.
    pub harness_title: Option<String>,
}

pub fn family1_truth_probe(handle: &str) -> Option<TruthProbe> {
    family1_truth_probe_many(&[handle.to_string()]).remove(handle)
}

/// The bounded drain for an EXITED child whose pipes may still be held by a
/// grandchild: both pipes are read to EOF on their own threads, the bytes
/// come back over a channel rather than a join (a join is its own unbounded
/// wait - a grandchild inherits the fds and outlives the child), and `grace`
/// bounds that wait. Returns the lossy-UTF-8 texts of stdout and stderr,
/// trimmed, for the removal cascade to fold into its refusal.
pub fn drain_to_detail(child: &mut std::process::Child, grace: Duration) -> String {
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = out_pipe.as_mut() {
            let _ = std::io::Read::read_to_end(pipe, &mut buf);
        }
        let _ = out_tx.send(buf);
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = err_pipe.as_mut() {
            let _ = std::io::Read::read_to_end(pipe, &mut buf);
        }
        let _ = err_tx.send(buf);
    });
    let out = out_rx.recv_timeout(grace).unwrap_or_default();
    let err = err_rx.recv_timeout(grace).unwrap_or_default();
    let stdout = String::from_utf8_lossy(&out);
    let stderr = String::from_utf8_lossy(&err);
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => "stderr and stdout were both empty".to_string(),
        (true, false) => stderr.trim().to_string(),
        (false, true) => stdout.trim().to_string(),
        (false, false) => format!("stderr: {}; stdout: {}", stderr.trim(), stdout.trim()),
    }
}

/// Probe family-1 truth within a caller-supplied total budget. The reader
/// is in process now, so the budget almost never binds; it stays the
/// deadline because callers hand one down and the contract is unchanged.
pub fn family1_truth_probe_with_timeout(handle: &str, timeout: Duration) -> Option<TruthProbe> {
    let deadline = Instant::now() + timeout;
    family1_truth_probe_many_measured_within(&[handle.to_string()], Some(deadline))
        .0
        .remove(handle)
}

/// The transcript state, LOWERED to `"unreachable"` when the shared verdict
/// affirmatively falsified the row.
///
/// `resume` and the attach pointer read this and match on `working | watching |
/// your-move` to decide "is live". Transcript state alone cannot see a dead
/// process, so a session whose process died forty minutes ago still reads
/// `working` and resume wakes/attaches to nothing. Passing the falsified case through
/// as a state neither arm matches drops both callers into their inconclusive
/// branch (refuse / no pointer), which is the safe answer.
///
/// The override is MONOTONE, matching `fno.agents.reachability`: it only ever
/// lowers a would-be-live reading, never raises `done`/`stalled` toward live and
/// never invents a verdict when the probe did not carry one.
pub fn family1_truth_state(handle: &str) -> Option<String> {
    let probe = family1_truth_probe(handle)?;
    Some(lower_state_with_verdict(&probe.state, probe.reachability.as_deref()).to_string())
}

fn lower_state_with_verdict<'a>(state: &'a str, reachability: Option<&str>) -> &'a str {
    if reachability == Some("unreachable") && matches!(state, "working" | "watching" | "your-move")
    {
        return "unreachable";
    }
    state
}

/// Variant of [`family1_truth_state`] for the resume smart verb. The shared
/// lowering renders a gone process as `"unreachable"` (matches no arm, so resume
/// refuses it as inconclusive - the safe answer for the mail path). Resume wants
/// the opposite for a worker the verdict confirms is DEAD: relaunch it. So an
/// `unreachable` verdict whose `basis` is the process being gone (`pane-gone` /
/// `process-gone`) lowers to `"stalled"` and resume's `done | stalled` relaunch
/// arm fires. A `silent` / `no-evidence` unreachable stays `"unreachable"`
/// (inconclusive): the process may still be alive, and relaunching would open a
/// second writer on one transcript.
///
/// Separate from [`family1_truth_state`] because routing a gone worker to
/// relaunch is a resume-specific call; the mail path's orphan-reason reader
/// (`family1_orphan_reason`) shares the probe but must not change with it.
pub fn family1_truth_state_for_resume(handle: &str) -> Option<String> {
    let probe = family1_truth_probe(handle)?;
    Some(
        lower_state_for_resume(
            &probe.state,
            probe.reachability.as_deref(),
            probe.basis.as_deref(),
        )
        .to_string(),
    )
}

/// [`lower_state_with_verdict`] plus one rule: a live-seeming state the verdict
/// falsified with evidence the PROCESS is gone (not merely silent) is dead, so
/// resume relaunches it. Falls through to the shared lowering for every other
/// case, so reachable-working stays live and a pre-verdict probe build is
/// unchanged.
fn lower_state_for_resume<'a>(
    state: &'a str,
    reachability: Option<&str>,
    basis: Option<&str>,
) -> &'a str {
    if reachability == Some("unreachable")
        && matches!(
            basis,
            Some("pane-gone") | Some("process-gone") | Some("exit-recorded")
        )
        && matches!(state, "working" | "watching" | "your-move")
    {
        return "stalled";
    }
    lower_state_with_verdict(state, reachability)
}

/// Build a [`TruthProbe`] from a parsed truth JSON body and its already-read
/// `state`. Shared by the success path and the non-zero-exit salvage
/// path so the field extraction has exactly one implementation.
fn build_truth_probe(parsed: Option<&serde_json::Value>, state: &str) -> TruthProbe {
    TruthProbe {
        state: state.to_owned(),
        // The shared reachability verdict, derived Python-side with the
        // falsifiers applied. Absent on a truth build that predates the
        // field, and callers then fall back to mapping `state` — which
        // is transcript activity only, so it cannot see a dead process.
        reachability: parsed
            .and_then(|value| value.get("reachability")?.as_str().map(str::to_owned)),
        basis: parsed.and_then(|value| value.get("basis")?.as_str().map(str::to_owned)),
        last_activity_age_s: parsed.and_then(|value| value.get("last_activity_age_s")?.as_f64()),
        // The age's instrument, straight off the payload. When the
        // resolver answered `unknown`, that key is null and `reason` carries
        // why; fall back to the reason, but ONLY the three unknown-path
        // words - a `stalled` row's `api-error-tail` reason is about its
        // tail, not about the age's instrument, and must not stand in for it.
        last_activity_basis: parsed
            .and_then(|value| {
                value
                    .get("last_activity_basis")?
                    .as_str()
                    .map(str::to_owned)
            })
            .or_else(|| {
                parsed
                    .and_then(|value| value.get("reason")?.as_str().map(str::to_owned))
                    .filter(|reason| {
                        matches!(
                            reason.as_str(),
                            "not-found" | "no-records" | "resolver-error"
                        )
                    })
            }),
        last_event_at: parsed
            .and_then(|value| value.get("last_event_at")?.as_str().map(str::to_owned)),
        last_message: parsed
            .and_then(|value| value.get("last_message")?.as_str().map(str::to_owned)),
        // Absent on a truth build that predates the field: null rather
        // than a fabricated variant, so a stale `fno` reads as "this
        // probe did not answer" instead of asserting no transcript.
        observed_model: parsed
            .and_then(|value| value.get("observed_model").cloned())
            .unwrap_or(serde_json::Value::Null),
        // Absent on an older `fno`: None, so the row renders as it does today.
        provider_refusal: parsed
            .and_then(|value| value.get("provider_refusal")?.as_str().map(str::to_owned)),
        harness_title: parsed
            .and_then(|value| value.get("harness_title")?.as_str().map(str::to_owned)),
    }
}

/// Decode ONE truth payload into a [`TruthProbe`]: the `{state, reachability,
/// basis, ...}` object `_truth_payload` writes, whether it arrived alone or as
/// one value of a `--handles` batch.
///
/// `None` when `state` is absent or is not one of the seven the reader emits,
/// which is the malformed-output case both entry points already refuse.
/// `warming` is the restart answer: reachability `unknown`, so every reader
/// takes its inconclusive branch.
///
/// The single decoder is the point. Two of them is how a batch reading and a
/// single reading of the same transcript start disagreeing about the same row.
fn parse_truth_payload(value: &serde_json::Value) -> Option<TruthProbe> {
    let state = value.get("state")?.as_str()?;
    match state {
        "done" | "watching" | "your-move" | "working" | "stalled" | "unknown" | "warming" => {
            Some(build_truth_probe(Some(value), state))
        }
        _ => None,
    }
}

/// [`family1_truth_probe`] for many handles at once. Inside the daemon every
/// handle answers from the in-memory cursors. Anywhere else the batch asks
/// the daemon over its socket, one round trip, so a CLI caller reads the
/// same warm cursors instead of rebuilding them. A daemon that is down, or
/// one that predates the `agent.truth` verb, falls back to the same reader
/// in this process: still Rust, still bounded, never a Python child. An
/// EMPTY slice answers an empty map without touching anything.
pub fn family1_truth_probe_many(
    handles: &[String],
) -> std::collections::HashMap<String, TruthProbe> {
    family1_truth_probe_many_measured(handles).0
}

/// Whether the batch instrument completed for the handles it was handed
/// ([`BatchOutcome::Measured`], including "ran clean and resolved nothing"),
/// or outlived its bound and answered for the page as a whole
/// ([`BatchOutcome::NotMeasured`]). A reader may not collapse the two:
/// `no-evidence` is a verdict the instrument earned, `unmeasured` says the
/// instrument did not run - the two facts this seam exists to separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    Measured,
    NotMeasured,
}

/// The list seam's entry point: whatever the batch measured, PLUS
/// whether it measured at all. A timeout keeps answers from pages that did
/// finish, which lets the row projection distinguish a measured handle from
/// one the instrument never reached.
pub fn family1_truth_probe_many_measured(
    handles: &[String],
) -> (std::collections::HashMap<String, TruthProbe>, BatchOutcome) {
    family1_truth_probe_many_measured_within(handles, None)
}

/// [`family1_truth_probe_many_measured`] under a caller's deadline: a handle
/// reached after the deadline returns [`BatchOutcome::NotMeasured`] with the
/// answers it did collect. `None` keeps the unbounded shape.
pub fn family1_truth_probe_many_measured_within(
    handles: &[String],
    deadline: Option<Instant>,
) -> (std::collections::HashMap<String, TruthProbe>, BatchOutcome) {
    if handles.is_empty() {
        return (std::collections::HashMap::new(), BatchOutcome::Measured);
    }
    let answered = if in_daemon() {
        None
    } else {
        ask_daemon(handles, deadline)
    };
    let (payloads, outcome) = match answered {
        Some(payloads) => (payloads, BatchOutcome::Measured),
        None => answer_payloads(handles, deadline),
    };
    let probes = payloads
        .into_iter()
        .filter_map(|(handle, payload)| Some((handle, parse_truth_payload(&payload)?)))
        .collect();
    (probes, outcome)
}

/// Answer every handle from the process-global cursors. The daemon reads
/// warm-only until its restart pass finishes, so a session it has not
/// rebuilt yet answers `warming` instead of a guess.
fn answer_payloads(
    handles: &[String],
    deadline: Option<Instant>,
) -> (Vec<(String, serde_json::Value)>, BatchOutcome) {
    let rows = crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
        .ok()
        .map(|r| r.entries);
    let stores = crate::session_truth::Stores::ambient();
    let cursors = crate::session_truth::cursor::global();
    let mode = if in_daemon() && !WARM_DONE.load(Ordering::Acquire) {
        crate::session_truth::ReadMode::WarmOnly
    } else {
        crate::session_truth::ReadMode::Build
    };
    let mut payloads = Vec::with_capacity(handles.len());
    for handle in handles {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return (payloads, BatchOutcome::NotMeasured);
        }
        let payload = crate::session_truth::resolve_payload_with(
            rows.as_deref(),
            handle,
            now_epoch_s(),
            &stores,
            cursors,
            mode,
        );
        payloads.push((handle.clone(), payload));
    }
    (payloads, BatchOutcome::Measured)
}

static IN_DAEMON: AtomicBool = AtomicBool::new(false);
static WARM_DONE: AtomicBool = AtomicBool::new(false);

fn in_daemon() -> bool {
    IN_DAEMON.load(Ordering::Acquire)
}

/// Mark this process as the daemon and rebuild every registry row's cursor
/// on a background thread. Each rebuild reads back from EOF to the newest
/// compact boundary or the 64 KB cap, never from the start. Until the pass
/// ends, a session it has not reached answers `warming`. The pass emits
/// `truth_warm_done` with its row count and wall time.
pub fn start_daemon_warm(home: &crate::paths::AgentsHome) {
    if IN_DAEMON.swap(true, Ordering::AcqRel) {
        return;
    }
    let home = home.clone();
    let spawned = std::thread::Builder::new()
        .name("truth-warm".into())
        .spawn(move || {
            let started = Instant::now();
            let rows = crate::state::load_registry(&home.registry_json())
                .map(|r| r.entries)
                .unwrap_or_default();
            let stores = crate::session_truth::Stores::ambient();
            let cursors = crate::session_truth::cursor::global();
            let warmed = rows
                .iter()
                .filter(|row| crate::session_truth::warm_row(row, &stores, cursors))
                .count();
            WARM_DONE.store(true, Ordering::Release);
            let _ = crate::events::EventEmitter::new(home.events_jsonl(), "daemon").emit(
                "truth_warm_done",
                &serde_json::json!({
                    "rows": rows.len(),
                    "warmed": warmed,
                    "ms": started.elapsed().as_millis() as u64,
                }),
            );
        });
    if spawned.is_err() {
        // No thread, no warm pass: read every cursor on demand instead.
        WARM_DONE.store(true, Ordering::Release);
    }
}

/// The daemon's `agent.truth` verb: `{"handles": [...]}` in, `{"answers":
/// {handle: payload}}` out, every payload from memory.
pub fn truth_rpc<C>(_ctx: &C, req: &crate::protocol::Request) -> crate::protocol::Response {
    let handles: Vec<String> = req
        .params
        .get("handles")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|h| h.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let (payloads, outcome) = answer_payloads(&handles, None);
    let answers: serde_json::Map<String, serde_json::Value> = payloads.into_iter().collect();
    crate::protocol::Response::ok(
        req.id,
        serde_json::json!({
            "answers": answers,
            "measured": outcome == BatchOutcome::Measured,
        }),
    )
}

/// One bounded `agent.truth` round trip on the daemon socket. `None` when no
/// daemon listens, the deadline passes, or the daemon does not know the
/// verb; the caller then reads in process.
fn ask_daemon(
    handles: &[String],
    deadline: Option<Instant>,
) -> Option<Vec<(String, serde_json::Value)>> {
    use std::io::{Read, Write};
    let budget = deadline
        .map(|d| d.saturating_duration_since(Instant::now()))
        .unwrap_or(ASK_BUDGET)
        .min(ASK_BUDGET);
    if budget.is_zero() {
        return None;
    }
    let sock = crate::paths::AgentsHome::from_env().supervisor_sock();
    let mut stream = std::os::unix::net::UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(budget)).ok()?;
    stream.set_write_timeout(Some(budget)).ok()?;
    let req =
        crate::protocol::Request::new(1, "agent.truth", serde_json::json!({ "handles": handles }));
    let body = serde_json::to_vec(&req).ok()?;
    stream.write_all(&(body.len() as u32).to_le_bytes()).ok()?;
    stream.write_all(&body).ok()?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len);
    if len > crate::protocol::MAX_FRAME_BYTES {
        return None;
    }
    let mut reply = vec![0u8; len as usize];
    stream.read_exact(&mut reply).ok()?;
    let resp: crate::protocol::Response = serde_json::from_slice(&reply).ok()?;
    let answers = resp.result()?.get("answers")?.as_object()?;
    Some(
        answers
            .iter()
            .map(|(h, payload)| (h.clone(), payload.clone()))
            .collect(),
    )
}

/// A daemon answer comes from memory, so a round trip this slow means the
/// daemon is wedged; the caller reads in process instead.
const ASK_BUDGET: Duration = Duration::from_secs(5);

fn now_epoch_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// How many handles a feed may carry within `bound`. Every handle answers
/// in process now, so the bound no longer prices interpreter cold starts:
/// the honest answer is "all of them", whatever `bound` says. Kept because
/// `org_board` sizes its slice with it and its shape is unchanged.
pub fn family1_truth_affordable_handles(_bound: Duration) -> usize {
    usize::MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC1/AC4 on the wire: the field arrives when Python sends it, and an older
    /// `fno` that sends no such key parses `None` rather than a fabricated
    /// value, so its rows render exactly as they did before.
    #[test]
    fn the_provider_refusal_parses_from_the_wire_and_absence_stays_none() {
        let refused = serde_json::json!({
            "state": "working",
            "reachability": "reachable",
            "last_activity_age_s": 469.0,
            "provider_refusal": "provider_4xx_quota",
        });
        assert_eq!(
            parse_truth_payload(&refused)
                .unwrap()
                .provider_refusal
                .as_deref(),
            Some("provider_4xx_quota")
        );
        let older = serde_json::json!({"state": "working", "reachability": "reachable"});
        assert!(parse_truth_payload(&older)
            .unwrap()
            .provider_refusal
            .is_none());
        // A non-string value is not a class name: absence, never a coerced one.
        let junk = serde_json::json!({"state": "working", "provider_refusal": 7});
        assert!(parse_truth_payload(&junk)
            .unwrap()
            .provider_refusal
            .is_none());
    }

    /// The affordability helper is the timeout formula's inverse: a feed of
    /// `n <= affordable` handles self-bounds at or under the bound the
    /// caller measured, the next handle overshoots it, and the boundaries
    /// hold (nothing under the cold start, everything at the ceiling). The
    /// pin keeps the two from drifting apart.
    #[test]
    fn every_handle_is_affordable_in_process() {
        // The reader answers in process now, so the bound prices nothing:
        // any feed size fits any bound.
        assert_eq!(
            family1_truth_affordable_handles(Duration::from_secs(0)),
            usize::MAX
        );
    }

    /// The age's instrument parses off the same wire, and on the
    /// unknown paths the resolver's reason word stands in for it - but ONLY
    /// the three unknown-path words. A `stalled` row's `api-error-tail`
    /// reason describes its tail, not the age's instrument, and must never
    /// masquerade as one. An older `fno` sending neither key parses None, so
    /// absence renders as absence.
    #[test]
    fn the_activity_basis_parses_with_the_unknown_reason_as_fallback() {
        let answered = serde_json::json!({
            "state": "working",
            "last_activity_age_s": 12.0,
            "last_activity_basis": "last-entry",
        });
        assert_eq!(
            parse_truth_payload(&answered)
                .unwrap()
                .last_activity_basis
                .as_deref(),
            Some("last-entry")
        );
        let unresolvable = serde_json::json!({
            "state": "unknown",
            "reason": "resolver-error",
        });
        assert_eq!(
            parse_truth_payload(&unresolvable)
                .unwrap()
                .last_activity_basis
                .as_deref(),
            Some("resolver-error"),
            "a crashed resolver must not render as a blank row"
        );
        let stalled_with_tail_reason = serde_json::json!({
            "state": "stalled",
            "reason": "api-error-tail",
            "last_activity_basis": "mtime",
        });
        assert_eq!(
            parse_truth_payload(&stalled_with_tail_reason)
                .unwrap()
                .last_activity_basis
                .as_deref(),
            Some("mtime"),
            "a tail reason never stands in for the instrument word"
        );
        let older = serde_json::json!({"state": "working"});
        assert!(parse_truth_payload(&older)
            .unwrap()
            .last_activity_basis
            .is_none());
    }

    #[test]
    fn an_unreachable_verdict_lowers_a_would_be_live_state() {
        assert_eq!(
            lower_state_with_verdict("working", Some("unreachable")),
            "unreachable"
        );
        // Monotone: a verdict never raises, and never rewrites a terminal state
        // (resume's relaunch arm keys on `done`/`stalled` and must keep working).
        assert_eq!(
            lower_state_with_verdict("done", Some("unreachable")),
            "done"
        );
        assert_eq!(
            lower_state_with_verdict("stalled", Some("unknown")),
            "stalled"
        );
        // No verdict on the wire (a truth build that predates the field) leaves
        // the pre-existing mapping exactly as it was.
        assert_eq!(lower_state_with_verdict("working", None), "working");
        assert_eq!(
            lower_state_with_verdict("working", Some("reachable")),
            "working"
        );
    }

    #[test]
    fn resume_lowering_treats_a_gone_process_as_dead() {
        // the resume variant lowers a live-seeming state the verdict
        // falsified with PROCESS-gone evidence to "stalled", so the relaunch arm
        // fires for a pane-gone worker instead of the inconclusive refusal.
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), Some("pane-gone")),
            "stalled"
        );
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), Some("process-gone")),
            "stalled"
        );
        // An exit record is the same affirmative evidence class: reconcile
        // PROVED the child gone before writing it, so a daemon-stopped row
        // relaunches too instead of refusing inconclusive forever.
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), Some("exit-recorded")),
            "stalled"
        );
        // A silent / no-evidence unreachable is NOT affirmatively dead: the
        // process may still be alive, so it stays "unreachable" (resume's
        // inconclusive refusal, never a relaunch that would double-write).
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), Some("silent")),
            "unreachable"
        );
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), None),
            "unreachable"
        );
        // Monotone: a terminal state is never rewritten, and the shared lowering
        // still owns the reachable / no-verdict cases unchanged.
        assert_eq!(
            lower_state_for_resume("done", Some("unreachable"), Some("pane-gone")),
            "done"
        );
        assert_eq!(lower_state_for_resume("working", None, None), "working");
        assert_eq!(
            lower_state_for_resume("working", Some("reachable"), Some("transcript")),
            "working"
        );
    }
}
