# Cross-model (provider-rotated) review panel

The internal sigma-review panel (`/review sigma`, backed by `cli/src/fno/review/`) runs six agents over a diff. By default every agent runs on **claude** - the same model family that usually wrote the code, so it shares the implementer's blind spots. Cross-model review lets individual agents run on a **different provider** (codex / gemini) to catch model-specific blind spots, as a second opinion baked into the in-house panel rather than a separate `/review peer` pass.

## Opt-in

Cross-model is OFF by default. The existing all-claude review is byte-for-byte unchanged until you turn it on. Two signals engage it, and either one is sufficient:

1. `config.review.cross_model.enabled: true` is the no-map-needed switch. It applies the curated default routing (the three correctness agents go cross-model, the rest stay on claude).
2. `config.review.agent_providers` is an explicit per-agent map. Setting it also turns cross-model on, and your routing wins over the curated default.

The panel has exactly six agents, and each is a valid key in `agent_providers`. Each value is one of `claude`, `codex`, `gemini`, or the sentinel `alternate` (a provider that differs from whoever wrote the code; see "Per-agent routing"). An agent you omit from the map stays on `claude`.

```yaml
config:
  review:
    cross_model:
      enabled: true            # optional: on its own this applies the curated default below
    agent_providers:           # optional: an explicit map (also turns cross-model on)
      # correctness reviewers - cross-model these to catch shared blind spots
      code_reviewer: alternate
      silent_failure_hunter: alternate
      type_design_analyzer: alternate
      # the rest - keep on claude (UI agents need Claude's browser tooling)
      integration_test_analyzer: claude
      ux_flow_tester: claude
      multi_device_checker: claude
```

The map above is the curated default written out in full, so you can copy it and change any single line (for example `ux_flow_tester: gemini`). When neither signal is set, the panel builds today's single all-claude runner and the legacy `::finding::` path runs unchanged (no JSON contract, no provider attribution, no cache-key change).

## Per-agent routing

`config.review.agent_providers` maps an agent name to a provider. The value is one of `claude`, `codex`, `gemini`, or the sentinel `alternate`.

- **Unset map** applies the curated default: the three correctness-focused agents (`code_reviewer`, `silent_failure_hunter`, `type_design_analyzer`) resolve to `alternate`; the other three stay on `claude`. The curated default is computed in the resolver, not baked into the config schema, so an empty map stays a faithful empty map.
- **A set map** wins per agent. An agent the map does not name falls back to `claude` (NOT the curated default).
- A **literal** provider (`codex`, `gemini`, `claude`) pins that agent unconditionally.
- `alternate` resolves at runtime to a provider that **differs from the implementer's** (read from the ledger `provider_id` for this session; absent assumes claude). It walks the `config.providers` rotation order, skips locked-out providers, and picks the first kind that is not the implementer's. The implementer's own provider is always excluded so "cross-model" genuinely means a different model.

UI agents (`ux_flow_tester`, `multi_device_checker`) stay on claude by default because codex/gemini agents cannot use Claude's browser tooling. Pinning them off-claude is allowed but loses those checks; the report flags it.

## Graceful degradation

Cross-model is never a hard error. When no differing or available provider exists (single-provider setup, or every alternate locked out), the agent runs on claude and the report says `cross-model unavailable: ran on claude` rather than silently appearing cross-modeled. An unknown provider literal (a typo like `grok`) warns and degrades to claude the same way.

A provider lockout discovered at dispatch time (the pinned provider is rate-limited) is a retryable failure: the runner falls through to claude for that one agent, the agent's findings still appear on the fallback, and no other agent is affected.

## Dispatch + the JSON findings contract

- **claude** agents run through the existing `claude_runner` (`claude -p`, bg short-id + poll).
- **codex / gemini** agents run through `agents_spawn_runner`, which dispatches a one-shot `fno agents spawn --provider <p> --once` and reads the model's reply text directly (codex/gemini one-shot returns the reply synchronously, not a short-id).

Both runners converge on one strict-JSON findings parser (`findings_parser.parse_findings_json`) so the confidence scorer and report builder stay provider-agnostic. Every agent prompt gets a JSON-contract addendum appended **at dispatch time** demanding a single JSON array of `{severity, message, file?, line?}` objects and forbidding interactive/clarifying questions (the agents run headless). The six bundled prompt files in `review/prompts/` are never modified, which is what keeps the cross-model-OFF path byte-for-byte unchanged: the JSON contract and the claude_runner JSON switch both activate only behind the opt-in gate.

A reply that is not a valid JSON array is a soft per-agent failure: it is recorded, shown in the report as `agent errored (unparseable findings)`, and never aborts the panel. A parse failure is terminal (the provider answered, just not in contract); a dispatch/lockout/timeout failure is retryable and falls through to claude.

## Report attribution

