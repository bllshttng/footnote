# Role-based model routing

fno spawns every claude worker on the primary model (Anthropic Opus, billed to the Max/coding pool). There was no per-task model selection, so auxiliary coordination work (backlog tidying, node orientation, memory consolidation) burned expensive coding usage. Role-based routing sends low-stakes coordination to a secondary provider (z.ai GLM by default, DeepSeek or others by config) while production work (writing the diff, the correctness verdict) stays on the primary model, without replacing the main models and without a proxy in the critical path.

## Why route by role, not task

A spawn's *role* is what it is doing, not what it is touching. `coordinate | tidy | orient | consolidate` shuffle the backlog and consolidate memory: route them. `implement | review-verdict` are reserved names that never route. Keying on role keeps the policy a tiny table instead of a per-task classifier.

The reserved names have since drifted from the dispatch surface, and the table below is the honest state rather than the original intent. Read [What the guard does and does not cover](#what-the-guard-does-and-does-not-cover) before treating either name as protection.

## Mechanism: per-spawn env

Each worker is a fresh `claude --bg` process, which speaks the **Anthropic** Messages API. A provider is usable here only via its Anthropic-compatible endpoint (z.ai: `https://api.z.ai/api/anthropic`; DeepSeek: `https://api.deepseek.com/anthropic`). The OpenAI-protocol endpoints the same vendors publish (z.ai's `/api/coding/paas/v4`) are for OpenAI-SDK consumers and a future codex/openai lane, not for a claude worker; a provider whose `protocol` is not `anthropic` is skipped here with a notice.

Routing stamps these env vars into the worker at spawn time:

```
ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic   # the provider's Anthropic endpoint
ANTHROPIC_AUTH_TOKEN=<provider key>                  # Bearer auth
ANTHROPIC_MODEL=glm-5.3                              # the routed model
ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.3                 # all tiers set to the routed
ANTHROPIC_DEFAULT_SONNET_MODEL=glm-5.3               #   model so the WHOLE worker
ANTHROPIC_DEFAULT_HAIKU_MODEL=glm-5.3                #   (incl. background haiku) routes
```

Claude Code internally requests opus/sonnet/haiku tiers (background tasks use haiku). Setting all four model vars to the routed model sends the entire worker to the secondary provider, so no Anthropic usage is recorded. Switching `base_url` per spawn is safe because each worker is its own process; the base_url is never switched mid-session. A stale `ANTHROPIC_API_KEY` inherited from the parent env is cleared on a routed spawn so the provider token wins.

## Shape

```
cmd_spawn --role  ->  dispatch_spawn  ->  _claude_create_path  ->  bg_create(role=...)
                                                                       |
                                                            resolve_route(role)
                                                                       |
                              {ANTHROPIC_BASE_URL, _AUTH_TOKEN, _MODEL, _DEFAULT_*_MODEL} | None
                                                                       |
                                                 None -> spawn env unchanged (primary model)
                                                 dict -> merged into spawn env (secondary)
```

`fno.agents.model_routing.resolve_route(role) -> dict | None` is the whole policy. `None` means "use the primary model, change nothing." The only hook point is `bg_create`'s spawn-env builder (`cli/src/fno/agents/harnesses/claude.py`).

## Two non-negotiable invariants

**Hard quality guard.** `implement` and `review-verdict` are in `PROTECTED_ROLES` and short-circuit to `None` *before* any config is read. No settings edit, however malformed, can make **either of those two role names** resolve to a secondary provider. The guard is structural, not a default. What it does not cover is below.

**Fail safe, not fail closed.** If no key is configured for the role's provider (the named env var / `.env` file has none), the role falls back to the primary Anthropic model with a one-line stderr notice, and the spawn still succeeds. `resolve_route` never raises.

## What the guard does and does not cover

The guard covers two role *names*. It does not cover the two things a reader reasonably assumes it covers.

**It does not keep the diff on the primary model.** `build` is a routable lane carrying exactly the payload `implement` names: `skills/target/scripts/dispatch-node.sh` attaches `--role build` to claude node dispatch, so a configured `build` route sends the worker that writes the diff to a secondary provider. That is deliberate, and config presence is the consent, but it means "no settings edit can route the diff" is false. `implement` is guarded; the lane that actually delivers is not.

**It does not decide the reviewer's model.** No dispatch surface anywhere passes `--role review-verdict`; the name resolves nothing because nothing declares it. The model that renders a correctness verdict is the model of the session that runs the review, and routing sets every entry in `MODEL_ENV_KEYS` for the whole worker process. So a worker routed by `build` renders its own `/code-review` verdict on the routed model, and no per-spawn role guard can see that, because the verdict is a later activity inside an already-routed process. Keep the reviewer off the authoring worker (see [review lanes](review-lanes.md)); a role table cannot enforce it.

`review_attestation` records the `model` and `provider` in effect when a local verdict was emitted, so this is auditable after the fact rather than assumed. Both fields are optional and best-effort: they report what the worker's environment *claimed*, which is not proof of the model that answered. Empty means *not observable*, not "primary" - `resolve_codex_route` carries a codex worker's route in `-c model=...` config args and puts only the API key in the environment, so a routed codex verdict records empty on both fields.

## Config

`config.model_routing` in `~/.fno/config.toml` (global) or `.fno/config.toml` (project-local override):

| Key | Default | Purpose |
|-----|---------|---------|
| `enabled` | `true` | Master on/off. |
| `providers` | _(built-in `zai`)_ | Name → `{protocol, base_url, api_key_env, api_key_file}`. Add `deepseek` etc.; override `zai` per field. |
| `roles` | _(built-in → `zai/glm-5.3`)_ | Role → `"provider/model"` (e.g. `tidy: "zai/glm-4.7"`; legacy comma `zai,glm-4.7` also accepted). |
| `extra_env` | `{}` | Extra env merged into routed spawns (e.g. `API_TIMEOUT_MS`, a cheaper per-tier model). |

A worked example:

```yaml
config:
  model_routing:
    enabled: true
    providers:
      # zai is built in (api/anthropic + ZAI_API_KEY); listed only to override or extend.
      deepseek:
        protocol: anthropic
        base_url: https://api.deepseek.com/anthropic
        api_key_env: DEEPSEEK_API_KEY
    roles:
      coordinate: "zai/glm-4.7"
      tidy: "zai/glm-4.7"
      orient: "zai/glm-4.7"
      consolidate: "zai/glm-5.3"
    extra_env:
      API_TIMEOUT_MS: "3000000"
```

The key (secret) never lives in `config.toml`: it is read from the process env var named by the provider's `api_key_env` (the built-in `zai` uses `ZAI_API_KEY`), falling back to `api_key_file` (e.g. modelkit's `.env`); process env wins. The endpoint and model are config fields, so swapping a vendor's endpoint or bumping the GLM version is a settings edit, not a code change.

## The `build` delivery lane

The auxiliary roles above are coordination work. `build` extends the same mechanism to *delivery* spawns (`/target bg` + blueprint autolaunch), so a whole feature build can run on GLM.

`build` is **opt-in by config presence**: it ships unconfigured and routes nothing (fail-safe `None`, byte-identical to today). Writing the roles line IS the consent:

```bash
fno config route set build zai/glm-5.3[1m]        # atomic config write; effect: next spawn
```

`dispatch-node.sh` passes `--role build` on every worker spawn unconditionally. The fail-safe makes that a no-op until the lane is configured, so there is no conditional plumbing. Each dispatch receipt carries a `route=` token. When the lane resolved, the token reads `route=zai/glm-5.3`. When it fell back, the token reads `route=primary`. A build that silently reverted to Anthropic - a keyless lane - is visible at the call site, not just in a buried stderr notice.

For a one-off "just this node on GLM" without flipping the lane default, `dispatch-node.sh <node> --route provider/model` (or `fno agents spawn --route ...`) forwards an explicit route. Unlike the role lane, an explicit `--route` **fails closed**: an unknown provider, non-anthropic protocol, or missing key refuses the spawn (you asked for GLM by name; billing Anthropic instead would violate intent). `--route` wins over a configured `build` lane on the same spawn.

## The `pr-create` lane

`/pr create` dispatches its worker on the `pr-create` role, not a hardcoded model tier. The role used to be a `model: haiku` literal baked into the agent; it now flows from `config.model_routing` so a Codex session opens its PR on its own model and an operator can route the cheap mechanical worker to a secondary provider without forking the skill.

`pr-create` is **opt-in by config presence**, exactly like `build`: it ships unconfigured and routes nothing (fail-safe `None`, so the worker runs on the invoking harness's primary model - no model literal in the skill). Writing the roles line IS the consent:

```bash
fno config route set pr-create zai/glm-4.7      # atomic config write; effect: next /pr create
```

The `/pr create` dispatch declares `--role pr-create` (or omits any `model:` override) at the spawn boundary, so the fail-safe makes the role a no-op until the lane is configured. The worker keeps its fresh, minimal context - branch, base, a one-line summary, and merge posture only - regardless of which model the role resolved to, because the small-context property is what makes the worker cheap, not the tier name.

## The stage table: per-verb profile overlay

Role routing keys on *what the worker is doing* (`--role build`). The stage table keys on *which verb started it*: `config.agents.profiles.<verb>` overlays `agents.defaults` field-by-field, selected by the seed's leading slash-verb (`/fno:blueprint x-123` -> the `blueprint` profile). It is the per-stage axis coordinate.

The stage table reaches **every** spawn that carries a slash-verb seed, including autonomous dispatch.
`skills/target/scripts/dispatch-node.sh` passes the verb as the spawn's positional message, so an autonomous `/target` or `/blueprint` worker inherits any field it did not itself pin from the matching profile.
An explicit flag always wins, and a `--role` whose lane resolves owns the model, so the role and stage layers do not collide on the model: a stage table `model` is not injected alongside a resolving role, and a stage table `route` owns the model the same way an explicit `--route` does.

```toml
[agents]
pane_group_max = 4

[agents.profiles.blueprint]
model = "opus"

[[agents.profiles.target.lanes]]
provider = "codex"
effort = "high"
substrate = "pane"
permission_mode = "yolo"
pane_group = "codex"

[[agents.profiles.target.lanes]]
provider = "claude"
route = "zai/glm-5.3[1m]"
substrate = "bg"
```

When a profile has `lanes`, the live-worker count chooses the round-robin start. Selection then skips forward past a routed vendor already at `agents.provider_limits`. If every lane is capped, or the provider count is incomplete, the spawn refuses rather than billing an unintended lane. Two escapes narrow that refusal, and neither weakens the skip. A command line can name its own lane with `--harness`, `-P` or `--route`. That spawn is not spending a capped vendor's budget, so it continues with no lane applied. `FNO_SPAWN_GATE=0`, the admission bypass, never blocks a spawn either. Substrate and `permission_mode` ride the lane, not the profile. The Claude/GLM lane can use `bg`, and the Codex lane needs a pane.

`pane_group` is injected as `fno agents spawn --tab <group>` on pane lanes. Placement happens AFTER the spawn, never before it. The pane reports its own squad, and the tab list is read scoped to that squad. The pane's own tab then joins the first `<group>`, `<group>-2`, ... tab below `pane_group_max`. If none has room, its tab takes the next sibling name instead. A group cannot combine with `--split`/`--at`. The pane then sits in a tab it does not own. The move takes that tab's other panes with it. The read-then-act is deliberately not globally serialized, so concurrent spawns can briefly overfill a tab. Placement never changes the worker route.

The two layers compose by design. The stage table picks the coordinate per verb; `--role`, attached by the dispatch lane, owns the model when it resolves. A field the dispatch pinned explicitly (harness, substrate) is not displaced, which is why a stage table entry can set the model or route without rerouting the fleet's binary.

`fno config doctor` checks the resolved posture before a worker is launched. It reports a substrate/provider pair the spawn seam cannot honor. It also probes whether THIS session can write the claim store, by writing a real file there and removing it. A hand-started session cannot receive a per-spawn grant, so that probe is the only thing covering it. A spawned worker is covered instead by the computed `--add-dir` set (see [coordination.md](coordination.md)).

## `fno config route` - legibility + on-the-fly switching

Four verbs over the same machinery (`model_routing.py` stays the single source of the env-var contract):

| Verb | Purpose |
|------|---------|
| `fno config route ls [-J]` | The effective merged table: role → `provider/model` → protocol → key status (which env var / file satisfied it, or MISSING) → auto-assigned-by. `-J` for scripts. |
| `fno config route set <role> <provider/model>` | Route a lane (atomic config write via `fno config set`). Refuses protected names + unknown providers pre-write. |
| `fno config route unset <role>` | Revert a lane to its built-in default (or unrouted); idempotent no-op if unconfigured. |
| `fno config route env <role \| provider/model>` | Print an eval-able export block for an interactive session: `eval "$(fno config route env build)" && claude`. Fails closed on a missing key (no partial block). |

`route env` is the sanctioned interactive switch - never editing `~/.claude/settings.json` (global, restart-bound, races parallel sessions). The `ccz`-style alias becomes a one-liner over it.

## GLM-5.2 operational defaults

A routed GLM worker wants a couple of env tweaks, carried by `extra_env` (config, not code):

```yaml
config:
  model_routing:
    roles:
      build: "zai/glm-5.3[1m]"          # [1m] = 1M-context; auto-injects the 800k auto-compact backstop
    extra_env:
      API_TIMEOUT_MS: "3000000"
```

A `[1m]` worker auto-injects `CLAUDE_CODE_AUTO_COMPACT_WINDOW=800000` as the compaction backstop. The `[1m]` variant already selects the 1M context; this var is the compaction threshold (how full the window gets before compaction), capped at the model window, so a value of `1000000` is a no-op (no compaction before the ceiling). The king handoff nudge fires at ~40%; 800000 (~80%) is the backstop above it. Override it via `extra_env` only to tune the backstop - setting `1000000` re-removes it.

The built-in `zai` provider already routes the background (haiku) tier to the cheaper `glm-4.7`. Opus/sonnet run `glm-5.3`, while judgment-light background traffic stays cheap on the same provider.

**`/effort` mapping.** GLM collapses `low`/`medium`/`high` to a single high setting; only `xhigh`/`max` reach its maximum reasoning. Pin a routed build lane to `high` or above (`--effort high`); a lower effort buys nothing on GLM.

## Inherited env and the daemon carrier

A long-lived background daemon holds a copy of the env of its first shell. It re-stamps that copy into every session it spawns. A shell with foreign model exports and no base URL poisons every child. Each child asks Anthropic for a model it does not serve. The whole tier errors rather than degrading. No config edit reaches a running daemon. Only a restart or a settings pin does.

`~/.claude/settings.json` `env` wins over an inherited value. That is the durable pin. It is the only fix that spares live sessions. The current Anthropic ids to pin, one per tier: `ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-haiku-4-5-20251001`, `ANTHROPIC_DEFAULT_SONNET_MODEL=claude-sonnet-5`, `ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-5`, `ANTHROPIC_DEFAULT_FABLE_MODEL=claude-fable-5`.

**The naming trap.** There is no Haiku 4.7. The current lineup is Haiku 4.5, with Sonnet, Opus and Fable at 5. The zai provider's `haiku_model` IS `glm-4.7`. That number does not transfer to an Anthropic id. Guessing `claude-haiku-4-7` fails exactly the way the GLM names do.

The code side of the same defense: `incoherent_model_env` (`cli/src/fno/agents/model_routing.py`) names every offending model var. An offending var carries a non-Anthropic id while the endpoint is Anthropic's. The substrate seams that copy the parent env strip those vars before any route overlay. Those seams are `bg_create`, `headless_create`, `_default_wake_fn`, and `_mesh_env_wrapper`. Each strip prints one stderr line. A bg spawn needs more than the strip. The serving session is forked by the claude daemon with the daemon's own env. So `bg_create` also floats a `--settings` file flooring the offending vars. The Python front door never scrubs `os.environ` at the routing seam. That scrub blinds `bg_create`'s floor decision. The compiled client is reachable without the front door (`fno-agents spawn`, the loop runtime). Its own spawn arms scrub the child env and float the same floor (`crates/fno-agents/src/model_env_scrub.rs`). A real route is never stripped. A foreign base URL serves those model ids, so the predicate returns empty. When the daemon itself pre-warms a spare session, a spawn-time scrub cannot reach it: the process already exists. The settings pin covers that case. The SessionStart detector (`hooks/attest-model.sh`) warns over the same five vars. It bails out on Bedrock/Vertex lanes. A parity test pins its var list to `MODEL_ENV_KEYS`. A new tier cannot land in one list and not the other.

## Scope and deferrals

Wires native per-spawn routing for the claude lane (Anthropic-protocol providers) with the fail-safe fallback and the hard guard. `extra_env` is the escape hatch for differentiated tiers (e.g. a cheaper `ANTHROPIC_DEFAULT_HAIKU_MODEL`). Deferred: a codex/openai lane that consumes the same provider registry over the OpenAI-protocol endpoints; claude-code-router (CCR) for routing an *in-session* subagent to a non-Anthropic provider; a config UI for editing roles (hand-edit is acceptable first). `consolidate` is already served out-of-repo by modelkit/memdream, which calls z.ai directly.

## Sigma panel routes

`review.agent_routes` optionally assigns a complete `harness`, route `provider`, and `model` tuple to a named sigma reviewer. Each configured reviewer starts its own named session, so a six-agent panel pays six SessionStart preambles. At the measured 50–60K tokens per preamble, a six-agent panel costs roughly 300–360K tokens before review work. Whenever the full panel must share one model, use whole-session routing.

```yaml
config:
  review:
    agent_routes:
      code_reviewer:
        harness: claude
        provider: zai
        model: glm-5.3
```
