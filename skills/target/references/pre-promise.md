# Pre-Promise Sequence

**Load when:** the pipeline appears done and target is preparing to emit `<promise>`.

The mechanical session side-effects (the ledger session-record, plan stamp/graduate, and the end-of-session handoff artifact) are NO LONGER part of this sequence. They re-home into `fno-agents finalize`, which the stop-hook shim runs on the terminal-allow boundary so they fire in EVERY mode (attended, autonomous, megawalk worker) even when the agent's context compacted before reaching pre-promise (control-plane step 6, ab-f8e5f214). You do not run cost calculation, ledger writes, plan stamping, or handoff generation by hand anymore.

What remains here is the advisory judgment work only the agent can decide (memory pass, deferrals capture), plus the cross-project recap, the pre-promise self-check, and the promise output. None of these gate `<promise>`: completion authority is the three external reads (PR + CI + reviews) plus the budget ceiling, decided by `fno-agents loop-check`. A skipped advisory step never blocks the promise and never re-opens the loop.

## Cross-Project Ship Recap (recap-only, dedup against earlier sends)

After the plan is stamped/graduated and before the gate audit, run a recap
step so peers whose surfaces this PR's diff actually touched - and who were
NOT already messaged at /think or /blueprint time - learn that the change has
shipped. This is recap-only: a peer that already received a question or
heads-up on the same plan does not get a third message.

Skip the entire step silently when:

- `~/.fno/config.toml` has no `config.inbox.peers` block (opt-in).
- The plan was not run through /target's PR flow (no `pr_url:` set on
  `target-state.md`).
- This is a cross-project intermediate ship - the recap fires only on the
  final project's ship so the message body can cite the shipped PR list,
  not a partial one. The bare `graduate` no-op for `len(urls) <
  expected_url_count` is the same trigger the recap reads.

Mechanic:

```python
from fno.inbox.settings import read_peer_surfaces, read_surface_patterns

peers = read_peer_surfaces()
patterns = read_surface_patterns()
if not peers:
    raise SystemExit(0)  # opt-in by design

# Read messaged_peers from plan frontmatter (if quick plan: the file itself;
# if folder plan: 00-INDEX.md). Fields not yet present -> empty list.
# Same field name used across /think, /blueprint, and /target for consistency.
messaged_peers = read_messaged_peers(plan_path)

# Resolve the diff: files changed in the merged PR body. For single-project
# plans this is the merge commit; for cross-project finals, it's the union
# of every project's merge commit captured in `urls:` on the plan stamp.
changed_files = git_diff_changed_files(merge_commit_or_union)
```

For each changed file, find which surface name(s) it matches, then find
which peer(s) own that surface. Apply the SAME multi-peer disambiguation
rule /think and /blueprint use: if a single surface is owned by more than one
peer (the architecture doc explicitly allows this - e.g. `api/routes/**`
owned by both backend and docs projects), do NOT multi-peer blast. Emit
`<help reason="cross-project-disambiguation" evidence="<file-path>:<surface-name>">`
and skip the send for that file. Multi-peer ambiguity is a config
decision the human needs to resolve, not something the recap should
guess past.

For each unambiguous (peer, surface) match where the peer is NOT in
`messaged_peers`, send ONCE and update the plan:

```bash
if fno mail send --to-project <peer> --kind heads-up \
     --body "shipped: <PR-TITLE>; touches surface <SURFACE-NAME>; PR: <PR-URL>" \
     --ref-pr <PR-NUMBER>; then
  # Append to messaged_peers: in the plan frontmatter so a future re-run
  # of the recap (e.g. resume after a partial failure) is idempotent.
  append_peer_to_messaged_peers "<peer>"
else
  # Send failed - record under messaged_peers_failed: so a future recap
  # retry treats it as "needs send" rather than "already sent". Step
  # failure is non-fatal; the promise still emits.
  append_peer_to_messaged_peers_failed "<peer>" "<reason>"
fi
```

**Anti-patterns:**

- Don't recap to peers already in `messaged_peers:`. The dedup is the whole
  point of the recap-only design.
- Don't multi-peer blast on a shared surface. Two peers owning the same
  surface name is a config decision; recap routes ambiguity to `<help>`,
  never sends to both.
- Don't recap when no surface pattern matches the diff. The change is
  internal by definition - peers don't need to know about it.
- Don't block on responses. Same fire-and-forget contract as /think and
  /blueprint sends.

