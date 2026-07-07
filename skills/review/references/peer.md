
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
- **provider** (optional, trailing word): `codex` (default) or `gemini`. A bare
  `claude` is rejected (it is the same model as the author - see Hard rules), but
  a **routed** `claude` peer IS allowed: a `config.review.peers` entry of the form
  `{provider: claude, model: "zai,glm-5.2"}` runs the claude CLI as transport over
  a genuinely different model (GLM via z.ai), which satisfies the distinct-model
  trust invariant (see step 3b).
- **base** (optional): override the diff base (default `origin/main`).
- **`adversarial`** (optional token, anywhere in the args): swaps the brief from
  defect-hunting to a design-challenge framing (`MODE=adversarial`; default
  `MODE=defect`). Everything else (RESOLVE, SPAWN, RELAY, advisory-by-default) is
  unchanged.
- **focus** (optional free text, adversarial mode only): after `adversarial`, the
  target, and the provider are removed, every remaining token is collected verbatim
  (in order) as a focus string that steers the challenge, e.g.
  `/review peer adversarial 208 gemini does the pr_number disjunct actually matter`.
  The target is only ever a token that is a PR number or resolves to a git branch, so
  a focus word is never mistaken for the target; with no such token the target is the
  current branch and every extra token is focus. Ignored in `MODE=defect`.
- **`--post`** (optional flag): after getting the review, POST it to the PR
  under `config.review.peer_identity` so it satisfies the login-based loop-check
  gate. This is the ONE mode where a peer review is a gate, not advisory (see
  step 6 and Hard rule 4). Requires a PR-number target, `peer_identity`, and a
  PAT in `config.review.peer_token_env`.

## Flow: RESOLVE -> BRIEF -> SPAWN (agent is the runner) -> RELAY -> OFFER

### 1. RESOLVE the diff

**Parse the mode first.** Strip an `adversarial` token from the peer args to set
`MODE=adversarial` (default `MODE=defect`). Then resolve the target
deterministically: the target is the single token that is a PR number (all digits)
or resolves to a git ref (`git rev-parse --verify <tok>`); `codex`/`gemini` is the
provider; `--*` are flags. In adversarial mode, every remaining token is collected
verbatim (in order) into `FOCUS`. Because only a PR#/branch token can be the target,
a focus word is never swallowed as the target; if no token resolves, the target is
the current branch and all leftovers are focus. `FOCUS` is only consumed by the
adversarial brief (step 2); ignore it in `MODE=defect`.

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

Use the defect brief for `MODE=defect`, the challenge brief for
`MODE=adversarial`. Both write the same `$BRIEF` file and carry the diff the same
way, so step 3 is identical.