Each agent's line in the review artifact carries an inline provider/model tag (`code_reviewer [codex/gpt-...]`), the degradation note when it fell back to claude, and `agent errored (unparseable findings)` for a soft-fail instead of dropping the agent. A cross-model run also adds a one-line cost note naming which providers were billed, because a cross-model panel spends a second provider's quota per review.

## Cache

The review result cache (`review/cache.py`) gains a provider dimension folded into the cache key and the cached body, so a cross-model run never collides with an all-claude entry for the same SHA. The dimension is the **per-agent routing** (each agent paired with its resolved provider), not just the set of providers, so two configs that send the same providers to different agents stay distinct. The write is keyed by the routing that **actually** ran: if a pinned provider was locked out and an agent fell back to claude, that run caches under the claude routing, so a later run that requests the now-recovered provider misses the cache and re-runs instead of being served the fallback. An empty/absent dimension reproduces the pre-cross-model key exactly, so existing all-claude cache entries still hit.

## Provider substrate reuse

Resolution reads the existing `config.providers` records + per-provider lockout state through `adapters/providers` (`loader.load_providers`, `runtime_state.is_in_cooldown`). It never builds a parallel provider list, so review and execution agree on what is available. When `config.providers` is unconfigured, only claude is available and `alternate` degrades cleanly.

## The `config.review.peers` gate: symmetric same-model guard

`config.review.peers` is a different mechanism from the panel above: it names harness peers (`codex` / `gemini` / a routed `claude`) that post a real PR review under `config.review.peer_identity`, and loop-check requires that login to have reviewed before the gate clears. Its whole point is one trust invariant: **at least one required reviewer runs a genuinely different model than the author.** A same-model peer reviewing its own work is exactly what the gate exists to prevent.

The author's model is a proxy for its invoking harness, resolved from the ambient env markers in the shared precedence `CODEX_THREAD_ID` > `CLAUDE_CODE_SESSION_ID` > `CODEX_SESSION_ID` > `GEMINI_SESSION_ID` (`harness_identity.py`, mirrored in `claims.rs`). A peer's effective family comes from its route provider when it names one, else its bare provider:

| Input | Family |
|---|---|
| harness/provider `claude` (no route) | anthropic |
| harness/provider `codex` | openai |
| harness/provider `gemini` | google |
| **claude** peer with a valid route `"route_provider,route_model"` | `route_provider`'s family (e.g. `zai,glm-5.2` -> zai, which is no known author family) |
| codex/gemini peer with a `model` route | the bare provider's family - the route is IGNORED (only the claude transport executes a route; codex/gemini dispatch runs the bare provider) |
| unknown provider | none (never matches any author family) |

A peer whose effective family equals the author's family is a **same-model peer** for that run. (The route provider `zai` maps to no known author family, so a `zai`-routed claude peer is genuinely cross-model.)

Enforcement is layered, and the gate is the point of record because that is where the invariant is spent:

- **Gate time (loop-check, `resolved_required_bots_for_author`):** for each required login contributed by `peers`, if every peer backing that login is same-model, the login is replaced with a distinct unsatisfiable sentinel and one loud line names the peer, the authoring harness, and the fix. A login backed by at least one cross-model peer (different family, or an unknown provider) is left alone, so `peers: [codex, gemini]` sharing one identity stays satisfiable on a codex-authored run (gemini backs the login) while `peers: [codex]` alone does not. The guard only ever REPLACES a peer login with the sentinel - it never removes a login, so the gate can only get stricter, never looser, and a login also required by `github_apps` is exempt.
- **Fail open on unknown authorship:** when no harness marker resolves (a manual CLI invocation, an unknown harness), the guard is inert and the required-login set is byte-identical to before - the load-time claude check remains the floor for the dominant author.
- **Load time (`config/__init__.py`):** rejects a bare or anthropic-routed `claude` peer. This is the fail-EARLY layer for a claude author only; the config file is harness-agnostic, so load time cannot know a codex/gemini author. A codex-authored repo that wants a claude-model peer is a known inverse gap, deferred until a real deployment needs it.
- **Dispatch time (`/review peer`):** refuses a bare peer whose provider matches the invoking harness at RESOLVE, the earliest advisory layer.

**Routed-transport limitation.** A claude-authored session routed to a different model (GLM via z.ai) still reads as anthropic-family here, because the harness is the model proxy. A bare claude peer on such a run would be discounted even though the real author model is GLM - the conservative direction (the gate HOLDS, it never wrongly clears). If a reliable ambient marker for the routed model appears, this can tighten later.

**Sanctioned per-run pattern.** Because the config is harness-agnostic, an author that knows its own harness picks the cross-model peer per run via a per-worktree config override: a claude worktree sets `peers: [codex]`, a codex worktree sets `peers: [gemini]`. The guard makes a wrong choice fail loudly instead of clearing silently; this pattern makes the right choice explicit per author.
