# Provider Rotation Substrate

Reference doc for `fno providers` (Spec 1 of 4 in the provider rotation plan).

This substrate manages provider records, credential staging, and dispatch-env
construction. It does NOT automate rotation, failover, or mid-session swapping.
See [What this substrate does NOT do](#what-this-substrate-does-not-do).

---

## Concepts

**Provider record** - A named configuration entry in `settings.yaml` describing
one CLI account: which binary to use (`claude`, `gemini`, `codex`, etc.), how
to authenticate (`oauth_dir` or `api_key`), and where credentials live on disk.

**Account** - The human-meaningful label for a subscription or API key
(`account_id`). Defaults to the provider `id` when not set.

**Staged provider** - A provider whose credentials have been materialised into
`~/.fno/providers/<id>/` as a directory or symlink. Staging is required
before `dispatch_env()` returns a usable env dict.

**Dispatch env** - A dict of environment variables (`{"CLAUDE_CONFIG_DIR": ...}`
or `{"HOME": ...}`) that, when merged into a subprocess's env, points the CLI
at the correct credentials directory.

---

## Schema reference

Provider records live under `config.providers` in `settings.yaml`.

```yaml
config:
  providers:
    active: claude-max-secondary     # id of the active provider (optional)
    records:
      - id: claude-max-secondary
        name: "Secondary Claude Max"
        cli: claude
        auth: oauth_dir
        credentials_source: /Users/me/.claude.secondary
        priority: 100
        account_id: account-secondary
        tags: [secondary, max]
        description: "Personal secondary subscription"
```

### Field reference

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `id` | string | yes | - | Unique identifier. Pattern: `[a-z][a-z0-9-]{0,63}`. |
| `name` | string | yes | defaults to `id` | Human-readable label. |
| `cli` | enum | yes | - | `claude` \| `gemini` \| `codex` \| `openclaw` \| `hermes` |
| `auth` | enum | yes | - | `oauth_dir` \| `api_key` |
| `priority` | integer | no | `100` | Lower = higher priority (reserved for future auto-selection). |
| `credentials_source` | path | conditional | - | Required when `auth: oauth_dir`. Absolute path to the credentials directory. |
| `env` | dict | conditional | - | Required when `auth: api_key`. Must contain at least one recognised key (`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENAI_API_KEY`). Values support `${...}` references. |
| `account_id` | string | no | `id` | Account label written to ledger entries for cost attribution. |
| `tags` | list[string] | no | `[]` | Arbitrary tags (reserved for future routing). |
| `description` | string | no | - | Free-text note. |

### Agent-to-agent switchboard (`config.agents.a2a`)

The session-to-session switchboard lets one held stream-json
thread drive another: `fno mail send A->B` writes a turn into B and, by
default, relays B's reply back into A as a literal user turn (true
agent-to-agent), alternating until a turn ceiling stops it. These settings live
under `config.agents.a2a`:

```yaml
config:
  agents:
    a2a:
      auto: true          # A2A relay toggle (default true)
      turn_ceiling: 6     # hard cap on total A<->B turns per exchange (>= 1)
```

| Field | Type | Default | Description |
|---|---|---|---|
| `auto` | bool | `true` | When true, B's reply relays back into A (and A's back into B, …) as literal user turns — autonomous agent-to-agent. When false, **observed mode**: the turn is delivered to B and B's reply is surfaced, but nothing relays back (no autonomous exchange). |
| `turn_ceiling` | int | `6` | Hard upper bound on total turns in one A<->B exchange. A **correctness** bound, not a preference: an unbounded relay burns plan credit forever, so the ceiling applies **regardless of `auto`** and must be `>= 1`. The exchange stops with a visible "loop ceiling reached" when it is hit. |

Both keys are read by `fno mail send`'s switchboard fast lane; a malformed or
absent block falls back to the defaults above. Turning `auto` off is the
conservative posture for unattended fleets — turns still deliver and are
observable via `fno agents watch`, but no autonomous back-and-forth runs.

> Headless permission posture (how the daemon answers a `can_use_tool` request
> inside an adopted thread with no human present) is a tracked follow-up under
> the same `config.agents.a2a.*` namespace; it is not yet a configurable key. The
> standing default until it lands is conservative: never auto-approve a tool whose
> effect reaches outside the session's cwd.

### Scope

Settings are read from two locations. Project-local wins over global:

1. `.fno/settings.yaml` (project, committed or gitignored)
2. `~/.fno/settings.yaml` (global, user-wide)

`fno providers add --scope project` writes to the project file.
`fno providers add --scope global` writes to the global file.
`fno providers use` defaults to `--scope project`.

---

## Auth strategies

### oauth_dir

Use for `claude`, `gemini`, and other CLIs whose credentials are stored as
files on disk (OAuth tokens, session cookies).

```yaml
auth: oauth_dir
credentials_source: /Users/me/.claude.secondary
```

Staging creates a symlink:

```
~/.fno/providers/<id>/.claude  ->  credentials_source   (for claude)
~/.fno/providers/<id>/home/.gemini  ->  credentials_source  (for gemini)
```

`dispatch_env()` returns `{"CLAUDE_CONFIG_DIR": "~/.fno/providers/<id>/.claude"}` for `claude`,
or `{"HOME": "~/.fno/providers/<id>/home"}` for other CLIs.

### api_key

Use for CLIs that read credentials from environment variables.

```yaml
auth: api_key
env:
  ANTHROPIC_API_KEY: "${KEYCHAIN:my-anthropic-key}"
```

Staging creates an empty marker directory (`~/.fno/providers/<id>/`);
no symlinks. `dispatch_env()` resolves all `${...}` references and returns the
resolved dict.

---

## env-value reference resolution

Values in `env:` support four syntaxes:

**`${ENV:VAR_NAME}`** - Reads `VAR_NAME` from the current process environment.
Raises `ProviderUnavailableError` if the variable is not set.

```yaml
ANTHROPIC_API_KEY: "${ENV:MY_ANTHROPIC_KEY}"
```

**`${KEYCHAIN:item}`** - Reads the password from macOS Keychain via
`security find-generic-password -w -s <item>`. Raises `ProviderUnavailableError`
if the item does not exist. macOS only.

```yaml
ANTHROPIC_API_KEY: "${KEYCHAIN:anthropic-work-account}"
```

**`${FILE:/path/to/file}`** - Reads the first line of the file, stripped of
whitespace. Raises `ProviderUnavailableError` if the file cannot be read.

```yaml
ANTHROPIC_API_KEY: "${FILE:/run/secrets/anthropic_key}"
```

**`${literal_value}`** - Any `${...}` value that contains no `:` character is
returned verbatim as `literal_value`. This is an escape mechanism for values
that start with `${` but are not references.