```bash
BRIEF="$TMP/peer-brief.txt"
if [ "${MODE:-defect}" = "adversarial" ]; then
  {
    echo "You are challenging a proposed change, not hunting defects. The diff below is for <PR #$N | branch $BRANCH>."
    echo "Context: <one line - the user's stated goal, or the repo's purpose from AGENTS.md>."
    echo "Question whether this should ship AS DESIGNED. Address, grounded in the diff:"
    echo "  - Is this the right approach, or is there a simpler / different design that does the job?"
    echo "  - What assumptions does it depend on, and what breaks if they do not hold?"
    echo "  - Where does it fail under real-world conditions: scale, concurrency, partial failure, empty/edge state, migration/rollback?"
    echo "  - What are the tradeoffs vs the alternatives - what does this design give up?"
    if [ -n "$FOCUS" ]; then echo "Focus: $FOCUS"; fi
    echo "Be concise and specific; skip praise. Do not invent findings - if the design is sound, say so plainly and say why."
    echo
    echo "--- DIFF ---"
    cat "$DIFF"
  } > "$BRIEF"
else
  {
    echo "You are doing a focused code review. Review the diff below for <PR #$N | branch $BRANCH>."
    echo "Context: <one line - the user's stated goal, or the repo's purpose from AGENTS.md>."
    echo "Report findings as P1 (blocking), P2 (should-fix), P3 (nit), each with file:line and a concrete fix."
    echo "Be concise; skip praise. If the diff is clean, say so plainly."
    echo
    echo "--- DIFF ---"
    cat "$DIFF"
  } > "$BRIEF"
fi
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

### 3a. SPAWN (codex / gemini) - the agent runs it (this is the whole point)

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

### 3b. GENERATE (routed claude -> GLM) - claude CLI as transport (x-ef41)

For a `config.review.peers` entry `{provider: claude, model: "<rprov>,<rmodel>"}`
(e.g. `zai,glm-5.2`), do NOT `fno agents spawn` - the claude CLI is only the
transport; the review model is the routed one (GLM via z.ai). Run `claude -p` with
the routed `ANTHROPIC_*` env built by the SAME model-routing layer the spawn path
uses (`resolve_explicit_route`), so the z.ai env-var contract lives in one place
and is never hand-rolled here. Clear the parent Anthropic credential first (exactly
as `providers/claude.py` does) or a lingering key sends the run back to Anthropic:

```bash
# $BRIEF = the review prompt (step 2); "zai,glm-5.2" = the peers entry's `model`.
# PY must run on the interpreter that has `fno` importable. A bare system python3
# lacks fno's deps (ModuleNotFoundError: pydantic), so do NOT default to it: probe
# for a working interpreter and FAIL LOUD if none is found. Override with
# FNO_PYTHON (e.g. the fno tool venv python, or `uv run --project <fno-src> python`
# in the footnote source tree).
PYBIN=""
for c in "$FNO_PYTHON" "$HOME/.local/share/uv/tools/fno/bin/python3" python3; do
  [ -n "$c" ] || continue
  if "$c" -c "import fno.agents.model_routing" 2>/dev/null; then PYBIN="$c"; break; fi
done
if [ -z "$PYBIN" ]; then
  echo "no interpreter with fno importable - set FNO_PYTHON to the fno tool venv python (or run under 'uv run --project <fno-src>')" >&2
  # abstain like any failed peer; do NOT fall back to a same-model review.
else
GLM_REVIEW="$("$PYBIN" - "$BRIEF" "zai,glm-5.2" <<'PY'
import sys, os, subprocess
from fno.agents.model_routing import resolve_explicit_route
brief = open(sys.argv[1]).read()
rprov, _, rmodel = sys.argv[2].partition(",")
route = resolve_explicit_route(rprov, rmodel, notice=lambda m: print(m, file=sys.stderr))
if not route:                      # z.ai key unset / provider misconfigured
    print("z.ai key unset - GLM peer skipped", file=sys.stderr)
    sys.exit(3)                    # fail-safe: NO silent Anthropic-billed fallback
env = dict(os.environ)
env.pop("ANTHROPIC_API_KEY", None)
env.pop("CLAUDE_CODE_OAUTH_TOKEN", None)
env.update(route)
r = subprocess.run(["claude", "-p", brief], env=env,
                   capture_output=True, text=True, timeout=300)
sys.stdout.write(r.stdout)
sys.stderr.write(r.stderr)          # surface claude's failure reason, don't drop it
sys.exit(r.returncode)
PY
)"
GLM_RC=$?
fi
```

Then relay `$GLM_REVIEW` through step 4 exactly like a codex/gemini result: a
non-zero `$GLM_RC` (including exit 3, key unset) is a FAILED/abstained peer - say
so plainly ("z.ai key unset - GLM peer skipped") and leave the gate as-is. NEVER
invent findings to stand in for the GLM review you did not actually get.

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

### 6. POST (only with `--post`) - the one gating mode

Default `/review peer` is advisory (step 5 ends it). With `--post`, after a
successful relay you POST the review to the PR under the harness peer identity
so it counts toward the loop-check gate (`config.review.peers`). This is the
deliberate, opt-in relaxation of Hard rule 4.

Preconditions (all required; if any is missing, STOP and say why - never fake a
post):
- the target is a PR number (a branch/bare target has no PR to post to),
- `config.review.peer_identity` is set (the distinct machine-account login),
- `config.review.peer_token_env` names an env var holding that account's PAT.

Then hand the relayed provider output to the posting helper, which posts the
body verbatim as a COMMENTED review under the identity and each P1 as an inline
blocking comment with the exact badge loop-check reads, idempotently per
PR-head, failing loud on any `gh` error:

```bash
# $REVIEW_FILE holds the verbatim provider output from step 3.
# Extract each blocking finding you identified as `path:line:message` and pass
# it with --p1 (repeatable). The helper embeds the P1 badge markup itself.
# For the routed-claude peer (step 3b) pass a distinct label - `claude-glm` - so
# its head marker (<!-- fno-peer:claude-glm:<sha> -->) and body do not collide
# with a codex/gemini peer on the same PR.
bash "${SKILL_DIR}/scripts/post-peer-review.sh" \
  --pr "$N" --provider "$PROVIDER" \
  --token-env "$(fno config get config.review.peer_token_env)" \
  --body-file "$REVIEW_FILE" \
  --p1 "src/foo.rs:42:Null deref when the cache is cold" \
  --p1 "src/bar.py:88:Off-by-one drops the last row"
