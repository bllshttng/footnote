# Axis vocabulary: harness, provider, model, effort, account

This document is the single source for footnote's axis definitions.
The code, the spawn/register receipts, the process environment, the config, and the skills all conform to the table below.
`scripts/ci/check-axis-vocabulary.sh` is the sole enforcer; it fails CI when an identifier, dict key, JSON key, or env var on one axis is assigned a literal from another.
When a reviewer and an agent disagree about an ambiguous site, this document resolves it the same way for both.

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

**Guard rule.** The guard flags only a harness value bound to a provider-named identifier.
`anthropic` under `provider` is correct; `claude` under `provider` is the defect.
A guard that flags the legal case fires on correct code, gets disabled, and the conflation regrows, so the predicate stays narrow.

**Name rule.** A directory named for one axis must not hold another axis's implementation.
This is the rule the first two cannot reach.
Both read the contents of files, and a path is only a traversal input.
So `cli/src/fno/agents/providers/` held harness adapters for a long time while `cli/src/fno/adapters/providers/` held real providers and accounts.
Nothing failed.
Two directories, one name, two axes: a reader who learns one cannot predict the other.
The harness package is now `cli/src/fno/agents/harnesses/`, and the harness docs are now `docs/harnesses/`.

## Effort: one axis, three harness spellings

The caller sets one value on the effort axis. Each harness spells the reasoning-effort parameter differently, and `thinking.type` and `reasoning.effort` are both effort, just spelled differently.

| Harness | Parameter the harness names | How fno emits it |
|---|---|---|
| claude | `thinking.type`, `output_config.effort` | `--effort <value>` |
| codex | `reasoning.effort` | `-c model_reasoning_effort=<value>` |
| opencode | (accepts the full value set) | no token emitted |

The code that holds this is `_EFFORT_ALLOWED` and `effort_tokens` in `cli/src/fno/agents/mux_spawn.py`. The per-harness value sets, verbatim from `_EFFORT_ALLOWED`, appear below.

| Harness | Allowed effort values |
|---|---|
| claude | `low`, `medium`, `high`, `xhigh`, `max` |
| codex | `minimal`, `low`, `medium`, `high`, `xhigh` |
| opencode | the full superset |

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

## Declared directory axes

Directory names are judged against `DECLARED_PATH_AXIS`, a map at the top of `scripts/ci/check-axis-vocabulary.sh`.
Each entry states the axis a directory's contents implement:

```python
DECLARED_PATH_AXIS = {
    "cli/src/fno/adapters/providers": "provider",
    "cli/src/fno/agents/harnesses": "harness",
    "docs/harnesses": "harness",
}
```

**To add a directory whose name contains `provider`, `harness`, or `model`:** add one line naming its repo-relative path and the axis its contents implement.
The check fails on two things.
An axis-named directory absent from the map fails as undeclared, because nothing vouches for the name it carries.
A declared directory whose declaration disagrees with the axis its own name states fails as a conflict.

**The entry is a human assertion, and the gate never verifies it.**
It compares your declaration against the directory's name and stops there.
It does not open the directory to check that the declaration is true, so a wrong entry passes green.
Review is the only thing that verifies a declaration.
That is why the value is spelled out, rather than the path merely being listed as permitted.

Classifying a directory by its contents was measured and rejected, not assumed unworkable.
The harness-adapter package held 298 harness literals to 5 vendor.
The correctly named provider package held 1024 to 49, because rotation and failover legitimately name the harnesses whose accounts they rotate.
Any threshold that flags the wrong directory also flags the right one.

Two further limits, both printed by the gate on every scan run so a green is not read as more than it is:

- **File names are not judged.** Fifty-two tracked files carry an axis word, and most are correct (`harness_map.py`, `model_routing.py`). A directory name is inherited by every import path beneath it, which is why one of them reached 82 files. A file name is local.
- **Symbol names are not judged.** `ProviderResult` and the `Provider*Error` classes are the content scan's subject and sit in the baseline.

**This rule governs package and directory names only. It does not rename the `provider` config field.**
That field stays exactly where the value rule puts it, with `route` and `account` added beside it and the allowlisted config sites parked.

## Ambiguous values

`opencode` is a legal harness and a legal provider.
`gemini` names a harness, a provider, and a model family.
The axis can never be inferred from a value, so no mechanical rename can be trusted: every site is read for intent.

These genuinely ambiguous values take an allowlist entry in `scripts/ci/axis-vocabulary-baseline.txt` rather than a blanket rule.
Each entry carries a one-line justification stating which two axes collide and why the site is correct.

## Allowlist justification procedure

When a site legitimately binds an ambiguous value and must be excluded from the guard:

1. Run `bash scripts/ci/check-axis-vocabulary.sh --write-baseline` and find the row the site produced in `scripts/ci/axis-vocabulary-baseline.txt`.
2. Copy that row, prefix it with `allowlist: `, and append ` | <one line>` stating which axes collide and why this binding is correct.
3. Delete the plain row you copied, so the site is held by the allowlist rather than by the ratchet.
4. Record a removal trigger for a time-boxed entry. The only such entry today is the `FNO_AGENT_PROVIDER` read-side compatibility window. Its trigger is the last in-flight worker that can carry the old variable.

The entry carries the whole finding, not just its path, and that is what scopes it.
Keying by path alone suppresses every axis violation in that file, including ones written later.
Keying by `path:line` stops suppressing the moment an edit above renumbers the site.
The recorded line is for a human chasing the justification.
The guard matches on the file and the binding.

An allowlist entry with no justification fails the guard, because a justification-free entry is exactly how a real violation would be smuggled past a reviewer.
An entry matching no current finding also fails, on a whole-repo scan.
A suppression whose site is gone is a trapdoor left open for the next binding that lands on the same name.

## Recognized and unrecognized harness values

Session-marker detection (`HARNESS_SESSION_MARKERS` in `cli/src/fno/harness_identity.py`) recognizes four harnesses today: `codex`, `claude`, `gemini`, `opencode`.
`agy` dispatches through its own adapter (`crates/fno-agents/src/agy_ask.rs`) but has no session marker.
`oh-my-pi` and `openclaw` are operator-named harnesses the code does not model at all yet; they appear in this table as legal vocabulary, not as values any code path recognizes.

The guard's harness-literal set is the union the operator declared, broader than the session-marker set, so a literal like `agy` or `openclaw` under a `provider` name is still a defect even though no marker detects the harness.
`oh-my-pi` and `openclaw` are excluded from the guard's harness-literal set only insofar as they would flag prose mentions; they are defects the instant they appear as a bound value where a provider is expected and the site is not allowlisted.

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

Four prior passes each fixed one surface and survived the conflation: renaming `fno providers` to accounts (superseded), the config managed-CLI entries (done), the `fno whoami` line (done), and the on-disk registry field (done).
The on-disk registry field pass is the instructive failure: it resolved the collision by deleting the provider field and declaring harness the sole identity axis, which removed the ability to express a real axis rather than disambiguating it.
This cutover instead keeps the word `provider` and restores its one correct meaning, and it leaves behind the guard as the artifact none of the four prior attempts did, which is why this one is expected to hold.
