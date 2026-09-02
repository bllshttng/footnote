# Cross-model (provider-rotated) review

A review written by the model family that wrote the code shares the implementer's blind spots. Cross-model review is the answer: route the review to a **different provider** (codex on a claude diff, claude on a codex diff) so a second model family reads the change. The panel that once embedded this as per-agent routing is retired; the live surfaces are below.

## The live surfaces

| Surface | What it does | Switch |
|---|---|---|
| `/fno:review peer` | one cross-model second opinion on a diff, with `--attest` or `--post` | needs a second provider in `config.accounts` |
| `config.review.peers` | required local cross-model reviewers gating the ship gate (see the symmetric guard below) | the peers list itself |
| `config.review.cross_model.enabled` | counts available other-provider kinds as review-assurance diversity when the review lane computes its assurance reading | opt-in, default off |

The retired panel's per-agent routing key `config.review.agent_routes` refuses with the replacement named: the review-posture ladder (`config.review.posture`) replaced per-agent review routing. The panel-era routing keys `config.review.agent_harnesses` / `config.review.agent_providers` no longer drive anything; the peer gate below is the cross-model enforcement surface. A configured `sigma` name refuses everywhere and names the owned lane. The specialist agents that once made up the panel remain individually invocable as agents.

## The `config.review.peers` gate: symmetric same-model guard

`config.review.peers` is a different mechanism from the panel above: it names local review harnesses (`codex` / `gemini` / a routed `claude`) and enforces one trust invariant: **at least one required reviewer runs a genuinely different model than the author.**
Identity-free entries form one composite gate satisfied by an explicit clean `peer` attestation pinned to current HEAD.
An entry with its own `identity`, or a review block with shared `peer_identity`, retains the legacy GitHub-posting carrier and requires that login to have reviewed.

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

- **Gate time (loop-check):** identity-free peers resolve to one `peer` reviewer key when any configured option is cross-model, or to an unmatchable local sentinel when every option is same-model.
  Identity-backed peers retain their required-login set; a login backed only by same-model peers is replaced by the existing login sentinel.
- **Unknown authorship:** when no harness marker resolves, the same-model guard is inert.
  Identity-backed logins remain unchanged, and identity-free peers still require the composite `peer` attestation.
- **Load time (`config/__init__.py`):** rejects a bare or anthropic-routed `claude` peer. This is the fail-EARLY layer for a claude author only; the config file is harness-agnostic, so load time cannot know a codex/gemini author. A codex-authored repo that wants a claude-model peer is a known inverse gap, deferred until a real deployment needs it.
- **Dispatch time (`/review peer`):** refuses a bare peer whose provider matches the invoking harness at RESOLVE, the earliest advisory layer.

**Routed-transport limitation.** A claude-authored session routed to a different model (GLM via z.ai) still reads as anthropic-family here, because the harness is the model proxy. A bare claude peer on such a run would be discounted even though the real author model is GLM - the conservative direction (the gate HOLDS, it never wrongly clears). If a reliable ambient marker for the routed model appears, this can tighten later.

**Symmetric repository pattern.** A repository can list `codex` plus `gemini` without identities.
For a Gemini author, Codex is eligible; for a Codex author, Gemini is eligible; for a Claude author, both are eligible.
Doctor reports the same-model option without letting it veto the eligible one, while loop-check requires one clean composite attestation.