```

**Advisory-first (x-ef41) - what "advisory" actually means here.** Be precise: any
`config.review.peers` entry IS read by loop-check as a required reviewer (it
resolves each entry to a posting login - the entry's own map `identity`, else the
shared `peer_identity` - and requires that login to have reviewed). So a GLM peer
is not gate-free simply by being routed. What keeps it advisory in v1 is that it
posts under the *shared* `peer_identity` alongside codex/gemini: peers sharing one
identity collapse to a SINGLE required login, cleared by ANY of them posting, so
adding GLM does not add a new hurdle and blocking judgment still rides the flagship
(`PROTECTED_ROLES` stance). (Caveat: a GLM peer that is the *only* peer, posting
under the shared identity, is effectively the sole thing satisfying that login -
there it does gate.) To PROMOTE GLM to an INDEPENDENT required gate once trusted,
give its peers entry its OWN map `identity` (a distinct login loop-check must see
separately) instead of letting it ride the shared `peer_identity` - a one-line
config change, no code.

On a **provider-failed** or **post-failed** outcome the helper exits non-zero
and prints why; relay that verbatim. The gate then stays UNMET with a stated
reason (fail closed) - it never silently "passes". A peer whose CLI produced no
review, or whose post to GitHub failed, must be reported, not papered over.

## Hard rules (non-negotiable)

1. **The agent runs the spawn, never the user.** The entire value is that the
   synchronous reply returns in-context. Telling the user to type
   `! fno agents spawn ...` recreates the exact bug this skill fixes.
2. **Never fabricate a review.** Empty/non-zero is FAILED-or-abstained, reported
   as such. A peer review you did not actually get is worse than none.
3. **codex, gemini, or a ROUTED claude only.** A bare `claude` peer is rejected -
   it is the author's own model (`claude --once` also has no one-shot lane), so
   point a bare-claude request at `/review sigma` or a normal Claude subagent. The
   ONE exception is a routed claude peer (`{provider: claude, model: "zai,glm-5.2"}`,
   step 3b): the claude CLI is transport for a genuinely different model (GLM), so
   it satisfies the distinct-model invariant and is generated via `claude -p` with
   the routed env, never `fno agents spawn`. The config loader enforces this - a
   `claude` peers entry with no `model` route is rejected at load.
4. **Advisory by default; a gate only with `--post`.** Bare `/review peer` is a
   coding-account read that never satisfies a review gate. With `--post` it is
   posted under the distinct `peer_identity` (NOT the author account) and DOES
   gate via `config.review.peers` - the identity's login is what loop-check
   matches, and its P1 inline comments block. It must post the provider output
   verbatim (Hard rule 2 still holds: never invent or soften a finding), and it
   must fail loud rather than mark the gate met on a post that did not happen.

## Multi-CLI

Claude-Code primary. Needs `fno agents` (the daemon backs codex/gemini `--once`)
and `gh`/`git`. If the daemon is down or the provider binary is missing, the spawn
fails loud and you report it - the skill degrades honestly and never fakes a
launch or a review.
