//! Which local reviews attest this head? The in-scope attestation chain, rounds since the last pass, refused and latest local verdicts.

use super::*;

/// One reviewer's latest attestation, the commit it pinned, and whether that
/// verdict was `pass` (: a `fail` whose findings are all terminal
/// answers the head too, so the coverage axis needs the verdict, not just the
/// pass subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalPass {
    pub(super) reviewer: String,
    pub(super) attester: Option<String>,
    /// The head this attestation pinned. Whether it still counts is
    /// [`review_freshness`]'s call, not this scan's.
    pub(super) head: String,
    /// The branch the attestation named. Empty only for the legacy exact-head
    /// fallback, so the verdict can carry the matching scope label.
    pub(super) branch: String,
    /// The merge-base end of the diff the attestation measured
    /// (`reviewed_base_sha`). Empty on events that predate the field; the
    /// tiling chain reads it, nothing else does.
    pub(super) reviewed_base: String,
    pub(super) is_pass: bool,
    /// The line that produced this entry was a RETRACTION
    /// (`retracts_attester`): it revokes, it never covers - the answered-fail
    /// arm must not resurrect the pass it killed.
    pub(super) is_retraction: bool,
    /// Positive fresh-context provenance from the attestation
    /// (`reviewer_context`: fresh | shared | unknown). None on events that
    /// predate the field. Read only by the posture verdict (rung 4); the
    /// coverage count never reads it.
    pub(super) reviewer_context: Option<String>,
}

/// The branch-scoped `review_attestation` chain, oldest first: branch match
/// with the legacy exact-head admission, both verdicts, head-pinned lines
/// only. ONE parse serves every consumer (disposition blockers, tiling
/// ranges, the answered-fail predicates); a second hand-copy of this loop is
/// how the gates drift.
pub(super) fn in_scope_chain(events_text: &str, head_branch: &str, head_sha: &str) -> Vec<Value> {
    let mut chain: Vec<Value> = Vec::new();
    for line in events_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let Some(line_head) = val.pointer("/data/head_sha").and_then(|v| v.as_str()) else {
            continue;
        };
        if line_head.is_empty() {
            continue;
        }
        let line_branch = val
            .pointer("/data/branch")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !attestation_in_scope(line_branch, line_head, head_branch, head_sha) {
            continue;
        }
        chain.push(val);
    }
    chain
}

/// The non-terminal blocking findings in the PR's attestation chain.
///
/// Terminal means: fixed (and a LATER round reviewed the fix delta),
/// non-blocking by the gate's own re-derivation, or declined WITH a recorded
/// reason. Origin never gates: whoever attested the disposition, the
/// terminality is the same. Pure: scans the events text, no IO. An empty
/// chain has no findings and blocks nothing.
pub fn disposition_blockers(
    events_text: &str,
    head_branch: &str,
    head_sha: &str,
) -> Vec<DispositionBlocker> {
    let chain = in_scope_chain(events_text, head_branch, head_sha);
    disposition_blockers_on_chain(&chain)
}

