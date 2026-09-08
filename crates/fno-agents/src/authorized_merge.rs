//! One authorized merge operation for both merge paths.
//!
//! Two callers used to answer "may this exact PR head be merged?" on their own.
//! `fno do pr merge` ran a long guard chain and then merged. `finalize` armed
//! GitHub's auto-merge queue after a shorter, different chain: it never read the
//! in-flight review hold, and it dropped the head pin when the covered head was
//! unreadable, which armed the queue on whatever head landed next.
//!
//! This module is the single owner. Both callers hand it a [`Request`] and read
//! one [`Outcome`] back. The decision is the same for an immediate merge and a
//! queue arm; only the effect differs, and the receipt keeps them apart:
//! `Armed` is a queue entry, never proof that a merge landed.
//!
//! Every read is one guarded fetch or one probe with a stated failure polarity.
//! An unreadable instrument is `Unknown`, never a clear answer.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the caller wants to happen once the decision clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Merge now (`fno do pr merge`).
    Merge,
    /// Arm GitHub's auto-merge queue (`finalize` at a green terminal).
    Arm,
}

impl Effect {
    fn word(self) -> &'static str {
        match self {
            Effect::Merge => "merge",
            Effect::Arm => "arm",
        }
    }

    fn parse(raw: &str) -> Option<Effect> {
        match raw {
            "merge" => Some(Effect::Merge),
            "arm" => Some(Effect::Arm),
            _ => None,
        }
    }
}

/// The receipt. `Armed` and `Merged` never collapse into one another: a queue
/// entry is a promise GitHub may keep later, and reporting it as a merge is how
/// a caller stops watching a PR that has not landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Merged {
        head: String,
    },
    Armed {
        head: String,
    },
    /// The decision cleared and the caller asked to stop there. Nothing ran.
    Authorized {
        head: String,
    },
    /// Retryable. The same command later can succeed.
    Held {
        reason: String,
    },
    /// Needs an operator action. Retrying changes nothing.
    Refused {
        reason: String,
    },
    /// The head moved between validation and the effect.
    HeadChanged {
        expected: String,
        actual: String,
    },
    /// An instrument could not answer. Never a verdict.
    Unknown {
        reason: String,
    },
    /// The effect ran and failed.
    Failed {
        reason: String,
    },
}

impl Outcome {
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::Merged { .. } => "merged",
            Outcome::Armed { .. } => "armed",
            Outcome::Authorized { .. } => "authorized",
            Outcome::Held { .. } => "held",
            Outcome::Refused { .. } => "refused",
            Outcome::HeadChanged { .. } => "head_changed",
            Outcome::Unknown { .. } => "unknown",
            Outcome::Failed { .. } => "failed",
        }
    }

    /// The reason line, or the head for the arms that carry one.
    pub fn detail(&self) -> String {
        match self {
            Outcome::Merged { head } | Outcome::Armed { head } | Outcome::Authorized { head } => {
                head.clone()
            }
            Outcome::Held { reason }
            | Outcome::Refused { reason }
            | Outcome::Unknown { reason }
            | Outcome::Failed { reason } => reason.clone(),
            Outcome::HeadChanged { expected, actual } => format!(
                "the PR head moved from {expected} to {actual} between validation and the effect"
            ),
        }
    }

    /// Whether the queue entry or the merge actually happened.
    pub fn effected(&self) -> bool {
        matches!(self, Outcome::Merged { .. } | Outcome::Armed { .. })
    }

    pub fn to_json(&self) -> Value {
        let mut out = json!({ "outcome": self.word(), "detail": self.detail() });
        match self {
            Outcome::Merged { head } | Outcome::Armed { head } | Outcome::Authorized { head } => {
                out["head"] = json!(head)
            }
            Outcome::HeadChanged { expected, actual } => {
                out["expected_head"] = json!(expected);
                out["actual_head"] = json!(actual);
            }
            _ => {}
        }
        out
    }
}

/// One authorized-merge ask.
#[derive(Debug, Clone)]
pub struct Request {
    pub cwd: PathBuf,
    /// The PR number, or `None` to resolve the current branch's open PR.
    pub pr: Option<u64>,
    pub effect: Effect,
    /// The caller's manifest fold of `auto_merge_approved`. `Some(false)` is a
    /// per-run refusal and outranks every grant. `None` means no manifest said
    /// anything, so the live config decides on its own.
    pub approved: Option<bool>,
    /// `auto_merge_source` from the same manifest, named in the refusal so an
    /// operator can see which layer said no.
    pub auto_merge_source: Option<String>,
    /// Verify CI before the effect. The queue enforces checks server-side, so
    /// the arm path leaves this false; `fno do pr merge` sets it from
    /// `auto_merge.require_checks_pass`.
    pub require_checks: bool,
    /// The head the caller's own coverage gate answered for. Used when present
    /// so the gate and the pin cannot describe two different commits; absent,
    /// the owner reads the covered head from the event journal itself.
    pub covered_head: Option<String>,
    /// Authorize and stop. The caller runs its own pre-effect side effects (the
    /// coverage status receipt) and then asks again for the effect, which
    /// re-runs the whole chain. Nothing is merged or armed on this pass.
    pub decide_only: bool,
}

/// A probe that either cleared, refused, or could not evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Refused(String),
    Clear,
    /// The probe ran and could not evaluate. NEVER maps to Clear.
    Inconclusive(String),
}

