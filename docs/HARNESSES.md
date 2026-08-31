<!-- style-exception: mechanical verb rename preserves pre-existing prose -->
# Harness Capabilities Reference

footnote runs as a host runtime on several AI coding CLIs. This is the public summary of what works where; the exhaustive substrate facts (per-event hook mappings, frontmatter matrices, directory conventions, variable substitution) are maintained internally.

## CLIs in scope

| CLI | Role for footnote | Parallel subagents |
|---|---|---|
| Claude Code | Native target. All hooks, all features. | Yes |
| Codex CLI | Native plugin: core workflows, supported lifecycle hooks, `AGENTS.md`, and project custom agents. | Yes where `spawn_agent` is available; explicit sequential fallback otherwise |
| Gemini CLI | Multi-CLI hook integration. | Sequential |
| Hermes | Loop-wrapper path. | Sequential |
| Openclaw | Loop-wrapper path. | Sequential |
| OpenCode | Native stop-hook plugin (world-gated, in-session re-drive) + loop-wrapper fallback. Reads `AGENTS.md` natively. | Sequential |
| Antigravity CLI (`agy`) | Native `Stop`-hook adapter (world-gated, `decision:"continue"` re-drive). Claude-shaped hook events, Gemini-family wire format. | Sequential |
| pi (`@earendil-works/pi-coding-agent`) | Pane-hosted TUI today, with the `pi --mode rpc` driving transport built and tested but not yet wired to a spawn arm. | Sequential |
| Cursor Agent (`cursor-agent`) | Pane-hosted TUI. `--print --output-format stream-json` is output-only, so the pane is the shipping driving lane. | Sequential |

Other CLIs (Cursor, GitHub Copilot Agents, Kiro, Qoder, Rovo Dev, Trae) are out of scope for footnote orchestration. For a new harness that enters scope, run `fno doctor harness <name> --live` and record its positive markers before adding a capability row; the runnable rubric is the evidence gate, not this summary.

## What this means in practice

