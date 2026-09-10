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

## The spawn seam: every launch crosses it

Every `fno agents spawn` crosses the Python seam (`inject_spawn_defaults`). The binary enforces this: a direct `fno-agents spawn` without the `--defaults-applied` marker is sent back to the front door once, and a marked spawn dispatches natively. The marker carries the seam's enforcement verdict (`enforced` or `unenforced`). It records a decision the seam already made. It never grants one.

A configured axis that was not applied says so. stderr names the dropped value, the config rung it came from, and the reason. One `spawn_defaults_applied` journal event per spawn records every resolved, applied, and suppressed axis, with empty values included. A new config-sourced spawn axis needs nothing else: route the value through the seam, and let the seam name what it did not apply.

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

The auxiliary roles above are coordination work. `build` extends the same mechanism to *delivery* spawns (`/target bg` + the ordered advance drain), so a whole feature build can run on GLM.

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

The harness axis has one home: `config.agents.profiles.<verb>.provider`. The autonomous dispatch resolver reads it, so one node dispatched through `dispatch-node.sh` and through `fno agents spawn` lands on one harness. The old `config.dispatch.harness` key is deprecated. It reads as the fallback rung beneath the stage table for one release, so an installation setting only that key is unchanged. When both keys are set and disagree, the stage table wins and the resolve receipt names the losing spelling. `fno config doctor` prints the migration for every config file still carrying the old key.

```toml
[agents]
pane_group_max = 4

[agents.profiles.blueprint]
model = "opus"

# lanes is RANK: the walk is in declared order, first pass wins.
[agents.profiles.target]
lanes = ["flash-zai", "luna-codex"]
on_exhausted = "queue"

[[routing.models]]
name = "flash-zai"
harness = "claude"
model = "glm-5.3-flash[1m]"
route = "zai/glm-5.3-flash[1m]"
account = "zai-main"
effort = "high"

[[routing.models]]
name = "luna-codex"
harness = "codex"
model = "gpt-5.6-luna"
effort = "xhigh"
```

### The harness overlay: one base, per-harness answers

A permission mode, an effort value, and a substrate are not fleet policies. Each is a flag spelling the harness defines, so one scalar under `agents.defaults` or `agents.profiles.<verb>` cannot serve every harness. The stage table therefore carries a `harness` table on the defaults and on every profile, keyed by harness name, whose entries re-answer `permission_mode`, `effort`, or `substrate` for that one harness, plus an opaque `args` list appended behind the spawn's `--` passthrough fence. `args` is how an operator references the harness's own bundle (`codex --profile <name>`, `claude --settings <file>`) instead of asking fno for a behavior column per harness: the overlay carries flags whose vocabulary the harness defines and fno already forwards, never a ranking field. `provider`, `model`, `route`, and `account` refuse at the spawn seam when found in an overlay: those are lane fields.

Precedence across the six rungs, one line: `explicit flag > lane > profiles.<verb>.harness.<h> > profiles.<verb> > defaults.harness.<h> > defaults`. The scalars keep their meaning as the base that works for most; nothing migrates. `fno config doctor` names every (rung, harness) pair a scalar cannot serve, with the overlay table that fixes it.

```toml
[agents.defaults]
permission_mode = "bypassPermissions"      # works for most: claude, grok

[agents.defaults.harness.codex]
permission_mode = "yolo"
args = ["--profile", "fno"]                # codex resolves the rest from [profiles.fno]

[agents.profiles.target]
effort = "high"

[agents.profiles.target.harness.codex]
effort = "xhigh"
```

When a profile has `lanes`, the list is the rank. The live-worker count plays no part in where the walk starts. A lane is either the name of a `[[routing.models]]` row or an inline table with the same fields. Both spellings fold into one row inventory, so there is one selection path. The inline shape is sugar, never a second leg. The spawn walks the lanes in declared order and takes the first lane that passes. A lane skips for three reasons. The pinned substrate or permission mode cannot ride its harness. Its routed vendor sits at `agents.provider_limits`. Live capacity reads `exhausted` for the account the row names. Every skip names the lane and the reason in the spawn receipt. A verb with lanes never consults the difficulty grid. A verb without lanes falls through to the grid over the whole inventory, exactly as before lanes existed.

Capacity resolves per lane, not per harness. Quota locks out at the ACCOUNT. A row that names a `config.accounts.records` id reads that account's own state. A lane pinned to a locked-out account skips. A sibling lane on a healthy account answers. The old harness-wide MAX read that same fleet as healthy. A row that names no account reads the harness-wide aggregate. That aggregate is the correct answer for an unnamed row. An account the capacity snapshot does not name reads `unknown`, which permits.

