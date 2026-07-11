---
name: agent
description: "Natural-language remote control for the `fno agents` worker mesh. Describe the outcome you want - spawn or hand off work, open an interactive discussion, message or inspect workers, drive a session, or stop it - and this skill resolves and runs the provider- and substrate-specific CLI command. It normalizes arguments, applies confirmation policy, and reports genuine receipts. Use when: 'spawn a worker for ab-XXXX', 'hand off this doc to codex', 'discuss the retry design with gemini', 'show my running agents', 'drive the reviewer', 'stop the billing worker'."
argument-hint: "<verb> [args]  |  [ask|handoff|discuss] <ab-xxxxxxxx | feature | doc-path | /command> [provider] [drive] [yolo] [model <name>] [as <name>] [merge]"
metadata:
  internal: false
requires:
  binaries:
    - "fno >= 0.1"
    - "jq >= 1.6"
---

# Agents

**Describe the agent outcome you want; this skill remembers the command.**

`/fno:agent` is the natural-language front door over the shipped `fno agents`
mesh. The CLI is intentionally precise, but its provider, substrate, lifecycle,
and receipt shapes are too much syntax to keep in working memory. State the
intent in ordinary language; the skill selects the real verb and arguments.

**You (the agent) are the runner.** Read the user's desired outcome, route it to
the right verb, normalize it, confirm only costed or destructive operations, run
the **genuine** `fno agents` command, and report the **real** captured receipt -
never a fabricated one.

This skill REUSES the shipped `fno agents` primitives. It does not reimplement
spawn, the addressed bus, or session observation. Its value is verb routing +
input normalization + confirmation policy + honest reporting. Smart-quote and
dashless parsing also keep it reliable from remote or runner-less controls.

`SKILL_DIR` below is `skills/agent` inside this plugin.

## Verb router

The first whitespace token of the argument is the **verb**. An unrecognized
leading token means the whole argument is a `spawn` task (this preserves the
bare `/agents <task>` ergonomics the former dispatch front door had) - but see
**bare-input announce** in the spawn flow: when bare input is NOT feature-shaped
(a path, a question, a continue/handoff phrasing), the skill surfaces the
`/target` build wrap and offers `handoff`/`discuss` before launching. The
focused core:

| Verb | Envelope | Routes to | Cost |
|------|----------|-----------|------|
| `spawn` (default) | normalize + honest-receipt (no confirm: free lane) | `fno agents spawn` - substrate axis (x-2c27): default `pane` (owned-PTY drivable); trailing `bg` -> detached `claude --bg` thread; trailing `headless` -> one-shot (`claude -p` / `codex --exec` / `agy -p`) | free (claude subscription) |
| `handoff <doc>` | normalize `--handoff` + honest-receipt (free lane) | `fno agents spawn` (Claude/Codex/Gemini continuation seed, NO `/target`; default `pane`) | free (provider subscription) |
| `discuss [seed]` | normalize `--discuss` + honest-receipt (free lane) | `fno agents spawn` (Claude/Codex/Gemini interactive pane, verbatim seed, NO `/target`) | free (provider subscription) |
| `send <name> "..."` | normalize recipient + addressed write | `fno mail send` (the addressed jsonl bus, sender-excluded) | free |
| `chat A B "..."` | normalize + **always-confirm** + honest-receipt | `fno agents chat` (stream-json adopt + switchboard relay) | **Agent SDK plan credit / turn** |
| `watch <name>` | thin pass-through | `fno agents watch` | free |
| `list` | thin pass-through | `fno agents list` | free |
| `whoami` | thin pass-through (read-only) | `fno agents whoami` | free |
| `logs <name>` | thin pass-through | `fno agents logs` | free |
| `stop <name>` | confirm (destructive) + pass-through | `fno agents stop` | free |

Advanced lifecycle intents (`drive`/`grid`/`attach`/`resume`/`reconcile`/`rm`/
`ack`/`promote`) map to their corresponding raw `fno agents` verbs; the skill
resolves their arguments and runs them rather than reimplementing them. The
capability matrix at `docs/provider-command-matrix.md` remains the per-provider
truth. For the full surface map - which verbs are human-facing vs
machine-internal vs exploratory channel infra - see
[references/fno-agents-surface.md](references/fno-agents-surface.md).

> `chat` is the only costed verb: it opens a live real-time channel and every
> hop spends Agent SDK plan credit (isolated from your interactive subscription),
> so it ALWAYS confirms. The default free claude<->claude channel is still the
> addressed bus drained at loop boundaries (`send`), not a live tail - reach for
> `chat` only when you want two workers conversing in real time right now.

Route on the verb, then run the matching section below.

---

## `spawn` (default verb) - launch a background worker

Build dispatch for all three providers. Strip a leading `spawn` verb if present;
otherwise the whole argument is the spawn task.

### Providers (all three are first-class)

| Provider | Build dispatch | Worker | Receipt |
|---|---|---|---|
| `claude` | `fno agents spawn` (default `pane` owned-PTY; `bg` -> `claude --bg`; `headless` -> `claude -p`) | owned pane, or backgrounded `/target` thread (`bg`) | compact JSON `.short_id` (reply on `headless`) |
| `codex` | `fno agents spawn` (exec) / `fno agents host` (`-i`); `headless` -> `codex --exec` | daemon-managed PTY worker | pretty JSON `.short_id` |
| `gemini` | `fno agents spawn` (exec) / `fno agents host` (`-i`); `headless` -> `agy -p` | daemon-managed PTY worker | pretty JSON `.short_id` |

