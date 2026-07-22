# Post-merge watcher (Phase 2 of the post-merge ritual)

`ab-4e9fb05a`. Phase 1 ([`/fno:pr merged`](../../skills/pr/references/merged.md), PR #384) closes + stamps a backlog node, harvests retro/carveout items, and writes prose follow-ups to the repo's vault inbox - but only when a human invokes it. A **web-button merge** (the GitHub UI) produces no local event, so the ritual never fires for those PRs.

This directory adds a per-repo macOS `LaunchAgent` that polls for merged PRs on an interval and fires the ritual headlessly for each new merge, catching web-button (and other out-of-band) merges.

## Files

| File | Purpose |
|------|---------|
| `watch.sh` | Poll merged PRs, fire the ritual for each PR merged since the watermark, advance the watermark only on success. |
| `com.fno.postmerge.plist.template` | LaunchAgent template (double-brace placeholders rendered per-repo by `install.sh`). |
| `install.sh` | Render + write the plist for THIS repo, then **print** it and the `launchctl load` command. Never loads it. |
| `uninstall.sh` | Unload + remove the plist. |

## How `watch.sh` works

1. Reads the per-repo watermark at `.fno/.post-merge-watermark` (last-processed PR `mergedAt`, ISO-8601). Empty on first run.
2. `gh pr list --state merged --json number,mergedAt,title` for the repo.
3. Selects PRs with `mergedAt` strictly after the watermark, **oldest-first**.
4. Fires the ritual for each (`claude --print --dangerously-skip-permissions "/fno:pr merged <pr>"`), **waited** so the exit status is known.
5. Advances the watermark to a PR's `mergedAt` **only after that PR's fire succeeds**. A failed fire leaves the watermark, so the merge is retried next poll. A mid-batch failure stops the run at the last successful PR.

The fire is run synchronously (not detached): the watermark may only advance on a known-successful fire, which a detached fire cannot report. The non-blocking guarantee lives at the launchd layer - the agent runs `watch.sh` off your interactive path.

### Env overrides

| Var | Effect |
|-----|--------|
| `POST_MERGE_MODEL` | Model for the per-merge `claude --print` ritual fire. Default `claude-haiku-4-5` (the ritual is mechanical, so Haiku keeps per-merge cost low). Set to `sonnet` for stronger triage judgment, or empty to inherit the CLI default. Set at install time to bake it into the plist. |
| `POST_MERGE_POLL_LIMIT` | `gh --limit` (default 100). |
| `POST_MERGE_WATERMARK_FILE` | Override the watermark path. |
| `POST_MERGE_PRS_JSON` | Supply the merged-PR JSON directly (bypasses `gh`; used by tests). |
| `POST_MERGE_FIRE_CMD` | Command run with the PR number appended instead of `claude --print ...` (used by tests; point at `true`/`false`). |

### Cost

The watcher itself spends **zero** Claude tokens: `watch.sh` is plain bash + `gh`, run by launchd on the interval. Claude is invoked only when a NEW merge is detected - one short, fresh `claude --print` per merge (default Haiku), which runs the ritual for that one PR and exits. So cost scales with the number of web-button merges, not with poll frequency.

## Install (human-gated)

```bash
# 1. Render + write the plist (does NOT load it):
POST_MERGE_INTERVAL=600 bash scripts/post-merge/install.sh
# 2. Review the printed plist.
# 3. Load it YOURSELF:
launchctl load ~/Library/LaunchAgents/com.fno.postmerge.<repo>.plist
```

`install.sh` deliberately never loads the agent. It installs a `LaunchAgent` that runs `claude --print` headlessly on a timer - system-touching - so a human reviews the rendered plist and loads it. `RunAtLoad` is `false`, so loading is side-effect-free until the first interval.

`POST_MERGE_INTERVAL` is the poll cadence in seconds (default 600; 5-15 min is a reasonable, prompt-cache-window-aware range).

## Uninstall

```bash
bash scripts/post-merge/uninstall.sh
```

## Optional: fire instantly on a terminal merge

The watcher catches every merge within one poll interval. If you want a **terminal** merge to fire the ritual immediately (no wait), add a shell wrapper to your `~/.zshrc` / `~/.bashrc`. This is documented, not auto-installed - adapt and opt in yourself:

```bash
# Wrap the gh merge subcommand so a successful terminal merge fires the ritual
# right away. `command gh ...` calls the real CLI; the ritual is idempotent
# per-PR (marker-keyed), so a later watcher poll for the same PR is a no-op.
fno-merge() {
  command gh pr merge "$@" || return $?
  # Resolve the PR number: explicit numeric arg, else the current branch's PR.
  local pr
  for a in "$@"; do [[ "$a" =~ ^[0-9]+$ ]] && pr="$a" && break; done
  [[ -z "$pr" ]] && pr="$(command gh pr view --json number --jq .number 2>/dev/null)"
  [[ -n "$pr" ]] && claude --print --dangerously-skip-permissions "/fno:pr merged $pr"
}
```

Run `fno-merge <pr-number>` (or `fno-merge` on a PR branch) instead of the bare merge subcommand. Leave it out entirely if you prefer to let the interval watcher handle everything.

## Tests

`tests/post-merge/test_watch.sh` exercises the watermark contract (fire oldest-first, no-reprocess at/below watermark, failed-fire-does-not-advance, mid-batch-failure-stops) via the `POST_MERGE_*` seams - no real `gh`/`claude` needed. Runs under macOS `/bin/bash` (3.2).