Plain strings (no `${` prefix) pass through unchanged.

---

## Filesystem layout

```
~/.fno/providers/
  claude-max-secondary/
    .claude -> /Users/me/.claude.secondary   # symlink (claude + oauth_dir)
  gemini-work/
    home/
      .gemini -> /Users/me/.gemini.work      # symlink (gemini + oauth_dir)
  openai-api/
    (empty marker dir - api_key auth, no symlink)
```

For `oauth_dir` auth, the symlink target is `credentials_source`. The symlink
is created by `staging.stage(record)` and verified by `staging.verify_staged(record)`.

---

## dispatch_env() contract

```python
from fno.adapters.providers.dispatch import dispatch_env
from pathlib import Path

env = dispatch_env(
    provider_id="claude-max-secondary",
    repo_root=Path("/path/to/project"),   # optional; defaults to os.getcwd()
    root=Path.home() / ".fno" / "providers",  # optional; override for tests
)
# Returns: {"CLAUDE_CONFIG_DIR": "/Users/me/.fno/providers/claude-max-secondary/.claude"}
```

**Input:** `provider_id` (string), optional `repo_root` (Path), optional `root` (Path).

**Output:** A dict of environment variables to merge into the subprocess env before
invoking the CLI binary. The dict is minimal: only the keys strictly required
for credential isolation.

**Isolation guarantee:** `dispatch_env()` is a pure function. It reads
`settings.yaml` and the filesystem but holds no module-level state and
acquires no locks. Safe to call concurrently from a `ThreadPoolExecutor` or
`asyncio` without additional synchronisation.

**Failure modes:**

- `ProviderNotFoundError` (subclass of `KeyError`) - `provider_id` not present
  in `config.providers.records`. The record was never configured.
- `ProviderUnavailableError` (subclass of `RuntimeError`) - the record exists
  but cannot be used right now. For `oauth_dir`: provider is not staged (call
  `staging.stage(record)` first). For `api_key`: an env reference cannot be
  resolved (missing env var, keychain item, or file).

The distinction matters for callers: `ProviderNotFoundError` is a configuration
error (stop, ask user to run `fno providers add`); `ProviderUnavailableError` is
a transient error (staging might fix it).

---

## Migration from cc-switch

The `cc-switch` tool swaps the active account by modifying which OAuth session
Claude Code reads at session start. `fno providers` replaces that step with
explicit staging + `CLAUDE_CONFIG_DIR` isolation.

### Recipe 1: Swap accounts before the next session (manual)

```bash
# One-time: register the secondary account
fno providers add claude-max-secondary \
    --cli claude --auth oauth_dir \
    --credentials-source ~/.claude.secondary \
    --scope global

# Stage it (creates the symlink under ~/.fno/providers/)
python3 -c "
from fno.adapters.providers.loader import load_providers
from fno.adapters.providers.staging import stage
cfg = load_providers()
stage(cfg.by_id['claude-max-secondary'])
"

# Activate it for the next session
fno providers use claude-max-secondary --scope global
```

### Recipe 2: Set up a secondary account from a backup credentials directory

If you keep a credentials backup (e.g., you copied `~/.claude/` to `~/.claude.backup`):

```bash
# Register pointing at the backup dir
fno providers add claude-backup \
    --cli claude --auth oauth_dir \
    --credentials-source ~/.claude.backup \
    --scope global \
    --account-id my-backup-account

# Stage (creates symlink)
python3 -c "
from fno.adapters.providers.loader import load_providers
from fno.adapters.providers.staging import stage
cfg = load_providers()
stage(cfg.by_id['claude-backup'])
"

# Verify staging is intact
python3 -c "
from fno.adapters.providers.loader import load_providers
from fno.adapters.providers.staging import verify_staged
cfg = load_providers()
rec = cfg.by_id['claude-backup']
print('staged:', verify_staged(rec))
"
```

---

## What this substrate does NOT do

This is Spec 1 of 4. Specs 2-4 extend the substrate with automation:

- **Reactive failover (Spec 2, planned):** no automatic switching when a provider
  hits a rate limit or returns an error. You must run `fno providers use` manually.
- **Per-agent sigma-review routing (Spec 3):** sigma-review subagents can be
  routed to a different coding model (`codex` / `gemini`). The shipped path is
  `config.review.cross_model` / `config.review.agent_providers`, resolved by the
  same `provider_resolution` code both `fno review` and `/review sigma`
  (via `fno review --print-providers`) dispatch through. The Spec-3 design below
  named a `config.agents.<name>.provider` key that was never wired - use the
  `config.review.*` keys instead.
- **Per-phase pinning + proactive round-robin (Spec 4, planned):** no automatic
  rotation across providers between phases. All phases in a session use the same
  active provider.
- **Error detection:** the substrate does not monitor for 429s, auth failures, or
  quota exhaustion. Detection lives in Spec 2.
- **Mid-session swap:** `dispatch_env()` reads from disk at call time; swapping the
  active provider mid-session (between phases) has no effect on already-dispatched
  processes.

---

## Troubleshooting

**`fno`: command not found**

`fno` is installed as a script by the `footnote` package. Run via
`uv run fno ...` from the `cli/` directory, or install the package into your
virtualenv with `uv pip install -e cli/`.

**OAuth refresh failing through the symlink**

If `claude` refreshes its OAuth token, it writes the new token to the resolved
path of the symlink target (`credentials_source`), not to
`~/.fno/providers/<id>/.claude/`. This is correct behaviour: the
symlink is transparent to the CLI binary. If token refresh fails, verify
that `credentials_source` is writable and that the symlink has not been
accidentally replaced with a regular directory.

**`${KEYCHAIN:item}` not resolving**

Run `security find-generic-password -w -s <item>` directly to verify the item
exists. Keychain access is macOS-only; `${ENV:...}` or `${FILE:...}` are
portable alternatives. If the item exists but `dispatch_env()` still raises
`ProviderUnavailableError`, check that the calling process has Keychain access
(interactive sessions have it automatically; headless scripts may not).

**settings.yaml validation failure on `fno providers add`**

The `add` command validates the record via Pydantic before writing. Common
causes:

- `id` contains uppercase or spaces. Pattern: `[a-z][a-z0-9-]{0,63}`.
- `auth: oauth_dir` without `--credentials-source`.
- `auth: api_key` with `--env` values that contain no recognised API key name
  (`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENAI_API_KEY`).

Run `fno providers show <id>` after adding to verify the stored record matches
intent.

**dispatch_env returns wrong env for the `claude` CLI kind**

For `auth: oauth_dir` + `cli: claude`, `dispatch_env()` returns
`{"CLAUDE_CONFIG_DIR": "..."}`. If the returned dict contains `HOME` instead,
the record was added with a non-`claude` CLI value. Remove and re-add with
`--cli claude`.

