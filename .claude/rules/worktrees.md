# Worktree convention

The single place that says where git worktrees go and what to do after creating one. Loaded every session via `AGENTS.md`. Skill defaults that place worktrees elsewhere lose to this rule.

Hook internals, removal, and the enforcement wiring live in [docs/architecture/worktree-mechanics.md](../../docs/architecture/worktree-mechanics.md). Read it before editing the `WorktreeCreate` hook, where exiting non-zero on the wrong payload shape creates the very worktree you meant to block, and when removing or pruning a worktree or tracing why a location gate fired.

## The rule

**The worktree root is config-driven via `config.paths.worktrees_base`. Set nothing and the defaults work.**

- **Unset (OSS-neutral default):** harness-native `<repo>/.claude/worktrees/<name>` (gitignored, search-clean). No config needed.
- **`config.paths.worktrees_base: <dir>`:** worktrees land at `<dir>/<repo>/<name>` (`<repo>` = `basename $(git rev-parse --show-toplevel)`).
- **`worktree.use_conductor_canonical: true` is DEPRECATED:** acts as `worktrees_base = ~/conductor/workspaces`; prefer `worktrees_base`.

## Creating and entering one

Add first, enter by path, then run the setup script from inside:

```bash
git worktree add <location>/<name> -b <name> origin/main
# then EnterWorktree with that path, then from inside it:
bash scripts/setup/setup-worktree.sh
```

**`EnterWorktree` by NAME fails here** (verified 2026-08-09): the hook defers, and the caller gets a hard failure with no worktree, NOT the documented fallback. So the add always comes first and the entry is always by path. Any path in `git worktree list` is enterable on first entry, and a `/target` cold-start reads that path from the `fno target start` receipt. A shell `cd` will not do instead; it does not persist across tool calls.

The setup script links shared state from canonical: the vault symlink, per-file `.fno/` state, the gitignored `.claude/` subdirs, and the harness config roots. It warns and skips any real (non-symlink) file at a target, leaving real state intact. Tracked files come from git checkout.

## Cargo build storage

Cargo targets remain worktree-local so sibling builds never share Cargo's artifact-directory lock. After linking succeeds, setup runs `fno worktree cleanup --cargo-targets --apply` (inspect first by omitting `--apply`). It reaps inactive targets older than seven days first, then the oldest inactive targets until allocated target bytes are at or below 64 GiB. A live target claim or rooted process protects its worktree; protected bytes that prevent the cap return `over-cap-protected` instead of deleting an active build. Repository Cargo config uses the optional compiler wrapper at `scripts/lib/cargo-rustc-wrapper.sh` plus `.cargo/config.toml`'s `[build] incremental = false`; that second setting is required, because sccache cannot cache incremental compilation and Cargo's dev profile defaults `incremental = true`. With both in place, installed sccache shares compilation with a default 10 GiB `SCCACHE_CACHE_SIZE` across every worktree; machines without sccache run rustc directly.

## Per-project worktree policy

Every code-payload dispatch routes through `fno worktree ensure`, which resolves a `worktree` policy.
Precedence: per-project `work.workspaces.<slug>.projects[].worktree` > global `config.worktree.policy` > built-in `harness-native`.

- **`never`** - launch in place on the canonical checkout (for projects whose working tree IS the product, e.g. an Obsidian vault). ensure prints the repo root, exit 0; callers skip `setup-worktree.sh`; the location gate treats the protected branch as `ok`.
- **`harness-native`** (default) - the harness's own location: claude lands at `<repo>/.claude/worktrees/<name>`, **always**, ignoring `worktrees_base`; Codex Desktop uses `/worktree` or **Hand off -> Worktree**. A harness with no native transition degrades to `~/.fno/worktrees` and does not inherit `worktrees_base`; ensure needs `--harness` and never guesses.
- **`external`** - fno-managed at `<worktrees_base>/<repo>/<name>`.

The per-project policy outranks `worktrees_base`: setting the base alone leaves a claude default where it is, and relocating it also needs `worktree.policy = "external"`. "conductor" is a `worktrees_base` value, not a policy value. A config parse error or out-of-enum value REFUSES creation (fail closed): ensure exits non-zero with empty stdout, so the caller holds rather than auto-isolating on a misconfig.

Both creation paths honor `never`, the hook resolving it through `fno worktree policy` (one resolver, no second precedence impl).

## Forbidden locations (regardless of config)

- `~/.warp/worktrees/...` (setup script never runs there).
- `<repo>/worktrees/` or any non-`.claude` path inside the checkout.
- `../<name>` or any sibling-of-canonical path.
- Anything beneath `$CODEX_HOME/worktrees`; Footnote never allocates there.

Exception: `/speculate` keeps its own `.claude/worktrees/<name>` placement even when `worktrees_base` is set (do not generalize).

## Override semantics

An explicit in-conversation user request for a different path outranks this rule; note that `.fno/` state links will not exist there. Do not solicit overrides.
