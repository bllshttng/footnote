//! The claude arm of the stop verb: `stop_claude`, its no-transport-id pid
//! escalation, and the ownership proof every signal in this module rests on.
//! Claude is shellout-managed (LD8): there is no worker PTY to signal, so the
//! daemon shells out to the claude supervisor's `stop` on the agent's short id
//! and marks the registry row exited on success. A stop exit is a receipt, not
//! a proof: the process set comes from claude's own daemon roster and any
//! survivor is ended here, so a live worker is never reported stopped. Moved
//! out of `daemon.rs` for the file budget, beside `rm_teardown`.

use serde_json::json;
use std::time::Duration;

use super::blocking_bound::off_executor;
use super::{
    now_rfc3339_like, pid_gone_within, pid_is_ours, process_start_time, state_error_code,
    update_registry_offloaded, Ctx, Request, Response,
};
use crate::claude_roster::ClaudeRoster;
use crate::protocol::ErrorCode;
use crate::state::{self, RegistryEntry};
use crate::AgentStatus;

/// How long a member gets to exit on its own after the ask, before the first
/// signal. A process that exits inside the grace is reported `shellout`: the
/// ask (or claude's own teardown) did the work.
const STOP_ASK_GRACE: Duration = Duration::from_secs(5);

/// True only when the process at `pid` is PROVABLY the recorded incarnation:
/// signalable, in pid range, AND its live start time reads back exactly
/// `start`. `pid_is_ours` alone trusts bare existence when either start time
/// is unreadable (its `_ => true` arm); that is tolerable for a probe but not
/// as the sole basis for a signal, so every kill here re-proves through this.
fn proved_ours(pid: u32, start: u64) -> bool {
    pid_is_ours(pid, Some(start)) && process_start_time(pid) == Some(start)
}

/// The one process set a stop may end: the roster worker claude names for the
/// session, plus every direct child (the `bg-pty-host` and its `bg-spare` are
/// separate process groups, so a group kill of the host would orphan the
/// session). Pure, so tests inject the roster, the process table and the
/// start-time reader. Refuses on any doubt, and a refusal means no caller in
/// this module signals anything.
fn capture_target(
    roster: &ClaudeRoster,
    short: &str,
    session_id: Option<&str>,
    table: &[crate::census::ProcRow],
    start_of: &dyn Fn(u32) -> Option<u64>,
) -> Result<Vec<(u32, u64)>, String> {
    let matches: Vec<_> = match session_id {
        Some(sid) => roster
            .workers
            .values()
            .filter(|w| w.session_id == sid)
            .collect(),
        None => roster
            .workers
            .values()
            .filter(|w| w.short_id() == short)
            .collect(),
    };
    let worker = match matches.as_slice() {
        [one] => *one,
        [] => {
            let wanted = session_id.unwrap_or(short);
            return Err(format!("no roster worker names {wanted}"));
        }
        [_, _, ..] => return Err("two roster workers match; the target is ambiguous".to_string()),
    };
    let Some(pid) = worker.pid else {
        return Err("the roster worker reports no pid".to_string());
    };
    if pid <= 1 {
        return Err(format!("worker pid {pid} is not a signalable target"));
    }
    if roster.supervisor_pid == Some(pid) {
        return Err("the worker pid is the roster's supervisor pid".to_string());
    }
    if pid == std::process::id() {
        return Err("the worker pid is this daemon".to_string());
    }
    let mut members = vec![pid];
    members.extend(
        table
            .iter()
            .filter(|row| row.ppid == pid)
            .map(|row| row.pid),
    );
    let mut proved = Vec::with_capacity(members.len());
    for member in members {
        if member <= 1 {
            return Err(format!("pid {member} is not a signalable target"));
        }
        match start_of(member) {
            Some(start) => proved.push((member, start)),
            None => return Err(format!("no readable start time for pid {member}")),
        }
    }
    Ok(proved)
}

