---
name: review
description: "Review a diff or a research brief. Routes to the fno-owned inline review lane (default: levels low/medium/high/xhigh/max, --comment, --fix, optional PR/branch/path target), runtime evidence (prove-it), the apply-or-skip cleanup pass (cleanup), a cross-model second opinion (peer), the advisory research-verify panel for a doc deliverable (research), or a self-cert attestation for the config.review.reviewers gate (declare). Use when: 'review this', 'code review', 'is this ready', 'prove it works', 'clean this up', 'get a second opinion', 'review this research brief', 'declare this reviewed'."
argument-hint: "[level] [--comment] [--fix] [<pr#>|<branch>|<path>] | prove-it [<target>] | cleanup [<target>] | peer [adversarial] [--attest|--post] [PR#|branch] [codex|gemini] | research [brief.md] | declare   e.g. (bare = the fno lane, level sized from the diff), `high --comment`, `657`, `prove-it`, `cleanup`, `peer 657 codex --attest`, `research out/topic.md`, `declare`"
requires:
  binaries:
    - "fno >= 0.1"
    - "gh >= 2.0"
    - "git >= 2.0"
---

<!-- style-exception: this file's producer bullets under "The attestation surface" are established dense single-line paragraphs joining several clauses with semicolons and dashes, a convention used throughout the file. Rewriting that file-wide convention is out of scope for x-e97b, which only touches one such bullet's body to name the new PostToolUse hook. -->

# Review

**One verb on a diff.** `/review` routes to the right reviewer set for the diff in front of you.

| Mode | What runs | Shared object |
|------|-----------|---------------|
| default (bare, or a level token) | the fno review lane: inline angles, dedup, three-state verify, one attestation | the diff |
| `prove-it` | runtime evidence at the changed code's real surface, with a validator that refuses an unprobed PASS | the running change |
| `cleanup` | the apply-or-skip pass: four cleanup angles once, no gate weight, no attestation | the diff |
| `peer` | a cross-model second opinion, optionally producing a verdict-gated local attestation | the diff |
| `research` | advisory research-verify panel (fact-checker / citation-auditor / contradiction-finder / completeness-critic) | a `doc` deliverable (brief + sources sidecar) |
| `sigma` | RETIRED. The token refuses and names the default lane as the replacement. | - |

This is a **router**, not a monolith. It parses the first argument as a mode, announces the resolved mode, then loads that mode's reference and follows it in this same context. It never calls another skill at runtime (it dispatches review subagents via the Task/Agent tool and loads modes via Read).

## Review-cap gate

The two-round cap is enforced at the review invocation itself, not only at the merge decision: the hold hook denies a review whose PR's rounds are spent, and the denial text is the instruction - decline the remaining findings with a recorded reason and merge. Two shapes pass the gate, both the attestation law's own. `--verify-fixes` in the invocation flags declares a scoped fix-verification of named findings, which is not a round. And a rebase delta measuring at or over the interdiff budget reviews freely.

## Active skill freshness preflight

Before resolving any argument, run the diagnostic against the active review instructions:

```bash
if FRESHNESS_OUTPUT=$(fno doctor plugin-file "$SKILL_DIR/SKILL.md"); then
  FRESHNESS_EXIT=0
else
  FRESHNESS_EXIT=$?
fi
printf '%s\n' "$FRESHNESS_OUTPUT"
case "$FRESHNESS_OUTPUT" in
  *PLUGIN_FILE_STALE*)
    exit "$FRESHNESS_EXIT"
    ;;
  *PLUGIN_FILE_UNKNOWN*)
    echo "review warning: active skill freshness is unknown; continuing"
    ;;
  *PLUGIN_FILE_FRESH*)
    ;;
  *)
    if [ "$FRESHNESS_EXIT" -ne 0 ]; then
      echo "review warning: freshness diagnostic unavailable (exit $FRESHNESS_EXIT); continuing"
    else
      echo "review refused: active skill freshness diagnostic returned no recognized marker"
      exit 4
    fi
    ;;
esac
```