For `auth: api_key`, `dispatch_env()` returns the resolved `env` dict directly.
If `CLAUDE_CONFIG_DIR` is expected but absent, the record is using `oauth_dir`
auth but is not staged. Call `staging.stage(record)` first.

---

## Failover (Spec 2)

Phase 02-03 of the rotation initiative ships **reactive failover**: when
a provider call returns a swap-trigger error, the rotation queue advances
to the next eligible provider, the in-flight subprocess inherits the new
active provider at its next spawn, and the per-turn provider stamp lets
downstream tooling reconstruct what ran where.

### Error Taxonomy

`cli/src/fno/adapters/providers/error_taxonomy.py` normalizes the
outcome of a provider call (HTTP status + body, or CLI subprocess exit
code + stderr) to a closed taxonomy. Only the first three classes
trigger a swap.

| Error class | Triggers swap | Example |
|---|---|---|
| `provider_5xx` | YES | HTTP 529 (overloaded), 500/502/503/504 |
| `provider_4xx_auth` | YES | HTTP 401, 403 - creds bad on this provider |
| `provider_4xx_quota` | YES | HTTP 402, 429; HTTP 200 with body containing "rate limit" or "quota exceeded" |
| `parser_error` | NO | HTTP 200 with unparseable body (OpenRouter envelope mismatch, HTML error page through 200) |
| `unknown` | NO | Anything else - surface to caller |

### Swap Rules

- **Storm-cap.** At most `config.providers.failover.max_swaps_per_phase`
  swaps per phase, default 5. Beyond this, the controller writes
  `blocked_reason: stuck:failover_thrash` to `target-state.md` and the
  typed-blocker stop hook trips BLOCKED.
- **No swap-back within phase.** Once swapped from A to B, A is
  ineligible for the rest of the phase. Cheap v0 hysteresis without a
  multi-success-counter health-check loop. Resets at phase boundaries.
- **End of queue.** When the queue is exhausted with the no-swap-back
  rule applied, the controller returns `QUEUE_EXHAUSTED`. The upstream
  loop layer's mode-aware branching (attended -> BLOCKED with
  `reason: all_providers_exhausted`; unattended -> sleep 5 min, restart)
  handles the action.

### Per-provider Cost Sub-cap

In addition to the existing session cap at
`config.budget.{attended|unattended}.cost_cap_usd`, each provider record
can declare its own per-session ceiling. When the per-provider spend
exceeds the sub-cap, the stop hook trips BLOCKED with axis tagged as
`per_provider`:

```yaml
config:
  providers:
    records:
      - id: claude-anthropic
        name: Claude Anthropic
        cli: claude
        auth: oauth_dir
        credentials_source: ~/.claude
        cost_cap_usd_per_session: 30
      - id: claude-openrouter
        name: Claude OpenRouter
        cli: claude
        auth: api_key
        env: {ANTHROPIC_API_KEY: "${KEYCHAIN:openrouter-key}"}
        cost_cap_usd_per_session: 30
    failover:
      max_swaps_per_phase: 5
```

The session cap and per-provider sub-cap are checked together; whichever
trips first wins. v0 attribution math is approximate
(`total_session_cost × turns_on_provider / total_turns`) which is enough
to bound damage. Exact per-segment math (rate card x tokens per
segment) is deferred to Spec 2.5.

### Subprocess Provider Stickiness

`spawn_with_provider_snapshot()` in
`cli/src/fno/adapters/providers/dispatch.py` reads the active
provider once under `fcntl.LOCK_SH` immediately before the spawn and
injects five env vars into the subprocess:

| Env var | Source |
|---|---|
| `FNO_PROVIDER_ID` | snapshot.id |
| `FNO_PROVIDER_AUTH` | snapshot.auth |
| `FNO_PROVIDER_CRED_REF` | snapshot.credential_ref (when set) |
| `FNO_PROVIDER_BASE_URL` | snapshot.base_url (when set) |
| `FNO_PROVIDER_PRICING` | JSON-serialized snapshot.pricing (when set) |

The subprocess and its descendants see the snapshotted provider for
their full lifetime, even if the parent flips `active` immediately
after spawn returns. This prevents the auth-mismatch cascade where a
goal-verifier subagent spans a swap and hits 401 on stale creds.

### Per-turn Attribution Sidecar

`cli/src/fno/turn_attribution.py` owns
`.fno/turn-attribution.jsonl`. Each line records one assistant
turn with the active provider and any normalized error class:

```jsonl
{"turn_index":0,"ts":"2026-05-05T01:23:45Z","provider_id":"claude-anthropic","error_class":null}
{"turn_index":1,"ts":"2026-05-05T01:24:48Z","provider_id":"claude-openrouter","error_class":"provider_5xx"}
```

Writes serialize via `fcntl.LOCK_EX` so concurrent dispatchers can't
tear the JSONL. Writes are non-blocking on failure: a turn that can't
write the stamp continues and the sidecar's missing entries fall back
to active-at-compute attribution at downstream read time.

The sidecar feeds:

- `fno.cost.compute_per_provider_cost` - per-provider rollup for
  the sub-cap detector
- `fno.cost.compute_per_turn_attribution` - generic per-provider
  turn count
- The stop hook's per-turn provider summary (logged at completion)
- `fno providers list` per-session per-provider spend (deferred to 2.5)

### State Files Owned by Spec 2

| File | Owner | Reset on |
|---|---|---|
| `.fno/turn-attribution.jsonl` | dispatch layer | session boundary (gitignored, re-created per session) |
| `.fno/failover-state.json` | failover controller | phase boundary (detected via phase_id mismatch) |
| `~/.fno/.settings.lock` | atomic_mutate_settings + read_active_provider_atomic | n/a (lock file, content irrelevant) |

---

## Per-agent routing (Spec 3)

> **Shipped path:** the wired cross-model routing for `/review sigma` and
> `fno review` uses `config.review.cross_model` / `config.review.agent_providers`
> (see `skills/review/sigma.md` -> "Cross-Model Review Routing"), resolved by
> `cli/src/fno/review/provider_resolution.py`. The `config.agents.<name>.provider`
> schema described in the rest of this section is the original Spec-3 design and
> was never wired; prefer the `config.review.*` keys.

Spec 3 lets each sigma-review subagent run on a
different provider, so model blind-spots cancel across reviews. The routing config
is optional and fully back-compatible: agents without a pinned provider fall back to
the global active provider.

### Schema

