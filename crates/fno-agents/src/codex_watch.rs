//! The codex daemon waker (US6b+c of the loop-check idle design): the fno-agents daemon is the
//! watcher for codex `<watching>` idles. loop-check's codex path registers
//! the watch (the idle event IS the registration), and this arm polls the PR
//! cheaply - the coalescing status cache, no model turns - and injects
//! `turn/start` via the codex daemon on CI settle. Inject failure retries
//! next tick; the claim-lease expiry respawns the node as the terminal
//! backstop. The declared-timeout wake stays with watch_expiry, whose mail
//! lane already reaches codex threads.
//!
//! Wake trigger rules:
//! - blocker "ci": inject when the coalescing status read answers
//!   settled: true (green or red; the session re-evaluates either way).
//! - every other blocker: no early wake; the expiry arm owns the timeout
//!   wake (already harness-neutral).
//! - any blocker: drop the watch when the thread leaves the loaded roster
//!   (a successful discovery that no longer lists the thread is definitive).

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

/// The codex watch this arm acts on: a `loop_check_watch_idle` row that names
/// harness codex and a live thread id, with the poll address the daemon needs.
#[derive(Debug, Clone)]
pub(crate) struct CodexWatch {
    pub(crate) watch: Watch,
    pub(crate) codex_thread_id: String,
    pub(crate) cwd: String,
}

