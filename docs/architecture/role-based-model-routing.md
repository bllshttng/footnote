# Role-based model routing

fno spawns every claude worker on the primary model (Anthropic Opus, billed to the Max/coding pool). There was no per-task model selection, so auxiliary coordination work (backlog tidying, node orientation, memory consolidation) burned expensive coding usage. Role-based routing sends low-stakes coordination to a secondary provider (z.ai GLM by default, DeepSeek or others by config) while production work (writing the diff, the correctness verdict) stays on the primary model, without replacing the main models and without a proxy in the critical path.

## Why route by role, not task

A spawn's *role* is what it is doing, not what it is touching. `coordinate | tidy | orient | consolidate` shuffle the backlog and consolidate memory: route them. `implement | review-verdict` write code and render the correctness verdict: primary model only. Keying on role keeps the policy a tiny table instead of a per-task classifier.

## Mechanism: per-spawn env

Each worker is a fresh `claude --bg` process, which speaks the **Anthropic** Messages API. A provider is usable here only via its Anthropic-compatible endpoint (z.ai: `https://api.z.ai/api/anthropic`; DeepSeek: `https://api.deepseek.com/anthropic`). The OpenAI-protocol endpoints the same vendors publish (z.ai's `/api/coding/paas/v4`) are for OpenAI-SDK consumers and a future codex/openai lane, not for a claude worker; a provider whose `protocol` is not `anthropic` is skipped here with a notice.

Routing stamps these env vars into the worker at spawn time:

```
ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic   # the provider's Anthropic endpoint
ANTHROPIC_AUTH_TOKEN=<provider key>                  # Bearer auth
ANTHROPIC_MODEL=glm-5.2                              # the routed model
ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.2                 # all tiers set to the routed
ANTHROPIC_DEFAULT_SONNET_MODEL=glm-5.2               #   model so the WHOLE worker
ANTHROPIC_DEFAULT_HAIKU_MODEL=glm-5.2                #   (incl. background haiku) routes
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

`fno.agents.model_routing.resolve_route(role) -> dict | None` is the whole policy. `None` means "use the primary model, change nothing." The only hook point is `bg_create`'s spawn-env builder (`cli/src/fno/agents/providers/claude.py`).

## Two non-negotiable invariants

**Hard quality guard.** `implement` and `review-verdict` are in `PROTECTED_ROLES` and short-circuit to `None` *before* any config is read. No settings edit, however malformed, can route the diff or the verdict to a secondary provider. The guard is structural, not a default.

**Fail safe, not fail closed.** If no key is configured for the role's provider (the named env var / `.env` file has none), the role falls back to the primary Anthropic model with a one-line stderr notice, and the spawn still succeeds. `resolve_route` never raises.

## Config

`config.model_routing` in `~/.fno/settings.yaml` (global) or `.fno/settings.yaml` (project-local override):

| Key | Default | Purpose |
|-----|---------|---------|
| `enabled` | `true` | Master on/off. |
| `providers` | _(built-in `zai`)_ | Name → `{protocol, base_url, api_key_env, api_key_file}`. Add `deepseek` etc.; override `zai` per field. |
| `roles` | _(built-in 4 → `zai,glm-5.2`)_ | Role → `"provider,model"` (e.g. `tidy: "zai,glm-4.7"`). |
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
      coordinate: "zai,glm-4.7"
      tidy: "zai,glm-4.7"
      orient: "zai,glm-4.7"
      consolidate: "zai,glm-5.2"
    extra_env:
      API_TIMEOUT_MS: "3000000"
```

The key (secret) never lives in `settings.yaml`: it is read from the process env var named by the provider's `api_key_env` (the built-in `zai` uses `ZAI_API_KEY`), falling back to `api_key_file` (e.g. modelkit's `.env`); process env wins. The endpoint and model are config fields, so swapping a vendor's endpoint or bumping the GLM version is a settings edit, not a code change.

## Scope and deferrals

Wires native per-spawn routing for the claude lane (Anthropic-protocol providers) with the fail-safe fallback and the hard guard. `extra_env` is the escape hatch for differentiated tiers (e.g. a cheaper `ANTHROPIC_DEFAULT_HAIKU_MODEL`). Deferred: a codex/openai lane that consumes the same provider registry over the OpenAI-protocol endpoints; claude-code-router (CCR) for routing an *in-session* subagent to a non-Anthropic provider; a config UI for editing roles (hand-edit is acceptable first). `consolidate` is already served out-of-repo by modelkit/memdream, which calls z.ai directly.