```yaml
config:
  providers:
    active: claude-anthropic
    records:
      - id: claude-anthropic
        name: Claude Anthropic
        cli: claude
        auth: oauth_dir
        credentials_source: ~/.claude
      - id: gemini-pro-1
        name: Gemini Pro 1
        cli: gemini
        auth: api_key
        env: { GEMINI_API_KEY: $GEMINI_KEY }
      - id: glm-zhipu
        name: GLM Zhipu
        cli: openclaw
        auth: api_key
        env: { OPENAI_API_KEY: $GLM_KEY }
  agents:
    code-reviewer:
      provider: claude-anthropic
    silent-failure-hunter:
      provider: gemini-pro-1
    type-design-analyzer:
      provider: glm-zhipu
```

Agent names under `config.agents.<name>` must exactly match the `subagent_type`
strings passed to `Task()` (case-sensitive).

### Dispatch flow

```
sigma-review SKILL
       |
       v
+------------------------------+
| resolve_agent_provider(name) |
|   -> provider_id             |
|   -> cli                     |
+------------------------------+
       |
       v
+------------------------------+    spawn event
| dispatch_sigma_subagent(...) | -----------------> .fno/events.jsonl
|   __enter__ emits spawn      |              |
+------------------------------+              |
       |                                      |
       v (cli == claude)                      |
+------------------------------+              |
| Caller invokes Task tool     |              |
| dispatch.record_complete(...)| 	          |
+------------------------------+              |
       |                                      |
       v (cli != claude)                      |
+------------------------------+    subprocess output
| subprocess via spawn_with_   | -----------------> .fno/sigma-review/{sid}/{agent}.out
| provider_snapshot, .wait()   |
+------------------------------+
       |
       v
+------------------------------+    complete event
| __exit__ emits complete      | -----------------> .fno/events.jsonl
+------------------------------+              |
                                              v
                              +----------------------------+
                              | verify_provenance at       |
                              | <promise> time:            |
                              | - Claude: transcript path  |
                              | - non-Claude: event path   |
                              +----------------------------+
```

### Failure-mode taxonomy

The stop hook's `verify_event_evidence` path diagnoses five failure modes when
at least one resolved agent is non-Claude:

| Diagnostic | User-visible failure | When |
|---|---|---|
| `subagent_spawn_missing:<agent>` | "Review claimed to dispatch X but no spawn event recorded" | Caller forgot to wrap Task() in `dispatch_sigma_subagent` |
| `subagent_complete_missing:<agent>` | "Subagent dispatched but never completed" | Subprocess crashed before complete event emitted (rare; structurally prevented by AC5-FR) |
| `subagent_pair_count_mismatch:<agent>:expected=N:got=M` | "Agent listed N times in agents_dispatched but only M spawn/complete pairs found" | Same agent dispatched twice; one call skipped the dispatcher wrap |
| `agent_mismatch:<agent_name>` | "Spawn event found for agent not declared in agents_dispatched" | Forged review artifact or caller dispatched an extra subagent without listing it |
| `subagent_orchestrator_skipped:<agent>` | "Claude dispatch entered but orchestrator never called record_complete" | Bug in the calling skill - review the SKILL.md prose contract |

### Migration

Existing `settings.yaml` files are unaffected. The `config.agents` block is optional;
when absent, every sigma-review subagent uses the global active provider exactly as
before. To opt in, add the block per the schema above. There is no migration step
or backfill required.

### verify_provenance evidence path

The stop hook's `verify_provenance` extension activates the event-evidence path only
when at least one resolved agent is non-Claude. Pure-Claude runs continue through the
existing transcript-parser path with no behavior change. The evidence logic is the
bundled binary's `fno-agents verify-evidence event` verb (folded out of the deleted
`scripts/lib/verify-event-evidence.sh` in US1); see
`crates/fno-agents/tests/verify_evidence_parity.rs` for the differential contract
tests and `tests/integration/test_per_agent_routing_bdd_invariants.py` for the
end-to-end BDD invariants.

## Failover hardening (Plan A)

The Spec 2 failover controller ships with a closed-taxonomy `ErrorClass` enum and fixed-cooldown swap behavior. Plan A of the 9router port adds two complementary behaviors without touching the swap-decision contract or the existing `failover-state.json` schema:

1. **Priority-ordered error rules.** Text-substring matches in the response body run BEFORE HTTP-status fallback, so `"rate limit"`, `"quota exceeded"`, `"capacity"`, `"overloaded"` etc. catch the rate-limit class even when the upstream provider returns 200 with a soft-error body. Status rules (401, 402, 403, 404, 429) act as fallbacks. Rules live in `cli/src/fno/adapters/providers/error_taxonomy.py::ERROR_RULES`, ported verbatim from 9router's `errorConfig.js`.

2. **Per-provider exponential backoff.** Repeated rate-limit/quota errors increment a per-provider `backoff_level` from 0 toward 15. The cooldown for the just-witnessed error is `BASE * 2^old_level` (1st hit -> 2000ms, 2nd -> 4000ms, 9th and beyond capped at MAX_BACKOFF_MS = 5min). A successful call clears the level back to 0 via the public `failover.record_success(provider_id)` helper.

### State separation

Plan A introduces a NEW state file at `.fno/provider-runtime-state.json` for per-provider backoff. This file is **distinct from** `failover-state.json`:

| File | Owns | Lifetime | Lock |
|---|---|---|---|
| `failover-state.json` | phase storm-cap, no-swap-back | per-phase (resets on phase boundary) | `<path>.lock` |
| `provider-runtime-state.json` | per-provider backoff_level + rate_limited_until | survives target spawns within a megawalk campaign; 1h TTL | `<path>.update.lock` |

The two files use different sidecar lock paths so the runtime-state writer cannot self-deadlock on `atomic_write`'s internal lock. The `failover-state.json` schema (phase_id, swaps_this_phase, last_swap_from, last_swap_at_iso) is unchanged.

### Schema (provider-runtime-state.json)

```json
{
  "schema_version": 1,
  "provider_health": {
    "claude-anthropic": {
      "provider_id": "claude-anthropic",
      "backoff_level": 3,
      "rate_limited_until": 1779402812.523,
      "last_error_at": 1779402796.412
    }
  }
}
```

- `schema_version` will bump to 2 when Plan B adds `combo_cursors`. Plan A reads files marked with future schema versions but logs nothing extra; unknown fields are ignored on parse.
- `backoff_level` is clamped to `[0, MAX_BACKOFF_LEVEL=15]` on construction AND on disk-read. A corrupt or hand-edited out-of-range integer is repaired in memory; the next write rewrites disk with the clamped value.
- `rate_limited_until` and `last_error_at` are unix epoch seconds (UTC). `last_error_at` is what the 1h TTL is measured against.

### Wiring into failover