## Step 1: Resolve the mode (ALWAYS announce it)

The grammar is `[level] [--comment] [--fix] [<pr#>|<branch>|<path>]`, with `prove-it`, `cleanup`, `peer`, `research`, and `declare` as leading mode tokens. A flag is a token in any accepted spelling of a known flag name: `--comment`, bare `comment`, or the em-dash `—comment` (what a phone autocorrects the double hyphen into), and the same three spellings for `fix`. The accepted set is the `canonical_flag` vocabulary in `cli/src/fno/review/invocation.py` - the one list both this router and the invocation telemetry read, so a spelling accepted here is also recorded there, and no fourth spelling is invented. Strip the flags BEFORE reading the first remaining token; the order is load-bearing, because stripping after the target test hands the flag tokens to the target slot, where they resolve to nothing and the run dies having posted nothing. Then read the first remaining token:

- **no argument** -> mode is the default lane at a level sized from the diff. Print exactly: `running fno review lane (default, level from diff)` and continue to Step 2.
- **a level token** (`low` `medium` `high` `xhigh` `max`) -> mode is the default lane at that explicit level. Print `running fno review lane (level <token>)`. Any remaining tokens are the target. Continue to Step 2.
- **`ultra`** -> REFUSE. Print exactly `refused: ultra is billed separately and no fno surface issues it; use max` and stop with a non-zero result. Run nothing, emit nothing.
- **`sigma`** -> REFUSE, naming the replacement. Print exactly `refused: sigma is retired; the default review lane replaced it - run /fno:review [level] [<target>] (or /fno:review peer for a cross-model read)` and stop with a non-zero result. Run nothing, dispatch nothing.
- **`prove-it`** -> mode is `prove-it`, runtime evidence. Print `running prove-it (runtime evidence)`. The remaining tokens, if any, are the target. Continue to Step 2p.
- **`cleanup`** -> mode is `cleanup`, the apply-or-skip pass. Print `running cleanup (apply-or-skip)`. The remaining tokens, if any, are the target. Continue to Step 2c.
- **`peer`** -> mode is `peer`. Print `running peer review (cross-model)`. The remaining tokens are peer's own arguments (`[PR#|branch] [codex|gemini]`). Continue to Step 3.
- **`research`** -> mode is `research`. Print `running research-verify (advisory)`. The remaining tokens, if any, are the brief path. Continue to Step 4.
- **`declare`** -> mode is `declare`. Print `emitting self-cert attestation (declare)`. Continue to Step 5.
- **any other non-empty token** -> one test decides between target and mode. If the token is all digits, it is a PR number - a target, not a mode. Else if `git rev-parse --verify --quiet <token>` succeeds, it is a branch target. Else if it contains a `/`, `.`, or ends in a recognized file suffix, treat it as a path target. Anything else is an unknown mode. Do NOT default, do NOT guess. Print:

  ```
  unknown review mode: '<token>'
  valid modes: prove-it, cleanup, peer, research, declare (bare = the fno review lane; or lead with a level: low medium high xhigh max)
  ```

  and stop with a non-zero result (emit no review, dispatch no agents). This is the locked router contract: an unknown non-empty mode never silently falls through to a default. A number, a resolvable ref, or a path-like token is a TARGET, never a mode.

- **a target-slot token that resolves to nothing but matches a known flag name** -> REFUSE loudly. If the token survives the flag strip and then resolves to no PR, no branch, and no path, print exactly `refused: '<token>' looks like a misspelled flag; accepted spellings are --comment, bare comment, or em-dash —comment (same three for fix)` and stop with a non-zero result. A flag that reached the target slot unstripped must never be silently absorbed as a target; the silent absorption is what made the misspelling invisible.

> A level is never inherited from a previous invocation: an explicit token records `explicit`, a bare invocation sizes from the diff, and no run reuses a typed level (that upstream behavior is a hazard, not a feature).

## Step 2: the default mode (the fno review lane)

### 2a. Empty-diff guard (before anything runs)

