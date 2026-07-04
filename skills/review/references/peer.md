
# Peer Review

**A second opinion on a diff from another coding model, in-session.**

`/review sigma` fans your changes out to internal Claude subagents: different
review dimensions, one model. This mode fans a single diff out to a *different
tool* - `codex` or `gemini` - for a genuine cross-model read. It runs the model's
own coding-account quota (a separate lane from the `chatgpt-codex-connector[bot]`
GitHub App), so it still works when that review bot returns "usage limits."

The one thing that makes this skill exist: **you are the runner.** When a human
types `! fno agents spawn ...` at the prompt, the reply lands in a local-command
output file and a caveat fences the agent off, so the findings never reach the
conversation without a manual nudge. When the *agent* runs the same
`fno agents spawn --once`, the synchronous reply returns in the tool result and
gets acted on the same turn. This skill is that pattern, plus diff assembly and
honest reporting.

It does not reimplement spawn, does not touch `/pr check`, and does not gate
anything. It is an on-demand verb. The peer review is **advisory**: it is not a
review from the bot account and never satisfies a `required_bots` gate.

## Inputs

- **target** (optional): a PR number (`657`), a branch name, or nothing.
  - PR number -> review `gh pr diff <N>`.
  - branch name -> review that branch's diff vs base.
  - nothing -> review the current branch vs base (`origin/main` unless overridden).
- **provider** (optional, trailing word): `codex` (default) or `gemini`. `claude`
  is rejected - it has no `--once` one-shot lane (see Hard rules).
- **base** (optional): override the diff base (default `origin/main`).

## Flow: RESOLVE -> BRIEF -> SPAWN (agent is the runner) -> RELAY -> OFFER

### 1. RESOLVE the diff

Pick the target and write the diff to a scratch file. Prefer the background-job
tmp dir when set, else the system temp dir:

```bash
TMP="${CLAUDE_JOB_DIR:-${TMPDIR:-/tmp}}/tmp"; mkdir -p "$TMP" 2>/dev/null || TMP="${TMPDIR:-/tmp}"
DIFF="$TMP/peer-review.diff"

# PR number:
gh pr diff "$N" > "$DIFF"

# OR a branch / current branch vs base. REF is the branch name when the target
# is a branch, else HEAD (the bare / current-branch case) - so a branch target
# reviews THAT branch, not whatever is currently checked out:
BASE="${BASE:-origin/main}"
REF="${BRANCH:-HEAD}"
git fetch -q origin 2>/dev/null || true
git diff "$BASE"..."$REF" > "$DIFF"   # three-dot resolves the merge-base internally
```

If the diff is empty, STOP and say so - there is nothing to review. Note the size
(`wc -c "$DIFF"`); it decides how the brief carries the diff in step 2.

### 2. BRIEF (assemble the review prompt as a file)

codex/gemini get a prose brief (they cannot interpret a slash command). Write the
full brief - instructions plus the diff - to a file so diff content containing
quotes, backticks, or `$` can never break the shell in step 3.

```bash
BRIEF="$TMP/peer-brief.txt"
{
  echo "You are doing a focused code review. Review the diff below for <PR #$N | branch $BRANCH>."
  echo "Context: <one line - the user's stated goal, or the repo's purpose from AGENTS.md>."
  echo "Report findings as P1 (blocking), P2 (should-fix), P3 (nit), each with file:line and a concrete fix."
  echo "Be concise; skip praise. If the diff is clean, say so plainly."
  echo
  echo "--- DIFF ---"
  cat "$DIFF"
} > "$BRIEF"
```

**Large diff (> ~120 KB).** A multi-hundred-KB brief can exceed the argv limit in
step 3. In that case, do NOT inline the diff. `$DIFF` lives in a temp dir that a
sandboxed model cannot read, so first re-stage it to a workspace-relative path
under the repo cwd. `.git/peer-review.diff` works well: it sits inside the cwd a
sandboxed `codex exec` / `gemini` can read locally (no network), and it never
shows up in `git status`. Then point the brief at that path instead of the diff
body:

```bash
cp "$DIFF" .git/peer-review.diff   # workspace-relative; readable by a sandboxed model
# ...and in the brief, replace the `--- DIFF ---` block with:
#   echo "Read the diff at .git/peer-review.diff."
```

Unlike a `gh`/`git` call, a local file read works even when the sandbox blocks
network.

### 3. SPAWN - the agent runs it (this is the whole point)

Run the genuine one-shot via your **Bash tool** (never instruct the user to type
it). Name is positional, message is the trailing positional, reply is on stdout:

```bash
fno agents spawn --provider "$PROVIDER" -H -t 300 "peer-$TARGET" "$(cat "$BRIEF")"
```

- `-H` (`--headless`) makes it a synchronous create -> exchange -> teardown
  one-shot; it blocks until the model answers, then prints the review to stdout
  and exits. It is the mobile-friendly alias for the legacy `--once`/`-o` (one
  hyphen, no `--substrate headless` to type); all three resolve to the headless
  lane.
- `"$(cat "$BRIEF")"` passes the brief as one already-expanded argument - the file
  content is not re-parsed by the shell, so any characters inside are safe.
- `-t 300` bounds the model run. Give the Bash call itself a generous timeout
  (e.g. 360000 ms) so the tool does not cut the review off early.
- `peer-$TARGET` is a throwaway name (`-H` tears the agent down); derive it
  from the target, e.g. `peer-pr657` or `peer-fix-ratios`.

### 4. RELAY honestly (the cardinal guard)

stdout IS the review. Judge the outcome only by exit code + emptiness, never by
sniffing for a tone:

- **exit 0 + non-empty stdout** -> relay the findings. Preview the salient ones in
  your message; the full text is in the tool result.
- **exit 0 + empty stdout**, or **non-zero exit** -> the provider failed or
  **abstained**. Report that plainly. If stdout/stderr mentions "usage limits" /
  quota, say the provider's quota is exhausted (the coding-account lane is also
  out) and suggest the other provider or `/review sigma`. NEVER invent findings to
  fill the gap.

### 5. OFFER to apply

Do not auto-apply. Summarize the P1/P2 findings and ask whether to address them.
On a yes, fix them like any other review feedback. P3 nits are optional - call
them out, let the user choose.

## Hard rules (non-negotiable)

1. **The agent runs the spawn, never the user.** The entire value is that the
   synchronous reply returns in-context. Telling the user to type
   `! fno agents spawn ...` recreates the exact bug this skill fixes.
2. **Never fabricate a review.** Empty/non-zero is FAILED-or-abstained, reported
   as such. A peer review you did not actually get is worse than none.
3. **codex or gemini only.** `claude --once` errors (claude peers are persistent
   bg threads, not one-shots). If the user asks for a claude review, point them at
   `/review sigma` (internal Claude panel) or a normal Claude subagent - do not try
   to force `--once`.
4. **Advisory, not a gate.** This review is from a coding account, not the bot
   account. It never satisfies a `required_bots` review gate and must not be
   presented as if it does. A human still merges.

## Multi-CLI

Claude-Code primary. Needs `fno agents` (the daemon backs codex/gemini `--once`)
and `gh`/`git`. If the daemon is down or the provider binary is missing, the spawn
fails loud and you report it - the skill degrades honestly and never fakes a
launch or a review.
