
# Post-merge ritual

Collapses the ritual you re-paste after every self-merge into one verb,
resolving the per-project path/name/cwd from settings so nothing is pasted:

1. **Completion stamp** - close the backlog node + stamp the plan.
2. **Prose follow-ups** - write todos / next-steps to this repo's vault
   `parking-lot.md` (LLM judgment over the merged diff).
3. **Triage** - file anything worth doing now as backlog nodes.
4. **Backfill slot** - present any data backfill the landed PR enables (declared
   in-build as a `kind:backfill` carve-out) and offer to run it with warm
   context, or file it as a node.
5. **Handoff slot** - offer to generate a handoff before the session closes.

Steps 1 and the mechanical half of 3 already exist as `fno` verbs
(`backlog reconcile`, `retro run`, `backlog idea`); this skill orchestrates
them and adds the judgment parts (step 2, "what is worth triaging", and the
warm-context backfill/handoff slots).

> **No Step-0 quiesce needed (post-wedge).** The original design (epic
> ab-77091d48) opened with a "quiesce the owning target session" step that
> flipped a `status: IN_PROGRESS` manifest to `COMPLETE` and wrote
> `.target-completed`. The control-plane wedge (ab-d0337fbc) deleted that status
> model: the `target-state.md` manifest is now immutable with no status field,
> and `fno-agents loop-check` already auto-resolves a session whose PR is merged
> + green + reviewed (a late `DonePRGreen`; a legacy `status:` manifest
> allow-exits immediately). The post-ship window is already quiesced by the
> loop-check verb, so this ritual never fights the hook and there is nothing to
> flip. Group 3 of that epic therefore ships the backfill carve-out kind and the
> backfill/handoff slots only; the quiesce step is obsolete (ab-4a1a4fea).

> **The prose queue is separate from the message bus.** This skill writes to a
> per-project **vault markdown file** at `internal/<area>/backlog/parking-lot.md`
> (a human reading queue of prose next-steps). That is NOT the cross-project
> message bus `fno mail` (`config.paths.inbox_dir`, thread-per-file). This
> skill never touches `fno mail`.

## Prerequisites

- The PR is already merged (this runs *after* merge, by hand or from the
  Phase 2 watcher).
- The repo's `.fno/config.toml` sets `config.post_merge.parking_lot_path`.
  Without it the skill fails loud (see Step 1). It never guesses a path,
  because the vault-area name does not equal the project name
  (`example-pipeline -> internal/etl/backlog/parking-lot.md`).
  Check this up front - before a merge - with `fno config doctor --post-merge`
  (the `/target` preflight also warns when it is unset, and `fno setup post-merge`
  scaffolds it).
- `gh` is authenticated for reading the merged diff.

## Autonomous mode (dispatched runs: no operator present)

Merge-detection dispatches this ritual as a background worker with **no human
to prompt** (`_spawn_post_merge_worker`, pr_watch, and `scripts/post-merge/watch.sh`
all pass an `autonomous` token on the invocation; a manual headless run sets
`POST_MERGE_NONINTERACTIVE=1` or runs under `claude --print`). An interactive
`--bg` worker that reaches a prompt slot **hangs forever** - the exact x-47be v1
stall this mode fixes.

**In autonomous mode you MUST NOT call `AskUserQuestion` (or any interactive
prompt) at any slot.** Take the no-operator branch everywhere and self-end after
Step 7:

- **Backfill slot (Step 4b)** -> file a node, never run it.
- **Handoff slot (Step 6b)** -> skip silently.
- **Keep-going follow-up dispatch (Step 3, x-3360)** -> the autonomous
  keep-going engine is what makes this loop generate the *next* unit of work
  instead of only closing the merged one. It runs inside `fno retro run
  --keep-going` (Step 3): after the surviving carve-outs are filed as nodes, it
  classifies each and dispatches under a shared per-day firehose ceiling
  (`config.think_spawn.daily_cap`, counted across every autonomous dispatch):
  a deferred carve-out (design unclear) -> `fno think dispatch`; an oos-bug
  carve-out (scoped, design clear) -> a bg `/target <node> no-merge`; anything
  else -> the filed node only. When the ceiling is hit, remaining follow-ups
  stay filed as backlog nodes (never dropped) and one cap line is printed. It is
  a no-op unless `config.keep_going.enabled` is armed, so you never hand-spawn a
  think/target yourself here - the verb owns it, deterministically and
  ceiling-bounded.
- **Self-end** after the Step-7 report: finish the turn, do not wait for input.
  Each dispatched worker is the next loop iteration; nothing stays resident.

Detect the mode once at the top: autonomous when an `autonomous` argument token
is present, `POST_MERGE_NONINTERACTIVE=1` is set, or there is no interactive
operator. Everything below is unchanged for an attended manual `/fno:pr merged <n>`.

## Step 0: Resolve the PR

If a PR number was passed as the argument, use it (ignore a trailing
`autonomous` mode token - it is not the PR). Otherwise find the most recently
merged PR for this repo:

```bash
# Strip the optional `autonomous` mode token, then take the PR number.
AUTONOMOUS=0
ARGS=("$@")
for a in "${ARGS[@]}"; do [[ "$a" == "autonomous" ]] && AUTONOMOUS=1; done
[[ "${POST_MERGE_NONINTERACTIVE:-0}" == "1" ]] && AUTONOMOUS=1
PR=""
for a in "${ARGS[@]}"; do [[ "$a" =~ ^[0-9]+$ ]] && { PR="$a"; break; }; done
if [[ -z "$PR" ]]; then
  PR="$(gh pr list --state merged --json number,mergedAt \
          --limit 1 --jq 'sort_by(.mergedAt) | last | .number')"
fi
[[ -n "$PR" && "$PR" != "null" ]] || { echo "post-merge: no merged PR found; pass a PR number."; exit 0; }
```