/// The PR's review-round total, on two evidence axes. The operator's ruling
/// made this a PER-PR TOTAL: `max_rounds` counts rounds
/// across the whole life of the PR, and a `verdict: pass` refunds nothing -
/// it is one round like any verdict, and its coverage role lives elsewhere
/// (the pass scan and the classify), never here. The name survives from the
/// reset semantics it used to implement; the docstring, not the name, is
/// the contract.
///
/// One reviewed HEAD is one round, on both axes. The chain axis counts
/// DISTINCT `head_sha` among the in-scope attestation rows (the declared
/// `review_round` wins when present, as the running max, and needs no
/// collapse because the declared number is already the round's identity;
/// rows from before the field existed collapse by head). A row with no
/// readable head is dropped before scoping here, as it always was: this
/// axis cannot place it in the branch's lineage. The Python mirror admits
/// it on a branch match and counts it, which is the one measured
/// divergence between the two. Two verdicts at one
/// unchanged head are therefore ONE round: a cap whose size depends on how
/// a reviewer batches its output is not a cap. The reviews axis, when
/// a payload is supplied, counts DISTINCT reviewed commits - a GitHub-App
/// reviewer's rounds leave no attestation row anywhere, so they exist only
/// as review objects, and every fix moves the head, making one reviewed
/// commit one round. No timestamp filter on this axis either: a pass that
/// truncated the reviews older than itself would refund rounds only GitHub
/// saw. No author filter: the codex cloud connector posts its review objects
/// under the PR author's own login (measured live - 116 of 117 objects on
/// the spinning specimen), so an author exclusion deletes the round trace on
/// exactly that lane. Known bound, accepted: reply volume at ONE commit is
/// neutral, but replies landed on distinct never-reviewed heads each count
/// as a round. No discriminator exists in the review-object data (the
/// measured lane's review bursts and the worker's replies share a login, a
/// state, and a commit), and over-counting fires the cap on a worker already
/// push-replying without re-review, where the old under-count spun forever.
/// The answer is ONE shared head set across both axes, never the sum of two
/// independent counters: a healthy lane leaves both traces per round (the
/// review object AND the attestation at the same commit), so a head counts
/// once whoever recorded it - and a bot round at one sha beside a local
/// attestation at another is two rounds, which separate per-axis counters
/// followed by a max() would have under-counted (law d-608344c1: a round is
/// keyed by head). Scoped exactly like
/// the tiling and disposition scans: branch match, with the legacy
/// exact-head admission. Pure: scans its inputs, no IO. The Python
/// gate-side mirror is `rounds_since_last_pass` in `_coverage_gate.py`; the
/// two are held equal by the shared corpus.
pub fn rounds_since_last_pass(
    events_text: &str,
    head_branch: &str,
    head_sha: &str,
    reviews: Option<&[Value]>,
) -> i64 {
    let mut declared_rounds: i64 = 0;
    let mut counted_heads: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in events_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let Some(line_head) = val.pointer("/data/head_sha").and_then(|v| v.as_str()) else {
            continue;
        };
        if line_head.is_empty() {
            continue;
        }
        let line_branch = val
            .pointer("/data/branch")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !attestation_in_scope(line_branch, line_head, head_branch, head_sha) {
            continue;
        }
        match val.pointer("/data/review_round").and_then(|v| v.as_i64()) {
            // A declared round number is already the round's identity, so the
            // running max cannot double-count and needs no head collapse.
            Some(n) if n >= 0 => declared_rounds = declared_rounds.max(n),
            // One reviewed head is ONE round, the same unit the reviews axis
            // uses (DISTINCT commit.oid). Without this the two axes measure
            // different things and the total is not a budget: a
            // producer that emits a corrective second verdict at an unchanged
            // head spends two rounds for zero code change.
            _ => {
                counted_heads.insert(line_head.to_string());
            }
        }
    }
    let Some(reviews) = reviews else {
        return declared_rounds.max(counted_heads.len() as i64);
    };
    // The reviews axis. An object counts when it names a real reviewed
    // commit (state and commit.oid present). Any login may carry it: the
    // codex cloud connector posts its review objects under the PR author's
    // own login, so an author filter deletes the trace on exactly that
    // lane, and reply volume is already neutral because the unit is the
    // DISTINCT commit.
    for review in reviews {
        if review
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            continue;
        }
        let Some(oid) = review
            .pointer("/commit/oid")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        counted_heads.insert(oid.to_string());
    }
    declared_rounds.max(counted_heads.len() as i64)
}