- **Skills are portable markdown** and work on every CLI in scope.
- **The autonomous target loop** runs natively on Claude Code, Codex, and Gemini: a stop-equivalent hook blocks session exit until a `<promise>` tag appears. Hermes and Openclaw use a loop wrapper (`scripts/run-target-loop.sh --driver <name>`), which polls for the same tag. OpenCode is first-class: `fno config setup` installs a local-file plugin (`~/.config/opencode/plugins/footnote.js`, no npm needed) that hooks `session.idle`, synthesizes a transcript, and shells `fno-agents loop-check` for the SAME world-gated completion check claude uses (promise scan + PR-for-HEAD + CI green + bots reviewed + no blocking finding). On a non-terminal decision it re-drives the same session in-context via `client.session.prompt`; on a terminal decision loop-check emits the `termination` event itself. loop-check is the sole completion authority, so OpenCode and Claude Code share one gate with no drift. **Antigravity CLI (`agy`)** is native the same way through a different surface: `fno config setup` registers a `Stop`-hook adapter (`hooks/agy-target-stop-hook.sh`) in agy's `hooks.json` (`~/.gemini/config/hooks.json`). agy's hooks use Claude-shaped event names but a Gemini-family wire format (camelCase stdin, `decision:"continue"` to keep working, JSON-only stdout), so the adapter synthesizes a claude-shaped transcript from agy's `transcript.jsonl` and shells the SAME `fno-agents loop-check` gate. `fullyIdle == false` keeps the session working until background tasks finish; a missing binary allows the stop (never an unstoppable loop) while a transient gate failure continues and retries.
- **Codex identity and lifecycle are native:** `CODEX_THREAD_ID` owns target manifests and node claims, while the Codex `Stop` and `PostToolUse` hooks drive target continuation and claim heartbeat/context monitoring. Only the supported Codex event subset is packaged.
- **Parallel subagent dispatch** (`/review sigma`, `/speculate`, parallel `/execute` waves) uses Claude's Agent tool or Codex project custom agents through `spawn_agent`. A Codex surface without that primitive reports the downgrade and runs sequentially; other in-scope CLIs keep their documented sequential or provider-specific path.
- **Thread dispatch is substrate-specific, and the worker command is harness-native:** `/target thread` runs Claude (`claude --bg`); every other harness dispatches one-shot `headless`. OpenCode's serve lane is launch-only (steering unbuilt), so its capability bit reads false and it defaults to `headless` until the steering lane ships its own unattended journey test; an explicit interactive `--substrate thread` spawn still reaches the lane. `bg` remains a deprecated input alias for `thread` for one release. The command a worker receives comes from the harness-capability map's per-harness `dispatch_command` (`fno agents dispatch resolve`), grounded in what each CLI actually supports, NOT a blanket "prose brief only" rule. Verified: **Claude** invokes `/target` (native slash command); **Codex** invokes `$fno:target` (its plugin exposes the footnote skill and `codex exec` expands `$plugin:skill`); **Antigravity (`agy`)**, gemini's successor, recognizes skills as active slash commands, so it invokes `/target`. **OpenCode** invokes `/fno:target` through its footnote plugin (`command_surface = "slash"`, `slash_prefix = "fno:"`; `opencode run --command fno:verb` expands it, and the serve lane's writer rides the same flag). Never pass a bare Claude `/target` string to a harness that cannot expand it. The build/route lane is Claude-only, so a non-Claude spawn drops `--role`/`--route`.
- **A hook that fires outside a session cannot live in the plugin manifest.** Claude sources plugin hooks from the running session's hook table, so a bare CLI subcommand with no agent session (`claude rm`) sees none of `hooks/hooks.json` - the command string is never even read, which is why an unset `${CLAUDE_PLUGIN_ROOT}` is never the explanation for such a failure. `WorktreeRemove` is the one event footnote needs there: Claude marks a hook-created worktree `hookBased` and will only remove it by running that hook, so without a settings-level copy every such worktree is stranded ("WorktreeRemove hook failed") forever. `fno config setup cli-hooks` merges it into `~/.claude/settings.json`, and because that path is persisted outside the session it is canonicalized first - writing a worktree path there would strand every worktree on the machine the moment that worktree is archived. The plugin entry stays for in-session removal, so in a session both fire, in parallel and undeduplicated (the dedup key includes the plugin root, which only one of them carries). That is safe because the destructive fallback is gated on the worktree's `.git` still being present and git serializes `worktree remove` itself, not because the script is merely idempotent; the loser of the race can still report a failure it did not cause. Related contract, easy to get wrong: the harness never stats the path afterward, so the hook's **exit code is the whole signal** - exit 0 means "removed" and the job record is deleted, which is why preserving or refusing a worktree must exit non-zero.
- **Context file:** footnote makes `AGENTS.md` canonical; `CLAUDE.md` and `GEMINI.md` are one-line stubs that import it, so every CLI inlines identical content.

### Human-approved law

`/fno:law` is portable on every harness, because it is one step and needs no harness-specific approval event. The operator types the ruling and `fno inbox law set` records it, returning a `d-` id in the same turn. A recording made from a chat carries `authority_source: chat_attested` and never claims the `operator` lane, so a reader can tell the two apart on any harness.

No harness exposes a human-origin discriminator, and ruling d-e1eec854 measured what that costs. A real Claude `UserPromptSubmit` capture on 2026-08-24 exposed `hook_event_name`, `session_id`, `transcript_path`, `cwd`, `prompt_id`, `permission_mode`, and `prompt`, and none of them marks a human. The transcript is no better: across every transcript in one machine's claude project directory, 2173 user turns carrying an `<fno_mail>` envelope were recorded with `promptSource: "typed"` and 2439 with `origin: {"kind": "human"}`, and `fno agents mail send --raw` strips the envelope that is the only remaining marker. So invocation is the authority and a mail-injected invocation cannot be refused. The staged-proposal ceremony that used to stand here refused a headless session and was waved through by an attended chat, which is the shape mail arrives in, so it was retired. See `skills/law/LIMITATIONS.md` for the trade.

