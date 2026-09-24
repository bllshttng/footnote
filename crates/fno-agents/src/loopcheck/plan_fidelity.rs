//! Does the plan's fidelity gate pass?

use super::*;

/// Plan-fidelity stop gate. The stop-gate half of AC5; the merge gate
/// (`_merge.py`, which imports the core in-process) is the other. Shells
/// `fno do plan fidelity --json <plan_path>` and blocks DonePRGreen when a planned
/// deliverable is unjoined and uncovered by a carveout - the agent must file a
/// carveout before it may stop. Mirrors `ProbeGate`'s shape deliberately.
#[derive(Debug)]
pub(super) enum FidelityGate {
    /// No plan, or the probe degraded. A missing/stale `fno` (one without the
    /// `plan fidelity` verb) must NOT wedge the stop gate - the merge gate is the
    /// backstop, and `fno doctor` flags the staleness. Fail open here.
    Absent,
    Pass,
    Refused {
        reason: String,
    },
    /// The child ran past `FIDELITY_TIMEOUT` and was killed. Fail
    /// open on the STOP decision like `Absent` - a hung probe must not wedge
    /// the gate that exists to let a finished session finally stop - but,
    /// unlike `Absent`, this is NAMED and carried into the emitted event so a
    /// timing-out probe is visible, never a silent pass indistinguishable
    /// from "no plan bound".
    Degraded {
        reason: String,
    },
}

/// Wall-clock ceiling for the `fno do plan fidelity` child. Same bound
/// as `PROBE_TIMEOUT`, under its own name because this and done_probes gate
/// different things and must be free to drift independently.
pub(super) const FIDELITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run `fno do plan fidelity --json` for the bound plan and classify the decision.
///
/// Fail-open on every error path (no fno, non-zero exit, unparseable JSON,
/// timeout): the stop gate must not block on a broken probe. The inversion
/// lives in the Python core (`fno.plan.fidelity`); Rust only reads the
/// `refused` bool, so there is one implementation of the join and the gate
/// and the loop cannot drift. `fno_bin` is resolved by the caller (from
/// `FNO_LOOPCHECK_FNO_BIN`, default `fno`) so this function is hermetically
/// testable with a stub script.
pub(super) fn evaluate_plan_fidelity(
    plan_path: Option<&str>,
    fno_bin: &OsStr,
    cwd: &Path,
    timeout: std::time::Duration,
) -> FidelityGate {
    let plan = match plan_path {
        Some(p) if !p.is_empty() => p,
        _ => return FidelityGate::Absent,
    };
    match run_bounded(
        fno_bin,
        &["do", "plan", "fidelity", plan, "--json"],
        cwd,
        timeout,
    ) {
        BoundedRun::Completed(out) => classify_plan_fidelity(&out.stdout),
        BoundedRun::SpawnFailed(_) | BoundedRun::WaitFailed => FidelityGate::Absent,
        BoundedRun::TimedOut(elapsed) => FidelityGate::Degraded {
            reason: format!(
                "plan fidelity check timed out after {:.1}s running `{} do plan fidelity {} --json` \
                 and was killed; degraded, not a pass - the fno CLI itself is hanging, \
                 investigate that command directly",
                elapsed.as_secs_f64(),
                fno_bin.to_string_lossy(),
                plan,
            ),
        },
    }
}

pub(super) fn classify_plan_fidelity(stdout: &[u8]) -> FidelityGate {
    let v: Value = match serde_json::from_slice(stdout) {
        Ok(v) => v,
        Err(_) => return FidelityGate::Absent,
    };
    match v.get("refused").and_then(|r| r.as_bool()) {
        Some(true) => FidelityGate::Refused {
            reason: v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("plan has unjoined deliverables with no covering carveout")
                .to_string(),
        },
        _ => FidelityGate::Pass,
    }
}