`failover.attempt_swap()` consults `classify_error(status, body)` after the existing `triggers_swap` check. If a rule matches (text rule OR status rule), `update_provider_health(provider_id, rule)` writes the new backoff record. The swap decision itself (storm-cap, no-swap-back, queue-exhausted) is unchanged - the new state is supplementary. Failures inside the runtime_state IO layer (`OSError`, `JSONDecodeError`) are swallowed and logged so the swap path is never blocked; programmer errors (TypeError, AttributeError, etc.) propagate so they surface in CI.

### Concurrency contract

- Writes to `provider-runtime-state.json` serialize via `filelock.FileLock(path + ".update.lock", timeout=5)`. The lock releases after `os.replace()` commits the tempfile.
- Lock-contention timeout (>5s) returns the last-known-good `ProviderHealth` without raising and without incrementing.
- Two parallel processes both calling `update_provider_health(provider, rule)` produce a final state where the level reflects both increments (no lost updates). Test `TestConcurrency::test_concurrency_no_lost_updates` pins this with `multiprocessing.Process(spawn)` workers.

### Plan B prerequisites

This is Plan A. Plan B (combos + round-robin rotation) extends `ProviderRuntimeState` with `combo_cursors` and adds `dispatch_with_combo()` consumers that read `is_in_cooldown()` for cooldown-aware candidate selection. Plan A explicitly does NOT add cooldown-aware filtering to `_next_eligible_provider`; that is Plan B scope.

## Per-model lockout granularity (Plan A1)

Plan A's `ProviderHealth` locks the WHOLE provider record when any model errors. Plan A1 adds per-model granularity so a 429 on `claude-opus-4-7` locks only that model and leaves `claude-sonnet-4-6` on the same Anthropic key usable. This closes the bulk of the Spec 2.5 lockout-precision gap.

### Schema delta (additive)

`ProviderHealth` gains a `model_locks: dict[str, float]` field mapping model identifier to unix-epoch cooldown expiry. Provider-level `rate_limited_until` is unchanged.

```json
{
  "schema_version": 1,
  "provider_health": {
    "claude-anthropic": {
      "provider_id": "claude-anthropic",
      "backoff_level": 3,
      "rate_limited_until": null,
      "last_error_at": 1779402796.412,
      "model_locks": {
        "claude-opus-4-7": 1779402820.0
      }
    }
  }
}
```

`schema_version` does NOT bump - this is a backward-compatible field addition. Older readers ignore the field; Plan A files without `model_locks` are read as `{}` (empty dict) by Plan A1 code.

### Write semantics

`update_provider_health(provider_id, rule, model=X)` writes ONLY `model_locks[X]`. `rate_limited_until` is preserved untouched (Locked Decision 2: model-locks-only when model is known). The provider-level `backoff_level` still increments per call so consecutive errors on sibling models ramp the cooldown correctly (Locked Decision 5). When `model=None`, behavior matches the Plan A baseline.

### Read semantics

`is_in_cooldown(provider_id, model=X)` does a two-level lookup:

1. If `model_locks[X] > now` → True (model-specific lock)
2. If `rate_limited_until > now` → True (provider-level lock)
3. Otherwise False

A provider-wide lock catches a query for any model on that record; a model-specific lock only catches queries for that exact model.

### TTL

Stale `model_locks` entries drop together with their parent `ProviderHealth` record when `last_error_at < now - 1h` (Locked Decision 6: record-level TTL only, no per-model TTL). No separate per-model expiry sweep.

### Producer wiring (`NormalizedError.model`)

`normalize(http_status, exit_code, body, *, model="claude-opus-4-7")` clamps to 256 bytes and threads the identifier through `NormalizedError.model`. `NormalizedError.__post_init__` rejects empty strings so an accidental `model=""` surfaces in CI instead of crashing the failover swap path. `failover.attempt_swap` forwards `error.model` to `update_provider_health(..., model=...)`. Existing call sites that omit `model=` route through the `model=None` branch and see exactly the Plan A baseline.

### Test contract

The headline scenario (`test_ac3_1_opus_locked_sonnet_free`) pins the user-visible behavior. The fcntl race test (`test_ac7_1_parallel_different_models_serialize`) verifies lost-update prevention: 10 parallel processes writing different model_locks to the same provider produce a final state with all 10 entries AND `backoff_level == 10`.

### Out of scope (for Plan A1)

- Codex / Gemini / GLM / OpenClaw / Hermes adapters (separate plans A2-A5)
- Plan B combo logic
- Per-segment cost-attribution math (separate Spec 2.5 follow-up)
- Lockout-reason persistence with semantic taxonomy beyond model granularity (separate Spec 2.5 follow-up)
- Per-model TTL (record-level TTL is the design)
- Adding new `ErrorClass` enum values (locked by design)

## Combos and round-robin (Plan B)

Combos are named ordered provider lists with a rotation strategy. They sit on top of the Plan A substrate (`ProviderHealth`, `is_in_cooldown`, `classify_error`, `update_provider_health`) and add per-combo cursor state in the same `provider-runtime-state.json` so parallel target spawns within a megawalk campaign share rotation.

### Schema

```yaml
config:
  providers:
    active: claude-primary             # existing
    active_combo: my-stack             # NEW (optional; set via `fno providers combos use`)
    records:
      claude-key-a:
        cli: claude
        auth: oauth_dir
        credentials_source: ~/.claude
      claude-key-b: { ... }
      claude-key-c: { ... }
    combos:                            # NEW
      my-stack:
        strategy: round_robin            # or "fallback"
        sticky_limit: 3                  # ignored for fallback
        providers:
          - claude-key-a
          - claude-key-b
          - claude-key-c
      cheap-only:
        strategy: fallback
        providers:
          - claude-key-c
          - gemini-codex
```

| Strategy | Behavior |
|----------|----------|
| `fallback` (default) | Sequential try-next-on-error. Preserves single-provider semantics when the list has one entry. |
| `round_robin` | Time-sliced cycle. The cursor sticks on one index for `sticky_limit` calls before advancing. `sticky_limit` is clamped to `1` minimum. |

### CLI surface

```
fno providers combos add <name> --strategy {fallback|round_robin} \
  --sticky N --providers a,b,c [--scope project|global]
fno providers combos list [--json]
fno providers combos remove <name> [--scope project|global]
fno providers combos test <name>      # config-only validation; reports per-member health
fno providers combos use <name> [--scope project|global]
```

`combos test` does NOT issue real API calls (smoke-pinging every member multiplies cost). For an active liveness probe, run `fno providers test <id> --smoke` per member.

### Resolution priority

When a subagent dispatch needs to pick a provider, `sigma_dispatch.resolve_dispatch_target` walks this chain (highest first):