All three create via `spawn` (Group 1 ab-8b3e4fe0: `ask` never creates - it
messages EXISTING peers only). The substrate axis (x-2c27) selects the host:
`pane` (default, owned-PTY drivable), `bg` (claude-only detached `claude --bg`
thread), `headless` (one-shot `claude -p` / `codex --exec` / `agy -p`). `bg` on a
non-claude provider is a hard error pointing to `headless`. Ask-mode
(one-shot Q&A) runs `spawn --once` for codex/gemini. A codex/gemini exec worker
is a **single autonomous pass**, not the claude "refuse to stop until shipped"
loop - do not imply loop-grade completion guarantees for them.

Every `spawn` captures (best-effort, non-blocking) the worker's full resume UUID
into the registry, distinct from the 8-hex short-id, so a worker is a complete,
identified citizen of the mesh that can later be addressed or escalated.

### Inputs (dashless grammar - phone-first)

The documented grammar carries **zero dash tokens** so a phone keyboard's smart
punctuation (which rewrites `--` into a long dash and mangles a lone `-`) can
never corrupt it. The user types barewords or natural language; you map them to
the structured fields. The canonical form:

```
[ask] <task> [provider] [drive] [yolo] [model <name>] [as <name>] [merge]
```

The angle brackets are placeholder notation, not literal syntax. Infer the
fields from the user's unquoted text (a free-form phrasing maps the same way -
see below):

- **task** (required): a backlog node id (`ab-XXXXXXXX`), a free-form feature
  description, or an explicit slash command (`/target ...`, `/pr check 42`).
- **`ask`** (alias `bare`, optional, LEADING verb after `spawn`): ask-mode -
  send the prompt verbatim, no `/target` wrap. Strip it before normalizing
  (pass `--ask`). "just ask" / "one-shot" / "quick question" map here.
- **provider** (optional, trailing bareword): any non-claude harness in the
  supported set - today `codex`, `gemini`, `agy`, `opencode`, with more coming.
  `spawn ab-X codex` -> provider `codex`; `spawn ab-X opencode` -> `opencode`.
  Default resolves from config -> `claude`. "on codex" / "with gemini" map here.
  The set is not hardcoded here: normalize matches any bareword in its
  `VALID_PROVIDERS` (which mirrors the Rust `KNOWN_PROVIDERS` source of truth),
  so a new harness needs no grammar edit. `docs/provider-command-matrix.md` is
  the per-provider capability truth. Quotes protect a trailing word that is
  ambiguously a provider. (megawalk drivers like `hermes`/`openclaw` are a
  different axis, not `spawn` providers.)
- **`drive`** (alias `interactive`, optional): route codex/gemini to a drivable
  `host` session instead of an autonomous `spawn`. No-op for claude. "drive it"
  / "interactive" map here.
- **`yolo`** (alias `auto`, optional): typing `yolo` drops the sandbox for this
  launch - codex runs `--dangerously-bypass-approvals-and-sandbox`, gemini runs
  bare `--yolo` (unsandboxed full-auto). You rarely need it: with NO flag, a
  headless codex/gemini worker is already BOUNDED - sandboxed AND never-prompt
  (codex `--sandbox workspace-write --ask-for-approval never`, gemini
  `--approval-mode yolo --sandbox`) - so it neither hangs nor roams outside the
  workspace. Reach for `yolo` only when you genuinely want no sandbox. Ignored
  for claude. "full auto" / "no sandbox" / "unsandboxed" map here. (To make full
  yolo the standing default for a provider instead of per-launch, set
  `config.agents.<provider>.headless_yolo: true`.)
