---
name: review
description: "Review a diff or a research brief. Routes to the internal multi-agent sigma panel (default), a cross-model second opinion (peer), the advisory research-verify panel for a doc deliverable (research), or a self-cert attestation for the config.review.reviewers gate (declare). Use when: 'review this', 'code review', 'is this ready', 'get a second opinion', 'have codex review this PR', 'review this research brief', 'declare this reviewed'."
argument-hint: "[sigma|peer|research|declare]  (peer: [adversarial] [--attest|--post] [PR#|branch] [codex|gemini] [focus...]; research: [brief.md])   e.g. (bare = sigma), `peer 657 codex --attest`, `peer adversarial codex`, `research out/topic.md`, `declare`"
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
| `sigma` (default) | internal multi-agent review panel with observed runtime attribution | the diff |
| `peer` | a cross-model second opinion, optionally producing a verdict-gated local attestation | the diff |
| `research` | advisory research-verify panel (fact-checker / citation-auditor / contradiction-finder / completeness-critic) | a `doc` deliverable (brief + sources sidecar) |

This is a **router**, not a monolith. It parses the first argument as a mode, announces the resolved mode, then loads that mode's reference and follows it in this same context. It never calls another skill at runtime (it dispatches review subagents via the Task/Agent tool and loads modes via Read).

## Step 1: Resolve the mode (ALWAYS announce it)

Parse the first argument token:

- **no argument** -> mode is `sigma`. Print exactly: `running sigma (default)` and continue to Step 2.
- **`sigma`** -> mode is `sigma`. Print `running sigma`. The remaining tokens, if any, are ignored by sigma (it auto-detects local commits vs PR context). Continue to Step 2.
- **`peer`** -> mode is `peer`. Print `running peer review (cross-model)`. The remaining tokens are peer's own arguments (`[PR#|branch] [codex|gemini]`). Continue to Step 3.
- **`research`** -> mode is `research`. Print `running research-verify (advisory)`. The remaining tokens, if any, are the brief path. Continue to Step 4.
- **`declare`** -> mode is `declare`. Print `emitting self-cert attestation (declare)`. Continue to Step 5.
- **any other non-empty token** -> this is an unknown mode. Do NOT default, do NOT guess. Print:

  ```
  unknown review mode: '<token>'
  valid modes: sigma (default), peer, research, declare
  ```

  and stop with a non-zero result (emit no review, dispatch no agents). This is the locked router contract: an unknown non-empty mode never silently falls through to a default.

> Note: a PR number or branch is NOT a bare `/review` argument. To review PR 657 with the internal panel, run `/review sigma` from a checkout of that branch (sigma auto-detects PR context); to get a cross-model read on PR 657, run `/review peer 657`.

## Step 2: sigma mode (internal six-agent panel)

### 2a. Empty-diff guard (before any dispatch)

If there is nothing to review, report it and exit cleanly - never dispatch agents against an empty diff:

```bash
BASE="${BASE:-origin/main}"
git fetch -q origin 2>/dev/null || true
# Only fire the guard when we are CONFIDENT the tree is empty: no staged or
# unstaged changes AND a resolvable base shows zero commits ahead. If BASE
# does not resolve (no origin remote, non-main default branch), do NOT
# short-circuit to "empty" - fall through to sigma.md, which resolves the
# diff itself and reports emptiness from there. This avoids silently skipping
# a review of committed work when the base ref is just unknown here.
if git diff --quiet 2>/dev/null && git diff --cached --quiet 2>/dev/null \
   && git rev-parse --verify --quiet "$BASE" >/dev/null 2>&1 \
   && [ -z "$(git log "$BASE"..HEAD --oneline 2>/dev/null)" ]; then
  echo "no changes to review"
  exit 0
fi
```

(If `origin/main` is not the right base for this repo, set `BASE` accordingly.)

### 2b. Run the panel

Load [sigma.md](references/sigma.md) and execute it in full, in this context. That reference is the canonical six-agent review process. It dispatches the reviewer subagents via the **Task/Agent tool**, never by invoking another skill at runtime.

### 2c. Agent-failure transparency (do not silently drop a dead reviewer)

The panel dispatches multiple subagents in parallel. If one of them fails to return (dies, errors, or times out):

- **report the surviving agents' findings** - a single dead reviewer does not void the review.
- **name the failed agent explicitly** in the report under a `## Reviewers that failed` line (agent name + the failure reason).

Never present a partial panel as a complete one, and never omit a reviewer that did not run.

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

`declare` is the bottom of the `config.review.reviewers` trust spectrum (sigma > cross-model /code-review > same-model /code-review > **declare**): a pure operator self-certification for a harness that has no other reviewer. It emits a head-pinned `review_attestation` event so a `reviewers: [declare]` gate can clear, and does nothing else.

Because it gives up "different model" entirely, it must be an **explicit** action - never inferred, never auto-emitted by any pipeline. State plainly what you are certifying (the current HEAD + the diff under review), then emit:

```bash
bash "${SKILL_DIR}/scripts/emit-attestation.sh" declare
```

The event is pinned to the current HEAD; if a new commit lands afterward, the declaration no longer counts and must be re-run. loop-check reads the event as gate evidence but never runs a reviewer or emits an attestation itself.

## The attestation surface (config.review.reviewers producers)

`sigma`, `peer`, `code-review`, and `declare` are local review producers. Each emits the SAME head-pinned `review_attestation` event via `scripts/emit-attestation.sh <reviewer>` on a pass - the single producer surface loop-check reads:

- **sigma** emits `sigma` when the panel returns with no unaddressed blocking finding (see [sigma.md](references/sigma.md)).
- **peer** emits `peer` only after `consume-peer-verdict.sh` validates an explicit clean cross-model verdict with zero blocking findings.
- **code-review** emits `code-review` on a clean native review via `hooks/code-review-attest.sh`. Claude accepts the structured empty findings report on `PostToolUse(ReportFindings)` and the Skill-tool `SubagentStop` shape. Codex accepts only a `Stop` payload whose exact `turn_id` has one readable `ExitedReviewMode` transcript item with an object-valued `findings: []`; `last_assistant_message` is not verdict evidence. The Codex registration runs before the target stop gate, so a clean `/review` needs no remembered second command. `skills/review/scripts/emit-attestation.sh code-review` is recovery only when a clean review is confirmed but its hook was unavailable or failed, never permission to attest over findings. The same label can be emitted by a spawned reviewer citizen, a separate session the author launches to run `/code-review` in its own worktree, which is the one path that yields an `other_session` origin rather than `self_attested`; see the spawned-reviewer lane in the review-lanes architecture doc.
- **declare** emits `declare` via Step 5 above.

Head-pinning is mandatory: the helper stamps `git rev-parse HEAD`, and loop-check only counts an attestation whose `head_sha` equals the current HEAD (a pass on a superseded commit is discarded). Absence holds the gate (fail closed).

Each reviewer also declares what it NEEDS in order to run - `sigma` needs subagent dispatch, `code-review` needs an operator, `declare` needs nothing - in `_RESOLVABLE_REVIEWERS` (`cli/src/fno/config/__init__.py`). `fno do target init` resolves that against the running session and refuses a gate nothing here can satisfy; `fno config doctor --review` reports the same read-only. A reviewer that cannot run is never quietly swapped for `declare`: that would clear the gate with no review behind it.

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
