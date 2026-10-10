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
        BoundedRun::SpawnFailed(_) | BoundedRun::WaitFailed | BoundedRun::Refused => {
            FidelityGate::Absent
        }
        BoundedRun::TimedOut(elapsed, _) => FidelityGate::Degraded {
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

/// The green-conjunct stop read for one fire: the plan-fidelity gate, or the
/// delegated-merge park that skips it.
///
/// A delegated merge (a per-run no-merge manifest, or a valid team ruling
/// hold on this node) skips the stop-time read: someone else merges, the
/// merge gate re-runs the same fidelity join at merge time, and DonePRGreen
/// is a shipped terminal, so the later join sees a delivered row rather than
/// stranding a row on a non-shipped terminal no later merge ever restamps.
/// The park names who merges so a human (or the merge queue) owns the next
/// act.
pub(super) struct GreenRead {
    /// The fidelity refusal that blocks DonePRGreen, if any.
    pub block: Option<String>,
    /// Set only when the merge is delegated: who lands this PR now.
    pub merge_owner: Option<String>,
}

pub(super) fn green_conjunct_read(
    manifest_no_merge: bool,
    manifest_source: Option<&str>,
    node_id: Option<&str>,
    plan_path: Option<&str>,
    cwd: &Path,
    pr_number: i64,
    head_oid: &str,
    timeout: std::time::Duration,
    fno_bin: &std::ffi::OsStr,
    mut on_degraded: impl FnMut(String),
) -> GreenRead {
    let ruling = node_id.and_then(super::awaiting_merge::ruling_hold);
    if manifest_no_merge || ruling.is_some() {
        let owner = if manifest_no_merge {
            let source = manifest_source.unwrap_or("unknown");
            let repo_slug = crate::finalize::slug_from_git_remote(cwd).unwrap_or_default();
            format!(
                "the operator (per-run no-merge, source {source}); attended grant: {}",
                crate::merge_grant::attended_grant_command(&repo_slug, pr_number, head_oid)
            )
        } else {
            format!("the ruling {}", ruling.unwrap_or_default())
        };
        return GreenRead {
            block: None,
            merge_owner: Some(owner),
        };
    }
    let mut block = None;
    match evaluate_plan_fidelity(plan_path, fno_bin, cwd, timeout) {
        FidelityGate::Refused { reason } => block = Some(reason),
        // Degraded fails OPEN on the stop decision (same as Absent - a hung
        // probe must not wedge the gate that lets a finished session stop),
        // but is emitted so it is never a SILENT pass: a probe that keeps
        // timing out stays visible in the event log even though it never
        // blocks.
        FidelityGate::Degraded { reason } => on_degraded(reason),
        _ => {}
    }
    GreenRead {
        block,
        merge_owner: None,
    }
}
