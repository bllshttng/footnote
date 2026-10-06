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
use crate::main_ci::main_ci_red_run;
use crate::org_board::prs::{pr_binding_keys, retarget_binding_refusal};
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
        /// Named on the receipt when a head-scoped operator grant cleared the
        /// per-run no-merge layer (`merge_grant::HeadGrant::Granted` was the
        /// only door through `authority_refusal`).
        merge_grant: Option<String>,
    },
    Armed {
        head: String,
        merge_grant: Option<String>,
    },
    /// The decision cleared and the caller asked to stop there. Nothing ran.
    Authorized { head: String },
    /// Retryable. The same command later can succeed.
    Held { reason: String },
    /// Needs an operator action. Retrying changes nothing.
    Refused { reason: String },
    /// The head moved between validation and the effect.
    HeadChanged { expected: String, actual: String },
    /// An instrument could not answer. Never a verdict.
    Unknown { reason: String },
    /// The effect ran and failed.
    Failed { reason: String },
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
            Outcome::Armed { head, .. } | Outcome::Authorized { head } => head.clone(),
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
                merge_grant,
            } => {
                out["head"] = json!(head);
                if let Some(note) = note {
                    out["note"] = json!(note);
                }
                if let Some(cleanup_failure) = cleanup_failure {
                    out["cleanup_failure"] = json!(cleanup_failure);
                }
                if let Some(g) = merge_grant {
                    out["merge_grant"] = json!(g);
                }
            }
            Outcome::Armed { head, merge_grant } => {
                out["head"] = json!(head);
                if let Some(g) = merge_grant {
                    out["merge_grant"] = json!(g);
                }
            }
            Outcome::Authorized { head } => {
                out["head"] = json!(head);
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
    /// The dispatch-hold answer the caller (the status read) already probed.
    /// `Some(None)` = probed clear; `Some(Some(reason))` = held; `None` = not
    /// supplied (the walk probes, and a real merge NEVER sees a supplied
    /// value: only the preview walk reads this, decide's own chain always
    /// probes live).
    pub supplied_dispatch_hold: Option<Option<String>>,
    /// The review-hold answer the caller already probed (`review_activity`
    /// reads the same registry the `review-hold check` verb does), as a
    /// refusal sentence in `review_hold_refusal`'s own shape. `Some(None)` =
    /// probed clear; `None` = not supplied (probe). Preview-only, same law as
    /// the dispatch-hold answer above.
    pub supplied_review_hold: Option<Option<String>>,
    /// The caller's own PR read (the status payload's projection), parsed by
    /// [`facts_from_pulls`] in place of the `fno do pr info` spawn. A payload
    /// too old to parse stays a fallback to the spawn, never a wrong fact.
    /// Preview-only.
    pub supplied_facts: Option<Value>,
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
    fn dispatch_hold(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome;
    fn review_hold(&self, cwd: &Path, pr: u64) -> ProbeOutcome;
    fn base_lineage(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome;
    /// Compile the merge result (merge-tree + the repo-wide static step).
    fn merge_result(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome;
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
    /// The base branch `main`'s CI verdict, read by the lead check-in's own
    /// reduction (`main_ci::main_ci_reading`, behind a short TTL row cache):
    /// a red object naming the failed workflow and head sha, or the
    /// `green`/`pending` word. `Err` = the read could not answer, which never
    /// reads as red. Default `pending` keeps the gate disarmed wherever an
    /// impl does not read main.
    fn main_ci_token(&self, _cwd: &Path) -> Result<Value, String> {
        Ok(Value::String("pending".to_string()))
    }
    /// The main-repair exemption: `None` when this PR's node carries the
    /// `main-repair` tag (the declared repair lands through a red main);
    /// `Some(lane)` otherwise, the repair-lane sentence the refusal quotes.
    /// Default `None` = exempt, so an impl that does not read the graph never
    /// manufactures a refusal.
    fn main_repair_hold(&self, _cwd: &Path, _facts: &PrFacts) -> Option<String> {
        None
    }
    /// The outside-PR gate: fleet automation never acts on outside code, and
    /// no admit door exists. Default `Clear` keeps an impl that does not read
    /// origin facts inert.
    fn outside_pr(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
        ProbeOutcome::Clear
    }
    /// The owner's per-PR hold, an operator law row at `pr-hold:<slug>#<n>`.
    /// Default `Clear` keeps an impl that does not read law rows inert.
    fn pr_hold(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
        ProbeOutcome::Clear
    }
}

/// A cleared decision: the effect may run, pinned to this head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorized {
    pub facts: PrFacts,
    pub head: String,
    pub strategy: String,
    /// Named when a head-scoped operator grant cleared the per-run no-merge
    /// layer of `authority_refusal`. Reaches the effect receipt verbatim.
    pub merge_grant: Option<String>,
}

/// The whole authorization. Order matters: the refusals that need an operator
/// come first, then the holds, then the head pin, then the effect's own
/// preconditions.
pub fn decide<P: Probes>(probes: &P, request: &Request) -> Result<Authorized, Outcome> {
    decide_observed(probes, request, &mut None)
}

fn decide_observed<P: Probes>(
    probes: &P,
    request: &Request,
    observed_head: &mut Option<String>,
) -> Result<Authorized, Outcome> {
    let cwd = request.cwd.as_path();
    let facts = probes
        .pr_facts(cwd, request.pr)
        .map_err(|reason| Outcome::Unknown { reason })?;
    *observed_head = Some(facts.head_sha.clone());

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
                    // Preview never runs an effect, so a receipt grant note
                    // would name nothing; the merge receipt is the one that
                    // must carry it.
                    merge_grant: None,
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

    // The outside-PR gate: outside is final. An unreadable origin read is
    // Unknown (fail closed), never a quiet Clear.
    match probes.outside_pr(cwd, &facts) {
        ProbeOutcome::Clear => {}
        ProbeOutcome::Refused(reason) => return Err(Outcome::Refused { reason }),
        ProbeOutcome::Inconclusive(reason) => return Err(Outcome::Unknown { reason }),
    }

    if let Some((_, reason)) = authority_refusal(probes, cwd, request, &facts) {
        return Err(Outcome::Refused { reason });
    }

    // The graph must see the PR. An unbound PR is Refused (retrying without
    // binding changes nothing, so Held would be a lie about the remedy); an
    // unreadable graph is Unknown, never a bound-or-unbound verdict.
    match probes.node_binding(cwd, &facts) {
        ProbeOutcome::Clear => {}
        ProbeOutcome::Refused(reason) => return Err(Outcome::Refused { reason }),
        ProbeOutcome::Inconclusive(reason) => return Err(Outcome::Unknown { reason }),
    }

    // The hold-while-red rule, mechanized: while main's latest settled run is
    // red, only a PR whose node is the declared main repair may merge. The
    // lead check-in's own reduction answers, never a check count; a pending
    // main is not red, and an unreadable verdict never manufactures a red
    // (only a POSITIVE red holds, the checks gate's law).
    if facts.base_ref == "main" {
        let token = probes.main_ci_token(cwd);
        if let Some((workflow, sha)) = token.as_ref().ok().and_then(main_ci_red_run) {
            if let Some(lane) = probes.main_repair_hold(cwd, &facts) {
                return Err(Outcome::Refused {
                    reason: main_red_reason(&workflow, &sha, &lane),
                });
            }
        }
    }

    if let Some(blocked) = probes.dispatch_hold(cwd, &facts).fail_closed() {
        return Err(Outcome::Held { reason: blocked });
    }

    // The owner's per-PR hold answers beside the dispatch hold: held, not
    // refused, because the owner releasing it clears it.
    if let Some(blocked) = probes.pr_hold(cwd, &facts).fail_closed() {
        return Err(Outcome::Held { reason: blocked });
    }

    // The in-flight review hold. Coverage answers what verdicts EXIST for a
    // head; it cannot say that a review is executing right now with its findings
    // uncommitted. The arm path never read this before, so a queue armed at the
    // terminal shipped the code a review was still fixing.
    if let Some(blocked) = probes.review_hold(cwd, facts.number).fail_closed() {
        return Err(Outcome::Held { reason: blocked });
    }

    // The user's look: the same gate the preview carries, held so the user
    // answering the page clears it on the next read.
    if let Some(blocker) =
        crate::merge_gates::visual_approval_blocker(probes, cwd, facts.number, &facts.head_sha)
    {
        return Err(Outcome::Held {
            reason: blocker.detail,
        });
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

    if let Some(reason) = probes.base_lineage(cwd, &facts).fail_open() {
        return Err(Outcome::Refused {
            reason: format!("stale base: {reason}"),
        });
    }

    // Two green parents can merge red: git joins hunks that never met on one
    // machine (3334b826a133 broke main with F821 out of a clean textual
    // merge). Held, not refused: the remedy is rebase, fix, push, retry.
    if let Some(reason) = probes.merge_result(cwd, &facts).fail_open() {
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
                // Arm is gated too: without a ruleset that demands an
                // up-to-date branch, GitHub merges an armed green PR at once.
                if request.effect != Effect::Preview && probes.require_fresh_ci(cwd) {
                    let stale = match probes.ci_base(cwd, &facts) {
                        ProbeOutcome::Inconclusive(reason) => {
                            return Err(Outcome::Unknown {
                                reason: format!(
                                    "{reason}; a merge needs proof its CI tested the current \
                                     {base}",
                                    base = facts.base_ref
                                ),
                            })
                        }
                        outcome => outcome.fail_open(),
                    };
                    let n = facts.number;
                    match probes.slot_holder(cwd, &facts.base_ref) {
                        // A claims io fault never blocks merges: fall back to
                        // the pre-slot behavior and take no slot.
                        Err(_) => {
                            if let Some(reason) = stale {
                                return Err(Outcome::Held {
                                    reason: format!(
                                        "{reason}; {}",
                                        retest_on_current_base(probes, cwd, &facts)
                                    ),
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
                                    let stale_holder = match probes.pr_facts(cwd, Some(m)) {
                                        Ok(holder_facts) => {
                                            probes
                                                .dispatch_hold(cwd, &holder_facts)
                                                .fail_open()
                                                .is_some()
                                                || is_terminal_state(&holder_facts.state)
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
                                                retest_on_current_base(probes, cwd, &facts)
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
                                                        remedy = retest_on_current_base(
                                                            probes, cwd, &facts
                                                        )
                                                    ),
                                                });
                                            }
                                            Err(_) => {
                                                return Err(Outcome::Held {
                                                    reason: format!(
                                                        "{reason}; {}",
                                                        stale_remedy(n, &facts.base_ref)
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
    // Reaching here with `approved == Some(false)` means the only door through
    // `authority_refusal` was a Granted head grant, so the receipt names it.
    let merge_grant = (request.approved == Some(false)).then(|| {
        format!(
            "operator head grant {}",
            facts.head_sha.chars().take(8).collect::<String>()
        )
    });
    Ok(Authorized {
        facts,
        head,
        strategy,
        merge_grant,
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

    // (2) authority: per-run refusal, live config, posture floor. The code
    // comes from authority_refusal itself, so a preview can never label a
    // blocker differently from the merge verb on the same head.
    if let Some((code, reason)) = authority_refusal(probes, cwd, request, facts) {
        blockers.push(match code {
            "auto_merge_disabled" => Blocker::refused("auto_merge_disabled", reason),
            "posture_floor" => Blocker::refused("posture_floor", reason),
            _ => Blocker::refused("per_run_no_merge", reason),
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

    // (3b) main red: the same refusal the effect path raises, as a blocker.
    if facts.base_ref == "main" {
        let token = probes.main_ci_token(cwd);
        if let Some((workflow, sha)) = token.as_ref().ok().and_then(main_ci_red_run) {
            if let Some(lane) = probes.main_repair_hold(cwd, facts) {
                blockers.push(Blocker::refused(
                    "main_red",
                    main_red_reason(&workflow, &sha, &lane),
                ));
            }
        }
    }

    // (3c) the outside-PR gate, the same refusal the effect path raises, so
    // `fno do pr status` never says ready where merge refuses.
    match probes.outside_pr(cwd, facts) {
        ProbeOutcome::Clear => {}
        ProbeOutcome::Refused(reason) => {
            blockers.push(Blocker::refused("outside_pr", reason));
        }
        ProbeOutcome::Inconclusive(reason) => {
            blockers.push(Blocker::unknown("outside_pr_unreadable", reason));
        }
    }

    // (4) holds, in decide's order. A preview ask may carry the dispatch-hold
    // answer its caller already probed; the supplied value rides only the
    // preview (advisory) walk, never decide's own merge chain.
    let dispatch_hold_outcome = match &request.supplied_dispatch_hold {
        Some(None) => ProbeOutcome::Clear,
        Some(Some(reason)) => ProbeOutcome::Refused(reason.clone()),
        None => probes.dispatch_hold(cwd, facts),
    };
    if let Some(reason) = dispatch_hold_outcome.fail_closed() {
        blockers.push(Blocker::held("dispatch_hold", reason));
    }
    // The supplied review-hold answer rides the preview exactly like the
    // dispatch-hold answer above: a probed clear is `Some(None)`, decide's own
    // chain never sees a supplied value.
    let review_hold_outcome = match &request.supplied_review_hold {
        Some(None) => ProbeOutcome::Clear,
        Some(Some(reason)) => ProbeOutcome::Refused(reason.clone()),
        None => probes.review_hold(cwd, facts.number),
    };
    if let Some(reason) = review_hold_outcome.fail_closed() {
        blockers.push(Blocker::held("review_in_flight", reason));
    }
    if let Some(reason) = probes.pr_hold(cwd, facts).fail_closed() {
        blockers.push(Blocker::held("pr_hold", reason));
    }

    // (4b) the user's look: a PR touching the configured paint surface holds
    // until an answered question page names it or a crown decision row
    // attests the user's chat approval of this head. Held, not refused: the
    // user answering the page clears it on the next read.
    if let Some(blocker) =
        crate::merge_gates::visual_approval_blocker(probes, cwd, facts.number, &facts.head_sha)
    {
        blockers.push(blocker);
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
    if let Some(reason) = probes.base_lineage(cwd, &facts).fail_open() {
        blockers.push(Blocker::refused(
            "stacked_base",
            format!("stale base: {reason}"),
        ));
    }
    if let Some(reason) = probes.merge_result(cwd, &facts).fail_open() {
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
        let n = facts.number;
        match probes.ci_base(cwd, facts) {
            ProbeOutcome::Refused(reason) => blockers.push(Blocker::held(
                "ci_base_stale",
                format!("{reason}; {}", stale_remedy(n, &facts.base_ref)),
            )),
            ProbeOutcome::Inconclusive(reason) => {
                blockers.push(Blocker::unknown("ci_base_unreadable", reason))
            }
            ProbeOutcome::Clear => {}
        }
        match probes.slot_holder(cwd, &facts.base_ref) {
            Err(_) => {}
            Ok(Some(m)) if m != n => {
                let evictable = match probes.pr_facts(cwd, Some(m)) {
                    Ok(hf) => {
                        probes.dispatch_hold(cwd, &hf).fail_open().is_some()
                            || is_terminal_state(&hf.state)
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
    let graph_path = crate::org_board::scope::graph_json_path(cwd);
    let store = GraphStore::new(&graph_path);
    backlog_api::rows(&store).ok()
}

/// The repo top-level for manifest lookups: manifests live at the project
/// root's `.fno/`, never under a subdirectory cwd.
fn repo_root(cwd: &Path) -> PathBuf {
    canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

/// The authority fold, in the order init folds it. A per-run refusal outranks
/// every grant - EXCEPT the head-scoped operator grant, the one sanctioned
/// remedy for a per-run no-merge, recorded out-of-band by a person at a
/// terminal through `fno backlog decide --authority operator` and never by a
/// session (`decide/__init__.py:297` refuses operator authority from agent
/// sessions). The grant clears only the per-run layer; the live-config and
/// posture-floor arms still run, and a push to a new head invalidates the
/// grant by subject construction.
fn authority_refusal<P: Probes>(
    probes: &P,
    cwd: &Path,
    request: &Request,
    facts: &PrFacts,
) -> Option<(&'static str, String)> {
    // The PR's own bound target manifest folds first when the caller carried
    // no posture: a status read never read it, and the merge verb's
    // spaces-aware read arrives here as `approved` when it did. A manifest
    // false is a per-run refusal like any caller-supplied one; a present but
    // unreadable manifest at the PR's branch fails closed. The exact-head
    // operator grant below stays the one sanctioned remedy.
    let mut approved = request.approved;
    let mut folded_source = request.auto_merge_source.clone();
    if approved.is_none() {
        match crate::merge_grant::branch_bound_manifest(cwd, &facts.head_ref) {
            crate::merge_grant::BoundRead::Read(bound) => {
                approved = bound.approved;
                folded_source = bound.source;
            }
            crate::merge_grant::BoundRead::Unreadable(why) => {
                return Some((
                    "manifest_unreadable",
                    format!(
                        "the PR's bound target manifest is present but unreadable ({why}); \
                         refusing without a readable posture"
                    ),
                ));
            }
            crate::merge_grant::BoundRead::None => {}
        }
    }
    let source = folded_source
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown (pre-provenance manifest)")
        .to_string();
    if approved == Some(false) {
        let slug = crate::finalize::slug_from_git_remote(&repo_root(cwd)).unwrap_or_default();
        let subject =
            crate::merge_grant::head_grant_subject(&slug, facts.number as i64, &facts.head_sha);
        let args: Vec<String> = [
            "backlog",
            "decisions",
            subject.as_str(),
            "--lane",
            "law",
            "--state",
            "live",
            "--json",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let stdout = probes
            .fno_shell(cwd, &args)
            .ok()
            .and_then(|(code, out, _)| (code == Some(0)).then_some(out));
        match crate::merge_grant::head_grant_status(stdout.as_deref()) {
            crate::merge_grant::HeadGrant::Granted => {}
            other => {
                let (state_word, detail) = match &other {
                    crate::merge_grant::HeadGrant::Absent => (
                        "absent",
                        "no operator row records a grant at this head".to_string(),
                    ),
                    crate::merge_grant::HeadGrant::Conflict => (
                        "conflicting",
                        "operator rows at this head disagree or carry no decision".to_string(),
                    ),
                    crate::merge_grant::HeadGrant::Unreadable(d) => ("unreadable", d.clone()),
                    crate::merge_grant::HeadGrant::Granted => unreachable!("matched above"),
                };
                return Some((
                    "per_run_no_merge",
                    format!(
                        "per-run no-merge (manifest auto_merge_approved is not true; \
                         auto_merge_source: {source}); the operator head grant reads \
                         {state_word} ({detail}); sanctioned override: {}",
                        crate::merge_grant::attended_grant_command(
                            &slug,
                            facts.number as i64,
                            &facts.head_sha
                        )
                    ),
                ));
            }
        }
    }
    let env_grant = approved == Some(true) && source == "env-target-auto-merge";
    if !env_grant && !probes.auto_merge_enabled(cwd) {
        return Some((
            "auto_merge_disabled",
            "auto_merge disabled (live config resolves auto_merge.enabled=false); sanctioned \
             override (operator levers): `fno config set auto_merge.enabled true`, or start the \
             run with TARGET_AUTO_MERGE=1 from the operator's shell"
                .to_string(),
        ));
    }
    probes
        .posture_floor_block(cwd)
        .map(|reason| ("posture_floor", reason))
}

/// Decide, then run the effect unless the caller asked to stop at the decision.
pub fn run<P: Probes>(probes: &P, request: &Request) -> Outcome {
    run_observed(probes, request, &mut None)
}

fn run_observed<P: Probes>(
    probes: &P,
    request: &Request,
    observed_head: &mut Option<String>,
) -> Outcome {
    match decide_observed(probes, request, observed_head) {
        Ok(authorized) if request.decide_only || request.effect == Effect::Preview => {
            Outcome::Authorized {
                head: authorized.head,
            }
        }
        Ok(authorized) => {
            let outcome = effect(probes, request, &authorized);
            crate::merge_provenance::record_outcome(request, &authorized.facts, &outcome);
            // Only a Merge hands the slot back. Every terminal effect() outcome of a Merge
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
                merge_grant: authorized.merge_grant.clone(),
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
                merge_grant: authorized.merge_grant.clone(),
            },
            Effect::Merge => Outcome::Merged {
                head: authorized.head.clone(),
                note: None,
                cleanup_failure: None,
                merge_grant: authorized.merge_grant.clone(),
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
            merge_grant: authorized.merge_grant.clone(),
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
                        merge_grant: authorized.merge_grant.clone(),
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

/// The fno gh proxy prefixes its own stderr with `fno config:` when it warns
/// about an unmodeled config key. That text is never gh's verdict.
fn is_config_warning(line: &str) -> bool {
    line.trim_start().starts_with("fno config:")
}

/// The first non-blank line of a command's output, capped for a receipt.
/// Leading `fno config:` warning lines are skipped so the reason names the
/// real gh error; a stderr of only warnings still reports its first line.
fn first_line(output: &str) -> String {
    let line = output
        .lines()
        .find(|line| !line.trim().is_empty() && !is_config_warning(line))
        .or_else(|| output.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("no error output");
    // Truncate by CHARACTER. A byte slice panics when the cut lands inside a
    // multi-byte character, and gh output carries them (a PR title, a branch
    // name, a localized git message).
    line.chars().take(200).collect()
}

fn classify_failure(effect: Effect, strategy: &str, output: &str) -> Outcome {
    let lower = output.to_lowercase();
    if lower.contains("secondary rate limit") {
        // A burst (a fleet undrafting many PRs at once) trips GitHub's
        // secondary limiter. The same command succeeds after the backoff, so
        // it holds like any other retryable state - never a merge-method
        // fault.
        return Outcome::Held {
            reason: format!(
                "GitHub secondary rate limit; wait out the backoff, then retry the {}: {}",
                effect.word(),
                first_line(output)
            ),
        };
    }
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
        let out = Command::new(crate::scrape::fno_bin())
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|error| error.to_string())?;
        Ok((out.status.code(), out.stdout, out.stderr))
    }

    fn fno_strings(cwd: &Path, args: &[String]) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String> {
        Self::fno(cwd, &args.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

impl Probes for RealProbes {
    fn pr_facts(&self, cwd: &Path, pr: Option<u64>) -> Result<PrFacts, String> {
        head_probe::read(cwd, pr)
    }

    fn node_binding(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        node_binding_probe(cwd, facts)
    }

    fn main_ci_token(&self, cwd: &Path) -> Result<Value, String> {
        crate::main_ci::main_ci_reading_cached(cwd)
    }

    fn main_repair_hold(&self, cwd: &Path, facts: &PrFacts) -> Option<String> {
        main_repair_hold_probe(cwd, facts)
    }

    fn outside_pr(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        crate::pr_admission::outside_pr(cwd, facts)
    }

    fn pr_hold(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        crate::pr_admission::pr_hold(cwd, facts)
    }

    fn dispatch_hold(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        crate::gate_probes::dispatch_hold(cwd, facts)
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

    fn base_lineage(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        let run = |args: &[&str], dir: &Path| {
            let mut cmd = Command::new(args[0]);
            cmd.args(&args[1..]).current_dir(dir);
            crate::gate_probes::run_probe(cmd)
        };
        crate::gate_probes::base_lineage(cwd, facts, &run)
    }

    fn merge_result(&self, cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        let run = |args: &[&str], dir: &Path| {
            let mut cmd = Command::new(args[0]);
            cmd.args(&args[1..]).current_dir(dir);
            crate::gate_probes::run_probe(cmd)
        };
        crate::gate_probes::merge_result(cwd, facts, &run)
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
        // No disjoint-files waiver: two PRs that share no file still break
        // main together when one's test reads what the other moved.
        ci_base_verdict(compare)
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
        // One owner: the CI verdict answers in process through the status
        // door. A preview ask never re-enters here (supplied facts win), so
        // the decision reads the same payload it would have spawned for.
        let payload = serde_json::json!({
            "cwd": cwd.display().to_string(),
            "pr": pr,
        });
        let (_code, stdout, _stderr) = crate::pr_status::cache::run_door("status-read", &payload);
        parse_checks_read(stdout.as_bytes())
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
/// A merge, never a rebase: the branch is already pushed.
fn stale_remedy(n: u64, base: &str) -> String {
    format!(
        "remedy: merge origin/{base} into the branch and push (fno do pr merge {n} does this \
         once it holds the merge slot), then fno do pr wait {n} --until settled, then retry"
    )
}

/// The slot holder's retest: GitHub merges the base into the PR branch (a
/// merge commit, never a rebase), pinned to the head this decision read, so
/// CI reruns against the current base. The next attempt reads that run.
fn retest_on_current_base<P: Probes>(probes: &P, cwd: &Path, facts: &PrFacts) -> String {
    let n = facts.number;
    let args = vec![
        "api".to_string(),
        "-X".to_string(),
        "PUT".to_string(),
        format!("repos/{{owner}}/{{repo}}/pulls/{n}/update-branch"),
        "-f".to_string(),
        format!("expected_head_sha={}", facts.head_sha),
    ];
    match probes.run_gh(cwd, &args) {
        Ok((true, _)) => format!(
            "merged {base} into PR {n}'s branch; CI reruns on the new head; \
             fno do pr wait {n} --until settled, then retry",
            base = facts.base_ref
        ),
        Ok((false, output)) => format!(
            "update-branch failed ({}); {}",
            first_line(&output),
            stale_remedy(n, &facts.base_ref)
        ),
        Err(error) => format!(
            "update-branch failed ({error}); {}",
            stale_remedy(n, &facts.base_ref)
        ),
    }
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

/// The graph tag that declares a repair lane: a node carrying it may merge
/// through a red main, because it IS the repair. The visual gate reads it
/// too: the lane's whole point is merging without further ceremony.
pub(crate) const MAIN_REPAIR_TAG: &str = "main-repair";

/// The refusal: the red run named, then the repair lane.
fn main_red_reason(workflow: &str, sha: &str, lane: &str) -> String {
    format!(
        "main is red: {workflow} failed at {}; merges hold while main's latest \
         settled run is red. {lane}",
        sha.chars().take(8).collect::<String>()
    )
}

/// Does one graph entry carry the tag?
pub(crate) fn node_carries_tag(entry: &Value, tag: &str) -> bool {
    entry
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| tags.iter().filter_map(Value::as_str).any(|t| t == tag))
}

/// The graph entry bound to this PR by its recorded `pr_number`. `None` on an
/// unreadable graph or no bound node: callers above treat absent as "not
/// bound", which for every gate here must hold rather than release.
pub(crate) fn pr_bound_entry(cwd: &Path, pr: u64) -> Option<Value> {
    let graph_path = crate::org_board::scope::graph_json_path(cwd);
    let store = GraphStore::new(&graph_path);
    let entries = backlog_api::rows(&store).ok()?;
    entries
        .into_iter()
        .find(|e| e.get("pr_number").and_then(Value::as_i64) == Some(pr as i64))
}

/// The main-repair exemption probe over the live graph: `None` when this
/// PR's node carries the [`MAIN_REPAIR_TAG`], else the repair-lane sentence.
fn main_repair_hold_probe(cwd: &Path, facts: &PrFacts) -> Option<String> {
    let graph_path = crate::org_board::scope::graph_json_path(cwd);
    let store = GraphStore::new(&graph_path);
    match backlog_api::rows(&store) {
        Err(e) => Some(format!(
            "the graph is unreadable ({}) so no repair lane can be named; tag \
             the repair node: fno backlog update <node> --tag {MAIN_REPAIR_TAG}",
            e.0
        )),
        Ok(entries) => main_repair_hold_from_entries(&entries, facts),
    }
}

/// The pure half of [`main_repair_hold_probe`], over already-read entries.
/// The PR's node is found by the same binding keys the node-binding gate
/// trusts; any one of them carrying the tag exempts the PR.
fn main_repair_hold_from_entries(entries: &[Value], facts: &PrFacts) -> Option<String> {
    let keys = pr_binding_keys(
        facts.number as i64,
        &facts.head_ref,
        Some(&facts.url),
        facts.body.as_deref(),
        entries,
    );
    let entry_of = |nid: &str| {
        entries
            .iter()
            .find(|e| crate::graph_store::entry_id(e) == Some(nid))
    };
    let bound: Vec<&Value> = keys
        .branch
        .iter()
        .chain(&keys.backrefs)
        .chain(&keys.trailer)
        .filter_map(|nid| entry_of(nid))
        .collect();
    if bound.iter().any(|e| node_carries_tag(e, MAIN_REPAIR_TAG)) {
        return None;
    }
    // The lane is named within the PR's own project: another project's
    // main-repair tag declares that project's main, never this one.
    let project = bound
        .iter()
        .find_map(|e| e.get("project").and_then(Value::as_str));
    let lanes: Vec<&str> = entries
        .iter()
        .filter(|e| node_carries_tag(e, MAIN_REPAIR_TAG))
        .filter(|e| match project {
            Some(p) => e.get("project").and_then(Value::as_str) == Some(p),
            None => true,
        })
        // A done node cannot be the repair lane: its repair landed, so a
        // pointer that still names one is stale; the sentence names a live
        // node or none.
        .filter(|e| e.get("status").and_then(Value::as_str) != Some("done"))
        .filter_map(|e| crate::graph_store::entry_id(e))
        .collect();
    Some(if lanes.is_empty() {
        format!(
            "no node is declared the repair lane; tag the repair node: \
             fno backlog update <node> --tag {MAIN_REPAIR_TAG}"
        )
    } else {
        format!("the declared repair lane is {}", lanes.join(", "))
    })
}

/// The node-binding gate over the live graph. The scope is the canonical repo
/// root: a repo whose graph names no node under it (a stock install that
/// never used the backlog, an external-tracker project) has nothing to bind
/// to and merges as before.
fn node_binding_probe(cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
    let root = canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let graph_path = crate::org_board::scope::graph_json_path(cwd);
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
/// keys are the board classifier's own (`org_board::prs::pr_binding_keys`).
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
    if let Some(detail) = retarget_binding_refusal(&keys, body, facts.number as i64, &facts.url) {
        return ProbeOutcome::Refused(detail);
    }
    ProbeOutcome::Clear
}

/// Parse the status read's own pull projection into [`PrFacts`]: the same
/// facts the `fno do pr info` spawn answers, from the payload the read
/// already fetched. A projection older than the `url`/`body` fields is NOT a
/// fact - the caller falls back to the spawn rather than walking with an
/// unevaluable backref key or a dead-instrument head.
pub fn facts_from_pulls(pulls: &Value, fallback_pr: Option<u64>) -> Result<PrFacts, String> {
    let number = pulls
        .get("number")
        .and_then(Value::as_u64)
        .or(fallback_pr)
        .ok_or_else(|| "the pull payload carried no PR number".to_string())?;
    let head_sha = pulls
        .get("headRefOid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if head_sha.is_empty() {
        return Err(format!(
            "the pull payload carried no head sha for PR {number}"
        ));
    }
    let url = pulls
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = pulls.get("body").and_then(Value::as_str);
    if url.is_empty() || body.is_none() {
        return Err("the pull payload predates the url/body fields".to_string());
    }
    Ok(PrFacts {
        number,
        head_sha,
        head_ref: pulls
            .get("headRefName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        base_ref: pulls
            .get("baseRefName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        url,
        body: Some(body.unwrap_or("").to_string()),
        state: pulls
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("OPEN")
            .to_string(),
        armed: pulls
            .get("auto_merge")
            .map(|v| !v.is_null())
            .unwrap_or(false),
    })
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

/// Did the CI at this head test a tree that already held the base tip? Only
/// the ancestry answers that. The run-timestamp heuristic this replaces
/// cleared a head whose runs started after the base tip moved, but a run at a
/// head that lacks the tip still never tested the merge - the back-to-back
/// merge race that went red on main three times in two days (2026-10-04/05).
pub fn ci_base_verdict(behind_by: u64) -> ProbeOutcome {
    if behind_by == 0 {
        return ProbeOutcome::Clear;
    }
    ProbeOutcome::Refused(format!(
        "ci_base_stale: PR is {behind_by} behind its base; no run at this head tested the merged tree"
    ))
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
    if payload.get("op").and_then(Value::as_str) == Some("pr-head") {
        return (
            0,
            format!("{}\n", head_probe::receipt(&payload)),
            String::new(),
        );
    }
    // The hold ops are merge-authority writes riding this verb's payload, not
    // a new top-level root: `{"op": "hold-set"|"freeze-set"|..., ...}` answers
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
    // The freeze ops are the scoped merge freeze's transport, riding the same
    // payload the hold ops use: `{"op": "freeze-set"|"freeze-clear"|"freeze-check",
    // ...}` writes and reads the team's freeze record.
    if payload
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op.starts_with("freeze-"))
    {
        let out = crate::merge_freeze::run(
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
    // The effect ops are the effect-classification door the ported Python
    // approvals callers route through, the same transport the grant ops use:
    // `{"op": "effect-classify"|"effect-submit"|"effect-verdict", ...}`.
    if payload
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op.starts_with("effect-"))
    {
        let out = crate::effect_gate::run_op(
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
        let op = payload.get("op").and_then(Value::as_str).unwrap_or("");
        // The verb-shaped door ops answer with the verb's own streams and
        // exit; the fact ops keep their JSON-receipt contract.
        if matches!(
            op,
            "status-read" | "status-wait" | "status-logs" | "status-ci"
        ) {
            return crate::pr_status::cache::run_door(op, &payload);
        }
        let out = crate::pr_status_facts::run_op(op, &payload);
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
        // The scoped merge freeze: an off-list PR refuses with a receipt
        // naming the freeze; an unreadable record refuses fail closed.
        if let Some((code, message)) = crate::merge_freeze::refusal(request.pr) {
            return (code, String::new(), message);
        }
    }
    // A preview ask answers with the structured receipt: `ready` is the
    // receipt's `blockers` being empty, so the verb prints the list itself
    // instead of the joined-prose Outcome form the effect arms render.
    if request.effect == Effect::Preview {
        let receipt = preview_receipt(&request);
        return (0, format!("{}\n", receipt), String::new());
    }
    // The receipt is the verdict, so the exit code answers only whether the verb
    // RAN: 0 with a receipt on stdout, 2 when the payload was unusable. A code
    // that also encoded refusal would make a held merge indistinguishable from a
    // binary that could not start, and the caller reads the receipt either way.
    let mut observed_head = None;
    let outcome = run_observed(&RealProbes, &request, &mut observed_head);
    let mut receipt = outcome.to_json();
    receipt["observed_head"] = json!(observed_head);
    (0, format!("{receipt}\n"), String::new())
}

mod head_probe;
mod preview_receipt;

pub(crate) use preview_receipt::{
    parse_request, preview_receipt, preview_receipt_payload, read_payload,
};

#[cfg(test)]
mod tests;
