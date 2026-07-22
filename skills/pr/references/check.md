
# Abilities Get PR Review

Poll for external code review on a PR, process inline comments, apply suggestions, and reply.

**Prerequisites:** PR must exist (use `/pr create` first)

## Step 0: Determine Reviewers

Resolve the configured external reviewers via `list-reviewers.sh`. The skill
supports multiple reviewers simultaneously: many repos have both Gemini and
Codex installed as GitHub Apps, and a single-reviewer skill silently misses
findings from whichever bot isn't configured.

```bash
# list-reviewers.sh emits one TAB-separated "type<TAB>bot_login" line per
# active reviewer; empty output (exit 0) means review is disabled.
# Use a portable read loop instead of mapfile (bash 4+) since macOS ships
# bash 3.2.
REVIEWER_TYPES=()
REVIEWER_BOTS=()
while IFS=$'\t' read -r rtype rbot; do
    [[ -n "$rtype" ]] || continue
    REVIEWER_TYPES+=("$rtype")
    REVIEWER_BOTS+=("$rbot")
done < <("${SKILL_DIR}/scripts/list-reviewers.sh")

if [[ ${#REVIEWER_TYPES[@]} -eq 0 ]]; then
    echo "External review disabled in settings."
    exit 0
fi
```

**Config schema (config.toml `config:` section):**

```yaml
config:
  # PREFERRED: plural list. Each item is gemini | coderabbit | claude | codex.
  external_reviewers:
    - gemini
    - codex

  # LEGACY: scalar form. Single-reviewer; treated as a one-item list.
  # Ignored when external_reviewers is set.
  external_reviewer: gemini

  # Optional override for the bot login when the configured type isn't one of
  # the four known names. Only consulted in single-reviewer mode.
  external_reviewer_bot: ""
```

When iterating reviewer-specific work (state checks, comment fetches, replies),
loop over `REVIEWER_TYPES[]` and `REVIEWER_BOTS[]`. For any code path that
hasn't been multi-reviewer-converted, bind the first configured reviewer to
the legacy scalars:

```bash
REVIEWER_TYPE="${REVIEWER_TYPES[0]}"
REVIEWER_BOT="${REVIEWER_BOTS[0]}"
REVIEWER_NAME="${REVIEWER_BOT%\[bot\]}"
```

### Reviewer Adapters

| Reviewer | Bot Login | Review State | Typical Wait |
|----------|-----------|-------------|--------------|
| gemini | `gemini-code-assist[bot]` | COMMENTED | 1-3 min |
| coderabbit | `coderabbitai[bot]` | COMMENTED | 1-5 min |
| claude | `claude[bot]` | COMMENTED or APPROVED | 1-2 min |
| codex | `chatgpt-codex-connector[bot]` | COMMENTED | 3-5 min (can be longer) |

> **Codex bot-login note.** The Codex Cloud reviewer logs in as
> `chatgpt-codex-connector[bot]` (NOT `openai-codex[bot]` or `codex[bot]`).
> Searching for `codex[bot]` against `gh api .../reviews` returns zero
> matches on bllshttng/footnote even when the reviewer has posted
> comments. Confirmed on PR #248 (sigma-review missed a P2 finding that
> chatgpt-codex-connector caught) and PR #254 (3 P1 + 2 P2 findings).
> If `external_reviewer: codex` is set in config.toml without an
> explicit `external_reviewer_bot:` override, the default below resolves
> to the correct login.

### Wait Configuration

Cron checks fire at +5 and +10 minutes after invocation. These are not configurable
via config.toml - the timing matches typical reviewer response times (1-5 min for
fast reviewers, up to 10 min for slower ones).

## Step 0b: Post harness peers (`config.review.peers`)

`external_reviewers` (Step 0) are GitHub Apps that review on GitHub on their
own. **Peers** are different: `config.review.peers` names harness CLIs
(`codex` / `gemini`) that YOU run locally and post as a real PR review under
`config.review.peer_identity`, so they satisfy the login-based loop-check gate
even when an App bot is usage-wedged. Drive this once per check cycle, BEFORE
polling, so the peer reviews are present when the gate reads.

```bash
PEERS="$(fno config get config.review.peers 2>/dev/null || echo '')"
```

If `PEERS` is empty, skip this step. Otherwise, for EACH configured peer
provider, run the posting mode of the peer skill against this PR:

- Invoke `/review peer <N> <provider> --post` (Skill). It runs the provider
  locally, then posts the verbatim review + P1 inline badges under
  `peer_identity` via `scripts/post-peer-review.sh` (idempotent per PR-head;
  fails loud on any `gh` error).