If there is nothing to review, report it and exit cleanly - never review an empty diff:

```bash
BASE="${BASE:-origin/main}"
git fetch -q origin 2>/dev/null || true
# Only fire the guard when we are CONFIDENT the tree is empty: no staged or
# unstaged changes AND a resolvable base shows zero commits ahead. If BASE
# does not resolve (no origin remote, non-main default branch), do NOT
# short-circuit to "empty" - fall through to the lane's Phase 0, which
# resolves the diff itself and reports emptiness from there. This avoids
# silently skipping a review of committed work when the base ref is just
# unknown here.
if git diff --quiet 2>/dev/null && git diff --cached --quiet 2>/dev/null \
   && git rev-parse --verify --quiet "$BASE" >/dev/null 2>&1 \
   && [ -z "$(git log "$BASE"..HEAD --oneline 2>/dev/null)" ]; then
  echo "no changes to review"
  exit 0
fi
```

(If `origin/main` is not the right base for this repo, set `BASE` accordingly.)

### 2b. Run the lane

Load [single-lane.md](references/single-lane.md) and execute it in full, in this context, with the resolved level, flags, and target. The lane runs inline as ordinary tool calls: mechanical checks, finder angles, dedup, three-state verify, cite-or-drop, carry-forward, then the classified emit. It dispatches ZERO review subagents and fires no native review verb; the panel that used to run here is retired, and the six specialist hunter agents remain individually invocable by their own names.

## Step 2p: prove-it mode (runtime evidence)

The second thing you run after a code review: a different CLASS of evidence, with a stopping condition. Load [prove-it.md](references/prove-it.md) and execute it in full, in this context: drive the changed code at its real surface, push on it with at least one marked probe, capture the artifact's own output, and end with the terminal `fno-prove-it:` JSON line. Then validate the record:

```bash
bash "${SKILL_DIR}/scripts/validate-prove-it.sh" <report-file>
```

The validator REFUSES a PASS whose Steps list carries no marked probe - a happy-path replay is not a verification - and passes FAIL, BLOCKED, and SKIP through untouched (they carry no verdict on the change). prove-it emits no attestation of its own; a PASS satisfies a declared `done_probe`, a FAIL is a blocking finding, and BLOCKED/SKIP read as unanswered.

## Step 2c: cleanup mode (apply-or-skip terminus)

Load [cleanup.md](references/cleanup.md) and execute it in full, in this context: four cleanup angles (Reuse, Simplification, Efficiency, Altitude) over the diff, each applied or skipped ONCE, each skip recorded with its reason. No verify pass, no threads, no re-review, and no attestation - a cleanup run writes no `review_attestation` row and clears no gate. It is fno's own pass, so it runs inline on every harness; an "unavailable on this harness" outcome is not one this mode can produce.

## Step 3: peer mode (cross-model second opinion)

Load [peer.md](references/peer.md) and execute it in full, in this context. That reference is the canonical cross-model peer-review process. It assembles the diff, spawns `codex` or `gemini` via `fno agents spawn --once` (the agent is the runner), and relays the findings honestly.

Adversarial sub-mode: an `adversarial` token anywhere in peer's args swaps the brief from defect-hunting to a design-challenge framing (is this the right approach, what assumptions does it depend on, where does it fail under real-world conditions, what are the tradeoffs), steerable by trailing focus text. It is parsed inside peer's own args (like `[PR#|branch] [provider]`), not a new top-level router mode, and stays advisory - it gates only with `--post`, same as the defect brief.

The peer review is advisory by default.
With `--attest`, it validates an explicit structured verdict and satisfies the identity-free `config.review.peers` gate at the current HEAD.
With legacy `--post`, it posts under a configured peer identity and satisfies that GitHub-login gate.

## Step 4: research mode (advisory research-verify panel)

Load [research-verify.md](references/research-verify.md) and execute it in full, in this context. That reference is the canonical research-verify process: it dispatches four claim-shaped reviewers (fact-checker / citation-auditor / contradiction-finder / completeness-critic) over a `doc` deliverable (the brief + its `sources.jsonl` sidecar) via the **Task/Agent tool**, never by invoking another skill at runtime.

