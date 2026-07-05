---
name: review
description: "Review a diff or a research brief. Routes to the internal six-agent Claude panel (sigma, default), a cross-model second opinion (peer), the advisory research-verify panel for a doc deliverable (research), or a self-cert attestation for the config.review.reviewers gate (declare). Use when: 'review this', 'code review', 'is this ready', 'get a second opinion', 'have codex review this PR', 'review this research brief', 'declare this reviewed'."
argument-hint: "[sigma|peer|research|declare]  (peer: [PR#|branch] [codex|gemini]; research: [brief.md])   e.g. (bare = sigma), `peer 657 codex`, `research out/topic.md`, `declare`"
requires:
  binaries:
    - "fno >= 0.1"
    - "gh >= 2.0"
    - "git >= 2.0"
---

# Review

**One verb on a diff.** `/review` routes to the right reviewer set for the diff in front of you.

| Mode | What runs | Shared object |
|------|-----------|---------------|
| `sigma` (default) | internal six-agent Claude review panel | the diff |
| `peer` | a cross-model second opinion (`codex` / `gemini`) on your coding-account quota | the diff |
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

The peer review is **advisory**: it runs on a coding-account quota, not the bot account, and never satisfies a `required_bots` review gate. A human still merges.

## Step 4: research mode (advisory research-verify panel)

Load [research-verify.md](references/research-verify.md) and execute it in full, in this context. That reference is the canonical research-verify process: it dispatches four claim-shaped reviewers (fact-checker / citation-auditor / contradiction-finder / completeness-critic) over a `doc` deliverable (the brief + its `sources.jsonl` sidecar) via the **Task/Agent tool**, never by invoking another skill at runtime.

The research-verify panel is **advisory**: the green/red verdict on a research brief is mechanical and belongs to `fno evals grade` (zero uncited claims, zero dead URLs, ≥1 golden checklist item per section). This panel annotates the brief; it never blocks, flips, or substitutes for the eval.

## Step 5: declare mode (self-cert attestation, the escape hatch)

`declare` is the bottom of the `config.review.reviewers` trust spectrum (sigma > cross-model /code-review > same-model /code-review > **declare**): a pure operator self-certification for a harness that has no other reviewer. It emits a head-pinned `review_attestation` event so a `reviewers: [declare]` gate can clear, and does nothing else.

Because it gives up "different model" entirely, it must be an **explicit** action - never inferred, never auto-emitted by any pipeline. State plainly what you are certifying (the current HEAD + the diff under review), then emit:

```bash
bash "${SKILL_DIR}/scripts/emit-attestation.sh" declare
```

The event is pinned to the current HEAD; if a new commit lands afterward, the declaration no longer counts and must be re-run. loop-check reads the event as gate evidence but never runs a reviewer or emits an attestation itself.

## The attestation surface (config.review.reviewers producers)

`sigma`, `code-review`, and `declare` are the local reviewers that satisfy a `config.review.reviewers` entry. Each emits the SAME head-pinned `review_attestation` event via `scripts/emit-attestation.sh <reviewer>` on a pass - the single producer surface loop-check reads:

- **sigma** emits `sigma` when the panel returns with no unaddressed blocking finding (see [sigma.md](references/sigma.md)).
- **code-review** emits `code-review` when a `/code-review` pass completes on the current HEAD (the operator runs the helper after the pass).
- **declare** emits `declare` via Step 5 above.

Head-pinning is mandatory: the helper stamps `git rev-parse HEAD`, and loop-check only counts an attestation whose `head_sha` equals the current HEAD (a pass on a superseded commit is discarded). Absence holds the gate (fail closed).

## Multi-CLI

Claude-Code primary. All modes need `fno` and `gh`/`git`; peer mode additionally needs the `fno agents` daemon for the `codex`/`gemini` one-shot lane, and research mode needs the Task/Agent tool to dispatch its roster. If a dependency is missing, the mode fails loud and reports it - it never fakes a review.
