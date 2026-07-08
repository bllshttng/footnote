# Domain Profiles

Domain profiles define what skill or command runs at each pipeline phase, enabling target to orchestrate non-code workflows (research, trading, marketing) with the same think → plan → execute → review → ship pattern.

## Schema

```yaml
# ~/.fno/config.toml (or .fno/config.toml)
domains:
  {domain-name}:
    phases:
      execute: {skill-name}     # default: fno:do waves
      review: {skill-name}      # default: fno:review
      validate: {bash-command}  # default: detected from project
      ship: {skill-name}        # default: fno:pr create
      external: {skill-name}    # default: fno:pr check
      docs: {skill-name}        # default: fno:ship-docs
    allow_claw: true|false      # default: true — set false to block autonomous mode
```

Only declare phases that differ from code defaults. Undeclared phases inherit the code default.

A phase value of `"none"` skips that phase entirely (sets gate to `skipped`).

## Code Defaults (implicit)

The `code` domain is never declared — it's the implicit fallback. These are its phase mappings:

| Phase | Default Skill/Command | Purpose |
|-------|----------------------|---------|
| execute | `fno:do waves` | Wave orchestration with TDD |
| review | `fno:review` | Code quality + integration tests |
| validate | *(project-detected)* | `npm run build` / `pytest` / etc. |
| ship | `fno:pr create` | Create GitHub PR |
| external | `fno:pr check` | External AI review (Gemini, etc.) |
| docs | `fno:ship-docs` | Architecture + how-to docs |

## Resolution Chain

Domain is resolved via a lookup chain (first non-empty wins):

```
--domain CLI flag  →  plan's domain: field  →  settings default_domain  →  "code"
```

### Phase Resolution (three levels)

Each phase resolves through its own chain:

```
plan-level phase override  →  domain profile phase  →  code default
```

This means a plan can override a single phase for its specific needs while inheriting the rest from its domain profile.

## Plan-Level Overrides

Plans can override individual phases in their `00-INDEX.md`:

```yaml
# 00-INDEX.md
domain: research
phases:
  ship: fno:publish-to-notion  # override just the ship phase
```

The plan's `phases:` section takes precedence over the domain profile. Undeclared phases fall through to the domain profile, then to code defaults.

## Example Profiles

### Research

```yaml
domains:
  research:
    phases:
      review: fno:fact-check
      validate: "python3 scripts/verify-citations.py"
      ship: fno:publish-to-obsidian
      external: none              # no external review for research
      docs: none                  # research IS the docs
```

### Trading

```yaml
domains:
  trading:
    phases:
      review: fno:risk-check
      validate: "python3 scripts/backtest.py"
      ship: fno:execute-order
      external: none
      docs: fno:trade-journal
    allow_claw: false             # NEVER run trading autonomously
```

### Marketing

```yaml
domains:
  marketing:
    phases:
      review: fno:brand-check
      validate: "python3 scripts/spell-check.py"
      ship: fno:publish-to-cms
      docs: none
```

## allow_claw

Controls whether autonomous (unattended) target runs can run for this domain:

- `true` (default): autonomous execution allowed
- `false`: unattended runs refuse to start, directing the user to interactive `/target` instead

Use `allow_claw: false` for domains with real-world consequences (trading, deployments) where human confirmation is critical.

## Model Fallback Chain

Configure automatic model fallback when the primary model hits rate limits or errors:

```yaml
# In config.toml config section:
config:
  model_fallback:
    chain:
      - claude-opus-4-6      # primary
      - claude-sonnet-4-6    # first fallback
      - claude-haiku-4-5     # emergency fallback
    retry_on:
      - rate_limit            # 429
      - overloaded            # 529
      - timeout               # connection timeout
    max_retries_per_model: 2  # tries per model before moving to next
    cooldown_seconds: 60      # wait before retry on rate limit
```

| Field | Default | Description |
|-------|---------|-------------|
| `chain` | `[claude-opus-4-6, claude-sonnet-4-6, claude-haiku-4-5]` | Ordered model preference |
| `retry_on` | `[rate_limit, overloaded, timeout]` | Error types that trigger retry/fallback |
| `max_retries_per_model` | `2` | Attempts per model before falling to next |
| `cooldown_seconds` | `60` | Wait before retrying on rate limit |

**Interactive (`/target`):** Presents the user with options (wait/switch/pause) via AskUserQuestion.

**Autonomous (unattended):** The external loop script (`scripts/run-target-loop.sh`) handles fallback automatically - detects errors, waits cooldown, retries with `--model` flag, moves through the chain.

## Shell API

Domain functions are available after sourcing `config.sh`:

```bash
source scripts/lib/config.sh

# Resolve domain from lookup chain
DOMAIN=$(resolve_domain "$FLAG" "$PLAN_DOMAIN" "$(get_config 'default_domain' '')")

# Get resolved phase skill/command
REVIEW_SKILL=$(get_domain_phase "$DOMAIN" "review")

# Check if domain allows autonomous mode
if ! domain_allows_claw "$DOMAIN"; then
    echo "Domain '$DOMAIN' blocks autonomous execution"
fi

# Check if domain is defined in settings
if ! domain_exists "$DOMAIN"; then
    echo "WARNING: unknown domain '$DOMAIN' — falling back to code defaults"
fi
```