When every lane skipped, `on_exhausted` declares the terminal. `refuse` (the default) stops the spawn rather than billing an unintended lane. `degrade` lets the profile scalars and `agents.defaults` answer. The lanes play no part, and the receipt names the degrade. `queue` exits 78 with a typed capacity refusal that a dispatcher reads as capacity, not config. The refusal carries `reason: slot_exhausted` and one entry per lane. An out-of-enum value refuses by name. It never silently coerces to a terminal nobody named. Two escapes narrow any refusal, and neither weakens the skip. A command line can name its own lane with `--harness`, `-P` or `--route`. `FNO_SPAWN_GATE=0`, the admission bypass, never blocks a spawn. Both degrade instead of refusing, whatever `on_exhausted` says. A lane string naming no declared row refuses at the spawn seam with the declared row names.

A lane is opaque about behavior and transparent about economics. fno ranks on what it declares: harness, optional band, cost, context, account. Everything else is passthrough to the harness's own bundle. That covers permission mode, sandbox, system prompt, skills, MCP and subagent roles. codex forwards them with `--profile`, claude with `--settings`. fno cannot win the flag-enumeration race against harnesses that grow launch flags every release, so it forwards and does not remodel. Substrate and `permission_mode` ride an inline lane as declared passthrough, and `pane_group` places the pane. Three personas the shape survives. A research-only installation fills one slot and leaves the rest empty. Empty means the harness default, never route nowhere. A single-frontier installation points every slot at one lane. A mixed installation orders cheap lanes first and lets the walk find the one that is up. `fno config route inventory` prints each verb's slot with live lane capacity. An armed router and an absent one are no longer the same silence.

`pane_group` is injected as `fno agents spawn --tab <group>` on pane lanes. Placement happens AFTER the spawn, never before it. The pane reports its own squad, and the tab list is read scoped to that squad. The pane's own tab then joins the first `<group>`, `<group>-2`, ... tab below `pane_group_max`. If none has room, its tab takes the next sibling name instead. A group cannot combine with `--split`/`--at`. The pane then sits in a tab it does not own. The move takes that tab's other panes with it. The read-then-act is deliberately not globally serialized, so concurrent spawns can briefly overfill a tab. Placement never changes the worker route.

The two layers compose by design. The stage table picks the coordinate per verb; `--role`, attached by the dispatch lane, owns the model when it resolves. A field the dispatch pinned explicitly (harness, substrate) is not displaced, which is why a stage table entry can set the model or route without rerouting the fleet's binary.

## What a plan carries, and what it never carries

A plan names one routing axis in its frontmatter: the `difficulty` band. The band is the only axis a plan carries. Waves stamp it, the dispatch grid consumes it, and the band-to-harness/model resolution lives entirely in config (the declared inventory, the stage table, the role lanes). A plan that names no model is not a gap. Models were kept out of plans on purpose, so re-routing the fleet stays a config edit and never a plan rewrite.

The second easy misread is a wave's `mode`. `mode: sequential | parallel` describes fan-out between the tasks inside that one wave. `parallel` says those tasks can run as concurrent subagents. `sequential` says one after another. It is not a cross-wave marker, and it is not a width claim. The join width function gives an undeclared task the previous wave's whole task list as blockers. It then adds derived partition edges only for waves whose mode is `parallel`. Flipping a wave to parallel can only narrow a plan's measured width, never widen it. Reading `mode: sequential` as "this plan is narrow" gets it backwards. Several single-task sequential waves measure wider than one big parallel wave, because the single wave is where all the tasks sit unblocked. `fno backlog join` sizes its worker pool from the measured width, the node priority and the highest wave band, never from a wave's mode.

## The declared inventory and the dispatch grid