1. `config.agents.<name>.provider`  (Spec 3 per-agent pin)
2. `TARGET_COMBO` env var             (set by `--combo` CLI flag, skill modifier, or megatron manifest)
3. `config.providers.active_combo`   (settings default)
4. `config.providers.active`         (existing fall-through)

Per-agent pins win over combos when both are configured for the same agent: combos compose with per-agent routing as additional fallback, not replacement.

Unknown combo (in env or settings) logs a WARNING and falls through to the next rule. `ComboNotFoundError` is reserved for `dispatch_with_combo` itself (the silent-bypass-blocker that callers can catch and fall through cleanly).

### Cursor state

Per-combo cursors live in `provider-runtime-state.json` under `combo_cursors.<name>`:

```json
{
  "schema_version": 2,
  "provider_health": { ... },
  "combo_cursors": {
    "my-stack": {
      "combo_name": "my-stack",
      "cursor_index": 1,
      "consecutive_use_count": 2,
      "providers_hash": "ab12cd34ef567890",
      "last_rotated_at": 1715432100.5
    }
  }
}
```

`providers_hash` is a stable order-sensitive sha256[:16] of the combo's providers tuple. When the user edits the combo (add/remove/reorder), the hash changes and the next read returns `None` (cursor invalidated) - the next `advance_cursor` resets to `(idx=0, count=1)` cleanly.

`last_rotated_at` drives the 24h TTL: a quiescent combo's cursor is dropped on the next locked write to `provider-runtime-state.json` and a future advance starts fresh at `(idx=0, count=1)`.

Cursor-state writes serialize via the same fcntl lock (`provider-runtime-state.json.update.lock`) as `update_provider_health` / `reset_provider_health`, so two parallel `advance_cursor` calls never lose updates.

### Sticky math (port of 9router's `getRotatedModels`)

For `round_robin` with `N` providers and `sticky_limit=K`:

| Call # | Returned cursor (idx, count) | Rotation result (providers=[a,b,c], K=3) |
|--------|------------------------------|------------------------------------------|
| 1      | (0, 1) | [a, b, c] |
| 2      | (0, 2) | [a, b, c] |
| 3      | (0, 3) | [a, b, c] |
| 4      | (1, 1) | [b, c, a] |
| 5      | (1, 2) | [b, c, a] |
| 6      | (1, 3) | [b, c, a] |
| 7      | (2, 1) | [c, a, b] |
| ...    | ...    | ...    |

Single-provider combos short-circuit (cursor never advances past `idx=0`).

### Skill + entry-point integration

| Surface | How combo is supplied |
|---------|-----------------------|
| `/target` skill | `/target combo my-stack "feature"` (positional 2-token modifier) |
| `/megawalk` skill | `/megawalk combo my-stack` |
| `run-target-loop.sh` | `TARGET_COMBO=my-stack bash scripts/run-target-loop.sh <plan>` (env; the `fno loop` verb is removed) |
| `fno megawalk` | `fno megawalk --combo my-stack` |

All paths terminate in setting `TARGET_COMBO=<name>` in the environment of spawned subprocesses (`spawn_with_provider_snapshot` already propagates env to target children).

### Failure modes

- **Empty `providers` list:** `Combo.__post_init__` raises `ValueError`; `load_combos` wraps as `ProviderConfigError`.
- **Invalid strategy:** raised at construction-time.
- **Unknown provider id in `--providers`:** rejected before `settings.yaml` mutation.
- **Combo deleted mid-dispatch:** `dispatch_with_combo` raises `ComboNotFoundError`; `sigma_dispatch.resolve_dispatch_target` catches its loader equivalent and falls through.
- **All members in cooldown:** `dispatch_with_combo` returns `QueueExhausted(retry_after=...)` with the soonest cooldown-expiry hint.
- **YAML round-trip via PyYAML loses comments:** documented limitation. Use `fno providers combos add/remove` for safe edits, or hand-edit and re-add.

## Codex CLI runtime adapter (Plan A2)

Plan A2 is the first non-Claude `RuntimeAdapter`
implementation. After this lands, a provider record with `cli: codex` works
end-to-end: `get_adapter("codex")` resolves cleanly, `dispatch_with_combo` from
Plan B picks it up automatically (Locked Decision 8: no changes to `rotation.py`),
and the universal error taxonomy from Plan A applies to Codex subprocess outcomes.

The adapter mirrors `ClaudeCodeAdapter` structurally because the abstraction
(3 primitives + health) is intentionally narrow. The only meaningful divergence
is which binary gets invoked (`codex`) and how its exit codes map to footnote'
closed `ErrorClass` enum.

### Shape

```
RuntimeAdapter Protocol (3 primitives + health)
       |
       v
CodexCliAdapter (name = "codex")
       |
       +-- spawn_worker(prompt) -> subprocess.Popen(["codex", "exec", prompt])
       |   (in-session env => skill_dispatch_required sentinel; no shell spawn)
       |
       +-- create_worktree(name) -> _shared.create_worktree(name) (Locked Decision 5)
       |
       +-- call_api(command, retries=3) -> subprocess.run(["codex"] + command)
       |   (retry on 137/143/124; non-retryable on usage errors)
       |
       +-- health() -> AdapterHealth
           (binary on PATH + version >= MIN_CODEX_VERSION + auth)
```

### In-session sentinel

Inside any CLI agent session (`CLAUDECODE_SESSION_ID` OR `CODEX_SESSION_ID`
present), shell-spawn is forbidden. `spawn_worker` returns the same
`{"action": "skill_dispatch_required", ...}` envelope as the Claude adapter,
forcing callers to use the Agent tool dispatch path instead. Both env vars
are checked because the adapter may be invoked from inside either flavour of
agent session.

### `call_api` retry policy

| Exit code | Source | Retryable | Map to |
|-----------|--------|-----------|--------|
| 0 | success | n/a | success |
| 1 | usage error | no | PARSER_ERROR |
| 2 | subcommand error | no (caller inspects stderr) | UNKNOWN or PROVIDER_5XX (with server-side hint) |
| 124 | timeout (coreutils convention) | yes | PARSER_ERROR |
| 137 | SIGKILL (128 + 9) | yes | PARSER_ERROR |
| 143 | SIGTERM (128 + 15) | yes | PARSER_ERROR |
| `-N` | Python subprocess signal-killed | yes | normalize to `128 + abs(N)` |

Backoff is exponential (`2 ** attempt` seconds, capped at 8s). Non-retryable
codes return the failed result immediately; the caller passes the returncode
through `map_codex_error` to decide whether to swap providers.

### `health()` checks

`health()` is non-invasive: no real API call, no prompt invocation. Three
checks in order:

1. **Binary**: `codex --version` is on PATH (10s timeout, FileNotFoundError ->
   ok=False with PATH error message).
