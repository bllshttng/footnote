# Role-based model routing

fno spawns every claude worker on the default model (Anthropic Opus, billed to the Max/coding pool). There was no per-task model selection, so cheap coordination work (backlog tidying, node orientation, memory consolidation) burned expensive coding usage. Role-based routing sends low-stakes coordination to cheap GLM (z.ai) while production work (writing the diff, the correctness verdict) stays on Opus, without replacing the main models and without a proxy in the critical path.

## Why route by role, not task

A spawn's *role* is what it is doing, not what it is touching. `coordinate | tidy | orient | consolidate` shuffle the backlog and consolidate memory: cheap. `implement | review-verdict` write code and render the correctness verdict: the money model. Keying on role keeps the policy a tiny fixed table instead of a per-task classifier.

## Mechanism: per-spawn env

Each worker is a fresh `claude --bg` process. Routing stamps three env vars into that worker's environment at spawn time:

```
ANTHROPIC_BASE_URL=https://api.z.ai/api/coding/paas/v4   # config.model_routing.zai_base_url
ANTHROPIC_AUTH_TOKEN=<z.ai key>                           # Bearer auth
ANTHROPIC_MODEL=glm-5.2                                   # config.model_routing.default_model
```

That routes the whole worker to GLM, billed to the z.ai pool, zero Anthropic usage. No proxy sits in the path. Switching `base_url` per spawn is safe precisely because each worker is its own process; the base_url is never switched mid-session.

A stale `ANTHROPIC_API_KEY` inherited from the parent env is cleared on a routed spawn so the z.ai `ANTHROPIC_AUTH_TOKEN` is the credential that wins.

## Shape

```
cmd_spawn --role  ->  dispatch_spawn  ->  _claude_create_path  ->  bg_create(role=...)
                                                                       |
                                                            resolve_route(role)
                                                                       |
                                          {ANTHROPIC_BASE_URL, _AUTH_TOKEN, _MODEL} | None
                                                                       |
                                                 None -> spawn env unchanged (default model)
                                                 dict -> merged into spawn env (GLM)
```

`fno.agents.model_routing.resolve_route(role) -> dict | None` is the whole policy. `None` means "use the default model, change nothing." The only hook point is `bg_create`'s spawn-env builder (`cli/src/fno/agents/providers/claude.py`).

## Two non-negotiable invariants

**Hard quality guard.** `implement` and `review-verdict` are in `PROTECTED_ROLES` and short-circuit to `None` *before* any config override is read. No settings edit, however malformed, can route the diff or the verdict to a cheap provider. The guard is structural, not a default.

**Fail safe, not fail closed.** If no z.ai key is configured (or the named env var / `.env` file has none), a cheap role falls back to the default Anthropic model with a one-line stderr notice, and the spawn still succeeds. `resolve_route` never raises.

## Config

`config.model_routing` in `~/.fno/settings.yaml` (project-local override allowed):

| Key | Default | Purpose |
|-----|---------|---------|
| `enabled` | `true` | Master on/off for cheap routing. |
| `zai_base_url` | `https://api.z.ai/api/coding/paas/v4` | z.ai endpoint for routed spawns. |
| `default_model` | `glm-5.2` | Default cheap model for routed roles. |
| `overrides` | `{}` | `role -> "provider,model"` map (e.g. `tidy: "zai,glm-4.5-air"`). |
| `zai_key_env` | `ZAI_API_KEY` | Env var name holding the z.ai key. |
| `zai_env_file` | _(none)_ | Optional path to a `.env` file holding the key (e.g. modelkit's `.env`). |

Everything except the secret is set in `config.model_routing` in `~/.fno/settings.yaml` (global) or `.fno/settings.yaml` (project-local override). The endpoint and the default model are both config fields, so swapping the z.ai endpoint or bumping the GLM version is a settings edit, not a code change. The key itself never lives in `settings.yaml`: it is resolved from the process env var named by `zai_key_env`, falling back to the `.env` file at `zai_env_file`; process env wins (mirrors modelkit's precedence). `glm-4.5-air` is too weak for reasoning-bearing work, so `default_model` is `glm-5.2`; pin a cheaper model per role for trivial classification via an override.

## v1 scope and deferrals

v1 wires native per-spawn routing for the claude lane with the fail-safe fallback and the hard guard. Only the `zai` provider is wired; an override naming another cheap provider degrades to the default model rather than erroring, keeping the multi-provider story (codex/gemini using their own keys) forward-compatible. Deferred: claude-code-router (CCR) for routing an *in-session* subagent to a non-Anthropic provider (the one case native env cannot cover), and a config UI for flipping roles (hand-edit is acceptable first). `consolidate` is already served out-of-repo by modelkit/memdream, which calls z.ai directly.