- **`model <name>`** (optional): exact model for the worker, plumbed to `fno
  agents spawn --model` (each provider's own `--model`). Two-word posture so a
  model name that is not a posture word is read as the value: `spawn ab-X model
  opus`, `spawn ab-X codex model gpt-5`. "on opus" / "use sonnet" map here (name
  the model after `model`). Default = the provider's default. There is NO short
  flag: `-m` is `--allow-merge`, so a bare `-m opus` would set merge, not the
  model - always write `model <name>`.
- **`as <name>`** (optional): explicit agent name. Default is derived
  (`<verb>-<node-id>-<slug>` / `<verb>-<slug>`). "call it X" / "name it X" map here.
- **`merge`** (optional): do NOT inject the no-merge intent. Default injects it
  so a fire-from-phone autonomous worker lands a PR for review, not an
  auto-merge. (`merge` only omits the no-merge intent; true auto-merge stays a
  separate opt-in.) "let it merge" / "can merge" map here.

`normalize.sh` is the deterministic backstop: it recognizes this closed posture
vocabulary as a contiguous TRAILING run (right-anchored), so a posture word that
sits mid-task ("spawn the node that will merge two branches") stays task text and
does NOT silently set the posture. A bare `as` with no name after it is an error.

**Dash-flags still work (undocumented back-compat).** A desktop scripter's
existing `--yolo` / `-n <name>` / `-m` / `--model <name>` / `-i` / `-y`
invocations are canonicalized to the same fields (`-y`/`--yes` is accepted and
ignored - the free lanes no longer confirm, see CONFIRM). `--model` has no short
form (`-m` is `--allow-merge`). A genuinely unknown dash-flag fails loud.
Dashless is the only form you advertise; never instruct a user to type a dash.

### Flow: NORMALIZE -> RESOLVE -> VALIDATE -> CONFIRM -> SPAWN -> REPORT

#### 1. NORMALIZE (deterministic helper)

Run the normalizer with the spawn task text exactly as given (pass it as ONE
argument). You may pass trailing posture barewords inside `--input` and let the
helper extract them, or pass the fields you inferred from natural language as the
internal flags below - the helper resolves both to identical fields, so either
path is safe. ALWAYS pass `--ask` for an `ask`/`bare` (leading) verb, `--handoff`
for a `handoff` verb, or `--discuss` for a `discuss` verb, so the verbatim
prompt/path/seed skips posture parsing.

```bash
bash "${SKILL_DIR}/scripts/normalize.sh" --input "<raw task [posture barewords]>" \
  [--provider <p>] [-n <n>|--name <n>] [-i] [--yolo] [--ask] [--handoff] [--discuss] \
  [-m|--allow-merge] [-y|--yes] [-P <project>|--project <project>] [-f|--force]
```

It strips smart quotes, parses the trailing dashless posture run, canonicalizes a
token-initial em/en-dash to `--`,
derives the agent name (`<verb>-<node-id>-<slug>` for a node, `<verb>-<slug>` for a feature),
resolves the provider (explicit -> `config.providers` via the shipped
`resolve_dispatch_target` -> `claude`), detects the node id, resolves a
`-P`/`--project` target to its work-map `resolved_cwd`, picks the payload
mode, and assembles the provider-aware `message`. Read its `key=value` output
line by line. Never `eval` it. Captured fields: `node`, `node_query`,
`spawn_next`, `next_scope`, `shape_hint`, `name`, `provider`, `mode`, `yolo`,
`yes`, `allow_merge`, `project`, `resolved_cwd`, `payload_mode`, `message`.

**Cross-project target (`-P`/`--project`).** By default a free-text spawn launches
in the caller's cwd. `-P <project>` retargets it: normalize resolves the registry
`name`/`short_name` (`config.work.workspaces.*.projects` in config.toml) to that
project's work-map root and emits `resolved_cwd`, which you pass to
`spawn.sh --cwd` (see SPAWN). A natural-language `in <project>` / `in the <project>
repo` / `as <project>` is YOUR job to translate: when you judge the phrasing is a
project directive (not task prose like "fix the bug in etl"), call normalize with
`-P <project>`; the deterministic layer only ever sees the unambiguous flag.
Unknown project, or a mapped path missing on disk, is a loud `status=error` (no
spawn). A backlog node carries its OWN project, so a node + `-P` is a refused
conflict UNLESS you also pass `-f`/`--force` (flag-win override: run the node's
work in the forced repo). When you re-run normalize after resolving a slug/`next`
to an ab-id (see RESOLVE), carry `-P`/`-f` through so the conflict check fires on
the real node.

- If `status=error`, STOP. Report the `error=` line. Do NOT spawn.
- Otherwise capture every field for RESOLVE/VALIDATE/CONFIRM/SPAWN.

**Payload modes** (chosen deterministically by normalize):

- **build** (default): run the work. claude gets `/target <text>` (+ `no-merge`);
  codex/gemini get a prose build BRIEF - never a literal `/target` string.
- **ask** (`ask`/`bare` verb): a one-shot question. The prompt is sent verbatim.
- **handoff** (`--handoff`): a doc path becomes a continuation seed (read the doc,
  continue from where it left off, do NOT re-derive a plan) + a standing
  guardrail against autonomous outward/irreversible actions. Supports Claude,
  Codex, and Gemini; NO `/target`, NO `no-merge`. See the `handoff` section.
- **discuss** (`--discuss`): a verbatim conversational seed -> a running,
  provider-native interactive pane. Claude/Codex/Gemini, NO `/target`. See the
  `discuss` section.
- **passthrough** (leading `/`): the explicit command, verbatim. claude-only;
  normalize refuses it for codex/gemini.

**`shape_hint`** (`path|question|continue|feature`): a deterministic read of what
KIND of payload this is. Use it ONLY on the bare-input path (no explicit verb) -
see **1a. ANNOUNCE** next. For an explicit `spawn`/`handoff`/`discuss`/`ask` verb,
ignore it (the user already declared intent).

#### 1a. ANNOUNCE (bare-input shape guard - attended only)

When the raw input carried **no explicit leading verb** (it fell through to the
default spawn task) AND `shape_hint != feature` AND the caller is **attended**,
do not silently `/target`-wrap. Announce the wrap and offer the two alternatives,
then act on the reply:

```
Treating this as a build (/target). This looks like a <shape_hint>, not a feature.
Reply `handoff` to continue from it without re-deriving, or `discuss` to open a chat thread. Otherwise I proceed with the build.
```

- Reply `handoff` -> re-run NORMALIZE with `--handoff` and run the `handoff` flow.
- Reply `discuss` -> re-run NORMALIZE with `--discuss` and run the `discuss` flow.
- Any other reply, or no reply, or an **unattended/headless** caller -> proceed
  with the normalized build (never block, never auto-reroute). REPORT still
  echoes the exact `/target ... no-merge` command that ran.

A genuine feature (`shape_hint=feature`) skips the announce entirely - no friction
on the dominant path. The shape classifier is deterministic but coarse; a false
positive costs one extra attended prompt and still defaults to build.

#### 1b. RESOLVE (id-free entry modes -> a concrete ab-id) (ab-f82e8083)

A backlog node id is an opaque `ab-{8hex}`. Four id-free ways to name the work
resolve to a concrete id here, in order. Resolve to ONE `ab-<id>`, then
**re-run normalize** with `--input "<that ab-id>"` (carrying the same
`--provider`/`--name`/posture flags) so `message`/`name`/`node` are rebuilt for
the real node. Skip this whole step when `node` is already a concrete id.

1. **`node` non-empty** (tier 1 exact id, or tier 3 a re-prefixed bare 8-hex) ->
   use it as-is.
2. **`spawn_next=1`** (tier 5) -> `fno backlog next $( [[ "$next_scope" == all ]] && echo --all || true )`
   (it already emits JSON; there is no `-J` flag on `next`). Take `.id`. If it is
   `null`/empty, report **"no ready node"** and launch nothing (AC3-ERR).
   Otherwise that id is the node.
3. **`node_query` non-empty** (tier 2 slug candidate) -> `fno backlog get "$node_query" --field id`.
   On exit 0 that is the node (exact slug -> ab-id). On a non-zero exit the slug
   missed; fall through to describe-it below using `node_query` as the description.
4. **describe-it (tier 4, free prose)** -> the only model-judged tier, and it
   **ALWAYS confirms** regardless of `config.agents.confirm` posture (Locked
   Decision 5: describe-it is carved out of the silent-launch lane). Run
   `fno backlog find "<description>" -J` to get the candidate set, **rank by
   meaning** over title+slug+details, then show the top match as
   `slug (ab-id) - title` plus **2-3 alternatives**, and ask `[y/N]`:
   - If the find errors (graph unreadable / lock) -> report the real error and
     launch nothing (never fabricate a match).
   - Zero candidates -> report **"no node matched '<description>'"**, launch
     nothing (AC2-ERR).
   - Two near-equal candidates -> present a **numbered list** and ask which (or
     none); never auto-pick (AC2-EDGE).
   - On **no** / none -> acknowledge **"launch cancelled"**, create no claim or
     worker (AC2-FR).
   - On **yes** -> that id is the node.

After resolving to an ab-id, re-normalize and continue. A resolution that names
a node in another project will boot the worker in that node's `_resolved_cwd`
(see SPAWN); the REPORT receipt names the resolved `slug (ab-id) + project + cwd`
so even a delegated `next`/`all` is never a silent surprise.

#### 2. VALIDATE (refuse bad input before any billed launch)

- **Node must resolve.** If `node` is non-empty, run `fno backlog get "$node"`.
  If it exits non-zero or returns no `.id`, STOP and tell the user. Do NOT spawn.
- **Collision pre-check (read-only), with a self-handoff exception.** If `node`
  is non-empty, run `fno claim status "node:$node" --json` and inspect `.state`
  and `.holder`. If `live`, decide whether the holder is a FOREIGN worker or
  YOURSELF handing off your own node:
  - **Self-handoff** (holder == your own claim): read your own holder from the
    session manifest - `sed -n 's/^target_claim_holder: *"\?\([^"]*\)"\?/\1/p'
    .fno/target-state.md` (empty if you hold no target claim). If it equals
    `.holder`, this is YOUR node, not a foreign collision - pass `--self
    "<holder>"` to `spawn.sh` so it emits a `self-handoff` receipt instead of a
    confusing `already-running`. `/agent` does NOT reassign the node from here:
    a node claim can be released only by the two sanctioned sites (`handoff.sh`
    or `fno backlog unclaim`, holder-verified - a helper subprocess release is
    an authority violation), and a bg spawn cannot emit the `delegated` event a
    clean takeover needs. For an immediate clean handoff use `/target`'s
    self-handoff (it archives state, emits the delegated event, and releases the
    claim atomically); or run `fno backlog unclaim <node>` and re-dispatch.
  - **Foreign collision** (holder is someone else, or you hold no claim): a
    worker already holds this node - tell the user (point at `fno agents logs
    $name`) and do NOT spawn a second loop.

  (`spawn.sh` re-checks atomically too: it treats a `live-claim` whose holder
  matches `--self` as a self-handoff and refuses any other.)

A free-form / ask payload (`node` empty) skips the node checks.

#### 3. CONFIRM (free lane: no confirm by default)

`spawn` is a **free, reversible lane**: an autonomous worker lands a PR for
REVIEW (it never auto-merges; `merge` only omits the no-merge intent), so there
is nothing billed or destructive to gate. It therefore does **not** confirm by
default. An unattended/headless caller always skips. Otherwise let the helper
compute the decision:

```bash
bash "${SKILL_DIR}/scripts/confirm-decision.sh" \
  --node "$node" --provider "$provider" --mode "$mode" \
  --payload-mode "$payload_mode" --yolo "$yolo" --yes "$yes" \
  --allow-merge "$allow_merge"
```

It emits `confirm_required` (0|1), `caveat` (0|1), `caveat_text`, a `warn` line,
and `reason`. `config.agents.confirm` (`always|auto|never`, model default `auto`)
is repurposed to an **opt-in** "confirm even the free lanes" for a cautious
operator: only `always` confirms; `auto` (default) and `never` skip. A
failed/invalid read degrades to the no-confirm default (the free lane has nothing
to gate) with an `fno update` hint - do not re-derive this; it lives in the
helper. The table it implements:

| Condition | Result |
|---|---|
| posture `auto` (default) or `never` | **no confirm** (free lane) |
| posture `always` (cautious opt-in) | confirm even the free lane |
| `-y`/`--yes` passed | skip (accepted-and-ignored: the free lane already does not confirm) |
| caveat present (codex/gemini **exec** / `yolo` / `merge`) | NOT a confirm - surfaces as a `warn` |
| `ask` payload | never confirm (a one-shot question is not a billed build) |
| unattended caller | skip |
| config read fails / invalid | no confirm (auto) + one staleness warning line |

**When `confirm_required=1`,** show the EXACT command you will run - including the
chosen `spawn|host` verb and `--yolo` when set - plus `caveat_text` when
`caveat=1`, and wait for `[y/N]`. On anything other than yes, STOP without
spawning. **When `confirm_required=0`,** skip straight to SPAWN; if `warn` is
non-empty, print it. The exact command is NOT hidden - REPORT echoes it on every
skip path.

#### 4. SPAWN (genuine execution + honest receipt)

Only after a yes, run the spawn helper with the normalized fields. It re-checks
for a live duplicate, picks the verb from `provider`/`mode`/`payload_mode`, runs
the real `fno agents` command (name POSITIONAL, `--provider`, NEVER `-p`/`--bare`),
and parses the receipt deterministically:

```bash
bash "${SKILL_DIR}/scripts/spawn.sh" --name "$name" --provider "$provider" \
  --message "$message" --mode "$mode" --payload-mode "$payload_mode" \
  [--model "$model"] [--substrate "$substrate"] [--yolo] [--node "$node"] [--self "$self_holder"] [--cwd "<cwd source, see below>"]
```

Pass `--self "$self_holder"` only for a confirmed self-handoff (your
`target_claim_holder` matched the live `.holder` in the collision pre-check); it
makes the atomic re-check emit a `self-handoff` receipt that routes you to the
sanctioned handoff, instead of a confusing foreign `already-running`. It neither
spawns nor releases the claim. Pass
`--model "$model"` only when normalize emitted a non-empty `model`
(spawn.sh forwards it to `fno agents spawn --model`; omit it for the provider
default). Pass `--yolo` only when normalize emitted `yolo=1`. Pass `--substrate "$substrate"`
only when normalize emitted a non-empty `substrate` (`bg` -> a detached `claude
--bg` thread; `headless` -> a one-shot `claude -p` / `codex --exec` / `agy -p`);
an empty `substrate` is the default `pane` (owned-PTY) and the flag is omitted.
Pass `--node` whenever `node` is non-empty. Choose the `--cwd` source in this priority order, so launch cwd
follows the work-map root:

1. normalize's `resolved_cwd` when non-empty (a `-P`/`--project` target, including
   a `-f`/`--force` override of a node's own cwd) - it wins over the node's cwd;
2. else the node's `_resolved_cwd` (from `fno backlog get "$node"`);
3. else the caller's `cwd`.

**Auto-worktree (x-9c4c).** When the payload writes code and the resolved
`--cwd` is a repo's MAIN checkout, `spawn.sh` deterministically creates
`~/conductor/workspaces/<repo>/<name>` on a fresh feature branch and launches the
worker THERE - born isolated, location verdict `ok` from line one, no reliance on
the worker self-creating a worktree. "Writes code" is keyed off `payload_mode`,
not the message text: a `build` dispatch (claude `/target` wrap OR a codex/gemini
`Implement ...` prose brief) and an explicit claude `/target`|`/do`|`/fix`
passthrough all isolate; `ask`/`handoff`/`discuss` and a non-code claude slash
command (`/think` writes a design doc) stay in repo root. An already-isolated
worktree cwd is not re-isolated; any creation error fails safe to repo root. This
is in `spawn.sh` (deterministic), so you do nothing here except relay the receipt
- its `cwd="<worktree>"` field on the launched line surfaces the real launch dir.

#### 5. REPORT (echo ONLY what actually happened)

**Receipt-echo invariant (every skip path).** When the confirm was auto-skipped
or bypassed, the receipt MUST echo the exact `fno agents` command that ran. No
launch path is ever invisible. If `confirm-decision.sh` emitted a `warn`,
include it here too.

**Resolved-handle echo (ab-f82e8083, Locked Decision 5: echo always).** When the
launch carries a node, the receipt MUST lead with the resolved
`slug (ab-id) + project + cwd` - read them from the `fno backlog get "$node"`
JSON (`.slug`, `.id`, `.project`, `._resolved_cwd`), falling back to `(ab-id)`
when the node is not yet slugged. This holds on EVERY exact-tier + `next` path,
including the posture=auto silent-launch lane, so even a delegated `next`/`all`
or a cross-project resolution is visible post-hoc. (describe-it already confirmed
the handle before launch.)

**Cross-project echo on the free-text path.** When the launch carried no node but
normalize emitted a non-empty `resolved_cwd` (a `-P`/`--project` target), the
receipt MUST lead with `project=<project> cwd=<resolved_cwd>` (both already in
hand from normalize - no extra lookup). A free-text cross-project hop is then just
as visible as a node one; no launch silently lands in a different repo.

Read `spawn.sh`'s single outcome line and relay it faithfully:

- `result=launched ... mode=exec ...` -> a background worker launched. Quote the
  **real** `short_id` and give `fno agents logs <name>` + `fno agents trace <name>`.
- `result=launched ... mode=spawn ...` -> an autonomously seeded handoff worker
  launched through the provider's spawn lane. The default Rust substrate is a
  drivable pane; this is not a one-shot reply or a refuse-to-stop loop.
- `result=launched ... mode=discuss ...` -> a seeded interactive discussion is
  running in a provider-native pane. Give `fno agents grid <name>` / `drive
  <name>` for live interaction and `fno mail send` for asynchronous follow-up.
- `result=launched ... mode=interactive ...` -> a drivable session **staged, NOT
  running yet**. Give `fno agents grid <name>` (or `fno agents drive <name>
  --mode interactive`). Note: no proactive push when it waits on input.
- `result=replied ...` (a one-shot ask or explicit headless substrate) -> the one-shot returned its
  answer synchronously; the **reply follows the outcome line** and IS the
  deliverable. Relay it (preview ~15 lines; full reply in `fno agents logs <name>`).
- `result=already-running ...` -> a worker already exists for this node/name; no
  second loop was created. Point at its logs.
- `result=failed reason="<...>"` -> report **FAILED** with the real reason. NEVER
  emit a short-id, fabricate a reply, or claim a worker launched.

### Receipt families (keyed by the verb spawn.sh ran)

`spawn.sh` branches the receipt parse on the verb/mode it ran - never on
sniffing output (Group 1 ab-8b3e4fe0 moved claude creation off `ask` onto
`spawn`, so claude takes the JSON `.short_id` family below, not a bare 8-hex
line):

- **`spawn --once` / `--substrate headless`**: a **client-side one-shot**
  (`codex exec` / `gemini -p`). stdout is the model REPLY verbatim, not a
  short-id. Success = exit 0 AND a non-empty reply; an empty reply (even on
  exit 0) is FAILED, never a fabricated answer.
- **`spawn` / `host`**: stdout is JSON
  carrying `{"short_id",...}` (compact or pretty depending on runtime and
  substrate). `.short_id` is parsed with `jq`; `bg` requires a whole-string
  8-hex id. The default/pane lane accepts the runtime's identifier-shaped
  handle: Rust returns a name-slug `short_id`; Python pane receipts have an
  empty worker-socket id, so `spawn.sh` uses the receipt's registry `name` only
  after matching provider/status and requiring both mux session and pane id.
  Empty, mismatched, or malformed receipts (even on exit 0) are FAILED.

Report only what `spawn.sh` actually captured - a real short-id, or a real reply.
"No valid receipt" is FAILED, full stop.

### Durable node-claim limitation (codex/gemini builds)

A claude `/target` build worker acquires the durable `node:<id>` claim itself. A
codex/gemini build runs through `fno agents spawn`, which does NOT run the
`/target` harness, so the worker does not self-acquire `node:<id>` today. The
dispatcher's `dispatch:<node>` reservation only covers the ~3-minute boot window;
after it expires, a second dispatch of the same node under a **different**
`--name` can double-launch. This is a provider-integration limitation tracked as
a follow-up. Re-dispatching the same node with the same derived name is always
safe (the registry same-name guard catches it).

---

## `handoff <doc>` - continue a doc without re-deriving

`handoff` exists because `build` (`/target`) is the wrong frame for a handoff. A
`/target` worker re-derives think->plan->do and loops until a PR is green - but a
handoff document already IS the plan, and a handoff is often multi-thread,
mostly-non-code continuation work that never produces a single green PR. So
`handoff` spawns a **plain autonomous worker** on Claude, Codex, or Gemini (no
`/target`, no loop-grade "refuse to stop" guarantee) seeded to read the doc and
continue from where it left off. The default substrate is the owned-PTY `pane`;
the worker starts autonomously and can later be driven through the provider's
supported pane tools. A handoff that is really a feature build still goes
through `build`.

It also injects a **standing guardrail**: the seed bars the worker from
autonomously taking outward-facing or irreversible actions (emails, deploys,
merges, publishing, contacting third parties) and tells it to STOP and surface
them via `<help reason="outward-action" evidence="...">` for human confirmation.
This is **prompt-level** enforcement in v1 (the model obeying the seed), observed
through provider-supported logs or pane tools; a harness-level tool gate is a
deferred follow-up. Say so honestly - do not imply the worker is sandboxed from
outward actions.

### Flow: NORMALIZE -> VALIDATE -> SPAWN -> REPORT (no confirm: free lane)

1. **NORMALIZE.** Strip the leading `handoff` verb; pass the rest as the doc path.
   Run `normalize.sh --input "<doc-path>" --handoff` (carry `--provider`/`as
   <name>`/`model <name>`/`yolo` only if given). Provider resolution is
   explicit -> configured -> Claude, constrained to Claude/Codex/Gemini. It emits
   `payload_mode=handoff` and a `message`
   that is the continuation seed (path + "do not re-derive" + GUARDRAIL +
   PR-for-review). On `status=error` (empty path or explicit unsupported
   provider), STOP and report the `error=` line.
2. **VALIDATE the doc path (best-effort).** If the path is **absolute** and does
   not exist, STOP and report the real missing path - never boot a worker pointed
   at nothing (AC5-ERR). If it is **relative**, you cannot reliably check it here
   (the worker's cwd may differ); proceed but note in REPORT that it must resolve
   at the worker's cwd, and prefer an absolute path.
3. **SPAWN.** Run the genuine autonomous wire with the seed verbatim:

   ```bash
   bash "${SKILL_DIR}/scripts/spawn.sh" --name "$name" --provider "$provider" \
     --message "$message" --mode exec --payload-mode handoff [--cwd "<cwd>"] \
     [--model "$model"] [--yolo]
   ```

4. **REPORT** the real receipt exactly as the `spawn` section's REPORT does
   (`result=launched ... mode=spawn` -> quote the real `short_id` and always give
   `fno agents logs <name>` plus `grid`/`drive` for the default pane;
   `result=failed` -> FAILED with the real reason, no fabricated short-id). Note
   it is an autonomously seeded, drivable continuation worker, not a
   refuse-to-stop loop, and that the outward-action guardrail is prompt-level.

---

## `discuss [seed]` - open a provider-native interactive discussion

`discuss` is a regular interactive Claude, Codex, or Gemini session seeded with
the user's words verbatim. It launches on the default owned-PTY pane, appears in
`fno agents list` / the grid, and remains drivable after the opening turn. No
`/target`, no build framing, and no cost beyond the provider subscription. Use
it when you want to talk, not build.

The seed is the **opening turn**, sent verbatim. Continue live through `fno agents
grid <name>` / `fno agents drive <name>` or send an asynchronous follow-up over
the bus. v1 requires a non-empty seed; a bare `discuss` with nothing to say is a
loud error rather than an idle thread.

### Flow: NORMALIZE -> SPAWN -> REPORT (no confirm: free lane)

1. **NORMALIZE.** Strip the leading `discuss` verb; pass the rest as the seed. Run
   `normalize.sh --input "<seed>" --discuss` (carry provider/name/model posture
   when supplied). Provider resolution is explicit -> configured -> Claude,
   constrained to Claude/Codex/Gemini. It emits `payload_mode=discuss` and
   `message` = the seed verbatim (a seed beginning with `/` remains chat text,
   never a passthrough command). On `status=error`, STOP and report it.
2. **SPAWN.** Run the genuine pane wire with the seed as the first turn:

   ```bash
   bash "${SKILL_DIR}/scripts/spawn.sh" --name "$name" --provider "$provider" \
     --message "$message" --mode exec --payload-mode discuss [--cwd "<cwd>"]
   ```

3. **REPORT** the real `mode=discuss` receipt. Point at `fno agents grid <name>` /
   `drive <name>` for live interaction, or `fno mail send <name> "<reply>"` for
   an asynchronous follow-up.

---

## `send <name> "<message>"` - message a peer over the bus

> Prefer the dedicated **`/mail`** skill for messaging (send / reply / unread /
> ack) - it is the runner-less front door over `fno mail`. This `send` verb stays
> here as a convenience so a spawn-and-message flow does not need a second skill.

Addressed delivery on the shipped jsonl bus (the cv-d54ddd45 fix): the message
is appended addressed-by-name and drained at the recipient's next loop boundary,
with the sender excluded. This is the default free claude<->claude channel.
`send` is NOT a live tail - delivery happens at the recipient's boundary, not
instantly.

1. Parse the recipient name (first token after `send`) and the body (the rest;
   strip smart quotes the same way normalize does for spawn).
2. Refuse an empty recipient or empty body before writing anything.
3. Run the genuine wire (reuse `fno mail send`; do NOT reimplement the bus):

   ```bash
   fno mail send "<name>" "<message>"
   # cross-project broadcast instead of a single peer:
   #   fno mail send --to-project "<project>" "<message>"
   ```

4. **REPORT** the real outcome line. `fno mail send` prints exactly one line
   (`msg-<id> delivered (hosted)` or `msg-<id> queued (durable)`); relay the
   message id and the resolved recipient so delivery is auditable. Exit 0 covers
   both delivered and queued (a not-currently-live recipient is queued durably
   and drained later - that is success, not an error).
   - **Unknown name** (`fno mail send` exits 16): report "unknown agent
     <name>" and that nothing was written. Do NOT guess a recipient.
   - Any other nonzero exit: report FAILED with the captured stderr; never
     report a phantom delivery.

`send` is free and never confirms (it is not a billed launch).

---

## `chat A B "<seed>"` - live escalation (costed, always-confirm)

The one billed verb. Where `send` drops an addressed message the peer drains at
its next loop boundary, `chat` opens a **live real-time channel** between two
claude workers: it adopts BOTH onto the shipped stream-json switchboard lane and
drives a bounded A<->B relay right now. Every hop spends Agent SDK plan credit
(isolated from your interactive subscription), so `chat` ALWAYS confirms - even
when `config.agents.confirm` is off. v1 is **claude<->claude only**.

This is a thin mouth over the shipped substrate (epic ab-d3a1ae3e). `chat`
DRIVES the daemon (adopt + switchboard); it never reimplements the lane.

1. Parse the two peer names (first two tokens after `chat`) and the seed (the
   rest; strip smart quotes the same way normalize does for spawn). Refuse if
   either name or the seed is empty - launch nothing.
2. **CONFIRM (always).** Show the exact command and the plan-credit caveat, then
   gate on `[y/N]` regardless of confirm posture. The `fno agents chat` command
   echoes both before its own gate, so a plain pass-through is honest; pass
   `-y/--yes` only when the user already confirmed.
3. Run the genuine wire (do NOT reimplement adopt or the switchboard):

   ```bash
   fno agents chat "<A>" "<B>" "<seed>"        # confirms, then runs
   fno agents chat "<A>" "<B>" "<seed>" --yes  # user already confirmed
   ```

   Each peer is adopted under a FRESH host name (`<peer>-chat`), because the
   daemon refuses adopting under a name already in the registry and claude has no
   fresh stream host - the resume keys on the peer's full session UUID, so the
   adopted host IS the peer's conversation, resumed.

4. **REPORT** the real terminal-state line `fno agents chat` prints:
   - **ok:** `chat A<->B: <turns>/<ceiling> turns over [A-chat, B-chat] (observe: fno agents watch B-chat)`.
     The channel is headless - follow it with `watch`, never a TUI.
   - **refused** (exit 1): a peer is a busy running `--bg` loop - relay the
     "<X> is busy (running loop)" reason; nothing was adopted.
   - **failed** (exit 1): unknown peer, a peer with no resolved session UUID (it
     cannot be live-escalated - re-spawn to capture the UUID, or use `send`), a
     dead `--resume` adopt child, or an undelivered seed - relay the captured
     reason; never report a phantom channel.
   - Notes on stderr (`note: ...`): a reached turn ceiling ends the relay; an
     aborted mid-adopt unwinds the already-adopted side and says so honestly (a
     stop the daemon could not confirm is reported as "may still be live", never
     asserted torn down).

The relay is bounded by `config.agents.a2a.turn_ceiling` (default 6); with
`config.agents.a2a.auto=false` it is a single mirrored hop with no autonomous
relay. Observe a running channel with `watch`, since a stream-json thread is
headless.

---

## `watch` / `list` / `whoami` / `logs` - observe (thin pass-through)

Run the raw verb and relay its output faithfully. These are reads; no confirm.

```bash
fno agents watch "<name>"     # follow a worker's output (claude-only)
fno agents list               # the mesh: live + registered sessions
fno agents whoami             # THIS worker's own registered name (+ enrichment)
fno agents logs "<name>"      # the worker's transcript
```

`whoami` answers "what is MY registered name" - the handle peers use to address
you via `fno mail send <name>`. It reads `FNO_AGENT_SELF` (the env the spawn path
injects), so a worker that lost track of its name after compaction has a
`fno`-native answer. Exit 3 (`not a registered mesh agent`) for a human /
top-level session. Distinct from top-level `fno whoami`, which reports operating
context (fleet -> walker -> session), not the mesh name.

For a stream-json (headless) thread there is no TUI - observe via `watch`/`logs`,
never a terminal. If the raw verb errors, relay the real error; do not invent
output.

---

## `stop <name>` - terminate (confirm, destructive)

Stopping a worker is destructive (it kills a running session), so confirm first
unless the caller is unattended/headless or passed `-y`/`--yes`:

```
About to stop a running worker:

    fno agents stop <name>

This terminates the session. Proceed? [y/N]
```

On yes, run `fno agents stop "<name>"` and relay the outcome. On anything else,
STOP without stopping the worker.

---

## Hard rules (non-negotiable)

1. **Never fabricate a receipt.** Report ONLY a short-id / message-id / reply
   that the helper or `fno agents` command actually captured. "No receipt" is
   FAILED. This is the cardinal guard.
2. **claude stays on the subscription lane.** claude spawns via
   `fno agents spawn --provider claude` (-> client-side `claude --bg --name`).
   NEVER `-p` / `--bare`, and NEVER route claude through `host`.
3. **The spawn confirm posture is `config.agents.confirm` (default `auto`), via
   `confirm-decision.sh`.** Auto-skip is node-id-only and caveat-free; a caveat
   always confirms under `auto`; `-y`/`--yes` and unattended callers skip; a
   failed read degrades to `always`. Every skip path still echoes the exact
   command. `send`/reads never confirm; `stop` always confirms (destructive).
4. **Sandboxed by default.** Append `--yolo` only when the user explicitly passed
   it; never infer it from the payload or provider.
5. **Do not reinvent provider routing or the bus.** Provider resolution lives in
   `normalize.sh`; addressed delivery lives in `fno mail send`. This skill
   routes verbs and reports honestly; it does not duplicate that machinery.

## Multi-CLI

This skill is Claude-Code primary. It needs `fno agents` (the daemon for
codex/gemini `spawn`/`host` and the bus) and `claude --bg` (the claude
subscription-lane spawn). On a CLI without them, the helpers report the failure
and the node stays re-dispatchable - they degrade honestly, never fake a launch.
See [docs/SKILL-COMPAT-MATRIX.md](../../docs/SKILL-COMPAT-MATRIX.md).

## Observability boundary (knowing a worker's state)

- **exec (default):** codex/gemini never surface a "waiting" state in the exec
  lane. When an action needs approval, codex auto-rejects it and continues;
  gemini aborts the run. Watch via `fno agents list` (status -> `exited`) and
  `fno agents logs <name>`.
- **interactive (`-i`):** the TUI genuinely waits at approval prompts. See it via
  `fno agents grid <name>` / `fno agents drive <name> --mode interactive`.
- **no proactive push** fires for a codex/gemini worker today; a claude `--bg`
  `/target` worker does fire `fno notify` when it stalls.