/// The pre-signal proof the stop verb and the terminal-stop sweep share: load
/// the default roster and the process table (both blocking reads, offloaded
/// from the async handlers) and name the exact process set the stop may end.
/// `Err` means no proof; no caller signals on `Err`. A session under an
/// isolated account root is never proved here, because only the default
/// daemon roster is read.
pub(super) fn prove_target(
    short: &str,
    session_id: Option<&str>,
) -> Result<Vec<(u32, u64)>, String> {
    let short = short.to_string();
    let session_id = session_id.map(str::to_string);
    off_executor(move || {
        let roster = ClaudeRoster::load_default().map_err(|e| format!("roster unreadable: {e}"))?;
        let (table, _) = crate::census::process_table();
        capture_target(
            &roster,
            &short,
            session_id.as_deref(),
            &table,
            &process_start_time,
        )
    })
}

/// End whatever survived the ask. Waits out the grace for each member, then
/// escalates SIGTERM -> SIGKILL with the ownership proof re-run before EVERY
/// signal, so a pid recycled inside a window takes nothing. Returns whether
/// any signal was sent and the pids still proved ours at the end.
pub(super) async fn end_survivors(members: &[(u32, u64)]) -> (bool, Vec<u32>) {
    let mut signalled = false;
    let mut survivors = Vec::new();
    for (pid, start) in members {
        if pid_gone_within(*pid, Some(*start), STOP_ASK_GRACE).await {
            continue;
        }
        if !proved_ours(*pid, *start) {
            // Died in the gap between the wait and the proof: gone, honestly.
            continue;
        }
        signalled = true;
        // SAFETY: ownership proved directly above; SIGTERM to our own worker.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGTERM);
        }
        if pid_gone_within(*pid, Some(*start), Duration::from_secs(5)).await {
            continue;
        }
        if proved_ours(*pid, *start) {
            // SAFETY: ownership re-proved after the grace, so a pid recycled
            // during it takes no signal.
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGKILL);
            }
        }
        if !pid_gone_within(*pid, Some(*start), Duration::from_secs(2)).await
            && proved_ours(*pid, *start)
        {
            survivors.push(*pid);
        }
    }
    (signalled, survivors)
}

/// The one registry write every confirmed claude stop ends in: Exited, the
/// exit stamp, and the stop record that says fno did this (the row's memory
/// the reachability gate reads, so a stopped row is not live for another 20
/// minutes). The error is the caller's response: a persist failure surfaces,
/// it never reads as a clean stop (silent-failure review).
pub(super) async fn mark_claude_stopped(
    ctx: &Ctx,
    req: &Request,
    name: &str,
) -> Result<(), Response> {
    let claude_name = name.to_string();
    update_registry_offloaded(ctx.home.registry_json(), move |r| {
        if let Some(e) = r.find_mut(&claude_name) {
            e.status = AgentStatus::Exited;
            e.exited_at = Some(now_rfc3339_like());
            state::record_stop(e, "stop-verb", Some("claude".into()));
        }
    })
    .await
    .map_err(|e| {
        Response::err(
            req.id,
            state_error_code(&e),
            format!("claude {name} stopped but registry write failed: {e}"),
        )
    })
}

/// Stop a claude row that has a recorded pid but no transport id, with the same
/// SIGTERM -> SIGKILL escalation `stop_worker_confirmed` uses. Returns true iff
/// the process is confirmed gone.
///
/// A row can carry a live process and no short id at all when the spawn receipt
/// never yielded one. Refusing there left the operator with a running worker and
/// no verb that addressed it -- the duplicate-worker half of the wave-boundary
/// handoff failure, which had to be killed by hand to restore one-writer
/// semantics. Unlike a PTY worker there is no socket to probe, so `pid_is_ours`
/// (which rejects pid <= 1, treats an unsignalable pid as not ours, and compares
/// the recorded start time) is both the liveness oracle and the recycle guard.
/// It is re-proved before EVERY signal so a pid recycled inside the grace window
/// is never killed.
pub(super) async fn stop_claude_pid_confirmed(entry: &RegistryEntry) -> bool {
    let Some(pid) = entry.pid else {
        return false;
    };
    // Require the incarnation token. Without it `pid_is_ours` falls back to bare
    // liveness, which cannot tell our worker from an unrelated process that
    // inherited the pid after it died. That is tolerable for a probe; it is not
    // tolerable as the sole basis for SIGKILL. Refusing costs a legacy row an
    // honest "cannot stop" message. Guessing costs someone else's process.
    let Some(start) = entry.pid_start_time else {
        return false;
    };
    if !proved_ours(pid, start) {
        return false;
    }
    // SAFETY: pid ownership proved directly above; SIGTERM to our own worker.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    if pid_gone_within(pid, entry.pid_start_time, Duration::from_secs(5)).await {
        return true;
    }
    if proved_ours(pid, start) {
        // SAFETY: ownership re-proved after the grace window, so a pid recycled
        // during it takes no signal.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    pid_gone_within(pid, entry.pid_start_time, Duration::from_secs(2)).await
}