/// Distinct `(reviewer, attester_session_id)` pairs' LATEST in-scope
/// attestation, each with the head it pinned and whether its verdict was
/// `pass` (the caller decides whether an answered `fail` counts; ).
/// Keying on the pair - not the reviewer name alone -
/// keeps a same-session re-run collapsed (one key, last-writer-wins, retraction
/// intact) while letting two sessions attesting under the same reviewer label
/// coexist: before this, a spawned peer emitting `code-review` replaced the
/// author's pass and a peer-reviewed PR read `1 reviewed` while deleting its
/// own control. `attester_session_id` is the harness session that emitted, or
/// None when the event predates the field. events.jsonl is append-ordered; a
/// later `fail` revokes, a later `pass` restores - mirrors
/// `unattested_reviewers_scan`'s retraction handling. Pure: scans text, no IO.
/// Presence-based: counts any reviewer regardless of the configured `reviewers`
/// list, so a worker-run `/code-review` counts even when `reviewers: []`.
///
/// Scoped to the PR under evaluation via `attestation_in_scope`: an
/// out-of-scope line is skipped ENTIRELY (never a verdict, never a stale
/// entry), which is also what keeps the pair key honest - one session
/// attesting PR B after PR A no longer overwrites A's pass with B's head.
/// A pass over a diff that changed NO FILE is a review of nothing, not a
/// review: a session resolving its target from a checkout sitting on the base
/// branch reads an empty diff and reports clean, and without this guard that
/// pass is byte-identical to a real one. The producer refuses to emit with
/// zero files, so a line carrying `reviewed_file_count: 0` is either
/// pre-guard or hand-crafted - skipped either way. Lines alone never decide
/// this: binary, pure-rename and empty-file diffs are real reviews whose
/// `reviewed_line_count` is an honest 0, so a 0-line row WITH files counts,
/// and a 0-line row from before the field existed (file count absent) is
/// still skipped - absence must not be read as "had files".
/// Whether a `review_attestation` line carries at least one keyed finding -
/// the answered-fail evidence predicate, ONE spelling for the reviewers scan
/// and the coverage promotion ( review 2, finding 4: the hand-copied
/// pair had already been flagged as the drift shape).
pub(super) fn line_carries_keyed_findings(val: &Value) -> bool {
    val.pointer("/data/findings")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter().any(|p| {
                p.get("finding_key")
                    .and_then(|v| v.as_str())
                    .map(|k| !k.is_empty())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub(super) fn zero_evidence_attestation(val: &Value) -> bool {
    if val
        .pointer("/data/reviewed_line_count")
        .and_then(|v| v.as_i64())
        != Some(0)
    {
        return false;
    }
    match val
        .pointer("/data/reviewed_file_count")
        .and_then(|v| v.as_i64())
    {
        Some(files) => files <= 0,
        None => true,
    }
}

/// Refused review attempts read from `review_invocation stage=refused` rows:
/// the durable terminal of a review that ran and produced no verdict because
/// there was nothing to read (reason `empty_diff`: the reviewer's measured
/// diff changed no file, so the producer refused to attest). Minted as a
/// `Refused` local verdict so `review_state` reads `ReviewerRefused` -
/// "attempted, nothing to review" - instead of the generic unreviewed that is
/// byte-identical to "never attempted" at every coverage surface. A refused
/// verdict never counts toward coverage; it only names the state, a later
/// real review outranks it (`review_state` checks `Reviewed` first), and a
/// moved head retires it (exact-head scoping, below).
pub(super) fn local_refused_verdicts(events_text: &str, head_sha: &str) -> Vec<ReviewerVerdict> {
    let mut out: Vec<ReviewerVerdict> = Vec::new();
    for line in events_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_invocation") {
            continue;
        }
        if val.pointer("/data/stage").and_then(|v| v.as_str()) != Some("refused") {
            continue;
        }
        let line_head = val
            .pointer("/data/head_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // EXACT-HEAD scoping, deliberately narrower than an attestation's: a
        // refusal is a terminal of one ATTEMPT at one measured head, not a
        // claim about the branch. Admitting it on the branch arm alone would
        // park ReviewerRefused at every later head (an author's fix push
        // cannot clear it, only a real review can), which reads downstream as
        // "the reviewer declined" at a head nobody attempted - and the local
        // refusal feeds the same reviewer_refused paths a bot quota bounce
        // does. Once the head moves, the state honestly returns to unreviewed:
        // nothing has reviewed the new head. An empty head (a forged or legacy
        // row) scopes to nothing.
        if line_head.is_empty() || line_head != head_sha {
            continue;
        }
        let mut name = val
            .pointer("/data/verb")
            .and_then(|v| v.as_str())
            .unwrap_or("review")
            .trim_start_matches('/')
            .to_string();
        if name.is_empty() {
            name = "review".to_string();
        }
        let refusal_reason = val
            .pointer("/data/reason")
            .and_then(|v| v.as_str())
            .filter(|r| !r.is_empty())
            .map(str::to_string);
        out.push(ReviewerVerdict {
            producer: CoverageProducer::LocalAttestation,
            name,
            verdict: CoverageVerdict::Refused,
            human_approval: false,
            author_approval: false,
            attestation_origin: AttestationOrigin::Unknown,
            reviewed_sha: line_head.to_string(),
            freshness: None,
            scope: None,
            refusal_reason,
            reviewer_context: None,
            // A refusal row records an attempt that RAN and declined: real
            // work with a real remedy, always owed its own name.
            required: true,
            passed: false,
        });
    }
    out
}

pub(super) fn local_latest_attestations(
    events_text: &str,
    head_branch: &str,
    head_sha: &str,
) -> (
    Vec<LocalPass>,
    std::collections::HashSet<(String, Option<String>)>,
) {
    // (reviewer, attester_session_id) -> (head it attested, branch it named,
    // was it a pass, the base its range measured, was it a retraction). The
    // attester lives in the key so cross-session attestations join instead of
    // replace. The HEAD is no longer a filter, it is a RESULT: which head an
    // attestation pinned is what the freshness predicate needs, and dropping
    // every non-matching line here is what made a rebase destroy a review.
    let mut latest: std::collections::HashMap<
        (String, Option<String>),
        (String, String, bool, String, bool, Option<String>),
    > = std::collections::HashMap::new();
    let mut raised_findings: std::collections::HashSet<(String, Option<String>)> =
        std::collections::HashSet::new();
    for line in events_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let Some(r) = val.pointer("/data/reviewer").and_then(|v| v.as_str()) else {
            continue;
        };
        // An event with no head_sha is not head-pinned evidence and is skipped
        // outright (defaulting it to "" would match a caller whose own head is
        // "", turning unpinned data into coverage - codex P1 on the attestation
        // gate, same class of lie this node deletes).
        let Some(line_head) = val.pointer("/data/head_sha").and_then(|v| v.as_str()) else {
            continue;
        };
        if line_head.is_empty() {
            continue;
        }
        if zero_evidence_attestation(&val) {
            continue;
        }
        // The branch this attestation named; empty on every event predating
        // the field, which attestation_in_scope then admits only on exact
        // head equality. Once the head moves, a pre-field line emits NO
        // verdict at all - deliberately: with no branch there is no way to
        // attribute the pass to THIS PR rather than a foreign one, and
        // recording it as Stale would pool every PR's legacy passes into
        // every coverage read (the cross-PR leak this node deletes). The
        // refusal's action for that cohort is the generic one: re-run the
        // review verb at HEAD, which emits a branch-scoped event.
        let line_branch = val
            .pointer("/data/branch")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !attestation_in_scope(&line_branch, line_head, head_branch, head_sha) {
            continue;
        }
        let is_pass = val.pointer("/data/verdict").and_then(|v| v.as_str()) == Some("pass");
        // attester_session_id is the live session that emitted; None on events
        // that predate the field (the whole backlog), which classifies as
        // Unknown downstream. Empty string is treated as None so the producer's
        // "unobservable" sentinel and the field's absence read identically.
        let attester = val
            .pointer("/data/attester_session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        // A RETRACTION names the pair it revokes in retracts_attester: an
        // operator session revoking a forged pass must not have to emit under
        // the victim's identity (which the attester binding now refuses). Key
        // the retraction on the NAMED pair so it lands on the pass being
        // revoked, while the retracting session's own entry stays untouched.
        let key_attester = val
            .pointer("/data/retracts_attester")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or(attester);
        // Under the pair key a peer's `fail` revokes only the peer's own pass;
        // the author's `pass` still counts toward coverage. Coverage counts
        // reviews performed, not approvals granted - the hold on a bad review
        // lives on `open_review_findings` and on `unattested_reviewers_scan`,
        // which keeps its name key (the config.review.reviewers gate).
        // A retraction names ONE pass, by head. When the pair's entry already
        // describes a different (newer) head, the named pass is superseded and
        // the retraction must not touch it: the verb emits the retraction after
        // any newer pass, so an unconditional insert would revoke a live pass
        // nobody asked to revoke. Same-head (and first-entry) inserts keep
        // today's semantics.
        let pair_key = (r.trim_start_matches('/').to_string(), key_attester);
        let is_retraction = val
            .pointer("/data/retracts_attester")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if is_retraction {
            if let Some((existing_head, _, _, _, _, _)) = latest.get(&pair_key) {
                if existing_head != line_head {
                    continue;
                }
            }
        }
        // reviewed_base_sha rides along for the tiling chain: a same-pair
        // re-attest after a push replaces the entry AND its range, so the
        // chain never reads a superseded range as covering anything.
        let reviewed_base = val
            .pointer("/data/reviewed_base_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Positive context provenance rides with the pair's latest line, so a
        // re-attest replaces the marker with the newest evidence.
        let reviewer_context = val
            .pointer("/data/reviewer_context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        //: a fail that RAISED keyed findings marks its own pair as
        // answer-capable. Per-pair, never chain-global: a bystander's
        // findings-free fail must not ride another reviewer's dispositions
        // (review finding 2), and a retraction line marks nothing. The mark
        // is CUMULATIVE across the pair's rounds by design - round 1 raises,
        // round 2 records the dispositions, and the pair's latest fail is
        // the answered verdict (the fix-delta shape). Recorded BEFORE the
        // insert consumes the key; for a non-retraction line key_attester IS
        // the line's own attester.
        if !is_pass && !is_retraction && line_carries_keyed_findings(&val) {
            raised_findings.insert(pair_key.clone());
        }
        latest.insert(
            pair_key,
            (
                line_head.to_string(),
                line_branch,
                is_pass,
                reviewed_base,
                is_retraction,
                reviewer_context,
            ),
        );
    }
    let mut out: Vec<LocalPass> = latest
        .into_iter()
        .map(
            |(
                (reviewer, attester),
                (head, branch, is_pass, reviewed_base, is_retraction, reviewer_context),
            )| {
                LocalPass {
                    reviewer,
                    attester,
                    head,
                    branch,
                    reviewed_base,
                    is_pass,
                    is_retraction,
                    reviewer_context,
                }
            },
        )
        .collect();
    out.sort_by(|a, b| {
        (&a.reviewer, &a.attester, &a.head).cmp(&(&b.reviewer, &b.attester, &b.head))
    });
    (out, raised_findings)
}

/// Stamp the resolved required set onto a classified report: a github_app
/// verdict is OWED exactly when its login is in `required_logins`. The
/// classifier itself reads only the configured list (a configured login's
/// verdict is owed until proven otherwise), so this is the one place that may
/// downgrade an OPTIONAL login - and an optional App that sat absent then
/// stops owning the receipt's waiting arm or the stop gate.
pub fn mark_owed_verdicts(coverage: &mut CoverageReport, required_logins: &[String]) {
    for verdict in &mut coverage.verdicts {
        if verdict.producer == CoverageProducer::GithubApp {
            verdict.required = required_logins
                .iter()
                .any(|l| login_matches_bot(&verdict.name, l));
        }
    }
}

/// Whether an author login is a KNOWN review App (a BOT_PROFILES login or a
/// configured github_app). A present review from such an App counts toward
/// coverage; a random `[bot]` suffix alone does not (it may be a non-review
/// automation).
pub(super) fn author_is_known_bot(author: &str, github_app_logins: &[String]) -> bool {
    BOT_PROFILES.iter().any(|p| author.contains(p.login))
        || github_app_logins
            .iter()
            .any(|l| login_matches_bot(author, l))
}

/// Whether an author login is a bot of any kind (known App, or any `[bot]`).
/// Used to separate a human GitHub approval from an app review on the same axis.
pub(super) fn author_is_bot(author: &str, github_app_logins: &[String]) -> bool {
    author.ends_with("[bot]") || author_is_known_bot(author, github_app_logins)
}

/// One local-attestation verdict from a pair's latest attestation, shared by
/// the pass arm and 's answered-fail arm so the two can never drift.
/// Counts (`Reviewed`) when the attestation's head is fresh OR a member of a
/// tiled chain; the authorship label and scope marker come off the entry.
pub(super) fn local_attestation_verdict(
    lp: &LocalPass,
    freshness: &dyn Fn(&str) -> Freshness,
    tiling: Option<&RangeTiling>,
    author_session: Option<&str>,
) -> ReviewerVerdict {
    let fresh = freshness(&lp.head);
    let chain_member = tiling
        .map(|t| t.tiled && t.chain_heads.iter().any(|h| h == &lp.head))
        .unwrap_or(false);
    ReviewerVerdict {
        producer: CoverageProducer::LocalAttestation,
        name: lp.reviewer.clone(),
        verdict: if fresh.counts() || chain_member {
            CoverageVerdict::Reviewed
        } else {
            CoverageVerdict::Stale
        },
        human_approval: false,
        author_approval: false,
        attestation_origin: classify_attestation_origin(lp.attester.as_deref(), author_session),
        reviewed_sha: lp.head.clone(),
        freshness: Some(fresh),
        reviewer_context: lp.reviewer_context.clone(),
        // In-scope guarantees one of exactly two shapes: a line admitted
        // while scope lasts (branch match, or the exact head sha - both of
        // which keep the carry reachable on later head moves), or a legacy
        // line admitted on exact head equality alone. The label lets a
        // refusal name the second kind rather than drop it silently.
        scope: Some(if lp.branch.is_empty() {
            AttestationScope::LegacyHeadMatch
        } else {
            AttestationScope::AttestedBranch
        }),
        refusal_reason: None,
        // The local lane is the config-floored reviewer (`self_review_required`);
        // its verdict is always owed, whatever the required bot list says.
        required: true,
        passed: lp.is_pass,
    }
}