## Step 0.5: First-action reservation (dedup mutex)

Before ANY mutating step, reserve this PR's ritual with a global TTL claim so a
second runner (an attended `/fno:pr merged` racing the auto-dispatched
`pr-merged-<N>` worker) cannot execute the destructive middle (Steps 2-4)
concurrently. This is the ritual's FIRST action once the PR is known - it is
pure bash/CLI (no prompt), so it behaves identically in attended and
autonomous/headless modes.

```bash
# Runner-unique, stable-per-runner holder: unique per session (the attended
# run and the dispatched worker each have their own) yet identical across this
# session's many short-lived bash sub-invocations, so Step 8 recomputes the
# SAME string to release. A shared-constant holder would read as an idempotent
# re-acquire and silently defeat the mutex - keep it session-keyed.
#
# `fno claim session-pid` exits 0 with EMPTY stdout when no claude ancestor
# exists (codex/gemini/plain-shell), so guard on EMPTINESS, not exit code - an
# empty suffix would collapse both racing runners to the SAME holder and
# silently defeat the mutex in exactly the autonomous modes this protects. `$$`
# is process-unique, so distinct runners still get distinct holders.
_SID="${CLAUDE_CODE_SESSION_ID:-}"
[[ -n "$_SID" ]] || _SID="$(fno claim session-pid 2>/dev/null || true)"
[[ -n "$_SID" ]] || _SID="$$"
HOLDER="postmerge:pr-${PR}:${_SID}"

# `reconcile:` routes to the GLOBAL claims root (~/.fno/claims), so the two
# racing runners - which run from different cwds - see each other's claim.
# --ttl 15m: the ritual is many short-lived bash calls with no durable PID to
# anchor PID-liveness to; 15m bounds a run that finishes in 1-3 min and
# self-frees on crash.
if fno claim acquire reconcile:pr-${PR} --holder "$HOLDER" --ttl 15m; then
  :   # won the race - we own the ritual
else
  rc=$?
  if [[ "$rc" == "1" ]]; then
    echo "post-merge: PR #${PR} ritual already claimed by another runner; it will complete the ritual. Exiting."
    exit 0
  fi
  # Any other non-zero (transient corrupt/gone-away, validation) - FAIL OPEN.
  # A claims-subsystem hiccup must not wedge the ritual; the Step-5 marker is
  # the backstop, and a simultaneous double-failure (the only double-fire
  # re-open) is vanishingly rare.
  echo "post-merge: reservation claim errored (exit $rc); proceeding without it (marker guard backstops)." >&2
fi

# Belt-and-braces: covers a cold re-run whose prior completed ritual's claim TTL
# has since expired. If this PR's parking-lot marker already exists there is
# nothing left to do - release our fresh claim and exit BEFORE any mutation.
# Best-effort resolve here (Step 1 does the fail-loud version); an unresolvable
# config/path just skips the shortcut and lets Step 1/Step 5 handle it.
_RR="$(git rev-parse --show-toplevel 2>/dev/null || true)"
_PLREL="$(fno config get config.post_merge.parking_lot_path 2>/dev/null || echo "")"
if [[ -n "$_RR" && -n "$_PLREL" ]] && \
   bash "${CLAUDE_PLUGIN_ROOT:-$_RR}/skills/pr/scripts/inbox-has-pr.sh" "$_RR/$_PLREL" "$PR" 2>/dev/null; then
  echo "post-merge: PR #${PR} already recorded (marker present) - releasing claim and exiting."
  fno claim release reconcile:pr-${PR} --holder "$HOLDER" 2>/dev/null || true
  exit 0
fi
```

## Step 1: Resolve per-project context (FAIL LOUD, never guess)

```bash
REPO_ROOT="$(git rev-parse --show-toplevel)" || { echo "post-merge: not in a git repo." >&2; exit 1; }

# Read config WITHOUT masking a read failure as "unset". `fno config get`
# prints an empty line for a known-but-unset key (clean exit 0); it exits
# NON-ZERO only when fno is missing/too old or config.toml fails to
# validate. Those are NOT "not opted in" - fail loud with the real reason
# instead of misreporting them as an unset path.
PM_ERR="$(mktemp)"
if ! ENABLED="$(fno config get config.post_merge.enabled 2>"$PM_ERR")"; then
  echo "post-merge: 'fno config get config.post_merge.enabled' failed (fno missing/too old, or config.toml invalid):" >&2
  cat "$PM_ERR" >&2; rm -f "$PM_ERR"; exit 1
fi
if ! PARKING_LOT_REL="$(fno config get config.post_merge.parking_lot_path 2>"$PM_ERR")"; then
  echo "post-merge: 'fno config get config.post_merge.parking_lot_path' failed (fno missing/too old, or config.toml invalid):" >&2
  cat "$PM_ERR" >&2; rm -f "$PM_ERR"; exit 1
fi
rm -f "$PM_ERR"

if [[ "$ENABLED" == "False" || "$ENABLED" == "false" ]]; then
  echo "post-merge: disabled for this repo (config.post_merge.enabled: false). Nothing to do."
  exit 0
fi

if [[ -z "$PARKING_LOT_REL" ]]; then
  echo "post-merge: config.post_merge.parking_lot_path is unset for this repo." >&2
  echo "Set it in .fno/config.toml, e.g.:" >&2
  echo "  config:" >&2
  echo "    post_merge:" >&2
  echo "      parking_lot_path: internal/<area>/backlog/parking-lot.md   # repo-relative" >&2
  exit 1   # FAIL LOUD - do not write to any queue
fi

# Defense-in-depth path guard. The schema validator already rejects absolute
# and '..' paths, but a stale installed fno (older schema) might not, so
# backstop here before joining onto the repo root.
case "$PARKING_LOT_REL" in
  /*|~*) echo "post-merge: parking_lot_path must be repo-relative (no leading / or ~); got: $PARKING_LOT_REL" >&2; exit 1 ;;
esac
case "/$PARKING_LOT_REL/" in
  */../*) echo "post-merge: parking_lot_path must not contain a '..' segment; got: $PARKING_LOT_REL" >&2; exit 1 ;;
esac

PARKING_LOT_PATH="$REPO_ROOT/$PARKING_LOT_REL"
PROJECT="$(fno config get config.project.id 2>/dev/null || echo "")"   # best-effort; backlog idea auto-detects when empty
```