## Pre-Promise Self-Check

Before outputting `<promise>`, verify the pipeline actually completed: sigma-review ran and found no blocking issues, tests pass, validate is green, a PR exists (unless no_ship), and external review is satisfied (unless no_external). These are not gate booleans to read from a file - they are things you did during the session. If any phase was skipped unintentionally, run it before emitting the promise.

**PR→node link assertion (x-e106).** When this session is node-bound (a
`graph_node_id` other than `null` in the manifest body) and a PR was created,
confirm `node.pr_number` equals the PR you are about to promise - the last-line
assertion for any ship that reached pre-promise through a path that skipped the
ship-phase link step. On mismatch, re-link, **read back**, and refuse to promise
if it still did not stick - a silently-unlinked node re-dispatches as duplicate
work, so this backstop is a real assertion, not a fire-and-forget re-link:

```bash
if [[ -n "${NODE_ID:-}" && "$NODE_ID" != "null" && -n "${PR_NUMBER:-}" ]]; then
  # `|| true`: a failing get must not abort the shell under set -e; treat it as
  # a mismatch that drives the re-link + refuse path below.
  got=$(fno backlog get "$NODE_ID" --field pr_number 2>/dev/null | tr -d '[:space:]' || true)
  if [[ "$got" != "$PR_NUMBER" ]]; then
    echo "pre-promise: node $NODE_ID pr_number=$got != PR #$PR_NUMBER; re-linking" >&2
    # The read-back below turns a failure into a printed reason and a promise
    # blocker, so this swallow is bounded and never invisible.
    # lint-ok: fno-mutation-swallowed
    fno backlog update "$NODE_ID" --pr-number "$PR_NUMBER" --pr-url "$PR_URL" 2>/dev/null || true
    got=$(fno backlog get "$NODE_ID" --field pr_number 2>/dev/null | tr -d '[:space:]' || true)
    if [[ "$got" != "$PR_NUMBER" ]]; then
      echo "<help reason=\"pr-node-link-failed\" evidence=\"node $NODE_ID pr_number=${got:-<none>} expected=$PR_NUMBER\">pre-promise re-link did not stick; refusing to promise. Fix: fno backlog update $NODE_ID --pr-number $PR_NUMBER --pr-url $PR_URL</help>"
      exit 1
    fi
  fi
fi
```

The loop-check verb (`fno-agents loop-check`) will verify the world independently: PR exists for HEAD + CI green + reviewed. A premature promise does not close the loop - it blocks with the failing read named, and the session continues until the world catches up or the backstop fires.

**Required-bot quota early warning (x-5d3e, advisory).** When a review gate would wait on a `config.review` required bot, a cached-quota check surfaces a coming wedge now instead of letting the gate hang silently for hours. Run it just before the promise; it is read-only, fail-open, and never gates:

```bash
fno providers required-bot-check
```

It prints nothing when every required bot's provider has headroom (or none are configured), and emits one `quota_required_bot_exhausted` decision event per exhausted bot. If it prints a warning, in attended mode surface the same facts so the operator can act (swap accounts / wait for the reset) rather than discovering the wedge later:

```
<help reason="required-bot-quota-exhausted" evidence="<the required-bot-check output>">a required review bot's provider is out of quota; the review gate will wedge until its reset. Consider swapping the account or waiting.</help>
```

This never blocks `<promise>` - it is an early warning, not a gate.

## Memory Pass (advisory)

Before emitting the promise, scan this session for novel learnings worth persisting. The goal is to populate `~/.claude/projects/.../memory/` with signal that would otherwise rot in the transcript and cause future-you to repeat a mistake or miss context.

The memory pass is informational. Run it before the promise output. When you have nothing worth writing, declare an explicit empty pass so the session record is complete. The loop-check verb does not gate on memory pass; it is a best-effort practice for session learning capture.

Either way, **always run the pass.** When you have nothing worth writing, declare an explicit empty pass so the session record is complete:

```bash
SESSION_ID=$(grep -E '^[[:space:]]*session_id:' .fno/target-state.md 2>/dev/null \
  | head -1 | sed 's/.*session_id:[[:space:]]*//' | tr -d '"')
bash "${CLAUDE_PLUGIN_ROOT}/scripts/memory/write-memory-entry.sh" \
  --empty-pass --session-id "$SESSION_ID"
```

