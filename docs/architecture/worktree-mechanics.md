# Worktree mechanics

The always-loaded half lives in [.claude/rules/worktrees.md](../../.claude/rules/worktrees.md), the authority on placement: where worktrees go, the policy values and their precedence, and the forbidden locations. This file is the authority on the machinery.

When you edit the hook, remove or prune a worktree, or trace why a location gate fired, read this first.

## The refusal shape

Both creation paths honor `policy = "never"`. The `WorktreeCreate` hook resolves the policy through `fno agents workspace worktree policy`, so there is one resolver and no second precedence implementation.

The refusal SHAPE is load-bearing and counter-intuitive. It differs by payload shape.

**path-present** (CC sends `.path`): the hook pre-creates the directory and is reaped. A non-zero exit falls back to CC's default flow, so exiting non-zero creates the very worktree you meant to block. The supported abort is **exit 0 with empty stdout**, which CC reads as "no successful output".

**name-only** (no `.path`, as EnterWorktree sends): the hook does not pre-create, and `test -d` finds nothing at fire time. A non-zero exit defers. That fallback does NOT hold here. The caller gets a hard failure and no worktree, which is why the rule file says to `git worktree add` first and enter by path.

The gate runs before the hook's own `cd`. An absent path fails at that `cd` first and takes the fallback branch, so gate placement is load-bearing too. The gate fails open on anything but an affirmative `never`, because a stale `fno` must not break interactive `claude --worktree`.

An in-session `claude --worktree` spawn is a child (`CLAUDE_CODE_CHILD_SESSION`) and never fires `WorktreeCreate`. Test with a top-level run.

When `worktrees_base` is set, the two paths still diverge on WHERE. Autonomous dispatch (`fno agents workspace worktree ensure`) stays harness-native unless `policy = "external"`. The hook relocates off `worktrees_base` directly.

## The unmanaged-repo over-reach

The plugin installs at the user level, so its hooks fire in every git repo on the machine, managed or not. A repo that declares nothing still resolves a policy: the built-in `harness-native`, degraded to `external` without a native harness. A repo whose HEAD sits on `main` or `master` then reads as `canonical-protected` to the location gate. A worker spawned there is blocked from editing and pushed into worktree ceremony the repo never asked for.

Measured 2026-09-13 in a fresh `git init` repo with no `.fno` and no config. The policy receipt printed bare `external` with no hint it had degraded. `check-impl-location.sh` from inside printed `verdict=canonical-protected`.

Three mechanisms close the gap. `FNO_WORKTREE_POLICY` is an env override above every config layer. It flows into the same fail-closed validation as a config value. A receipt reading `source=env` is the operator's proof of who set it.

The dispatcher pins `never` for a spawn into an undeclared FOREIGN repo. Repo identity is the git common dir, so a linked worktree dispatching into its own canonical checkout is not foreign. The dispatch receipt prints `worktree=never` and names the target repo undeclared.

`policy_cmd` prints `source=` and the degraded clause in `ensure`'s vocabulary. The location helper reads LINE 1 of that receipt. A whole-output exact match against the multi-line receipt can block every `never` repo.

The ceremony itself is now ceilinged. A dead worker on 2026-08-22 ran create ceremony forever inside a foreign repo. It created, relocated, exited, re-entered, then went silent. The `WorktreeCreate` hook counts create requests per session in a session-keyed latch. The latch lives at `latches/.worktree-create-<session-id>`, per [state-root-inventory](state-root-inventory.md). Past three attempts the hook aborts the supported way: exit 0 with empty stdout. Non-zero falls back to the harness's default flow and creates the worktree being refused. The refusal names the repo, the count, and the escape: `FNO_WORKTREE_POLICY=never`, or declaring the project.

A successful create clears the latch. Only repeated failing ceremony accumulates. Payloads with no session_id are never counted. Manual callers invoke the copy that way. The cap is per-session and is not a time bound. A ceiling for a hang nobody has re-observed is machinery ahead of evidence.

The live-run question is now answered. First attempt on 2026-09-13 was refused by the fleet footprint gate: `cpu_share_undecidable`, 31 unattributed bg-socket rows with the 60% ceiling inside the 19.4-75.8% attribution band. A later attempt the same day ran clean. One worker spawned into a fresh `git init` repo with no `.fno` and no config. Tasked with a one-line edit, it appended, committed, and exited. The positive control held: the file on disk actually changed. The run used the deployed path, before this node's changes ship. The guards main landed after 2026-08-23 are the likely reason the loop is gone.