An empty `$PARKING_LOT_REL` here means the key is genuinely unset (the read
succeeded), so it is the "not opted in" signal - **stop and print the
actionable message above; do not write prose.** A read *failure* already
exited non-zero above with the real cause, so the two cases never collapse
into the same path.

> **Failure handling (applies to every `fno` verb in Steps 2-6).** None of
> these should be fired blind. Capture each verb's exit code; on a non-zero
> exit, record what failed (verb + stderr) and surface it in the Step 7
> report under "failures" - do NOT report a clean success when a step
> errored. In the headless watcher path, exit non-zero so the watcher logs
> it (see "Headless invocation"). A merged PR that fails to reconcile is a
> real problem, not a no-op.

## Step 2: Completion stamp

Close the backlog node whose PR merged outside the ship gate and stamp its
plan. If you know the node id, scope it; otherwise sweep:

```bash
# Capture the ids reconcile closed into NODE_IDS (Step 8a reaps each closed
# node's build-worker row). --json runs the same mutations; only the output
# shape changes, so the failure flag still reads reconcile's own exit code.
RECONCILE_ERR="$(mktemp)"
if ! RECONCILE_JSON="$(fno backlog reconcile --json 2>"$RECONCILE_ERR")"; then
  echo "post-merge: reconcile FAILED - record in report" >&2
  cat "$RECONCILE_ERR" >&2
  RECONCILE_FAILED=1
fi
rm -f "$RECONCILE_ERR"
NODE_IDS="$(printf '%s' "$RECONCILE_JSON" | jq -r '.closed[]?.node_id // empty' 2>/dev/null | tr '\n' ' ')"
# On the dominant /target ship-gate path the node is closed+stamped BEFORE this
# ritual runs, so reconcile no-ops and .closed[] is empty. Union in the node the
# PR already maps to (read-only pr_number scan of the graph) so Step 8a still
# reaps its build-worker row. Deduped so the out-of-gate case reaps once.
GJ="$(python3 -c 'from fno.paths import graph_json; print(graph_json())' 2>/dev/null || echo "$HOME/.fno/graph.json")"
# graph.json stores nodes flat under `.entries`, and pr_number is not unique
# (same PR can map to multiple ids), so union EVERY match - the reap loop below
# already iterates NODE_IDS and skips non-live/absent rows, making extra ids safe.
for PR_NODE in $(jq -r --argjson pr "$PR" '.entries[]? | select(.pr_number == $pr) | .id' "$GJ" 2>/dev/null || true); do
  case " $NODE_IDS " in *" $PR_NODE "*) : ;; *) NODE_IDS="${NODE_IDS}${NODE_IDS:+ }$PR_NODE" ;; esac
done
# full sweep above, or scope it: fno backlog reconcile --node ab-XXXXXXXX --json
```

`reconcile` is idempotent and a no-op when nothing drifted. If the PR maps to
no node, that is fine (reconcile closes nothing, exit 0, `NODE_IDS` empty) -
continue. A non-zero exit is a genuine failure (e.g. corrupt graph.json): keep
going so the inbox prose still lands, but flag it in the report.

### Step 2b: Stamp ship provenance (post-merge takeover, x-b6e4)

A post-merge session that runs this ritual is a distinct ship-phase contributor
from the one that opened the PR. Append a `ship` lifecycle entry for **the node
this specific `$PR` maps to** - NOT the whole `NODE_IDS` sweep, which may include
unrelated nodes the unscoped reconcile closed in the same pass. `--pr` resolves
exactly one same-repo PR-linked node and warns+skips on zero or multiple
(Locked Decision 9: never fan out):

```bash
# --repo scopes resolution to THIS repo: pr_number is not unique across repos in
# the cross-project graph, so a bare number fans out to `ambiguous` and skips
# (x-d5f9). A gh miss degrades to a bare number - a safe skip, never a wrong stamp.
REPO_SLUG="$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)"
fno backlog session add --pr-number "$PR" ${REPO_SLUG:+--repo "$REPO_SLUG"} --phase ship || true
```

