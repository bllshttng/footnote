
# Post-merge ritual

The ~90% of this ritual that is pure CLI orchestration now runs as ONE idempotent
hidden verb: `fno pr ritual <pr>`. This body runs that verb, reads its receipts,
and does only the **judgment residue** itself - the parts that need a human (or a
headless LLM leg) reading the merged diff.

1. **Mechanical core** - `fno pr ritual <n>` closes the node, harvests the retro,
   advances auto-continue, closes the skill-diff loop, syncs the canonical
   checkout, archives the worktree, and reaps dead agent-view rows. One run, one
   receipt block, never a swallowed failure.
2. **Judgment residue** (this body) - triage this PR's deferral-born nodes, and
   write parking-lot prose / file triage nodes from the merged diff.

The verb replaced a 1117-line bash ritual whose snippets carried a proven
zsh/ugrep silent-misfire class (x-f47f: `${VAR:+...}` word-splitting,
empty-alternation grep). Python builds each argv explicitly, so that whole class
is structurally gone and every leg's failure is loud - there is no `|| true`
anywhere in the verb.

## Prerequisites

- The PR is already merged (this runs *after* merge, by hand or from a watcher).
- `.fno/config.toml` sets `config.post_merge.parking_lot_path`. Check before a
  merge with `fno config doctor --post-merge`; scaffold it with
  `fno setup post-merge`. Without it the verb's judgment leg reports
  `parking_lot=unset` and prose is skipped (never guessed).
- `gh` is authenticated for reading the merged diff.

## Autonomous mode (no operator present)

Merge-detection dispatches this ritual as a background worker with no human to
prompt (the watcher passes an `autonomous` token; a manual headless run sets
`POST_MERGE_NONINTERACTIVE=1`). In that mode:

- Run `fno pr ritual <n> --autonomous` (the flag also turns on under
  `POST_MERGE_NONINTERACTIVE=1`).
- The verb spawns the judgment leg itself as ONE headless one-shot
  (`fno agents spawn --substrate headless`) when its inputs are non-empty, never
  a `claude --bg` thread (epic Locked Decision 9).
- **Never call `AskUserQuestion`** in autonomous mode. Every judgment slot below
  has a no-prompt branch; self-end after the report.

## Step 1: Resolve the PR

If a PR number was passed, use it. Otherwise the verb auto-resolves the most
recently merged PR for this repo (no argument needed).

## Step 2: Run the mechanical core

```bash
fno pr ritual <pr>            # attended: you do the judgment below
fno pr ritual <pr> --autonomous   # autonomous: verb spawns the judgment leg
```

The verb prints one receipt line per leg, then exits non-zero if any leg failed:

```
step=mutex status=ok detail=acquired
step=reconcile status=ok detail=closed=1
step=plan-reconcile status=ok detail=...
step=session-add status=ok detail=...
step=retro status=ok detail=...
step=advance status=ok detail=exit=0 (3 progress lines)
step=skill-diff status=ok detail=...
step=sync-canonical status=skipped detail=not configured
step=archive status=ok detail=archived
step=judgment status=ok detail=deferred-to-skill (attended); deferred=2 files=14 lines=320 parking_lot=set bar=above
step=reap-rows status=ok detail=no lingering rows
```

**Read the receipts.** Each `status=failed` names a real failure (exit code +
tail line); the verb exits non-zero so a partial run is never readable as
success. A re-run is resume-safe: completed legs no-op, failed legs retry, no
leg double-applies (reconcile/retro/advance/sync/skill-diff each dedup on their
own markers and claims).

The verb absorbed four prior ritual bugs (each verified by a test in
`cli/tests/`): **x-c4ff** (only real verbs are called - `skill-diff reconcile`,
`pr sync-canonical` both exist; no dangling references), **x-fb99**
(`parking_lot_path` resolved against the canonical root, never a worktree cwd),
**x-adf9** (canonical-sync pipes closed + timeouted so a trailing `fno restart`
daemon cannot wedge it), **x-0d66** (the advance leg bounded + streamed).

## Step 3: Judgment residue (attended only)

The verb prints a `step=judgment` line carrying its computed inputs, e.g.
`deferred=2 files=14 lines=320 parking_lot=set bar=above`. In an attended run it
defers the judgment to this body (`deferred-to-skill`). Do both steps below.

In autonomous mode (`--autonomous`) the verb already spawned the judgment leg as
a headless one-shot - skip this section and go to Step 4.

### 3a. Warm-window triage of this PR's deferral-born nodes

