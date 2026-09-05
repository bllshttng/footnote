//! `fno-agents wait` -- block until a named agent reaches a target state.
//!
//! Client-side and daemon-free by design. The daemon writes `registry.json`
//! atomically (tempfile + rename), so a plain shared-lock read always sees a
//! coherent snapshot; we poll that file and fold each row's effective state
//! through the same 3-tier lattice the badge uses (in-TTL `inside_leg` > fresh
//! `screen_state` > liveness), reusing [`InsideLegReport::is_live_at`] /
//! [`ScreenStateReport::is_live_at`] rather than forking crate `fno`'s
//! `derive_rows` (which the daemon crate cannot import -- the dependency runs
//! the other way). Terminal `Exited`/`PermanentDead` tops the lattice as `done`.
//!
//! Exit codes: `0` match, [`WAIT_TIMEOUT_EXIT`] (124, the GNU `timeout(1)`
//! convention) on timeout, `13` unknown agent, `2` usage, `1` read error.

use crate::paths::AgentsHome;
use crate::state::{self, InsideLegState, RegistryEntry};
use crate::AgentStatus;
use serde_json::json;
use std::time::{Duration, Instant};

/// Exit code when `wait` times out before the agent reaches the target state.
/// 124 is the code GNU `timeout(1)` uses, so scripts already special-case it.
pub const WAIT_TIMEOUT_EXIT: i32 = 124;

/// Registry poll interval. The registry is one local file the daemon writes
/// atomically, so a bounded poll is fine (the plan's stated v1 approach); no
/// fs-watch dependency for a file that changes on the order of seconds.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Default wait budget when `--timeout-ms` is omitted.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The effective state a `wait` observes, folded from a registry row. Mirrors
/// the badge lattice: `Working`/`Blocked`/`Done` are live verdicts; `Idle` is
/// "alive but no live working/blocked/done badge" (badge `None` in `derive_rows`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffState {
    Working,
    Blocked,
    Done,
    Idle,
}

impl EffState {
    /// Lowercase wire label (matches the inside-leg / screen-state vocabulary).
    pub fn label(self) -> &'static str {
        match self {
            EffState::Working => "working",
            EffState::Blocked => "blocked",
            EffState::Done => "done",
            EffState::Idle => "idle",
        }
    }
}

/// Fold one registry row to its effective state + the authority that decided it
/// (`"exit"` | `"hook"` | `"screen"` | `"liveness"`), at `now_secs` epoch
/// seconds. This is the daemon-side re-expression of crate `fno`'s `derive_rows`
/// lattice over the typed row: pane-exit > in-TTL hook > fresh screen > liveness.
pub fn effective_state(e: &RegistryEntry, now_secs: u64) -> (EffState, &'static str) {
    // Pane exit tops the lattice: a dead pane is `done`, never resurrected by a
    // stale badge.
    if matches!(e.status, AgentStatus::Exited | AgentStatus::PermanentDead) {
        return (EffState::Done, "exit");
    }
    // Hook (inside_leg) is senior and TTL-gated. A lapsed hook row does NOT fall
    // through to screen_state -- a hook-capable row is never scraped, so it goes
    // straight to liveness-only (mirrors derive_rows: the screen rung is reached
    // only for rows with no inside_leg at all).
    if let Some(leg) = &e.inside_leg {
        if leg.is_live_at(now_secs) {
            let st = match leg.state {
                InsideLegState::Working => EffState::Working,
                InsideLegState::Blocked => EffState::Blocked,
                InsideLegState::Done => EffState::Done,
            };
            return (st, "hook");
        }
        return (EffState::Idle, "liveness");
    }
    // Screen-manifest fallback, only for hook-less rows.
    if let Some(ss) = &e.screen_state {
        if ss.is_live_at(now_secs) {
            let st = match ss.state.as_str() {
                "working" => EffState::Working,
                "blocked" => EffState::Blocked,
                // "idle" and any unknown verdict read as idle.
                _ => EffState::Idle,
            };
            return (st, "screen");
        }
    }
    (EffState::Idle, "liveness")
}