2. **Version**: `MIN_CODEX_VERSION = "0.117.0"` is the minimum supported.
   Unparseable version strings surface a distinct "could not parse codex version
   string: '...'" message, not the misleading "version too old".
3. **Auth**: either `~/.codex/auth.json` exists and is non-empty (oauth) OR
   `OPENAI_API_KEY` env var is set (api_key). OAuth takes precedence when both
   are present, matching Codex's own resolution preference.

### `map_codex_error` order of operations

Per Locked Decision 4, error mapping is per-adapter and calls Plan A's
universal `classify_error` first. The order is:

1. **Universal text rules first**. `normalize(http_status=None, exit_code=rc,
   body=stderr)` plus a fall-through walk of Plan A's `ERROR_RULES`. Catches
   `rate limit` / `too many requests` / `quota exceeded` / `capacity` /
   `overloaded` regardless of which CLI emitted the error, plus auth-shape
   phrases like `no credentials`. Backoff matches map to `PROVIDER_4XX_QUOTA`
   + `triggers_swap=True`; long-cooldown matches like `no credentials` map to
   `PROVIDER_4XX_AUTH` + `triggers_swap=True`.
2. **Negative returncode normalisation**. Python's `subprocess.run` returns
   `-signal_number` for signal-killed children on POSIX (so SIGKILL surfaces
   as `-9`, while the same outcome under a shell appears as `137`). Negative
   values are normalised to their shell-style counterparts so both call paths
   classify identically.
3. **Codex-specific exit-code fallback** for cases the universal rules don't
   cover: `0` -> `UNKNOWN` (defensive; success shouldn't reach the mapper),
   `1` -> `PARSER_ERROR`, `2` with server-side hint (`internal error`,
   `unavailable`, `5xx`, `server`) -> `PROVIDER_5XX` + swap, `2` without hint
   -> `UNKNOWN`, `124/137/143` -> `PARSER_ERROR`.

`body_excerpt` is truncated to 256 chars and stderr blob to 64KB before
processing, matching Plan A's existing conventions.

### `_shared.create_worktree` (Locked Decision 5)

The `create_worktree` primitive is CLI-agnostic and lives in
`cli/src/fno/adapters/_shared.py`. Both `ClaudeCodeAdapter` and
`CodexCliAdapter` delegate to it. Status vocabulary aligns with
`runtime/worktree.py` (`"created"` | `"already-exists"`) so callers can switch
layers without translating return shapes. The helper raises `RuntimeError`
with the captured stderr text when `git worktree add` fails, so callers see
why instead of a bare exit code.

### Plan B integration is automatic (Locked Decision 8)

Plan B's `dispatch_with_combo("my-mixed-stack", fn)` calls
`get_adapter(record.cli)` for each provider in the combo. Registering
`CodexCliAdapter` as `"codex"` in `cli/src/fno/adapters/__init__.py`
is the only change needed - no edits to `rotation.py`, `dispatch_with_combo`,
or any combo-resolution code.

Example combo that routes work across Claude and Codex:

```yaml
config:
  providers:
    combos:
      mixed-stack:
        strategy: round_robin
        providers: [claude-anthropic, codex-openai]
        sticky_limit: 3
    records:
      - id: claude-anthropic
        cli: claude
        auth: oauth_dir
        credentials_source: ~/.claude
      - id: codex-openai
        cli: codex
        auth: oauth_dir
        credentials_source: ~/.codex
```

Run with `TARGET_COMBO=mixed-stack bash scripts/run-target-loop.sh <plan>` or `/target combo mixed-stack
"<feature>"` and dispatch rotates between the two CLIs per the sticky limit.

### Provider record example

A minimal Codex provider record:

```yaml
config:
  providers:
    records:
      - id: codex-openai
        name: Codex OpenAI
        cli: codex
        auth: oauth_dir
        credentials_source: ~/.codex
      # OR with api_key auth:
      - id: codex-api
        name: Codex API
        cli: codex
        auth: api_key
        env:
          OPENAI_API_KEY: $OPENAI_KEY
```

After adding the record, `fno providers test codex-openai --smoke` validates
the adapter end-to-end against the binary.

### Out of scope for Plan A2

- Other CLI adapters: `gemini.py`, `glm.py`, `openclaw.py`
  (separate plans, one per CLI; Hermes shipped as Plan A3)
- HTTP API translation (Locked Decision 12: footnote is a CLI subprocess
  orchestrator, not an HTTP translator)
- Per-mission `~/.codex/config.toml` overrides (user-side)
- Cost-aware routing / Codex-specific pricing in provider records
  (deferred to Spec 2.5)
- Populating `NormalizedError.model` from Codex's `--model` flag (Plan A1
  adds the field; Plan A2 leaves it `None`. Future plans can populate it
  for per-model lockout granularity.)

## Hermes Agent CLI runtime adapter (Plan A3)

Plan A3 is the second non-Claude `RuntimeAdapter`. After it lands, a
provider record with `cli: hermes` resolves through `get_adapter("hermes")`,
`dispatch_with_combo` picks it up via Locked Decision 8, and the universal
error taxonomy applies to Hermes subprocess outcomes.

Hermes Agent (`/nousresearch/hermes-agent`) is an open-source AI agent
platform with persistent memory and tool-calling. The adapter dispatches
via `hermes chat -q "<prompt>"` and lets Hermes' own memory semantics
apply. Unlike Claude Code and Codex (one-shot CLIs), Hermes carries
memory across invocations by default; the adapter does not enforce
statelessness, that is a Hermes-server-side concern.

### Shape

```
RuntimeAdapter Protocol (3 primitives + health)
       |
       v
HermesCliAdapter (name = "hermes")
       |
       +-- spawn_worker(prompt) -> subprocess.Popen(["hermes", "chat", "-q", prompt])
       |   (in-session env => skill_dispatch_required sentinel; no shell spawn)
       |
       +-- create_worktree(name) -> _shared.create_worktree(name) (Locked Decision 5)
       |
       +-- call_api(command, retries=3) -> subprocess.run(["hermes"] + command)
       |   (retry on 137/143/124; non-retryable on usage errors)
       |
       +-- health() -> AdapterHealth
           (binary on PATH + hermes doctor exit 0 + config dir present)
```

### Differences from the Codex adapter

