<!-- style-exception: verbatim moves of provider, posture and observability prose from the style-exception'd agent/SKILL.md root; the plan requires authority statements to travel intact when compacting -->

# Workflow routes (conditional recipes)

Load this file only when the spawn is NOT a default claude pane build/seed: a named non-default provider or substrate, a model/effort/permission posture, or when observing a worker's state. The SKILL.md root carries the flow, the confirm policy, the hard rules, and the receipt contract; this file carries the per-provider and per-posture detail those triggers need.

## The provider and substrate matrix

| Provider | Dispatch | Worker | Receipt |
|---|---|---|---|
| `claude` | `fno agents spawn` (default `pane` owned-PTY; `thread` -> `claude --bg`; `headless` -> `claude -p`) | owned pane, or persistent thread (`thread`) | compact JSON `.short_id` (reply on `headless`) |
| `codex` | `fno agents spawn` (exec) / `fno agents host` (`-i`); `headless` -> `codex --exec` | daemon-managed PTY worker | pretty JSON `.short_id` |
| `agy` | `fno agents spawn` (exec) / `fno agents host` (`-i`); `headless` -> `agy -p` | daemon-managed PTY worker | pretty JSON `.short_id` |

The `gemini` CLI is deprecated (Google retired it); `agy` is its first-class successor and the row that used to say "gemini" names `agy` now. A `gemini` provider token is refused at normalize.

All three create via `spawn`. The substrate axis (x-61df) selects the host: `pane` (default, owned-PTY drivable), `thread` (a persistent continuation lane), `headless` (one-shot `claude -p` / `codex --exec` / `agy -p`). The per-harness support matrix, including refusals, lives in `docs/architecture/thread-lanes.md`; this skill does not restate provider verdicts. A one-shot Q&A is the `headless` substrate (x-cbb0: it subsumes the retired one-shot ask; today's `ask` verb is the sync lane to an existing worker). A codex/agy exec worker is a **single autonomous pass**, not the claude "refuse to stop until shipped" loop - do not imply loop-grade completion guarantees for them.

Every `spawn` captures (best-effort, non-blocking) the worker's full resume UUID into the registry, distinct from the short display id, so a worker is a complete, identified citizen of the mesh that can later be addressed or escalated. Address peers by the full session id; the head-8 prefix is a display handle that collides under UUIDv7 minute-siblings (see the mail skill).

## Posture deep-dives

- **`drive`** (alias `interactive`): routes codex/agy to a drivable `host` session instead of an autonomous `spawn`. No-op for claude. "drive it" / "interactive" map here.
- **`yolo`** (aliases `auto`, `-Y`, `--yolo`): typing `yolo` drops the sandbox for this launch - codex runs `--dangerously-bypass-approvals-and-sandbox`, agy runs bare `--yolo` (unsandboxed full-auto). You rarely need it: with NO flag, a headless codex/agy worker is already BOUNDED - sandboxed AND never-prompt - so it neither hangs nor roams outside the workspace. Reach for `yolo` only when you genuinely want no sandbox. For claude (which has no `--yolo` flag) it maps to `--permission-mode bypassPermissions`, the equivalent full-auto/no-gates posture, so a yolo'd claude worker runs gate-free instead of stalling on a permission prompt; an explicit `--permission-mode` you pass wins over this default. "full auto" / "no sandbox" / "unsandboxed" map here. (To make full yolo the standing default for a provider instead of per-launch, set `config.agents.<provider>.headless_yolo: true`.)
- **`model <name>`**: exact model for the worker, plumbed to `fno agents spawn --model`. Two-word posture so a model name that is not a posture word is read as the value: `spawn ab-X model opus`, `spawn ab-X codex model gpt-5`. "on opus" / "use sonnet" map here. Default = the provider's default. There is NO short flag: `-m` is `--allow-merge`, so a bare `-m opus` would set merge, not the model - always write `model <name>`.
- **`effort <value>`**: reasoning-effort tier, plumbed to `fno agents spawn --effort`. Orthogonal to `model`: the model selects which model runs, while effort tunes how hard it reasons. The CLI validates the provider-specific vocabulary and rejects unmappable values before spawning.
- **`--permission-mode <v>`**: the worker's harness permission posture, passed straight to `fno agents spawn --permission-mode` (claude `default|acceptEdits|plan|bypassPermissions`; codex/agy/opencode mapped). Value validation is the CLI's (fail-closed), not the skill's.
- **Tier-3 harness passthrough** (x-b6e2): `--add-dir <dir>` grants extra write access (claude/codex/agy; additive). `--agent <name>` pins its sub-agent (claude/opencode). `--tools <list>` / `--deny-tools <list>` scope its tool set (claude `--allowedTools`/`--disallowedTools`). Forwarded straight to `fno agents spawn`, opaque to the skill; the CLI maps or fails closed per provider. A no-equivalent provider cell is rejected before spawn.

## Observing a worker's state (the observability boundary)

- **exec (default):** codex/agy never surface a "waiting" state in the exec lane. When an action needs approval, codex auto-rejects it and continues; agy aborts the run. Watch via `fno agents list` (status -> `exited`) and `fno agents logs <name>`.
- **interactive (`-i`):** the TUI genuinely waits at approval prompts. See it via `fno agents grid <name>` / `fno agents drive <name> --mode interactive`.
- **no proactive push** fires for a codex/agy worker today. When a claude `--bg` `/target` worker stalls, it does fire `fno inbox notify`.