impl ProbeOutcome {
    /// An unevaluated probe refuses. The hold probes take this polarity: a merge
    /// taken while something unseen was still writing is the whole defect.
    pub fn fail_closed(self) -> Option<String> {
        match self {
            ProbeOutcome::Refused(reason) | ProbeOutcome::Inconclusive(reason) => Some(reason),
            ProbeOutcome::Clear => None,
        }
    }

    /// An unevaluated probe proceeds, with a breadcrumb. The lineage probe takes
    /// this polarity: refusing on a gh hiccup turns auto-merge into something
    /// that silently never works, which reads exactly like nobody opting in.
    pub fn fail_open(self) -> Option<String> {
        match self {
            ProbeOutcome::Refused(reason) => Some(reason),
            ProbeOutcome::Clear | ProbeOutcome::Inconclusive(_) => None,
        }
    }
}

/// What one guarded fetch of the PR says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrFacts {
    pub number: u64,
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    /// `OPEN`, `MERGED`, or `CLOSED`.
    pub state: String,
    /// GitHub's auto-merge queue already owns this PR. Rides the same pulls
    /// payload as the head, so no second probe can disagree with it about which
    /// head it describes.
    pub armed: bool,
}

/// The outside world, injectable so the decision is testable without a network.
pub trait Probes {
    fn pr_facts(&self, cwd: &Path, pr: Option<u64>) -> Result<PrFacts, String>;
    fn dispatch_hold(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    fn review_hold(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    fn base_lineage(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    /// `green` | `red` | `pending` | `unknown`.
    fn checks_verdict(&self, cwd: &Path, pr: u64) -> String;
    fn covered_head(&self, cwd: &Path) -> Option<String>;
    fn auto_merge_enabled(&self, cwd: &Path) -> bool;
    fn posture_floor_block(&self, cwd: &Path) -> Option<String>;
    fn strategy(&self, cwd: &Path) -> String;
    /// Run `gh` with these arguments. `Ok((success, combined_output))`.
    fn run_gh(&self, cwd: &Path, args: &[String]) -> Result<(bool, String), String>;
}

/// A cleared decision: the effect may run, pinned to this head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorized {
    pub facts: PrFacts,
    pub head: String,
    pub strategy: String,
}

/// The whole authorization. Order matters: the refusals that need an operator
/// come first, then the holds, then the head pin, then the effect's own
/// preconditions.
pub fn decide<P: Probes>(probes: &P, request: &Request) -> Result<Authorized, Outcome> {
    let cwd = request.cwd.as_path();
    let facts = probes
        .pr_facts(cwd, request.pr)
        .map_err(|reason| Outcome::Unknown { reason })?;

    // A merged or closed PR has no would-merge left. Every guard below protects
    // what WOULD merge, so answering "unreviewed" here sends a caller hunting a
    // defect that is blocking nothing.
    if facts.state == "MERGED" || facts.state == "CLOSED" {
        return Err(Outcome::Held {
            reason: format!(
                "PR {} is already {}; nothing to merge",
                facts.number,
                facts.state.to_lowercase()
            ),
        });
    }

    if let Some(refusal) = authority_refusal(probes, cwd, request) {
        return Err(Outcome::Refused { reason: refusal });
    }

    if let Some(blocked) = probes.dispatch_hold(cwd, facts.number).fail_closed() {
        return Err(Outcome::Held { reason: blocked });
    }

    // The in-flight review hold. Coverage answers what verdicts EXIST for a
    // head; it cannot say that a review is executing right now with its findings
    // uncommitted. The arm path never read this before, so a queue armed at the
    // terminal shipped the code a review was still fixing.
    if let Some(blocked) = probes.review_hold(cwd, facts.number).fail_closed() {
        return Err(Outcome::Held { reason: blocked });
    }

    // The pin. An unreadable covered head refuses instead of falling back to an
    // unpinned effect: an unpinned arm lets a racing push land an unreviewed
    // head through GitHub's queue, which is the failure this owner exists for.
    let Some(head) = request
        .covered_head
        .clone()
        .or_else(|| probes.covered_head(cwd))
        .filter(|sha| !sha.is_empty())
    else {
        return Err(Outcome::Unknown {
            reason: format!(
                "no covered head is readable for PR {}; refusing an unpinned {}",
                facts.number,
                request.effect.word()
            ),
        });
    };
    if head != facts.head_sha {
        return Err(Outcome::HeadChanged {
            expected: head,
            actual: facts.head_sha,
        });
    }

    if let Some(reason) = probes.base_lineage(cwd, facts.number).fail_open() {
        return Err(Outcome::Refused {
            reason: format!("stale base: {reason}"),
        });
    }

    if request.require_checks {
        match probes.checks_verdict(cwd, facts.number).as_str() {
            "green" => {}
            verdict @ ("pending" | "unknown") => {
                return Err(Outcome::Held {
                    reason: format!(
                        "checks are {verdict}; require_checks_pass forbids merging without green"
                    ),
                })
            }
            verdict => {
                return Err(Outcome::Failed {
                    reason: format!(
                        "checks are {verdict}; require_checks_pass forbids merging without green"
                    ),
                })
            }
        }
    }

    let strategy = probes.strategy(cwd);
    Ok(Authorized {
        facts,
        head,
        strategy,
    })
}

/// The posture fold, in the order init folds it. A per-run refusal outranks
/// every grant; an explicit per-run env grant satisfies the standing arm on its
/// own; otherwise the LIVE config decides, so a manifest snapshot never outlives
/// an operator flipping the switch off mid-flight.
fn authority_refusal<P: Probes>(probes: &P, cwd: &Path, request: &Request) -> Option<String> {
    let source = request
        .auto_merge_source
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown (pre-provenance manifest)");
    if request.approved == Some(false) {
        return Some(format!(
            "per-run no-merge (manifest auto_merge_approved is not true; auto_merge_source: \
             {source}); sanctioned override: an out-of-band merge by the operator, or re-arm \
             the run's dispatch (attended and without --no-merge)"
        ));
    }
    let env_grant = request.approved == Some(true) && source == "env-target-auto-merge";
    if !env_grant && !probes.auto_merge_enabled(cwd) {
        return Some(
            "auto_merge disabled (live config resolves auto_merge.enabled=false); sanctioned \
             override (operator levers): `fno config set auto_merge.enabled true`, or start the \
             run with TARGET_AUTO_MERGE=1 from the operator's shell"
                .to_string(),
        );
    }
    probes.posture_floor_block(cwd)
}

/// Decide, then run the effect unless the caller asked to stop at the decision.
pub fn run<P: Probes>(probes: &P, request: &Request) -> Outcome {
    match decide(probes, request) {
        Ok(authorized) if request.decide_only => Outcome::Authorized {
            head: authorized.head,
        },
        Ok(authorized) => effect(probes, request, &authorized),
        Err(outcome) => outcome,
    }
}

fn effect<P: Probes>(probes: &P, request: &Request, authorized: &Authorized) -> Outcome {
    let cwd = request.cwd.as_path();
    let number = authorized.facts.number;
    if authorized.facts.armed {
        return match request.effect {
            // Re-arming is a no-op on GitHub's side, so the receipt says armed
            // without spending a request.
            Effect::Arm => Outcome::Armed {
                head: authorized.head.clone(),
            },
            // Merging now would race the queue that already owns this PR.
            Effect::Merge => Outcome::Held {
                reason: format!(
                    "PR {number} is already armed in GitHub's auto-merge queue; \
                     the queue merges it when checks pass"
                ),
            },
        };
    }

    let strategy = &authorized.strategy;
    let mut args = vec!["pr".to_string(), "merge".to_string(), number.to_string()];
    if request.effect == Effect::Arm {
        args.push("--auto".to_string());
    }
    args.push(format!("--{strategy}"));
    // Never optional. gh refuses the whole call if the head moved, which is the
    // last guard between this decision and a racing push.
    args.push("--match-head-commit".to_string());
    args.push(authorized.head.clone());

    let (ok, output) = match probes.run_gh(cwd, &args) {
        Ok(result) => result,
        Err(error) => {
            return Outcome::Failed {
                reason: format!("gh {} failed to run: {error}", request.effect.word()),
            }
        }
    };
    if ok {
        return match request.effect {
            Effect::Arm => Outcome::Armed {
                head: authorized.head.clone(),
            },
            Effect::Merge => Outcome::Merged {
                head: authorized.head.clone(),
            },
        };
    }

    // gh exits non-zero on an already-merged PR, and on a post-merge step that
    // failed after the server-side merge landed. Re-read rather than match the
    // error phrasing: the durable signal is the PR's own state.
    match probes.pr_facts(cwd, Some(number)) {
        Ok(after) if after.state == "MERGED" => Outcome::Merged {
            head: authorized.head.clone(),
        },
        Ok(after) if after.head_sha != authorized.head => Outcome::HeadChanged {
            expected: authorized.head.clone(),
            actual: after.head_sha,
        },
        Ok(_) => {
            // gh refuses before merging when the branch is checked out in
            // another worktree, which is every worktree-first run. Recover
            // through the REST endpoint, carrying the same pin: `sha` is that
            // endpoint's `--match-head-commit`, so the retry cannot quietly
            // land a head the decision never saw.
            if request.effect == Effect::Merge && checkout_refused(&output) {
                let api = vec![
                    "api".to_string(),
                    "--method".to_string(),
                    "PUT".to_string(),
                    format!("repos/{{owner}}/{{repo}}/pulls/{number}/merge"),
                    "-f".to_string(),
                    format!("merge_method={strategy}"),
                    "-f".to_string(),
                    format!("sha={}", authorized.head),
                ];
                if let Ok((true, _)) = probes.run_gh(cwd, &api) {
                    return Outcome::Merged {
                        head: authorized.head.clone(),
                    };
                }
            }
            classify_failure(request.effect, strategy, &output)
        }
        // The state never became readable, so a landed merge and a failed one
        // look the same. Unknown keeps that in the receipt.
        Err(reason) => Outcome::Unknown {
            reason: format!(
                "merge state unreadable after a failed {}: {reason}",
                request.effect.word()
            ),
        },
    }
}

/// gh's two spellings for "that branch is checked out somewhere else". Matching
/// phrasing is only safe here because the state read above already proved the
/// merge did NOT land, so a wrong guess costs one refused API call.
fn checkout_refused(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower.contains("is already used by worktree") || lower.contains("already checked out")
}

fn classify_failure(effect: Effect, strategy: &str, output: &str) -> Outcome {
    let first = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no error output");
    let lower = output.to_lowercase();
    let reason = if lower.contains("not mergeable") {
        "not mergeable (conflicts or base changed)".to_string()
    } else if lower.contains("protected") {
        "branch protected".to_string()
    } else if lower.contains("required review") {
        "required review pending".to_string()
    } else {
        format!(
            "gh {} with --{strategy} failed (check the repo allows that merge method): {}",
            effect.word(),
            &first[..first.len().min(200)]
        )
    };
    Outcome::Failed { reason }
}

// ---------------------------------------------------------------------------
// The real probes.
// ---------------------------------------------------------------------------

/// The production [`Probes`]: one guarded `fno do pr info` fetch, the shipped
/// hold and lineage verbs, and the repo's own config readers.
pub struct RealProbes;

impl RealProbes {
    fn fno(cwd: &Path, args: &[&str]) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String> {
        let out = Command::new("fno")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|error| error.to_string())?;
        Ok((out.status.code(), out.stdout, out.stderr))
    }
}

impl Probes for RealProbes {
    fn pr_facts(&self, cwd: &Path, pr: Option<u64>) -> Result<PrFacts, String> {
        let number = pr.map(|n| n.to_string());
        let mut args = vec!["do", "pr", "info"];
        if let Some(number) = number.as_deref() {
            args.push(number);
        }
        let (_code, stdout, stderr) = Self::fno(cwd, &args)?;
        let payload: Value = serde_json::from_slice(&stdout).map_err(|error| {
            let detail = String::from_utf8_lossy(&stderr);
            format!(
                "fno do pr info returned unreadable JSON ({error}): {}",
                detail.trim()
            )
        })?;
        parse_pr_facts(&payload)
    }

