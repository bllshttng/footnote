//! What does the block message say? The sized self-review hint and the block reason builder.

use super::*;

/// The sized self-review invocation for this worker, via `fno do target
/// review-invocation`. The single construction site stays Python
/// (`self_review_invocation` + `level_for_diff`); Rust is a dumb pipe for its
/// stdout, so the invocation a held worker is told to run cannot drift from
/// the builder the parity and single-source gates police. `--harness` is
/// passed from the SAME `resolve_harness` the gate uses, so the render names
/// the verb for the harness that will actually run it.
///
/// Fail-open on every error path (no fno, non-zero exit, empty or multi-line
/// stdout, timeout): the hint is advisory text appended to refusal reasons,
/// and a missing hint must never change a gate verdict - callers fall back to
/// the levelless "run the review verb at HEAD" line. `fno_bin` resolved by the
/// caller (from `FNO_LOOPCHECK_FNO_BIN`, default `fno`) so this is hermetically
/// testable with a stub script, same as `evaluate_plan_fidelity`.
pub(super) fn sized_self_review_hint(
    fno_bin: &str,
    cwd: &Path,
    harness: Option<&str>,
) -> Option<String> {
    let mut args: Vec<&str> = vec!["do", "target", "review-invocation"];
    if let Some(h) = harness {
        args.push("--harness");
        args.push(h);
    }
    match run_bounded(
        std::ffi::OsStr::new(fno_bin),
        &args,
        cwd,
        std::time::Duration::from_secs(10),
    ) {
        BoundedRun::Completed(out) => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // One sane line only: a wrapper's chatter or an error page must
            // never end up embedded in a held reason as if it were an
            // invocation. Empty means the render itself gave up. An unsized
            // render (a repo whose default branch is neither main nor master)
            // returns the `<level>` placeholder form - a non-runnable string
            // in a copy-me slot, so it reads as no hint and the levelless
            // line stands.
            if s.is_empty() || s.contains('\n') || s.len() > 80 || s.contains("<level>") {
                None
            } else {
                Some(s)
            }
        }
        _ => None,
    }
}

