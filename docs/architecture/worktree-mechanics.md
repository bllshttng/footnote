# Worktree mechanics

The parts of the worktree contract that only some sessions reach.
The hook's refusal shape, removal and pruning, and the three enforcement mechanisms.

The always-loaded half lives in [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md).
That file is the authority on placement: where worktrees go, the policy values and their precedence, and the forbidden locations.
This file is the authority on the machinery.

When you edit the hook, remove or prune a worktree, or trace why a location gate fired, read this first.

## The refusal shape

Both creation paths honor `policy = "never"`. The `WorktreeCreate` hook resolves the policy through `fno agents workspace worktree policy`, so there is one resolver and no second precedence implementation.

The refusal SHAPE is load-bearing and counter-intuitive.
It differs by payload shape.

**path-present** (CC sends `.path`): the hook pre-creates the directory and is reaped.
A non-zero exit falls back to CC's default flow.
So exiting non-zero creates the very worktree you meant to block.
The supported abort is **exit 0 with empty stdout**, which CC reads as "no successful output".

**name-only** (no `.path`, as EnterWorktree sends): the hook does not pre-create, and `test -d` finds nothing at fire time.
A non-zero exit defers.
That fallback does NOT hold here.
The caller gets a hard failure and no worktree, which is why the rule file says to `git worktree add` first and enter by path.

The gate runs before the hook's own `cd`.
An absent path fails at that `cd` first and takes the fallback branch, so gate placement is load-bearing too.
The gate fails open on anything but an affirmative `never`, because a stale `fno` must not break interactive `claude --worktree`.

An in-session `claude --worktree` spawn is a child (`CLAUDE_CODE_CHILD_SESSION`) and never fires `WorktreeCreate`.
Test with a top-level run.

When `worktrees_base` is set, the two paths still diverge on WHERE. Autonomous dispatch (`fno agents workspace worktree ensure`) stays harness-native unless `policy = "external"`.
The hook relocates off `worktrees_base` directly.

## Claude Code's worktree Bash isolation

With `worktree.bgIsolation: "worktree"`, Claude Code runs a static analyzer over a worktree-isolated session's Bash commands. That setting is Claude Code's default since 2.1.222. This repo sets it in `.claude/settings.local.json`. The analyzer is Claude Code's own code. Footnote cannot widen or narrow it, and its refusal text is fixed. What footnote owns is the command its hooks and skills tell an agent to run. Those must be shapes the analyzer admits.

The predicate was probed live on CC 2.1.251 from a worktree session. When a command holds anything the analyzer cannot statically resolve, it is refused as too complex to verify. The refused constructs: environment-variable expansion (`echo "$VAR"`), command substitution (`$(...)`), arithmetic expansion (`$((n+1))`), and loops (`while`, `for`). The admitted constructs: plain commands, literal local variables (`i=2; echo "$i"`), pipes, `;`/`&&` compounds, redirects, and `bash /abs/path/script.sh` file indirection.

Two facts agents keep getting wrong. The refusal's mention of the redirect is generic wording, not the trigger. The trigger is the construct the analyzer cannot parse. The analyzer is also NOT a path boundary. An absolute-path write outside the worktree runs ungated, including into the canonical checkout. Footnote's own hooks are that boundary (`worktree-write-protect.sh`, `git-protection.py`).

The sanctioned escapes, in order. Read env with `printenv VAR` instead of `echo "$VAR"`. Wait on CI or a review with `fno do pr wait <N> --until settled|review` instead of a hand-rolled poll loop. An inline `while`/`$(...)` watcher is refused, so no fno surface can instruct one. For a genuinely complex one-liner, put it in a file and run `bash <file>`.



A harness or Codex substrate with no native worktree transition degrades to the Footnote-owned `<state_dir>/worktrees` fallback, normally `~/.fno/worktrees`. That fallback is Footnote's own allocation. It does not inherit an external allocator configured by `worktrees_base`, so a repo that sets the base still lands there under `harness-native`. For that reason `fno agents workspace worktree ensure` requires `--harness` and never guesses the substrate.

