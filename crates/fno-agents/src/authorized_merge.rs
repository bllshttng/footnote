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

use crate::backlog::api::{self as backlog_api, Store as GraphStore};
use crate::backlog_ready::detect_project;
use crate::claims::{self, ClaimState};
use crate::king_board::prs::pr_binding_keys;
use crate::paths::canonical_repo_root;

/// A rebase, this repo's measured rust-ci max (31.3m), and one sweep tick
/// (600s) round up with margin to 60 minutes.
const MERGE_SLOT_TTL_MS: i64 = 60 * 60 * 1000;
/// The receipt text derives its minute count from here, so a retuned TTL
/// never leaves the wording stale.
const MERGE_SLOT_TTL_MINUTES: i64 = MERGE_SLOT_TTL_MS / 60_000;

/// What the caller wants to happen once the decision clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Merge,
    Arm,
    /// Answer "may this PR head merge now?" and run nothing, write nothing:
    /// never takes, renews or releases the merge slot. `fno do pr status`
    /// reads this receipt as `ready`.
    Preview,
}

impl Effect {
    fn word(self) -> &'static str {
        match self {
            Effect::Merge => "merge",
            Effect::Preview => "preview",
            Effect::Arm => "arm",
        }
    }

    fn parse(raw: &str) -> Option<Effect> {
        match raw {
            "merge" => Some(Effect::Merge),
            "preview" => Some(Effect::Preview),
            "arm" => Some(Effect::Arm),
            _ => None,
        }
    }
}

/// One gate's answer, named so a reader can key on the word a status read
/// has always shown (`review_in_flight`, `merge_slot_held`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub code: String,
    pub class: BlockerClass,
    pub detail: String,
}

/// How a blocked preview reads. The classes mirror the receipt words a
/// caller already knows, so the verdict doc keeps one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockerClass {
    /// Retryable: the same command later can succeed.
    Held,
    /// Needs an operator action. Retrying changes nothing.
    Refused,
    /// An instrument could not answer. Never a verdict.
    Unknown,
}

impl Blocker {
    pub fn held(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            class: BlockerClass::Held,
            detail: detail.into(),
        }
    }

    pub fn refused(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            class: BlockerClass::Refused,
            detail: detail.into(),
        }
    }

    pub fn unknown(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            class: BlockerClass::Unknown,
            detail: detail.into(),
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
        /// How the merge landed, when it was not the plain path. The REST
        /// recovery for a worktree-held branch sets it. The caller renders it
        /// as the success reason, never as trouble.
        note: Option<String>,
        /// Set only when the merge landed and something around it did NOT: a
        /// local post-merge step that failed after the server-side merge. The
        /// caller renders it as a partial outcome. Keeping it apart from
        /// `note` is load-bearing: a recovery that worked is not a failure,
        /// and reporting it as one made every worktree-first merge read
        /// partial.
        cleanup_failure: Option<String>,
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
            Outcome::Merged { head, .. } => head.clone(),
            Outcome::Armed { head } | Outcome::Authorized { head } => head.clone(),
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
            Outcome::Merged {
                head,
                note,
                cleanup_failure,
            } => {
                out["head"] = json!(head);
                if let Some(note) = note {
                    out["note"] = json!(note);
                }
                if let Some(cleanup_failure) = cleanup_failure {
                    out["cleanup_failure"] = json!(cleanup_failure);
                }
            }
            Outcome::Armed { head } | Outcome::Authorized { head } => out["head"] = json!(head),
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
    /// Which caller asks: `"durable_grant"` (the watcher's merge phase) or
    /// absent/`"manifest"` (the interactive verb). On the durable-grant lane a
    /// red verdict is the state a working session is in while it pushes fixes,
    /// so it holds; spending a retry on it parked six open PRs in one
    /// afternoon. The interactive lane keeps `Failed`, where the
    /// `merge_status=failed` stamp is the worker's signal to stop.
    pub authority: Option<String>,
    /// Merge-side flake acceptance, passed by the merge verb (`--accept-flake`).
    pub accept_flake: bool,
    /// Preview-supplied facts, so a preview ask never spawns `fno do pr
    /// status` for what its caller already computed. When a field is absent
    /// the walk reads its own probes instead.
    pub supplied_verdict: Option<String>,
    /// The rollup counts behind `supplied_verdict`, so the walk can name the
    /// KIND of red (`ci_cancelled_retrigger`, `commit_status_red`) the way
    /// the status read always has.
    pub supplied_counts: Option<Value>,
    pub supplied_rerun_recovered: Option<bool>,
    /// `Some(Some(n))` = n unresolved; `Some(None)` = the read answered
    /// unknown; None = not supplied (probe instead).
    pub supplied_optional_unresolved: Option<Option<i64>>,
    /// GitHub's own merge-hold words, as status computed them (`github_blocked`,
    /// `github_behind`, ...). Absent: the walk derives them from `checks_read`.
    pub supplied_github_blockers: Option<Vec<String>>,
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
    /// The PR page URL, as the payload carried it.
    pub url: String,
    /// The PR body, the third binding key's source. `None` only when the
    /// payload carried no body key at all, which is how an out-of-date
    /// deployed `fno` looks; a null body reads as an empty string.
    pub body: Option<String>,
    /// `OPEN`, `MERGED`, or `CLOSED`.
    pub state: String,
    /// GitHub's auto-merge queue already owns this PR. Rides the same pulls
    /// payload as the head, so no second probe can disagree with it about which
    /// head it describes.
    pub armed: bool,
}

/// One `fno do pr status` read, both facts. `verdict` is the CI word the
/// checks arm has always matched on. `github_block` is GitHub's own hold on
/// the merge, named with the context it is missing, from the same payload:
/// the door used to parse this JSON and keep one key of twenty-six.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksRead {
    pub verdict: String,
    pub github_block: Option<String>,
    /// From the same status payload: unresolved OPTIONAL review findings.
    /// `Some(None)` = the payload answered unknown; None = no answer at all.
    pub optional_unresolved: Option<Option<i64>>,
    /// From the same status payload: CI was red, then green on rerun without
    /// a fresh review. Some(false) or None never holds.
    pub rerun_recovered: Option<bool>,
    /// The check names the rerun recovered from, for the hold's own reason.
    pub rerun_failures: Option<Vec<String>>,
}