`config.routing` declares the model inventory. One `[[routing.models]]` row per model carries `name`, `harness`, `model`, and optional `band`, `effort`, `cost_per_mtok_in`, `context`, `route`, `account`, and `color` (the sideline lane color when the row matches an agent, parsed by the mux's Rust reader and declared in the schema so a typo surfaces at config validation).

A small built-in table sits under this key as a **fallback**, never the authority. Config overrides it and extends it. A row naming an existing model replaces only the fields it names. A new name is added to the set. Adding a model, provider or harness stays a config edit. It is never a Python edit. A stranger's install declares a fleet that outranks every built-in row.

The fallback keeps a tier request answerable where nothing is declared. Review level names a model for every level. Answering nothing drops `/code-review` to the provider default everywhere. The grid is unaffected and stays config-first. A virgin install records `grid=no-inventory-declared` and injects nothing. The grid asks whether config declared a row, not whether any row exists.

`cli/src/fno_routing_sample/routing_sample.toml` ships as a labelled sample inside the package, so an installed wheel finds it too. No routing code path reads it. `fno config route init` appends it to your config commented out. `fno doctor route` lists every declared row with its resolved band and reachability verdict. A row on an uninstalled harness refuses BY NAME on stderr.

## The strict inventory policy

`routing.enforce_inventory` (default off) turns the declared inventory from a preference into a boundary. Under it, every spawn qualifies against the declared slots only. An explicit `--model`, `-P` or `--route` no slot declares is refused by name, and so is an unqualified lane. The harness default and the built-in fallback sit out of the decision path entirely. `routing.operator_access` (default `unknown`) says where the operator watches from: `local`, `remote`, or `unknown`, and unknown filters like remote.

A row can carry `operator_view`: `claude-native` or `codex-native`, matching its harness. A row with a vendor `route` cannot: that coordinate is not native, and labeling it so is a named refusal. Under `remote` or `unknown`, only labeled rows qualify. The point is observability: a launch must land in a view the operator can actually see. Under `local` every declared lane qualifies.

The work kind picks the slot. Rust owns the ruling: a planless `/target` does planning work and walks the blueprint slot while its command stays target. A planned target, think, blueprint, review, and crown walk their own slots. The qualification owner is one Rust verb, `fno-agents route-slot`. The spawn seam, `fno backlog explain`, and `fno config route inventory` all read the same decision, and the readouts carry its verdict: `routing=armed|unarmed|policy-held|capacity-held`. A policy hold is never described as a spent quota, and a held capacity never as a broken dispatch.

Completion evidence is a read-only audit: `fno-agents route-slot audit --project <root> --node <node> --since 30m --json`. A bounded snapshot (config fingerprint, spawn receipts, registry rows, decision records) is loaded through the established readers and a pure verifier answers. Exit 0 prints `ROUTING_POLICY_VERIFIED` per session and names the account, vendor/model and observed model. The operator view evidence is a live decision record under subject `routing-view:<session-id>`. Record it with `fno inbox decide` only after the operator confirms that exact session in the named view. A worker or peer assertion is not operator confirmation. Anything missing, stale, contradictory, or merely simulated exits nonzero naming its boundary.

Cost belongs to the ACCESS PATH, not the model. The same model reached two ways is two rows with two cost profiles. One vendor prices a pair 3x cheaper on subscription credits and 18x cheaper on API dollars. The rows are never averaged. Cheap is never read as a proxy for weak: the cheaper row can also carry more context, more throughput, and less latency.

The optional OpenRouter snapshot can supply a percentile for a row whose `band` is unset. It can never make the grid inert.

**The objective is itself a config key.** `routing.objective` is one of three values. `cheapest-that-clears` (the default) orders by declared cost, then by the weakest band that clears. `best-available` orders by band descending, then percentile. `prefer-harness` tries rows on `routing.prefer_harness` first. Tier still wins there, and the band is never lowered to stay on the harness.

**Precedence is per-axis, not per-spawn.** An explicit flag or a profile field occupies the axis it names and nothing more. A profile LANE is atomic and occupies all three axes. A bare profile FIELD does not. `[agents.profiles.target] provider = "codex"` pins the harness. The grid still supplies model and effort within codex. A pinned `-P` or `--route` owns the model axis, and the receipt records `grid=model-axis-occupied`. Pinned `--substrate` and `--permission-mode` FILTER the candidate set instead of cancelling the decision. An empty filtered set records `grid=constrained-empty`.

**The grid joins difficulty, priority, capacity and role.** Priority bends the band: `p0` up, `p3` down. Absent difficulty rounds UP to the strong band. Plan-presence selects the role. A `/target` on an unplanned node does planning work and bills at the planning tier, band floored high. A planned `/target` bills execution at the stamped band. A protected role (`implement`, `review-verdict`) forces `best-available` and the `PROTECTED_ROLE_FLOOR` ceiling. It never refuses, and it names itself in the receipt. Capacity expands a harness to its accounts and aggregates MAX (see [axis-vocabulary.md](axis-vocabulary.md)). Any healthy account means usable. `exhausted` requires every account exhausted. Unknown capacity PERMITS a candidate and records `capacity=unknown-permitted`. Only a positive `exhausted` or `blocked` marker removes one. Every grid path appends its terminal reason to the `from_config` receipt, so an inert grid says why.

**Effort is the third grid coordinate.** The grid injects an atomic harness/model/effort triple. A row whose harness has no effort surface omits it, and nothing is injected. An explicit `--effort` wins.

**A crown spawn gets a profile key.** A seed with no leading slash-verb is every king seed, and it resolves the profile key `crown`. `[agents.profiles.crown]` reaches a crown spawn exactly like every other stage row. The attended/unattended axis is declared this way, never inferred. The response-time instrument was retracted because fno mail is injected as user-shaped text.

`fno config doctor` checks the resolved posture before a worker is launched. It reports a substrate/provider pair the spawn seam cannot honor. It also probes whether THIS session can write the claim store, by writing a real file there and removing it. A hand-started session cannot receive a per-spawn grant, so that probe is the only thing covering it. A spawned worker is covered instead by the computed `--add-dir` set (see [coordination.md](coordination.md)).

## The spawn seam contract

Every launch crosses the Python seam (`agents.spawn_defaults.inject_spawn_defaults`). The seam resolves provider, model, effort, substrate, permission-mode, route, account and pane-group from config and profile defaults, injects the flags, and marks the launch `--defaults-applied=<state>` straight after the verb. The binary reads no config: an unmarked direct `fno-agents spawn` is bounced back to the front door once, and the re-exec falls back to `fno-py` because a bare venv install ships no `fno` entrypoint.

The receipt is exactly one `spawn_defaults_applied` row per completed resolution in the agents journal (`state_dir/events.jsonl`; the `FNO_EVENTS_PATH` pin redirects it under the hermetic guard). The row keeps the flat envelope - `kind` plus named fields, no nesting - and carries `name`, `verb`, `seed`, the routing config `fingerprint`, `resolved` (every axis as value and rung, empties included: "the config read as empty here" and "the value was suppressed" are different facts), `applied`, and `suppressed` (each omitted axis with its reason). The WRITE belongs to the `route-slot journal` op, not Python: the seam resolves the journal path and feeds the payload, the verb appends. The emit can never raise: a missing binary or an unwritable journal never turns an already-valid launch into a crash, and a diagnostic failure never waives strict qualification, which is decided upstream of the emit.

The `fingerprint` is a short hash of the routing-relevant non-secret config inputs: declared rows, the policy fields, and the slot table. It answers "was the config that decided this the config that launched". A changed fingerprint says the next launch re-selects; it is never an ownership token.

The walk's answer carries `refusal_terminal {class, text}` beside the verbatim chain. A `config` fault's text is bare; a `strict` refusal's carries the policy annotation naming `config routing.enforce_inventory`. Consumers read the field; the chain strings stay verbatim for the seam, advance and doctor matchers.

Config is a leaf: the schema validates types only, and the spawn seam and the resolver validate meaning. No value validation lives in the config blocks.

## Two keys, two axes

`[[routing.models]]` is the band inventory the difficulty grid reads (`route_resolve.resolve_grid`). `model_routing.roles` is the per-role provider map spawn-env applies. They are different axes and neither seeds the other.

The grid stays config-first, so an undeclared inventory routes nothing. `resolve_grid` records `grid=no-inventory-declared`, the `dispatch_spawned` receipt carries that reason, and every banded plan lands on the ambient default. Having `model_routing.roles` set does not change it. That key is exactly what makes an undeclared inventory read as working. When the inventory is undeclared, `fno config doctor` prints a `band routing inactive:` line. When the roles key is also set, the line names it as the other axis.

One ruling is open: whether roles can seed the inventory, or whether the two keys stay separate. Until it is made, the doctor line keeps the gap visible.

## `fno config route` - legibility + on-the-fly switching

Six verbs over the same machinery (`model_routing.py` stays the single source of the env-var contract):

| Verb | Purpose |
|------|---------|
| `fno config route ls [-J]` | The effective merged table: role → `provider/model` → protocol → key status (which env var / file satisfied it, or MISSING) → auto-assigned-by. `-J` for scripts. |
| `fno config route set <role> <provider/model>` | Route a lane (atomic config write via `fno config set`). Refuses protected names + unknown providers pre-write. |
| `fno config route unset <role>` | Revert a lane to its built-in default (or unrouted); idempotent no-op if unconfigured. |
| `fno config route env <role \| provider/model>` | Print an eval-able export block for an interactive session: `eval "$(fno config route env build)" && claude`. Fails closed on a missing key (no partial block). |
| `fno config route inventory [-J]` (also `fno doctor route`) | Every declared `[[routing.models]]` row with its resolved band and reachability verdict; an uninstalled harness refuses by name on stderr. |
| `fno config routing init` | Append the shipped routing sample (`fno_routing_sample/routing_sample.toml`), commented out, to your config. |

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

A `[1m]` worker injects `CLAUDE_CODE_AUTO_COMPACT_WINDOW=800000` as the compaction backstop. The variant already selects the 1M context. This variable only sets the compaction threshold, capped at the model window. A value of `1000000` is therefore a no-op. The king compact nudge fires near 40%. The 800000 backstop fires near 80%. Override it through `extra_env` only to tune that backstop.

The built-in `zai` provider already routes the background (haiku) tier to the cheaper `glm-4.7`. Opus/sonnet run `glm-5.3`, while judgment-light background traffic stays cheap on the same provider.

**`/effort` mapping.** GLM collapses `low`/`medium`/`high` to a single high setting; only `xhigh`/`max` reach its maximum reasoning. Pin a routed build lane to `high` or above (`--effort high`); a lower effort buys nothing on GLM.

## Inherited env and the daemon carrier

A long-lived background daemon holds a copy of the env of its first shell. It re-stamps that copy into every session it spawns. A shell with foreign model exports and no base URL poisons every child. Each child asks Anthropic for a model it does not serve. The whole tier errors rather than degrading. No config edit reaches a running daemon. Only a restart or a settings pin does.

`~/.claude/settings.json` `env` wins over an inherited value. That is the durable pin. It is the only fix that spares live sessions. The current Anthropic ids to pin, one per tier: `ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-haiku-4-5-20251001`, `ANTHROPIC_DEFAULT_SONNET_MODEL=claude-sonnet-5`, `ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-5`, `ANTHROPIC_DEFAULT_FABLE_MODEL=claude-fable-5`.

**The naming trap.** There is no Haiku 4.7. The current lineup is Haiku 4.5, with Sonnet, Opus and Fable at 5. The zai provider's `haiku_model` IS `glm-4.7`. That number does not transfer to an Anthropic id. Guessing `claude-haiku-4-7` fails exactly the way the GLM names do.

The code side of the same defense: `incoherent_model_env` (`cli/src/fno/agents/model_routing.py`) names every offending model var. An offending var carries a non-Anthropic id while the endpoint is Anthropic's. The substrate seams that copy the parent env strip those vars before any route overlay. Those seams are `bg_create`, `headless_create`, `_default_wake_fn`, and `_mesh_env_wrapper`. Each strip prints one stderr line. A bg spawn needs more than the strip. The serving session is forked by the claude daemon with the daemon's own env. So `bg_create` also floats a `--settings` file flooring the offending vars. The Python front door never scrubs `os.environ` at the routing seam. That scrub blinds `bg_create`'s floor decision. The compiled client is reachable without the front door (`fno-agents spawn`, the loop runtime). Its own spawn arms scrub the child env and float the same floor (`crates/fno-agents/src/model_env_scrub.rs`). A real route is never stripped. A foreign base URL serves those model ids, so the predicate returns empty. When the daemon itself pre-warms a spare session, a spawn-time scrub cannot reach it: the process already exists. The settings pin covers that case. The SessionStart detector (`hooks/attest-model.sh`) warns over the same five vars. It bails out on Bedrock/Vertex lanes. A parity test pins its var list to `MODEL_ENV_KEYS`. A new tier cannot land in one list and not the other.

## Scope and deferrals

Wires native per-spawn routing for the claude lane (Anthropic-protocol providers) with the fail-safe fallback and the hard guard. `extra_env` is the escape hatch for differentiated tiers (e.g. a cheaper `ANTHROPIC_DEFAULT_HAIKU_MODEL`). Deferred: a codex/openai lane that consumes the same provider registry over the OpenAI-protocol endpoints. Also deferred: an external router for in-session subagent routing to a non-Anthropic provider. Also deferred: a config UI for editing roles (hand-edit is acceptable first). `consolidate` is already served out-of-repo by modelkit/memdream, which calls z.ai directly.

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