## Removal

```bash
fno agents workspace worktree archive <name|path>           # the public guarded path
bash scripts/setup/archive-worktree.sh <name|path>   # the shared implementation
```

The CLI and compatibility lifecycle entry delegate to the script above. They expose `--force`, `--yes` (skip kill prompt), and `--delete-branch` without copying the checks.

Without `--force`, archival refuses on dirty state, unpushed commits, live sessions, unreadable process snapshots, failed salvage, app ownership, canonical checkout, and removal-time changes. A retired `--kill-orphans` flag is on this list too. Parsing it prints one refusal line and sets nothing, because release by parentage killed real pane keepers.

With `--force`, the script measures and prints every dirty path. It prints each unpushed commit's abbreviated SHA and subject. It prints positive live-session evidence before removal. It still refuses unverifiable evidence. Its final receipt distinguishes discarded worktree state from preserved or deleted branch data.

NEVER `rm -rf` a worktree, which leaves dangling refs.

Post-merge pruning is automated. Every gh-confirmed MERGED archive leg first mints a TTL reap order (`reap:pr-<n>`, 24h, the TTL ceiling), even when the ritual runs from the canonical checkout and cannot resolve the merged worktree. Minting clears the sweep stamp best-effort so the next idle tick can pay the order immediately. The daemon's six-hour worktree sweep checks each repository's claim scope and runs that repository's pass with `--apply` while one of its orders stands, report-only otherwise: a timer tick alone still removes nothing. An unreadable order probe skips that repository and emits its exit status plus first stderr line; it never collapses into report-only. The sweep's own guards (reapable, live claim, rooted processes) decide tree by tree, so an order never forces a protected tree. If the mint fails, the archive leg fails loudly; if direct archival cannot complete after a successful mint, the standing order preserves the owed work. `fno agents workspace worktree cleanup --merged` (dry-run by default, both removal modes) sweeps landed ones by hand with `--apply`.

Every removal emits one `worktree_removed` event row (path, caller, claim read, reason). The row mirrors to the machine-global journal. Before this emission landed no removal path recorded anything, so a lost tree left no attributable evidence.

### The DIRTY bucket under a done node (law d-cfcf5a8e)

The merge reaper removes a done-and-merged node's tree whatever its git status, keeps the branch, and holds only unpushed work: a HEAD that is not an ancestor of origin/main is not dirt, and an unreadable origin holds too. The recoverability argument is the ruling: the branch is pushed, the transcript persists, the node records the PR, so removal is cheap and reversible and hoarding is not. An OPEN node's tree keeps the old boundary, report only; setup's own symlinks into canonical are the one discounted case (`reason=setup-links`). While a request's tree is held (unpushed, or a removal that failed) the request stays pending and echoes the hold at most once an hour, so a later pass takes the tree once the hold clears instead of tombstoning it forever.

### The unborn bucket

A worktree added on a new branch off `origin/main` has zero commits of its own until its first commit. The branch is then a literal ancestor of main, so the gate read it clean plus merged: exactly the bucket the sweep prunes. Measured 2026-09-12: 29 `worktree_removed` rows in one night carried that read. Three of them were live dispatches mid-setup. A worker whose cwd vanishes goes quiet with no error anyone can read. The gate now refuses such a tree (`reapable=no reason=unborn`, row `kept (unborn)`) inside a 30-minute setup window. The window is measured from the `.git` file's mtime, which git writes once at `worktree add` and never rewrites.

Neither reading alone is safe. Bare zero-commits-ahead also reads a branch whose work landed and was then rebased. Refusing that bricks the reaper for every merged tree. The branch reflog is the discriminator: creation writes one entry, and any commit, reset or rebase writes more. The reflog read alone can also hold a landed tree whose reflog expired (90 days by default). So the tree's age is the second reading, and an old unborn tree is still reclaimable. A detached HEAD answers not-unborn: content judges those, and the sweep counts their unpushed commits. An unanswerable probe answers unborn: a probe that cannot read never authorizes a removal. One carve-out: an archive that names a single tree is a human decision, so the manual archive lifts the refusal. The bulk reapers keep it.