**Fail closed on partial coverage (Concurrency invariant).** When several peers
share one `peer_identity`, loop-check sees a single login and cannot tell codex
from gemini. So if ANY listed peer did not post (its CLI failed / abstained /
the post errored), you MUST stop and report it - do NOT treat the `peers` bucket
as satisfied because a co-listed peer posted under the shared identity. A user
who wants the gate itself to enforce per-peer coverage gives each peer its own
`{provider, identity}` map entry. Never mark the gate met on a peer that did
not actually post.

## Philosophy

- **Err on the side of implementation** - We won't do it later
- **Must-do**: Silent failures, DRY violations, Critical/High priority
- **Skip**: Purely stylistic suggestions with no functional impact

## Process

### 1. Get PR Info

```bash
# If PR number not provided, get from current branch
gh pr view --json number,url --jq '{number: .number, url: .url}'
```

### 2. Wait for Review (Cron-Based)

Schedule two one-shot cron checks instead of a blocking poll loop. This frees
the session to do other work while waiting.

**Step 2a: Quick check** - try immediately in case the review is already in.
Loop over every configured reviewer; if ANY has posted (`COMMENTED` or
`APPROVED`), skip the cron and jump to Step 3:

```bash
OWNER=$(gh repo view --json owner --jq '.owner.login')
REPO=$(gh repo view --json name --jq '.name')

# REVIEWER_BOTS[] is set up in Step 0.
ANY_READY=0
for bot in "${REVIEWER_BOTS[@]}"; do
    # `tail -1` picks the LATEST review state. A bot can post multiple reviews
    # over a PR's lifetime (e.g. COMMENTED, then later APPROVED on a new push);
    # `head -1` would return the oldest, which may not reflect current status.
    state=$(gh api repos/$OWNER/$REPO/pulls/$PR_NUMBER/reviews \
        --jq ".[] | select(.user.login == \"$bot\") | .state" 2>/dev/null | tail -1)
    if [[ "$state" == "COMMENTED" || "$state" == "APPROVED" ]]; then
        echo "found review from $bot (state: $state)"
        ANY_READY=1
    fi
done

if [[ "$ANY_READY" == "1" ]]; then
    : # at least one reviewer has posted; proceed to Step 3 (which iterates all)
fi
```

If `ANY_READY=1`, skip to Step 3 immediately (Step 3 collects findings from
all configured reviewers, not just the first one ready).

**Otherwise, schedule the cron checks** (no inline polling event needed - the loop-check shim reads PR state directly via `gh`).

**Step 2b: Schedule cron checks** if no review yet:

Use `CronCreate` to schedule two one-shot checks:

1. **Primary check at +5 minutes** (recurring: false) - check for review, process if found
2. **Fallback check at +10 minutes** (recurring: false) - process review if found, otherwise
   set `external_review_passed: skipped` with reason "reviewer timeout"

The cron prompt should include the full context needed to resume: PR number,
owner, repo, the FULL list of reviewer bot logins (not just one), and
instructions to fetch comments from every reviewer, implement high/critical
findings, push fixes, and reply to the PR. When the cron prompt fires, it
must run `list-reviewers.sh` again (config could have changed) and iterate
the same way Step 2a does.

**Why cron over polling:** The old `sleep 15` loop burned ~$0.14 per empty iteration in
token overhead. Two one-shot cron checks at fixed times cost nothing while idle and fire
only when the review is likely ready.

**IMPORTANT:**
- Do NOT fall back to a bash polling loop - use CronCreate
- If the review arrives before the cron fires (e.g., task notification), process it immediately
- If both cron checks find no review, set external_review_passed to skipped (not false)

### 3. Fetch Inline Comments

Once at least one review is received, collect inline code comments from EVERY
configured reviewer. Different bots use different priority badge formats
(Gemini: `![medium]`, Codex: `![P2 Badge]`, etc.) - keep the source bot in
view when normalizing priorities downstream.

```bash
# Get inline code review comments from every configured reviewer.
# IMPORTANT: user.login includes [bot] suffix on the /comments endpoint.
for i in "${!REVIEWER_BOTS[@]}"; do
    bot="${REVIEWER_BOTS[$i]}"
    type="${REVIEWER_TYPES[$i]}"
    echo "=== $type ($bot) ==="
    gh api repos/$OWNER/$REPO/pulls/$PR_NUMBER/comments \
      --jq ".[] | select(.user.login == \"$bot\") | {reviewer: \"$type\", path: .path, line: .line, body: .body}"
done
```