## pi: two lanes, one session, and one unsafe half

pi is the first harness whose driving lane is neither a shellout nor a keystroke PTY, and the first whose worst failure mode is a SUCCESS. Read this before wiring anything to it.

**The two lanes, and which one ships.** `pi --mode rpc` speaks strict JSONL over stdin and stdout: commands in, typed events out, LF the only record delimiter, an `id` correlating request to response. That is the DRIVING lane, and today it is a driver library with tests and no spawn arm: `thread` reads false on pi's capability row and `fno agents spawn -H pi` gives you the pane. A plain interactive `pi` in a mux pane is the WATCHING lane, showing pi's real TUI. They are mutually exclusive per PROCESS, chosen at exec, and never per session: measured 2026-08-28, a TUI opened on a session an rpc driver was already holding JOINED it, rendered that session's own turns, and left the session-file count at one. So the pane is a view onto a live rpc session rather than a rival launch, and `fno agents attach` on a pi row execs that TUI directly.

**A pi session is the pair `(cwd, session_id)`.** Sessions live at `~/.pi/agent/sessions/<encoded-cwd>/<ISO-timestamp>_<session-id>.jsonl`, where the encoding replaces path separators with `-` and fences the result with `--` at both ends. The id alone addresses nothing. For a worktree-first fleet this bites immediately: a resume issued from the canonical checkout cannot see a session started in a worktree, and on pi that miss is not an error, it CREATES a second session under the same id.

**fno mints the id and pi adopts it.** `--session-id <id>` is documented as "Use exact project session ID, creating it if missing" and behaves that way across separate processes. fno must always pass one. pi's own default is a UUIDv7, whose head-8 is the same clock bucket that collides two codex short ids.

**Appends are safe. Creates are not, and creates fail SILENTLY.** Joining an existing session concurrently was measured four times: the second process exits 0 in about five seconds and its user turn hangs off the holder's last assistant message by `parentId`, one file and one linear chain. Four SIMULTANEOUS creates on one id produced four session files 49ms apart. Every process exited 0. Every process printed "creating a new session with that id". Every file was internally perfect. Then a later resume of that id picked the OLDEST, wrote no fifth file, and named none of the other three. Every component succeeded and the answer was wrong.

So fno serialises the CREATE DECISION and nothing else, using the claim primitive that already exists on a `pi-session:<cwd>:<id>` key. The winner creates, the losers join, and the claim is released as soon as the session exists. The claim is the right instrument rather than a file count because a pi session's file appears at the FIRST TURN ATTEMPT: a live rpc session with no prompt sent leaves the directory empty, so counting files measures nothing in exactly the window the race lives in. A claim is written at acquire.

The reader half is separate and outlives the fix: when one id resolves to more than one session, `fno agents attach` REFUSES and names every session with its timestamp, selecting none. Duplicates can pre-exist from a crash or a hand-run pi, and no claim taken today serialises one already on disk. Never rank duplicates by content: an empty assistant `content` array marks a turn that was attempted and FAILED, so the emptier file is often the one worth reading.

**Two traps that cost a turn each.**

1. rpc mode EXITS ON STDIN EOF, mid-turn, with status 0. A prompt fed from a file yielded five events and stopped at the user's own `message_end`; the assistant never spoke and the exit code still read success. Hold stdin open for the session's life and settle on the typed `agent_settled` event. A clean exit proves nothing.
2. `--provider openai-codex` WITHOUT `--model` does not resolve to gpt-5.5. It falls through to a Bedrock model and fails with "Token is expired. To refresh this SSO session run 'aws sso login'", naming AWS and misdirecting completely. Pass both, always.

## Cursor Agent: pane lane and remote chat identity

Cursor Agent is a pane-hosted TUI. Its `--print --output-format stream-json` mode emits output but has no input-format, RPC, ACP, serve, or stdio drive lane, so fno ships the pane keystroke path. The pane readiness markers are the captured `Working` or `Running` state and the idle `→ Add a follow-up` marker.

