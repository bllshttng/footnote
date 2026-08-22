# Codex Provider Guide

footnote ships one native Codex marketplace identity, `footnote`, and one plugin identity, `fno@footnote`.
The release and dev channels select the source behind that identity; they never install side-by-side plugin names.

## Plugin Channels

Use the release channel for normal Codex sessions.
It installs `fno@footnote` from the Git-backed `bllshttng/footnote` marketplace and keeps the version in `.codex-plugin/plugin.json` authoritative.

```bash
fno config setup codex-plugin --channel release
```

Use the dev channel while changing plugin content locally.
It installs `fno@footnote` from the durable canonical checkout rather than a disposable feature worktree.

```bash
fno config setup codex-plugin --channel dev
```

Codex caches plugins by version, so local edits at the same manifest version require an explicit refresh.
Refresh removes and re-adds the selected copy through Codex, which deterministically rebuilds its cache without changing release version files.

```bash
fno config setup codex-plugin --channel dev --refresh
```

Setup first validates the requested marketplace and plugin in an isolated temporary `CODEX_HOME`, leaving the working channel untouched when the candidate is invalid.
It then replaces the source behind `footnote`, verifies the installed version and complete loadable payload, and writes the channel marker only after that verification succeeds.
Every failed live switch restores the previous marketplace registration, plugin, and exact marker bytes; a rollback failure is persisted and reported distinctly by `fno doctor`.
Setup also migrates and removes legacy `footnote-dev` registrations, `fno@footnote-dev`, and their cache.
An already-correct selection is a no-op unless `--refresh` is present.
Every mutation can require hook approval and takes effect in a new Codex session.
Blocking `cli-ci` installs Codex 0.145 and runs the isolated real-binary containment and rollback regressions on every affected pull request.

`fno doctor` reports Codex plugin freshness separately from Python and Rust CLI freshness.
It compares the selected channel's loadable source payload with `CODEX_HOME/plugins/cache/<marketplace>/fno/<version>` and gives the exact refresh command for wrong-channel, missing-cache, version-mismatch, and payload-drift findings.

The verified payload preserves one loaded set of:

- skills from `skills/`
- agents from `agents/` and `.codex/agents/`
- commands from `commands/`
- plugin-bundled Codex lifecycle hooks through `hooks/codex-hooks.json`

Codex treats plugin hooks as untrusted until you approve them. Approve the footnote
hooks when prompted. `SessionStart` injects project vision, `fno whoami`, worktree
hygiene, and setup nudges through `hookSpecificOutput.additionalContext`; the other
packaged hooks provide the supported target-loop and safety lifecycle described below.

## Codex App Worktrees

Codex app worktrees are managed under `$CODEX_HOME/worktrees` and start from tracked files in the selected Git branch.
That is why `.codex/agents/*.toml`, `.codex-plugin/plugin.json`, `hooks/codex-hooks.json`, and `.agents/plugins/marketplace.json` are committed.

Start a target chat from the canonical repository project in Codex Desktop, then run `fno do target start <node>`.
Under the default `harness-native` policy, the command prints `native-handoff=required` and creates no worktree, branch, manifest, or claim.
Use `/worktree` or **Hand off -> Worktree** in that same chat and rerun the exact command from the Codex-managed worktree.
The retry verifies Desktop rollout identity, the `$CODEX_HOME/worktrees` boundary, Git registration against the canonical common directory, clean detached state, and the remote main base before creating `feature/<node>` and initializing the target.

This same-thread handoff is load-bearing for Remote Control project roll-up.
Codex Desktop records the managed chat against the canonical project while each chat retains a distinct worktree and branch, so concurrent Footnote chats remain isolated but appear under the same Footnote project in Desktop and mobile Remote.
Footnote reads the current thread's Desktop project assignment only as a strict ownership proof: the assigned cwd must equal the native worktree and the assigned local project's root must equal the canonical repository.
It fails closed on a missing or changed schema, never writes the private assignment state, and never asks its external allocator to create a directory under `$CODEX_HOME/worktrees`.

Codex TUI, headless, and other substrates that cannot perform the Desktop handoff degrade to an honest external worktree under `~/.fno/worktrees`.
That degraded native fallback deliberately ignores `config.paths.worktrees_base`; an explicitly configured `worktree.policy = "external"` still honors the configured allocator.

Archive the associated chat to snapshot and remove a Codex-managed worktree.
Footnote's archive and merged-worktree cleanup commands refuse to delete app-owned paths, even with `--force`.
For a stale project in Desktop, use the project sidebar `...` menu and **Remove**.
OpenAI's current documentation describes no equivalent mobile project-removal action, so use Desktop for that cleanup.

The native plugin blocks `Edit`/`Write` tool calls from a canonical protected checkout (`main`, `master`, or detached HEAD) before `apply_patch` lands.
The handoff receipt is the supported transition rather than an instruction to edit in the canonical checkout.