| Concern | Codex (A2) | Hermes (A3) |
|---------|-----------|-------------|
| Subcommand | `codex exec <prompt>` | `hermes chat -q "<prompt>"` |
| In-session env vars | `CLAUDECODE_SESSION_ID`, `CODEX_SESSION_ID` | adds `HERMES_SESSION_ID` |
| In-session check | `os.environ.get(...) is truthy` | `os.environ.get(...) is not None` (fail-closed; empty string still counts) |
| Health probe | `codex --version` + version parsing + auth file or env | `hermes doctor` exit code + config dir candidate search |
| Min version | `MIN_CODEX_VERSION = "0.117.0"` | none (no `--version` flag documented) |
| Auth model | `~/.codex/auth.json` (oauth) or `OPENAI_API_KEY` | not enforced at adapter layer; Hermes wraps an underlying provider |
| Statefulness | one-shot per invocation | persistent memory across invocations (server-side concern) |
| Exit codes | 1=usage, 2=subcommand | 1=usage, 2=runtime, 3=auth |

### In-session sentinel (fail-closed)

Inside any CLI agent session (`CLAUDECODE_SESSION_ID`, `CODEX_SESSION_ID`,
or `HERMES_SESSION_ID` set to anything including empty string), shell-spawn
is forbidden. `spawn_worker` returns the standard
`{"action": "skill_dispatch_required", ...}` envelope, forcing callers to
use Agent-tool dispatch instead. The empty-string treatment is fail-closed
specifically because Hermes' persistent memory makes silent in-session
spawn doubly dangerous: a second hermes invocation could observe or
corrupt state from the parent session.

### `health()` checks

`health()` is non-invasive: no real LLM call, no chat invocation. Three
checks in order:

1. **Binary**: `hermes doctor` is on PATH (15s timeout; longer than Codex's
   10s because `doctor` may probe multiple subsystems). FileNotFoundError
   surfaces ok=False with a PATH error message.
2. **Doctor exit**: `hermes doctor` returns exit 0. Non-zero surfaces
   ok=False with the exit code in the error message.
3. **Config dir**: at least one of `~/.config/hermes`, `~/.hermes`, or
   `~/Library/Application Support/hermes` exists as a directory. The
   ordering is XDG-first, POSIX-home second, macOS-specific third.
   - A candidate path that exists as a file or broken symlink surfaces
     a distinct "config path exists but is not a directory" error so
     the operator knows to clean up the stray entry rather than
     re-running `hermes setup` futilely.

Every health return path - happy and unhappy - populates `doctor_exit`,
`doctor_stdout`, and `doctor_stderr` in `details` (None on early-return
paths) so downstream consumers can read them without a KeyError guard.

### `map_hermes_error` order of operations

Per Locked Decision 4, error mapping is per-adapter and walks Plan A's
universal `classify_error` first. The order is:

1. **Universal text rules first**. `normalize(http_status=None,
   exit_code=rc, body=stderr)` plus a fall-through walk of Plan A's
   `ERROR_RULES`. Catches `rate limit` / `too many requests` /
   `quota exceeded` / `capacity` / `overloaded` regardless of which CLI
   emitted the error, plus auth-shape phrases like `no credentials`.
   Backoff matches map to `PROVIDER_4XX_QUOTA` + `triggers_swap=True`;
   long-cooldown matches like `no credentials` map to
   `PROVIDER_4XX_AUTH` + `triggers_swap=True`.
2. **Negative returncode normalisation**. Same as Codex: signal-killed
   subprocess returncodes (`-N`) are normalised to shell-style 128+N
   when `signum < 128`, so both Python subprocess and shell call paths
   classify identically.
3. **Hermes-specific exit-code fallback** for cases the universal rules
   don't cover: `0` -> `UNKNOWN` (defensive), `1` -> `PARSER_ERROR`,
   `2` with server-side hint (`internal error`, `unavailable`, `5xx`,
   `server`, `upstream`) -> `PROVIDER_5XX` + swap, `2` without hint
   -> `UNKNOWN`, `3` -> `PROVIDER_4XX_AUTH` + swap (the Hermes-specific
   exit-3-as-auth-error convention; verify against real binary per
   [VERIFY-AT-IMPL] in `hermes.py`), `124/137/143` -> `PARSER_ERROR`.

`body_excerpt` is truncated to 256 characters and stderr blob to 64K
characters before processing (the constant names say "bytes" but the
slices are character-counted; a Codex-parity rename is queued as a
follow-up).

### Plan B integration is automatic (Locked Decision 8)

Plan B's `dispatch_with_combo("my-mixed-stack", fn)` calls
`get_adapter(record.cli)` for each provider in the combo. Registering
`HermesCliAdapter` as `"hermes"` in
`cli/src/fno/adapters/__init__.py` is the only change needed - no
edits to `rotation.py`, `dispatch_with_combo`, or any combo-resolution
code.

Example combo that routes work across Claude, Codex, and Hermes:

```yaml
config:
  providers:
    combos:
      multi-cli:
        strategy: round_robin
        providers: [claude-anthropic, codex-openai, hermes-nous]
        sticky_limit: 3
    records:
      - id: claude-anthropic
        cli: claude
        auth: oauth_dir
        credentials_source: ~/.claude
      - id: codex-openai
        cli: codex
        auth: oauth_dir
        credentials_source: ~/.codex
      - id: hermes-nous
        cli: hermes
        auth: oauth_dir
        credentials_source: ~/.config/hermes
```

### Provider record example

A minimal Hermes provider record:

```yaml
config:
  providers:
    records:
      - id: hermes-nous
        name: Hermes Nous
        cli: hermes
        auth: oauth_dir
        credentials_source: ~/.config/hermes
```

### `[VERIFY-AT-IMPL]` markers (pre-merge gates)

Plan A3 was written against `ctx7`-fetched documentation rather than a
running Hermes binary. The implementer flagged five assumptions in
`hermes.py` with `[VERIFY-AT-IMPL]` markers:

- Doctor command (`hermes doctor`) exit-code semantics and stdout shape
- Config directory canonical path (currently three candidates)
- Exit code numbering (1/2/3 follow the Codex convention)
- Doctor timeout (15s; tune based on real-binary timings)
- Auth env-var name(s); the adapter treats them as opaque

Real-binary smoke verification is gated behind `FNO_RUN_SMOKE=1`
per AC4.3. The implementer (or the user before merge) should run
`hermes doctor` and `hermes chat -q "test"` against an installed binary
and either remove the markers or correct the values.

### Out of scope for Plan A3

- Cross-adapter parity fixes (a sigma-review pass surfaced several
  symmetric bugs in Codex; they land in a separate follow-up PR rather
  than expanding Plan A3's surface)
- HTTP API translation (Locked Decision 12)
- Hermes server-side configuration (user installs and sets up Hermes;
  footnote just invokes the binary)
- Forcing stateless dispatch when Hermes is configured stateful
  (server-side concern; documented but not enforced)
- Hermes MCP server mode (`hermes mcp serve`) is a different shape from
  one-shot CLI dispatch; separate concern
- Other CLI adapters: `gemini.py`, `glm.py`, `openclaw.py` (separate
  plans, one per CLI)