When only one reviewer is configured, this loop iterates once and behavior
is identical to the single-reviewer path.

### 4. Parse Review Priority

Each comment body contains a priority badge. Different reviewers use
different conventions; normalize them to one of `critical | high | medium | low`:

| Reviewer | Badge pattern in body | Normalize to |
|---|---|---|
| Gemini | `![critical]`, `![high]`, `![medium]`, `![low]` | same word |
| Codex | `![P1 Badge]`, `![P2 Badge]`, `![P3 Badge]` | `high`, `medium`, `low` |
| CodeRabbit | (varies; check the badge image text) | best-effort match |
| Claude | (usually no badge - read body sentiment) | `medium` default |

| Priority | Action |
|---|---|
| Critical | **MUST implement** |
| High | **MUST implement** |
| Medium | Implement if reasonable effort |
| Low | Skip unless trivial |

**Extract priority from body:**
```bash
# Example Gemini: "![high](https://www.gstatic.com/codereviewagent/high-priority.svg)"
# Example Codex:  "![P1 Badge](https://img.shields.io/badge/P1-red?style=flat)"
# Parse and normalize per reviewer.
```

### 4b. Verify Before Implementing (Critical Step)

For each HIGH or CRITICAL suggestion, before implementing:

1. **Read the actual code** the reviewer is commenting on — does the reviewer's understanding match reality?
2. **Check if the "problem" exists** — sometimes reviewers flag non-issues
3. **Grep for usage** — if the reviewer says "remove this", verify nothing depends on it
4. **Run existing tests** — if they pass, the current code isn't "broken" regardless of priority badge

| Reviewer Says | You Should Do |
|-------------|---------------|
| "This will crash when X" | Verify: can X actually happen in your app? |
| "Remove unused code" | `grep -r "function_name"` — is it really unused? |
| "Use pattern Y instead" | Is pattern Y actually better for YOUR codebase? |
| "Add error handling for Z" | Check: is Z already handled upstream? |
| "This is a security issue" | ALWAYS investigate — but verify the attack vector exists |

### 5. Analyze and Decide

**Always implement:**
- Silent failures (errors swallowed, missing error handling)
- DRY violations (duplicated code that should be shared)
- All `critical` and `high` priority items
- Security issues
- Bugs (incorrect behavior)

**Implement if reasonable:**
- `medium` priority items
- Performance improvements
- Type safety improvements

**Skip:**
- `low` priority purely stylistic suggestions
- Suggestions that would require significant refactoring unrelated to PR scope
- "Nice to have" extractions (e.g., "extract to hook" when current code is clear)

### 6. Implement Feedback

For each item to implement:

1. Read the suggestion carefully
2. Understand the file and context
3. Make the change
4. **Commit with descriptive message** describing the actual change

**Commit message format:**
```bash
# Good - describes what was actually changed
git commit -m "fix(attendance): use robust IPv6 validation with net.ParseIP"
git commit -m "fix(attendance): memoize checklist status calculation with useMemo"
git commit -m "fix(commands): shorten description to 80 chars"

# Bad - doesn't describe what changed
git commit -m "fix: implement reviewer suggestions"
git commit -m "fix(review): address feedback"
```

### 7. Push Updates

```bash
# Push all fixes
git push
```

### 8. Reply Per-Thread, Then Post the Consolidated Summary

#### 8a. Per-thread replies (LOAD-BEARING for the autonomous loop)

The loop-check review gate (control-plane step 2, ab-f1c5a9ed) computes a
blocking finding (codex P1 / gemini critical|high) as **addressed** ONLY when
its thread has a non-bot reply AND (a fix commit landed after the finding's
timestamp OR the reply body carries `wontfix:`). A consolidated top-level
comment is invisible to that detection - PR #447 was addressed with zero
in-thread replies and could not have reached DonePRGreen under the gate.
Without this step, an otherwise-green PR stalls until backstop/budget.

For EVERY blocking finding (`![P1 Badge]` from codex, `![critical]`/`![high]`
from gemini), post a reply **in that finding's thread** after pushing the fix:

