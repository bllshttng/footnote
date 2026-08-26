<!-- style-exception: the angle bodies, the low pass, the three-state verdict definitions, and both anti-noise rules are verbatim extractions from the /code-review skill spec. Rewriting verbatim text to satisfy sentence-length rules would break the provenance contract that makes these definitions the lane's vocabulary. -->

# The fno review lane

The owned reviewer. One context walks every angle, then verifies, as ordinary tool calls in the current session. It never dispatches subagents, never fires a native review verb, a pane, or the mail bus, and it satisfies the same `code-review` gate entry the native verb satisfies.

Why inline, always: a fan-out needs a provider that permits it, and a native verb needs a transport that can fail in each measured way this lane replaces. A single context has neither dependency. Inline is the primary shape, not a fallback.

Inputs from the router: a level (`low` `medium` `high` `xhigh` `max`), optional flags (`--comment`, `--fix`), and an optional target (PR number, branch, or path).

## Phase 0 - gather the diff

Run `git diff @{upstream}...HEAD` (or `git diff main...HEAD` / `git diff HEAD~1` if there's no upstream) to get the unified diff under review. If there are uncommitted changes, or the range diff is empty, also run `git diff HEAD` and include the working-tree changes in scope - the review often runs before the commit. If a PR number, branch name, or file path was passed as an argument, review that target instead. Treat this diff as the review scope.

Pin the reviewed head now: `REVIEWED_HEAD=$(git rev-parse HEAD)`. Do not resolve it again after the pass runs.

## Phase 1 - find candidates

Work every angle in this same context, in one pass - do not skip angles for lack of fan-out, and do not spawn subagents. Each angle surfaces candidates with `file`, `line`, a one-line `summary`, and a concrete `failure_scenario`.

| level | angles | cap | bias |
|-------|--------|-----|------|
| low | one careful pass (below) | 4 | precision |
| medium | A B C Reuse Simplification Efficiency Altitude Conventions | 8 | precision |
| high | the 8-angle set | 10 | recall |
| xhigh | the 10-angle set, adds D and E | 15 | recall |
| max | the 10-angle set, adds D and E | 15 | recall |

### The low pass

One tool call: read the unified diff from Phase 0. No full-file reads. Flag runtime-correctness bugs visible from the hunk alone: inverted/wrong condition, off-by-one, null/undefined deref where adjacent lines show the value can be absent, removed guard, falsy-zero check, missing `await`, wrong-variable copy-paste, error swallowed in a catch that should propagate. Also flag, still from the hunk alone, new code that duplicates an existing helper visible in the diff context, and dead code the diff leaves behind. Do not flag style, naming, perf, missing tests, or anything outside the hunk.

### Angle A - line-by-line diff scan

Read every hunk in the diff, line by line. Then Read the enclosing function for each hunk - bugs in unchanged lines of a touched function are in scope (the PR re-exposes or fails to fix them). For every line ask: what input, state, timing, or platform makes this line wrong? Look for inverted/wrong conditions, off-by-one, null/undefined deref, missing `await`, falsy-zero checks, wrong-variable copy-paste, error swallowed in catch, unescaped regex metachars.

### Angle B - removed-behavior auditor

For every line the diff DELETES or replaces, name the invariant or behavior it enforced, then search the new code for where that invariant is re-established. If you can't find it, that's a candidate: a removed guard, a dropped error path, a narrowed validation, a deleted test that was covering a real case.

### Angle C - cross-file tracer

For each function the diff changes, find its callers (Grep for the symbol) and check whether the change breaks any call site: a new precondition, a changed return shape, a new exception, a timing/ordering dependency. Also check callees: does a parallel change in the same PR make a call unsafe?

### Angle D - language-pitfall specialist

Scan for the classic pitfalls of the diff's language/framework - for example: JS falsy-zero, `==` coercion, closure-captured loop var; Python mutable default args, late-binding closures; Go nil-map write, range-var capture; SQL injection; timezone/DST drift; float equality. Flag any instance the diff introduces.

### Angle E - wrapper/proxy correctness

When the PR adds or modifies a type that wraps another (cache, proxy, decorator, adapter): check that every method routes to the wrapped instance and not back through a registry/session/global - e.g. a caching provider holding a `delegate` field that resolves IDs via `session.get(...)` instead of `delegate.get(...)` will re-enter the cache or recurse. Also check that the wrapper forwards all the methods the callers actually use.

### Reuse

The angles above hunt for bugs; this one and the next two hunt for cleanup in the changed code. Flag new code that re-implements something the codebase already has - Grep shared/utility modules and files adjacent to the change, and name the existing helper to call instead.

### Simplification

Flag unnecessary complexity the diff adds: redundant or derivable state, copy-paste with slight variation, deep nesting, dead code left behind. Name the simpler form that does the same job.

### Efficiency

Flag wasted work the diff introduces: redundant computation or repeated I/O, independent operations run sequentially, blocking work added to startup or hot paths. Also flag long-lived objects built from closures or captured environments - they keep the entire enclosing scope alive for the object's lifetime (a memory leak when that scope holds large values); prefer a class/struct that copies only the fields it needs. Name the cheaper alternative.

### Altitude

Check that each change is implemented at the right depth, not as a fragile bandaid. Special cases layered on shared infrastructure are a sign the fix isn't deep enough - prefer generalizing the underlying mechanism over adding special cases.

### Conventions (CLAUDE.md)

Find the CLAUDE.md files that govern the changed code: the user-level ~/.claude/CLAUDE.md, the repo-root CLAUDE.md, plus any CLAUDE.md or CLAUDE.local.md in a directory that is an ancestor of a changed file (a directory's CLAUDE.md only applies to files at or below it). Read each one that exists, then check the diff for clear violations of the rules they state. Only flag a violation when you can quote the exact rule and the exact line that breaks it - no style preferences, no vague "spirit of the doc" inferences. In the finding, name the CLAUDE.md path and quote the rule so the report can cite it. If no CLAUDE.md applies, return nothing for this angle.

Cleanup, altitude, and conventions candidates use the same `file`/`line`/`summary` shape; in `failure_scenario`, state the concrete cost (what is duplicated, wasted, harder to maintain, or which CLAUDE.md rule is broken) instead of a crash. Correctness bugs always outrank cleanup, altitude, and conventions findings when the output cap forces a cut.

Pass every candidate with a nameable failure scenario through - finders that silently drop half-believed candidates bypass the verify step and are the dominant cause of misses.

## Phase 2 - dedup then verify

Dedup candidates that point at the same line/mechanism, keeping the one with the most concrete failure scenario. Then verify each survivor yourself, in this context, assigning exactly one of:

- **CONFIRMED** - can name the inputs/state that trigger it and the wrong output or crash. Quote the line.
- **PLAUSIBLE** - mechanism is real, trigger is uncertain (timing, env, config). State what would confirm it.
- **REFUTED** - factually wrong (code doesn't say that) or guarded elsewhere. Quote the line that proves it.

**PLAUSIBLE by default** - do not refute a candidate for being "speculative" or "depends on runtime state" when the state is realistic: concurrency races, nil/undefined on a rare-but-reachable path (error handler, cold cache, missing optional field), falsy-zero treated as missing, off-by-one on a boundary the code does not exclude, retry storms / partial failures, regex/allowlist that lost an anchor. These are PLAUSIBLE.

**REFUTED** only when constructible from the code: factually wrong (quote the actual line); provably impossible (type/constant/invariant - show it); already handled in this diff (cite the guard); or pure style with no observable effect.

Keep CONFIRMED and PLAUSIBLE. Drop REFUTED from the findings array, and record each REFUTED verdict in the report with its proving line quoted (the exact format is in Report and emit). A REFUTED verdict that cannot quote a line that exists in the cited file is not REFUTED: the candidate stays PLAUSIBLE and enters the findings array.

### Cite-or-drop (the mechanical backstop)

For every kept finding, open the cited `file:line` and check the finding's verbatim quote against the actual file content, not plausibility. A finding whose quote is missing from the cited location, does not match the file, or does not support the claim is unverifiable: it scores in the 0-25 abstain band, is filtered below the 80 reporting threshold, and is named in the report as `dropped-unverifiable` rather than omitted silently. When uncertain whether the quote supports the claim, prefer the abstain band over guessing high - a dropped uncertain finding is correct; a confidently wrong one is not.

### Carry-forward (prior rounds)

An incremental round narrows scope only to the increment since the last reviewed head, and the narrowing is earned, never assumed. Resolve the prior head through the existing accessor and verify it before trusting it:

```bash
PRIOR_HEAD=$(fno do review --sigma-last-head --sigma-node "$NODE_ID" --sigma-pr "$PR_NUMBER" 2>/dev/null || true)
SCOPE="first-round"
if [ -n "$PRIOR_HEAD" ] && git merge-base --is-ancestor "$PRIOR_HEAD" HEAD 2>/dev/null; then
  if git diff --name-only "$PRIOR_HEAD..HEAD" | grep -qE '(^|/)(CLAUDE\.md|AGENTS\.md)$|^\.claude/rules/'; then
    SCOPE="first-round"    # rules changed: a new rule can condemn cleared code, so full scope
  else
    SCOPE="incremental"
  fi
fi
```

A prior head that is not an ancestor of HEAD (rebase, squash, force-push) reads as `first-round`: full scope. An empty increment resets to full scope the same way, so a zero-file round can never pass vacuously. The single changed-files producer is this block; no other diff read in the pass names its own base.

On an incremental round, read the prior round's report through the existing inspect surface and keep its exit status:

```bash
if PRIOR_REPORT=$(fno do review --inspect-sigma --sigma-node "$NODE_ID" --sigma-pr "$PR_NUMBER" --sigma-head "$PRIOR_HEAD" --json 2>/dev/null); then
  INSPECT=ok
else
  INSPECT=failed
fi
```

With `INSPECT=ok` and no critical or high findings in the prior body, there is nothing to carry. With `INSPECT=failed`, the prior report was not readable and the narrowed scope is unproven: re-run this round at full scope before any verdict, and emit no attestation from the incomplete round. Never read a failed inspection as an empty prior report - a live blocking finding can sit in the unread report.

For each critical or high finding in a readable prior body, re-validate its cited quote at the CURRENT head, the same cite-or-drop check as above:

- quote still matches at the cited `file:line` -> the finding is unresolved. Carry it verbatim into this round's report tagged `carried from round <id>`. It counts as a blocking finding of THIS round.
- quote is gone or no longer supports the claim -> it lands in the 0-25 abstain band, drops below the 80 threshold, and is named `dropped-unverifiable`.

While any carried finding is unresolved, the verdict cannot be pass.

## Phase 3 - sweep for gaps (xhigh and max only)

Take one more pass, same context, as a fresh reviewer who has the verified list. Re-read the diff and enclosing functions looking ONLY for defects not already listed. Do not re-derive or re-confirm anything already there - the job is gaps. Focus on what the first pass tends to miss: moved/extracted code that dropped a guard or anchor; second-tier footguns (dataclass default evaluated once, `hash()` non-determinism, lock-scope shrink, predicate methods with side effects); setup/teardown asymmetry in tests; config defaults flipped.

Surface up to 8 additional candidates, each naming a defect not already on the list. Verify each through Phase 2. If nothing new, the sweep is empty - do not pad.

## Report and emit

The report opens with one scope line and ends with one verdict line. The scope line is `Scope: first-round` on a full review, `Scope: incremental` on a narrowed round, or `Scope: full-scope (prior report unreadable; inspect failed)` after a failed inspection forced the wider scope. The verdict line is `Verdict: pass` or `Verdict: fail`, and it mirrors the classifier: pass only when the blocking count is zero and no carried finding is unresolved.

The resolved route line comes from the one seam: `fno do review resolve-level <level>` prints the full record (level, level_source, band, effort, model, provider, degraded_max), and the report carries it verbatim so a reader never has to reconstruct what ran.

Between them the report carries, in order: the resolved route line (level, band, effort, model, provider), the findings ranked most-severe first, the REFUTED section, the dropped-unverifiable names, and any carried findings with their tags.

Each REFUTED entry is one line in the exact shape:

```
- <file>:<line> REFUTED <summary> - proving line: "<verbatim quote>"
```

The findings payload is a fenced JSON array, at most the level's cap, ranked most-severe first:

```json
[
  {
    "file": "path/to/file",
    "line": 123,
    "summary": "one-sentence statement of the defect",
    "short_summary": "the claim alone at 60 characters or less",
    "failure_scenario": "concrete inputs/state -> wrong output/crash",
    "category": "correctness",
    "verdict": "CONFIRMED"
  }
]
```

`short_summary` is the claim alone, no rationale or consequence clause. `category` is a short kebab-case slug for the angle that produced it (`correctness`, `simplification`, `efficiency`, `reuse`, `altitude`, `conventions`, or a more specific slug like `test-coverage`). `verdict` is CONFIRMED or PLAUSIBLE; a REFUTED candidate never enters the array. A carried finding enters the array with the verdict its re-validation produced.

Write the array to a temp file, classify it, and emit through the shared producer surface. The lane never counts its own findings for the emit decision - the classifier's blocking count decides:

```bash
CLASSIFIED=$(fno do review classify --findings-file "$FINDINGS" --emit-record)
BLOCKING=$(jq -r '.findings_blocking' <<<"$CLASSIFIED")
if [ "$BLOCKING" = "0" ]; then
  bash "${SKILL_DIR}/scripts/emit-attestation.sh" code-review pass --findings-file "$FINDINGS"
else
  bash "${SKILL_DIR}/scripts/emit-attestation.sh" code-review fail --findings-file "$FINDINGS"
fi
```

## Flags

`--comment`: when the target is a GitHub PR, post each finding as an inline PR comment (`gh api repos/{owner}/{repo}/pulls/<n>/comments`, one call per finding, a suggestion block only when it fully fixes the issue). When the target is not a PR, print the findings and note the flag was ignored.

`--fix`: apply the findings to the working tree after the report: fix each one directly - correctness bugs and reuse/simplification/efficiency cleanups alike. Skip any finding whose fix would change intended behavior, require changes well outside the reviewed diff, or that you judge to be a false positive - note the skip rather than arguing with it. Then emit on the NEW head after the fix commit: a pass on a superseded commit is discarded, so the attestation must name the head the fixes landed on. Verify the fix delta first.