Harness + session id default from the ambient identity; idempotent (this exact
session's `ship` entry is added once) and best-effort - a missing-identity,
no-match, or ambiguous-PR warning skips silently and never blocks the ritual.

## Step 3: Mechanical triage harvest

```bash
# In autonomous mode add --keep-going so the keep-going engine (x-3360)
# classifies surviving carve-outs and dispatches follow-up /think or /target work
# under the firehose ceiling. A no-op unless config.keep_going.enabled is armed.
KEEP_GOING_FLAG=()
if [[ "$AUTONOMOUS" == "1" ]]; then KEEP_GOING_FLAG=(--keep-going); fi
# Guarded expansion: under Bash 3.2 + set -u, "${arr[@]}" on an empty array errors.
fno retro run --pr-number "$PR" "${KEEP_GOING_FLAG[@]+"${KEEP_GOING_FLAG[@]}"}" || { echo "post-merge: retro run FAILED - record in report" >&2; RETRO_FAILED=1; }
# Processes any retro/.triage-pending sentinels AND explicitly harvests this
# PR's carve-outs. The bare `retro run` only fires when a sentinel exists; a
# manual merge with no node<->PR link drops none, so its carve-outs (now stored
# under the canonical root, surviving worktree archival) would never be
# harvested. `--pr "$PR"` closes that gap. If several sessions are in flight,
# scope it with `--session <sid>` so only this PR's carve-outs are harvested.
```

## Step 3b: Auto-continue (merge-triggered next dispatch)

Now that the node is closed (Step 2) and its retro is harvested (Step 3),
hand the merge event to the shared auto-continue verb so the next now-unblocked
node auto-builds without a manual "kick off the next group?" prompt:

```bash
fno backlog advance --closed "$NODE_ID" --project "$PROJECT" \
  || echo "post-merge: auto-continue advance returned non-zero (non-fatal) - record in report" >&2
```

This is **opt-in and non-fatal**. `advance` gates on `config.auto_continue`
itself (a no-op `advance_skipped{disabled}` when off, the default), honors a
live `walker:<root>` (skips during a megawalk), and dedups via the
`dispatch:<id>` reservation - so calling it here AND from Step 2's `reconcile`
for the same merge dispatches the successor at most once (AC1-FR). Pass
`--closed "$NODE_ID"` only if a node id was resolved for this PR; otherwise drop
the flag (advance reads `fno backlog next` regardless - the flag is just
race-ordering provenance). `$NODE_ID` is optional; if the skill never resolved
one, run `fno backlog advance --project "$PROJECT"`.

The reconcile in Step 2 already fires advance for a node it closes; this
explicit call covers the case where the node was already closed before
`/pr merged` ran (reconcile then no-ops, so the successor would otherwise never
be dispatched).

## Step 3c: Skill-diff eval-after-merge (close the loop)

If the merged PR is a skill-diff proposer PR, re-score the merged skill against
the exact corpus items its diff targeted and emit the `skill_diff_eval_closed`
receipt (the before/after delta). Call it unconditionally - the verb self-guards
(a plain "not a known proposer PR" no-op for any ordinary PR) and dedups on the
receipt, so this is safe for every merge and idempotent on a re-run:

```bash
fno skill-diff reconcile --pr-number "$PR" \
  || echo "post-merge: skill-diff eval-after-merge returned non-zero (non-fatal) - record in report" >&2
```

This is the **merge-triggered fast path** (fast feedback on the just-merged
diff). The periodic proposer tick's bare `fno skill-diff reconcile` full sweep is
the backstop for a missed merge-trigger; both key idempotency on the
`skill_diff_eval_closed` receipt, so firing both dispatches at most one re-eval
per PR. Non-fatal: a re-eval failure leaves the PR detectable as un-closed for
the next tick and never blocks the rest of the ritual.

## Step 3d: Canonical sync (bring the local env up to the merged HEAD)

Sync the CANONICAL checkout + installed tooling to the merged HEAD so the local
env is never left stale after a merge (the manual `git checkout main && git pull
&& fno update && fno restart` becomes a ritual step). Opt-in and self-deduping:

```bash
fno pr sync-canonical --pr-number "$PR" \
  || echo "post-merge: canonical sync returned non-zero (non-fatal) - record in report" >&2
```

**Opt-in, non-fatal, exactly-once.** A no-op unless `config.post_merge.sync_command`
is set (prints `not configured` and exits 0). It ALWAYS targets the canonical
checkout even when this ritual runs from a worktree (a worktree cannot
`git checkout main` without hijacking its branch). It gates on
`config.post_merge.sync_paths` (a docs-only merge skips the sync) and dedups on a
`.fno/post-merge-synced/<merge-sha>` marker, so running it here AND from a
merge-detection auto-dispatch for the same merge runs `sync_command` at most
once. A failure withholds the marker (visible retry next reconcile) and never
blocks the rest of the ritual.

## Step 3e: Warm-window triage of this PR's deferral-born nodes

The merge lands inside the 3-day window where deferral returns actually happen,
so this is the cheapest moment to decide the fate of the nodes W1's `/pr create`
step filed from this PR's "Out of scope" section. Each such node carries
`deferred from PR:` in its details and (when the ship session knew its node)
`parent: $NODE_ID`. Collect them by provenance, scoped to this project:

```bash
BORN_JSON="$(fno backlog find 'deferred from PR' --project "$PROJECT" -J 2>/dev/null || echo '[]')"
```

Do NOT add `-s idea`: a deferral-born node may have already been triaged or moved before the merge landed, and those are exactly the ones warm-window triage should still surface. Provenance + parent linkage is the scope.

Keep only the rows that belong to THIS PR: `parent == "$NODE_ID"` when `$NODE_ID`
is set, else those whose `details` name this PR's branch. If the filtered set is
empty, **skip this step silently** - most PRs defer nothing (Boundary: zero
deferral-born nodes is a no-op).

**Attended** (an operator is present, `$AUTONOMOUS != 1`): present each node once
as `<id> <title>` and offer a one-touch decision, running the chosen `fno backlog`
verb:
- **promote** -> `fno backlog rank <id> --top` (float it to run next), optionally with `fno backlog reprioritize <id> -p p1`;
- **keep** as filed -> no-op;
- **defer** explicitly -> `fno backlog defer <id>`;
- **supersede** (already covered / obsolete) -> `fno backlog supersede <id> --by <other-id>`, or `fno backlog done <id>` if it is moot.

**Unattended** (`$AUTONOMOUS == 1`): do NOT prompt. Log each node as `undecided`
in the report (Step 7) and continue - undecided is exactly today's status quo, no
regression.

Non-fatal and skippable: a query failure or an unresolved choice records the node
as undecided and never blocks the ritual's remaining steps.

## Step 4: Best-effort worktree archive

After the mechanical triage steps complete, archive the feature's worktree so
Conductor-managed trees (`~/conductor/workspaces/<repo>/<name>`) do not linger
forever. This step is best-effort: any failure leaves the worktree in place and
continues the ritual.

```bash
# Resolve the feature's worktree by matching the merged branch against all
# known worktrees in the canonical checkout.
# NR==1 (the first --porcelain line is always the main worktree) and reads to
# EOF rather than `awk ... exit`, which would close the pipe early and SIGPIPE
# `git worktree list` under `set -euo pipefail` (the bug PR #519 fixed in the
# archive script). The skill bash runs without pipefail so it is not fatal here,
# but keep it drain-safe for defense-in-depth.
CANONICAL_ROOT="$(git worktree list --porcelain | awk 'NR==1 {sub(/^worktree /, ""); print}')"
BRANCH_NAME="$(gh pr view "$PR" --json headRefName --jq '.headRefName')"
WORKTREE_PATH=""

if [[ -n "$BRANCH_NAME" && "$BRANCH_NAME" != "null" ]]; then
  while IFS= read -r wt_path; do
    wt_branch="$(git -C "$wt_path" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    if [[ "$wt_branch" == "$BRANCH_NAME" && "$wt_path" != "$CANONICAL_ROOT" ]]; then
      WORKTREE_PATH="$wt_path"
      break
    fi
  done < <(git worktree list --porcelain | awk '/^worktree / {sub(/^worktree /, ""); print}' | tail -n +2)
fi

if [[ -z "$WORKTREE_PATH" ]]; then
  echo "post-merge: no worktree found for branch $BRANCH_NAME - skipped (merged from canonical or already removed)."
elif [[ "$(cd "$WORKTREE_PATH" 2>/dev/null && pwd)" == "$(pwd)" ]]; then
  # Running inside the worktree being archived - skip and tell the operator.
  echo "post-merge: session is running inside the feature worktree ($WORKTREE_PATH)."
  echo "    Run this manually from canonical when done:"
  echo "    bash scripts/setup/archive-worktree.sh $WORKTREE_PATH --yes"
elif [[ ! -f "$CANONICAL_ROOT/scripts/setup/archive-worktree.sh" ]]; then
  echo "post-merge: archive-worktree.sh not found at $CANONICAL_ROOT/scripts/setup/ - skipped."
else
  echo "post-merge: archiving worktree $WORKTREE_PATH ..."
  if ! bash "$CANONICAL_ROOT/scripts/setup/archive-worktree.sh" "$WORKTREE_PATH" --yes 2>&1; then
    echo "post-merge: worktree archive returned non-zero (checks failed or remove failed) - skipped (worktree left in place)."
    echo "    To archive manually: bash scripts/setup/archive-worktree.sh $WORKTREE_PATH --yes"
  fi
fi
```

Guard rules:
- **Never use `--force`**. Strict checks (clean tree, no unpushed commits, no live target session) stay ON so a partially-dirty worktree is never silently destroyed.
- **Never archive the canonical checkout** - the script already refuses, but we also never pass the canonical root as the target.
- **Session inside the worktree** - print the manual command and continue; do not attempt self-removal.
- **Missing archive script** - older checkouts may not have it; skip silently.
- **Any exit non-zero from the script** - surface verbatim, mark step "skipped (checks failed)", and continue. This step never blocks the rest of the ritual.

## Step 4b: Backfill slot (warm-context data backfills the PR enables)

A data backfill the just-landed PR makes possible is declared during the build
as a carve-out: `fno carveout add --kind backfill --need "<precondition>" "<what
+ command>"`. The generic retro harvest skips `kind:backfill` (Step 3 never
consumes them), so they SURVIVE here for this slot to handle.

**Scope to THIS PR's session(s) first.** The canonical ledger may hold backfills
from several concurrent target sessions; this slot must only handle the ones the
merged PR's build session declared, never another PR's (consuming/filing those
under the wrong PR/project). Resolve the owning session(s) from `ledger.json`
(which records `pr_number` / `pr_url` per session), then pass them to `list`:

```bash
LEDGER_JSON="$(fno state path ledger || true)"   # fno.paths-resolved; honors config.paths.ledger_json (fail-open under set -e)
SESSIONS=$(jq -r --argjson pr "$PR" '
  .entries[]
  | select((.pr_number == $pr) or ((.pr_url // "") | test("/" + ($pr|tostring) + "$")))
  | ((.sessions // []) + [.session_id])[]
' "$LEDGER_JSON" 2>/dev/null | grep -vxE 'null|' | sort -u)

SESS_ARGS=(); for s in $SESSIONS; do SESS_ARGS+=(--session-id "$s"); done
if [[ ${#SESS_ARGS[@]} -gt 0 ]]; then
  fno carveout list --kind backfill "${SESS_ARGS[@]}" --json
else
  # No session resolves for this PR (manual merge with no ledger record, or a
  # pre-ledger PR). Do NOT consume cross-session backfills under this PR: list
  # them informationally only, and do NOT `resolve` any in this case.
  echo "post-merge: no owning session resolved for PR #$PR; backfills (if any) shown read-only, not consumed:" >&2
  fno carveout list --kind backfill --json
fi
```

Each line is one carve-out (`{id, need, description, priority, session_id, ...}`).
When the session scoping succeeded (the `SESS_ARGS` branch), handle each as
below; in the fallback branch present them read-only and skip the `resolve` step.
**This slot NEVER auto-runs a backfill on mere detection** (a backfill that lands
via two paths - operator-run AND this slot - would double-apply):

- **Interactive (you can ask the operator).** Present the `description` (the what
  + command) and its `--need` precondition, then ask whether to run it now with
  warm context. On **yes**, confirm the precondition holds (e.g. the migration is
  applied), run the command, report the outcome. On **no**, file a node (below).
- **Autonomous / headless** (the `autonomous` token, `claude --print`,
  `--dangerously-skip-permissions`, `POST_MERGE_NONINTERACTIVE=1`, or no operator
  to ask): do NOT run anything and do NOT prompt; file a node so the warm-context
  offer is not lost.
- **File a node (declined or headless).** Never silently drop it:

  ```bash
  fno backlog idea "backfill: <concise title>" \
    --details "Enabled by PR #$PR. Precondition: <need>. Command: <description>." \
    --priority "<carve-out priority or p2>" \
    --project "$PROJECT" --cwd "$REPO_ROOT" \
    || { echo "post-merge: backfill backlog idea FAILED - record in report" >&2; }
  ```

Once a backfill is handled (run OR filed as a node), remove it from the ledger so
a later `/pr merged` never re-offers it - but ONLY in the session-scoped branch
(in the read-only fallback, leave it for the owning PR's `/pr merged`):

```bash
fno carveout resolve <cv-id>
```

**Idempotency / interrupt-safety.** This slot runs BEFORE the Step-5 marker guard
so it executes on every invocation, including a re-run after an interrupt. A
backfill is consumed (`resolve`) ONLY once handled; an unhandled one survives in
the ledger and is re-offered next run. A carve-out with an empty `description` is
presented as "(no command)" and offered skip/file only - never executed as an
empty string.

## Step 5: Idempotency check

Before writing prose, ask whether this PR's section already exists:

```bash
SKILL_DIR="${CLAUDE_PLUGIN_ROOT:-$REPO_ROOT}/skills/pr"
if bash "$SKILL_DIR/scripts/inbox-has-pr.sh" "$PARKING_LOT_PATH" "$PR"; then
  echo "post-merge: parking-lot already has a section for PR #$PR - skipping prose (idempotent)."
  # reconcile + retro run above are already idempotent, so a re-run is a full no-op.
  exit 0
fi
```

(The helper exits 0 when the `<!-- post-merge:pr-<N> -->` marker is already
present, 1 when it is safe to write.)

## Step 6: Read the merged diff and apply judgment

```bash
gh pr diff "$PR"                 # or: git show <merge_sha>
gh pr view "$PR" --json title,body,mergedAt
```

Read the diff. Decide two things:

**(a) Follow-ups -> `$PARKING_LOT_PATH`.** Two kinds land here; always append, never
overwrite.

*Narrative (prose timeline).* Append a dated section keyed by the PR number for
idempotency. The marker comment on the first line is what Step 5 detects:

```markdown

<!-- post-merge:pr-123 -->
## Post-merge follow-ups - PR #123 (2026-05-30)

_<pr title>, merged 2026-05-30. Written by /fno:pr merged._

- A thing to keep an eye on now that this shipped.
- [ ] a decision/sign-off only the maintainer can make #jc
```

Keep the narrative to genuine context: a thing to watch, plus `#jc` items only
the maintainer can do (a decision, a sign-off, a manual setup) - per your repo's
maintainer-todo convention. Implementation work does NOT get `#jc`.

*Actionable capture-tier items (typed, deduped).* For each small follow-on the
diff implies that is below a full node (a deferred edge case, a tidy-up), emit a
TYPED `fu-*` line via `fno backlog capture add` instead of a freeform bullet, so it
is visible to `fno backlog capture list --by-type` / `tidy` and is never re-filed
as a duplicate:

```bash
fno backlog capture add "<concise title>" \
  --source "PR#$PR" \
  --why "<one line: what the diff deferred>" \
  --where "<file/area, optional>" \
  --priority p2 \
  || { echo "post-merge: inbox add FAILED for '<title>' - record in report" >&2; }
```

`add` resolves the same capture-tier file the read commands use - it honors this
repo's `config.post_merge.parking_lot_path` (the same file this narrative is written
to) - so the typed item is reachable by `fno backlog capture list --by-type` /
`tidy` / `promote` / `dismiss`, not just written somewhere they cannot see. It
runs a dedup pre-check: if an open item already covers the same (title + where)
it returns the existing id and mints nothing (JSON `"deduped": true`), so a
re-fire never duplicates an item. Supply `--where` whenever two distinct
follow-ups could share a title: with no `--where` the dedup key is the normalized
title alone, so a generic title can absorb an unrelated item. Node-worthy work
still goes to (b).

**(b) Triage-worthy work -> backlog nodes.** For each item worth doing now,
file a node in the correct project (same judgment you apply by hand):

```bash
fno backlog idea "<concise title>" \
  --details "<why; references PR #$PR>" \
  --priority p2 \
  --project "$PROJECT" \
  --cwd "$REPO_ROOT" \
  || { echo "post-merge: backlog idea FAILED for '<title>' - record in report" >&2; }
```

**`fno backlog idea` does NOT dedupe** - it appends a fresh node every call.
The ONLY idempotency barrier is the Step 5 marker guard, which short-circuits
the entire run (prose AND triage) for a PR already processed. So never run
Step 6 if Step 5 reported the section already exists; that guard, not
`backlog idea`, is what prevents duplicate nodes on a re-fire.

## Step 6b: Handoff slot (offer a handoff before close)

Before the session closes, offer to capture a handoff so end-of-session knowledge
is a prompted step, not something the operator must remember.

- **Interactive.** Offer to generate a handoff document (the `handoff` skill)
  covering what merged and any open threads. On yes, invoke it; on no, skip.
- **Autonomous / headless.** Skip silently (never prompt) - there is no operator,
  and the Step-6 prose follow-ups already captured the durable context.

Advisory and best-effort: a skipped or failed handoff never changes the merge
outcome or the rest of the ritual.

## Step 7: Report

Summarize what happened in one block: PR number, node closed (or "no node"),
retro items harvested, inbox section written (or skipped-idempotent), the
ids/titles of any backlog nodes filed, and the backfill slot outcome (ran /
filed-as-node <id> / none declared).

If any Step 2-6 verb exited non-zero (`RECONCILE_FAILED` / `RETRO_FAILED` /
a failed `backlog idea`), the report MUST include a **Failures** line naming
the verb and its error. A partial run is never reported as a clean success.
In the headless watcher path, also exit non-zero so the failure is logged.

## Step 8a: Reap the original build worker's agent-view row

The `/target` build that shipped this PR left its own `target-<node>-<slug>` row
in `fno agents list`. The daemon retires that worker's *process* after ~1h idle,
but the *row* lingers until then, so the agent view accumulates one dead
`target-*` row per merge. This reaps it, gated behind the same
`config.post_merge.self_reap` opt-in that governs Step 8's self-clean.

Run this BEFORE Step 8: the build worker is a *different* session with its own
row, so it can be reaped while this ritual is still alive to report it; Step 8's
self-clean can tear down the ritual's own session, so it must run last.

```bash
# NODE_IDS: the node id(s) reconcile closed for this PR (from Step 2). Empty
# when the PR mapped to no node -> the whole step is skipped. Best-effort:
# every failure is logged and stepped past, never fatal.
if [[ -n "${NODE_IDS:-}" ]]; then
  # Same coercer as Step 8: default OFF; only an explicit affirmative auto-reaps.
  SELF_REAP="$(fno config get config.post_merge.self_reap 2>/dev/null || true)"
  REAP_ON=0
  case "$(printf '%s' "$SELF_REAP" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')" in
    true|1|yes|on) REAP_ON=1 ;;
  esac

  for NODE in $NODE_IDS; do
    # Resolve by the purpose-built name convention target-<node>-<slug>. NOT by
    # node claim: a finished worker has already released its node:<id> claim, so
    # only the row and its name survive. Skip `status == "live"` rows - a node id
    # can be re-used by a fresh re-dispatch or a G4 de-stub, and killing an
    # actively running worker would be data loss.
    ROWS="$(fno agents list --json 2>/dev/null \
      | jq -r --arg n "target-${NODE}-" \
          '.agents[]? | select(.name | startswith($n))
                      | select(.status != "live")
                      | .name' 2>/dev/null || true)"
    [[ -z "$ROWS" ]] && continue   # no lingering row -> skip silently

    while IFS= read -r ROW; do
      [[ -z "$ROW" ]] && continue
      if [[ "$REAP_ON" == "1" ]]; then
        echo "post-merge: reaping build worker row $ROW (node $NODE)."
        # STOP before RM - `claude rm` on a live agent orphans its supervisor.
        fno agents stop "$ROW" 2>&1 || echo "post-merge: 'fno agents stop $ROW' non-zero (continuing)."
        fno agents rm   "$ROW" 2>&1 || echo "post-merge: 'fno agents rm $ROW' non-zero - clear it manually."
      else
        echo "post-merge: build worker row $ROW (node $NODE) still in 'fno agents list'. Clear when ready:"
        echo "    fno agents stop $ROW && fno agents rm $ROW"
      fi
    done <<< "$ROWS"
  done
fi
```

**Why reuse `self_reap` (no new flag).** Reaping a finished merged build worker's
row is the same action class, risk profile, and default-off caution as Step 8's
self-clean - a second config field would mean a registry FIELD_META entry + docs
regen for zero added expressiveness.

## Step 8: Self-clean (agent-view row)

A background `/fno:pr merged` worker leaves a finished row in `claude agents`:
the daemon retires the *process* after ~1h idle, but the *row* lingers until
reaped, so the agent view accumulates one dead row per merge. This step lets the
worker clear its own row.

Release our reservation now that the durable Step-6 marker is written - it is
the barrier for any later re-run, so the claim's job is done. Best-effort and
non-fatal: release matches on our own holder, so it drops only our claim and
never a successor's TTL-expired re-acquire. Skipping it just lets the claim
linger to TTL expiry.

```bash
# Recompute the SAME holder Step 0.5 used (guard on emptiness, not exit code).
_SID="${CLAUDE_CODE_SESSION_ID:-}"
[[ -n "$_SID" ]] || _SID="$(fno claim session-pid 2>/dev/null || true)"
[[ -n "$_SID" ]] || _SID="$$"
HOLDER="postmerge:pr-${PR}:${_SID}"
fno claim release reconcile:pr-${PR} --holder "$HOLDER" 2>/dev/null \
  || echo "post-merge: reservation release skipped (already gone / holder mismatch); TTL will reap." >&2
```

Run LAST, after the Step-7 report is already emitted - the report must reach the
operator before the session can tear itself down:

```bash
# Only a daemon-managed background session has a row to reap. Interactive runs
# and the headless `claude --print` watcher have no CLAUDE_JOB_DIR / no
# agent-view row, so they skip this silently.
if [[ -n "${CLAUDE_JOB_DIR:-}" ]]; then
  JOB_ID="$(basename "$CLAUDE_JOB_DIR")"
  # Default OFF. A malformed/absent value - or an installed fno too old to know
  # the key - reads empty and is treated as off; only an explicit affirmative
  # auto-reaps. (Mirrors the config.post_merge.self_reap coercer.)
  SELF_REAP="$(fno config get config.post_merge.self_reap 2>/dev/null || true)"
  case "$(printf '%s' "$SELF_REAP" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')" in
    true|1|yes|on)
      echo "post-merge: self_reap on - removing this worker's agent-view row ($JOB_ID)."
      # Best-effort. Step 4 already archived the conductor worktree; `claude rm`
      # only removes a Claude-created `.claude/worktrees/` tree and KEEPS any
      # tree with uncommitted changes (printing its path), so this never loses
      # work. The call may tear down the session mid-command - fine, the report
      # is already delivered.
      claude rm "$JOB_ID" 2>&1 \
        || echo "post-merge: 'claude rm $JOB_ID' returned non-zero (row left in place) - clear it manually."
      ;;
    *)
      echo "post-merge: ritual complete. This worker's row will linger in 'claude agents'."
      echo "    Clear it when ready (removes the row; keeps the transcript + any dirty worktree):"
      echo "    claude rm $JOB_ID"
      echo "    (Set config.post_merge.self_reap: true to auto-clear finished /fno:pr merged workers.)"
      ;;
  esac
fi
```

**Why opt-in (default off).** Auto-removing an agent-view row is the same action
that, applied indiscriminately to every finished session, sweeps up threads the
operator was still using. Scoped to a finished `/fno:pr merged` worker the risk
is far lower - its PR is merged and the ritual is done - but the default stays
print-the-command so rows clear on the operator's cadence. Flip
`config.post_merge.self_reap: true` once trusted.

## Edge cases

- **PR maps to no backlog node** - `reconcile` closes nothing; still write the
  inbox section and triage. Not an error.
- **Cross-project / multiple nodes** - `reconcile` handles per node; write one
  inbox section per repo (this skill operates on the current repo's cwd).
- **Re-run for the same PR** - Step 5's marker guard short-circuits the PROSE
  and TRIAGE (Step 6) before any `backlog idea` fires. `reconcile` and
  `retro run` are independently idempotent; `backlog idea` is NOT, so the
  marker guard is the sole barrier against duplicate triage nodes - do not
  bypass it. The Step-4b backfill slot runs BEFORE the marker guard, so it is
  NOT short-circuited: it re-offers any backfill still surviving in the ledger
  (one already handled was `resolve`-d away, so it does not re-appear).
- **Backfill carve-out with no command** - present it as "(no command)" and
  offer skip/file only; never execute an empty string (Step 4b).
- **Cold catch-up (old merge)** - safe; the section marker still guards dupes.
- **Self-reap on a re-run** - Step 5's marker guard `exit 0`s before Step 8, so a
  re-run never re-reaps. That is fine: the first run already removed the row (if
  `self_reap` was on), and a re-run that short-circuits has no new row to clear.

## Headless invocation

Phase 2's per-repo watcher fires this skill headlessly after a web-button or
`gh pr merge` merge:

```bash
claude --print --dangerously-skip-permissions "/fno:pr merged <pr>"
```

So every step must be non-interactive and safe to re-run. Never prompt; on
missing config, fail loud (Step 1) and exit non-zero so the watcher logs it. In
this path the Step-4b backfill slot files each backfill as a backlog node (it
never runs one), and the Step-6b handoff slot is skipped. The Step-8 self-clean
also no-ops: `claude --print` has no `CLAUDE_JOB_DIR` / agent-view row to reap.

## See also

- Design + locked decisions: `internal/fno/design/2026-05-30-auto-post-merge-ritual.md`
- Post-ship-window design (backfill/handoff slots, dropped Step-0 quiesce): `internal/fno/plans/2026-06-02-target-post-ship-phase.md`
- Reused verbs: `fno backlog reconcile`, `fno retro run`, `fno backlog idea`, `fno carveout list`, `fno carveout resolve`.
- The cross-project message bus (different thing): `skills/inbox/SKILL.md`.
