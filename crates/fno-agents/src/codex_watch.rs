//! The settle arm (US6b+c of the loop-check idle design, every harness): the
//! fno-agents daemon is the watcher for a parked `<watching>` idle. loop-check
//! registers the watch (the idle event IS the registration), and this arm
//! polls the PR cheaply - the coalescing status cache, no model turns - and
//! wakes the owning session on CI settle through that harness's own lane:
//! codex keeps the `turn/start` socket inject the codex waker introduced,
//! every other harness rides the mail lane whose durable queue is the
//! routability floor.
//! Leads and workers are the same consumer: the eligibility rule is claim
//! ownership, never role. Inject failure retries next tick; the claim-lease
//! expiry respawns the node as the terminal backstop. The declared-timeout
//! wake stays with watch_expiry, whose mail lane already reaches every
//! harness.
//!
//! Wake trigger rules:
//! - blocker "ci": wake when the coalescing status read answers
//!   settled: true (green or red; the session re-evaluates either way).
//! - codex watches: drop the watch when the thread leaves the loaded roster
//!   (a successful discovery that no longer lists the thread is definitive).
//!   Other harnesses have no roster read; their lane's durable floor and the
//!   claim/liveness expiry reap them.
//! - every other blocker: no early wake; the expiry arm owns the timeout
//!   wake (already harness-neutral).
//!
//! The file keeps the `codex_watch` name it was born with; the arm it houses
//! is the settle arm for every parkable harness.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::paths::AgentsHome;
use crate::watch_expiry::{self, Evidence, Watch};

/// Poll cadence. One coalesced status read per ci watch per tick: the cache
/// serves one live gh read per TTL across the whole fleet, so N watchers on
/// one PR cost one network read.
const INTERVAL: Duration = Duration::from_secs(60);

/// Settle wakes stand down this close to expiry: the timeout wake owns the
/// boundary, and the grace keeps the two arms' evidence reads from
/// straddling the expiry line inside one inject round trip.
const EXPIRY_GRACE_MS: i64 = 90 * 1000;

/// The arm state: cadence stamp + one-in-flight gate, the shape every
/// FleetArms member keeps.
#[derive(Default)]
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

/// The watch this arm acts on: a `loop_check_watch_idle` row from a harness
/// that may park, with the addresses its lane needs - the session id (every
/// lane resolves it) and, for codex, the thread id the socket inject names.
#[derive(Debug, Clone)]
pub(crate) struct SettleWatch {
    pub(crate) watch: Watch,
    pub(crate) harness: String,
    pub(crate) codex_thread_id: Option<String>,
    pub(crate) cwd: String,
}