`cursor-agent create-chat` mints a full UUID and stays alive after printing it. fno reads the first line, terminates the helper, records that UUID before launching, and always passes it explicitly to `--resume`. A bare `--resume` opens Cursor's interactive picker. The chat transcript is remote: identity is proven when a second process on the same chat recalls the first process's nonce, never by looking for a local session file.

The credential is the operator's login. `~/.cursor/` is a mixed GUI-and-CLI state root: `cli-config.json` carries CLI settings while `hooks.json` is unattributed between the CLI and editor. fno never passes `-w`, `--worktree`, or `--worktree-base`; it owns the worktree outside Cursor's native `~/.cursor/worktrees/` location.

**Mail will ride `steer`, and today it does not.** The pane lane is what ships, so an envelope reaches a pi worker as keystrokes at a measured 0 ms enter delay. The driver implements `steer` and the tests cover it; nothing dispatches it yet. When the rpc spawn arm lands, this is the path it takes, and the reason is worth keeping: pi delivers a steering message after the current assistant turn finishes its tool calls and before the next LLM call, which is exactly the semantics fno's mail injection wants and what typing into a pane only approximates. Read the response literally: `success: true` means accepted, queued, or handled. Failures after acceptance arrive through the event stream, never as a second response for the same id.

**Permissions and credentials are operator territory.** pi ships no permission popups at all, so its three `permission_response` actions are unsupported and its readiness manifest declares no blocked rule. `pi auth` is read-only, three verbs, none of which writes; only the TUI's `/login <provider>` writes a credential. Never synthesize one. The pane lane is the only lane where that command can be run, which is its second job beyond watching.

**tmux note.** pi prints its own warning when `extended-keys` is off: modified Enter keys collapse to plain Enter. Plain Enter is `\r` and submits, which is all fno's submit path needs, so this only matters for a human wanting a newline without submitting. `set -g extended-keys on` in `~/.tmux.conf` fixes it.

## Antigravity plugin packaging

The `scripts/install/agy-plugin.sh` bundle ships exactly `plugin.json`, `skills/`, and `agents/`, and nothing else; the script stages that allowlist into a temp directory and installs the staging copy, because handing `agy plugin install` the repo root deep-copies it with symlinks dereferenced (a 9.1 GB corrupt half-copy when measured against agy 1.1.16 on 2026-08-20).

Hooks are not bundled. `fno config setup` already registers the Stop and PreInvocation adapters in `~/.gemini/config/hooks.json` under a `footnote` key, so a `hooks.json` inside the bundle would register the same adapters a second time and double-fire the stop gate.

agy 1.1.16 stages installed plugins at `~/.gemini/config/plugins/<name>/`, not the `~/.gemini/antigravity-cli/plugins/` path its CLI reference documents; the install script checks both, config first, and verifies success by grepping `agy plugin list` for the plugin name rather than by checking that a file landed.

The root `plugin.json` validates against a published schema with `additionalProperties: false` over `name` and `description`, so the manifest can carry no version and no permissions block; permissions live only in `~/.gemini/antigravity-cli/settings.json`, and `scripts/release/sync-version.sh` accordingly leaves the root manifest out of `JSON_MANIFESTS` while tracking every other release manifest.

## Official CLI documentation

| CLI | Docs |
|---|---|
| Claude Code | https://code.claude.com/docs |
| Codex CLI | https://developers.openai.com/codex |
| Gemini CLI | https://geminicli.com/docs |
| OpenCode | https://opencode.ai/docs |
| Antigravity CLI (`agy`) | https://antigravity.google/docs/cli/reference |
| pi | ships its own docs with the package (`docs/rpc.md`, `docs/sessions.md`, `docs/tmux.md`) |

For per-skill cross-CLI consequences see [docs/SKILL-COMPAT-MATRIX.md](SKILL-COMPAT-MATRIX.md); for how footnote wires into each CLI's hook surface see [docs/architecture/multi-cli-hooks.md](architecture/multi-cli-hooks.md).
