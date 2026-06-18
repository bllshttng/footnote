# Review Pipeline: Confidence Scorer

The code-review pipeline ships a 6-agent panel through `claude -p` subprocess calls. After findings return, they pass through a confidence scorer that drops low-confidence noise before the report is generated.

## Why subprocess, not SDK

The scorer lives in `cli/src/fno/review/scorers/claude_scorer.py` and shells out to `claude -p --output-format json --model claude-haiku-4-5`.

A direct `anthropic.Anthropic()` SDK call would be simpler code, but would require users to set `ANTHROPIC_API_KEY` as a separate environment variable on top of the Claude Code OAuth credentials they already have. Every user who runs `fno review` has Claude Code installed (that's how the review agents themselves get spawned), so the OAuth path in `~/.claude/.credentials.json` / macOS Keychain is always present. Adding a parallel credential requirement for one small task was poor UX.

Using `claude -p` for the scorer also matches the existing `ClaudeCodeAdapter.spawn_worker` pattern for the review panel itself, so there is one code path to reason about for "talk to Claude".

## Batching to amortize spawn cost

`claude -p` has roughly 500 ms of per-call spawn overhead. Scoring 10 findings individually would add 5 s of pure overhead per review, which is visible to the user.

The scorer exposes two entry points:

- `claude_scorer(finding) -> int` - single-finding call, returns 0 on any failure.
- `claude_scorer_batch(findings) -> list[int]` - single subprocess call that asks the model to return a JSON array of integers in the same order as the input.

The batch function carries a `__batch__ = True` attribute. In `score_findings`, the runtime dispatch check is `getattr(resolved, "__batch__", False)`: batch scorers take the one-shot path, per-finding scorers (including user overrides) keep the N-call loop. This preserves the public `Callable[[Finding], int]` contract for user-supplied scorers while letting the default amortize spawn cost.

## Failure semantics

All failure modes in both entry points return a score of 0 (filtered out at the default 80 threshold). They always log a single stderr line so the failure is visible to the operator:

| Failure | Contract |
|---------|----------|
| Subprocess timeout | 0 (single) or `[0, ...]` (batch), log once |
| `claude` binary missing (`FileNotFoundError`) | 0 / zeros, log once |
| Non-zero exit | 0 / zeros, log stderr snippet |
| Outer JSON parse failure | 0 / fallback to per-finding |
| Inner array length mismatch (batch only) | Fall back to per-finding calls |
| Non-numeric entry in batch array (e.g. `null`, `"oops"`, `true`) | 0 for that index, one aggregate log summarizing the offending indices |
| `claude` disappears mid-fallback | Zero out the remaining findings, one aggregate log |

The fallback paths (length mismatch, JSON parse) turn a bad batch reply into per-finding calls rather than losing every score. A systemic failure (missing binary, timeout) returns zeros directly so the review pipeline still emits an artifact with a sensible "nothing confident enough" verdict instead of crashing.

## Resolver selection

`_resolve_default_scorer` in `cli/src/fno/review/confidence_scorer.py` uses `shutil.which("claude")` to decide:

- `claude` on PATH -> return `claude_scorer_batch` (the batch entry point).
- `claude` absent -> return `pass_through_scorer` with a one-shot stderr warning, so the review still produces an artifact (every finding passes through at confidence 100, leaving threshold tuning to the operator).

Users supplying their own scorer via `score_findings(scorer=...)` bypass the resolver entirely; the batch dispatch still works (set `your_scorer.__batch__ = True`).