Which exact call stopped returning on 2026-08-22 stays unpinned, and the loop shape is probably gone. This node adds defense in depth on the same path. The dispatcher pin catches the undeclared case before the child starts. The receipt makes the pin visible. The create-attempt cap bounds any recurrence the earlier guards miss.

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

Post-merge pruning is automated. Every gh-confirmed MERGED archive leg first mints a TTL reap order (`reap:pr-<n>`, 24h, the TTL ceiling). When the ritual runs from the canonical checkout and cannot resolve the merged worktree, the mint still stands. Minting clears the sweep stamp best-effort so the next idle tick can pay the order immediately. The daemon's six-hour worktree sweep checks each repository's claim scope. While one of its orders stands it runs that repository's pass with `--apply`. Otherwise it is report-only. A timer tick alone still removes nothing. An unreadable order probe skips that repository and emits its exit status plus first stderr line. It never collapses into report-only. The sweep's own guards (reapable, live claim, rooted processes) decide tree by tree, so an order never forces a protected tree. If the mint fails, the archive leg fails loudly. If direct archival cannot complete after a successful mint, the standing order preserves the owed work. `fno agents workspace worktree cleanup --merged` (dry-run by default, both removal modes) sweeps landed ones by hand with `--apply`.

Every removal emits one `worktree_removed` event row (path, caller, claim read, reason). The row mirrors to the machine-global journal. Before this emission landed no removal path recorded anything, so a lost tree left no attributable evidence.

### The DIRTY bucket under a done node (law d-cfcf5a8e)

The merge reaper removes a done-and-merged node's tree whatever its git status, keeps the branch, and holds only when a live process cwd is inside it. When a request names no tree, it resolves the branch through `git worktree list`. The canonical checkout is never selected. The probe is cwd-only, never `lsof +D`. The branch and transcript remain recovery paths, so removal is cheap and reversible. An OPEN node's tree keeps the old boundary, report only. Setup's own symlinks into canonical are the one discounted case (`reason=setup-links`). An unreadable cwd probe holds fail-closed. The request then stays pending and echoes the hold at most once an hour. A later pass takes the tree once the hold clears instead of tombstoning it forever. The request expiry counts from its mint.

### The done-node bucket

The 2026-09-18 crown's hand pass is the specification. It enumerated 42 trees and kept 41. The crown then pruned 13 by hand with zero commits and zero files lost. Behind `--done-node` the gate applies that pass's rule as a receipt (`reapable=yes reason=done-node`, row `would-archive (done-node)`). Removal takes the TREE and keeps its BRANCH. The node must read `done` or `superseded` in any store. No live or suspect `node:<id>` claim can hold it. Tracked files must be clean, and the tree must be 30 minutes old or more after its merge (law d-cf7d93bd). `blocked` still owns its tree. A tree with no node token gets the same grant from a branch `git` already merged (`evidence=merged`). A clean tree with a merged branch reads its base verdict, so the arm never shrinks an existing removal.

The gate is `crates/fno-agents/src/worktree_reapable.rs`, the one classifier since the done-node arm deleted the Python leg. The typer leaf and `scripts/lib/worktree-reapable.sh` exec the binary. The daemon probes call the module in-process. Node ids resolve from the manifest's `graph_node_id`, then from node-id tokens in the branch name, then in the directory basename. Status reads through the working graph plus the advisory archive. A node no store knows refuses (`reason=node-unknown`), fail closed.

Only the merged sweep sets the flag. `archive-worktree.sh --done-node` re-reads the gate with the flag at its strict check and again at removal time. Only a fresh `reason=done-node` passes either read. Before removal it salvages every non-discounted untracked path to `<canon>/.fno/salvage/<date>-<node>/untracked/`. A detached HEAD is pinned to `salvage/<tree-name>` first (`-<short sha>` appended on a name collision). A salvage or pin failure keeps the tree (exit 5). Unpushed commits stop being a keep reason because `git worktree remove` keeps the branch. Modified tracked content, live claims, rooted processes and sub-48-hour trees stay kept, exactly as before.

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