The empty-pass flag writes the artifact with `entries_written: 0 approved: true` so the session record is complete. The pass is advisory: it never gates `<promise>` (the loop-check verb does not read it), but running it - even as an explicit empty pass - keeps the session learning trail honest.

### What qualifies

Ask: **would removing this from memory cause future-me to repeat the mistake or miss the context?** If no, skip. If yes, write.

Candidate categories:

- **Corrections from the user** - explicit pushback ("no, don't do X"), "actually Y is right", or any moment where Jason told you a better approach.
- **Surprises during execution** - a hook tripped unexpectedly, a tool behaved differently than its docs said, a script swallowed an error silently, an API returned something unexpected.
- **Validated approaches** - a non-obvious choice you proposed that the user confirmed as correct (especially "yes, that's exactly right" or a confirmed pattern the user wants repeated).
- **Project facts not in git** - deadlines, scope decisions, who is doing what next, or why something was intentionally scoped out.

### What to skip

- Anything already obvious from the diff or commit messages.
- Code patterns and conventions that belong in CLAUDE.md or the codebase itself.
- One-off task details with no reuse value.
- Near-duplicates of existing memory entries. The writer's dedup (exit 2) catches exact-name collisions, but you should avoid semantic near-duplicates too - read MEMORY.md index before writing.

### Writing recipe

For each candidate, construct a JSON object and call the writer:

```bash
# Memory dir uses Claude's slash-encoded full-path scheme so the same
# recipe works for any project, not just this one. MUST match the
# post-merge pass recipe in skills/pr/references/check.md so both checkpoints
# land entries in the same directory.
# Resolve the CANONICAL repo root, NOT the worktree: from a conductor worktree
# `git rev-parse --show-toplevel` returns the worktree path, which slash-encodes
# to a different ~/.claude/projects/<dir> and splits memory across two dirs. The
# common git-dir's parent is always the main worktree, so memory lands in one place.
_CANON_ROOT="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)")"
MEMORY_DIR="${HOME}/.claude/projects/$(printf '%s' "$_CANON_ROOT" | sed 's|/|-|g')/memory"
SESSION_ID=$(grep -E '^[[:space:]]*session_id:' .fno/target-state.md 2>/dev/null \
  | head -1 | sed 's/.*session_id:[[:space:]]*//' | tr -d '"')

CANDIDATE=$(jq -n \
  --arg type "feedback" \
  --arg name "short_slug_here" \
  --arg description "One-line description for the MEMORY.md index." \
  --arg body "$(cat <<'BODY'
Rule or observation in plain prose. Include a ## Why section and a ## How to apply section when relevant.
BODY
)" \
  '{type: $type, name: $name, description: $description, body: $body}')

bash "${CLAUDE_PLUGIN_ROOT}/scripts/memory/write-memory-entry.sh" \
  --memory-dir "$MEMORY_DIR" \
  --session-id "$SESSION_ID" \
  --candidate "$CANDIDATE"
# exit 0 = wrote/updated; exit 2 = dedup (fine, no action needed); exit 1 = real error
```

Use `"type": "feedback"` for corrections and surprises; `"type": "project"` for decisions, strategy, and validated architecture choices.

The `name` field is the slug used for the filename (e.g. `feedback_no_foo_pattern` produces `feedback_no_foo_pattern.md`). Keep it specific but short.

The `body` is the full entry text - write it as you would want to read it six months from now. A `## Why` and `## How to apply` section make entries much more useful than bare facts.

### Non-fatal semantics

This step is non-fatal. If `write-memory-entry.sh` is missing or non-executable, log a warning to stderr but do not block `<promise>`:

```bash
if [[ ! -x "${CLAUDE_PLUGIN_ROOT}/scripts/memory/write-memory-entry.sh" ]]; then
    echo "memory-pass: write-memory-entry.sh not found or not executable - skipping" >&2
fi
```

When zero candidates pass the bar, write nothing. Silence is fine. An empty memory pass with no new entries is the correct outcome for routine sessions with no novel signal.

## Deferrals Capture (advisory)

Sibling to the memory pass, but for the *substrate* rather than your memory. Scan this session for small follow-ups that surfaced and were deferred ("replace this lambda with a def", "add a `Literal[]` here", "audit the failure-path emits") - items too small for an idea node but worth not losing. These land in the backlog capture tier (`fno backlog capture`), a markdown holding-pen below idea nodes. See `docs/triage.md` in the footnote repo for the full triage + promotion flow.

