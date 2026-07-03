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
    let reg = state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    Ok(reg
        .entries
        .iter()
        .find(|e| e.name == name)
        .map(|e| effective_state(e, now)))
}

/// `fno-agents wait --agent <name> --state idle|blocked|done [--timeout-ms N] [--json]`
pub async fn run_wait(rest: &[String], home: &AgentsHome) -> i32 {
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

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match find_effective(home, &name, now_secs()) {
            Ok(Some((st, authority))) => {
                if st == target_state {
                    if json_out {
                        println!("{}", json!({"state": st.label(), "authority": authority}));
                    } else {
                        println!("{name} is {} (via {authority})", st.label());
                    }
                    return 0;
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
        if Instant::now() >= deadline {
            // Report the last-observed state (one read; the timeout path is rare).
            let last = find_effective(home, &name, now_secs())
                .ok()
                .flatten()
                .map(|(s, _)| s.label())
                .unwrap_or("unknown");
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

    // A report with no ttl_ms never ages out -> always live at any `now`.
    fn live_leg(state: &str) -> Value {
        json!({"state": state, "seq": 1, "received_at": "2026-01-01T00:00:00Z"})
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
}