/// Every current parkable watch in the window: a harness the lease admits,
/// and for codex a thread id. The latest idle row per session wins (the same
/// newest-per-session rule watch_expiry's watches() applies).
pub(crate) fn settle_watches(evidence: &[Evidence]) -> Vec<SettleWatch> {
    let mut latest: std::collections::BTreeMap<String, SettleWatch> = Default::default();
    for row in evidence
        .iter()
        .filter(|row| row.kind == watch_expiry::WATCH_IDLE)
    {
        let Some(session_id) = row.session_id.clone() else {
            continue;
        };
        let data = &row.data;
        let Some(harness) = data
            .get("harness")
            .and_then(Value::as_str)
            .filter(|h| crate::loopcheck::watch_lease::IDLE_HARNESSES.contains(h))
            .map(str::to_string)
        else {
            continue;
        };
        let codex_thread_id = if harness == "codex" {
            match data
                .get("codex_thread_id")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
            {
                Some(thread) => Some(thread.to_string()),
                None => continue, // a codex watch without its socket address is unroutable
            }
        } else {
            None
        };
        let Some(expires_at_ms) = data.get("expires_at_ms").and_then(Value::as_i64) else {
            continue;
        };
        let watch = Watch {
            event_id: row.event_id.clone(),
            seq: row.seq,
            session_id: session_id.clone(),
            pr: data.get("pr").and_then(Value::as_i64),
            node: data
                .get("node")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            blocker: data
                .get("blocker")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            task_id: data
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            reason: data
                .get("reason")
                .and_then(Value::as_str)
                .filter(|r| !r.trim().is_empty())
                .map(str::to_string),
            expires_at_ms,
            ts_ms: row.ts_ms,
        };
        let entry = SettleWatch {
            watch,
            harness,
            codex_thread_id,
            cwd: data
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        if latest.get(&session_id).map_or(true, |old| {
            (entry.watch.ts_ms, entry.watch.seq) > (old.watch.ts_ms, old.watch.seq)
        }) {
            latest.insert(session_id, entry);
        }
    }
    latest.into_values().collect()
}

/// Whether the arm must act on this watch now: unexpired, still the session's
/// current watch (no later loop_check/terminal row), and no wake receipt yet
/// (the receipt dedupe covers wake success, roster drop, and the expiry
/// arm's own wake, so exactly one lane ever fires per watch episode).
pub(crate) fn should_act(watch: &Watch, now_ms: i64, evidence: &[Evidence]) -> bool {
    now_ms < watch.expires_at_ms
        && watch_expiry::is_current_watch(watch, evidence)
        && !woken_already(watch, evidence)
}

fn woken_already(watch: &Watch, evidence: &[Evidence]) -> bool {
    evidence.iter().any(|row| {
        row.session_id.as_deref() == Some(watch.session_id.as_str())
            && row.kind == watch_expiry::WAKE_EVENT
            && row.data.get("watch_event_id").and_then(Value::as_str)
                == Some(watch.event_id.as_str())
    })
}

/// One coalesced status answer, degraded to not-settled on a refused read.
pub(crate) struct PollAnswer {
    settled: bool,
    verdict: String,
}

/// One settled delivery: the lane that took it and whether it landed live
/// (`delivered=false` with a lane that queues durably is still terminal - the
/// message reaches the session at its next turn).
pub(crate) type Delivered = Result<(&'static str, bool), String>;

/// One pass: consume watches, drop dead codex ones, wake settled ci watches
/// through the owning session's lane. `roster`, `poll` and `deliver` are
/// injected so tests stage the world without a live daemon, gh, mail bus, or
/// codex app-server.
pub(crate) fn run_pass_with(
    home: &AgentsHome,
    roster: &dyn Fn() -> Result<HashSet<String>, &'static str>,
    poll: &dyn Fn(&str, u64) -> Result<PollAnswer, String>,
    deliver: &dyn Fn(&SettleWatch, &str) -> Delivered,
    emitter: &crate::events::EventEmitter,
) -> Result<(), String> {
    let now_ms = millis_now();
    let evidence = watch_expiry::read_evidence(home, now_ms)?;
    let watches = settle_watches(&evidence);
    if watches.is_empty() {
        return Ok(());
    }
    // One roster read per pass; an unreadable roster keeps every watch (the
    // claim-lease expiry stays the backstop, never a drop on a blind read).
    let roster = match roster() {
        Ok(set) => Some(set),
        Err(_) => None,
    };
    // One claim read per pass, the expiry arm's eligibility map: session ->
    // its single live node claim (or the error that says unreadable).
    let claims = watch_expiry::current_node_claims(home)?;
    for watch in watches {
        let w = &watch.watch;
        if !should_act(w, now_ms, &evidence) {
            continue;
        }
        // Roster exit is definitive - and codex-only: the codex loaded-roster
        // discovery is the one lane that can prove its session left. A
        // successful discovery that no longer lists the thread ends the
        // watch; a blind roster read never drops.
        if let Some(thread) = &watch.codex_thread_id {
            if let Some(set) = &roster {
                if !set.contains(thread) {
                    emit_receipt(
                        emitter,
                        w,
                        false,
                        "codex_daemon",
                        &format!("watch dropped: thread {thread} left the loaded roster"),
                    );
                    continue;
                }
            }
        }
        // Only a ci watch has an early wake; the expiry arm owns the rest.
        if w.blocker != "ci" {
            continue;
        }
        // Grace before expiry belongs to the timeout wake: inside it the two
        // arms' evidence reads can straddle the expiry line before either
        // receipt lands, and both would wake. The bound is longer than one
        // inject round trip, so a receipt always precedes the expiry arm's
        // read.
        if w.expires_at_ms - now_ms < EXPIRY_GRACE_MS {
            continue;
        }
        // The expiry arm's eligibility rule, shared: a wake goes only to a
        // session that still owns the same live node claim, so a settled PR
        // never burns a turn into a session whose node moved on. The claims
        // map indexes by the registry's harness_session_id (for codex the
        // thread id), which a target session's manifest session_id need not
        // equal, so both identities answer. A read error keeps the watch
        // (retry next tick), never a wake.
        let claim = claims
            .get(&w.session_id)
            .or_else(|| claims.get(watch.codex_thread_id.as_deref().unwrap_or("")))
            .cloned()
            .unwrap_or(Ok(None));
        let node = match claim {
            Ok(Some(node)) => node,
            _ => continue,
        };
        if !w.node.is_empty() && node != w.node {
            continue;
        }
        let Some(pr) = w.pr else {
            continue;
        };
        let poll_result = poll(&watch.cwd, pr.unsigned_abs());
        let Some(answer) = poll_result.ok() else {
            continue; // refused read: retry next tick (c) - never a drop
        };
        if !answer.settled {
            continue;
        }
        let text = wake_text(w, &answer.verdict);
        match deliver(&watch, &text) {
            Ok((via, delivered)) => {
                emit_receipt(emitter, w, delivered, via, "watch settled; wake accepted");
            }
            Err(_) => {
                // Delivery failure retries next tick (AC4-FR); the receipt is
                // not emitted, so no dedupe suppresses the retry.
            }
        }
    }
    Ok(())
}

fn wake_text(watch: &Watch, verdict: &str) -> String {
    format!(
        "Automatic watch-settle notice from the fno daemon. Your ci watch on PR #{pr} \
settled ({verdict}): the daemon woke this session instead of a watcher. Re-evaluate now.",
        pr = watch.pr.unwrap_or(0),
    )
}

/// One watch receipt: a settled wake accepted, or a watch dropped. Split
/// from the decision body so the failed-write case is named on stderr
/// instead of vanishing (silent-failure posture).
fn emit_receipt(
    emitter: &crate::events::EventEmitter,
    watch: &Watch,
    delivered: bool,
    via: &str,
    reason: &str,
) {
    let payload = serde_json::json!({
        "session_id": watch.session_id,
        "node": watch.node,
        "watch_event_id": watch.event_id,
        "blocker": watch.blocker,
        "task_id": watch.task_id,
        "expires_at_ms": watch.expires_at_ms,
        "delivered": delivered,
        "via": via,
        "reason": reason,
    });
    if let Err(error) = emitter.emit(watch_expiry::WAKE_EVENT, &payload) {
        eprintln!("settle-watch: receipt emit failed: {error}");
    }
}

fn millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// The settle delivery through one session's harness lane. Codex keeps the
/// turn/start socket inject; every other harness rides the mail lane (live
/// inject first, the durable queue carried home by the resume fallback).
/// Any accepted outcome means the message reaches the session, and the
/// receipt keeps the retry from double-sending.
fn deliver_settle(watch: &SettleWatch, text: &str) -> Delivered {
    if watch.harness == "codex" {
        let thread = match watch.codex_thread_id.as_deref() {
            Some(thread) => thread,
            None => return Err("codex watch lost its thread id".into()),
        };
        return match crate::codex_inject::deliver_via_codex_daemon_sync(thread, text) {
            Ok(()) => Ok(("codex_daemon", true)),
            // The wire send succeeded and only the response was lost, so
            // the turn may already run: a receipt here suppresses the
            // redelivery that would start a duplicate turn every tick.
            Err(crate::codex_inject::ReviewStartError::Reason("turn-start-unacked")) => {
                Ok(("codex_daemon", true))
            }
            Err(e) => Err(format!("{e:?}")),
        };
    }
    let mut runner = crate::burn_watch::run_command;
    let (delivered, via) = crate::burn_watch::wake_with_text(
        &watch.watch.session_id,
        false,
        text,
        "settle-watch",
        &mut runner,
    );
    match (delivered, via) {
        (true, via) => Ok((via, true)),
        (false, via) => Err(format!("mail lane refused the settle wake via {via}")),
    }
}

/// Production closures: the loaded-roster read, the coalesced status poll,
/// and the per-harness settle delivery.
pub(crate) fn run_pass(home: &AgentsHome) -> Result<(), String> {
    run_pass_with(
        home,
        &crate::codex_inject::loaded_thread_ids,
        &|cwd, pr| {
            let (_code, out, _, _) = crate::pr_status::cache::cached_status(cwd, pr, false);
            Ok(PollAnswer {
                settled: out.get("settled").and_then(Value::as_bool) == Some(true),
                verdict: out
                    .get("verdict")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
            })
        },
        &deliver_settle,
        &crate::events::EventEmitter::new(crate::daemon::global_events_path(home), "daemon"),
    )
}

pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    {
        let mut last = arm
            .last_tick
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if last.is_some_and(|tick| tick.elapsed() < INTERVAL)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        if let Err(error) = run_pass(&home) {
            eprintln!("settle-watch: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventEmitter;
    use std::cell::RefCell;

    /// A temp agents home plus its global journal path, the fixture the
    /// evidence reader and the receipt emitter share. The session's claims
    /// root moves into the temp dir for the test's life. Two live sessions
    /// are staged: a codex thread whose registry harness_session_id
    /// (`thread-a`) DIVERGES from the watch's manifest session_id
    /// (`codex-sess`) - the eligibility lookup must answer through the
    /// thread-id identity - and an opencode worker keyed by its own session
    /// id, the shape a real target session presents on either harness.
    fn staged() -> (AgentsHome, std::path::PathBuf, tempfile::TempDir) {
        let td = tempfile::TempDir::new().unwrap();
        let claims_root = td.path().join("claims-root");
        std::fs::create_dir_all(&claims_root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let home = AgentsHome::at(td.path().join("agents"));
        let _ = home.ensure_root();
        let mut codex_entry = crate::state::RegistryEntry::default();
        codex_entry.name = "codex-worker".into();
        codex_entry.harness_session_id = Some("thread-a".into());
        codex_entry.status = crate::AgentStatus::Live;
        let mut oc_entry = crate::state::RegistryEntry::default();
        oc_entry.name = "oc-worker".into();
        oc_entry.harness_session_id = Some("oc-sess-a".into());
        oc_entry.status = crate::AgentStatus::Live;
        let mut registry = crate::state::Registry::default();
        registry.entries.push(codex_entry);
        registry.entries.push(oc_entry);
        crate::registry_store::seed_raw(
            &home.registry_json(),
            serde_json::to_vec(&registry).unwrap(),
        );
        let acquired = crate::claims::acquire(
            "node:x-codexwt",
            "target-session:thread-a",
            crate::claims::AcquireOpts {
                pid: Some(std::process::id()),
                identity: Some(("thread-a".into(), "codex".into())),
                root: None,
                events_dir: Some(td.path().join("claim-events")),
                ..Default::default()
            },
        );
        assert!(matches!(
            acquired,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let acquired = crate::claims::acquire(
            "node:x-ocwt",
            "target-session:oc-sess-a",
            crate::claims::AcquireOpts {
                pid: Some(std::process::id()),
                identity: Some(("oc-sess-a".into(), "opencode".into())),
                root: None,
                events_dir: Some(td.path().join("claim-events")),
                ..Default::default()
            },
        );
        assert!(matches!(
            acquired,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let events = crate::daemon::global_events_path(&home);
        (home, events, td)
    }

    fn stage_row(events: &std::path::Path, row: serde_json::Value) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(events)
            .unwrap();
        writeln!(f, "{row}").unwrap();
    }

    fn watch_row(
        session: &str,
        harness: &str,
        thread: Option<&str>,
        node: &str,
        pr: i64,
        expires_at_ms: i64,
    ) -> serde_json::Value {
        serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "type": "loop_check_watch_idle",
            "source": "hook",
            "data": {
                "session_id": session,
                "harness": harness,
                "codex_thread_id": thread,
                "cwd": "/repo/wt",
                "node": node,
                "pr": pr,
                "blocker": "ci",
                "expires_at_ms": expires_at_ms
            }
        })
    }

    /// The wake journal the tests read back: (harness, address, text) per
    /// accepted delivery, keyed across passes.
    type Journal = RefCell<Vec<(String, String, String)>>;

    fn new_journal() -> Journal {
        RefCell::new(Vec::new())
    }

    /// A ci watch pair + the pass seams: the roster closure that names
    /// `thread-a` only when asked, a poll that answers the given verdict,
    /// and a deliver closure recording every accepted wake.
    fn pass_with(
        home: &AgentsHome,
        events: &std::path::Path,
        settled: bool,
        roster_has_thread: bool,
        deliver_ok: bool,
        journal: &Journal,
    ) {
        let roster = move || {
            if roster_has_thread {
                Ok(HashSet::from(["thread-a".to_string()]))
            } else {
                Ok(HashSet::new())
            }
        };
        let poll = move |_cwd: &str, _pr: u64| {
            Ok(PollAnswer {
                settled,
                verdict: if settled {
                    "green".into()
                } else {
                    "pending".into()
                },
            })
        };
        let deliver = move |watch: &SettleWatch, text: &str| {
            let addr = watch
                .codex_thread_id
                .clone()
                .unwrap_or_else(|| watch.watch.session_id.clone());
            if deliver_ok {
                let (via, delivered) = if watch.harness == "codex" {
                    ("codex_daemon", true)
                } else {
                    ("mail", true)
                };
                journal
                    .borrow_mut()
                    .push((watch.harness.clone(), addr, text.to_string()));
                Ok((via, delivered))
            } else {
                Err("lane down".into())
            }
        };
        run_pass_with(
            home,
            &roster,
            &poll,
            &deliver,
            &EventEmitter::new(events.to_path_buf(), "daemon"),
        )
        .unwrap();
    }

    fn woken(journal: &Journal, harness: &str) -> Vec<(String, String)> {
        journal
            .borrow()
            .iter()
            .filter(|(h, _, _)| h == harness)
            .map(|(_, a, t)| (a.clone(), t.clone()))
            .collect()
    }

    #[test]
    fn settled_watch_wakes_once_through_the_owning_lane_then_dedupes() {
        // AC4-HP + the receipt dedupe, both lanes: first pass wakes the
        // codex thread through turn/start AND the opencode worker through
        // its mail lane; the receipts suppress the second pass. AC4-FR: a
        // failed delivery leaves no receipt, so the next pass retries.
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, events, _td) = staged();
        let expires = millis_now() + 600_000;
        stage_row(
            &events,
            watch_row(
                "codex-sess",
                "codex",
                Some("thread-a"),
                "x-codexwt",
                42,
                expires,
            ),
        );
        stage_row(
            &events,
            watch_row("oc-sess-a", "opencode", None, "x-ocwt", 43, expires),
        );
        let journal = new_journal();
        pass_with(&home, &events, true, true, true, &journal);
        let codex = woken(&journal, "codex");
        assert_eq!(codex.len(), 1, "one codex inject on settle: {codex:?}");
        assert_eq!(codex[0].0, "thread-a");
        assert!(codex[0].1.contains("PR #42"), "{:?}", codex[0].1);
        let oc = woken(&journal, "opencode");
        assert_eq!(oc.len(), 1, "one mail wake on settle: {oc:?}");
        assert_eq!(
            oc[0].0, "oc-sess-a",
            "the mail lane addresses the session id"
        );
        assert!(oc[0].1.contains("PR #43"), "{:?}", oc[0].1);
        // The emitter wrote the receipts to the same journal; the second
        // pass must not wake again.
        pass_with(&home, &events, true, true, true, &journal);
        assert_eq!(
            woken(&journal, "codex").len(),
            1,
            "receipt dedupes the wake"
        );
        assert_eq!(
            woken(&journal, "opencode").len(),
            1,
            "receipt dedupes the wake"
        );
        // AC4-FR: a failed delivery leaves no receipt, so the next pass
        // retries (fresh rows, new watch episodes).
        stage_row(
            &events,
            watch_row(
                "codex-sess",
                "codex",
                Some("thread-a"),
                "x-codexwt",
                42,
                millis_now() + 600_000,
            ),
        );
        let journal2 = new_journal();
        pass_with(&home, &events, true, true, false, &journal2);
        assert_eq!(
            woken(&journal2, "codex").len(),
            0,
            "failed lane stayed silent"
        );
        pass_with(&home, &events, true, true, true, &journal2);
        assert_eq!(woken(&journal2, "codex").len(), 1, "the retry ran");
        pass_with(&home, &events, true, true, true, &journal2);
        assert_eq!(woken(&journal2, "codex").len(), 1, "receipt then dedupes");
    }

    #[test]
    fn roster_gates_codex_watches_only() {
        // Roster exit is definitive - for codex alone: the drop receipt
        // marks delivered=false and names the thread, so the expiry arm
        // never re-wakes it, while the same empty roster never drops (or
        // blocks) an opencode watch, whose lane has no roster and wakes
        // through mail. The receipts are read back through the store, the
        // same path the evidence reader (and so the dedupe) uses.
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, events, _td) = staged();
        let expires = millis_now() + 600_000;
        stage_row(
            &events,
            watch_row(
                "codex-sess",
                "codex",
                Some("thread-a"),
                "x-codexwt",
                42,
                expires,
            ),
        );
        stage_row(
            &events,
            watch_row("oc-sess-a", "opencode", None, "x-ocwt", 43, expires),
        );
        let journal = new_journal();
        pass_with(&home, &events, true, false, true, &journal);
        assert!(
            woken(&journal, "codex").is_empty(),
            "no wake on a dead thread"
        );
        assert_eq!(
            woken(&journal, "opencode").len(),
            1,
            "an empty codex roster never gates another harness"
        );
        let rows = crate::event_store::query_events(
            &events,
            &crate::event_store::EventQuery::of_types(&[watch_expiry::WAKE_EVENT]),
        )
        .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "one drop receipt + one mail receipt: {:?}",
            rows.len()
        );
        let drops: Vec<serde_json::Value> = rows
            .iter()
            .filter_map(|row| {
                let data: serde_json::Value = serde_json::from_str(row.line.as_str()).unwrap();
                (data["data"]["delivered"] == false).then_some(data)
            })
            .collect();
        assert_eq!(drops.len(), 1, "exactly one drop receipt: {drops:?}");
        let data = &drops[0];
        assert_eq!(data["data"]["via"], "codex_daemon");
        assert!(
            data["data"]["reason"]
                .as_str()
                .unwrap()
                .contains("left the loaded roster"),
            "{data}"
        );
    }
}