The research-verify panel is **advisory**: the green/red verdict on a research brief is mechanical and belongs to `fno doctor evals grade` (zero uncited claims, zero dead URLs, ≥1 golden checklist item per section). This panel annotates the brief; it never blocks, flips, or substitutes for the eval.

## Step 5: declare mode (self-cert attestation, the escape hatch)

`declare` is the bottom of the `config.review.reviewers` trust spectrum (cross-model peer > the fno lane > **declare**): a pure operator self-certification for a harness that has no other reviewer. It emits a head-pinned `review_attestation` event so a `reviewers: [declare]` gate can clear, and does nothing else. A declare attestation clears no fail-with-CONFIRMED-findings state; the class gate bounds it, which is why it survives at all.

Because it gives up "different model" entirely, it must be an **explicit** action - never inferred, never auto-emitted by any pipeline. State plainly what you are certifying (the current HEAD + the diff under review), then emit:

```bash
bash "${SKILL_DIR}/scripts/emit-attestation.sh" declare
```

The event is pinned to the current HEAD; if a new commit lands afterward, the declaration no longer counts and must be re-run. loop-check reads the event as gate evidence but never runs a reviewer or emits an attestation itself.

## The attestation surface (config.review.reviewers producers)

`peer`, `code-review`, and `declare` are local review producers. Each emits the SAME head-pinned `review_attestation` event via `scripts/emit-attestation.sh <reviewer>` on a pass - the single producer surface loop-check reads:

- **the fno lane** (the default) emits `code-review` through the lane's emit step ([single-lane.md](references/single-lane.md)): `pass` only when `fno do review classify` yields zero blocking findings, `fail` carrying the classified record. One contract, one emit path, no hook-availability dependency.
- **peer** emits `peer` only after `consume-peer-verdict.sh` validates an explicit clean cross-model verdict with zero blocking findings.
- **code-review** is the gate entry the fno lane satisfies above. The native verb path remains for an operator who runs it by choice: `hooks/code-review-attest.sh` classifies the findings the native review produced and emits on EITHER outcome, with the dual Claude (`PostToolUse(ReportFindings)` / Skill-tool `SubagentStop`) and Codex (`Stop` payload with a readable structured completion) trigger shapes. `skills/review/scripts/emit-attestation.sh code-review` is recovery when the lane and the hook were both unavailable, never permission to attest a verdict a review did not produce. A spawned reviewer citizen or a separate operator session can emit the same label, yielding an `other_session` origin rather than `self_attested`; see the spawned-reviewer lane in the review-lanes architecture doc.
- **declare** emits `declare` via Step 5 above. `sigma` is retired and emits nothing: a config still naming it fails loud at init with the default lane named as the replacement.

Head-pinning is mandatory: the helper stamps `git rev-parse HEAD`, and loop-check only counts an attestation whose `head_sha` equals the current HEAD (a pass on a superseded commit is discarded). Absence holds the gate (fail closed).

**Termination (the round budget).** A blocking finding is cleared by fixing it. The next review covers the fix delta. Nothing else clears it on your own signature. A non-blocking finding needs no action to clear the gate. Answer it in thread or skip it. Note the skip rather than arguing with it. When the gate reports IMPOSSIBLE, stop. Do not request another review. At the round cap without a hard finding the gate FILES the remainder and the PR merges, so a fourth round is never the answer. Report the blocking findings and the two remedies to the operator. The remedies are a non-author GitHub approval on the PR, or the coverage-override label. A refused gate escalates.

## The manual coverage emit (sanctioned, with preconditions)