/// Parse a `--state` target into the `EffState` it names. Only the three
/// documented targets are accepted (`working` is a transient, not a wait goal).
fn parse_target(s: &str) -> Option<EffState> {
    match s {
        "idle" => Some(EffState::Idle),
        "blocked" => Some(EffState::Blocked),
        "done" => Some(EffState::Done),
        _ => None,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the registry and fold the named row's effective state.
/// `Ok(None)` == no such agent (a fast, non-retryable miss).
fn find_effective(
    home: &AgentsHome,
    name: &str,
    now: u64,
) -> Result<Option<(EffState, &'static str)>, String> {
    Ok(find_effective_entry(home, name, now)?.map(|(_, state, authority)| (state, authority)))
}

/// Read the named registry row, fold its candidate state, and retain the row
/// for any authority that must reconcile the candidate with a second source.
fn find_effective_entry(
    home: &AgentsHome,
    name: &str,
    now: u64,
) -> Result<Option<(RegistryEntry, EffState, &'static str)>, String> {
    let reg = state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    Ok(reg.entries.iter().find(|e| e.name == name).map(|e| {
        let (state, authority) = effective_state(e, now);
        (e.clone(), state, authority)
    }))
}

/// `fno-agents wait --agent <name> --state idle|blocked|done [--timeout-ms N] [--json]`
pub async fn run_wait(rest: &[String], home: &AgentsHome) -> i32 {
    run_wait_with_timed_probe(rest, home, |handle, timeout| {
        crate::truth_probe::family1_truth_probe_with_timeout(handle, timeout)
    })
    .await
}

/// Testable implementation of [`run_wait`], with the family-1 truth reader
/// injected so wait's authority arbitration can be tested without spawning a
/// second CLI process.
pub async fn run_wait_with_probe(
    rest: &[String],
    home: &AgentsHome,
    truth_probe: impl Fn(&str) -> Option<crate::truth_probe::TruthProbe>,
) -> i32 {
    run_wait_with_timed_probe(rest, home, |handle, _timeout| truth_probe(handle)).await
}

async fn run_wait_with_timed_probe(
    rest: &[String],
    home: &AgentsHome,
    truth_probe: impl Fn(&str, Duration) -> Option<crate::truth_probe::TruthProbe>,
) -> i32 {
    let mut name: Option<String> = None;
    let mut target: Option<String> = None;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut json_out = false;

    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--agent" => match it.next() {
                Some(v) => name = Some(v.clone()),
                None => {
                    eprintln!("fno-agents: --agent needs a value");
                    return 2;
                }
            },
            "--state" => match it.next() {
                Some(v) => target = Some(v.clone()),
                None => {
                    eprintln!("fno-agents: --state needs a value");
                    return 2;
                }
            },
            "--timeout-ms" => match it.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(n) => timeout_ms = n,
                None => {
                    eprintln!("fno-agents: --timeout-ms needs a numeric value");
                    return 2;
                }
            },
            "--json" | "-J" => json_out = true,
            other if other.starts_with("--") => {
                eprintln!("fno-agents: wait: unknown flag: {other}");
                return 2;
            }
            // A bare positional is accepted as the agent name (parity with `logs`).
            _ if name.is_none() => name = Some(a.clone()),
            _ => {
                eprintln!("fno-agents: wait: unexpected argument: {a}");
                return 2;
            }
        }
    }

    let name = match name {
        Some(n) => n,
        None => {
            eprintln!("fno-agents: wait requires --agent <name>");
            return 2;
        }
    };
    let target_state = match target.as_deref().map(parse_target) {
        Some(Some(t)) => t,
        Some(None) => {
            eprintln!("fno-agents: wait --state must be idle|blocked|done");
            return 2;
        }
        None => {
            eprintln!("fno-agents: wait requires --state idle|blocked|done");
            return 2;
        }
    };

    // checked_add so an astronomically large --timeout-ms (which would overflow
    // the Instant) is treated as "wait indefinitely" rather than panicking.
    let deadline = Instant::now().checked_add(Duration::from_millis(timeout_ms));
    let mut last_observation: Option<String>;
    loop {
        match find_effective_entry(home, &name, now_secs()) {
            Ok(Some((entry, st, authority))) => {
                last_observation =
                    Some(format!("{} (candidate_authority={authority})", st.label()));
                if st == target_state {
                    if target_state == EffState::Done
                        && authority == "hook"
                        && entry.harness_name() == "claude"
                    {
                        if let Some(handle) = entry
                            .harness_session_id
                            .as_deref()
                            .filter(|handle| !handle.is_empty())
                        {
                            let probe_timeout = deadline
                                .map(|d| d.saturating_duration_since(Instant::now()))
                                .unwrap_or(Duration::from_secs(5));
                            match truth_probe(handle, probe_timeout) {
                                Some(probe) if probe.state == "done" => {
                                    if json_out {
                                        println!(
                                            "{}",
                                            json!({
                                                "state": st.label(),
                                                "authority": "transcript",
                                                "candidate_authority": authority,
                                            })
                                        );
                                    } else {
                                        println!(
                                            "{name} is {} (authority=transcript; candidate_authority={authority})",
                                            st.label(),
                                        );
                                    }
                                    return 0;
                                }
                                Some(probe) => {
                                    last_observation = Some(format!(
                                        "done (candidate_authority={authority}, transcript={})",
                                        probe.state
                                    ));
                                }
                                None => {
                                    last_observation = Some(
                                        "done (candidate_authority=hook, transcript=unknown/probe-failed)"
                                            .to_string(),
                                    );
                                }
                            }
                        } else {
                            last_observation = Some(
                                "done (candidate_authority=hook, transcript=unknown; no handle)"
                                    .to_string(),
                            );
                        }
                    } else {
                        if json_out {
                            println!("{}", json!({"state": st.label(), "authority": authority}));
                        } else {
                            println!("{name} is {} (via {authority})", st.label());
                        }
                        return 0;
                    }
                }
            }
            // Unknown agent: an immediate, non-retryable miss (AC edge).
            Ok(None) => {
                eprintln!("fno-agents: no such agent: {name}");
                return 13;
            }
            Err(e) => {
                eprintln!("fno-agents: wait: {e}");
                return 1;
            }
        }
        // A `None` deadline (overflow above) never fires -> effectively infinite.
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // Report the last-observed state (one read; the timeout path is rare).
            let last = last_observation
                .or_else(|| {
                    find_effective(home, &name, now_secs())
                        .ok()
                        .flatten()
                        .map(|(s, _)| s.label().to_string())
                })
                .unwrap_or_else(|| "unknown".to_string());
            eprintln!(
                "fno-agents: wait timed out after {timeout_ms}ms \
                 (agent {name} last observed: {last}, wanted: {})",
                target_state.label()
            );
            return WAIT_TIMEOUT_EXIT;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// Deserialize a `RegistryEntry` from the minimal wire shape plus overrides.
    /// Building via serde (not the struct literal) keeps the fixture robust to
    /// the row's many daemon-set fields and exercises the real read path.
    fn entry(overrides: Value) -> RegistryEntry {
        let mut base = json!({
            "name": "a",
            "provider": "claude",
            "cwd": "/tmp",
            "created_at": "2026-01-01T00:00:00Z",
            "status": "live",
        });
        if let (Value::Object(b), Value::Object(o)) = (&mut base, overrides) {
            b.extend(o);
        }
        serde_json::from_value(base).expect("fixture deserializes")
    }

    // Keep the measured row inside its TTL while remaining independent of the
    // wall clock used by this test.
    fn live_leg(state: &str) -> Value {
        json!({
            "state": state,
            "seq": 1,
            "received_at": "2026-01-01T00:00:00Z",
            "ttl_ms": 4_000_000_000_000u64
        })
    }

    fn truth_probe(state: &str) -> crate::truth_probe::TruthProbe {
        crate::truth_probe::TruthProbe {
            state: state.into(),
            reachability: Some("reachable".into()),
            basis: Some("transcript".into()),
            last_activity_age_s: Some(1.0),
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
            harness_title: None,
        }
    }

    const NOW: u64 = 1_800_000_000; // well past any fixture stamp

    #[test]
    fn exited_row_is_done_via_exit() {
        let e = entry(json!({"status": "exited"}));
        assert_eq!(effective_state(&e, NOW), (EffState::Done, "exit"));
        let e = entry(json!({"status": "permanent_dead"}));
        assert_eq!(effective_state(&e, NOW), (EffState::Done, "exit"));
    }

    #[test]
    fn live_hook_maps_state_to_badge() {
        assert_eq!(
            effective_state(&entry(json!({"inside_leg": live_leg("working")})), NOW),
            (EffState::Working, "hook")
        );
        assert_eq!(
            effective_state(&entry(json!({"inside_leg": live_leg("blocked")})), NOW),
            (EffState::Blocked, "hook")
        );
        assert_eq!(
            effective_state(&entry(json!({"inside_leg": live_leg("done")})), NOW),
            (EffState::Done, "hook")
        );
    }

    #[test]
    fn lapsed_hook_is_idle_liveness_not_screen() {
        // A hook-capable row whose report aged out drops to liveness-only; it
        // must NOT fall through to a screen verdict (per-capability arbitration).
        let e = entry(json!({
            "inside_leg": {"state": "working", "seq": 1,
                           "received_at": "2020-01-01T00:00:00Z", "ttl_ms": 1000},
            "screen_state": {"state": "blocked", "rule": "r", "seq": 1,
                             "at": "2026-01-01T00:00:00Z"},
        }));
        assert_eq!(effective_state(&e, NOW), (EffState::Idle, "liveness"));
    }

    #[test]
    fn hookless_row_uses_screen_verdict() {
        let e = entry(json!({
            "screen_state": {"state": "blocked", "rule": "r", "seq": 1,
                             "at": "2026-01-01T00:00:00Z"},
        }));
        assert_eq!(effective_state(&e, NOW), (EffState::Blocked, "screen"));

        let e = entry(json!({
            "screen_state": {"state": "idle", "rule": "r", "seq": 1,
                             "at": "2026-01-01T00:00:00Z"},
        }));
        assert_eq!(effective_state(&e, NOW), (EffState::Idle, "screen"));
    }

    #[test]
    fn bare_row_is_idle_liveness() {
        assert_eq!(
            effective_state(&entry(json!({})), NOW),
            (EffState::Idle, "liveness")
        );
    }

    #[test]
    fn parse_target_rejects_non_targets() {
        assert_eq!(parse_target("idle"), Some(EffState::Idle));
        assert_eq!(parse_target("blocked"), Some(EffState::Blocked));
        assert_eq!(parse_target("done"), Some(EffState::Done));
        assert_eq!(parse_target("working"), None); // transient, not a goal
        assert_eq!(parse_target("bogus"), None);
    }

    #[tokio::test]
    async fn live_claude_hook_done_with_working_truth_stays_pending() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "harness": "claude",
                "harness_session_id": "claude-session",
                "inside_leg": live_leg("done"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "done".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
        ];
        let result =
            run_wait_with_probe(&args, &home, |_handle| Some(truth_probe("working"))).await;
        assert_eq!(result, WAIT_TIMEOUT_EXIT);
    }

    #[tokio::test]
    async fn live_claude_hook_done_with_done_truth_succeeds() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "harness": "claude",
                "harness_session_id": "claude-session",
                "inside_leg": live_leg("done"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "done".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
            "--json".to_string(),
        ];
        let probes = std::sync::atomic::AtomicUsize::new(0);
        let result = run_wait_with_probe(&args, &home, |handle| {
            assert_eq!(handle, "claude-session");
            probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(truth_probe("done"))
        })
        .await;
        assert_eq!(result, 0);
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn live_codex_hook_done_skips_claude_truth_probe() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "harness": "codex",
                "harness_session_id": "codex-session",
                "inside_leg": live_leg("done"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "done".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
        ];
        let result = run_wait_with_probe(&args, &home, |_handle| {
            panic!("Claude truth probe invoked for a Codex row")
        })
        .await;
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn live_claude_blocked_target_keeps_exact_state_semantics() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "harness": "claude",
                "harness_session_id": "claude-session",
                "inside_leg": live_leg("blocked"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "blocked".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
        ];
        let result = run_wait_with_probe(&args, &home, |_handle| {
            panic!("Claude truth probe invoked for a non-done wait target")
        })
        .await;
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn timed_truth_probe_receives_only_remaining_wait_budget() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "harness": "claude",
                "harness_session_id": "claude-session",
                "inside_leg": live_leg("done"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "done".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
        ];
        let observed_budget = std::sync::Mutex::new(None);
        let result = run_wait_with_timed_probe(&args, &home, |_handle, timeout| {
            *observed_budget.lock().expect("budget lock") = Some(timeout);
            None
        })
        .await;
        assert_eq!(result, WAIT_TIMEOUT_EXIT);
        assert!(
            observed_budget
                .lock()
                .expect("budget lock")
                .expect("probe was called")
                <= Duration::from_millis(50)
        );
    }

    #[tokio::test]
    async fn exited_claude_row_is_done_without_truth_probe() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "status": "exited",
                "harness": "claude",
                "harness_session_id": "claude-session",
                "inside_leg": live_leg("done"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "done".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
        ];
        let result = run_wait_with_probe(&args, &home, |_handle| {
            panic!("Claude truth probe invoked for an exited row")
        })
        .await;
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn live_claude_hook_done_requires_done_transcript_state() {
        for transcript_state in ["working", "watching", "your-move", "stalled", "unknown", ""] {
            let dir = tempfile::tempdir().expect("tmpdir");
            let home = AgentsHome::at(dir.path());
            state::update_registry(&home.registry_json(), |registry| {
                registry.entries.push(entry(json!({
                    "harness": "claude",
                    "harness_session_id": "claude-session",
                    "inside_leg": live_leg("done"),
                })));
            })
            .expect("registry fixture writes");

            let args = vec![
                "--agent".to_string(),
                "a".to_string(),
                "--state".to_string(),
                "done".to_string(),
                "--timeout-ms".to_string(),
                "50".to_string(),
            ];
            let result =
                run_wait_with_probe(&args, &home, |_handle| Some(truth_probe(transcript_state)))
                    .await;
            assert_eq!(
                result, WAIT_TIMEOUT_EXIT,
                "transcript state {transcript_state:?}"
            );
        }
    }

    #[tokio::test]
    async fn live_claude_hook_done_probe_failure_stays_pending() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "harness": "claude",
                "harness_session_id": "claude-session",
                "inside_leg": live_leg("done"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "done".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
        ];
        let result = run_wait_with_probe(&args, &home, |_handle| None).await;
        assert_eq!(result, WAIT_TIMEOUT_EXIT);
    }

    #[tokio::test]
    async fn live_claude_hook_done_without_handle_stays_pending() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let home = AgentsHome::at(dir.path());
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(entry(json!({
                "harness": "claude",
                "log_path": "/tmp/claude-session.log",
                "inside_leg": live_leg("done"),
            })));
        })
        .expect("registry fixture writes");

        let args = vec![
            "--agent".to_string(),
            "a".to_string(),
            "--state".to_string(),
            "done".to_string(),
            "--timeout-ms".to_string(),
            "50".to_string(),
        ];
        let result = run_wait_with_probe(&args, &home, |_handle| {
            panic!("Claude truth probe invoked without a resolved handle")
        })
        .await;
        assert_eq!(result, WAIT_TIMEOUT_EXIT);
    }
}