    fn dispatch_hold(&self, cwd: &Path, pr: u64) -> ProbeOutcome {
        match Self::fno(cwd, &["do", "pr", "hold-check", &pr.to_string()]) {
            Ok((code, stdout, stderr)) => classify_hold_probe(code == Some(0), &stdout, &stderr),
            Err(error) => ProbeOutcome::Inconclusive(format!(
                "dispatch hold check unavailable ({error}); refusing to assume unheld"
            )),
        }
    }

    fn review_hold(&self, cwd: &Path, pr: u64) -> ProbeOutcome {
        // `review-hold check` is the one owner of "a review is RUNNING": exit 0
        // clear, 3 held, 4 the probe could not answer.
        match Self::fno(cwd, &["do", "pr", "review-hold", "check", &pr.to_string()]) {
            Ok((code, stdout, stderr)) => {
                let detail = probe_detail(&stdout, &stderr);
                match code {
                    Some(0) => ProbeOutcome::Clear,
                    Some(3) => ProbeOutcome::Refused(if detail.is_empty() {
                        "a review is in flight on this PR".to_string()
                    } else {
                        detail
                    }),
                    other => ProbeOutcome::Inconclusive(format!(
                        "review-activity-unreadable (exit {other:?}): {detail}; \
                         refusing to assume no review is running"
                    )),
                }
            }
            Err(error) => ProbeOutcome::Inconclusive(format!(
                "review-activity-unreadable ({error}); refusing to assume no review is running"
            )),
        }
    }