/// `head_shipped` is passed in for the same reason `async_wait_class` takes
/// it: this function held the THIRD copy of the predicate, and its copy is the
/// one that rendered "push the latest commits" at a session with nothing to
/// push. Computed once by the caller via `head_is_shipped`.
pub(super) fn build_block_reason(
    pr: &PrInfo,
    local_head: &str,
    open_findings_empty: bool,
    head_shipped: bool,
) -> String {
    // The ONE predicate. A message that prescribes the arm-and-tag ritual for a
    // blocker `async_wait_class` refuses to idle is the code contradicting
    // itself, and a session complied with exactly that roughly ten times on
    // #618. Deriving the hint from the classifier makes the two agree by
    // construction rather than by two hand-kept branch orders.
    let idlable = async_wait_class(pr, open_findings_empty, head_shipped);
    let hint = |blocker: &str| -> String {
        if idlable == Some(blocker) {
            arm_watch_hint(pr.number, blocker)
        } else {
            String::new()
        }
    };
    if !pr.state.is_open_or_merged() {
        // `None` is the pre-ship state EVERY session sits in before the PR
        // exists, not an error, so it gets the create verb. Sending it to
        // GitHub to "verify" a PR that was never opened is an errand with no
        // end. The catch-all keeps the verify wording, because it means gh
        // answered a state this build does not know.
        let guidance = match pr.state {
            PrState::Closed => {
                "PR is closed. Reopen with `gh pr reopen`, or create a new PR: `gh pr create`"
            }
            PrState::None => "No PR for HEAD yet. Create one: `/fno:pr create`, or `gh pr create`",
            _ => "PR state is not open or merged - verify on GitHub and update locally",
        };
        return format!(
            "no open/merged PR for HEAD (pr_state={}). {}",
            pr.state.as_str(),
            guidance
        );
    }

    // Gated on the shared predicate, not on bare inequality. A branch that
    // fast-forwarded or rebased onto a base already containing its merge has a
    // different head and nothing to push, and this message told two such
    // sessions to push anyway. The message itself is unchanged for the case it
    // is right about: a genuinely unpushed commit.
    // The `is_empty` half is load-bearing and was nearly dropped in the
    // refactor. `head_is_shipped` answers false for an EMPTY recorded head, so
    // a bare `!head_shipped` would render this message with a blank sha for a
    // `gh pr view` that returned an open PR without `headRefOid`, hiding the
    // real CI or review blocker behind an unactionable push instruction. That
    // is the same wedge class this whole change exists to remove.
    if !pr.head_oid.is_empty() && !head_shipped {
        return format!(
            "PR #{} head {} != local HEAD {}: push the latest commits before completing",
            pr.number,
            short_sha(&pr.head_oid),
            short_sha(local_head)
        );
    }

    // A conflicting head has work to do NOW (rebase): "CI still running" or
    // "declare ci.declared_none" would prescribe waiting on a head that can
    // start no new check, and whose existing results are stale. It follows
    // the head arm on purpose: a local head that differs from the PR head
    // means the worker may have rebased already and just not pushed.
    if let Some(r) = conflicting_reason(pr) {
        return r;
    }

    if !pr.ci_conclusion.is_ok() {
        if pr.ci_conclusion == CiConclusion::None {
            return format!(
                "no CI checks found on PR #{}; declare ci.declared_none: true in settings if intentional",
                pr.number
            );
        }
        // Pending is "not green YET", not red. The MUTE_PROBE_N probe
        // runs done() while CI is commonly still in flight,
        // so a "CI failed" message here would mislead the blocked agent
        // into debugging a nonexistent failure on every quiet fire.
        if pr.ci_conclusion == CiConclusion::Pending {
            return format!("CI still running on PR #{}.{}", pr.number, hint("ci"));
        }
        let check_name = match &pr.ci_conclusion {
            CiConclusion::Failure(Some(name)) => name.as_str(),
            _ => "CI",
        };
        // No `hint("ci")` here. This arm is reachable only on Failure(_), and
        // `async_wait_class` refuses `Some("ci")` for exactly that conclusion,
        // so the hint is always empty. Rendering it would be dead today and a
        // contradiction the day someone relaxes that guard: read the log now,
        // and idle waiting, in one sentence.
        return format!(
            "CI red on PR #{}: {} failed. Read the failing log: `fno do pr logs {}`.",
            pr.number, check_name, pr.number
        );
    }

    if !pr.reviewed {
        // Order: work you can do now, cheapest-to-invalidate first, then the
        // async wait. An unaddressed finding leads because addressing it MOVES
        // HEAD, which supersedes any attestation produced before it - naming
        // the reviewer first would make a session run sigma twice. The bot
        // wait comes last: naming only the bot hides the half the session can
        // act on now, and if the bot never posts, the local work never happens
        // and the run dies on budget with the gate still unmet.
        if !pr.unaddressed_findings.is_empty() {
            // AC2-UI: name the specific finding (path:line) and the remedy.
            let f = &pr.unaddressed_findings[0];
            let more = if pr.unaddressed_findings.len() > 1 {
                format!(" [+{} more]", pr.unaddressed_findings.len() - 1)
            } else {
                String::new()
            };
            // AC14: "reply in-thread" alone is a half-remedy - a reply
            // that does not address the bot by its full login never reaches it.
            // Name the handle when the finding author is a known bot.
            let reply_to = profile_by_author(&f.author)
                .map(|p| format!(" addressed to {}", p.reply_handle))
                .unwrap_or_default();
            // The silent failure (PR #447, #787): a finding answered with a
            // top-level PR comment reads as unaddressed because this gate only
            // walks in_reply_to_id chains on /pulls/N/comments. The worker
            // sees green CI, its own fix commits, and a loop that will not
            // terminate, with nothing connecting the two. When any unaddressed
            // finding has no reply at all, lead with the mechanism and the
            // exact command - "reply in-thread" alone was misread twice as
            // "I did reply" by workers who had posted top-level comments.
            let no_reply = pr
                .unaddressed_findings
                .iter()
                .filter(|fnd| !fnd.had_reply)
                .count();
            if no_reply > 0 {
                return format!(
                    "PR #{}: {} blocking finding(s) unaddressed, {} with no in-thread reply. \
                     A top-level PR comment is NOT detected - this gate reads in_reply_to_id chains \
                     on /pulls/{}/comments only. Reply in-thread: gh api repos/$OWNER/$REPO/pulls/{}/comments \
                     -F in_reply_to=<id> -f body='Fixed in <sha>: ...' (or wontfix: <reason>). \
                     First: {} {} at {}:{}{}",
                    pr.number,
                    pr.unaddressed_findings.len(),
                    no_reply,
                    pr.number,
                    pr.number,
                    f.author,
                    f.severity,
                    f.path,
                    f.line,
                    more
                );
            }
            return format!(
                "PR #{}: {} {} at {}:{} unaddressed (reply in-thread{} or wontfix:){}",
                pr.number, f.author, f.severity, f.path, f.line, reply_to, more
            );
        }
        if !pr.unattested_reviewers.is_empty() {
            // The branch that was missing. Without it a local-only
            // reviewers gate fell through to the generic string below and told
            // the session to wait on a bot that was never required.
            //
            // No arm_watch_hint here, deliberately: `async_wait_class` has
            // ALREADY excluded this blocker from idling, because no GitHub
            // reviewer will ever post the attestation and the session would park
            // forever. Emitting the arm-and-tag ritual on a blocker the same
            // file refuses to idle is the code contradicting itself, and a
            // session did comply with it roughly ten times.
            let head = short_sha(local_head);
            // The verb a wedged session is told to run is harness-correct:
            // codex gets `/review`, claude `/code-review`. Resolved here from the
            // ambient author markers (the same `resolve_harness` the gate uses)
            // rather than threaded through every caller, so the 20+ build_block_reason
            // call sites stay single-arg.
            let author_harness = crate::claims::resolve_harness();
            // The sized render for the code-review verb, resolved once for the
            // whole list. The map value carries a `<level>` placeholder this
            // surface's reader has no renderer for; the bridge substitutes the
            // diff-sized level. None keeps the placeholder form (fail-open,
            // same as every other hint consumer).
            let sized = sized_self_review_hint(
                &std::env::var("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|_| "fno".into()),
                &std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()),
                author_harness.as_deref(),
            );
            let items: Vec<String> = pr
                .unattested_reviewers
                .iter()
                .map(|r| {
                    // "no attestation" is a lie to a session that ran the
                    // reviewer and got told no; name that case separately.
                    let state = if r.failed_at_head {
                        " (attested at this head, verdict NOT pass)".to_string()
                    } else {
                        match &r.superseded_head {
                            Some(h) => {
                                format!(" (passed at {}, superseded by this head)", short_sha(h))
                            }
                            None => String::new(),
                        }
                    };
                    if r.name == SAME_MODEL_LOCAL_PEER_SENTINEL {
                        return format!(
                            "peer{} -> configure a cross-model peer or routed model",
                            state
                        );
                    }
                    if r.name == LOCAL_PEER_REVIEWER {
                        return format!("peer{} -> run `/fno:review peer --attest`", state);
                    }
                    match reviewer_invocation_for(&r.name, author_harness.as_deref()) {
                        Some((inv, self_cert)) => {
                            let mark = if self_cert {
                                " [self-cert: asserts no review evidence]"
                            } else {
                                ""
                            };
                            // The sized render replaces the placeholder-carrying
                            // map value verbatim: it IS the same invocation with
                            // the level substituted by the Python single source.
                            let inv = if r.name == "code-review" {
                                sized.as_deref().unwrap_or(inv)
                            } else {
                                inv
                            };
                            // code-review is a native harness verb whose clean
                            // result normally reaches the shared attester. The
                            // helper remains the loud recovery path if that
                            // confirmed clean result did not produce evidence.
                            // The fno-skill reviewers (sigma, declare) attest
                            // inside their own invocation.
                            let emit_step = if r.name == "code-review" {
                                format!(
                                    ", if the confirmed clean review did not emit, recover with `bash skills/review/scripts/emit-attestation.sh {}`",
                                    r.name
                                )
                            } else {
                                String::new()
                            };
                            format!("{}{} -> run `{}`{}{}", r.name, state, inv, emit_step, mark)
                        }
                        None => format!("{}{}", r.name, state),
                    }
                })
                .collect();
            let corrupt = match pr.malformed_attestations {
                0 => String::new(),
                n => format!(" ({n} unparseable attestation line(s) ignored)"),
            };
            return format!(
                "PR #{}: reviewers gate unmet at {} - missing attestation for: {}{}. \
                 This is local work to DO, not a wait: no GitHub reviewer posts these, \
                 so do not arm a watcher.",
                pr.number,
                head,
                items.join("; "),
                corrupt
            );
        }
        if !pr.missing_bots.is_empty() || !pr.stale_bots.is_empty() {
            // A stale bot's "missing" is a re-read, not a first read. The
            // suffix naming the commit it actually read is formatted HERE and
            // nowhere else, so the nudge messages, the stale-only sentence,
            // and the mixed-set tail cannot drift apart about the same bot -
            // the local-reviewer branch above names its superseded sha the
            // same way. An unpinned read (empty sha) gets no suffix; the
            // coverage axis owns that message.
            let read_note = |login: &str| match pr.stale_bots.iter().find(|(b, _)| b == login) {
                Some((_, sha)) if !sha.is_empty() => {
                    format!(" (read {}, superseded by this head)", short_sha(sha))
                }
                _ => String::new(),
            };
            //: render per nudge state. `hint("review")` is derived from
            // async_wait_class, so it is EMPTY for NeedsNudge/Unresponsive (both
            // non-idlable) and PRESENT for Awaiting/NotNudgeable by construction -
            // the arm-and-tag ritual can never appear on a blocker the same file
            // refuses to idle (the contradiction removed). NeedsNudge and
            // Unresponsive lead because they are work/decisions, not waits. These
            // branches come BEFORE the stale-only sentence on purpose: a stale
            // bot in NeedsNudge or Unresponsive state needs the concrete remedy
            // (the trigger command, the counters, the do-not-arm guidance), and
            // a generic re-read sentence would strand it.
            if let Some(n) = pr
                .bot_nudges
                .iter()
                .find(|n| n.class == NudgeClass::NeedsNudge)
            {
                return format!(
                    "PR #{}: {}{} reviews on mention, not on push, and has not been asked. Run:\n  \
                     gh pr comment {} --body \"{}\"\nthen arm a watcher (nudge {} of {}).{}",
                    pr.number,
                    n.login,
                    read_note(&n.login),
                    pr.number,
                    n.review_handle,
                    n.nudges + 1,
                    n.ceiling,
                    hint("review")
                );
            }
            if let Some(n) = pr
                .bot_nudges
                .iter()
                .find(|n| n.class == NudgeClass::Unresponsive)
            {
                return format!(
                    "PR #{}: {}{} did not review after {} nudges over {}m. Nothing further \
                     will arrive on its own. Either post the review by hand, or move this \
                     login to config.review.optional_apps (honored-if-present, never waited \
                     on). Not a wait: do not arm a watcher.{}",
                    pr.number,
                    n.login,
                    read_note(&n.login),
                    n.nudges,
                    n.span_min,
                    hint("review")
                );
            }
            if let Some(n) = pr
                .bot_nudges
                .iter()
                .find(|n| n.class == NudgeClass::Awaiting)
            {
                return format!(
                    "PR #{}: {}{} nudged {}m ago ({} of {}), awaiting review.{}",
                    pr.number,
                    n.login,
                    read_note(&n.login),
                    n.newest_age_min,
                    n.nudges,
                    n.ceiling,
                    hint("review")
                );
            }
            // A stale-only blocker with no nudge state worth acting on gets its
            // own sentence: the bot responded, so "has not reviewed" would be
            // the exact lie this branch deletes.
            if pr.missing_bots.is_empty() {
                let items: Vec<String> = pr
                    .stale_bots
                    .iter()
                    .map(|(b, _)| format!("{b}{}", read_note(b)))
                    .collect();
                return format!(
                    "PR #{}: {} reviewed an older commit whose code no longer matches \
                     this head - ask for a re-read (a push alone does not re-trigger it).{}",
                    pr.number,
                    items.join("; "),
                    hint("review")
                );
            }
            // All NotNudgeable (or not classified) + hint (AC5 - a non-nudgeable
            // required bot keeps the pre- behavior). The remedy must NOT
            // say "trigger it": `nudge_class_idlable` counts NotNudgeable as
            // idlable, so `hint("review")` renders the arm-and-tag ritual right
            // after this sentence, and telling a session to act and to idle in
            // one line is the contradiction removed.
            // A stale bot riding along in a mixed set keeps the note so its
            // entry does not read as "never responded" (each required bot is in
            // exactly one of the two lists, so no dedup is needed).
            // The same-model peer sentinel is a required login by construction
            // and no review can ever carry its NUL-wrapped name, so it reaches
            // here through the `_ => missing_bots.push(bot)` arm. Printing it as
            // a bot name told the operator to move an unmatchable sentinel to
            // `optional_apps`. The real remedy is the one the stderr line at the
            // rewrite site gives, and the reviewers-gate arm above already
            // special-cases its local twin.
            let mut names: Vec<String> = Vec::new();
            let mut same_model_peer = false;
            for b in &pr.missing_bots {
                if b == SAME_MODEL_PEER_SENTINEL {
                    same_model_peer = true;
                } else {
                    names.push(b.clone());
                }
            }
            names.extend(
                pr.stale_bots
                    .iter()
                    .map(|(b, _)| format!("{b}{}", read_note(b))),
            );
            if names.is_empty() {
                // No hint: nothing will ever post for the sentinel, so arming a
                // watcher parks the session on a wait with no end. Keeping the
                // loop awake lets the NoProgress backstop reap it instead.
                return format!(
                    "PR #{}: the cross-model peer gate is unmet - the configured peer is the \
                     author's own model, so nothing can satisfy it. Configure a cross-model \
                     peer or a model route.",
                    pr.number
                );
            }
            let peer_note = if same_model_peer {
                " The cross-model peer gate is also unmet: configure a cross-model peer or a model route."
            } else {
                ""
            };
            // "footnote posts no mention", NOT "a mention will not trigger it":
            // NotNudgeable means no profile, an override, a disabled nudge, or
            // the sentinel. Only the last says anything about the bot itself.
            return format!(
                "PR #{}: {} has not reviewed. footnote has no nudge configured for it, so it \
                 posts no mention. Wait for it to post, or move it to \
                 config.review.optional_apps if it never will.{}{}",
                pr.number,
                names.join(", "),
                peer_note,
                hint("review")
            );
        }
        // Unknown AFTER the specific arms but BEFORE the config complaint:
        // the unmet gate, the unaddressed finding, and the outstanding bot
        // are all more actionable than the read remedy, but "no reviewer is
        // outstanding" beside an unread coverage axis is the read remedy's
        // exact case (the pinned test: Unknown names the read remedy, never
        // a config lecture).
        if matches!(pr.coverage.coverage, Coverage::Unknown) {
            return coverage_unavailable_description(&pr.head_oid);
        }
        // Reaching here means missing_bots is empty, which `async_wait_class`
        // treats as non-idlable, so this must not teach the arm-and-tag ritual
        // either (the two must never disagree about whether a wait is valid).
        return format!(
            "PR #{} not yet reviewed and no reviewer is outstanding. \
             Check config.review: github_apps (GitHub App logins) and \
             reviewers (local reviewers like sigma). \
             Nothing here will arrive on its own.",
            pr.number
        );
    }

    // And the reviewed-but-Unknown case (a satisfied gate beside an unread
    // axis) keeps the read remedy here, outside the block.
    if matches!(pr.coverage.coverage, Coverage::Unknown) {
        return coverage_unavailable_description(&pr.head_oid);
    }

    // A merge-slot hold renders beside the other hold remedies
    // (conflicting_reason), so both hold arms teach one voice and the
    // classifier's hint rides by construction.
    if let Some(r) = merge_slot_reason(pr, open_findings_empty, head_shipped) {
        return r;
    }

    format!("PR #{} done() returned false (unknown reason)", pr.number)
}