`deferred=N` in the judgment line is the count of this PR's open deferral-born
nodes (the ones `/pr create` filed from this PR's "Out of scope" section). If
`N=0`, skip. Otherwise list and decide each:

```bash
fno backlog find 'deferred from PR #<n>'
```

For each open node, offer a one-touch decision (in autonomous mode: log each as
`undecided` and continue - no regression over today):

- **promote** -> `fno backlog rank <id> --top`
- **keep** as filed -> no-op
- **defer** explicitly -> `fno backlog defer <id>`
- **supersede** -> `fno backlog supersede <id> --by <other-id>`, or
  `fno backlog done <id>` if moot.

### 3b. Read the merged diff and apply judgment

```bash
gh pr diff "<n>"
gh pr view "<n>" --json title,body,mergedAt
```

The judgment line's `bar=above|below` (files/lines vs the parking-lot bar) tells
you whether prose is warranted; read the diff regardless and decide with judgment.

**(a) Parking-lot prose -> `$PARKING_LOT_PATH`.** `parking_lot=set` means the
verb resolved the canonical parking-lot file (resolve it yourself the same way if
needed: repo-relative path under the canonical root, never the worktree). Append
a dated section keyed by the PR number - the `<!-- post-merge:pr-<N> -->` marker
on the first line is the idempotency guard (a re-run is a full no-op):

```markdown

<!-- post-merge:pr-123 -->
## Post-merge follow-ups - PR #123 (2026-07-23)

_<pr title>, merged 2026-07-23. Written by /fno:pr merged._

- A thing to keep an eye on now that this shipped.
- [ ] a decision/sign-off only the maintainer can make #jc
```

Write the section with an **append-only redirect** (`printf '%s' "$section" >> "$PARKING_LOT_PATH"`),
never an Edit-tool read-modify-write. O_APPEND is what makes the shared file
safe without a lock: same-PR concurrency is already excluded by the verb's mutex
claim, and two rituals for different PRs each do one atomic append.

Keep the narrative to genuine context: a thing to watch, plus `#jc` items only
the maintainer can do (a decision, a sign-off, a manual setup). Implementation
work does NOT get `#jc`.

For each small follow-on below a full node, emit a TYPED `fu-*` line instead of a
freeform bullet so it is deduped and visible to `fno backlog capture list`:

```bash
( cd "$(git rev-parse --git-common-dir | xargs dirname)" \
  && fno backlog capture add "<concise title>" --source "PR#<n>" \
     --why "<one line>" --where "<file/area>" --priority p2 )
```

**(b) Triage-worthy work -> backlog nodes.** For each item worth doing now, file a
node in the canonical project:

```bash
fno backlog idea "<concise title>" --details "<why; references PR #<n>>" \
  --priority p2 --project "<project>" --cwd "<canonical-root>"
```

`fno backlog idea` does NOT dedupe - the per-PR marker is the sole barrier
against duplicate triage nodes on a re-fire, so never run 3b if the marker already
exists.

### Filing rule: route by provenance

The per-PR marker is the right idempotency scope for **diff-derived** findings
(the diff is processed once). It is the WRONG scope for **environment failures**
(a ritual leg itself breaking re-fires on every merge with a fresh marker).

- **Diff-derived** -> `capture add` (below-node) or `backlog idea` (node-worthy),
  as above. Run `fno backlog find` first (search-before-idea, no dupes).
- **Environment failure** (any `status=failed` receipt line) -> NEVER
  `backlog idea`. Emit one deduped capture item keyed on the failing leg name:

  ```bash
  ( cd "<canonical-root>" && fno backlog capture add "ritual failure: <step>" \
      --where "<step>" --source "PR#<n>" --why "<the receipt detail>" )
  ```

  The dedup key is (title + where), both from the failing step name - so the Nth
  ritual hitting the same broken leg returns the existing `fu-*` id instead of
  minting a clone. Promotion to a node happens once at triage via
  `capture promote`.

### 3c. Handoff slot (offer before close)

- **Attended.** Offer to generate a handoff (the `handoff` skill) covering what
  merged and open threads. On yes, invoke it; on no, skip.
- **Autonomous.** Skip silently - the parking-lot prose already captured the
  durable context.

Advisory: a skipped or failed handoff never changes the merge outcome.

## Step 4: Report

Summarize in one block: PR number, the verb's receipts (node closed / no node,
retro harvested, sync/archive outcome), parking-lot section written or skipped,
nodes/capture-items filed, and any handoff.

**Carry exactly one `Failures:` line, always** - including the literal
`Failures: none` on a clean run. Derive it from the verb's `status=failed`
receipt lines (a partial run must never read as clean):

```bash
# From the verb's stdout, the failed steps (empty -> none).
FAILS="<comma-separated failed step= lines, or 'none'>"
printf 'Failures: %s\n' "$FAILS"
```

In the headless watcher path, exit non-zero when any leg failed so the watcher
logs it.

Then file the environment failures as deduped capture items only (3b's provenance
rule), never as `backlog idea` nodes.

## Edge cases

- **PR maps to no node** - `reconcile` closes nothing; still do 3b. Not an error.
- **Re-run for the same PR** - the verb's legs are individually idempotent, and
  the `<!-- post-merge:pr-<N> -->` marker guards the judgment prose/triage. Do
  not re-run 3b if the marker already exists.
- **Empty diff (merge commit with no file changes)** - the verb reports
  `bar=below`; treat as below the parking-lot bar, never an error.
- **Run from inside the merged PR's own worktree** - the archive leg defers to
  `fno worktree cleanup --merged --apply` (run from canonical); it never
  self-removes.

## See also

- The verb: `fno pr ritual` (`cli/src/fno/pr/_ritual.py`) and its command in
  `cli/src/fno/pr/cli.py`.
- Plan + locked decisions:
  `internal/fno/plans/20260723-post-merge-mechanical-core-x-bbde.md`.
- Design + locked decisions (original ritual):
  `internal/fno/design/2026-05-30-auto-post-merge-ritual.md`.
- Reused verbs: `fno backlog reconcile`, `fno retro run`, `fno backlog advance`,
  `fno skill-diff reconcile`, `fno pr sync-canonical`, `fno backlog find`,
  `fno backlog capture add`, `fno carveout`, `fno agents spawn`.
- The cross-project message bus (different thing): `skills/mail/SKILL.md`.