```bash
# FINDING_ID = the root comment's .id from Step 3 (in_reply_to_id == null).

# Fixed: name the commit that addresses it.
gh api "repos/$OWNER/$REPO/pulls/$PR_NUMBER/comments" \
  -F in_reply_to="$FINDING_ID" \
  -f body="Fixed in ${FIX_COMMIT_SHA}: <one-line summary of the fix>." \
  || echo "WARN: in-thread reply to finding $FINDING_ID FAILED - the review gate will not see this finding as addressed"

# Declined: carry the wontfix: marker (case-insensitive token) + the reason.
gh api "repos/$OWNER/$REPO/pulls/$PR_NUMBER/comments" \
  -F in_reply_to="$FINDING_ID" \
  -f body="wontfix: <reason this finding is intentionally not addressed>." \
  || echo "WARN: in-thread reply to finding $FINDING_ID FAILED - the review gate will not see this finding as addressed"
```

**Rules:**

- **Idempotent re-reply:** before posting, re-fetch `/pulls/$PR_NUMBER/comments`
  and skip any finding whose thread already has a reply with
  `in_reply_to_id == $FINDING_ID` from a non-bot login - running this skill
  twice must not post duplicate replies.
- **Failure surfaced, never swallowed:** if a reply POST fails, report it and
  do NOT count the finding as addressed - the loop will keep blocking on it,
  and silently claiming success would strand the session.
- **Scope:** only BLOCKING findings gate the loop. Medium/low/P2/P3 findings
  do not require an in-thread reply (though one never hurts).
- The reply must be in-thread (`in_reply_to`), not a new top-level comment:
  the gate reads `in_reply_to_id` chains on `/pulls/N/comments`.

#### 8b. Consolidated summary (kept - augments, never replaces 8a)

After the per-thread replies, post **one consolidated reply** that thanks
every reviewer who participated and lists the implemented + skipped findings
attributed to each. A single comment keeps the PR thread readable and
prevents per-reviewer ping noise.

```bash
# Build the @-mention list dynamically from REVIEWER_BOTS (Step 0).
MENTIONS=()
for bot in "${REVIEWER_BOTS[@]}"; do
    MENTIONS+=("@${bot%\[bot\]}")
done
# Render as "@a and @b" (2 reviewers) or "@a, @b, and @c" (3+); for one
# reviewer it's just "@a".
case ${#MENTIONS[@]} in
    0) MENTION_TEXT="" ;;
    1) MENTION_TEXT="${MENTIONS[0]}" ;;
    2) MENTION_TEXT="${MENTIONS[0]} and ${MENTIONS[1]}" ;;
    *) last_idx=$((${#MENTIONS[@]} - 1))
       # printf -joined approach. "${arr[*]}" with IFS=', ' would only use the
       # first IFS char as the separator (producing "@a,@b" not "@a, @b"); the
       # printf form expands each element individually and includes the full
       # ", " separator between them.
       MENTION_TEXT="$(printf '%s, ' "${MENTIONS[@]:0:$last_idx}")and ${MENTIONS[$last_idx]}" ;;
esac

REPLY_BODY="## Code Review Response

Thanks ${MENTION_TEXT} for the review!

### Implemented

| Reviewer | File | Issue | Fix |
|---|---|---|---|
| Gemini | \`path/to/file.ts\` | HIGH: Description | How I fixed it |
| Codex | \`path/to/other.md\` | P2: Description | How I fixed it |

### Skipped

| Reviewer | File | Issue | Reason |
|---|---|---|---|
| Gemini | \`path/to/file.tsx\` | MEDIUM: Description | Why I skipped it |
"

gh pr comment $PR_NUMBER --body "$REPLY_BODY"
```

