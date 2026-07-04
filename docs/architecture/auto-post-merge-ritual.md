# Automatic post-merge ritual

After self-merging a PR, the same 3-step ritual used to get hand-pasted into a session, with only the per-project path/name/cwd swapped: (1) close + stamp the backlog node, (2) write prose follow-up todos to that project's vault `inbox.md`, (3) triage anything worth doing now into the backlog. The `/fno:post-merge` skill collapses that paste into one verb whose paths resolve from settings, never from the invocation.

Extends the retro / auto-triage feature ([retro-auto-triage.md](retro-auto-triage.md)), which filed graph nodes mechanically but never wrote the per-project prose `inbox.md` (the LLM-judgment step).

## Two gaps this closes

The existing machinery covered ~70% of the ritual but left two gaps:

1. **Prose todos were 100% manual.** `fno backlog reconcile` *explicitly* never writes inbox lines, and `fno retro run` files graph nodes, not the per-project vault markdown at `internal/<area>/backlog/inbox.md`. Writing those prose next-steps requires reading the merged diff and applying judgment, so it stayed a re-pasted prompt.
2. **No trigger fires at merge.** `reconcile` runs on the *next* footnote session in the repo or a megawalk iteration; a GitHub web-button self-merge produces no local event at all. (Phase 2 below; deferred.)

> **Two different "inbox"es.** This skill writes the per-project **vault markdown** `internal/<area>/backlog/inbox.md` (a human reading queue). That is NOT the cross-project message bus `fno mail` (`config.paths.inbox_dir`, thread-per-file). The vault-area name does not equal the project name (`example-pipeline -> internal/etl`, `acme-web -> internal/web`), which is exactly why the path must be explicit config and is never derived.

## Phase 1 - the skill (shipped)

`/fno:post-merge [pr]` is an LLM skill (not a pure CLI verb, because steps 2-3 are judgment over the diff). Sequence:

```
Step 0  resolve PR        explicit arg, else most-recently-merged for the repo (gh)
Step 1  resolve context   config.post_merge.{enabled,parking_lot_path} + config.project.id
                          FAIL LOUD if parking_lot_path unset; distinguish a read failure
                          (fno missing/too old or settings invalid) from "not opted in"
Step 2  completion stamp   fno backlog reconcile [--node ab-XXXX]  (close + stamp)
Step 3  mechanical triage  fno retro run                          (sentinels + carveouts)
Step 4  idempotency        inbox-has-pr.sh <inbox> <pr>  -> exit 0 = already done, skip
Step 5  judgment           read gh pr diff; (a) append dated prose section to parking-lot.md
                          keyed by a PR-number marker; (b) fno backlog idea per item
Step 6  report             one block; a Failures line if any verb exited non-zero
```

### Config (per repo `.fno/settings.yaml`)

```yaml
config:
  post_merge:
    parking_lot_path: internal/<area>/backlog/parking-lot.md   # repo-relative; vault-area != project name
    enabled: true                                              # opt-in per repo (default true)
```

`PostMergeBlock` (`cli/src/fno/config/__init__.py`) defaults `parking_lot_path` to `None` so an un-opted-in repo resolves to an empty value (`fno config get` prints an empty line on exit 0) and the skill fails loud rather than guessing. The validator rejects glob chars, over-length paths, and - because the skill joins this onto the repo root and writes to it - **absolute paths, `~`, and `..` segments** at the schema level (the single enforcement point; the skill's bash guard is defense-in-depth for a stale installed `fno`).

### Idempotency

Each prose section starts with an HTML-comment marker `<!-- post-merge:pr-<N> -->`. `skills/pr/scripts/inbox-has-pr.sh` greps for that marker (fno-free, so it is deterministic across CLI versions); exit 0 means "already written, skip". This marker guard is the *sole* idempotency barrier for the judgment half: `fno backlog reconcile` and `fno retro run` are independently idempotent, but `fno backlog idea` is not (it appends a fresh node every call), so Step 5 must never run once Step 4 short-circuits.

### No new mutation primitives

The skill reuses `fno backlog reconcile`, `fno retro run`, and `fno backlog idea`. The only new code is the read-only `config.post_merge` schema block and the skill + its idempotency helper.

## Phase 2 - the trigger (deferred, gated)

A per-repo **launchd watcher** polls `gh pr list --state merged` on an interval, tracks a per-repo watermark, and fires the skill headlessly (`claude --print --dangerously-skip-permissions "/fno:post-merge <pr>"`) for each new merge. A `gh pr merge` shell wrapper is optional instant-fire sugar for terminal merges.

A GitHub Action cannot do this: the ritual needs local state (`~/.fno/graph.json`, the Obsidian vault, the repo working copy), and `/schedule` cloud agents lack the local creds. Polling locally is required.

Phase 2 is intentionally **not shipped with Phase 1**: it installs a plist to the user's machine, which is gated on showing the operator the plist first. Phase 1 lands pure value with no system changes. Phase 2 is captured as a follow-up.

## Files

| Path | Role |
|---|---|
| `skills/pr/references/merged.md` | the skill |
| `skills/pr/scripts/inbox-has-pr.sh` | fno-free idempotency guard (marker grep) |
| `cli/src/fno/config/__init__.py` | `PostMergeBlock` (parking_lot_path, enabled) + validator |
| `cli/tests/unit/test_config_post_merge.py` | schema + `fno config get` resolution tests |
| `cli/tests/unit/test_post_merge_inbox_idempotency.py` | idempotency-guard tests |
