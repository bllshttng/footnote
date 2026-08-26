# Axis vocabulary: harness, provider, model, effort, account

This document is the single source for footnote's axis definitions. The code, receipts, environment, config, and skills conform to the table below. These definitions guide implementation and review. No CI vocabulary ratchet enforces them. When a reviewer and an agent disagree about an ambiguous site, this document resolves it for both.

## The five axes

| Axis | Means | Values |
|---|---|---|
| harness | the CLI binary being launched | claude code, codex cli, oh-my-pi, openclaw, agy, opencode |
| provider | an issuer of models | anthropic, openai, zai, deepseek, google/gemini |
| model | the LLM answering | opus, gpt-5.6-sol, sonnet, glm-5.2[1m] |
| effort | how much reasoning the model spends | low, medium, high, xhigh, max |
| account | a login granting access to a harness or provider, carrying an API key or OAuth credential | the `config.accounts.records` entries |

The CLI flags already name each axis correctly and are out of scope for any rename:
`-H/--harness`, `-P/--provider`, `-m/--model`, `--effort`, `--account`.

## Why the axes collide

The axes are orthogonal but not independent.
A provider may create a harness, may deliver models, or may do both:

| Provider | Creates harness | Delivers models |
|---|---|---|
| Anthropic | Claude Code | opus, sonnet, haiku |
| OpenAI | Codex CLI | gpt-5.6-sol |
| Google | Gemini CLI / Antigravity | gemini family |
| zai | - | glm-5.2 |
| deepseek | - | deepseek models |

Every provider that ships a harness shares a colloquial name with it.
`claude` is a harness whose provider is Anthropic, and "claude" also reads as the vendor in ordinary speech; `codex` and `gemini` are the same, and `gemini` additionally names a model family.
That shared name is why `provider = "claude"` looked reasonable to whoever wrote it, and why four prior surface-local renames did not stick: the collision is the domain, not a typo.

## The three rules

**Value rule.** The correct value under a `provider` key for a `claude` harness is `anthropic`, never `claude`.
The provider-to-harness mapping is fixed:

| harness | provider |
|---|---|
| claude | anthropic |
| codex | openai |
| gemini | google |
| agy | (google, via Antigravity) |
| opencode | (operator-configured; opencode is also itself a provider) |

A receipt that renames a `provider` key to `harness` and then emits a second `provider` field still reading `claude` has fixed nothing.
The defect is the value under the name, not the name alone.

**Review rule.** Treat only a harness value bound to a provider-named identifier as a defect. `anthropic` under `provider` is correct. `claude` under `provider` is the defect. Calling the legal case a defect obscures the distinction this vocabulary exists to preserve.

**Name rule.** A directory named for one axis must not hold another axis's implementation.
This is the rule the first two cannot reach.
Both read the contents of files, and a path is only a traversal input.
So `cli/src/fno/agents/providers/` held harness adapters for a long time while `cli/src/fno/adapters/providers/` held real providers and accounts.
Nothing failed.
Two directories, one name, two axes: a reader who learns one cannot predict the other.
The harness package is now `cli/src/fno/agents/harnesses/`, and the harness docs are now `docs/harnesses/`.

## Named exception: the `agents.defaults.provider` config key

One config key breaks the naming convention on purpose.

`agents.defaults.provider` carries harness values (`claude/codex/gemini/agy/opencode`) and loses to `-H`. The flag `-P/--provider` carries vendor values. The same word names two axes.

The value rule reads a key's harness values as correct, because they are legal for the harness axis. Only the name collides.

Set the vendor axis with `agents.defaults.route` (vendor/model, position-carried). Treat `agents.defaults.provider` as the harness default it behaves as.

The key is deliberately not renamed here. A rename breaks every config that set it. It needs its own node with a deprecation path.

## Effort: one axis, three harness spellings

The caller sets one value on the effort axis. Each harness spells the reasoning-effort parameter differently, and `thinking.type` and `reasoning.effort` are both effort, just spelled differently. fno translates the flag spelling and passes the value through. The provider/model owns the accepted vocabulary, so fno does not keep a per-provider allowlist.

| Harness | Parameter the harness names | How fno emits it |
|---|---|---|
| claude | `thinking.type`, `output_config.effort` | `--effort <value>` |
| codex | `reasoning.effort` | `-c model_reasoning_effort=<value>` |
| opencode | provider/model-defined | no token emitted |

