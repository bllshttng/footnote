# Four-axis vocabulary: harness, provider, model, effort, account

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

## The two rules

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

## Ambiguous values

`opencode` is a legal harness and a legal provider.
`gemini` names a harness, a provider, and a model family.
The axis can never be inferred from a value, so no mechanical rename can be trusted: every site is read for intent.

These genuinely ambiguous values take an allowlist entry in `scripts/ci/axis-vocabulary-baseline.txt` rather than a blanket rule.
Each entry carries a one-line justification stating which two axes collide and why the site is correct.

## Allowlist justification procedure

When a site legitimately binds an ambiguous value and must be excluded from the guard:

1. Add one line to `scripts/ci/axis-vocabulary-baseline.txt` with the file, line, binding, literal, and the two axes involved.
2. Append a `# justification: <one line>` stating which axes collide and why this binding is correct.
3. Record a removal trigger if the entry is time-boxed (the only such entry today is the `FNO_AGENT_PROVIDER` read-side compatibility window, which expires when no in-flight worker can carry the old variable).

An allowlist entry with no justification fails the guard, because a justification-free entry is exactly how a real violation would be smuggled past a reviewer.

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
| `FNO_AGENT_HARNESS` | harness | injected at spawn, read for identity |
| spawn/register receipt `harness` | harness | |
| spawn/register receipt `provider` | provider | present only when a route was applied |
| `observed_model` | model | the sole answer to "what is this worker actually running" |

## Prior attempts

Four prior passes each fixed one surface and survived the conflation: x-4704 (`fno providers` to accounts, superseded), x-2599 (config managed-CLI entries, done), x-2966 (the `fno whoami` line, done), x-880e (the on-disk registry field, done).
x-880e is the instructive failure: it resolved the collision by deleting the provider field and declaring harness the sole identity axis, which removed the ability to express a real axis rather than disambiguating it.
This cutover instead keeps the word `provider` and restores its one correct meaning, and it leaves behind the guard as the artifact none of the four prior attempts did, which is why this one is expected to hold.
