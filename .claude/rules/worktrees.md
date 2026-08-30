# Worktree convention

The single place that says where git worktrees go and what to do after creating one. Loaded every session via `AGENTS.md`. Skill defaults that place worktrees elsewhere lose to this rule.

Read [worktree-mechanics](../../docs/architecture/worktree-mechanics.md) for hook internals, removal, cargo storage, and the Bash isolation map. Before editing the `WorktreeCreate` hook: a non-zero exit on the wrong payload shape CREATES the worktree you meant to block. In a worktree, Bash refuses `$` expansion, `$(...)`, and loops. Use `printenv`, fno verbs, or `bash <file>`.

## The rule

**The worktree root is config-driven via `config.paths.worktrees_base`. Set nothing and the defaults work.**

- **Unset (OSS-neutral default):** harness-native `<repo>/.claude/worktrees/<name>` (gitignored, search-clean). No config needed.
- **`config.paths.worktrees_base: <dir>`:** worktrees land at `<dir>/<repo>/<name>` (`<repo>` = `basename $(git rev-parse --show-toplevel)`).
- **`worktree.use_conductor_canonical: true` is DEPRECATED:** acts as `worktrees_base = ~/conductor/workspaces`. Prefer `worktrees_base`.
- Cargo targets stay worktree-local (sibling builds never share the artifact lock). Details in [worktree-mechanics](../../docs/architecture/worktree-mechanics.md).

## Creating and entering one

Add first, enter by path, then run the setup script from inside:

```bash
git worktree add <location>/<name> -b <name> origin/main
# then EnterWorktree with that path, then from inside it:
bash scripts/setup/setup-worktree.sh
```

**`EnterWorktree` by NAME fails here**: name-only defers and the caller hard-fails with no worktree. So add first, enter by path. Any path in `git worktree list` is enterable, and a `/target` cold-start reads it from the `fno target start` receipt. A shell `cd` will not do. It does not persist across tool calls.

Setup links shared state from canonical: vault symlink, per-file `.fno/` state, gitignored `.claude/` subdirs, harness config roots. It warns and skips real files. Tracked files come from git checkout.

## Removal

The removal contract, missing until 174 trees piled up (74 GB). Three buckets, one trigger, one gate:

- **DIRTY** - never touched by any automatic path. Report only.
- **clean + unmerged** - never auto-pruned. Report the branch so a human judges (open PR or abandoned work).
- **clean + merged** - prune the TREE, keep the BRANCH. The tree is a checkout. The branch is the work.
- **Trigger: MERGE, never node-done.** A done node can sit on an unmerged branch whose only checkout is that tree. The post-merge ritual is the home.
- **Gate: `reapable`** (`fno agents workspace worktree reapable`). The tool enforces the buckets, not each caller.
- **Backstop: the daemon's daily `cleanup --merged` sweep** - the ritual only sees its own PRs.

Verb: `fno agents workspace worktree cleanup --merged` (dry-run default, `--apply` executes, from canonical). Orders, guards, and events in [worktree-mechanics](../../docs/architecture/worktree-mechanics.md).

## Per-project worktree policy

Every code-payload dispatch routes through `fno agents workspace worktree ensure`, which resolves a `worktree` policy.
Precedence: per-project `work.workspaces.<slug>.projects[].worktree` > global `config.worktree.policy` > built-in `harness-native`.

- **`never`** - launch in place on the canonical checkout (for projects whose tree IS the product, e.g. an Obsidian vault). ensure prints the repo root, exit 0; callers skip `setup-worktree.sh`; the location gate treats the protected branch as `ok`.
- **`harness-native`** (default) - the harness's own location: claude lands at `<repo>/.claude/worktrees/<name>`, **always**, ignoring `worktrees_base`. Codex Desktop uses `/worktree` or **Hand off -> Worktree**. A harness with no native transition degrades to `~/.fno/worktrees` (no `worktrees_base` inheritance); ensure needs `--harness` and never guesses.
- **`external`** - fno-managed at `<worktrees_base>/<repo>/<name>`.

The per-project policy outranks `worktrees_base` (relocating the claude default also needs `policy = "external"`; "conductor" is a base value, not a policy value). A parse error or out-of-enum value REFUSES creation (fail closed): ensure exits non-zero with empty stdout, so the caller holds.

Both creation paths honor `never`, the hook resolving it through `fno agents workspace worktree policy` (one resolver, no second precedence impl).

## Forbidden locations (regardless of config)

- `~/.warp/worktrees/...` (setup script never runs there).
- `<repo>/worktrees/` or any non-`.claude` path inside the checkout.
- `../<name>` or any sibling-of-canonical path.
- Anything beneath `$CODEX_HOME/worktrees`; Footnote never allocates there.

Exception: `/speculate` keeps its own `.claude/worktrees/<name>` placement even when `worktrees_base` is set (do not generalize).

## Override semantics

An explicit in-conversation user request for a different path outranks this rule; note that `.fno/` state links will not exist there. Do not solicit overrides.