`effort_tokens` in `cli/src/fno/agents/mux_spawn.py` owns only this flag translation. Gemini and agy have no fno effort surface and refuse `--effort`. Other harnesses pass the value to their provider CLI.

claude accepts `max`, and codex does not. codex accepts `minimal`, and claude does not. An `--effort` value valid for one harness can be invalid for another, so `effort_tokens` validates against the resolved harness's own set, not the union.

## Provider translation, and why an unset effort costs money

A harness's own effort parameter is not the final word. The provider on the other end of the wire applies its own translation, recorded verbatim from z.ai's own model documentation.

| Input | Resolves to |
|---|---|
| `thinking.type` unset / `true` / `enabled` / `adaptive` | `max` (the default) |
| `thinking.type` `false` / `disabled` / `none` / `off` | `low` (not off) |
| `reasoning_effort` `minimal` / `light` / `low` | `low` |
| `reasoning_effort` `medium` / `high` | `high` |
| `reasoning_effort` `xhigh` / `max` / `ultra` | `max` |
| `reasoning_effort` anything unrecognised | `max`, with a logged hint |

Priority, verbatim: "Explicit Effort > thinking toggle > default `max`".

An unset effort resolves to `max`. fno spawns most workers without one, so a mechanical task pays maximum reasoning tokens by default.

## Resolver authority

`inject_spawn_defaults` (`cli/src/fno/agents/spawn_defaults.py`) decides which config value fills which axis on a spawn. It holds one rule: an explicit command-line axis is never overwritten by a profile default. A profile can fill an axis the command line left unset. A profile-filled harness that cannot carry an already-typed route is the case this plan handles. When that fill makes an explicitly-set axis unusable, the refusal names the config path, the value, the axis it set, and the caller's own flags. This is a cross-axis collision, not a precedence bug. No field-wise rule was ever violated, so the report says what happened instead of what looks like an override.

## Ambiguous values

`opencode` is a legal harness and a legal provider.
`gemini` names a harness, a provider, and a model family.
The axis can never be inferred from a value, so no mechanical rename can be trusted: every site is read for intent.

When the surrounding type or field name does not reveal the axis, document the ambiguous binding at the site.

## Recognized and unrecognized harness values

Session-marker detection (`HARNESS_SESSION_MARKERS` in `cli/src/fno/harness_identity.py`) recognizes four harnesses today: `codex`, `claude`, `gemini`, `opencode`.
`agy` dispatches through its own adapter (`crates/fno-agents/src/agy_ask.rs`) but has no session marker.
`oh-my-pi` and `openclaw` are operator-named harnesses the code does not model at all yet; they appear in this table as legal vocabulary, not as values any code path recognizes.

A literal like `agy` or `openclaw` under a provider-named binding is still a defect even though no session marker detects that harness.

## Surfaces that conform

| Surface | Axis it carries | Notes |
|---|---|---|
| `-H/--harness` | harness | correct, do not rename |
| `-P/--provider` | provider | correct, do not rename |
| `-m/--model`, `--effort`, `--account` | model, effort, account | correct |
| `config.model_routing.providers` | provider | the genuine provider axis |
| `config.accounts.records` | account | |
| `config.agent_harnesses` | harness | |
| `config.agents.defaults.effort`, `config.agents.profiles.<verb>.effort` | effort | |
| `config.agents.defaults.provider`, `config.agents.profiles.<verb>.provider` | harness | the field name is parked by the value rule; the resolver receipt names the axis it feeds, not the field |
| `FNO_AGENT_HARNESS` | harness | injected at spawn, read for identity |
| spawn/register receipt `harness` | harness | |
| spawn/register receipt `provider` | provider | present only when a route was applied |
| spawn/register receipt `model` | model | effective model; an explicit `--model` wins over `--route` |
| `observed_model` | model | the sole answer to "what is this worker actually running" |

## Prior attempts

Four prior passes each fixed one surface, yet the conflation survived. They renamed `fno providers` to accounts and updated the managed-CLI config. They also changed the `fno whoami` line and on-disk registry field. The registry pass shows the failure clearly. It deleted the provider field and made harness the sole identity axis. That removed a real axis instead of disambiguating it. This cutover keeps the word `provider` and restores its correct meaning across the product surfaces above.