**Reply guidelines:**
- Thank every participating reviewer in the opening line
- Attribute each finding to the reviewer who flagged it (the table's first column)
- List each implemented suggestion with file, issue, and fix
- List each skipped suggestion with file, issue, and **reason**
- Be specific and professional

### 9. Check for Additional Comments

After pushing fixes, check every configured reviewer for follow-up comments
(reviewers often re-review automatically when a new commit lands):

```bash
# Check for new comments from every reviewer.
for i in "${!REVIEWER_BOTS[@]}"; do
    bot="${REVIEWER_BOTS[$i]}"
    type="${REVIEWER_TYPES[$i]}"
    echo "=== $type ($bot) follow-up ==="
    gh api repos/$OWNER/$REPO/pulls/$PR_NUMBER/comments \
      --jq ".[] | select(.user.login == \"$bot\") | {created_at: .created_at, path: .path, body: .body}"
done
```

If new high/critical items appear from any reviewer, repeat steps 5-8.

### 10. Mark Complete

Once all feedback is addressed:

```
✓ PR #[NUMBER] review complete
- Reviewers: ${REVIEWER_TYPES[@]} (${REVIEWER_BOTS[@]})
- Implemented: X suggestions
- Skipped: Y suggestions (with reasons)
- Ready for human review
```

### 11. Post-Merge Memory Pass (if sentinel present)

After the review cycle completes, check for a pending post-merge memory pass:

```bash
STATE_DIR="$(git rev-parse --show-toplevel 2>/dev/null || pwd)/.fno"
SENTINEL="$STATE_DIR/.memory-pass-pending"
```

If `$SENTINEL` exists, run the discovery script and process its output:

```bash
PASS_JSON=$(bash "${CLAUDE_PLUGIN_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}/scripts/memory/post-merge-pass.sh")
```

The script emits a single JSON object with keys `pr`, `merged_at`, `late_comments`,
`late_reviews`, and `done_with_concerns`. It removes the sentinel on success, making
it a one-shot operation.

For each entry in `late_comments` and `late_reviews`, and for each path listed in
`done_with_concerns`, decide whether the signal is memory-worthy. Apply the same bar
as the pre-promise pass: "would removing this cause future-me to repeat the mistake
or miss the context?" Comments that are purely operational ("thanks, merged!") are not
memory-worthy. Reviewer concerns that flag a pattern not caught by existing rules are.

For each candidate that passes the bar, call `write-memory-entry.sh`:

```bash
SESSION_ID=$(grep -E "^[[:space:]]*session_id:" "$STATE_DIR/target-state.md" 2>/dev/null \
    | tail -1 | sed -e 's/^[[:space:]]*session_id:[[:space:]]*//' -e 's/[[:space:]]*$//')
# Memory dir uses Claude's slash-encoded full-path scheme (e.g.
# /Users/foo/code/me/fno -> -Users-foo-code-me-fno). MUST match
# the pre-promise pass recipe so both checkpoints land entries in the same
# directory. Using basename here would write to ~/.claude/projects/<dir>/memory/
# which is NOT where Claude reads MEMORY.md from.
# Resolve the CANONICAL repo root, NOT the worktree: from a conductor worktree
# `git rev-parse --show-toplevel` returns the worktree path, which slash-encodes
# to a different dir and splits memory in two. The common git-dir's parent is
# always the main worktree, so both checkpoints land in one place.
_CANON_ROOT="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)")"
MEMORY_DIR="${HOME}/.claude/projects/$(printf '%s' "$_CANON_ROOT" | sed 's|/|-|g')/memory"

bash "${CLAUDE_PLUGIN_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}/scripts/memory/write-memory-entry.sh" \
    --memory-dir "$MEMORY_DIR" \
    --session-id "$SESSION_ID" \
    --candidate '<JSON for this candidate>'
```

If the pass script exits non-zero or no candidates pass the bar, continue silently.
This step is best-effort and must not block the review cycle from completing.

### 12. Post-Merge Retro-Triage Pass (if `.triage-pending` present)

Distinct from the memory pass above: the memory pass writes *lessons* (memory
entries); the retro-triage pass writes *actionable work* (backlog nodes for
left-out items - declined review findings, carve-outs, deferred findings). They
run at the same checkpoint and read overlapping sources, but emit to different
artifacts; the same reviewer comment can legitimately produce both.

`scripts/lib/pr-merge.sh` drops `.fno/.triage-pending` (carrying the
PR number and the merge-time mode) on a successful merge. Consume it via the
shared routine - the same one the universal retro-sentinel consumer uses:

```bash
fno retro run   # consumes retro-pending/*.json AND the local .triage-pending
```

`fno retro run` harvests, classifies, dedups, and lands left-out work, then
removes `.triage-pending` only on a clean run (a partial harvest or a land
failure retains it for retry). It is idempotent: a finding already filed as a
node is skipped, so both this fast-path and a later reconcile sweep firing for
the same PR collapse to a single node set. Best-effort; never blocks the cycle.

---

## Command Reference

### Check Review Status
```bash
gh api repos/OWNER/REPO/pulls/PR_NUMBER/reviews \
  --jq ".[] | select(.user.login == \"$REVIEWER_BOT\") | {state: .state, body: .body}"
```

### Get Inline Comments
```bash
gh api repos/OWNER/REPO/pulls/PR_NUMBER/comments \
  --jq ".[] | select(.user.login == \"$REVIEWER_BOT\") | {path: .path, line: .line, body: .body}"
```

### Get PR Summary Comment
```bash
# Note: gh pr view uses author.login WITHOUT [bot] suffix
gh pr view PR_NUMBER --json comments \
  --jq ".comments[] | select(.author.login == \"$REVIEWER_NAME\") | .body"

# Alternative using gh api (uses user.login WITH [bot] suffix)
gh api repos/OWNER/REPO/issues/PR_NUMBER/comments \
  --jq ".[] | select(.user.login == \"$REVIEWER_BOT\") | .body"
```

### Reply to PR
```bash
gh pr comment PR_NUMBER --body "Your response message here"
```

---

## API Quirk: Login Name Inconsistency

**IMPORTANT:** GitHub has inconsistent login names for bot accounts:

| Command | Field | Value |
|---------|-------|-------|
| `gh api .../reviews` | `user.login` | `$REVIEWER_BOT` (with `[bot]`) |
| `gh api .../comments` | `user.login` | `$REVIEWER_BOT` (with `[bot]`) |
| `gh pr view --json` | `author.login` | `$REVIEWER_NAME` (no `[bot]`) |

Always use `$REVIEWER_BOT` (with `[bot]` suffix) for `gh api` commands.

---

## Priority Decision Matrix

| Category | Priority | Example | Action |
|----------|----------|---------|--------|
| Bug | Critical/High | "This will crash when X" | MUST fix |
| Silent Failure | High | "Error is swallowed here" | MUST fix |
| Security | Critical/High | "SQL injection possible" | MUST fix |
| DRY | Medium/High | "This duplicates code in Y" | MUST fix |
| Type Safety | Medium | "Use type guard instead of assertion" | Fix |
| Performance | Medium | "Memoize this calculation" | Fix if easy |
| Readability | Medium | "Extract to custom hook" | Consider |
| Magic Numbers | Medium/Low | "Define as constant" | Fix (trivial) |
| Naming | Low | "Consider renaming X to Y" | Skip unless confusing |
| Comments | Low | "Add JSDoc here" | Skip |

---

## Key Principles

- **Actually wait for the review** - Run the polling script, don't skip
- **Bias toward implementation** - We won't do it later
- **Trust reviewer priority levels** - High/Critical are usually valid
- **Silent failures are non-negotiable** - Always fix error handling gaps
- **DRY is non-negotiable** - Duplicated code rots
- **Commit each fix separately** - Clear history of review responses
- **Descriptive commit messages** - Say what changed, not "addressed feedback"
- **Always reply to the reviewer** - Explain what you implemented and what you skipped

## NEVER (Anti-Patterns)

**NEVER implement all suggestions blindly:**
- Reviewers analyze code in isolation — they don't know your architecture decisions
- A "high priority" badge doesn't mean "must implement" — verify it's correct first
- "Err on the side of implementation" applies to VALID suggestions, not all suggestions

**NEVER trust priority badges as absolute:**
- Reviewers sometimes mark stylistic suggestions as "high"
- A "critical" that suggests changing working code to a different pattern isn't critical
- YOUR job: verify the suggestion is technically correct for THIS codebase, THEN implement

**NEVER skip the verification step:**
- Before implementing any suggestion: check if it breaks existing tests
- Before implementing a "remove this" suggestion: grep for usage first (YAGNI check)
- Before implementing a refactor suggestion: verify the existing code actually has the problem

**NEVER implement suggestions that conflict with human partner's decisions:**
- If the reviewer suggests X but human partner previously decided Y → ask human partner
- Architecture decisions > review suggestions
- When in doubt, flag it rather than implement it

**NEVER swallow polling failures:**
- If the polling script times out, that's NOT "no review" — it's "unknown state"
- Always tell the user about the timeout — don't silently proceed
- Check PR manually before assuming no review exists

## After review: completion signal

When review resolves (all blocking findings addressed in-thread and pushed), nothing needs to be emitted or sealed: the stop hook's `done()` check reads the PR's review state from GitHub directly inside `fno-agents loop-check` (ab-d0337fbc + step 2 ab-f1c5a9ed). Specifically it verifies (a) every REQUIRED review login - `config.review.github_apps` (the legacy `required_bots` aliases it) plus each `config.review.peers` posting identity - has a completed review pass and (b) every blocking inline finding on `/pulls/N/comments` has a qualifying in-thread ack. A `config.review.optional_apps` login is *honored-if-present*: its absence never blocks (so a usage-wedged App bot can't stall the gate), but a blocking finding it DOES post still holds the gate (Step 8a's replies + your fix commits). The bot reviews, per-thread replies, and fix commits in PR history ARE the completion evidence.
