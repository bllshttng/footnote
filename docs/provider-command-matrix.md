# Provider × command capability matrix

`fno agents` is one surface over four CLIs (`claude`, `codex`, `gemini`, `agy`), but the CLIs are not symmetric: some verbs are claude-only, some are codex/gemini-only, and the live-conversation transport differs per provider. This page is the source of truth for which `fno agents <verb>` works against which provider. (The deep transport internals are maintained internally.)

> **agy (Antigravity CLI), Phase C.** agy is a dispatchable worker via `spawn --provider agy --once` (a one-shot `agy -p`, plain-text reply) and, post-x-3ab8, a plain `spawn --provider agy` resolves a live owned-PTY pane (interactive by default). It is held to the columns below only where verified: `spawn` **yes**. Because agy v1.0.x emits plain text with **no parseable session id**, it is **stateless** — the live pane works while attached, but there is **no re-attach after it settles** (no resume id to key on); `ask`-by-name resume is refused (use a fresh `--once`), `resume`/`attach` are **no**, and reachability is always inconclusive (never orphaned). `host`/`promote`/`drive`/`grid` are untested for agy this release.

> **opencode.** opencode has **no PTY provider** in `fno-agents` (readiness + stop-hook only), so it is **headless-only**: `spawn --provider opencode` cannot resolve an interactive pane and stays a non-tiled run. The x-3ab8 interactive default applies only to the four PTY providers; opencode is unaffected. File a follow-up node if an opencode PTY provider is wanted.

## The matrix

Legend: **yes** / **no** / **n/a** / **partial** (works under a stated condition).

| Verb | claude | codex | gemini | Notes |
|------|:------:|:-----:|:------:|-------|
| `spawn` | yes | yes | yes | Create + register. **Interactive by default** (x-3ab8): a plain `spawn --provider <claude\|codex\|gemini\|agy>` lands an owned, grid-drivable interactive pane (the daemon owns the PTY, so it still runs fire-and-forget; a grid attaches/takes over later). `--once` is the headless opt-out — claude `--bg`, codex `exec`, gemini one-shot, agy `-p` — not shown in the grid. |
| `promote --from <uuid>` | yes | yes | yes | Adopt a settled session into a live host. claude → stream-json lane; codex/gemini → PTY host. |
| `host [--provider P] [task]` | yes | yes | yes | Now a thin alias for `spawn --mode interactive` (x-3ab8) — kept for callers/docs/muscle-memory, not deprecated. Forces the owned interactive lane for any PTY provider; for claude it mints the pinned session id and hosts a fresh subscription-billed owned-PTY pane. To adopt an *existing* idle claude session instead, use `promote --from <uuid>`. |
| `ask <name>` (sync) | partial | yes | yes | claude live-ask is reachable only for MCP-channel sessions, or adopt with `promote` then `send`. codex/gemini intercept the reply client-side. |
| `send <name>` / `--to-project` | yes | yes | yes | Async, durable-first bus delivery; never waits for a reply. |
| `chat A B "<seed>"` | yes | no | no | Costed, always-confirm. Drives a bounded A↔B relay; v1 claude↔claude only. Observe with `watch`. |
| `inbox` / `ack` | yes | yes | yes | Bus-cursor read + advance; registry-agnostic. |
| `watch <name>` | yes | no | no | Observe a held thread's turns. The headless analog of `drive`/`grid`. |
| `drive` / `grid` | partial | yes | yes | PTY-TUI driving. claude tiles in the grid as an owned interactive PTY pane (`host --provider claude --mode interactive`, or `>`-prefix in the grid launcher); codex/gemini via their PTY host. An adopted stream-json claude is `watch` + `send` only. |
| `attach <name>` | yes | no | no | Re-exec into the running session's own TUI. |
| `resume <name>` | yes | yes | yes | Re-exec the provider's resume CLI in the agent's recorded cwd. |
| `register-channel` / `push-channel` / `unregister-channel` | yes | no | no | MCP channel sidecar; claude-only this release. |
| `list` / `logs` / `stop` / `rm` / `reconcile` / `trace` / `status` / `ping` / `restart` | yes | yes | yes | Provider-agnostic registry / admin. |

## Why the asymmetries exist

codex and gemini are driven through a pseudo-terminal the daemon owns. claude now has its own daemon-owned interactive PTY lane too (`host --provider claude --mode interactive`): the daemon spawns subscription-billed `claude --session-id <uuid>` (never `-p`), so the grid tiles it as an owned pane you type straight into. claude is also reachable two other ways without a fresh PTY: a **stream-json host lane** (`promote --from <uuid> --provider claude`, the live lane you adopt an idle session into) or an **MCP channel** sidecar (reaches only sessions launched with the channel). Live `ask` against claude is `partial` for the same reason.

## See also

- [provider-rotation.md](provider-rotation.md) - provider records, failover, and the switchboard settings schema.
- `skills/using-abilities/SKILL.md` - the two-surface orientation loaded each session.