Cargo writes intermediates outside the checkout, and final binaries inside it. The tracked `.cargo/config.toml` sets `build.build-dir = "{cargo-cache-home}/build/{workspace-path-hash}"`, so a bare clone sends every intermediate to `~/.cargo/build/<h2>/<hash>` and leaves `crates/<crate>/target` holding only the final binaries. fno exports `CARGO_BUILD_BUILD_DIR=<base>/{workspace-path-hash}` to the doctor-test child env, the plugin-install env exports, the shell rc, and the target stop hook, which sets it for its loop-check child when the session snapshot lacks it. That moves the same layout under `<base>`: `paths.cargo_targets_base`, default `~/.fno/cargo-build`. The hash is per workspace root. Every checkout gets its own build dir. Every worktree gets its own. The two workspaces inside one tree hash separately. Parallel builds therefore never serialize on a shared directory. Never put `build.target-dir` in `.cargo/config.toml`: that key has no workspace hash. Every sibling worktree then shares ONE build directory, and the fleet's builds serialize again. Harness plugin installs copy from the filtered stage (`fno config plugin install`), so the caches never carry build output either.

The cleanup sweep (`fno agents workspace worktree cleanup --cargo-targets`) reclaims the in-checkout half. Its inventory walks `crates/*/target` per registered worktree. A candidate must carry Cargo's own `CACHEDIR.TAG` on the nearest ancestor, whatever the directory is named. The base is fno-owned and the tag is Cargo's marker, so a path failing either conjunct is counted as `link-not-owned` and never deleted. A symlink under a registered tree whose resolved directory sits under a managed base and carries the tag is deleted resolved-dir-first, then the link. These are the legacy caches the retired post-hoc relocation verb left behind. Select only `crates/*/target` by path, never by directory name. `cli/src/fno/target`, `skills/target` and `tests/target` are source dirs, and a name-based sweep deleted 66 of them across 26 worktrees on 2026-09-02.

Build-base hash dirs are the `cargo_build_dirs` lane of `fno doctor reclaim`, in `crates/fno-agents/src/cargo_build_dirs.rs`. `cleanup --cargo-targets` delegates its build-base rows to `fno-agents reclaim cargo-build-dirs` and prints its lines. The lane answers env-independently. It resolves every registered workspace's `crates/*/Cargo.toml` twice, once with `CARGO_BUILD_BUILD_DIR` set to the fno base and once with it removed. Both bases are inventoried whatever the caller's env carries. The lane manages the fno base always. The fallback base counts only under strict guards. It is where env-unset cargo runs land, and it must carry the `<2-hex shard>/<hex hash>` shape. It must also sit outside every registered tree and be none of `/`, `$HOME`, or the fno base. Four lanes apply, first match wins. Fresh: quiet under 6h, kept. Orphan: carries this repo's package fingerprints while no registered tree resolves to it, reaped. Age: owned and quiet 3 days or more, reaped. Cap: while total bytes exceed `min(24 GiB, 50% of free space)`, owned rows are reaped least recently used first, fresh rows included, until the total is back under the cap. When any metadata read fails, the orphan lane is disabled for the run and the summary names the manifest. Orphan and age deletes re-read quiet first. Cap deletes skip that and instead check two things right before each row's own delete: the flock, and a live-cargo guard. The live-cargo guard reads each running cargo process cwd with one lsof call. It excludes hash dirs resolved by that tree. Cargo test drops cargo-lock between binaries, so flock alone misses that gap. The cwd guard protects the dir during that gap. A row stays out of cap pressure for CAP_MIN_QUIET_SECS, 15 minutes. This backstop covers an absent lsof or an odd process name. If the live-cargo read cannot be trusted, the cap lane uses the full FRESH_SECS floor. When lsof fails or a live cwd's tree cannot answer its manifests, use the full FRESH_SECS floor. An empty live set is not proof that nothing is live. Every delete takes `flock(LOCK_EX | LOCK_NB)` on each profile's `.cargo-lock`. A held lock reads `build-in-progress` and the row stays, so a build mid-flight is never deleted. The summary ends with `before_bytes`, `cap_exceeded` and `cap_held`. They give the bytes read at the start, whether the cap was exceeded, and the reason each standing row was kept. The tree-removal reclaim (`reclaim remove-for`) resolves a tree's manifests both ways before the checkout goes. The merge reaper calls it in-process. It deletes each resolved dir that is tagged and under a managed base, behind the same flock guard. `FNO_CARGO_FREE_BYTES` overrides the free-space read and `FNO_CARGO_TARGETS_BASE` the build base, for tests. Setup still runs the sweep with `--apply` after linking (inspect first by omitting `--apply`). Repository Cargo config uses the wrapper at `scripts/lib/cargo-rustc-wrapper.sh`, gated by `incremental = false`. The wrapper drops `CARGO_BUILD_BUILD_DIR`, `CARGO_BUILD_TARGET_DIR` and `CARGO_TARGET_DIR` before sccache runs. sccache hashes every `CARGO_*` var into its cache key, and `fno doctor test` sets a new build dir each run. Registry deps then share one cache across worktrees. Workspace crates, proc-macros, build scripts and test binaries never share. Their compiled output bakes their own paths. On macOS sccache ignores `XDG_CACHE_HOME`, so `neutralise` pins `SCCACHE_DIR` and a test child cannot start a server on a sandbox cache. Machines without sccache run rustc directly.