A PR whose session ended still has a certification path: `fno doctor event emit -t review_coverage -d '{...}'` is a sanctioned surface, not a loophole, and the merge-path reader accepts its row. The emit reaches BOTH the project log and the machine-global log (the type rides `GLOBAL_MIRROR_TYPES`), so a row emitted inside a worktree whose journal is not linked to canonical is still found by a canonical merge. Two preconditions keep it honest: `head_sha` in the payload must equal the PR's live `headRefOid` (a row pinned to a stale head is refused by the gate's staleness conjunct, not honored), and a required `code-review` reviewer still needs its own `local_attestation` verdict entry - the coverage row records the state of the world, it does not substitute for the review a configured reviewer owes. Nobody may read this verb as a way to hand-clear the gate.


Each reviewer also declares what it NEEDS in order to run - `code-review` needs only the plugin itself (the lane runs inline on every harness), `declare` needs nothing - in `_RESOLVABLE_REVIEWERS` (`cli/src/fno/config/__init__.py`). `fno do target init` resolves that against the running session and refuses a gate nothing here can satisfy; `fno config doctor --review` reports the same read-only. A reviewer that cannot run is never quietly swapped for `declare`: that would clear the gate with no review behind it.

## Your skill can be someone's ship gate

A project registers its own reviewer under `[review.reviewer_registry.<name>]` in `config.toml` and then names it in `config.review.reviewers`, at which point the loop refuses `DonePRGreen` until that reviewer has attested at the current HEAD.
Two rungs are available to a third-party skill or plugin author, and the difference between them is what the gate is allowed to claim.

```toml
[review]
reviewers = ["/my-security-skill"]

[review.reviewer_registry.my-security-skill]
kind = "harness-skill"
requires = "skill"          # resolved against the harness's skill roots at init
invocation = "/my-security-skill"
asserts = "invocation"
```

**Rung one: emit no findings.** `asserts = "invocation"` proves your skill ran at the reviewed commit and claims nothing about its verdict.
That is a forcing function, not a review.
footnote does not parse skill output and will not pretend otherwise: a per-skill output contract it cannot enforce would fail by misreading a "PASS" and clearing a real gate.

The attestation itself is still emitted - "emit nothing" means your skill reports no *findings*, not that the gate clears by itself.
Nothing witnesses an invocation implicitly.
The `/target` session runs your `invocation` and then `bash skills/review/scripts/emit-attestation.sh <name>` on the final HEAD, exactly as it does for `code-review` ([ship-and-promise.md](../target/references/ship-and-promise.md)).
That helper takes any reviewer name, which is why a registered reviewer needs no producer machinery of its own.

**Rung two: emit your findings.** Call `fno backlog annotate add -m "<finding>" --node <id>` and the gate becomes real.
An unaddressed blocking finding holds the loop until someone resolves it, independently of any attestation, and it needs no new footnote machinery on your side.

`requires = "skill"` is checked at `fno do target init`: a skill that resolves on none of the harness's skill roots refuses there, naming the roots searched, rather than wedging the stop gate after the work is done.
When the probe cannot answer - a harness footnote does not know, an unreadable root, a `plugin:skill` qualified name whose cache layout footnote does not read - it resolves `unverifiable` and proceeds with one note, because refusing a session over a reviewer that is actually installed is the worse failure.

A registered reviewer never enters `_RESOLVABLE_REVIEWERS`; the two are unioned at lookup time, and a built-in wins a name collision, so no project can redefine `sigma` into something weaker.

For a guardrail that has an exit code, skip all of this and use a probe: top-level `done_probes` in `config.toml` is run by loop-check itself (60s each, cap 3 per source) and is the strongest rung available, because footnote verifies it rather than witnessing it.
Both the project's list and any a plan declares must pass; a plan can add probes and can never silence the project's.
A probe is an *observation* - one that mutates the repo races the session's own edits.

## Known Limitations and Deferred Work

- Green CI does not prove reviewer coverage. See [LIMITATIONS.md](LIMITATIONS.md).

## Multi-CLI

Claude-Code primary. All modes need `fno` and `gh`/`git`; peer mode additionally needs the `fno agents` daemon for the `codex`/`gemini` one-shot lane, and research mode needs the Task/Agent tool to dispatch its roster. If a dependency is missing, the mode fails loud and reports it - it never fakes a review.