    fn base_lineage(&self, cwd: &Path, pr: u64) -> ProbeOutcome {
        match Self::fno(cwd, &["do", "pr", "base-lineage-check", &pr.to_string()]) {
            Ok((code, _stdout, stderr)) => match code {
                Some(0) => ProbeOutcome::Clear,
                Some(3) => {
                    ProbeOutcome::Refused(String::from_utf8_lossy(&stderr).trim().to_string())
                }
                other => ProbeOutcome::Inconclusive(format!(
                    "exit {other:?}: {}",
                    String::from_utf8_lossy(&stderr).trim()
                )),
            },
            Err(error) => ProbeOutcome::Inconclusive(format!("spawn error: {error}")),
        }
    }

    fn checks_verdict(&self, cwd: &Path, pr: u64) -> String {
        match Self::fno(cwd, &["do", "pr", "status", &pr.to_string()]) {
            Ok((_code, stdout, _stderr)) => serde_json::from_slice::<Value>(&stdout)
                .ok()
                .and_then(|v| v.get("verdict").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_string()),
            Err(_) => "unknown".to_string(),
        }
    }

    fn covered_head(&self, cwd: &Path) -> Option<String> {
        covered_head_from_event(cwd)
    }

    fn auto_merge_enabled(&self, cwd: &Path) -> bool {
        crate::agents_config::auto_merge_enabled(cwd)
    }

    fn posture_floor_block(&self, cwd: &Path) -> Option<String> {
        crate::agents_config::automerge_posture_floor_block_reason(cwd)
    }

    fn strategy(&self, cwd: &Path) -> String {
        crate::agents_config::auto_merge_strategy(cwd)
    }

    fn run_gh(&self, cwd: &Path, args: &[String]) -> Result<(bool, String), String> {
        let out = Command::new("gh")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|error| error.to_string())?;
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), combined))
    }
}

fn probe_detail(stdout: &[u8], stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr).trim().to_string();
    if err.is_empty() {
        String::from_utf8_lossy(stdout).trim().to_string()
    } else {
        err
    }
}