/// Stop a Claude agent (AC7-EDGE). Claude is shellout-managed (LD8): there is no
/// worker PTY to signal, so the daemon shells out to the claude supervisor's
/// `stop` on the agent's short id and marks the registry row exited on success.
pub(super) async fn stop_claude(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    entry: &RegistryEntry,
) -> Response {
    let short = match entry
        .transport_short()
        .or(entry.session_id.as_deref())
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_string(),
        None => {
            // No transport id: fall back to signalling the recorded pid rather
            // than refusing a row whose process is still running.
            if stop_claude_pid_confirmed(entry).await {
                if let Err(response) = mark_claude_stopped(ctx, req, name).await {
                    return response;
                }
                let _ = ctx.emitter.emit(
                    "agent_stopped",
                    &json!({"name": name, "backend": "claude", "stopped_by": "pid"}),
                );
                return Response::ok(
                    req.id,
                    json!({"stopped": true, "backend": "claude", "pid": entry.pid}),
                );
            }
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!(
                    "agent {name} is claude but has no short id and no live process \
                     to stop. `rm` will refuse this row too while it is stored live, so \
                     stopping has no exit here: the row can neither prove liveness \
                     nor be addressed. The override for that case is documented in \
                     `fno agents rm --help`, not here."
                ),
            );
        }
    };
    // Capture the target BEFORE the ask, so the start times are the pre-stop
    // incarnations. A refused capture changes nothing: today's ask-only
    // receipt runs, labelled with the reason no proof existed.
    let target = prove_target(&short, entry.harness_session_id.as_deref());
    // Bound the subprocess so a hung `claude` can never wedge this RPC
    // handler, the same way the background-sweep twin is bounded.
    let ask = crate::lifecycle_child::bounded_claude_stop(&short, Duration::from_secs(15)).await;
    match target {
        Ok(members) => {
            // Whatever the ask returned, the proof decides: a process that is
            // gone is gone, and a survivor is never reported stopped.
            let (signalled, survivors) = end_survivors(&members).await;
            let pids: Vec<u32> = members.iter().map(|(pid, _)| *pid).collect();
            if survivors.is_empty() {
                if let Err(response) = mark_claude_stopped(ctx, req, name).await {
                    return response;
                }
                let stopped_by = if signalled { "pid" } else { "shellout" };
                let _ = ctx.emitter.emit(
                    "agent_stopped",
                    &json!({
                        "name": name,
                        "backend": "claude",
                        "stopped_by": stopped_by,
                        "pids": pids,
                    }),
                );
                // Report the id we actually stopped with (`short`), not
                // `entry.short_id`: a row with only a generic session_id and an
                // empty short_id would otherwise print `stopped: <name> ()` and
                // break the stop output contract (Codex P2).
                return Response::ok(
                    req.id,
                    json!({
                        "stopped": true,
                        "backend": "claude",
                        "short_id": short,
                        "stopped_by": stopped_by,
                        "pids": pids,
                    }),
                );
            }
            let listed = survivors
                .iter()
                .map(|pid| pid.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let _ = ctx.emitter.emit(
                "agent_stop_refused",
                &json!({"name": name, "pids": survivors}),
            );
            Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!(
                    "claude stop {short} returned but pid {listed} for {name} survived \
                     SIGTERM and SIGKILL; the row stays live. The documented override \
                     for a row whose process survives every stop is `fno agents rm`."
                ),
            )
        }
        Err(reason) => match ask {
            Err(_) => Response::err(
                req.id,
                ErrorCode::Internal,
                // retired-ok: reports which shellout timed out, not a step to run.
                format!("claude stop {short} timed out"),
            ),
            Ok(Ok(o)) if o.status.success() => {
                if let Err(response) = mark_claude_stopped(ctx, req, name).await {
                    return response;
                }
                let _ = ctx
                    .emitter
                    .emit("agent_stopped", &json!({"name": name, "backend": "claude"}));
                Response::ok(
                    req.id,
                    json!({
                        "stopped": true,
                        "backend": "claude",
                        "short_id": short,
                        "process_proof": format!("unavailable: {reason}"),
                    }),
                )
            }
            Ok(Ok(o)) => Response::err(
                req.id,
                ErrorCode::Internal,
                format!(
                    // retired-ok: reports which shellout failed, not a step to run.
                    "claude stop {short} failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            ),
            Ok(Err(e)) => Response::err(
                req.id,
                ErrorCode::Internal,
                format!("could not exec `claude stop`: {e}"),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_roster::RosterWorker;

    /// AC1-HP: the proof holds for a real incarnation and fails on a token
    /// that differs by one.
    #[test]
    fn proved_ours_demands_the_exact_start_time() {
        let pid = std::process::id();
        let Some(start) = process_start_time(pid) else {
            return; // platform cannot read start times: nothing provable here
        };
        assert!(proved_ours(pid, start));
        assert!(!proved_ours(pid, start.wrapping_add(1)));
    }

    fn roster_with(session_id: &str, pid: Option<u32>) -> ClaudeRoster {
        let mut roster = ClaudeRoster {
            proto: 1,
            supervisor_pid: Some(4242),
            updated_at: None,
            workers: Default::default(),
        };
        let worker = RosterWorker {
            session_id: session_id.to_string(),
            pid,
            proc_start: None,
            pty_sock: None,
            pty_auth: None,
            cli_version: None,
            cwd: String::new(),
            worktree_path: None,
        };
        roster.workers.insert(worker.short_id().to_string(), worker);
        roster
    }

    fn start_of_every(pid: u32) -> Option<u64> {
        Some(u64::from(pid) + 7)
    }

    /// AC2-HP: the worker plus its direct children, each with a start time.
    #[test]
    fn capture_target_names_the_worker_and_its_children() {
        let roster = roster_with("ee99ff00-7777-8888-9999-aaaabbbbcccc", Some(5002));
        let table = vec![
            crate::census::test_proc_row(5002, 1, "claude bg-pty-host"),
            crate::census::test_proc_row(5100, 5002, "claude bg-spare"),
        ];
        let got = capture_target(
            &roster,
            "ee99ff00",
            Some("ee99ff00-7777-8888-9999-aaaabbbbcccc"),
            &table,
            &start_of_every,
        )
        .expect("proved target");
        assert_eq!(got, vec![(5002, 5009), (5100, 5107)]);
    }

    /// AC2-ERR: a same-short worker with a different session id refuses and
    /// names the mismatch.
    #[test]
    fn capture_target_refuses_a_session_id_mismatch() {
        let roster = roster_with("ee99ff00-other-session-uuid", Some(5002));
        let err = capture_target(
            &roster,
            "ee99ff00",
            Some("ee99ff00-7777-8888-9999-aaaabbbbcccc"),
            &[],
            &start_of_every,
        )
        .expect_err("mismatch refuses");
        assert!(
            err.contains("ee99ff00-7777-8888-9999-aaaabbbbcccc"),
            "{err}"
        );
    }

    /// AC2-ERR: the supervisor pid and this daemon's pid are never targets.
    #[test]
    fn capture_target_refuses_the_supervisor_and_the_daemon() {
        let roster = roster_with("ee99ff00-7777-8888-9999-aaaabbbbcccc", Some(4242));
        assert!(
            capture_target(&roster, "ee99ff00", None, &[], &start_of_every).is_err(),
            "supervisor pid refuses"
        );
        let roster = roster_with(
            "ee99ff00-7777-8888-9999-aaaabbbbcccc",
            Some(std::process::id()),
        );
        assert!(
            capture_target(&roster, "ee99ff00", None, &[], &start_of_every).is_err(),
            "the daemon's own pid refuses"
        );
    }

    /// AC2-ERR: one unreadable start time refuses the whole set, so nothing
    /// is ever signalled half-proved.
    #[test]
    fn capture_target_refuses_an_unreadable_start_time() {
        let roster = roster_with("ee99ff00-7777-8888-9999-aaaabbbbcccc", Some(5002));
        let table = vec![crate::census::test_proc_row(5100, 5002, "claude bg-spare")];
        let err = capture_target(&roster, "ee99ff00", None, &table, &|p| {
            (p != 5100).then_some(1)
        })
        .expect_err("unreadable start time refuses");
        assert!(err.contains("5100"), "{err}");
    }

    /// AC2-EDGE: no session id on the row and two workers sharing the short
    /// id: ambiguous, refused. The roster map is keyed by the short id, so a
    /// second insert under the same key would silently replace the first;
    /// the fixture uses distinct keys, the way a torn roster could list one
    /// session twice.
    #[test]
    fn capture_target_refuses_an_ambiguous_short_id() {
        let first = roster_with("ee99ff00-7777-8888-9999-aaaabbbbcccc", Some(5002));
        let second = RosterWorker {
            session_id: "ee99ff00-dead-beef-4242-aaaabbbbcccc".to_string(),
            pid: Some(5003),
            ..first.workers.values().next().unwrap().clone()
        };
        let mut roster = ClaudeRoster {
            proto: 1,
            supervisor_pid: Some(4242),
            updated_at: None,
            workers: Default::default(),
        };
        roster.workers.insert(
            "torn-one".to_string(),
            first.workers.values().next().unwrap().clone(),
        );
        roster.workers.insert("torn-two".to_string(), second);
        assert!(capture_target(&roster, "ee99ff00", None, &[], &start_of_every).is_err());
    }

    /// AC3-HP: a detached TERM-ignoring host and its child, captured as the
    /// target, both end; the independent ps oracle decides death, and the
    /// answer reports the signal.
    #[tokio::test]
    async fn end_survivors_ends_a_sigterm_ignoring_host_and_its_child() {
        // The host traps TERM and stays alive over a detached sleeper child
        // (the redirect keeps the pipe from holding anything open).
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 60 >/dev/null 2>&1 & echo $$ $!; wait")
            .output()
            .expect("spawn detached pair");
        let fields: Vec<u32> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        let (host, child) = (fields[0], fields[1]);
        let start_of = |pid: u32| process_start_time(pid);
        let Some(host_start) = start_of(host) else {
            unsafe {
                libc::kill(host as libc::pid_t, libc::SIGKILL);
            }
            return;
        };
        let members = vec![(host, host_start), (child, start_of(child).unwrap_or(1))];

        let (signalled, survivors) = end_survivors(&members).await;

        let ps_says_alive = |pid: u32| {
            std::process::Command::new("ps")
                .args(["-p", &pid.to_string()])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter(|l| l.split_whitespace().next() == Some(&pid.to_string()))
                        .count()
                        > 0
                })
                .unwrap_or(false)
        };
        assert!(signalled, "an ask that did nothing leaves the signal to us");
        assert!(survivors.is_empty(), "no survivor survives SIGKILL");
        assert!(!ps_says_alive(host), "host gone by the oracle");
        assert!(!ps_says_alive(child), "child gone by the oracle");
    }

    /// AC3-ERR: a recorded start time that does not match the live process
    /// sends no signal; the pid reads as recycled, not killed.
    #[tokio::test]
    async fn end_survivors_never_signals_a_recycled_pid() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 60 >/dev/null 2>&1 & echo $!")
            .output()
            .expect("spawn detached sleeper");
        let pid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("sleeper pid");
        let Some(start) = process_start_time(pid) else {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            return;
        };
        let members = vec![(pid, start.wrapping_add(1))];

        let (signalled, survivors) = end_survivors(&members).await;

        assert!(!signalled, "a recycled pid takes no signal");
        assert!(survivors.is_empty(), "reported gone as recycled");
        let alive = std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        assert!(alive, "the process must still be alive");
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}