## Local-Development Fallback

For older Codex builds or CLI-only sessions where plugin-bundled hooks are unavailable,
wire the SessionStart hook into user config:

```bash
fno config setup cli-hooks-codex
```

The compatibility command remains available:

```bash
fno config setup cli-hooks --no-gemini --no-claude
```

`--no-claude` keeps this Codex-only: without it, `cli-hooks` also wires Claude's
WorktreeRemove hook into `~/.claude/settings.json`, which is unrelated to Codex.

Native plugin hooks are preferred when the Codex build supports them. The user-level
`$CODEX_HOME/config.toml` hook is a fallback for local development; Codex records its
approval separately under `[hooks.state]`. Check the effective fallback wiring and trust
state without modifying either hook layer:

```bash
fno doctor --codex-hooks
```

The presence of a `[hooks.state]` `trusted_hash` is reported as
`recorded-unverified`, not as proof that the current command is trusted. footnote does
not currently reproduce Codex's local hash-verification contract, so the diagnostic
stays advisory/warn and asks you to confirm approval in Codex itself.

Codex may report `loading hooks from both ... hooks.json and ... config.toml` when the
legacy `$CODEX_HOME/hooks.json` and preferred TOML layer both contain SessionStart hooks.
If the JSON entries are footnote-owned, migrate only those entries with:

```bash
fno config setup cli-hooks-codex --migrate-legacy-hooks-json
```

The migration preserves foreign JSON hooks. For example, a `herdr-agent-state.sh` hook is
not owned by footnote and remains in `hooks.json`; consolidate it into `config.toml`
manually if desired. Do not delete the legacy file until every foreign hook has been
accounted for.

For dev-only skill symlinks:

```bash
./scripts/setup.sh --provider codex
```

This populates `.agents/skills/plugin--fno--*` without replacing the native plugin
marketplace fixture.

## Custom Agents

Codex reads project custom agents recursively from `.codex/agents/*.toml`. Those files
are generated from canonical `agents/*.md` definitions:

```bash
python scripts/sync-codex-agents.py
python scripts/sync-codex-agents.py --check
```

Run the generator after changing `agents/*.md`. The check mode fails when generated
Codex agents are missing, stale, or no longer parse as TOML.

The generator preserves native Codex model names plus explicit `sandbox_mode` and
`nickname_candidates` fields. Claude-only model tiers (`haiku`, `sonnet`, `opus`, and
`inherit`) are omitted so Codex can use its configured model. Source tools determine a
predictable sandbox (`workspace-write` for write-capable tools, otherwise `read-only`),
while Claude-only `skills` and `disallowedTools` remain visible in the generated
developer instructions as behavioral context.

## Target Loop Hooks

Custom agents and target loop hooks are separate surfaces. The files under
`.codex/agents/` make footnote's specialist agents available to Codex; they do not
make `/fno:target` continue autonomously.

Target continuation is driven by hook events. `hooks/codex-hooks.json` wires the
Codex-supported subset needed for target loops: `Stop` for
`hooks/target-stop-hook.sh` (`fno-agents loop-check` + `finalize`), `PostToolUse`
for claim heartbeat/context monitoring, compact handoff hooks, subagent guards,
and the PreToolUse state/git protection guards.

Do not copy the full Claude hook manifest into Codex. Codex does not support every
Claude lifecycle event in `hooks/hooks.json`; `WorktreeCreate`, `CwdChanged`,
`FileChanged`, `SessionEnd`, and `StopFailure` are intentionally excluded here.

## Session Identity and Workflow Posture

In a Codex task, `CODEX_THREAD_ID` is the durable session identity. footnote prefers it
when creating target manifests, node claims, graph provenance, and follow-on dispatch
context. The shared SessionStart wrapper also registers that thread for addressable
`fno agents mail` delivery. If Codex does not provide it, the shared harness resolver falls
back to the other supported session markers and finally the existing generated
target-session id.

The core `target`, `do`, `think`, and `blueprint` workflows use the same canonical
markdown on Codex. `target` continues natively through the packaged `Stop` hook. Agent
work uses project custom agents and `spawn_agent` when the running Codex surface exposes
them; when a required primitive is unavailable, the workflow announces the limitation
and executes sequentially on the main task instead of implying parallel work occurred.

`/target bg` remains specifically a Claude `claude --bg` dispatch surface. A Codex build
dispatch receives a prose brief through an owned-PTY `pane` or a one-shot `headless`
spawn; it is never sent a Claude slash command and is never reported as `claude --bg`.

## Dependency Model

Core dependencies:

- `bash`
- `git`
- `gh`
- `jq`

Optional dependencies are reported by `./scripts/doctor.sh`.