### Idle-tree reclaim

The retire tick reclaims quiet build output from linked trees.
It uses Cargo metadata, Cargo cache tags, and tracked-file checks.
The tree must have no live registry or Claude roster session.
The existing cargo-build-dirs dry run prints the same idle-tree lines.

Every row line names `owner=`, `node=` and `session=`. `owner=` names the registered tree whose manifests resolve to the dir, or `none`. `node=` and `session=` come from registry rows whose cwd falls under that tree. Only live sessions appear. When the registry cannot be read, the line reads `unread`. `fno doctor reclaim cargo-build-dirs` then answers which session holds the disk.

The wrapper sets the shared sccache size to 30G unless `SCCACHE_CACHE_SIZE` is set. At 10G the cache sat at its limit on a fleet machine. It kept only 6.5 hours of history, so a branch parked overnight rebuilt cold. A running sccache server keeps its old size until it exits. sccache refuses proc-macro, build-script, bin and test crate types by design. About half the calls never reach the cache, and no setting changes that. The build dir stays per worktree. A shared dir adds reuse of registry proc-macros and build scripts, and nothing more. It also breaks the reclaim at worktree removal, and one `cargo clean` then wipes every worktree. The repo sets no `jobs` key, so a solo clone builds at full width. A per-user `jobs` cap slows every build and still does not stop two cargo runs at once, because cargo shares no jobserver across separate runs.

## Worktree builds and live stores

A binary built inside a linked worktree refuses to open an SQLite store under the passwd home's `.fno`. Every writable open in both crates checks the running executable's nearest `.git` ancestor before it connects. In a linked worktree that ancestor is a `.git` file, and the open is refused with a message that names the store and the worktree. The operator's stores keep their schema and their rows. The canonical checkout carries a `.git` directory. A deployed install carries no `.git` ancestor. Both stay allowed. A store outside the home `.fno`, or under the worktree itself, is never refused. Read-only opens stay unfenced, because a read cannot migrate a store. There is no bypass flag. The remedy is the deployed binary (`fno doctor update`), or a store that belongs to the checkout. Python store writers carry no such check.

## Enforcement

Three mechanisms share one read-only verdict helper, `hooks/helpers/check-impl-location.sh`. It emits `verdict=ok|canonical-protected` plus a nested-worktree advisory, and always exits 0.

- **SessionStart heads-up** (`hooks/session-start.sh`): on the canonical protected branch, it prints a non-blocking note.
- **Implementation-entry gate** (`/target`, `/execute`, `/fix`): `/execute` and `/fix` refuse before the first write on `canonical-protected`. The escape is `TARGET_LOCATION_OK=main-acknowledged`. `/target` alone resolves instead of refusing. It runs `fno do target start <node>`, the one-verb cold start, and continues from the worktree in its receipt. It never prompts, and it never needs the escape hatch. The verdict carries no attendance signal. It is also read before attendance resolves. So a prompt branching on attended-vs-unattended here has no machine input to branch on.
- **Config-driven relocation** (`hooks/worktree-setup.sh`): refuses outright on `policy = "never"`. Both creation hooks defer to `fno agents workspace worktree policy`, the resolver `worktree ensure` uses. So `worktrees_base` set relocates `claude --worktree` to `<worktrees_base>/<repo>/<name>` on every creation path, and the key alone is sufficient (no `policy = "external"` needed). With the knob unset, the placement stays harness-native. `scripts/setup/worktree-create-hook.sh` is the user-global wiring for non-footnote repos and resolves the same way.

Wire exactly one `WorktreeCreate` hook per repo. The plugin hook and a user-global one merge across settings levels and race each other. For non-footnote repos, wire `scripts/setup/worktree-create-hook.sh` into `~/.claude/settings.json` and leave the plugin hook out.