**The pass is advisory** (`deferrals_captured`, ab-d63cdd57): run-and-log, never a gate. It writes a `.fno/artifacts/deferrals-${session_id}.md` artifact and emits an `inbox_add`/`inbox_empty_pass` event for the session as an observable record, but its absence never blocks `<promise>` and never re-opens the loop (completion authority is the three external reads plus budget). Skipped entirely when `no_deferrals_capture: true` (S size).

Run the mechanical scan first (it reads the transcript named by `claude_transcript_id`, or stdin):

```bash
SESSION_ID=$(grep -E '^[[:space:]]*session_id:' .fno/target-state.md 2>/dev/null \
  | head -1 | sed 's/.*session_id:[[:space:]]*//' | tr -d '"')
TRANSCRIPT_ID=$(grep -E '^[[:space:]]*claude_transcript_id:' .fno/target-state.md 2>/dev/null \
  | head -1 | sed 's/.*claude_transcript_id:[[:space:]]*//' | tr -d '"')
# claude_transcript_id is a UUID, not a path - resolve it to the .jsonl transcript.
TRANSCRIPT_FILE=""
if [[ -n "$TRANSCRIPT_ID" && "$TRANSCRIPT_ID" != "null" ]]; then
  TRANSCRIPT_FILE=$(find "$HOME/.claude/projects" -name "${TRANSCRIPT_ID}.jsonl" 2>/dev/null | head -1)
fi
# scan returns JSON candidate snippets with line refs; the LLM judges them.
# Falls back to piped stdin when no transcript path resolves (see "Degrade" below).
fno backlog capture scan "${TRANSCRIPT_FILE:-/dev/stdin}" 2>/dev/null || true
```

**Attended** (propose-then-confirm): present the scanned candidates you judge worth keeping via `AskUserQuestion`, then write only the user-confirmed ones:

```bash
fno backlog capture add "title" --source "PR#NNN" --priority p2 \
  --why "one-line rationale (<=120 chars)" --where "file/area"
```

**Unattended** (scan-and-write, like the memory pass): add the qualifying items directly without asking.

**Seal the pass.** After adding items, record completion:

```bash
fno backlog capture capture-pass --session-id "$SESSION_ID"   # writes deferrals-<sid>.md with the count
```

**Honest empty pass.** When nothing this session is worth capturing, declare it explicitly (the `--reason` is mandatory - it is the anti-rubber-stamp guard):

```bash
fno backlog capture empty-pass --reason "no deferrals worth capturing this session" --session-id "$SESSION_ID"
```

**Degrade, never crash.** If no transcript path resolves (e.g. `claude_transcript_id: null`), declare an empty pass with a reason instead:

```bash
fno backlog capture empty-pass --reason "transcript unavailable" --session-id "$SESSION_ID"
```

## Promise Output

Output the completion promise when the pipeline is done. Choose the variant that matches the auto-merge outcome from Phase 7a:

```
# outcome: merged
<promise>MISSION COMPLETE: all tasks done, tests passing, docs generated, PR #42 merged.</promise>

# outcome: queued
<promise>MISSION COMPLETE: all tasks done, tests passing, docs generated, PR #42 queued for auto-merge.</promise>

# outcome: failed
<promise>MISSION COMPLETE: all tasks done, tests passing, docs generated, PR #42 created; auto-merge failed: <reason>. Merge manually.</promise>

# outcome: skipped (auto_merge_approved: false) - unchanged from today
<promise>MISSION COMPLETE: all tasks done, tests passing, docs generated, PR created</promise>
```

## Cross-Project Completion Gates

When `cross_project: true` in target-state.md:
- ALL projects must have a PR created and reviewed
- ALL projects must have a `pr_url` visible in the cross-project pipeline summary
- The loop-check verb reads the world independently; the promise succeeds only when all reads pass

Promise tag format for cross-project:
```
<promise>MISSION COMPLETE: all {N} project PRs created and linked. PRs: {url1}, {url2}</promise>
```

## Post-Promise Behavior (CRITICAL)

After outputting `<promise>...</promise>`:
- **STOP IMMEDIATELY.** Do not output any more text.
- **Do NOT use AskUserQuestion.** No "What's next?" no options, no follow-up.
- **Ignore task notifications.** If subagents deliver results after the promise, do not respond to them.
- The stop hook will handle session continuation. Your job is done.
