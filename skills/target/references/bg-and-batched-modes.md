# Background dispatch (`bg`) and batched mode (`batched`)

Read this when the argument leads with `bg <node...>` / `bg --all-ready` (fire-and-forget dispatch) or `batched <node>` (a batch-lane member run dispatched by the active-backlog daemon). Neither is a normal pipeline; each is a constrained flow.

## Background Dispatch (`bg`)

If `bg <node...>` or `bg --all-ready` is passed (US5 targeted bg-dispatch): this is a **fire-and-forget** dispatch of one or more `ready` backlog nodes as fresh `claude --bg` `/target` workers. Do NOT init a pipeline, do NOT resume, do NOT write `target-state.md` — the planning session stays usable for more `/think` + `/blueprint` while the dispatched workers run on their own (the fresh bg process IS the context "clear"; the agent cannot `/clear` itself). Run the dispatch primitive, relay its per-node outcome lines verbatim with the `fno agents logs <name>` hints, then STOP:

```bash
bash "${SKILL_DIR}/scripts/dispatch-node.sh" <node...|--all-ready> [--flags "<size/modifiers>"] [--allow-merge] [--max N] [--permission-mode <mode>]
```

Each line is one of `launched` / `already-running` / `parked` / `skipped-done` / `failed` / `deferred-cap`, followed by a `summary:` line; never silent. Locked semantics:
- Under `--all-ready` only `ready` nodes dispatch. An **explicitly-named** node also dispatches when its status is `idea` (the triage pile; naming it is the human's vet, the worker runs think->blueprint->do); `blocked`/`deferred` are always **parked** (pre-planned future work), never launched. A node a live worker already holds (`node:<id>` claim) is **already-running**, never double-dispatched.
- Each worker launches via `fno agents spawn --provider claude --substrate bg` (the detached `claude --bg` thread; Group 1 ab-8b3e4fe0 moved creation off `ask`), NEVER `--bare`/`-p` (subscription lane only). The `--substrate bg` key is load-bearing: the post-x-3ab8 default substrate is `pane` (owned-PTY), which would stall a fire-and-forget dispatch at a placement prompt (x-2c27).
- `no-merge` is injected by default (an autonomous worker lands a PR for review, not an auto-merge); pass `--allow-merge` to opt out.
- A dispatch failure is surfaced and leaves the node `ready`/re-dispatchable; it never reports a launch that did not happen, and never falls back to `-p`/API-credit billing.

Multi-CLI: `/target bg` preserves its Claude semantics and requires `claude --bg` + `fno agents`; on a CLI without them the dispatch reports the failure and the node stays `ready` (degrade, never fake a launch). Separate Codex build dispatches use a prose brief through an owned-PTY (`pane`) or one-shot `headless` spawn, never a literal `/target`; unsupported non-Claude slash-command passthrough is rejected before spawn.

**Spawn substrate axis (x-2c27).** `fno agents spawn --substrate <pane|bg|headless>` names one axis - where an off-thread `/target` runs: `pane` (owned-PTY drivable pane; the default), `bg` (detached `claude --bg` thread; what `/target bg` dispatches), `headless` (a one-shot `claude -p` / `codex --exec` / `agy -p`). `/target bg` is the autonomous-dispatch verb because only `bg` is both detached AND able to run the full multi-phase pipeline. For an attended drivable pane use `/agent spawn /target <node>` (substrate `pane`); `headless` (one-shot) does not fit a multi-phase `/target` run and is a `/agent spawn headless` surface for cross-provider one-shots, not a `/target` dispatch verb. `bg` is claude-only; a non-claude `bg` is a hard error pointing to `headless`.

## Batched mode (`batched <node>`)

If ARGUMENTS lead with `batched <node>` (dispatched by the active-backlog daemon when `config.batch.enabled` is on and the batch policy says join-or-start), this is a **batch-lane member run** (x-6cdf). The member's commits land on a **shared batch branch** and ship via ONE batch PR, not a per-node PR. It is a constrained `/target`:

1. **Skip cold-start.** The daemon already created/selected the batch worktree and passed `TARGET_BATCH_WORKTREE` + `TARGET_BATCH_BRANCH`. Do NOT run `fno target start` (that would mint a second worktree). Instead `cd "$TARGET_BATCH_WORKTREE"` and run `TARGET_BATCHED=1 fno target init --input <node>` so the manifest carries `batched: true` (loop-check reads it to terminate as `DoneBatched`, not a hang). Do NOT set `no_ship`/advisory - a batched unit is neither.
2. **Implement + commit atomically to the shared branch.** Same per-task commit discipline as a normal run; the commits accumulate on `$TARGET_BATCH_BRANCH` alongside the batch's other members. Run `/review` + `fno test` as usual.
3. **Join the batch + mark the node.** After the work is committed: `fno backlog batch join --domain <domain> --node <node> --summary "<one-line>"` (adds the member to the batch state), then read the batch id (`fno backlog batch status --domain <domain>` -> `batch_id`) and `fno backlog update <node> --batch <batch_id>` (marks the graph node so `next`/`ready` stop selecting it). Mark the node **before** promising.
4. **Do NOT create a PR.** Batched mode skips the ship phase entirely - the batch's single `/pr create` (driven by `fno backlog batch ship` when the batch closes) opens one PR for all members. Emit `<promise>MISSION COMPLETE: batched member committed to <branch></promise>`; loop-check sees `batched: true` + the promise and terminates the member as `DoneBatched`.

The daemon consults `should_close` after the member returns and, when the batch closes (full / next node is a different domain / drain), runs `fno backlog batch ship` to open the batch PR and record the shared PR ref on every member. On merge, `fno backlog reconcile` closes each member by that shared pr_number. If a batched member FAILS/BLOCKS, the daemon abandons the batch (`fno backlog batch ship` requeues members as individual PRs). v1 has no revert surgery - a bad batch costs at most the same CI as no batching.