/// Parse `fno do pr info`. An error field, a missing number, or a missing head
/// sha is a dead instrument, never a PR with no head.
pub fn parse_pr_facts(payload: &Value) -> Result<PrFacts, String> {
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    let number = payload
        .get("pr")
        .and_then(Value::as_u64)
        .ok_or_else(|| "fno do pr info carried no PR number".to_string())?;
    let head_sha = payload
        .get("head_sha")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if head_sha.is_empty() {
        return Err(format!(
            "fno do pr info carried no head sha for PR {number}"
        ));
    }
    Ok(PrFacts {
        number,
        head_sha,
        head_ref: payload
            .get("head_ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        base_ref: payload
            .get("base_ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        state: payload
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("OPEN")
            .to_string(),
        // The armed flag rides this same payload. A second `gh pr view` probe
        // could answer about a head this one never saw.
        armed: payload
            .get("auto_merge")
            .map(|v| !v.is_null())
            .unwrap_or(false),
    })
}

/// A moved verb spelling prints one teaching line to stderr. It is not a refusal
/// reason: kept, it would lead the operator message with noise and permanently
/// mask the empty-output fallback below.
pub fn classify_hold_probe(success: bool, stdout: &[u8], stderr: &[u8]) -> ProbeOutcome {
    if success {
        return ProbeOutcome::Clear;
    }
    let stripped: String = String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !(line.starts_with("fno ") && line.contains(" is now fno ")))
        .collect::<Vec<&str>>()
        .join("\n");
    let detail = if stripped.trim().is_empty() {
        stdout
    } else {
        stripped.as_bytes()
    };
    let message = String::from_utf8_lossy(detail).trim().to_string();
    ProbeOutcome::Refused(if message.is_empty() {
        "dispatch hold state unreadable; refusing to assume unheld".to_string()
    } else {
        message
    })
}

/// The head sha from the latest covered `review_coverage` event that matches the
/// current HEAD, or None.
pub fn covered_head_from_event(cwd: &Path) -> Option<String> {
    let path = crate::paths::events_path(cwd);
    let content = std::fs::read_to_string(&path).ok()?;
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut latest: Option<String> = None;
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(Value::as_str) != Some("review_coverage") {
            continue;
        }
        if val.pointer("/data/coverage").and_then(Value::as_str) != Some("covered") {
            continue;
        }
        if val
            .pointer("/data/reviewed_count")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            <= 0
        {
            continue;
        }
        let ev_head = val
            .pointer("/data/head_sha")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !head.is_empty() && ev_head != head {
            continue;
        }
        latest = Some(ev_head.to_string());
    }
    latest.filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// The `authorized-merge` verb: one JSON payload in, one receipt out.
// ---------------------------------------------------------------------------

pub fn run_authorized_merge(args: &[String]) -> i32 {
    let (code, stdout, stderr) = run_authorized_merge_capture(args);
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    code
}

/// Test-friendly variant: returns (exit_code, stdout, stderr) without printing.
pub fn run_authorized_merge_capture(args: &[String]) -> (i32, String, String) {
    let payload: Value = match read_payload(args) {
        Ok(value) => value,
        Err(message) => return (2, String::new(), message),
    };
    let request = match parse_request(&payload) {
        Ok(request) => request,
        Err(message) => return (2, String::new(), format!("authorized-merge: {message}\n")),
    };
    let outcome = run(&RealProbes, &request);
    // 0 only when the effect happened, or when a decide-only pass cleared.
    // Every refusal, hold and unknown is 2, the established retry-or-escalate
    // code; the receipt carries which it was.
    let code = if outcome.effected() || matches!(outcome, Outcome::Authorized { .. }) {
        0
    } else {
        2
    };
    (code, format!("{}\n", outcome.to_json()), String::new())
}

fn read_payload(args: &[String]) -> Result<Value, String> {
    let text = match args.first() {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("authorized-merge: cannot read payload {path}: {e}\n"))?,
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| format!("authorized-merge: stdin read failed: {e}\n"))?;
            buf
        }
    };
    serde_json::from_str(&text).map_err(|e| format!("authorized-merge: bad payload: {e}\n"))
}