/// Every current codex watch in the window: harness codex + a thread id.
/// The latest idle row per session wins (the same newest-per-session rule
/// watch_expiry's watches() applies).
pub(crate) fn codex_watches(evidence: &[Evidence]) -> Vec<CodexWatch> {
    let mut latest: std::collections::BTreeMap<String, CodexWatch> = Default::default();
    for row in evidence
        .iter()
        .filter(|row| row.kind == watch_expiry::WATCH_IDLE)
    {
        let Some(session_id) = row.session_id.clone() else {
            continue;
        };
        let data = &row.data;
        if data.get("harness").and_then(Value::as_str) != Some("codex") {
            continue;
        }
        let Some(thread_id) = data
            .get("codex_thread_id")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
        else {
            continue;
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
        let entry = CodexWatch {
            watch,
            codex_thread_id: thread_id.to_string(),
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
/// (the receipt dedupe covers inject success, roster drop, and the expiry
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

/// One pass: consume codex watches, drop dead ones, wake settled ci watches.
/// `roster`, `poll` and `inject` are injected so tests stage the world
/// without a live daemon, gh, or codex app-server.
pub(crate) fn run_pass_with(
    home: &AgentsHome,
    roster: &dyn Fn() -> Result<HashSet<String>, &'static str>,
    poll: &dyn Fn(&str, u64) -> Result<PollAnswer, String>,
    inject: &dyn Fn(&str, &str) -> Result<(), String>,
    emitter: &crate::events::EventEmitter,
) -> Result<(), String> {
    let now_ms = millis_now();
    let evidence = watch_expiry::read_evidence(home, now_ms)?;
    let watches = codex_watches(&evidence);
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
        // Roster exit is definitive: a successful discovery that no longer
        // lists the thread ends the watch (the session left, the daemon was
        // restarted without it, or the thread closed). A blind roster read
        // never drops.
        if let Some(set) = &roster {
            if !set.contains(&watch.codex_thread_id) {
                emit_receipt(
                    emitter,
                    w,
                    false,
                    "codex_daemon",
                    &format!(
                        "watch dropped: thread {} left the loaded roster",
                        watch.codex_thread_id
                    ),
                );
                continue;
            }
        }
        // Only a ci watch has an early wake; the expiry arm owns the rest.
        if w.blocker != "ci" {
            continue;
        }
        // Grace before expiry belongs to the timeout wake: inside it the two
        // arms' evidence reads can straddle the expiry line before either
        // receipt lands, and both would inject. The bound is longer than one
        // inject round trip, so a receipt always precedes the expiry arm's
        // read.
        if w.expires_at_ms - now_ms < EXPIRY_GRACE_MS {
            continue;
        }
        // The expiry arm's eligibility rule, shared: a wake goes only to a
        // session that still owns the same live node claim, so a settled PR
        // never burns a turn into a session whose node moved on. The claims
        // map indexes by the registry's harness_session_id (the codex thread
        // id), which a target session's manifest session_id need not equal,
        // so both identities answer. A read error keeps the watch (retry
        // next tick), never a wake.
        let claim = claims
            .get(&w.session_id)
            .or_else(|| claims.get(&watch.codex_thread_id))
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
        match inject(&watch.codex_thread_id, &text) {
            Ok(()) => {
                emit_receipt(
                    emitter,
                    w,
                    true,
                    "codex_daemon",
                    "watch settled; turn/start accepted",
                );
            }
            Err(_) => {
                // Inject failure retries next tick (AC4-FR); the receipt is
                // not emitted, so no dedupe suppresses the retry.
            }
        }
    }
    Ok(())
}

fn wake_text(watch: &Watch, verdict: &str) -> String {
    format!(
        "Automatic watch-settle notice from the fno daemon. Your ci watch on PR #{pr} \
settled ({verdict}): the daemon woke this thread instead of a watcher. Re-evaluate now.",
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
        eprintln!("codex-watch: receipt emit failed: {error}");
    }
}

fn millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// Production closures: the loaded-roster read, the coalesced status poll,
/// and the codex-daemon inject.
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
        &|thread, text| {
            match crate::codex_inject::deliver_via_codex_daemon_sync(thread, text) {
                Ok(()) => Ok(()),
                // The wire send succeeded and only the response was lost, so
                // the turn may already run: a receipt here suppresses the
                // redelivery that would start a duplicate turn every tick.
                Err(crate::codex_inject::ReviewStartError::Reason("turn-start-unacked")) => Ok(()),
                Err(e) => Err(format!("{e:?}")),
            }
        },
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
            eprintln!("codex-watch: {error}");
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
    /// root moves into the temp dir for the test's life, and one live node
    /// claim (`node:x-codexwt`) is staged beside a Live registry row whose
    /// harness_session_id (`thread-a`) DIVERGES from the watch's manifest
    /// session_id (`codex-sess`): the eligibility lookup must answer through
    /// the thread-id identity, the shape a real target session presents.
    fn staged() -> (AgentsHome, std::path::PathBuf, tempfile::TempDir) {
        let td = tempfile::TempDir::new().unwrap();
        let claims_root = td.path().join("claims-root");
        std::fs::create_dir_all(&claims_root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let home = AgentsHome::at(td.path().join("agents"));
        let _ = home.ensure_root();
        let mut entry = crate::state::RegistryEntry::default();
        entry.name = "codex-worker".into();
        entry.harness_session_id = Some("thread-a".into());
        entry.status = crate::AgentStatus::Live;
        let mut registry = crate::state::Registry::default();
        registry.entries.push(entry);
        std::fs::write(home.registry_json(), serde_json::to_vec(&registry).unwrap()).unwrap();
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

    fn watch_row(pr: i64, expires_at_ms: i64) -> serde_json::Value {
        serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "type": "loop_check_watch_idle",
            "source": "hook",
            "data": {
                "session_id": "codex-sess",
                "harness": "codex",
                "codex_thread_id": "thread-a",
                "cwd": "/repo/wt",
                "node": "x-codexwt",
                "pr": pr,
                "blocker": "ci",
                "expires_at_ms": expires_at_ms
            }
        })
    }

    /// A codex ci watch + poll seam pair: the shared roster closure that
    /// keeps the thread, and a poll that answers the given verdict.
    fn pass_with(
        home: &AgentsHome,
        events: &std::path::Path,
        settled: bool,
        roster_has_thread: bool,
        inject_result: bool,
        injected: &RefCell<Vec<String>>,
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
        let inject = move |thread: &str, _text: &str| {
            injected.borrow_mut().push(thread.to_string());
            if inject_result {
                Ok(())
            } else {
                Err("no-daemon".into())
            }
        };
        run_pass_with(
            home,
            &roster,
            &poll,
            &inject,
            &EventEmitter::new(events.to_path_buf(), "test"),
        )
        .unwrap();
    }

    #[test]
    fn settled_ci_watch_wakes_once_then_dedupes() {
        // AC4-HP + the receipt dedupe: first pass injects; the receipt it
        // emits suppresses the second pass (and the expiry arm).
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, events, _td) = staged();
        let expires = millis_now() + 600_000;
        stage_row(&events, watch_row(42, expires));
        let injected = RefCell::new(Vec::new());
        pass_with(&home, &events, true, true, true, &injected);
        assert_eq!(injected.borrow().len(), 1, "one inject on settle");
        // The emitter wrote the receipt to the same journal; the second pass
        // must not inject again.
        pass_with(&home, &events, true, true, true, &injected);
        assert_eq!(injected.borrow().len(), 1, "receipt dedupes the wake");
        // AC4-FR: a failed inject leaves no receipt, so the next pass retries.
        stage_row(&events, watch_row(42, millis_now() + 600_000));
        let injected2 = RefCell::new(Vec::new());
        pass_with(&home, &events, true, true, false, &injected2);
        assert_eq!(injected2.borrow().len(), 1, "failed inject attempted");
        pass_with(&home, &events, true, true, true, &injected2);
        assert_eq!(injected2.borrow().len(), 2, "the retry ran");
        pass_with(&home, &events, true, true, true, &injected2);
        assert_eq!(injected2.borrow().len(), 2, "receipt then dedupes");
    }

    #[test]
    fn unloaded_thread_drops_the_watch() {
        // Roster exit is definitive: the drop receipt marks delivered=false
        // and names the thread, so the expiry arm never re-wakes it. The
        // receipt is read back through the store, the same path the evidence
        // reader (and so the dedupe) uses.
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, events, _td) = staged();
        let expires = millis_now() + 600_000;
        stage_row(&events, watch_row(42, expires));
        let injected = RefCell::new(Vec::new());
        pass_with(&home, &events, true, false, true, &injected);
        assert!(injected.borrow().is_empty(), "no inject on a dead thread");
        let rows = crate::event_store::query_events(
            &events,
            &crate::event_store::EventQuery::of_types(&[watch_expiry::WAKE_EVENT]),
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "one drop receipt: {:?}", rows.len());
        let data: serde_json::Value = serde_json::from_str(rows[0].line.as_str()).unwrap();
        assert_eq!(data["data"]["delivered"], false);
        assert!(
            data["data"]["reason"]
                .as_str()
                .unwrap()
                .contains("left the loaded roster"),
            "{data}"
        );
    }
}