/// The outside world, injectable so the decision is testable without a network.
pub trait Probes {
    fn pr_facts(&self, cwd: &Path, pr: Option<u64>) -> Result<PrFacts, String>;
    /// Does the graph see this PR? `Refused` when no binding key names a
    /// node, `Inconclusive` when the graph or the body cannot be read.
    fn node_binding(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome;
    fn dispatch_hold(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    fn review_hold(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    fn base_lineage(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    /// Compile the merge result (merge-tree + the repo-wide static step).
    fn merge_result(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    fn ci_base(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome;
    fn require_fresh_ci(&self, cwd: &Path) -> bool;
    /// The PR number holding `merge-slot:<base_ref>` when the claim reads
    /// `Live` or `Suspect`. `Free`/`Stale` read `Ok(None)`. `Corrupted` or an
    /// unparseable holder is `Err`.
    fn slot_holder(&self, cwd: &Path, base_ref: &str) -> Result<Option<u64>, String>;
    /// Take the merge slot for `pr` on a bounded TTL lease.
    fn take_slot(&self, cwd: &Path, base_ref: &str, pr: u64) -> Result<(), String>;
    /// Release the merge slot if `pr` still holds it. Errors are ignored:
    /// release is best-effort, and the TTL is the backstop.
    fn release_slot(&self, cwd: &Path, base_ref: &str, pr: u64);
    /// The four verdict words `green` | `red` | `pending` | `unknown`, plus
    /// GitHub's own ruleset hold when the same payload names one.
    fn checks_read(&self, cwd: &Path, pr: u64) -> ChecksRead;
    fn covered_head(&self, cwd: &Path) -> Option<String>;
    fn auto_merge_enabled(&self, cwd: &Path) -> bool;
    fn posture_floor_block(&self, cwd: &Path) -> Option<String>;
    fn strategy(&self, cwd: &Path) -> String;
    /// Run `gh` with these arguments. `Ok((success, combined_output))`.
    fn run_gh(&self, cwd: &Path, args: &[String]) -> Result<(bool, String), String>;
    /// Shell the `fno` CLI for a gate probe: `Ok((exit_code, stdout, stderr))`,
    /// None code = signal death. The coverage and fidelity gates ride this, so
    /// Fake-based tests answer the gates without a network.
    fn fno_shell(
        &self,
        cwd: &Path,
        args: &[String],
    ) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String>;
    /// Live parallel-lane claims. The default 0 keeps the overlap gate
    /// disarmed wherever an impl does not count lanes (the sequential default).
    fn live_lanes(&self, _cwd: &Path) -> usize {
        0
    }
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

    // The preview arm answers the same gates read-only and never reaches an
    // effect: `fno do pr status` reads its receipt as `ready`.
    if request.effect == Effect::Preview {
        return match preview_walk(probes, request, &facts) {
            PreviewVerdict::Go { .. } => {
                let head = facts.head_sha.clone();
                Ok(Authorized {
                    facts,
                    head,
                    strategy: probes.strategy(cwd),
                })
            }
            PreviewVerdict::Blocked(blockers) => Err(Outcome::Held {
                reason: blockers
                    .iter()
                    .map(|b| b.detail.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            }),
        };
    }

    // A merged or closed PR has no would-merge left. Every guard below protects
    // what WOULD merge, so answering "unreviewed" here sends a caller hunting a
    // defect that is blocking nothing.
    if is_terminal_state(&facts.state) {
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

    // The graph must see the PR. An unbound PR is Refused (retrying without
    // binding changes nothing, so Held would be a lie about the remedy); an
    // unreadable graph is Unknown, never a bound-or-unbound verdict.
    match probes.node_binding(cwd, &facts) {
        ProbeOutcome::Clear => {}
        ProbeOutcome::Refused(reason) => return Err(Outcome::Refused { reason }),
        ProbeOutcome::Inconclusive(reason) => return Err(Outcome::Unknown { reason }),
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

    // Two green parents can merge red: git joins hunks that never met on one
    // machine (3334b826a133 broke main with F821 out of a clean textual
    // merge). Held, not refused: the remedy is rebase, fix, push, retry.
    if let Some(reason) = probes.merge_result(cwd, facts.number).fail_open() {
        return Err(Outcome::Held {
            reason: format!("red merge result: {reason}"),
        });
    }

    let checks = probes.checks_read(cwd, facts.number);
    // The gates the Python merge verb owned before this port. They run before
    // the CI/slot gates below so a PR the coverage or stub gate holds never
    // takes the merge slot and squats it for its TTL.
    if request.effect == Effect::Merge {
        let (coverage_blocker, _waiver) =
            crate::merge_gates::coverage_gate(probes, cwd, facts.number);
        if let Some(blocker) = coverage_blocker {
            return Err(Outcome::Held {
                reason: blocker.detail,
            });
        }
        if let Some(blocker) = walk_entries(cwd).and_then(|entries| {
            crate::merge_gates::stub_manifest_gate(&repo_root(cwd), &entries, facts.number)
        }) {
            return Err(Outcome::Held {
                reason: blocker.detail,
            });
        }
        if let Some(blocker) = crate::merge_gates::plan_fidelity_blocker(probes, cwd, facts.number)
        {
            return Err(Outcome::Refused {
                reason: blocker.detail,
            });
        }
        if let Some(blocker) = crate::merge_gates::overlap_blocker(probes, cwd, facts.number) {
            return Err(Outcome::Held {
                reason: blocker.detail,
            });
        }
        if request.require_checks {
            if let Some(blocker) = flake_blocker(request, &checks) {
                return Err(Outcome::Held {
                    reason: blocker.detail,
                });
            }
        }
        if let Some(blocker) = optional_reviews_blocker(checks.optional_unresolved) {
            return Err(Outcome::Held {
                reason: blocker.detail,
            });
        }
    }

    // The GitHub hold rides the same status read the checks arm already paid
    // for, and outranks the require_checks flag: a ruleset hold is not a
    // question about CI greenness, and gating it on that flag repeats the
    // category error this arm exists to close. Held, not Failed - a required
    // check that is merely pending still arrives, and the reason names the
    // missing context so a human can narrow the ruleset when it never can.
    // Merge only: Arm hands the PR to GitHub's own queue, which is designed
    // to wait out a missing requirement, so arming on a ruleset hold is the
    // right move and this hold must not stand in its way.
    if request.effect == Effect::Merge {
        if let Some(missing) = &checks.github_block {
            return Err(Outcome::Held {
                reason: format!(
                    "GitHub holds this merge: required checks missing at the head ({missing})"
                ),
            });
        }
    }
    if request.require_checks {
        // Only a POSITIVE red fails. Every other non-green answer holds, so a
        // read that could not run - `fno do pr status` says `error` when the
        // fetch is rate-limited or the network is down - retries instead of
        // stamping the node's merge status failed.
        match checks.verdict.as_str() {
            "green" => {
                if request.effect == Effect::Merge && probes.require_fresh_ci(cwd) {
                    let stale = probes.ci_base(cwd, &facts).fail_open();
                    let n = facts.number;
                    match probes.slot_holder(cwd, &facts.base_ref) {
                        // A claims io fault never blocks merges: fall back to
                        // the pre-slot behavior and take no slot.
                        Err(_) => {
                            if let Some(reason) = stale {
                                return Err(Outcome::Held {
                                    reason: format!("{reason}; {}", stale_remedy(n)),
                                });
                            }
                        }
                        Ok(mut holder) => {
                            // A holder that merged, closed, went red, or took
                            // a dispatch hold frees its slot now instead of
                            // waiting out the TTL. The hold read is fail_open:
                            // evicting on an unreadable read would collapse the
                            // ordering the slot exists to keep.
                            if let Some(m) = holder {
                                if m != n {
                                    let holder_held =
                                        probes.dispatch_hold(cwd, m).fail_open().is_some();
                                    let stale_holder = holder_held
                                        || match probes.pr_facts(cwd, Some(m)) {
                                            Ok(holder_facts) => {
                                                is_terminal_state(&holder_facts.state)
                                                    || probes.checks_read(cwd, m).verdict == "red"
                                            }
                                            // Unreadable holder PR keeps the
                                            // slot; the TTL bounds it.
                                            Err(_) => false,
                                        };
                                    if stale_holder {
                                        probes.release_slot(cwd, &facts.base_ref, m);
                                        holder = None;
                                    }
                                }
                            }
                            match holder {
                                Some(m) if m != n => {
                                    return Err(Outcome::Held {
                                        reason: format!(
                                            "{ci}merge_slot_held: PR {m} holds the merge slot; \
                                             PR {n} waits so PR {m}'s rebased CI stays current; \
                                             the slot frees when PR {m} merges, closes, goes red, \
                                             takes a dispatch hold, or its {ttl}m lease ends",
                                            ci = if stale.is_some() {
                                                "ci_base_stale; "
                                            } else {
                                                ""
                                            },
                                            ttl = MERGE_SLOT_TTL_MINUTES
                                        ),
                                    });
                                }
                                Some(_self_held) => {
                                    if let Some(reason) = stale {
                                        // Never re-acquire: it could extend
                                        // the TTL and starve the queue.
                                        return Err(Outcome::Held {
                                            reason: format!(
                                                "{reason}; PR {n} holds the merge slot; {}",
                                                stale_remedy(n)
                                            ),
                                        });
                                    }
                                }
                                None => {
                                    if let Some(reason) = stale {
                                        match probes.take_slot(cwd, &facts.base_ref, n) {
                                            Ok(()) => {
                                                return Err(Outcome::Held {
                                                    reason: format!(
                                                        "{reason}; PR {n} now holds the merge \
                                                         slot for {ttl}m; {remedy}",
                                                        ttl = MERGE_SLOT_TTL_MINUTES,
                                                        remedy = stale_remedy(n)
                                                    ),
                                                });
                                            }
                                            Err(_) => {
                                                return Err(Outcome::Held {
                                                    reason: format!(
                                                        "{reason}; {}",
                                                        stale_remedy(n)
                                                    ),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "red" => {
                return Err(match request.authority.as_deref() {
                    Some("durable_grant") => Outcome::Held {
                        reason: "checks are red; the healer or the worker owns the next push"
                            .to_string(),
                    },
                    _ => Outcome::Failed {
                        reason: "checks are red; require_checks_pass forbids merging without green"
                            .to_string(),
                    },
                })
            }
            verdict => {
                return Err(Outcome::Held {
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
/// The preview's verdict shape: authorized, or every blocker the gates could
/// evaluate. The one receipt `fno do pr status` reads as `ready`.
pub enum PreviewVerdict {
    Go { waiver: Option<String> },
    Blocked(Vec<Blocker>),
}

/// The read-only preview walk: same gates, same order as the effect path's
/// first-refusal chain, all collected. Preview never writes: take_slot and
/// release_slot have no preview call site, so a stale PR with a free slot
/// reads `ci_base_stale` with the slot untouched (AC1).
pub fn preview_walk<P: Probes>(probes: &P, request: &Request, facts: &PrFacts) -> PreviewVerdict {
    let cwd = request.cwd.as_path();
    let mut blockers: Vec<Blocker> = Vec::new();
    let mut entries: Option<Vec<Value>> = None;
    let mut checks: Option<ChecksRead> = None;

    // (1) terminal. AC3: the blockers are exactly [pr_terminal].
    if is_terminal_state(&facts.state) {
        return PreviewVerdict::Blocked(vec![Blocker::held(
            "pr_terminal",
            format!(
                "PR {} is already {}; nothing to merge",
                facts.number,
                facts.state.to_lowercase()
            ),
        )]);
    }

    // (2) authority: per-run refusal, live config, posture floor. The codes
    // re-derive authority_refusal's fold order, read-only.
    if let Some(reason) = authority_refusal(probes, cwd, request) {
        let code = if request.approved == Some(false) {
            "per_run_no_merge"
        } else {
            let env_grant = request.approved == Some(true)
                && request.auto_merge_source.as_deref() == Some("env-target-auto-merge");
            if !env_grant && !probes.auto_merge_enabled(cwd) {
                "auto_merge_disabled"
            } else {
                "posture_floor"
            }
        };
        blockers.push(match code {
            "per_run_no_merge" => Blocker::refused("per_run_no_merge", reason),
            "auto_merge_disabled" => Blocker::refused("auto_merge_disabled", reason),
            _ => Blocker::refused("posture_floor", reason),
        });
    }

    // (3) node binding: unbound is refused, unreadable is unknown.
    match probes.node_binding(cwd, facts) {
        ProbeOutcome::Clear => {}
        ProbeOutcome::Refused(reason) => {
            blockers.push(Blocker::refused("node_unbound", reason));
        }
        ProbeOutcome::Inconclusive(reason) => {
            blockers.push(Blocker::unknown("node_binding_unknown", reason));
        }
    }

    // (4) holds, in decide's order.
    if let Some(reason) = probes.dispatch_hold(cwd, facts.number).fail_closed() {
        blockers.push(Blocker::held("dispatch_hold", reason));
    }
    if let Some(reason) = probes.review_hold(cwd, facts.number).fail_closed() {
        blockers.push(Blocker::held("review_in_flight", reason));
    }

    // (5) the pin.
    if let Some(head) = request
        .covered_head
        .clone()
        .or_else(|| probes.covered_head(cwd))
        .filter(|sha| !sha.is_empty())
    {
        if head != facts.head_sha {
            blockers.push(Blocker::unknown(
                "head_moved",
                format!(
                    "the PR head moved from {head} to {} between validation and the effect",
                    facts.head_sha
                ),
            ));
        }
    } else {
        blockers.push(Blocker::unknown(
            "head_not_covered",
            format!(
                "no covered head is readable for PR {}; refusing an unpinned merge",
                facts.number
            ),
        ));
    }

    // (6) lineage + merge result, in decide's order.
    if let Some(reason) = probes.base_lineage(cwd, facts.number).fail_open() {
        blockers.push(Blocker::refused(
            "stacked_base",
            format!("stale base: {reason}"),
        ));
    }
    if let Some(reason) = probes.merge_result(cwd, facts.number).fail_open() {
        blockers.push(Blocker::held(
            "red_merge_result",
            format!("red merge result: {reason}"),
        ));
    }

    // (7) the status payload: supplied facts win, so a preview ask never
    // spawns `fno do pr status` for a fact its caller computed.
    let checks = checks.get_or_insert_with(|| match &request.supplied_verdict {
        Some(word) => ChecksRead {
            verdict: word.clone(),
            github_block: None,
            optional_unresolved: request.supplied_optional_unresolved,
            rerun_recovered: request.supplied_rerun_recovered,
            rerun_failures: None,
        },
        None => probes.checks_read(cwd, facts.number),
    });
    let optional = request
        .supplied_optional_unresolved
        .or(checks.optional_unresolved);

    // (7b) the ported gates, always in preview (the effect path runs them
    // only for Merge). Fidelity reads the ledger, not the graph rows, so it
    // runs even when the store is unreadable: the gate list must not shrink
    // with an ingredient the gate does not use.
    if entries.is_none() {
        entries = walk_entries(cwd);
    }
    let repo_root_path = repo_root(cwd);
    let (coverage_blocker, waiver) = crate::merge_gates::coverage_gate(probes, cwd, facts.number);
    if let Some(blocker) = coverage_blocker {
        blockers.push(blocker);
    }
    if let Some(entry_slice) = entries.as_deref() {
        if let Some(blocker) =
            crate::merge_gates::stub_manifest_gate(&repo_root_path, entry_slice, facts.number)
        {
            blockers.push(blocker);
        }
    }
    if let Some(blocker) = crate::merge_gates::plan_fidelity_blocker(probes, cwd, facts.number) {
        blockers.push(blocker);
    }
    if let Some(blocker) = crate::merge_gates::overlap_blocker(probes, cwd, facts.number) {
        blockers.push(blocker);
    }
    if let Some(blocker) = flake_blocker(request, &checks) {
        blockers.push(blocker);
    }
    if let Some(blocker) = optional_reviews_blocker(optional) {
        blockers.push(blocker);
    }

    // (8) GitHub's own hold: supplied words, else the parsed github_block.
    let github_words: Vec<String> = request.supplied_github_blockers.clone().unwrap_or_else(|| {
        checks
            .github_block
            .clone()
            .map(|_m| vec!["github_blocked".to_string()])
            .unwrap_or_default()
    });
    for word in &github_words {
        let (code, detail): (&str, String) = match word.as_str() {
            "github_blocked" => (
                "github_blocked",
                match &checks.github_block {
                    Some(m) => format!(
                        "GitHub holds this merge: required checks missing at the head ({m})"
                    ),
                    None => "GitHub holds this merge".to_string(),
                },
            ),
            other => (other, other.to_string()),
        };
        blockers.push(Blocker::held(code, detail));
    }

    // (9) the CI verdict gate, same precondition as the effect path.
    if request.require_checks && checks.verdict != "green" {
        let word = ci_blocker_word(&checks.verdict, request.supplied_counts.as_ref());
        let detail = format!(
            "checks are {}; require_checks_pass forbids merging without green",
            checks.verdict
        );
        if checks.verdict == "red" && request.authority.as_deref() == Some("durable_grant") {
            // The durable-grant lane holds on red (a working session pushes
            // fixes); the interactive lane refuses. Class follows the lane.
            blockers.push(Blocker::held(
                word.as_str(),
                "checks are red; the healer or the worker owns the next push",
            ));
        } else {
            blockers.push(Blocker::refused(word.as_str(), detail));
        }
    }

    // (10) the slot gate, read-only: same precondition (fresh-CI posture,
    // green) and same eviction readability as the effect path, but take_slot
    // and release_slot have no preview call site.
    if request.require_checks && checks.verdict == "green" && probes.require_fresh_ci(cwd) {
        let stale = probes.ci_base(cwd, facts).fail_open();
        let n = facts.number;
        if let Some(reason) = &stale {
            blockers.push(Blocker::held(
                "ci_base_stale",
                format!("{reason}; {}", stale_remedy(n)),
            ));
        }
        match probes.slot_holder(cwd, &facts.base_ref) {
            Err(_) => {}
            Ok(Some(m)) if m != n => {
                let holder_held = probes.dispatch_hold(cwd, m).fail_open().is_some();
                let evictable = holder_held
                    || match probes.pr_facts(cwd, Some(m)) {
                        Ok(hf) => {
                            is_terminal_state(&hf.state)
                                || probes.checks_read(cwd, m).verdict == "red"
                        }
                        Err(_) => false,
                    };
                if !evictable {
                    blockers.push(Blocker::held(
                        "merge_slot_held",
                        format!(
                            "merge_slot_held: PR {m} holds the merge slot; PR {n} waits so \
                             PR {m}'s rebased CI stays current; the slot frees when PR {m} \
                             merges, closes, goes red, takes a dispatch hold, or its {ttl}m lease ends",
                            ttl = MERGE_SLOT_TTL_MINUTES
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
    if blockers.is_empty() {
        PreviewVerdict::Go { waiver }
    } else {
        PreviewVerdict::Blocked(blockers)
    }
}

/// The status-side name for a non-green verdict: the KIND of red, never a
/// generic one. A red whose every failure is a taken-away run is a
/// cancelled-retrigger; one whose every failure is a StatusContext is a
/// status red; anything else is the generic word. Unreadable counts
/// degrade to the generic name - the verdict itself stays authoritative.
fn ci_blocker_word(verdict: &str, counts: Option<&Value>) -> String {
    if verdict != "red" {
        return format!("ci_{verdict}");
    }
    if let Some(c) = counts {
        let uf = c.get("unsettled_fail").and_then(Value::as_i64).unwrap_or(0);
        let f = c.get("fail").and_then(Value::as_i64).unwrap_or(0);
        let fs = c.get("fail_statuses").and_then(Value::as_i64).unwrap_or(0);
        if uf > 0 && uf == f {
            return "ci_cancelled_retrigger".to_string();
        }
        if fs > 0 && fs == f {
            return "commit_status_red".to_string();
        }
    }
    "ci_red".to_string()
}

/// The flake gate, shared by the effect and preview walks: a rerun-recovered
/// green is not a clean green. `accept_flake` is the sanctioned override.
fn flake_blocker(request: &Request, checks: &ChecksRead) -> Option<Blocker> {
    if request.accept_flake {
        return None;
    }
    if checks.rerun_recovered != Some(true) {
        return None;
    }
    let failed = checks
        .rerun_failures
        .as_ref()
        .map(|names| names.join(", "))
        .unwrap_or_else(|| "unknown checks".to_string());
    Some(Blocker::held(
        "rerun_recovered_green",
        format!(
            "rerun-recovered green (earlier failed attempt: {failed}); merge held. \
             Sanctioned override: fno do pr merge <pr> --accept-flake"
        ),
    ))
}

/// The optional-reviews gate, shared by the effect and preview walks: an
/// unknown read never passes for "none unresolved".
fn optional_reviews_blocker(value: Option<Option<i64>>) -> Option<Blocker> {
    match value {
        None | Some(None) => Some(Blocker::unknown(
            "optional_reviews_unknown",
            "optional review findings unreadable; refusing to assume none unresolved",
        )),
        Some(Some(0)) => None,
        Some(Some(_)) => Some(Blocker::held(
            "optional_reviews_unresolved",
            "optional review findings unresolved",
        )),
    }
}

/// Graph rows for the stub-manifest and plan-fidelity gates. An unreadable
/// store degrades to None (the default hard merge path), as the Python did.
fn walk_entries(cwd: &Path) -> Option<Vec<Value>> {
    let graph_path = crate::king_board::scope::graph_json_path(cwd);
    let store = GraphStore::new(&graph_path);
    backlog_api::rows(&store).ok()
}

/// The repo top-level for manifest lookups: manifests live at the project
/// root's `.fno/`, never under a subdirectory cwd.
fn repo_root(cwd: &Path) -> PathBuf {
    canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

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
        Ok(authorized) if request.decide_only || request.effect == Effect::Preview => {
            Outcome::Authorized {
                head: authorized.head,
            }
        }
        Ok(authorized) => {
            let outcome = effect(probes, request, &authorized);
            // decide() takes the slot only under Effect::Merge, so only a
            // Merge hands it back. Every terminal effect() outcome of a Merge
            // - landed, durably Failed, HeadChanged, or Unknown - releases it
            // here rather than starving the queue for the rest of the lease.
            // An arm leaves GitHub to merge later; a slot dropped here lets a
            // racer restale the queue the armed PR is still waiting in, and a
            // holder that arms keeps the slot until its merge lands, which the
            // eviction above then reads as terminal. A PR that never held the
            // slot releases nothing (holder-matched).
            if request.effect == Effect::Merge {
                probes.release_slot(
                    request.cwd.as_path(),
                    &authorized.facts.base_ref,
                    authorized.facts.number,
                );
            }
            outcome
        }
        Err(outcome) => outcome,
    }
}

fn effect<P: Probes>(probes: &P, request: &Request, authorized: &Authorized) -> Outcome {
    let cwd = request.cwd.as_path();
    let number = authorized.facts.number;
    if authorized.facts.armed {
        return match request.effect {
            // Preview never reaches an effect (run() short-circuits it).
            Effect::Preview => {
                return Outcome::Unknown {
                    reason: "preview never runs an effect".to_string(),
                }
            }
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
                note: None,
                cleanup_failure: None,
            },
            Effect::Preview => Outcome::Unknown {
                reason: "preview never runs an effect".to_string(),
            },
        };
    }

    // gh exits non-zero on an already-merged PR, and on a post-merge step that
    // failed after the server-side merge landed. Re-read rather than match the
    // error phrasing: the durable signal is the PR's own state.
    match probes.pr_facts(cwd, Some(number)) {
        Ok(after) if after.state == "MERGED" => Outcome::Merged {
            head: authorized.head.clone(),
            note: Some("merged server-side".to_string()),
            cleanup_failure: Some(format!(
                "gh {} exited non-zero after the server-side merge: {}",
                request.effect.word(),
                first_line(&output)
            )),
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
                    // The recovery WORKED, so it carries no cleanup failure.
                    return Outcome::Merged {
                        head: authorized.head.clone(),
                        note: Some("merged server-side (worktree fallback)".to_string()),
                        cleanup_failure: None,
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

/// The first non-blank line of a command's output, capped for a receipt.
fn first_line(output: &str) -> String {
    let line = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no error output");
    // Truncate by CHARACTER. A byte slice panics when the cut lands inside a
    // multi-byte character, and gh output carries them (a PR title, a branch
    // name, a localized git message).
    line.chars().take(200).collect()
}

fn classify_failure(effect: Effect, strategy: &str, output: &str) -> Outcome {
    let lower = output.to_lowercase();
    let reason = if lower.contains("fno/review-coverage") {
        // This verb published that status itself moments ago. GitHub has not
        // observed it yet, so the refusal clears on a retry.
        "fno/review-coverage is still required after its success status was published; \
         GitHub may not have observed the update yet - retry the merge"
            .to_string()
    } else if lower.contains("not mergeable") {
        "not mergeable (conflicts or base changed)".to_string()
    } else if lower.contains("protected") {
        "branch protected".to_string()
    } else if lower.contains("required review") {
        "required review pending".to_string()
    } else {
        format!(
            "gh {} with --{strategy} failed (check the repo allows that merge method): {}",
            effect.word(),
            first_line(output)
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

    fn fno_strings(cwd: &Path, args: &[String]) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String> {
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

    fn node_binding(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        node_binding_probe(cwd, facts)
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

    fn merge_result(&self, cwd: &Path, pr: u64) -> ProbeOutcome {
        match Self::fno(cwd, &["do", "pr", "merge-result-check", &pr.to_string()]) {
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

    fn ci_base(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        let endpoint = |path: String, jq: &str| {
            self.run_gh(
                cwd,
                &["api".to_string(), path, "--jq".to_string(), jq.to_string()],
            )
        };
        let compare = match endpoint(
            format!(
                "repos/{{owner}}/{{repo}}/compare/{}...{}",
                facts.base_ref, facts.head_sha
            ),
            ".behind_by",
        ) {
            Ok((true, output)) => match output.trim().parse::<u64>() {
                Ok(value) => value,
                Err(_) => {
                    return ProbeOutcome::Inconclusive(format!(
                        "ci base compare unreadable: expected behind_by, got {}",
                        first_line(&output)
                    ))
                }
            },
            Ok((false, output)) => {
                return ProbeOutcome::Inconclusive(format!(
                    "ci base compare unreadable: {}",
                    first_line(&output)
                ))
            }
            Err(error) => {
                return ProbeOutcome::Inconclusive(format!("ci base compare unreadable: {error}"))
            }
        };
        let base_tip = match endpoint(
            format!("repos/{{owner}}/{{repo}}/commits/{}", facts.base_ref),
            ".commit.committer.date",
        ) {
            Ok((true, output)) => output.trim().to_string(),
            Ok((false, output)) => {
                return ProbeOutcome::Inconclusive(format!(
                    "ci base tip unreadable: {}",
                    first_line(&output)
                ))
            }
            Err(error) => {
                return ProbeOutcome::Inconclusive(format!("ci base tip unreadable: {error}"))
            }
        };
        let runs = match endpoint(
            format!(
                "repos/{{owner}}/{{repo}}/actions/runs?event=pull_request&head_sha={}&per_page=100",
                facts.head_sha
            ),
            ".workflow_runs[] | [.name, .created_at] | @tsv",
        ) {
            Ok((true, output)) => {
                let mut runs = Vec::new();
                for line in output.lines().filter(|line| !line.trim().is_empty()) {
                    let Some((name, created_at)) = line.split_once('\t') else {
                        return ProbeOutcome::Inconclusive(format!(
                            "ci runs unreadable: expected workflow and created_at, got {}",
                            first_line(line)
                        ));
                    };
                    runs.push((name.to_string(), created_at.to_string()));
                }
                runs
            }
            Ok((false, output)) => {
                return ProbeOutcome::Inconclusive(format!(
                    "ci runs unreadable: {}",
                    first_line(&output)
                ))
            }
            Err(error) => {
                return ProbeOutcome::Inconclusive(format!("ci runs unreadable: {error}"))
            }
        };
        let verdict = ci_base_verdict(compare, &base_tip, &runs);
        let ProbeOutcome::Refused(stale) = verdict else {
            return verdict;
        };
        let pull_head = format!("pull/{}/head", facts.number);
        let fetch = Command::new("git")
            .args([
                "fetch",
                "--no-tags",
                "--quiet",
                "origin",
                &facts.base_ref,
                &pull_head,
            ])
            .current_dir(cwd)
            .output();
        let overlap = match fetch {
            Ok(output) if output.status.success() => {
                let Some((_, since)) = oldest_current_run(&runs) else {
                    return stale_overlap_verdict(
                        stale,
                        facts.number,
                        Err("no current workflow run".to_string()),
                    );
                };
                crate::merge_gates::stale_overlap(
                    cwd,
                    &format!("origin/{}", facts.base_ref),
                    &facts.head_sha,
                    &since,
                )
            }
            Ok(output) => Err(format!(
                "fetch failed: {}",
                first_line(&String::from_utf8_lossy(&output.stderr))
            )),
            Err(error) => Err(format!("fetch failed: {error}")),
        };
        stale_overlap_verdict(stale, facts.number, overlap)
    }

    fn require_fresh_ci(&self, cwd: &Path) -> bool {
        crate::agents_config::auto_merge_require_fresh_ci(cwd)
    }

    fn slot_holder(&self, cwd: &Path, base_ref: &str) -> Result<Option<u64>, String> {
        slot_holder_read(cwd, base_ref)
    }

    fn take_slot(&self, cwd: &Path, base_ref: &str, pr: u64) -> Result<(), String> {
        // `merge-slot:` is repo-local, so rootless claim operations share the
        // current repo space with every worktree.
        let key = slot_key(base_ref);
        let holder = slot_holder_key(pr);
        let (state, record) = claims::status(&key, None);
        let primary_holder = slot_holder_from_record(&key, state, record)?;
        if primary_holder.is_some_and(|existing| existing != pr) {
            return Err(format!(
                "merge slot already held by pr:{}",
                primary_holder.unwrap()
            ));
        }
        let legacy_holder = legacy_slot_holder(cwd, &key)?;
        if legacy_holder.is_some_and(|existing| existing != pr) {
            return Err(format!(
                "merge slot already held by pr:{}",
                legacy_holder.unwrap()
            ));
        }
        if primary_holder.is_none() && legacy_holder.is_some() {
            return Ok(());
        }
        let opts = crate::claims::AcquireOpts {
            pid_unavailable: true,
            ttl_ms: Some(MERGE_SLOT_TTL_MS),
            root: None,
            reason: Some("ci_base_stale merge slot".to_string()),
            ..Default::default()
        };
        match crate::claims::acquire(&key, &holder, opts) {
            crate::claims::AcquireOutcome::Acquired(_) if primary_holder.is_some() => Ok(()),
            crate::claims::AcquireOutcome::Acquired(_) => match legacy_slot_holder(cwd, &key) {
                Ok(None) => Ok(()),
                Ok(Some(existing)) => {
                    let _ = crate::claims::release(&key, &holder, None, None);
                    if existing == pr {
                        Ok(())
                    } else {
                        Err(format!("merge slot already held by pr:{existing}"))
                    }
                }
                Err(error) => {
                    let _ = crate::claims::release(&key, &holder, None, None);
                    Err(error)
                }
            },
            crate::claims::AcquireOutcome::HeldByOther { holder, .. } => {
                Err(format!("merge slot already held by {holder}"))
            }
            crate::claims::AcquireOutcome::Error(error) => Err(error),
        }
    }

    fn release_slot(&self, cwd: &Path, base_ref: &str, pr: u64) {
        let key = slot_key(base_ref);
        let holder = slot_holder_key(pr);
        let _ = crate::claims::release(&key, &holder, None, None);
        if let Some(root) = canonical_repo_root(cwd) {
            let _ = crate::claims::release(&key, &holder, Some(&root), None);
        }
    }

    fn checks_read(&self, cwd: &Path, pr: u64) -> ChecksRead {
        match Self::fno(cwd, &["do", "pr", "status", &pr.to_string()]) {
            Ok((_code, stdout, _stderr)) => parse_checks_read(&stdout),
            Err(_) => ChecksRead {
                verdict: "unknown".to_string(),
                github_block: None,
                optional_unresolved: None,
                rerun_recovered: None,
                rerun_failures: None,
            },
        }
    }

    fn fno_shell(
        &self,
        cwd: &Path,
        args: &[String],
    ) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String> {
        Self::fno_strings(cwd, args)
    }

    fn live_lanes(&self, cwd: &Path) -> usize {
        // The parallel-lane count the Python hold derived (`claims.lanes.
        // active_lane_count`): live `lane-slot:` claims at the canonical
        // repo's own claims root. The global root is other repos' lanes and
        // must not count. A probe miss answers 0 - the Python miss contract
        // that disarms the overlap hold rather than blocking on our own read.
        let Some(repo) = canonical_repo_root(cwd) else {
            return 0;
        };
        let dir = repo.join(crate::claims::CLAIMS_DIRNAME);
        crate::claims::list_in(std::slice::from_ref(&dir), Some("lane-slot:"), false)
            .map(|records| records.len())
            .unwrap_or(0)
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

/// Parse one `fno do pr status` stdout into both facts the door needs. An
/// unreadable read claims neither: `unknown` holds under `require_checks`
/// exactly as before, and no GitHub hold is asserted from output that never
/// named one.
fn parse_checks_read(stdout: &[u8]) -> ChecksRead {
    let unknown = ChecksRead {
        verdict: "unknown".to_string(),
        github_block: None,
        optional_unresolved: None,
        rerun_recovered: None,
        rerun_failures: None,
    };
    let Ok(v) = serde_json::from_slice::<Value>(stdout) else {
        return unknown;
    };
    let optional_unresolved = v.get("optional_reviews_unresolved").map(|raw| {
        if let Some(n) = raw.as_i64() {
            Some(n)
        } else {
            None
        }
    });
    let rerun_recovered = v.get("rerun_recovered").and_then(Value::as_bool);
    let rerun_failures = v.get("recovered_failures").and_then(|raw| {
        let rows = raw.as_array()?;
        Some(
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        )
    });
    let verdict = v
        .get("verdict")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_string());
    let state = v.get("github_merge_state");
    let source = || {
        state
            .and_then(|s| s.get("source"))
            .and_then(Value::as_str)
            .unwrap_or("github_blocked")
            .to_string()
    };
    let github_block = v
        .get("ready_blockers")
        .and_then(Value::as_array)
        .filter(|b| b.iter().any(|b| b.as_str() == Some("github_blocked")))
        .map(|_| {
            match state
                .and_then(|s| s.get("missing_required_checks"))
                .and_then(Value::as_array)
            {
                Some(names) if !names.is_empty() => {
                    let joined = names
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    if joined.is_empty() {
                        source()
                    } else {
                        joined
                    }
                }
                _ => source(),
            }
        });
    ChecksRead {
        verdict,
        github_block,
        optional_unresolved,
        rerun_recovered,
        rerun_failures,
    }
}

/// A merged or closed PR has no would-merge left, for the PR under decision
/// and for a merge-slot holder alike.
fn is_terminal_state(state: &str) -> bool {
    state == "MERGED" || state == "CLOSED"
}

/// The `ci_base_stale` remedy, shared by every `Held` reason that ends in it.
fn stale_remedy(n: u64) -> String {
    format!("remedy: fno do pr rebase {n}, then fno do pr wait {n} --until settled, then retry")
}

/// The merge-slot claim key for a base branch. It is repo-local, so rootless
/// claim operations resolve the shared space directory for every worktree.
fn slot_key(base_ref: &str) -> String {
    format!("merge-slot:{base_ref}")
}

fn slot_holder_key(pr: u64) -> String {
    format!("pr:{pr}")
}

pub(crate) fn parse_slot_holder(holder: &str) -> Option<u64> {
    holder.strip_prefix("pr:")?.parse::<u64>().ok()
}

/// The one merge-slot claim read. Strict polarity kept: a corrupted claim or
/// an unparseable holder is an Err (the merge path refuses on an unreadable
/// slot rather than merging past it); the fail-open consumer
/// ([`merge_slot_holder`]) maps Err to None at its own boundary. The primary
/// read uses the repo-space lockfile; a pre-move lockfile at the canonical root
/// remains a migration fallback until its lease ends.
fn slot_holder_read(cwd: &Path, base_ref: &str) -> Result<Option<u64>, String> {
    let key = slot_key(base_ref);
    let (state, record) = claims::status(&key, None);
    match slot_holder_from_record(&key, state, record)? {
        Some(holder) => Ok(Some(holder)),
        None => legacy_slot_holder(cwd, &key),
    }
}

fn legacy_slot_holder(cwd: &Path, key: &str) -> Result<Option<u64>, String> {
    let Some(root) = canonical_repo_root(cwd) else {
        return Ok(None);
    };
    let (state, record) = claims::status(key, Some(&root));
    slot_holder_from_record(key, state, record)
}

fn slot_holder_from_record(
    key: &str,
    state: ClaimState,
    record: Option<claims::ClaimRecord>,
) -> Result<Option<u64>, String> {
    match state {
        ClaimState::Corrupted => Err(format!("merge slot claim corrupted: {key}")),
        ClaimState::Live | ClaimState::Suspect => {
            let record = record
                .ok_or_else(|| format!("merge slot claim {key} read {state:?} with no record"))?;
            parse_slot_holder(&record.holder)
                .map(Some)
                .ok_or_else(|| format!("merge slot holder unparseable: {}", record.holder))
        }
        _ => Ok(None),
    }
}
/// The live merge-slot holder for `base_ref`, fail-open: any claims fault
/// reads as None so a consumer that only decides whether idling is safe (the
/// loopcheck classifier) never blocks on a claims io error. Some(pr) only for
/// a LIVE or SUSPECT slot whose holder parses; a self-held slot stays
/// Some(self) and the caller filters it.
pub(crate) fn merge_slot_holder(cwd: &Path, base_ref: &str) -> Option<u64> {
    slot_holder_read(cwd, base_ref).ok().flatten()
}

fn probe_detail(stdout: &[u8], stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr).trim().to_string();
    if err.is_empty() {
        String::from_utf8_lossy(stdout).trim().to_string()
    } else {
        err
    }
}

/// The node-binding gate over the live graph. The scope is the canonical repo
/// root: a repo whose graph names no node under it (a stock install that
/// never used the backlog, an external-tracker project) has nothing to bind
/// to and merges as before.
fn node_binding_probe(cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
    let root = canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let graph_path = crate::king_board::scope::graph_json_path(cwd);
    let store = GraphStore::new(&graph_path);
    match backlog_api::rows(&store) {
        Err(e) => ProbeOutcome::Inconclusive(format!(
            "graph unreadable ({}); refusing to assume bound",
            e.0
        )),
        Ok(entries) => node_binding_from_entries(&root, &entries, facts),
    }
}

/// The binding decision over already-read graph entries: the pure half of
/// [`node_binding_probe`], so a unit test needs no filesystem. The three
/// keys are the board classifier's own (`king_board::prs::pr_binding_keys`).
fn node_binding_from_entries(root: &Path, entries: &[Value], facts: &PrFacts) -> ProbeOutcome {
    if detect_project(entries, &root.to_string_lossy()).is_none() {
        return ProbeOutcome::Clear;
    }
    let Some(body) = facts.body.as_deref() else {
        return ProbeOutcome::Inconclusive(
            "PR body was not in the fetch (deployed fno predates the body field; \
             fno doctor update); refusing to assume bound"
                .to_string(),
        );
    };
    let keys = pr_binding_keys(
        facts.number as i64,
        &facts.head_ref,
        Some(&facts.url),
        Some(body),
        entries,
    );
    if let Some(detail) = keys.unbound_detail() {
        if facts.url.is_empty() {
            // The backref key scopes by url; with none it was unevaluable,
            // not empty, so the answer is Unknown rather than a refusal.
            return ProbeOutcome::Inconclusive(
                "PR carried no comparable url, so the graph back-pointer key \
                 could not be scoped; refusing to assume unbound"
                    .to_string(),
            );
        }
        return ProbeOutcome::Refused(format!(
            "PR {n} is unbound: {detail}. A merge the graph cannot see is refused. \
             Bind it: pick or file the node (fno backlog idea \"...\"), run \
             fno do pr closure-trailer <id> [--extra <id> ...], append the \
             printed ONE line to the PR \
             body, then retry. A revert or hotfix binds the same way; no flag \
             bypasses this gate.",
            n = facts.number
        ));
    }
    ProbeOutcome::Clear
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
        url: payload
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // The body rides this same payload. `None` means the key was absent:
        // an out-of-date deployed `fno`, not a bodyless PR.
        body: payload
            .get("body")
            .map(|v| v.as_str().unwrap_or("").to_string()),
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

fn valid_github_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
}

fn oldest_current_run(runs: &[(String, String)]) -> Option<(String, String)> {
    let mut newest_by_workflow: Vec<(String, String)> = Vec::new();
    for (name, created_at) in runs {
        if let Some((_, newest)) = newest_by_workflow
            .iter_mut()
            .find(|(known, _)| known == name)
        {
            if created_at > newest {
                *newest = created_at.clone();
            }
        } else {
            newest_by_workflow.push((name.clone(), created_at.clone()));
        }
    }
    newest_by_workflow
        .into_iter()
        .min_by(|(_, left), (_, right)| left.cmp(right))
}

pub(crate) fn stale_overlap_verdict(
    stale: String,
    pr: u64,
    overlap: Result<crate::merge_gates::StaleOverlap, String>,
) -> ProbeOutcome {
    match overlap {
        Ok(result) if result.shared.is_empty() => {
            eprintln!(
                "pr-merge: ci_base_stale waived: {} files landed since CI base {}, none shared with PR {pr}",
                result.landed,
                result.ci_base_sha.chars().take(8).collect::<String>()
            );
            ProbeOutcome::Clear
        }
        Ok(result) => {
            let shown = result
                .shared
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let extra = if result.shared.len() > 3 {
                format!(" and {} more", result.shared.len() - 3)
            } else {
                String::new()
            };
            ProbeOutcome::Refused(format!(
                "{stale}; shares {} files with main since CI base {}: {shown}{extra}",
                result.shared.len(),
                result.ci_base_sha.chars().take(8).collect::<String>()
            ))
        }
        Err(error) => ProbeOutcome::Refused(format!("{stale}; file overlap unreadable ({error})")),
    }
}

/// Did the green runs test a merge ref that already held the base tip?
pub fn ci_base_verdict(
    behind_by: u64,
    base_tip_at: &str,
    runs: &[(String, String)],
) -> ProbeOutcome {
    if behind_by == 0 || runs.is_empty() {
        return ProbeOutcome::Clear;
    }
    if !valid_github_timestamp(base_tip_at)
        || runs
            .iter()
            .any(|(_, created_at)| !valid_github_timestamp(created_at))
    {
        return ProbeOutcome::Inconclusive(
            "ci base freshness unreadable: timestamp is not YYYY-MM-DDTHH:MM:SSZ".to_string(),
        );
    }

    let Some((name, created_at)) = oldest_current_run(runs) else {
        return ProbeOutcome::Clear;
    };
    if created_at.as_str() >= base_tip_at {
        ProbeOutcome::Clear
    } else {
        ProbeOutcome::Refused(format!(
            "ci_base_stale: the oldest current run ({name}, created {created_at}) predates base tip {base_tip_at}; PR is {behind_by} behind"
        ))
    }
}

/// The head sha from the latest covered `review_coverage` event that matches the
/// current HEAD, or None.
pub fn covered_head_from_event(cwd: &Path) -> Option<String> {
    let path = crate::paths::events_path(cwd);
    let content = crate::event_store::journal_text(&path, &["review_coverage"]);
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

/// The merges hold, read the way the spawn gate reads its breaker: a stop
/// holding merges (or an unreadable record, fail closed) refuses the merge
/// primitive with the breaker generation and reason. `None` admits.
fn merges_breaker_refusal() -> Option<(i32, String)> {
    match crate::fleet_incident::verdict_for("merges") {
        crate::fleet_incident::Verdict::Clear(_) => None,
        crate::fleet_incident::Verdict::Stopped(r) => Some((
            crate::spawn_gate::EXIT_FLEET_STOP,
            format!(
                "refused: fleet incident stop holds merges (generation {}, reason: {}); \
                 reopen with `fno agents incident clear --reason <text>`\n",
                r.generation, r.reason
            ),
        )),
        crate::fleet_incident::Verdict::Unavailable(d) => Some((
            crate::spawn_gate::EXIT_FLEET_STOP_UNAVAILABLE,
            format!(
                "refused: fleet incident state is unreadable ({d}); the merge primitive fails closed\n"
            ),
        )),
    }
}

/// Test-friendly variant: returns (exit_code, stdout, stderr) without printing.
pub fn run_authorized_merge_capture(args: &[String]) -> (i32, String, String) {
    let payload: Value = match read_payload(args) {
        Ok(value) => value,
        Err(message) => return (2, String::new(), message),
    };
    // The hold ops are merge-authority writes riding this verb's payload, not
    // a new top-level root: `{"op": "hold-set"|"hold-release", ...}` answers
    // with one receipt instead of a merge verdict.
    if payload
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op.starts_with("hold-"))
    {
        let out = crate::merge_hold::run(
            payload.get("op").and_then(Value::as_str).unwrap_or(""),
            &payload,
        );
        return (0, out, String::new());
    }
    // The grant ops are the durable-grant reader riding this verb's payload,
    // the same transport the hold ops use: `{"op": "grant-verdict", ...}`
    // answers one PR, `{"op": "grant-queue", ...}` answers the merge queue.
    if payload
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op.starts_with("grant-"))
    {
        let out = crate::merge_grant::run_op(
            payload.get("op").and_then(Value::as_str).unwrap_or(""),
            &payload,
        );
        return (0, out, String::new());
    }
    // The status ops are the pr-status fact readers riding this verb's
    // payload, the same transport the hold and grant ops use:
    // `{"op": "status-merge-blocker"|"status-failure-cause", ...}`.
    if payload
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op.starts_with("status-"))
    {
        let out = crate::pr_status_facts::run_op(
            payload.get("op").and_then(Value::as_str).unwrap_or(""),
            &payload,
        );
        return (0, out, String::new());
    }
    let request = match parse_request(&payload) {
        Ok(request) => request,
        Err(message) => return (2, String::new(), format!("authorized-merge: {message}\n")),
    };
    // The merges hold: the one merge primitive refuses like the spawn gate
    // does, naming the breaker generation and reason, before any probe or
    // queue work. Reads stay reads: preview and decide-only asks answer
    // normally, and the quota ops above never touch the breaker.
    if !request.decide_only && matches!(request.effect, Effect::Merge | Effect::Arm) {
        if let Some((code, message)) = merges_breaker_refusal() {
            return (code, String::new(), message);
        }
    }
    // A preview ask answers with the structured receipt: `ready` is the
    // receipt's `blockers` being empty, so the verb prints the list itself
    // instead of the joined-prose Outcome form the effect arms render.
    if request.effect == Effect::Preview {
        let cwd = request.cwd.as_path();
        let receipt = match RealProbes.pr_facts(cwd, request.pr) {
            Err(reason) => serde_json::json!({ "outcome": "unknown", "reason": reason }),
            Ok(facts) => match preview_walk(&RealProbes, &request, &facts) {
                PreviewVerdict::Go { waiver } => {
                    let mut receipt = serde_json::json!({
                        "outcome": "authorized",
                        "head": facts.head_sha,
                        "blockers": [],
                    });
                    if let Some(note) = waiver {
                        receipt["coverage_waiver"] = Value::String(note);
                    }
                    receipt
                }
                PreviewVerdict::Blocked(rows) => serde_json::json!({
                    "outcome": "held",
                    "head": facts.head_sha,
                    "blockers": rows
                        .iter()
                        .map(|b| serde_json::json!({
                            "code": b.code,
                            "class": match b.class {
                                BlockerClass::Held => "held",
                                BlockerClass::Refused => "refused",
                                BlockerClass::Unknown => "unknown",
                            },
                            "detail": b.detail,
                        }))
                        .collect::<Vec<_>>(),
                }),
            },
        };
        return (0, format!("{}\n", receipt), String::new());
    }
    // The receipt is the verdict, so the exit code answers only whether the verb
    // RAN: 0 with a receipt on stdout, 2 when the payload was unusable. A code
    // that also encoded refusal would make a held merge indistinguishable from a
    // binary that could not start, and the caller reads the receipt either way.
    let outcome = run(&RealProbes, &request);
    (0, format!("{}\n", outcome.to_json()), String::new())
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
        .ok_or_else(|| "payload needs effect merge|arm|preview".to_string())?;
    Ok(Request {
        cwd,
        pr: payload.get("pr").and_then(Value::as_u64),
        effect,
        approved: payload.get("approved").and_then(Value::as_bool),
        auto_merge_source: payload
            .get("auto_merge_source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        // Preview defaults its CI gate ON: the question is "may this head
        // merge NOW", and a merge ask carries its own posture. A payload can
        // still pass require_checks: false to ask the gates without CI.
        require_checks: payload
            .get("require_checks")
            .and_then(Value::as_bool)
            .unwrap_or(effect == Effect::Preview),
        accept_flake: payload
            .get("accept_flake")
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
        authority: payload
            .get("authority")
            .and_then(Value::as_str)
            .map(str::to_owned),
        supplied_verdict: payload
            .get("verdict")
            .and_then(Value::as_str)
            .map(str::to_owned),
        supplied_counts: payload.get("counts").cloned(),
        supplied_rerun_recovered: payload.get("rerun_recovered").and_then(Value::as_bool),
        supplied_optional_unresolved: payload.get("optional_reviews_unresolved").map(|v| {
            if let Some(n) = v.as_i64() {
                Some(n)
            } else {
                None
            }
        }),
        supplied_github_blockers: payload
            .get("github_blockers")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        facts: Option<PrFacts>,
        facts_error: Option<String>,
        /// Answer for the binding probe alone; `None` (the default) reads Clear.
        node_binding: Option<ProbeOutcome>,
        dispatch_hold: Option<ProbeOutcome>,
        review_hold: Option<ProbeOutcome>,
        lineage: Option<ProbeOutcome>,
        merge_result: Option<ProbeOutcome>,
        ci_base: Option<ProbeOutcome>,
        fresh_ci: Option<bool>,
        ci_base_calls: RefCell<u32>,
        checks: Option<String>,
        /// GitHub's ruleset hold for the PR under decision; `None` (the
        /// default) reads no hold, so every pre-existing test keeps its behavior.
        github_block: Option<String>,
        /// Optional-review answer for the PR under decision; the default
        /// `Some(Some(0))` keeps every pre-existing test passing.
        optional_unresolved: Option<Option<i64>>,
        /// Rerun-recovery answer for the PR under decision.
        rerun_recovered: Option<bool>,
        /// The coverage verb's exit for the PR under decision; None reads 0.
        coverage_exit: Option<i32>,
        /// The plan-fidelity gate's verdict for a test-armable refusal.
        plan_fidelity_refused: bool,
        covered_head: Option<String>,
        enabled: bool,
        floor: Option<String>,
        gh_ok: bool,
        gh_output: String,
        /// Answer for the REST recovery call alone, so a test can fail the
        /// `gh pr merge` and let the retry succeed.
        gh_recovery_ok: Option<bool>,
        gh_calls: RefCell<Vec<Vec<String>>>,
        /// Simulated merge-slot claim: `None` is free, `Some(pr)` is held.
        slot: RefCell<Option<u64>>,
        slot_holder_err: bool,
        take_slot_err: bool,
        take_slot_calls: RefCell<Vec<u64>>,
        release_slot_calls: RefCell<Vec<u64>>,
        /// Facts and checks for a PR other than the one under decision, keyed
        /// by PR number - a holder read in the slot logic.
        other_facts: RefCell<HashMap<u64, PrFacts>>,
        other_checks: RefCell<HashMap<u64, String>>,
        /// Dispatch holds keyed by PR, so a test can hold a holder without
        /// holding the PR under decision.
        other_holds: RefCell<HashMap<u64, ProbeOutcome>>,
    }

    fn open_facts() -> PrFacts {
        PrFacts {
            number: 7,
            head_sha: "abc123".to_string(),
            head_ref: "feature/x".to_string(),
            base_ref: "main".to_string(),
            url: "https://github.com/o/r/pull/7".to_string(),
            body: Some("Backlog-Closure: x-aaaa\n".to_string()),
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
            optional_unresolved: Some(Some(0)),
            ..Default::default()
        }
    }

    struct ClaimsRootRestore(Option<std::ffi::OsString>);

    impl Drop for ClaimsRootRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
                None => std::env::remove_var("FNO_CLAIMS_ROOT"),
            }
        }
    }

    fn with_claims_root<T>(root: &Path, f: impl FnOnce() -> T) -> T {
        let _env_lock = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let restore = ClaimsRootRestore(std::env::var_os("FNO_CLAIMS_ROOT"));
        std::env::set_var("FNO_CLAIMS_ROOT", root);
        let result = f();
        drop(restore);
        result
    }

    #[test]
    fn slot_holder_reads_lockfiles_and_refuses_corrupted_claims() {
        let temp = tempfile::TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = slot_key("main");
            assert!(matches!(
                claims::acquire(
                    &key,
                    &slot_holder_key(17),
                    claims::AcquireOpts {
                        pid_unavailable: true,
                        ttl_ms: Some(MERGE_SLOT_TTL_MS),
                        ..Default::default()
                    }
                ),
                claims::AcquireOutcome::Acquired(_)
            ));
            assert_eq!(
                slot_holder_read(Path::new("/repo"), "main").unwrap(),
                Some(17)
            );
            assert!(
                !temp.path().join("graph.db").exists(),
                "merge slots must use the claim lockfiles"
            );

            let corrupt = slot_key("broken");
            let path = claims::claim_path(&corrupt, None).unwrap();
            std::fs::write(path, "not: [valid yaml").unwrap();
            let error = slot_holder_read(Path::new("/repo"), "broken").unwrap_err();
            assert!(error.contains("corrupted"), "{error}");
        });
    }

    #[test]
    fn take_slot_refuses_a_live_legacy_lockfile_without_creating_a_second_slot() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(init.success());

        let current_root = temp.path().join("current");
        with_claims_root(&current_root, || {
            let key = slot_key("main");
            assert!(matches!(
                claims::acquire(
                    &key,
                    &slot_holder_key(8),
                    claims::AcquireOpts {
                        root: Some(repo.clone()),
                        pid_unavailable: true,
                        ttl_ms: Some(MERGE_SLOT_TTL_MS),
                        ..Default::default()
                    }
                ),
                claims::AcquireOutcome::Acquired(_)
            ));

            let error = RealProbes.take_slot(&repo, "main", 9).unwrap_err();
            assert!(error.contains("pr:8"), "{error}");
            assert!(!claims::claim_path(&key, None).unwrap().exists());
        });
    }

    impl Probes for Fake {
        fn pr_facts(&self, _cwd: &Path, pr: Option<u64>) -> Result<PrFacts, String> {
            if let Some(error) = &self.facts_error {
                return Err(error.clone());
            }
            let main = self.facts.clone().ok_or_else(|| "no facts".to_string())?;
            match pr {
                Some(n) if n != main.number => self
                    .other_facts
                    .borrow()
                    .get(&n)
                    .cloned()
                    .ok_or_else(|| format!("no facts for pr {n}")),
                _ => Ok(main),
            }
        }
        fn node_binding(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
            self.node_binding.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn dispatch_hold(&self, _cwd: &Path, pr: u64) -> ProbeOutcome {
            if let Some(hold) = self.other_holds.borrow().get(&pr) {
                return hold.clone();
            }
            self.dispatch_hold.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn review_hold(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
            self.review_hold.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn base_lineage(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
            self.lineage.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn merge_result(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
            self.merge_result.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn ci_base(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
            *self.ci_base_calls.borrow_mut() += 1;
            self.ci_base.clone().unwrap_or(ProbeOutcome::Clear)
        }
        fn require_fresh_ci(&self, _cwd: &Path) -> bool {
            self.fresh_ci.unwrap_or(true)
        }
        fn slot_holder(&self, _cwd: &Path, _base_ref: &str) -> Result<Option<u64>, String> {
            if self.slot_holder_err {
                return Err("merge slot claim corrupted".to_string());
            }
            Ok(*self.slot.borrow())
        }
        fn take_slot(&self, _cwd: &Path, _base_ref: &str, pr: u64) -> Result<(), String> {
            self.take_slot_calls.borrow_mut().push(pr);
            if self.take_slot_err {
                return Err("merge slot already held by pr:99".to_string());
            }
            *self.slot.borrow_mut() = Some(pr);
            Ok(())
        }
        fn release_slot(&self, _cwd: &Path, _base_ref: &str, pr: u64) {
            self.release_slot_calls.borrow_mut().push(pr);
            let mut held = self.slot.borrow_mut();
            if *held == Some(pr) {
                *held = None;
            }
        }
        fn checks_read(&self, _cwd: &Path, pr: u64) -> ChecksRead {
            let mine = self.facts.as_ref().map(|f| f.number) == Some(pr);
            let verdict = if mine {
                self.checks.clone().unwrap_or_else(|| "green".to_string())
            } else {
                self.other_checks
                    .borrow()
                    .get(&pr)
                    .cloned()
                    .unwrap_or_else(|| "green".to_string())
            };
            ChecksRead {
                verdict,
                github_block: if mine {
                    self.github_block.clone()
                } else {
                    None
                },
                optional_unresolved: if mine {
                    self.optional_unresolved
                } else {
                    Some(Some(0))
                },
                rerun_recovered: if mine { self.rerun_recovered } else { None },
                rerun_failures: None,
            }
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
            if args.first().map(String::as_str) == Some("api") {
                if let Some(ok) = self.gh_recovery_ok {
                    return Ok((ok, String::new()));
                }
            }
            Ok((self.gh_ok, self.gh_output.clone()))
        }
        fn fno_shell(
            &self,
            _cwd: &Path,
            args: &[String],
        ) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String> {
            let covered = if self.plan_fidelity_refused {
                br#"{"refused": true, "reason": "test"}"#.to_vec()
            } else {
                br#"{"refused": false}"#.to_vec()
            };
            // The coverage ask is the `do pr coverage-check` shell; any other
            // fno-shell call is a fidelity ask in these tests.
            let is_coverage = args.len() >= 3 && args[2] == "coverage-check";
            Ok((
                if is_coverage {
                    self.coverage_exit.or(Some(0))
                } else {
                    Some(0)
                },
                if is_coverage { Vec::new() } else { covered },
                Vec::new(),
            ))
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
            authority: None,
            accept_flake: false,
            supplied_verdict: None,
            supplied_counts: None,
            supplied_rerun_recovered: None,
            supplied_optional_unresolved: None,
            supplied_github_blockers: None,
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
    fn a_stale_pr_takes_the_free_slot_and_holds_a_second_stale_pr_behind_it() {
        // AC1-HP.
        let mut fake = Fake {
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 3 behind".to_string())),
            ..clean()
        };
        let req7 = Request {
            require_checks: true,
            pr: Some(7),
            ..request(Effect::Merge)
        };
        let outcome7 = run(&fake, &req7);
        assert_eq!(outcome7.word(), "held");
        assert!(outcome7.detail().contains("PR 7 now holds the merge slot"));
        assert_eq!(*fake.slot.borrow(), Some(7));

        fake.facts = Some(PrFacts {
            number: 8,
            head_sha: "def456".to_string(),
            ..open_facts()
        });
        fake.covered_head = Some("def456".to_string());
        let req8 = Request {
            require_checks: true,
            pr: Some(8),
            ..request(Effect::Merge)
        };
        let outcome8 = run(&fake, &req8);
        assert_eq!(outcome8.word(), "held");
        let detail8 = outcome8.detail();
        assert!(detail8.contains("merge_slot_held"));
        assert!(detail8.contains("PR 7"));
        assert_eq!(
            *fake.slot.borrow(),
            Some(7),
            "the slot must stay with the first holder"
        );
    }

    #[test]
    fn a_holder_with_fresh_ci_clears_and_releases_the_slot_on_merge_while_a_racer_waits() {
        // AC2-HP.
        let mut fake = Fake {
            slot: RefCell::new(Some(7)),
            facts: Some(PrFacts {
                number: 9,
                head_sha: "nine".to_string(),
                ..open_facts()
            }),
            covered_head: Some("nine".to_string()),
            ..clean()
        };
        let req9 = Request {
            require_checks: true,
            pr: Some(9),
            ..request(Effect::Merge)
        };
        let outcome9 = run(&fake, &req9);
        assert_eq!(outcome9.word(), "held");
        let detail9 = outcome9.detail();
        assert!(detail9.contains("merge_slot_held"));
        assert!(detail9.contains("PR 7"));
        assert_eq!(
            *fake.slot.borrow(),
            Some(7),
            "the waiting PR must not take the slot"
        );

        fake.facts = Some(PrFacts {
            number: 7,
            head_sha: "abc123".to_string(),
            ..open_facts()
        });
        fake.covered_head = Some("abc123".to_string());
        let req7 = Request {
            require_checks: true,
            pr: Some(7),
            ..request(Effect::Merge)
        };
        let outcome7 = run(&fake, &req7);
        assert_eq!(
            outcome7.word(),
            "merged",
            "the slot holder with fresh CI clears and merges"
        );
        assert_eq!(
            *fake.slot.borrow(),
            None,
            "the slot releases once the Merged outcome lands"
        );
    }

    #[test]
    fn a_holder_that_went_red_releases_its_slot_to_a_waiting_stale_pr() {
        // AC3-EDGE.
        let fake = Fake {
            slot: RefCell::new(Some(7)),
            facts: Some(PrFacts {
                number: 8,
                head_sha: "eight".to_string(),
                ..open_facts()
            }),
            covered_head: Some("eight".to_string()),
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 4 behind".to_string())),
            ..clean()
        };
        fake.other_facts.borrow_mut().insert(
            7,
            PrFacts {
                number: 7,
                state: "OPEN".to_string(),
                ..open_facts()
            },
        );
        fake.other_checks.borrow_mut().insert(7, "red".to_string());

        let req8 = Request {
            require_checks: true,
            pr: Some(8),
            ..request(Effect::Merge)
        };
        let outcome8 = run(&fake, &req8);
        assert_eq!(outcome8.word(), "held");
        assert!(outcome8.detail().contains("PR 8 now holds the merge slot"));
        assert_eq!(*fake.slot.borrow(), Some(8));
        assert_eq!(*fake.release_slot_calls.borrow(), vec![7]);
    }

    #[test]
    fn a_holder_under_a_dispatch_hold_releases_its_slot_to_a_waiting_stale_pr() {
        // A held holder is neither terminal nor red, so only the hold read
        // frees it. The hold answers per PR: if the fake keyed it globally,
        // PR 8 would refuse at the hold gate before reaching the slot logic.
        let fake = Fake {
            slot: RefCell::new(Some(7)),
            facts: Some(PrFacts {
                number: 8,
                head_sha: "eight".to_string(),
                ..open_facts()
            }),
            covered_head: Some("eight".to_string()),
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 4 behind".to_string())),
            ..clean()
        };
        fake.other_facts.borrow_mut().insert(
            7,
            PrFacts {
                number: 7,
                state: "OPEN".to_string(),
                ..open_facts()
            },
        );
        fake.other_checks
            .borrow_mut()
            .insert(7, "green".to_string());
        fake.other_holds.borrow_mut().insert(
            7,
            ProbeOutcome::Refused("dispatch_hold: held by the crown for a queued node".to_string()),
        );

        let req8 = Request {
            require_checks: true,
            pr: Some(8),
            ..request(Effect::Merge)
        };
        let outcome8 = run(&fake, &req8);
        assert_eq!(outcome8.word(), "held");
        assert!(outcome8.detail().contains("PR 8 now holds the merge slot"));
        assert_eq!(*fake.slot.borrow(), Some(8));
        assert_eq!(*fake.release_slot_calls.borrow(), vec![7]);
    }

    #[test]
    fn an_inconclusive_hold_read_keeps_the_slot_with_its_holder() {
        // The eviction hold read is fail_open: an unreadable hold must not
        // evict, so the lease stays the bound.
        let fake = Fake {
            slot: RefCell::new(Some(7)),
            facts: Some(PrFacts {
                number: 8,
                head_sha: "eight".to_string(),
                ..open_facts()
            }),
            covered_head: Some("eight".to_string()),
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 4 behind".to_string())),
            ..clean()
        };
        fake.other_facts.borrow_mut().insert(
            7,
            PrFacts {
                number: 7,
                state: "OPEN".to_string(),
                ..open_facts()
            },
        );
        fake.other_checks
            .borrow_mut()
            .insert(7, "green".to_string());
        fake.other_holds.borrow_mut().insert(
            7,
            ProbeOutcome::Inconclusive("hold-check could not run".to_string()),
        );

        let req8 = Request {
            require_checks: true,
            pr: Some(8),
            ..request(Effect::Merge)
        };
        let outcome8 = run(&fake, &req8);
        assert_eq!(outcome8.word(), "held");
        assert!(outcome8.detail().contains("merge_slot_held"));
        assert_eq!(*fake.slot.borrow(), Some(7));
        assert!(fake.release_slot_calls.borrow().is_empty());
    }

    #[test]
    fn an_arm_never_releases_the_slot() {
        // decide() takes the slot only under Effect::Merge, so the release in
        // run() belongs to the same effect: an armed holder keeps the slot
        // until its merge lands, which the eviction then reads as terminal.
        let fake = Fake {
            slot: RefCell::new(Some(7)),
            ..clean()
        };
        let req = Request {
            require_checks: true,
            pr: Some(7),
            ..request(Effect::Arm)
        };
        let outcome = run(&fake, &req);
        assert_eq!(outcome.word(), "armed");
        assert!(fake.release_slot_calls.borrow().is_empty());
        assert_eq!(*fake.slot.borrow(), Some(7));
    }

    #[test]
    fn a_holder_whose_merge_attempt_fails_releases_its_own_slot() {
        // run() releases unconditionally once effect() has run (Merged,
        // Failed, HeadChanged, or Unknown alike): the slot's protective job
        // is done the moment decide() clears, so nothing after that should
        // starve the queue for the rest of the 60m lease. Failed exercises
        // it here; the release call itself no longer branches on outcome.
        let fake = Fake {
            slot: RefCell::new(Some(7)),
            gh_ok: false,
            gh_output: "not mergeable (conflicts or base changed)".to_string(),
            ..clean()
        };
        let req = Request {
            require_checks: true,
            pr: Some(7),
            ..request(Effect::Merge)
        };
        let outcome = run(&fake, &req);
        assert_eq!(outcome.word(), "failed");
        assert_eq!(
            *fake.slot.borrow(),
            None,
            "a durable merge failure must free the slot rather than starve the queue"
        );
    }

    #[test]
    fn slot_holder_error_fails_open_to_the_stale_hold_without_taking_a_slot() {
        // AC4-ERR.
        let fake = Fake {
            slot_holder_err: true,
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 5 behind".to_string())),
            ..clean()
        };
        let req7 = Request {
            require_checks: true,
            pr: Some(7),
            ..request(Effect::Merge)
        };
        let outcome = run(&fake, &req7);
        assert_eq!(outcome.word(), "held");
        let detail = outcome.detail();
        assert!(detail.contains("ci_base_stale"));
        assert!(
            !detail.contains("merge slot"),
            "a fail-open read must not mention the slot"
        );
        assert!(fake.take_slot_calls.borrow().is_empty());
    }

    #[test]
    fn ci_base_verdict_refuses_the_pr_2094_shape() {
        let runs = (0..8)
            .map(|i| (format!("workflow-{i}"), "2026-09-16T09:17:32Z".to_string()))
            .collect::<Vec<_>>();
        let outcome = ci_base_verdict(3, "2026-09-16T09:56:52Z", &runs);
        assert!(
            matches!(outcome, ProbeOutcome::Refused(reason) if reason.contains("ci_base_stale"))
        );
    }

    #[test]
    fn ci_base_verdict_uses_each_workflows_newest_run() {
        let runs = vec![
            ("cli-ci".to_string(), "2026-09-16T09:17:32Z".to_string()),
            ("cli-ci".to_string(), "2026-09-16T10:00:00Z".to_string()),
            ("rust-ci".to_string(), "2026-09-16T10:00:01Z".to_string()),
        ];
        assert_eq!(
            ci_base_verdict(3, "2026-09-16T09:56:52Z", &runs),
            ProbeOutcome::Clear
        );
    }

    #[test]
    fn ci_base_verdict_clears_when_head_contains_base() {
        let runs = vec![("cli-ci".to_string(), "2026-09-16T09:17:32Z".to_string())];
        assert_eq!(
            ci_base_verdict(0, "2026-09-16T09:56:52Z", &runs),
            ProbeOutcome::Clear
        );
    }

    #[test]
    fn ci_base_verdict_clears_without_runs() {
        assert_eq!(
            ci_base_verdict(3, "2026-09-16T09:56:52Z", &[]),
            ProbeOutcome::Clear
        );
    }

    #[test]
    fn ci_base_verdict_is_inconclusive_for_malformed_timestamps() {
        let runs = vec![("cli-ci".to_string(), "not-a-timestamp".to_string())];
        assert!(matches!(
            ci_base_verdict(3, "2026-09-16T09:56:52Z", &runs),
            ProbeOutcome::Inconclusive(_)
        ));
    }

    #[test]
    fn oldest_current_run_uses_each_workflows_newest_run_then_takes_the_oldest() {
        let runs = vec![
            ("cli-ci".to_string(), "2026-09-16T09:17:32Z".to_string()),
            ("cli-ci".to_string(), "2026-09-16T10:00:00Z".to_string()),
            ("rust-ci".to_string(), "2026-09-16T10:00:01Z".to_string()),
        ];
        assert_eq!(
            oldest_current_run(&runs),
            Some(("cli-ci".to_string(), "2026-09-16T10:00:00Z".to_string()))
        );
    }

    #[test]
    fn oldest_current_run_returns_none_without_workflow_runs() {
        assert_eq!(oldest_current_run(&[]), None);
    }

    #[test]
    fn a_stale_ci_base_clears_when_no_changed_files_are_shared() {
        let outcome = stale_overlap_verdict(
            "ci_base_stale: old run".to_string(),
            2094,
            Ok(crate::merge_gates::StaleOverlap {
                ci_base_sha: "abcdef123456".to_string(),
                landed: 4,
                shared: Vec::new(),
            }),
        );
        assert_eq!(outcome, ProbeOutcome::Clear);
    }

    #[test]
    fn a_disjoint_stale_ci_base_handles_a_malformed_short_sha_without_panicking() {
        let outcome = stale_overlap_verdict(
            "ci_base_stale: old run".to_string(),
            2094,
            Ok(crate::merge_gates::StaleOverlap {
                ci_base_sha: "abcdefgé".to_string(),
                landed: 1,
                shared: Vec::new(),
            }),
        );
        assert_eq!(outcome, ProbeOutcome::Clear);
    }

    #[test]
    fn a_stale_ci_base_refuses_with_shared_paths_and_a_bounded_list() {
        let outcome = stale_overlap_verdict(
            "ci_base_stale: old run".to_string(),
            8,
            Ok(crate::merge_gates::StaleOverlap {
                ci_base_sha: "abcdef123456".to_string(),
                landed: 5,
                shared: vec![
                    "docs/guide.md".to_string(),
                    "hooks/a.json".to_string(),
                    "hooks/b.json".to_string(),
                    "hooks/c.json".to_string(),
                ],
            }),
        );
        assert!(matches!(outcome, ProbeOutcome::Refused(reason)
            if reason.starts_with("ci_base_stale")
                && reason.contains("docs/guide.md")
                && reason.contains("hooks/a.json")
                && reason.contains("hooks/b.json")
                && reason.contains("and 1 more")
                && !reason.contains("hooks/c.json")));
    }

    #[test]
    fn an_unreadable_stale_overlap_fails_closed() {
        let outcome = stale_overlap_verdict(
            "ci_base_stale: old run".to_string(),
            8,
            Err("fetch failed".to_string()),
        );
        assert!(matches!(outcome, ProbeOutcome::Refused(reason)
            if reason.starts_with("ci_base_stale")
                && reason.contains("file overlap unreadable (fetch failed)")));
    }

    #[test]
    fn a_stale_ci_base_holds_a_checked_merge_with_a_remedy() {
        let fake = Fake {
            ci_base: Some(ProbeOutcome::Refused(
                "ci_base_stale: run predates base".to_string(),
            )),
            ..clean()
        };
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        let outcome = run(&fake, &req);
        assert_eq!(outcome.word(), "held");
        assert!(outcome.detail().contains("ci_base_stale"));
        assert!(outcome.detail().contains("fno do pr rebase"));
        assert!(fake.gh_calls.borrow().is_empty());
        assert_eq!(*fake.ci_base_calls.borrow(), 1);
    }

    #[test]
    fn an_arm_skips_ci_base_freshness() {
        let fake = Fake {
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: old".to_string())),
            ..clean()
        };
        let mut req = request(Effect::Arm);
        req.require_checks = true;
        assert_eq!(run(&fake, &req).word(), "armed");
        assert_eq!(*fake.ci_base_calls.borrow(), 0);
    }

    #[test]
    fn a_merge_without_required_checks_skips_ci_base_freshness() {
        let fake = Fake {
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: old".to_string())),
            ..clean()
        };
        assert_eq!(run(&fake, &request(Effect::Merge)).word(), "merged");
        assert_eq!(*fake.ci_base_calls.borrow(), 0);
    }

    #[test]
    fn an_inconclusive_ci_base_probe_fails_open() {
        let fake = Fake {
            ci_base: Some(ProbeOutcome::Inconclusive("gh unavailable".to_string())),
            ..clean()
        };
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        assert_eq!(run(&fake, &req).word(), "merged");
    }

    #[test]
    fn disabled_fresh_ci_requirement_skips_the_probe() {
        let fake = Fake {
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: old".to_string())),
            fresh_ci: Some(false),
            ..clean()
        };
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        assert_eq!(run(&fake, &req).word(), "merged");
        assert_eq!(*fake.ci_base_calls.borrow(), 0);
    }

    #[test]
    fn an_unbound_pr_refuses_both_effects_and_calls_no_gh() {
        // The merge the graph cannot see is refused before any hold or check
        // read: retrying without binding changes nothing, so Held would name
        // the wrong remedy.
        for effect in [Effect::Merge, Effect::Arm] {
            let fake = Fake {
                node_binding: Some(ProbeOutcome::Refused(
                    "PR 7 is unbound: branch names no node; no node carries this PR; \
                     body carries no closure line. A merge the graph cannot \
                     see is refused. Bind it: pick or file the node (fno backlog idea \
                     \"...\"), run fno do pr closure-trailer <id>, append the printed \
                     line to the PR body, then retry."
                        .to_string(),
                )),
                ..clean()
            };
            let outcome = run(&fake, &request(effect));
            assert_eq!(outcome.word(), "refused", "{effect:?}");
            assert!(outcome.detail().contains("PR 7 is unbound"), "{effect:?}");
            assert!(outcome.detail().contains("closure-trailer"), "{effect:?}");
            assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
        }
    }

    #[test]
    fn an_unreadable_binding_reads_unknown_not_bound() {
        // AC3-ERR: an instrument that cannot answer never becomes a verdict.
        // Bound still flows: every other test runs the default Clear fake,
        // which proves the gate reads the binding and refuses nothing else.
        for effect in [Effect::Merge, Effect::Arm] {
            let fake = Fake {
                node_binding: Some(ProbeOutcome::Inconclusive(
                    "graph unreadable; refusing to assume bound".to_string(),
                )),
                ..clean()
            };
            let outcome = run(&fake, &request(effect));
            assert_eq!(outcome.word(), "unknown", "{effect:?}");
            assert!(outcome.detail().contains("refusing to assume bound"));
            assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
        }
    }

    #[test]
    fn a_repo_with_no_node_under_it_is_never_refused() {
        // AC4-HP: the scope. A graph whose nodes all live in other repos has
        // nothing this PR could bind to, so the gate is silent there.
        let entries = vec![json!({"id": "x-bbbb", "cwd": "/other/repo", "project": "other"})];
        let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &open_facts());
        assert_eq!(outcome, ProbeOutcome::Clear);
    }

    #[test]
    fn an_unreadable_body_or_graph_refuses_to_assume_bound() {
        // AC4-ERR: a missing body key is an out-of-date deployed fno, never a
        // bound PR; the remedy names the update.
        let entries = vec![json!({"id": "x-bbbb", "cwd": "/this/repo", "project": "fno"})];
        let facts = PrFacts {
            body: None,
            ..open_facts()
        };
        let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &facts);
        let ProbeOutcome::Inconclusive(reason) = outcome else {
            unreachable!("a missing body is Inconclusive, never a verdict")
        };
        assert!(reason.contains("fno doctor update"));

        // A repo-scoped PR with no binding key at all is the refusal, with
        // the bind remedy.
        let facts = PrFacts {
            head_ref: "docs/crown-succeed-faq".to_string(),
            body: Some(String::new()),
            ..open_facts()
        };
        let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &facts);
        let ProbeOutcome::Refused(reason) = outcome else {
            unreachable!("three missing keys refuse")
        };
        assert!(reason.contains("closure-trailer"));
        assert!(reason.contains("no flag bypasses this gate"));

        // No comparable url leaves the backref key unevaluable: Unknown, not
        // a refusal on a PR that may be bound through the back-pointer.
        let facts = PrFacts {
            head_ref: "docs/crown-succeed-faq".to_string(),
            url: String::new(),
            body: Some(String::new()),
            ..open_facts()
        };
        let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &facts);
        let ProbeOutcome::Inconclusive(reason) = outcome else {
            unreachable!("an unscopeable backref key is Inconclusive")
        };
        assert!(reason.contains("could not be scoped"));
    }

    #[test]
    fn a_red_merge_result_holds_both_effects_and_calls_no_gh() {
        // 3334b826a133: two green parents merged into a red main. Held, not
        // refused - the remedy is rebase, fix, push, retry.
        for effect in [Effect::Merge, Effect::Arm] {
            let fake = Fake {
                merge_result: Some(ProbeOutcome::Refused(
                    "merge-result: REFUSED - cli/src/fno/graph/store.py:525:21: F821 Undefined name `_TAG_SHUTDOWN`"
                        .to_string(),
                )),
                ..clean()
            };
            let outcome = run(&fake, &request(effect));
            assert_eq!(outcome.word(), "held", "{effect:?}");
            assert!(outcome.detail().contains("red merge result"), "{effect:?}");
            assert!(outcome.detail().contains("F821"), "{effect:?}");
            assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
        }
    }

    #[test]
    fn an_inconclusive_merge_result_proceeds_with_a_breadcrumb() {
        // Fail-open, like the lineage probe: a deployed fno lacking the verb
        // or a gh hiccup must not make auto-merge silently never work.
        let fake = Fake {
            merge_result: Some(ProbeOutcome::Inconclusive(
                "exit 4: probe failed".to_string(),
            )),
            ..clean()
        };
        assert_eq!(run(&fake, &request(Effect::Merge)).word(), "merged");
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
                head: "abc123".to_string(),
                note: None,
                cleanup_failure: None,
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
    fn a_red_verdict_holds_on_the_durable_grant_lane_and_fails_interactive() {
        // Red CI is the state a working session is in while it pushes fixes.
        // Spending a retry on it parked six open PRs in one afternoon.
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        req.authority = Some("durable_grant".to_string());
        let red = Fake {
            checks: Some("red".to_string()),
            ..clean()
        };
        let held = run(&red, &req);
        assert_eq!(held.word(), "held");
        assert!(held
            .detail()
            .contains("the healer or the worker owns the next push"));
        assert!(red.gh_calls.borrow().is_empty());

        // The interactive lane keeps `failed`: the merge_status=failed stamp is
        // the worker's signal, and the existing tests above depend on it.
        let mut interactive = request(Effect::Merge);
        interactive.require_checks = true;
        interactive.authority = Some("manifest".to_string());
        assert_eq!(run(&red, &interactive).word(), "failed");
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
        assert_eq!(outcome.word(), "merged");
        // The cleanup failure rides its OWN field. A bare success would lose
        // it, and folding it into `note` would report the merge as partial.
        let Outcome::Merged {
            note,
            cleanup_failure,
            ..
        } = outcome
        else {
            unreachable!()
        };
        assert_eq!(note.as_deref(), Some("merged server-side"));
        assert!(cleanup_failure
            .expect("a cleanup failure")
            .contains("failed to delete local branch"));
    }

    #[test]
    fn a_worktree_recovery_that_worked_carries_no_cleanup_failure() {
        // The regression this pins: the recovery is how the merge landed, not
        // trouble around it. Rendered as a cleanup failure, every worktree-held
        // merge - which is every worktree-first run - reported partial.
        let fake = Fake {
            gh_ok: false,
            gh_output: "fatal: 'x' is already used by worktree at '/w'".to_string(),
            gh_recovery_ok: Some(true),
            ..clean()
        };
        let authorized = Authorized {
            facts: open_facts(),
            head: "abc123".to_string(),
            strategy: "merge".to_string(),
        };
        let outcome = effect(&fake, &request(Effect::Merge), &authorized);
        assert_eq!(
            outcome,
            Outcome::Merged {
                head: "abc123".to_string(),
                note: Some("merged server-side (worktree fallback)".to_string()),
                cleanup_failure: None,
            }
        );
    }

    #[test]
    fn a_checks_read_that_could_not_run_holds_instead_of_failing() {
        // `fno do pr status` answers `error` when the fetch is rate-limited or
        // the network is down. Failing on it stamps the node merge status
        // failed for a read that never described the checks at all.
        for verdict in ["error", "pending", "unknown"] {
            let fake = Fake {
                checks: Some(verdict.to_string()),
                ..clean()
            };
            let mut req = request(Effect::Merge);
            req.require_checks = true;
            assert_eq!(run(&fake, &req).word(), "held", "verdict {verdict}");
        }
        let red = Fake {
            checks: Some("red".to_string()),
            ..clean()
        };
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        assert_eq!(run(&red, &req).word(), "failed");
    }

    #[test]
    fn a_ruleset_hold_holds_and_names_the_missing_context() {
        // The door fetched the hold's own name moments before `gh pr merge`
        // and used to spend a failure on it. Held, not Failed: a required
        // check that is merely pending still arrives.
        let fake = Fake {
            checks: Some("green".to_string()),
            github_block: Some("smoke".to_string()),
            ..clean()
        };
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        let outcome = run(&fake, &req);
        assert_eq!(outcome.word(), "held");
        assert!(outcome.detail().contains("smoke"));
        assert!(fake.gh_calls.borrow().is_empty());
    }

    #[test]
    fn a_ruleset_hold_outranks_the_require_checks_flag() {
        // A ruleset hold is not a question about CI greenness. A door gated on
        // the flag would attempt the bypass its own reader just refused.
        let fake = Fake {
            github_block: Some("stacked-base-guard".to_string()),
            ..clean()
        };
        let req = request(Effect::Merge);
        assert_eq!(req.require_checks, false);
        let outcome = run(&fake, &req);
        assert_eq!(outcome.word(), "held");
        assert!(outcome.detail().contains("stacked-base-guard"));
    }

    #[test]
    fn an_arm_effect_proceeds_past_a_ruleset_hold_to_the_queue() {
        // Arm hands the PR to GitHub's own queue, which waits out a missing
        // requirement by design; the hold must not stand in its way.
        let fake = Fake {
            github_block: Some("smoke".to_string()),
            ..clean()
        };
        let mut req = request(Effect::Arm);
        req.decide_only = true;
        assert_eq!(
            run(&fake, &req),
            Outcome::Authorized {
                head: "abc123".to_string()
            }
        );
    }

    #[test]
    fn a_green_read_without_a_github_block_authorizes_as_before() {
        let fake = clean();
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        req.decide_only = true;
        assert_eq!(
            run(&fake, &req),
            Outcome::Authorized {
                head: "abc123".to_string()
            }
        );
    }

    #[test]
    fn an_unparseable_status_read_claims_no_github_block() {
        let read = parse_checks_read(b"error: rate limited");
        assert_eq!(read.verdict, "unknown");
        assert_eq!(read.github_block, None);
    }

    #[test]
    fn a_github_block_with_null_missing_falls_back_to_the_source() {
        // merge_blocker answers `missing_required_checks: null` with a source
        // line saying the block stands, so the reason must carry that line.
        let payload = r#"{"verdict": "green", "ready_blockers": ["github_blocked"], "github_merge_state": {"state": "blocked", "blockers": ["github_blocked"], "missing_required_checks": null, "source": "the rules read failed or names no unsatisfied rule; the block stands"}}"#;
        let read = parse_checks_read(payload.as_bytes());
        assert_eq!(read.verdict, "green");
        assert_eq!(
            read.github_block.as_deref(),
            Some("the rules read failed or names no unsatisfied rule; the block stands")
        );
    }

    #[test]
    fn the_parser_consumes_a_real_status_payload_verbatim() {
        // Captured verbatim from a live `fno do pr status` read, 2026-09-19.
        let payload = r#"{"pr": "2251", "head": "d38a744c36b97c65df67df7f4245f6704adfa494", "verdict": "green", "settled": true, "green": true, "pr_state": "MERGED", "mergeable": "UNKNOWN", "github_merge_state": null, "checks": {"total": 37, "check_runs": 36, "statuses": 1, "fail_check_runs": 0, "fail_statuses": 0, "pass": 37, "fail": 0, "pending": 0, "unsettled": 0, "unsettled_fail": 0}, "optional_reviews": [], "optional_reviews_unresolved": 0, "optional_reviews_resolved_unchanged": 0, "review_coverage": {"coverage": "not_asked", "reviewed_count": 0, "self_attested_count": 0, "head_sha": null, "stale_verdicts": [], "note": "not asked: PR is terminal (merged or closed); this says nothing about coverage at merge time"}, "review_posture": null, "merge_authority": {"auto_merge_enabled": true, "grant": "dispatch", "mergeable_autonomously": true}, "merge_execution": null, "rounds_used": null, "max_rounds": null, "rounds_exhausted": null, "rounds_note": "no review_coverage row at this head; run fno-agents review-coverage", "review_activity": {"blocker": "", "detail": "", "hold": null, "worktree": {"probed": false, "path": null, "dirty": null, "head": null, "note": "not asked: PR is terminal"}}, "dispatch_hold": null, "ready": true, "ready_blockers": []}"#;
        let read = parse_checks_read(payload.as_bytes());
        assert_eq!(read.verdict, "green");
        assert_eq!(read.github_block, None);
    }

    #[test]
    fn a_multibyte_error_line_is_truncated_without_panicking() {
        // A byte slice at 200 panics when the cut lands inside a character.
        let line = "e".repeat(198) + &"é".repeat(20);
        let cut = first_line(&line);
        assert_eq!(cut.chars().count(), 200);
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

    fn preview_request(pr: u64) -> Request {
        Request {
            pr: Some(pr),
            effect: Effect::Preview,
            require_checks: true,
            ..request(Effect::Preview)
        }
    }

    fn preview_blockers(fake: &Fake, req: &Request) -> Vec<Blocker> {
        let facts = fake.facts.clone().unwrap_or_else(open_facts);
        match preview_walk(fake, req, &facts) {
            PreviewVerdict::Go { .. } => Vec::new(),
            PreviewVerdict::Blocked(rows) => rows,
        }
    }

    #[test]
    fn a_stale_pr_behind_a_held_slot_previews_both_blockers_without_taking_the_slot() {
        // AC1.
        let fake = Fake {
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 3 behind".to_string())),
            slot: RefCell::new(Some(7)),
            other_facts: RefCell::new({
                let mut map = HashMap::new();
                map.insert(
                    7,
                    PrFacts {
                        number: 7,
                        state: "OPEN".to_string(),
                        ..open_facts()
                    },
                );
                map
            }),
            ..clean()
        };
        let req = Request {
            pr: Some(8),
            covered_head: Some("abc123".to_string()),
            ..preview_request(8)
        };
        // The Fake pins covered_head to its own facts head, so pr 8 with the
        // default facts reads a moved head; give pr 8 its own facts.
        let mut fake = fake;
        fake.facts = Some(PrFacts {
            number: 8,
            head_sha: "def456".to_string(),
            ..open_facts()
        });
        fake.covered_head = Some("def456".to_string());
        let blockers = preview_blockers(&fake, &req);
        let codes: Vec<&str> = blockers.iter().map(|b| b.code.as_str()).collect();
        assert!(codes.contains(&"ci_base_stale"), "{codes:?}");
        assert!(codes.contains(&"merge_slot_held"), "{codes:?}");
        let slot_blocker = blockers
            .iter()
            .find(|b| b.code == "merge_slot_held")
            .expect("slot blocker");
        assert!(slot_blocker.detail.contains("PR 7"), "{slot_blocker:?}");
        assert!(fake.take_slot_calls.borrow().is_empty());
        assert!(fake.release_slot_calls.borrow().is_empty());
    }

    #[test]
    fn a_terminal_pr_previews_exactly_pr_terminal() {
        // AC3.
        let fake = Fake {
            facts: Some(PrFacts {
                state: "MERGED".to_string(),
                ..open_facts()
            }),
            ..clean()
        };
        let blockers = preview_blockers(&fake, &preview_request(7));
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert_eq!(blockers[0].code, "pr_terminal");
    }

    #[test]
    fn merge_and_preview_name_the_same_first_blocker_for_every_gate() {
        // AC2: for each gate made to refuse in turn, the Merge outcome's
        // detail and the Preview's first blocker are the SAME answer - one
        // decision, two collectors.
        let base = || Fake { ..clean() };
        let scenarios: Vec<(&str, Fake)> = vec![
            (
                "pr_terminal",
                Fake {
                    facts: Some(PrFacts {
                        state: "CLOSED".to_string(),
                        ..open_facts()
                    }),
                    ..base()
                },
            ),
            (
                "node_unbound",
                Fake {
                    node_binding: Some(ProbeOutcome::Refused(
                        "unbound: no node names this PR".to_string(),
                    )),
                    ..base()
                },
            ),
            (
                "dispatch_hold",
                Fake {
                    dispatch_hold: Some(ProbeOutcome::Refused(
                        "dispatch_hold: held by tgt-x".to_string(),
                    )),
                    ..base()
                },
            ),
            (
                "review_in_flight",
                Fake {
                    review_hold: Some(ProbeOutcome::Refused(
                        "review_in_flight: held by tgt-x at abc123".to_string(),
                    )),
                    ..base()
                },
            ),
            (
                "red_merge_result",
                Fake {
                    merge_result: Some(ProbeOutcome::Refused(
                        "F821 in the merged tree".to_string(),
                    )),
                    ..base()
                },
            ),
            (
                "stacked_base",
                Fake {
                    lineage: Some(ProbeOutcome::Refused(
                        "base no longer reaches the default branch".to_string(),
                    )),
                    ..base()
                },
            ),
            (
                "github_blocked",
                Fake {
                    github_block: Some("smoke".to_string()),
                    ..base()
                },
            ),
            (
                "review_coverage_uncovered",
                Fake {
                    coverage_exit: Some(3),
                    ..base()
                },
            ),
            (
                "optional_reviews_unresolved",
                Fake {
                    optional_unresolved: Some(Some(2)),
                    ..base()
                },
            ),
            (
                "ci_pending",
                Fake {
                    checks: Some("pending".to_string()),
                    ..base()
                },
            ),
        ];
        for (code, fake) in scenarios {
            let req_merge = Request {
                require_checks: true,
                ..request(Effect::Merge)
            };
            let outcome = run(&fake, &req_merge);
            let blockers = preview_blockers(&fake, &preview_request(7));
            assert!(
                !blockers.is_empty(),
                "{code}: preview cleared while merge refused ({outcome:?})"
            );
            let first = blockers[0].code.clone();
            assert_eq!(
                outcome.detail(),
                blockers[0].detail,
                "{code}: merge detail and the preview's first blocker ({first}) disagree"
            );
        }
    }

    #[test]
    fn a_cleared_preview_runs_no_effect_and_takes_no_slot() {
        let fake = Fake {
            fresh_ci: Some(false),
            ..clean()
        };
        let outcome = run(&fake, &preview_request(7));
        assert_eq!(outcome.word(), "authorized");
        assert!(fake.take_slot_calls.borrow().is_empty());
        assert!(fake.release_slot_calls.borrow().is_empty());
        assert!(fake.gh_calls.borrow().is_empty());
    }

    #[test]
    fn the_red_kind_word_splits_by_the_counts() {
        let counts = |uf: i64, f: i64, fs: i64| serde_json::json!({"unsettled_fail": uf, "fail": f, "fail_statuses": fs});
        assert_eq!(
            ci_blocker_word("red", Some(&counts(1, 1, 0))),
            "ci_cancelled_retrigger"
        );
        assert_eq!(
            ci_blocker_word("red", Some(&counts(0, 2, 2))),
            "commit_status_red"
        );
        assert_eq!(ci_blocker_word("red", Some(&counts(1, 3, 1))), "ci_red");
        assert_eq!(ci_blocker_word("red", None), "ci_red");
        assert_eq!(ci_blocker_word("pending", None), "ci_pending");
    }

    #[test]
    fn a_preview_payload_defaults_its_ci_gate_on() {
        let payload: Value =
            serde_json::from_str(r#"{"cwd": "/tmp", "effect": "preview", "pr": 7}"#).unwrap();
        let request = parse_request(&payload).unwrap();
        assert_eq!(request.effect, Effect::Preview);
        assert!(request.require_checks);
        let merge_payload: Value =
            serde_json::from_str(r#"{"cwd": "/tmp", "effect": "merge", "pr": 7}"#).unwrap();
        assert!(!parse_request(&merge_payload).unwrap().require_checks);
    }

    #[test]
    fn live_lanes_counts_live_lane_claims_at_the_canonical_root() {
        // The overlap hold arms on this count; the trait default 0 would
        // leave it dead in production. The claim is planted through the same
        // acquire API the lane runtime uses, in a fresh git repo, so the
        // canonical-root resolution and the claims scan both run for real.
        let base = std::env::temp_dir().join(format!("x53c5-lanes-{}", std::process::id()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init failed for the lane-count fixture"
        );
        let opts = crate::claims::AcquireOpts {
            pid: Some(std::process::id()),
            ttl_ms: Some(60_000),
            root: Some(repo.clone()),
            ..Default::default()
        };
        let claimed = matches!(
            crate::claims::acquire("lane-slot:0", "parallel-lane:live-lanes-test", opts),
            crate::claims::AcquireOutcome::Acquired(_)
        );
        assert!(claimed, "the planted lane claim must acquire");
        assert_eq!(
            RealProbes.live_lanes(&repo),
            1,
            "the live lane claim must count"
        );
        let _ = crate::claims::release(
            "lane-slot:0",
            "parallel-lane:live-lanes-test",
            Some(repo.as_path()),
            None,
        );
        assert_eq!(
            RealProbes.live_lanes(&repo),
            0,
            "released lane must not count"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn covered_head_reads_a_store_committed_coverage_row() {
        // AC5-HP: a covered row at HEAD committed to the store only.
        let _root = crate::paths::DeclaredRoot::declare("am_covered_head_store");
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output();
        // No git repo in the temp dir: HEAD is empty, so the scan accepts any
        // head. A store-only covered row still answers.
        let _ = head;
        let line = serde_json::json!({
            "ts": "2026-09-17T12:00:00Z", "type": "review_coverage", "source": "review",
            "data": {"head_sha": "aaaaaaaaaa", "coverage": "covered", "reviewed_count": 2}
        })
        .to_string();
        let events = crate::paths::events_path(cwd);
        crate::event_store::append_envelope(&events, &line, None).unwrap();
        let covered = covered_head_from_event(cwd);
        assert_eq!(covered.as_deref(), Some("aaaaaaaaaa"), "{covered:?}");
    }
}