### Who occupies a worktree

The process table plus the lsof cwd snapshot (`_wt_pids`) is the only truthful occupancy source. Every classification reads that enumeration. None invents a second one.

Never ask a recorded cwd which tree a worker occupies. The agents-registry `cwd` field is the spawn directory, and the claude job `state.json` `cwd` field is the spawn directory too. Measured 2026-09-11: 29 of 30 alive registry rows and 17 of 17 live bg jobs read the canonical checkout while their sessions wrote inside worktrees. A hold detector built on either field named its own live worktree free. The classifier is `scripts/lib/worktree_occupancy.py`. It never reads those fields. The bridge is `scripts/lib/worktree-occupancy.sh`.

`claude bg-spare` argv is identical for a live session and an idle spare, so argv alone can never mark a spare reapable. The identity is the daemon's rendezvous socket farm (`session_procs.bg_socket_pid_map`), which joins a pid to its job id. A join miss holds.

Four classes release a tree. One: a keeper the keeper lane names REAP. Two: an orphaned claude Bash-tool shell, ppid 1 with argv `zsh|bash|sh -c source <home>/.claude/shell-snapshots/...`. Three: a claude job in a terminal state whose transcript is silent past `STALLED_AFTER_S` (7200 s). Four: any descendant of these. Every other process keeps the tree. A hit the classifier cannot place keeps the tree with reason `unclassified: <name>`. A pid with no ps row keeps it with reason `no ps row`. Absence of a recognised holder is never proof a tree is free.

A kept tree prints the evidence per pid:

    kept (processes: 1 held, 1 inert)
        26287 holds claude job abc123 working, transcript 79s | claude bg-spare --bg-spare ...
        26288 inert socket absent, no registry row claims it | fno-agents-worker --pane ...

An all-inert tree falls through to `would-archive` (dry run) or removal. `archive-worktree.sh` classifies its own fresh re-enumeration a second time and signals only `terminate` rows. `retire` rows carry a claude job id, and the sweep releases those job records through `claude rm`.

## Commit-time salvage refs

`scripts/setup/setup-worktree.sh` installs a shared `post-commit` dispatcher that runs the committing worktree's `hooks/worktree-salvage-ref.sh`. Every commit advances a local `refs/fno/salvage/<worktree>` ref so a detached or provider-killed worktree stays recoverable without a network dependency.

Remote mirroring is on by default: `setup-worktree.sh` sets `fno.salvageRemoteMirror true` in every worktree it prepares. Each commit then reaches `refs/fno/salvage/<worktree>` on origin within seconds, so death before a PR loses nothing. To opt a worktree back out, for example an air-gapped clone, run `git config --local --unset fno.salvageRemoteMirror`. The local salvage ref stays active either way, and a remote failure never blocks the commit. When `fno do target start` dispatches, it reads `origin/feature/<node>` and the salvage ref and continues whichever is ahead of main. See `worktree ensure`.

## Cargo build storage

Cargo writes intermediates outside the checkout, and final binaries inside it. The tracked `.cargo/config.toml` sets `build.build-dir = "{cargo-cache-home}/build/{workspace-path-hash}"`, so a bare clone sends every intermediate to `~/.cargo/build/<h2>/<hash>` and leaves `crates/<crate>/target` holding only the final binaries. fno exports `CARGO_BUILD_BUILD_DIR=<base>/{workspace-path-hash}` to the doctor-test child env, the plugin-install env exports, and the shell rc. That moves the same layout under `<base>`: `paths.cargo_targets_base`, default `~/.fno/cargo-build`. The hash is per workspace root. Every checkout gets its own build dir. Every worktree gets its own. The two workspaces inside one tree hash separately. Parallel builds therefore never serialize on a shared directory. Never put `build.target-dir` in `.cargo/config.toml`: that key has no workspace hash. Every sibling worktree then shares ONE build directory, and the fleet's builds serialize again. Harness plugin installs copy from the filtered stage (`fno config plugin install`), so the caches never carry build output either.

