
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

## Step 0: Resolve the PR

If a PR number was passed as the argument, use it. Otherwise find the most
recently merged PR for this repo:

```bash
PR="${1:-}"
if [[ -z "$PR" ]]; then
  PR="$(gh pr list --state merged --json number,mergedAt \
          --limit 1 --jq 'sort_by(.mergedAt) | last | .number')"
fi
[[ -n "$PR" && "$PR" != "null" ]] || { echo "post-merge: no merged PR found; pass a PR number."; exit 0; }
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
fno backlog reconcile || { echo "post-merge: reconcile FAILED - record in report" >&2; RECONCILE_FAILED=1; }
# full sweep above, or scope it: fno backlog reconcile --node ab-XXXXXXXX
```

`reconcile` is idempotent and a no-op when nothing drifted. If the PR maps to
no node, that is fine (reconcile closes nothing, exit 0) - continue. A
non-zero exit is a genuine failure (e.g. corrupt graph.json): keep going so
the inbox prose still lands, but flag it in the report.

## Step 3: Mechanical triage harvest

```bash
fno retro run --pr-number "$PR" || { echo "post-merge: retro run FAILED - record in report" >&2; RETRO_FAILED=1; }
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
SESSIONS=$(jq -r --argjson pr "$PR" '
  .entries[]
  | select((.pr_number == $pr) or ((.pr_url // "") | test("/" + ($pr|tostring) + "$")))
  | ((.sessions // []) + [.session_id])[]
' "$REPO_ROOT/.fno/ledger.json" 2>/dev/null | grep -vxE 'null|' | sort -u)

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
- **Headless / non-interactive** (`claude --print`, `--dangerously-skip-permissions`,
  `POST_MERGE_NONINTERACTIVE=1`, or no operator to ask): do NOT run anything; file
  a node so the warm-context offer is not lost.
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
- **Headless / non-interactive.** Skip silently - there is no operator to prompt,
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

## Step 8: Self-clean (agent-view row)

A background `/fno:pr merged` worker leaves a finished row in `claude agents`:
the daemon retires the *process* after ~1h idle, but the *row* lingers until
reaped, so the agent view accumulates one dead row per merge. This step lets the
worker clear its own row.

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