fn parse_request(payload: &Value) -> Result<Request, String> {
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "payload needs a cwd".to_string())?;
    let effect = payload
        .get("effect")
        .and_then(Value::as_str)
        .and_then(Effect::parse)
        .ok_or_else(|| "payload needs effect merge|arm".to_string())?;
    Ok(Request {
        cwd,
        pr: payload.get("pr").and_then(Value::as_u64),
        effect,
        approved: payload.get("approved").and_then(Value::as_bool),
        auto_merge_source: payload
            .get("auto_merge_source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        require_checks: payload
            .get("require_checks")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        covered_head: payload
            .get("covered_head")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        decide_only: payload
            .get("decide_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        facts: Option<PrFacts>,
        facts_error: Option<String>,
        dispatch_hold: Option<ProbeOutcome>,
        review_hold: Option<ProbeOutcome>,
        lineage: Option<ProbeOutcome>,
        checks: Option<String>,
        covered_head: Option<String>,
        enabled: bool,
        floor: Option<String>,
        gh_ok: bool,
        gh_output: String,
        gh_calls: RefCell<Vec<Vec<String>>>,
    }

    fn open_facts() -> PrFacts {
        PrFacts {
            number: 7,
            head_sha: "abc123".to_string(),
            head_ref: "feature/x".to_string(),
            base_ref: "main".to_string(),
            state: "OPEN".to_string(),
            armed: false,
        }
    }

    fn clean() -> Fake {
        Fake {
            facts: Some(open_facts()),
            covered_head: Some("abc123".to_string()),
            enabled: true,
            gh_ok: true,
            ..Default::default()
        }
    }

    impl Probes for Fake {
        fn pr_facts(&self, _cwd: &Path, _pr: Option<u64>) -> Result<PrFacts, String> {
            if let Some(error) = &self.facts_error {
                return Err(error.clone());
            }
            self.facts.clone().ok_or_else(|| "no facts".to_string())
        }
        fn dispatch_hold(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
            self.dispatch_hold.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn review_hold(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
            self.review_hold.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn base_lineage(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
            self.lineage.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn checks_verdict(&self, _cwd: &Path, _pr: u64) -> String {
            self.checks.clone().unwrap_or_else(|| "green".to_string())
        }
        fn covered_head(&self, _cwd: &Path) -> Option<String> {
            self.covered_head.clone()
        }
        fn auto_merge_enabled(&self, _cwd: &Path) -> bool {
            self.enabled
        }
        fn posture_floor_block(&self, _cwd: &Path) -> Option<String> {
            self.floor.clone()
        }
        fn strategy(&self, _cwd: &Path) -> String {
            "squash".to_string()
        }
        fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String), String> {
            self.gh_calls.borrow_mut().push(args.to_vec());
            Ok((self.gh_ok, self.gh_output.clone()))
        }
    }

    fn request(effect: Effect) -> Request {
        Request {
            cwd: PathBuf::from("/tmp"),
            pr: Some(7),
            effect,
            approved: Some(true),
            auto_merge_source: Some("config".to_string()),
            require_checks: false,
            covered_head: None,
            decide_only: false,
        }
    }

    #[test]
    fn a_live_review_hold_holds_both_effects_and_calls_no_gh() {
        // AC1-HP. The arm path never read this hold before, so a queue armed at
        // the terminal shipped the code a review was still fixing.
        for effect in [Effect::Merge, Effect::Arm] {
            let fake = Fake {
                review_hold: Some(ProbeOutcome::Refused(
                    "review_in_flight: held by tgt-x at abc123".to_string(),
                )),
                ..clean()
            };
            let outcome = run(&fake, &request(effect));
            assert_eq!(outcome.word(), "held", "{effect:?}");
            assert!(outcome.detail().contains("review_in_flight"));
            assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
        }
    }

    #[test]
    fn an_unreadable_review_probe_holds_rather_than_assuming_none_runs() {
        let fake = Fake {
            review_hold: Some(ProbeOutcome::Inconclusive(
                "review-activity-unreadable (exit Some(4))".to_string(),
            )),
            ..clean()
        };
        assert_eq!(run(&fake, &request(Effect::Arm)).word(), "held");
        assert!(fake.gh_calls.borrow().is_empty());
    }

    #[test]
    fn a_per_run_refusal_outranks_a_standing_grant_and_names_its_source() {
        // AC1-EDGE.
        let fake = clean();
        let mut req = request(Effect::Merge);
        req.approved = Some(false);
        req.auto_merge_source = Some("flag-no-merge".to_string());
        let outcome = run(&fake, &req);
        assert_eq!(outcome.word(), "refused");
        assert!(
            outcome.detail().contains("flag-no-merge"),
            "{}",
            outcome.detail()
        );
        assert!(fake.gh_calls.borrow().is_empty());
    }

    #[test]
    fn a_disabled_standing_switch_refuses_and_names_the_operator_levers() {
        // AC1-EDGE: the authority evidence is specific, never "not allowed".
        let fake = Fake {
            enabled: false,
            ..clean()
        };
        let outcome = run(&fake, &request(Effect::Arm));
        assert_eq!(outcome.word(), "refused");
        assert!(outcome.detail().contains("auto_merge.enabled"));
        assert!(outcome.detail().contains("TARGET_AUTO_MERGE=1"));
    }

    #[test]
    fn an_env_grant_satisfies_the_arm_without_the_standing_switch() {
        let fake = Fake {
            enabled: false,
            ..clean()
        };
        let mut req = request(Effect::Arm);
        req.auto_merge_source = Some("env-target-auto-merge".to_string());
        assert_eq!(run(&fake, &req).word(), "armed");
    }

    #[test]
    fn an_unknown_authority_read_refuses_rather_than_arming() {
        // A manifest that says nothing falls to the live config, which is off.
        let fake = Fake {
            enabled: false,
            ..clean()
        };
        let mut req = request(Effect::Arm);
        req.approved = None;
        req.auto_merge_source = None;
        assert_eq!(run(&fake, &req).word(), "refused");
    }

    #[test]
    fn the_posture_floor_refuses_both_effects() {
        for effect in [Effect::Merge, Effect::Arm] {
            let fake = Fake {
                floor: Some("review posture no_review is below the automerge floor".to_string()),
                ..clean()
            };
            let outcome = run(&fake, &request(effect));
            assert_eq!(outcome.word(), "refused");
            assert!(fake.gh_calls.borrow().is_empty());
        }
    }

    #[test]
    fn an_unreadable_covered_head_never_produces_an_unpinned_request() {
        // AC2-HP. This is the finalize defect: the old arm dropped
        // --match-head-commit when the journal could not be read.
        for effect in [Effect::Merge, Effect::Arm] {
            let fake = Fake {
                covered_head: None,
                ..clean()
            };
            let outcome = run(&fake, &request(effect));
            assert_eq!(outcome.word(), "unknown", "{effect:?}");
            assert!(outcome.detail().contains("unpinned"));
            assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
        }
    }

    #[test]
    fn a_push_between_validation_and_effect_reads_as_head_changed() {
        // AC2-HP.
        let fake = Fake {
            facts: Some(PrFacts {
                head_sha: "def456".to_string(),
                ..open_facts()
            }),
            ..clean()
        };
        let outcome = run(&fake, &request(Effect::Merge));
        assert_eq!(outcome.word(), "head_changed");
        assert!(fake.gh_calls.borrow().is_empty());
    }

    #[test]
    fn an_arm_receipt_names_the_head_and_is_never_merged() {
        // AC2-EDGE. A queue entry is not a landed merge.
        let fake = clean();
        let outcome = run(&fake, &request(Effect::Arm));
        assert_eq!(
            outcome,
            Outcome::Armed {
                head: "abc123".to_string()
            }
        );
        let calls = fake.gh_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&"--auto".to_string()));
        assert!(calls[0].contains(&"--match-head-commit".to_string()));
        assert!(calls[0].contains(&"abc123".to_string()));
    }

    #[test]
    fn an_immediate_merge_never_passes_auto() {
        let fake = clean();
        let outcome = run(&fake, &request(Effect::Merge));
        assert_eq!(
            outcome,
            Outcome::Merged {
                head: "abc123".to_string()
            }
        );
        let calls = fake.gh_calls.borrow();
        assert!(!calls[0].contains(&"--auto".to_string()));
        assert!(calls[0].contains(&"--match-head-commit".to_string()));
    }

    #[test]
    fn an_already_armed_pr_stands_down_on_merge_and_no_ops_on_arm() {
        let armed = PrFacts {
            armed: true,
            ..open_facts()
        };
        let merge_fake = Fake {
            facts: Some(armed.clone()),
            ..clean()
        };
        assert_eq!(run(&merge_fake, &request(Effect::Merge)).word(), "held");
        assert!(merge_fake.gh_calls.borrow().is_empty());

        let arm_fake = Fake {
            facts: Some(armed),
            ..clean()
        };
        assert_eq!(run(&arm_fake, &request(Effect::Arm)).word(), "armed");
        assert!(arm_fake.gh_calls.borrow().is_empty());
    }

    #[test]
    fn a_merged_pr_holds_before_every_other_guard() {
        let fake = Fake {
            facts: Some(PrFacts {
                state: "MERGED".to_string(),
                ..open_facts()
            }),
            enabled: false,
            ..clean()
        };
        let outcome = run(&fake, &request(Effect::Merge));
        assert_eq!(outcome.word(), "held");
        assert!(outcome.detail().contains("already merged"));
    }

    #[test]
    fn an_unreadable_pr_fetch_is_unknown_never_a_verdict() {
        let fake = Fake {
            facts_error: Some("gh api pulls/7 failed".to_string()),
            ..clean()
        };
        assert_eq!(run(&fake, &request(Effect::Arm)).word(), "unknown");
    }

    #[test]
    fn require_checks_holds_on_pending_and_fails_on_red() {
        let mut req = request(Effect::Merge);
        req.require_checks = true;

        let pending = Fake {
            checks: Some("pending".to_string()),
            ..clean()
        };
        assert_eq!(run(&pending, &req).word(), "held");
        assert!(pending.gh_calls.borrow().is_empty());

        let red = Fake {
            checks: Some("red".to_string()),
            ..clean()
        };
        assert_eq!(run(&red, &req).word(), "failed");
        assert!(red.gh_calls.borrow().is_empty());
    }

    #[test]
    fn a_gh_failure_over_a_merge_that_landed_reads_as_merged() {
        // The local post-merge step can fail after the server-side merge landed.
        let fake = Fake {
            gh_ok: false,
            gh_output: "failed to delete local branch".to_string(),
            facts: Some(PrFacts {
                state: "MERGED".to_string(),
                ..open_facts()
            }),
            ..clean()
        };
        let authorized = Authorized {
            facts: open_facts(),
            head: "abc123".to_string(),
            strategy: "squash".to_string(),
        };
        let outcome = effect(&fake, &request(Effect::Merge), &authorized);
        assert_eq!(
            outcome,
            Outcome::Merged {
                head: "abc123".to_string()
            }
        );
    }

    #[test]
    fn a_decide_only_pass_authorizes_without_touching_gh() {
        let fake = clean();
        let mut req = request(Effect::Merge);
        req.decide_only = true;
        assert_eq!(
            run(&fake, &req),
            Outcome::Authorized {
                head: "abc123".to_string()
            }
        );
        assert!(fake.gh_calls.borrow().is_empty());
    }

    #[test]
    fn a_callers_covered_head_outranks_the_journal_read() {
        // The caller's coverage gate and the pin must describe one commit.
        let fake = Fake {
            covered_head: Some("stale999".to_string()),
            ..clean()
        };
        let mut req = request(Effect::Merge);
        req.covered_head = Some("abc123".to_string());
        assert_eq!(run(&fake, &req).word(), "merged");
        assert!(fake.gh_calls.borrow()[0].contains(&"abc123".to_string()));
    }

    #[test]
    fn a_worktree_held_branch_recovers_through_the_rest_endpoint_with_the_pin() {
        let fake = Fake {
            gh_ok: false,
            gh_output: "fatal: 'feature/x' is already used by worktree at /w".to_string(),
            ..clean()
        };
        let authorized = Authorized {
            facts: open_facts(),
            head: "abc123".to_string(),
            strategy: "squash".to_string(),
        };
        // run_gh answers the same failure for both calls, so the recovery here
        // is the argv, not the outcome: the retry must carry sha=<pinned head>.
        let _ = effect(&fake, &request(Effect::Merge), &authorized);
        let calls = fake.gh_calls.borrow();
        assert_eq!(
            calls.len(),
            2,
            "the checkout refusal must retry through REST"
        );
        assert!(
            calls[1].contains(&"sha=abc123".to_string()),
            "{:?}",
            calls[1]
        );
        assert!(calls[1].contains(&"merge_method=squash".to_string()));
    }

    #[test]
    fn an_arm_never_takes_the_worktree_recovery() {
        // A queue arm is not a merge, so a checkout refusal has nothing to
        // recover: PUT .../merge would merge NOW, past the queue's own wait.
        let fake = Fake {
            gh_ok: false,
            gh_output: "fatal: 'feature/x' is already used by worktree at /w".to_string(),
            ..clean()
        };
        let authorized = Authorized {
            facts: open_facts(),
            head: "abc123".to_string(),
            strategy: "squash".to_string(),
        };
        let _ = effect(&fake, &request(Effect::Arm), &authorized);
        assert_eq!(fake.gh_calls.borrow().len(), 1);
    }

    #[test]
    fn a_dead_state_read_after_a_failed_effect_is_unknown() {
        let fake = Fake {
            gh_ok: false,
            gh_output: "boom".to_string(),
            facts_error: Some("gh api pulls/7 failed".to_string()),
            ..clean()
        };
        let authorized = Authorized {
            facts: open_facts(),
            head: "abc123".to_string(),
            strategy: "squash".to_string(),
        };
        assert_eq!(
            effect(&fake, &request(Effect::Merge), &authorized).word(),
            "unknown"
        );
    }

    #[test]
    fn hold_probe_fails_closed_on_every_non_success() {
        assert_eq!(
            classify_hold_probe(true, b"unheld", b""),
            ProbeOutcome::Clear
        );
        assert_eq!(
            classify_hold_probe(false, b"", b"dispatch-hold:x-owner"),
            ProbeOutcome::Refused("dispatch-hold:x-owner".to_string())
        );
        assert_eq!(
            classify_hold_probe(false, b"", b""),
            ProbeOutcome::Refused(
                "dispatch hold state unreadable; refusing to assume unheld".to_string()
            )
        );
    }

    #[test]
    fn hold_probe_strips_the_move_teaching_line() {
        assert_eq!(
            classify_hold_probe(
                false,
                b"",
                b"fno pr hold-check is now fno do pr hold-check\ndispatch-hold:x-owner\n"
            ),
            ProbeOutcome::Refused("dispatch-hold:x-owner".to_string())
        );
        assert_eq!(
            classify_hold_probe(
                false,
                b"",
                b"fno pr hold-check is now fno do pr hold-check\n"
            ),
            ProbeOutcome::Refused(
                "dispatch hold state unreadable; refusing to assume unheld".to_string()
            )
        );
    }

    #[test]
    fn probe_outcome_projections_state_their_inconclusive_policy() {
        let refused = ProbeOutcome::Refused("stale base".to_string());
        assert_eq!(refused.clone().fail_closed().as_deref(), Some("stale base"));
        assert_eq!(refused.fail_open().as_deref(), Some("stale base"));
        assert_eq!(ProbeOutcome::Clear.fail_closed(), None);
        assert_eq!(ProbeOutcome::Clear.fail_open(), None);
        let inconclusive = ProbeOutcome::Inconclusive("exit 4: unknown".to_string());
        assert_eq!(
            inconclusive.clone().fail_closed().as_deref(),
            Some("exit 4: unknown"),
            "the hold probes refuse on an unevaluated read"
        );
        assert_eq!(
            inconclusive.fail_open(),
            None,
            "the lineage probe proceeds on an unevaluated read"
        );
    }

    #[test]
    fn pr_facts_reads_the_armed_flag_from_the_same_payload_as_the_head() {
        let payload = json!({
            "pr": 7,
            "head_sha": "abc123",
            "head_ref": "feature/x",
            "base_ref": "main",
            "state": "OPEN",
            "auto_merge": {"enabled_by": {"login": "someone"}}
        });
        let facts = parse_pr_facts(&payload).expect("facts parse");
        assert!(facts.armed);
        assert_eq!(facts.head_sha, "abc123");

        let unarmed = json!({"pr": 7, "head_sha": "abc123", "auto_merge": Value::Null});
        assert!(!parse_pr_facts(&unarmed).expect("facts parse").armed);
    }

    #[test]
    fn pr_facts_refuses_a_payload_with_no_head_sha() {
        assert!(parse_pr_facts(&json!({"pr": 7})).is_err());
        assert!(parse_pr_facts(&json!({"error": "gh api failed"})).is_err());
    }

    #[test]
    fn the_verb_refuses_a_payload_that_names_no_effect() {
        assert!(parse_request(&json!({"cwd": "/tmp"})).is_err());
        assert!(parse_request(&json!({"effect": "arm"})).is_err());
        assert!(parse_request(&json!({"cwd": "/tmp", "effect": "sideways"})).is_err());
        let ok = parse_request(&json!({"cwd": "/tmp", "effect": "arm", "pr": 7}))
            .expect("a well-formed payload parses");
        assert_eq!(ok.effect, Effect::Arm);
        assert_eq!(ok.pr, Some(7));
    }
}