The cleanup sweep (`fno agents workspace worktree cleanup --cargo-targets`) reclaims both halves. Its inventory walks `crates/*/target` per registered worktree and the sharded hash dirs under the build base. A candidate must carry Cargo's own `CACHEDIR.TAG` on the nearest ancestor, whatever the directory is named. The base is fno-owned and the tag is Cargo's marker, so a path failing either conjunct is counted as `link-not-owned` and never deleted. A symlink under a registered tree whose resolved directory sits under a managed base and carries the tag is deleted resolved-dir-first, then the link. These are the legacy caches the retired post-hoc relocation verb left behind. Select only `crates/*/target` by path, never by directory name. `cli/src/fno/target`, `skills/target` and `tests/target` are source dirs, and a name-based sweep deleted 66 of them across 26 worktrees on 2026-09-02.

Setup runs the sweep with `--apply` after linking (inspect first by omitting `--apply`). It reaps inactive targets older than seven days first, then the oldest inactive candidates until allocated bytes sit at or below the effective ceiling. The ceiling is `min(64 GiB, --free-share-pct percent of free disk space)`, default 50 percent. A nearly full disk therefore tightens the ceiling instead of leaving an under-cap `ok` verdict on a full volume. `FNO_CARGO_FREE_BYTES` overrides the free-space read and `FNO_CARGO_TARGETS_BASE` the build base, for tests. A live target claim or rooted process protects its worktree. A live workspace's build-dir hash dir is protected too. The sweep runs `cargo metadata` for every live worktree's workspaces. That command reports the resolved `build_directory` since Cargo 1.91. No dir in that set is ever deleted. If any metadata read fails, the whole build-base lane is protected for that run. A blind sweep is the one mistake this lane cannot undo. Protected bytes that prevent the ceiling return `over-cap-protected` instead of deleting an active build, and the summary line always names `free_bytes` and `effective_cap_bytes`. Repository Cargo config uses the wrapper at `scripts/lib/cargo-rustc-wrapper.sh`, gated by `incremental = false`. Sccache shares a 10 GiB cache, machines without it run rustc directly.

## Enforcement

Three mechanisms share one read-only verdict helper, `hooks/helpers/check-impl-location.sh`. It emits `verdict=ok|canonical-protected` plus a nested-worktree advisory, and always exits 0.

- **SessionStart heads-up** (`hooks/session-start.sh`): on the canonical protected branch, it prints a non-blocking note.
- **Implementation-entry gate** (`/target`, `/execute`, `/fix`): `/execute` and `/fix` refuse before the first write on `canonical-protected`. The escape is `TARGET_LOCATION_OK=main-acknowledged`. `/target` alone resolves instead of refusing. It runs `fno do target start <node>`, the one-verb cold start, and continues from the worktree in its receipt. It never prompts, and it never needs the escape hatch. The verdict carries no attendance signal. It is also read before attendance resolves. So a prompt branching on attended-vs-unattended here has no machine input to branch on.
- **Config-driven relocation** (`hooks/worktree-setup.sh`): refuses outright on `policy = "never"`. Both creation hooks defer to `fno agents workspace worktree policy`, the resolver `worktree ensure` uses. So `worktrees_base` set relocates `claude --worktree` to `<worktrees_base>/<repo>/<name>` on every creation path, and the key alone is sufficient (no `policy = "external"` needed). With the knob unset, the placement stays harness-native. `scripts/setup/worktree-create-hook.sh` is the user-global wiring for non-footnote repos and resolves the same way.

Wire exactly one `WorktreeCreate` hook per repo.
The plugin hook and a user-global one merge across settings levels and race each other.
For non-footnote repos, wire `scripts/setup/worktree-create-hook.sh` into `~/.claude/settings.json` and leave the plugin hook out.
