# Provider Rotation Substrate

Reference doc for `fno config accounts` (Spec 1 of 4 in the provider rotation plan).

This substrate manages provider records, credential staging, and dispatch-env
construction. It does NOT automate rotation, failover, or mid-session swapping.
See [What this substrate does NOT do](#what-this-substrate-does-not-do).

---

## The four axes

Four different things used to share the word `provider`.
They are orthogonal, and no code path may infer which one is meant from a value: `opencode` is a legal member of both the harness set and the provider set, so only position disambiguates.

| Axis | Values | Where it lives |
|------|--------|----------------|
| **harness** | `claude`, `codex`, `opencode`, `agy`, `trae`, `openclaw`, `hermes` | `--harness/-H`; each record's `harness` field |
| **provider** | `anthropic`, `openai`, `zai`, `moonshot`, ... | `--provider/-P`, `--route`; `config.model_routing.providers` |
| **model** | per-provider model names | `--model/-m` |
| **account** | `readyrule`, `makers`, ... | `config.accounts.records`; `fno config accounts` |

`provider` is a live axis naming the model **vendor**, and it keeps the word.
What moved off it is the **account** axis: named, working instances of a harness, which is what this document's records are.
Two accounts can name the same harness and the same vendor and still be different billing identities, which is exactly the distinction the old name hid.

## Concepts

**Account record** - A named configuration entry in `config.toml` describing
one CLI account: which harness binary to use (`claude`, `codex`, etc.), how
to authenticate (`oauth_dir`, `api_key`, or `managed`), and where credentials
live on disk. The defining property is that the CLI tool *works*, not that
there is a login per se.

**Account** - The human-meaningful label for a subscription or API key
(`account_id`). Defaults to the record `id` when not set.

**Staged provider** - A provider whose credentials have been materialised into
`~/.fno/providers/<id>/` as a directory or symlink. Staging is required
before `dispatch_env()` returns a usable env dict.

**Dispatch env** - A dict of environment variables (`{"CLAUDE_CONFIG_DIR": ...}`
or `{"HOME": ...}`) that, when merged into a subprocess's env, points the CLI
at the correct credentials directory.

---

## Schema reference

Account records live under the top-level `[accounts]` table in `config.toml`. A pre-rename `[providers]` table is still read; the next account write migrates the file.

```toml
[accounts]
active = "claude-max-secondary"     # id of the active provider (optional)

[[accounts.records]]
id = "claude-max-secondary"
name = "Secondary Claude Max"
harness = "claude"
auth = "oauth_dir"
credentials_source = "/Users/me/.claude.secondary"
priority = 100
account_id = "account-secondary"
tags = ["secondary", "max"]
description = "Personal secondary subscription"
```

### Field reference

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `id` | string | yes | - | Unique identifier. Pattern: `[a-z][a-z0-9-]{0,63}`. |
| `name` | string | yes | defaults to `id` | Human-readable label. |
| `harness` | enum | yes | - | `claude` \| `codex` \| `opencode` \| `agy` \| `trae` \| `openclaw` \| `hermes` \| `gemini` (pre-rename name: `cli`, still read) |
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

```toml
[agents.a2a]
auto = true            # A2A relay toggle (default true)
turn_ceiling = 6       # hard cap on total A<->B turns per exchange (>= 1)
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

1. `.fno/config.toml` (project, committed or gitignored)
2. `~/.fno/config.toml` (global, user-wide)

`fno config accounts add --scope project` writes to the project file.
`fno config accounts add --scope global` writes to the global file.
`fno config accounts use` defaults to `--scope project`.

---

## Auth strategies

### oauth_dir

Use for `claude`, `gemini`, and other CLIs whose credentials are stored as
files on disk (OAuth tokens, session cookies).

```toml
auth = "oauth_dir"
credentials_source = "/Users/me/.claude.secondary"
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

```toml
auth = "api_key"
env = { ANTHROPIC_API_KEY = "${KEYCHAIN:my-anthropic-key}" }
```

Staging creates an empty marker directory (`~/.fno/providers/<id>/`);
no symlinks. `dispatch_env()` resolves all `${...}` references and returns the
resolved dict.

### managed

Use for multiple accounts that must share one CLI credential slot. Registering
an account snapshots its credentials under `~/.fno/providers/<id>/`; `fno
providers use <id>` captures the outgoing account, materializes the selected
snapshot, verifies it, and only then marks the slot active. A live CLI process
that is using the shared slot defers the switch instead of rotating credentials
under that process.

Codex switches add a native local-schema check after materialization: Footnote
runs `codex login status` with the slot's exact `CODEX_HOME`. Exit zero means
Codex recognized the stored auth. Any completed nonzero result rejects the
switch and attempts to restore the outgoing credentials. If a Codex process
started during the check, rollback is withheld rather than rewriting its live
credential slot, and the receipt says the slot may still hold the selected
account. Footnote clears the active-slot stamp in that case so a later retry
cannot misattribute and overwrite another account's saved credentials. A missing
Codex binary or a five-second timeout falls back to structural verification and
is disclosed as a weaker guarantee in the command receipt. This check does not
contact the service or prove that a remotely revoked token is still valid.
Claude managed switches retain structural verification only.

If verification is interrupted, Footnote attempts rollback before propagating
Ctrl-C. When rollback cannot restore a known outgoing slot, the CLI prints the
bounded indeterminate-state receipt before exiting with the interrupt status.

---

## Quota survival: `config_dir` accounts, `pick`, and `doctor`

No in-session credential swap is possible.
A `claude` process reads `CLAUDE_CONFIG_DIR` once at launch, so every account switch happens at a process boundary, and the only useful moment to choose is just before one starts.

### The prerequisite: an account that participates in quota survival needs its own `config_dir`

```bash
fno config accounts register readyrule --config-dir ~/.claude-alt
fno config accounts register makers    --config-dir ~/.claude
```

A `config_dir` record is the only shape where all three of these hold at once: the usage probe can read that account's own credentials, a worker can be pinned to it while a different account is active, and two workers can run on two accounts concurrently.
A shared-slot `managed` account remains fully supported for single-account operation; it simply cannot be a picker candidate, because it reaches a worker only through the daemon-wide `~/.claude` slot.

On a fresh machine this costs one `claude /login` per account, each in its own dir.
It is an operator setup step, not configuration.

### `fno config accounts pick`

```
fno config accounts pick [--combo <name>] [--exclude <id>...] [--json] [--print-env]
```

Prints the account to launch on: the first candidate in combo order that still has headroom and that footnote can actually pin a worker to.
stdout is the bare account id (or the JSON verdict); stderr always carries the reason and every candidate's headroom, so the receipt is readable whether or not the pick succeeded.

| exit | meaning |
|------|---------|
| 0 | an account was chosen; its id is on stdout |
| 3 | every launchable candidate is `EXHAUSTED` |
| 4 | there is no launchable candidate at all - a setup problem, not a quota one |
| 5 | picking is switched off (only with `--if-armed`, see below) |

The distinction between 3 and 4 is load-bearing.
Exit 4 names each record and tells you to register one with `--config-dir`; collapsing it into 3 would report a setup gap as a quota condition.

Selection introduces no new ranking: it is the existing headroom-ordered walk over the active combo, with `EXHAUSTED` skipped and `UNKNOWN` treated as healthy.
Combo order is how you express preference, exactly as elsewhere.
Probing goes through the standard TTL cache, so repeated calls inside `probe_ttl_seconds` cost nothing.

`--print-env` emits the picked account's **complete** env overlay, one `KEY=VALUE` per line, for a shell alias or `claude --settings` wrapper you own:

```
ANTHROPIC_API_KEY=
CLAUDE_CODE_OAUTH_TOKEN=
ANTHROPIC_BASE_URL=
...
CLAUDE_CONFIG_DIR=/Users/you/.claude-alt
```

An empty value means **clear this variable**.
Pinning `CLAUDE_CONFIG_DIR` alone is only half an overlay: an inherited `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, or routed `ANTHROPIC_BASE_URL` outranks it, so the worker would authenticate or bill through a different route while the receipt named the picked account.
Every Python spawn substrate already scrubs that list before applying an overlay; emitting it here is what lets a non-Python caller apply the whole thing from one source of truth.

`--if-armed` makes the verb itself honor `pick_on_launch`, declining with exit 5 when it is false.
It exists for callers that must respect the opt-in but cannot read config themselves (the Rust loop dispatcher), so the knob keeps exactly one implementation.
A bare `fno config accounts pick` you type by hand always answers: you asked.

Footnote does not own your interactive `claude` invocation and does not pretend to; you relaunch.

### `providers.quota.pick_on_launch`

```toml
[providers.quota]
pick_on_launch = true
```

When true, a spawn with no explicit `--account` consults the picker and launches on an account with headroom.
Defaults **false**: picking changes which account gets billed without being asked, so it is armed deliberately.
The external loop honors the same knob, via `pick --if-armed`.

Two spawns are never picked for, whatever the knob says:

- one that passed an explicit `--account` - it wins and is never second-guessed
- one that passed `--route` or `--role` - `fno agents spawn` already refuses `--account` alongside either, because the route's `ANTHROPIC_*` overrides the account's `CLAUDE_CONFIG_DIR` and silently mis-bills. Auto-picking would reassemble exactly that combination behind the refusal.

### `fno config accounts doctor`

One read-only verb reporting the store's real condition, exiting non-zero when anything is wrong:

- a record whose stored credential duplicates another's (two ids, one account)
- a stored credential past its expiry
- a `config_dir` that is missing or holds no login
- a tainted slot
- a shared-slot account with no proven identity bound (why its usage reads `unknown`)
- a stamp the live slot credential contradicts (an out-of-band `/login`)

`register` refuses a duplicate credential up front, but that guard is register-time only.
`doctor` is the surface that reports stores predating it, and the verb that confirms a `--config-dir` conversion took.
It stays read-only: for the one finding it cannot merely report, it names `fno config accounts reconcile-slot <harness>` as the repair.

### `fno config accounts reconcile-slot <harness>`

The store assumes footnote performs every slot transition, and two things break that assumption.
A switch that proceeds under live pins marks the slot **tainted**, which makes every usage read for that account `unknown` until the taint clears.
And `claude /login` replaces the shared credential directly, telling footnote nothing, so the active stamp keeps naming whoever was there before.

`reconcile-slot` closes both by treating the live credential as the source of truth and the store as a cache.
It reads the slot credential, resolves its principal from the live OAuth profile, and compares that against each registered account's proven identity.
On exactly one match it refreshes that account's snapshot from the live blob, stamps it active, and clears the taint, all under the same mutex a switch takes.

```bash
fno config accounts reconcile-slot claude
```

It is deliberately **not** a `clear-taint` verb, and it will not act on anything short of proof.
An unreachable profile endpoint, a malformed response, a slot `security` could not read, a principal matching no registered account, or one matching two of them each leave the stamp, the snapshots, and the taint byte-identical, and exit non-zero naming which of those happened.
Clearing a taint without proof would file one account's usage under another's name, which is worse than staying `unknown`.

It also refuses with `slot-pinned` while a session that was pinning when the taint was written is still alive.
Proving the principal proves it now, and that session was launched under the outgoing account, so its next token refresh can overwrite the slot with that account's credential.
The taint marker records those sessions for exactly this check: a session started after the switch already read the new credential and never blocks a repair, which matters because on the shared slot the pinning session is usually the account being proven.
Each is recorded as a pid *and* its start time, because pids are reused and a recycled one wearing a dead session's number would hold the repair open permanently.
Reading the slot also ignores any ambient `CLAUDE_CONFIG_DIR`, since a worker pinned to another account exports it and reconciliation would otherwise prove the pinned account and stamp it onto the canonical slot.
On macOS the shared slot has two Keychain items, scoped and unscoped, and a stale one can sit beside a live one holding a different account.
The on-disk `~/.claude/.credentials.json` is a third source: the usage probe reads it first even on macOS, so a stale file bearer could otherwise prove out and have its quota reported while the Keychain account occupies the slot.
Reconciliation resolves both and refuses with `ambiguous-slot` when they name different accounts: that is not a tie to break, because whichever was stamped, some reader would get the other one.
Every distinct candidate must prove, and they must all name one account.
No candidate is set aside, whatever the reason it did not prove: a 401 rejects an access token while its refresh token may still be live, so `claude` can refresh that account straight back into the slot it reads first, and an unanswered call says nothing at all.
The same rule reaches the usage probe, which reports `unknown` for a slot presenting more than one distinct credential however well the bearer it holds proves out, since `claude` reads the scoped Keychain item first while the probe reads the unscoped one.
Capture-before-overwrite reads the same candidates, so the two cannot disagree about which credential belongs to a record; with more than one distinct credential present it captures nothing, since a lost rotated token is recoverable with a login while another account's credential filed under this record is silent.
The pin check runs before the profile call rather than after it, and the slot is re-read and compared before anything is written, so a writer that replaces the credential during that call and then exits refuses with `slot-changed` instead of getting its credential stamped under the proven account's name.
A reconciliation against a store that does not exist yet returns `no-managed-store` without creating it: `matched` is the only outcome allowed to touch disk, and that includes the directory.

A record's principal is bound only at `register`, which is the one moment the operator asserts that the signed-in account IS this record.
`register` reads the slot once, under the same mutex a switch takes, and that single read serves the proof, the snapshot, and the binding.
It refuses outright when the slot holds two different accounts, so it can never bind a stale credential under a new id; proving one read and snapshotting another is exactly how account A's identity would end up bound to account B's credential.
It also refuses when the proven identity already belongs to another record.
The existing duplicate check compares tokens, which rotate, so the same account registered again after a rotation would slip past it and leave two records sharing one quota pool that reconciliation could never tell apart.
It re-checks the slot after resolving the identity, so an out-of-band login during that network round trip refuses with `slot-changed` rather than stamping the account it proved.
The config save happens inside that same lock and before any store write, because store residue from a failed registration is what a later attempt reads as a duplicate credential and refuses; the active stamp comes last.
The save re-reads the record set under the lock, so two concurrent registrations cannot each merge into the same stale set and drop one another.
The slot is tainted for the whole commit and cleared only after a final stable read, so a crash or a Keychain read that fails at the end can never leave an unverified stamp trusted.
A login that landed during the writes leaves the account registered but its stamp tainted, with a warning naming `reconcile-slot`.
Reconciliation commits the same way, which matters most on the out-of-band `/login` path: that one starts with no marker at all, so without a provisional taint there would be no cover at all while it wrote.
Stamping first would leave the stamp naming an unconfigured orphan if the save failed, and every configured shared account unattributable behind it.
The identity compared is the account *and* the organization, because Claude Code usage is organization-scoped and one human can belong to two organizations; an identity missing either half is not comparable and fails closed.
A switch deliberately does not bind, because it materializes the record's stored snapshot and a snapshot's provenance is the store rather than the operator: an earlier out-of-band login plus capture-before-overwrite can leave one account's credential filed under another's id, which is what the `duplicate-credential` finding reports.
An account that has never been registered since this landed has no bound principal, so reconciliation reports `zero-match` and names the live account; sign that account in and re-run `fno config accounts register <id>` to bind it.

A fresh usage probe also checks the stamp it is about to trust.
An out-of-band `/login` leaves a stamp that is wrong and *untainted*, so nothing upstream hesitates and the live account's usage gets filed under the stamped record's name.
When the stamped record's live principal provably belongs to someone else, the probe repairs once and otherwise reports `unknown` rather than a confident wrong number.
Proven principal evidence is cached briefly so this costs no call on the common path.

Shared-slot attribution needs *fresh proof*, so an identity that cannot be proven is refused rather than assumed.
Refusing costs little: the usage endpoint that would consume the attribution shares a host with the profile endpoint, so an outage that hides identity has already taken the measurement with it.
A store registered before principals existed therefore reads `unknown` until you re-register its accounts, which is what binds them; `doctor` reports `unbound-principal` and names the command, so the `unknown` is never silent.

A harness with no principal endpoint is a different case and keeps the stamp-trusting behavior.
Codex can never prove a slot principal, so refusing there would silence its measurement permanently for no gain.
Records with their own `config_dir` are exempt too, since they are attributable without the shared slot at all.

The check runs per credential, immediately before that credential is spent on a usage request.
The probe tries several bearers because a stale scoped Keychain item can 401 while the unscoped one is live, so a check anchored to "the slot" could prove one credential while the request used another - the same misattribution by a longer route.
A bearer that fails the check is skipped rather than measured, so another account's usage is never fetched at all.

Proven evidence is cached against a digest of the credential it was proven about, not just against time.
Time alone would let an out-of-band `/login` inside the TTL reuse evidence about the credential it replaced, making the check built to catch that login the thing that hides it.

A tainted slot self-heals the same way: before a fresh usage probe refuses a tainted managed occupant, it runs the same primitive once, and resumes only if identity was proven.
A refusal is backed off briefly so an unmatchable slot cannot re-hit the endpoint on every probe; only failures are cached, and only as backoff, never as proof.

Two boundaries are worth knowing.
A record with its own `config_dir` is attributable without the shared slot at all, so it never enters reconciliation and taint can never affect it.
And the active shared-slot occupant correctly keeps `config_dir = None`, because interactive `claude` reads `~/.claude`; that is not a defect to fix by giving it a dir.

---

## env-value reference resolution

Values in a record's `env` table support four syntaxes (shown here as the
`env` inline table a `[[accounts.records]]` entry carries):

**`${ENV:VAR_NAME}`** - Reads `VAR_NAME` from the current process environment.
Raises `ProviderUnavailableError` if the variable is not set.

```toml
env = { ANTHROPIC_API_KEY = "${ENV:MY_ANTHROPIC_KEY}" }
```

**`${KEYCHAIN:item}`** - Reads the password from macOS Keychain via
`security find-generic-password -w -s <item>`. Raises `ProviderUnavailableError`
if the item does not exist. macOS only.

```toml
env = { ANTHROPIC_API_KEY = "${KEYCHAIN:anthropic-work-account}" }
```

**`${FILE:/path/to/file}`** - Reads the first line of the file, stripped of
whitespace. Raises `ProviderUnavailableError` if the file cannot be read.

```toml
env = { ANTHROPIC_API_KEY = "${FILE:/run/secrets/anthropic_key}" }
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

## Route survival across a relaunch

A routed claude spawn writes its endpoint, auth token, and tier maps to a
content-addressed `0600` file under `~/.fno/route-settings/<sha16>.json`, and the
worker's registry row records that file's PATH in `route_settings_path` (schema
v12).
The row stores the path only: the file carries a live `ANTHROPIC_AUTH_TOKEN` and
the registry has no `0600` guarantee.

Relaunching a worker means starting a new harness process for it, and the route
comes only from the flags on that new invocation, so without the recorded path a
relaunch comes back on the default Anthropic account.
That failure is expensive precisely because it is not visible: the worker runs,
bills the wrong vendor, and reports nothing.
So the relaunch door reads the recorded path and either re-applies the route or
refuses non-zero naming the file it could not restore.

Which commands are relaunch doors is narrower than it looks, and the distinction
is load-bearing:

| Command | Starts a new process? | Route handling |
|---|---|---|
| `fno agents spawn --resume <uuid>` | yes | restore the recorded route, or refuse (exit 2) |
| `fno agents resume`, dead-row arm (`claude --resume`) | yes | re-apply via `--settings`, or refuse (exit 13) |
| `fno agents resume`, live arm (`claude attach`) | no | nothing to do |
| `fno agents attach` | no | nothing to do |

`fno agents resume` is two arms, and only one of them relaunches.
It probes liveness first: a reachable supervisor gets `claude attach <short_id>`,
which opens a session that is still running ("The session keeps running either
way", `claude attach --help`), so the route lives in that process and no attach
can lose it.
An exited one gets `claude --resume <uuid>`, which starts a new process and is
therefore a genuine relaunch that must carry the route.
That arm lives in Rust (`client_verbs.rs`), not in `resume_cli.py`, because
`resume` is in `RUST_CLIENT_VERBS` and auto-routes to the daemon binary - reading
only the Python path is how you conclude, wrongly, that resume can never lose a
route.

Both doors apply the same usability rule, not just an existence check: a recorded
file that is missing, unparseable, or carries only the auth-scrub floor all
refuse.
claude reads an empty settings value as unset, so a floor-only file selects
nothing and the worker would come back on the default account in silence - the
same outcome as a missing file, so it takes the same refusal.
A check on one door that only tested existence would make the two disagree while
this page calls them equivalent.

The spawn restore resolves its source row by the transcript being resumed, not by
the spawn's name: `spawn other-name --resume <uuid>` relaunches the same
transcript under a fresh row, and the route lives on the old one.

### Why this is claude-only

The recorded artifact is a claude `--settings` JSON, and only claude's route
lives entirely in env vars.
A codex route selects its endpoint through inline `-c` config args
(`model_providers.<name>` plus `model_provider`), and `CodexRoute.env` carries
only the API key.
Recording that env would let a relaunch "restore the route" onto codex's own
default provider while holding the route's key: half a restore, reported as a
whole one, which is worse than no restore at all.
So a non-claude row records nothing and its relaunch behavior is unchanged.
Codex route survival needs an artifact that also carries the config args; that is
not built here.

### Two things the restore deliberately does not replay

The recorded file is the auth-scrub floor (every `SCRUB_AUTH_VARS` entry as an
empty string) with the route written on top.
An empty value means "unset" only to claude reading a settings *file*; a process
environment has no such rule.
So the restore returns the route's own keys and drops the floor, and the relaunch
re-applies the floor when it writes its own settings file.
Replaying it instead would hand the revived worker an `ANTHROPIC_API_KEY=""` that
the original launch never carried.

An explicit `--account` on a revive COMPOSES with the restored route, exactly as
it does with a flag-supplied one: the route wins endpoint, auth, and
model as one unit through the settings file, while the account's
`CLAUDE_CONFIG_DIR` rides the spawn env and selects the per-account daemon.
Nothing refuses the pair - `fno agents spawn` does not either, so a refusal here
would have been this path inventing a rule the rest of the system does not have.

The restored route goes THROUGH `resolve_spawn_route` rather than past it.
That call is the single composition decision, and it is where managed OAuth
refuses a foreign endpoint layered over the default Claude credential slot.
A restored route assigned past it would be the one route in the system exempt
from a guard every other route pays.

The `pick_on_launch` headroom picker skips a `--resume` spawn on both seams
(`_pick_account_at_seam` and `dispatch_spawn`), so any `--account` reaching the
restore is one the operator typed rather than an advisory guess merged into the
route.
It has its own reason to stay out anyway: a transcript lives under the config dir
it was created in, so pointing `CLAUDE_CONFIG_DIR` at a picked account resumes
into a directory where the uuid does not exist.

A restore is announced (`route: restored from <path>`) for the same reason the
`fno agents resume` arm announces its own.
A relaunch that changes destination silently is the failure this whole path
exists to remove; a restore that says nothing is that silence pointed the other
way.

The recorded path answers "what was this worker launched with, so it can be
launched that way again". It never answers "what is this worker running now" -
a recorded value reports the intended route in exactly the case where a fallback
happened, so that question is read from the session transcript instead.

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
`config.toml` and the filesystem but holds no module-level state and
acquires no locks. Safe to call concurrently from a `ThreadPoolExecutor` or
`asyncio` without additional synchronisation.

**Failure modes:**

- `ProviderNotFoundError` (subclass of `KeyError`) - `provider_id` not present
  in `config.accounts.records`. The record was never configured.
- `ProviderUnavailableError` (subclass of `RuntimeError`) - the record exists
  but cannot be used right now. For `oauth_dir`: provider is not staged (call
  `staging.stage(record)` first). For `api_key`: an env reference cannot be
  resolved (missing env var, keychain item, or file).

The distinction matters for callers: `ProviderNotFoundError` is a configuration
error (stop, ask user to run `fno config accounts add`); `ProviderUnavailableError` is
a transient error (staging might fix it).

---

## Migration from cc-switch

The `cc-switch` tool swaps the active account by modifying which OAuth session
Claude Code reads at session start. `fno config accounts` replaces that step with
explicit staging + `CLAUDE_CONFIG_DIR` isolation.

### Recipe 1: Swap accounts before the next session (manual)

```bash
# One-time: register the secondary account
fno config accounts add claude-max-secondary \
    --harness claude --auth oauth_dir \
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
fno config accounts use claude-max-secondary --scope global
```

### Recipe 2: Set up a secondary account from a backup credentials directory

If you keep a credentials backup (e.g., you copied `~/.claude/` to `~/.claude.backup`):

```bash
# Register pointing at the backup dir
fno config accounts add claude-backup \
    --harness claude --auth oauth_dir \
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
  hits a rate limit or returns an error. You must run `fno config accounts use` manually.
- **Per-agent sigma-review routing (Spec 3):** sigma-review subagents can be
  routed to a different coding model (`codex` / `gemini`). The shipped path is
  `config.review.cross_model` / `config.review.agent_harnesses`, resolved by the
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

**config.toml validation failure on `fno config accounts add`**

The `add` command validates the record via Pydantic before writing. Common
causes:

- `id` contains uppercase or spaces. Pattern: `[a-z][a-z0-9-]{0,63}`.
- `auth: oauth_dir` without `--credentials-source`.
- `auth: api_key` with `--env` values that contain no recognised API key name
  (`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENAI_API_KEY`).

Run `fno config accounts show <id>` after adding to verify the stored record matches
intent.

**dispatch_env returns wrong env for the `claude` CLI kind**

For `auth: oauth_dir` + `harness: claude`, `dispatch_env()` returns
`{"CLAUDE_CONFIG_DIR": "..."}`. If the returned dict contains `HOME` instead,
the record was added with a non-`claude` CLI value. Remove and re-add with
`--harness claude`.

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
| `provider_4xx_quota` | YES | HTTP 402, 429; HTTP 200 or a dead-session result whose body contains "rate limit", "quota exceeded", or "usage limit" (the claude CLI's own exhaustion phrasing) |
| `parser_error` | NO | HTTP 200 with unparseable body (OpenRouter envelope mismatch, HTML error page through 200) |
| `unknown` | NO | Anything else - surface to caller |

### Swap Rules

- **Storm-cap.** At most `config.accounts.failover.max_swaps_per_phase`
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

### Exhaustion-triggered auto-switch (multi-account)

When the recovery sweep swaps a rate-limited claude worker onto a second claude
record, the account switch depends on the swapped-to record's `auth` strategy:

- **`oauth_dir`** (two-dir substrate): nothing extra - the record's own config
  dir rides the respawn's `dispatch_env`, so the new worker reads the other
  account's credentials directly.
- **`managed`** (single shared slot): `attempt_swap` itself materializes the candidate into the slot before it writes the routing pointer. It uses the same capture-before-overwrite and live-pin gate as `fno config accounts use`. It refuses the swap outright on a pin or store error, instead of reporting one that never happened.

This closes x-8665. The pointer used to flip while the slot kept the exhausted account's credentials. `list` and dispatch read the new pointer, but every worker kept the old creds.

This materialization is **opt-in**. It is gated by `auto_switch`, on `attempt_swap`'s only production caller, the recovery sweep:

```toml
[accounts]
auto_switch = true   # default false; arms managed-account materialization on auto-switch
```

When `auto_switch` is off (default), the sweep tells `attempt_swap` not to materialize a managed candidate (`materialize_managed=False`). The pointer still flips, today's warn/defer/nudge behavior, unchanged. The slot does not. The sweep degrades to the bounded nudge instead of redispatching onto un-switched (exhausted) credentials.

A caller that omits the argument gets the default: materialize. That default is correct for every `attempt_swap` caller except the recovery sweep, which has its own separate opt-in contract to honor.

A live-pin defer or a store/keychain failure degrades the same way, regardless of the setting. Single-account and `oauth_dir` setups are unaffected by the knob.

### Per-provider Cost Sub-cap

In addition to the existing session cap at
`config.budget.{attended|unattended}.cost_cap_usd`, each provider record
can declare its own per-session ceiling. When the per-provider spend
exceeds the sub-cap, the stop hook trips BLOCKED with axis tagged as
`per_provider`:

```toml
[[accounts.records]]
id = "claude-anthropic"
name = "Claude Anthropic"
harness = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
cost_cap_usd_per_session = 30

[[accounts.records]]
id = "claude-openrouter"
name = "Claude OpenRouter"
harness = "claude"
auth = "api_key"
env = { ANTHROPIC_API_KEY = "${KEYCHAIN:openrouter-key}" }
cost_cap_usd_per_session = 30

[accounts.failover]
max_swaps_per_phase = 5
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
- `fno config accounts list` per-session per-provider spend (deferred to 2.5)

### State Files Owned by Spec 2

| File | Owner | Reset on |
|---|---|---|
| `.fno/turn-attribution.jsonl` | dispatch layer | session boundary (gitignored, re-created per session) |
| `.fno/failover-state.json` | failover controller | phase boundary (detected via phase_id mismatch) |
| `~/.fno/.settings.lock` | atomic_mutate_settings + read_active_provider_atomic | n/a (lock file, content irrelevant) |

---

## Per-agent routing (Spec 3)

> **Shipped path:** the wired cross-model routing for `/review sigma` and
> `fno review` uses `config.review.cross_model` / `config.review.agent_harnesses`
> (see `skills/review/references/sigma.md` -> "Cross-Model Review Routing"), resolved by
> `cli/src/fno/review/provider_resolution.py`. The `config.agents.<name>.provider`
> schema described in the rest of this section is the original Spec-3 design and
> was never wired; prefer the `config.review.*` keys.

Spec 3 lets each sigma-review subagent run on a
different provider, so model blind-spots cancel across reviews. The routing config
is optional and fully back-compatible: agents without a pinned provider fall back to
the global active provider.

### Schema

```toml
[accounts]
active = "claude-anthropic"

[[accounts.records]]
id = "claude-anthropic"
name = "Claude Anthropic"
harness = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"

[[accounts.records]]
id = "gemini-pro-1"
name = "Gemini Pro 1"
harness = "gemini"
auth = "api_key"
env = { GEMINI_API_KEY = "$GEMINI_KEY" }

[[accounts.records]]
id = "glm-zhipu"
name = "GLM Zhipu"
harness = "openclaw"
auth = "api_key"
env = { OPENAI_API_KEY = "$GLM_KEY" }

# Per-agent provider bindings live in the sibling top-level [agents] block.
[agents.code-reviewer]
provider = "claude-anthropic"
[agents.silent-failure-hunter]
provider = "gemini-pro-1"
[agents.type-design-analyzer]
provider = "glm-zhipu"
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

### Migration

Existing `config.toml` files are unaffected. The `config.agents` block is optional;
when absent, every sigma-review subagent uses the global active provider exactly as
before. To opt in, add the block per the schema above. There is no migration step
or backfill required.

### verify_provenance evidence path

The bundled binary's `fno-agents verify-evidence` verb exposes three sub-commands
(`child-promise`, `has-nonclaude`, `receipt`); the per-agent `event` sub-verb and
its `subagent_spawn` / `subagent_complete` pair verifier were removed for cause
(production sigma dispatch goes through the raw Task tool and never reached the
wrapper). See `crates/fno-agents/tests/verify_evidence_parity.rs` for the
contract tests against golden output captured before the bash oracle was deleted.

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

```toml
[accounts]
active = "claude-primary"             # existing
active_combo = "my-stack"             # NEW (optional; set via `fno config accounts combos use`)

[[accounts.records]]
id = "claude-key-a"
name = "Claude Key A"
harness = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
# ... claude-key-b and claude-key-c are more [[accounts.records]] entries, same shape

[accounts.combos.my-stack]           # NEW
strategy = "round_robin"              # or "fallback"
sticky_limit = 3                      # ignored for fallback
providers = ["claude-key-a", "claude-key-b", "claude-key-c"]

[accounts.combos.cheap-only]
strategy = "fallback"
providers = ["claude-key-c", "gemini-codex"]
```

| Strategy | Behavior |
|----------|----------|
| `fallback` (default) | Sequential try-next-on-error. Preserves single-provider semantics when the list has one entry. |
| `round_robin` | Time-sliced cycle. The cursor sticks on one index for `sticky_limit` calls before advancing. `sticky_limit` is clamped to `1` minimum. |

### CLI surface

```
fno config accounts combos add <name> --strategy {fallback|round_robin} \
  --sticky N --providers a,b,c [--scope project|global]
fno config accounts combos list [--json]
fno config accounts combos remove <name> [--scope project|global]
fno config accounts combos test <name>      # config-only validation; reports per-member health
fno config accounts combos use <name> [--scope project|global]
```

`combos test` does NOT issue real API calls (smoke-pinging every member multiplies cost). For an active liveness probe, run `fno config accounts test <id> --smoke` per member.

### Resolution priority

When a subagent dispatch needs to pick a provider, `agents.dispatch_target.resolve_dispatch_target` walks this chain (highest first):

1. `config.agents.<name>.provider`  (Spec 3 per-agent pin)
2. `TARGET_COMBO` env var             (set by `--combo` CLI flag, skill modifier, or megatron manifest)
3. `config.accounts.active_combo`   (settings default)
4. `config.accounts.active`         (existing fall-through)

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
- **Unknown provider id in `--providers`:** rejected before `config.toml` mutation.
- **Combo deleted mid-dispatch:** `dispatch_with_combo` raises `ComboNotFoundError`; `agents.dispatch_target.resolve_dispatch_target` catches its loader equivalent and falls through.
- **All members in cooldown:** `dispatch_with_combo` returns `QueueExhausted(retry_after=...)` with the soonest cooldown-expiry hint.
- **YAML round-trip via PyYAML loses comments:** documented limitation. Use `fno config accounts combos add/remove` for safe edits, or hand-edit and re-add.

## Runtime adapter documentation

The Codex and other non-Hermes runtime adapter designs that previously appeared here are retired.
The active provider registry is Hermes-only, and new provider dispatch must use the canonical `fno agents spawn` seam.

Historical adapter implementation details were removed with the dead runtime paths and are intentionally not documented as current APIs.
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
`get_adapter(record.harness)` for each provider in the combo. Registering
`HermesCliAdapter` as `"hermes"` in
`cli/src/fno/adapters/__init__.py` is the only change needed - no
edits to `rotation.py`, `dispatch_with_combo`, or any combo-resolution
code.

Example combo that routes work across Claude, Codex, and Hermes:

```toml
[accounts.combos.multi-cli]
strategy = "round_robin"
providers = ["claude-anthropic", "codex-openai", "hermes-nous"]
sticky_limit = 3

[[accounts.records]]
id = "claude-anthropic"
name = "Claude Anthropic"
harness = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"

[[accounts.records]]
id = "codex-openai"
name = "Codex OpenAI"
harness = "codex"
auth = "oauth_dir"
credentials_source = "~/.codex"

[[accounts.records]]
id = "hermes-nous"
name = "Hermes Nous"
harness = "hermes"
auth = "oauth_dir"
credentials_source = "~/.config/hermes"
```

### Provider record example

A minimal Hermes provider record:

```toml
[[accounts.records]]
id = "hermes-nous"
name = "Hermes Nous"
harness = "hermes"
auth = "oauth_dir"
credentials_source = "~/.config/hermes"
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

## Quota-aware dispatch

Rotation (above) is **reactive**: a call fails, the error taxonomy classifies
it, and backoff cools the loser down. Quota-aware dispatch adds a **predictive**
layer that reads remaining-quota + reset time *before* a dispatch decision, so
the system can defer, reroute, or warn instead of burning a failed call to learn
what a probe could have told it. It is advisory and fail-open: when quota data
is absent or stale, behavior is byte-for-byte the reactive baseline.

### Layers

- **Probe** (`adapters/providers/usage.py`): `probe_usage(record)` returns a
  `UsageSnapshot` (per-window `used_pct` clamped to [0,100] + `resets_at` epoch)
  or `None`. Claude reads its OAuth usage endpoint; codex reads the `rate_limits`
  payload in its most recent session events. Both are `[VERIFY-AT-IMPL]` and
  fail-open - any failure (endpoint drift, 401, timeout, missing files) is
  `None`, never a raise, and never logs a token. Other CLIs are `None` in v1.
- **Snapshot cache**: an additive `usage` field on `provider-runtime-state.json`
  under the existing `.update.lock` (no schema bump - the `model_locks`
  precedent). Every existing writer carries it through so a health/cursor write
  never drops it. A snapshot older than `probe_ttl_seconds` reads as absent.
- **Headroom predicate** (`runtime_state.headroom`): `OK | LOW | EXHAUSTED |
  UNKNOWN`. A window whose `resets_at` is already past never binds (its limit
  has reset even if the snapshot is stale). No data is `UNKNOWN`, which orders
  **with** `OK` - absence of a probe is not evidence of trouble.

### Consumers

- **Combo rotation** (`rotation.dispatch_with_combo`): an `EXHAUSTED` member is
  skipped like a cooldown (its reset feeds the `QueueExhausted.retry_after`
  hint); non-exhausted members are stably ordered `OK`/`UNKNOWN` before `LOW`.
  Cache-only (no probe - dispatch stays latency-clean).
- **Node dispatcher** (`dispatch._dispatch_one`, `backlog.advance`): both go
  through the one shared route selector,
  `fno.agents.autonomous_route.select_autonomous_route`, so they cannot
  disagree. With `defer_dispatch` on, a ready node whose resolved provider is
  `EXHAUSTED` (or `LOW` with a reset inside `defer_horizon_minutes`) is **not**
  dispatched here - it either cuts over to another record (below) or leaves a
  `quota-deferred` receipt + one decision event, node left in `ready`, the first
  tick after the reset dispatching it. `p0` and explicit human dispatch verbs
  always fire. This is the one probe site (refresh-on-stale).
- **Lane routing** (review panel `alternate` selection): a kind whose records
  are all `EXHAUSTED` is stably demoted below kinds with headroom. Explicit
  per-agent pins and role→provider config mappings are never overridden.
- **Promise-time warning** (`fno config accounts required-bot-check`): read-only,
  warns when a `config.review` required-bot's provider is `EXHAUSTED`, naming
  the reset, so a coming review-gate wedge surfaces immediately.

When `defer_dispatch` is on, the probe runs. The knob is `false` by default. Off, `evaluate_quota_signal` short-circuits to `UNKNOWN` (reason `defer-dispatch-off`) before it ever calls the probe - see the CLI's DISARMED footer below.

When the resolved signal is `UNKNOWN` for any reason and the launch proceeds anyway, one `quota_rotation_declined` event fires. It names the reason and the age of any usage snapshot. A launch that went out blind is now distinguishable in the journal from a system that never needed to rotate.

### CLI

- `fno config accounts usage [--json/-J] [--refresh]` - per-provider windows (used %,
  resets-in). `--refresh` forces a probe past the TTL cache and renders that probe's
  own result, with no second cache read: two reads of one observation can disagree,
  and that disagreement is what once printed `unknown` while the probe returned real
  windows. An unknown provider is `{"state": "unknown", "reason": "<slug>"}` in JSON
  and `unknown (<slug>)` in human output, where the slug names the boundary that
  failed - `harness-unsupported` (no probe for this CLI), `auth-unsupported` (an
  api_key record; the probes read OAuth bearers), `unattributed` (no credential
  provably this record's), `probe-failed`, `probe-error`, `record-missing`,
  `config-unreadable`, `no-windows`, or `not-probed` (no `--refresh` and no fresh
  snapshot). Capability is classified before attribution, so an unsupported harness
  or an api_key record is never reported as an account-binding fault it does not
  have. A known provider additionally
  carries `"persisted": false` when the reading is good but its cache write lost the
  update-lock race; the reading is still displayed, because persistence and
  displayability are separate outcomes.
- `fno config accounts list` gains a compact `headroom=` column, plus a `usage=<age>` column.
- `fno config accounts required-bot-check [--json]` - the pre-promise early warning.

`usage=<age>` is the age of the cached usage snapshot. It is distinct from `snapshot=<age>`, a `managed` record's credential blob age, unrelated and not governed by the same TTL.

`usage=never` names a record with no landed probe. `usage=<age> (STALE, ttl=<ttl>)` names a cached reading older than `probe_ttl_seconds`.

When `defer_dispatch` is `false`, `list` also prints a one-line footer naming the disarmed knob. `defer_dispatch` defaults off, so a fresh install never probes. This display gap is what let quota-aware dispatch go unobserved for months.

### Config (`config.accounts.quota`)

| Key | Default | Meaning |
|---|---|---|
| `defer_dispatch` | `false` | opt-in: autonomous paths may defer on quota |
| `defer_threshold_pct` | `90` | worst-window used % that marks `LOW` |
| `probe_ttl_seconds` | `300` | snapshot freshness window |
| `defer_horizon_minutes` | `60` | only defer on `LOW` when the reset is this close |

And one key on the dispatch block, because it is a routing decision rather than
a quota-probe tuning knob:

| Key | Default | Meaning |
|---|---|---|
| `config.dispatch.on_exhaustion` | `"defer"` | `"failover"` lets an exhausted launch walk the active combo instead of waiting |
| `config.dispatch.cutover_low_after_minutes` | `0` (off) | minutes: a `LOW` window resetting FARTHER out than this cuts over now |

The second predicate is inverted from `defer_horizon_minutes` on purpose. For
deferring, a distant reset means *wait*; for cutting over, a distant reset means
*leave now*, because waiting is the only alternative - a 95%-used weekly window
resetting 70 hours out is exactly the case cutover exists for. Reusing the defer
horizon here would route backwards.

Cutover is cross-**harness** when the selected record's `harness` differs
(claude -> codex resolves to `headless` automatically). It needs a launchable
record for that harness already registered and in the active combo; footnote
never synthesizes one. An explicit harness, provider, account, model, or node
pin is never rerouted - it may still defer. The receipt is
`dispatch_failover`, emitted only after the spawn succeeds.

The two knobs are independent, so a cutover window shorter than the defer
horizon makes both predicates true for one `LOW` reset. Defer wins that overlap:
a near reset is waited out rather than churning harnesses, which is the policy
the horizon exists to express.

Shell dispatchers reach the same decision through `fno dispatch resolve
--autonomous`, which folds the route into the resolved tuple and adds
`route_action` / `route_account` / `route_source` / `route_retry_at`. Only the
destination's record id crosses that boundary; `fno agents spawn
--dispatch-account <record>` resolves its credentials on the other side.
That is a separate flag from `--account`, which is operator intent and
claude-only, while a cutover exists to land on another harness.

Probing and display are always on; only the autonomous *deferral* is gated,
matching the opt-in posture of `backlog advance` and auto-merge. Cost-to-finish
routing is out of scope for v1; the headroom seam is where cost data plugs in
later.

### The credential-shape discriminator: `managed` vs `api_key`

A rotation onto a `managed` record needs a human at a login prompt. One Claude/codex login shares the CLI's one credential slot. `managed.switch` materializes the *stored* snapshot into the slot, but a slot with only one account registered has nowhere to rotate to without a fresh `/login`.

An `api_key` record has no such ceiling. Its credential rides the env overlay (`ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`). `pick_account` and `attempt_swap` can select it with nobody watching. Every `managed` record shares one failure domain, the subscription, not the account row. A Claude-wide outage takes them all down together but never touches this lane.

So a two-record queue of `readyrule` + `makers`, both `auth: managed`, is not really two candidates for an unattended failover. It is one failure domain with two names.

The fix is not more `managed` records. It is one `api_key` record as the last-resort rung.

The lane is already wired end to end. `resolve_account_overlay` returns the `api-key` lane and `pick_account` accepts it. This needs no code, only registration.

```bash
fno config accounts add anthropic-key \
  --harness claude \
  --auth api_key \
  --env ANTHROPIC_API_KEY=sk-ant-... \
  --priority 200 \
  --name "Anthropic API key" \
  --scope global
```

Equivalent hand-edit under the existing `[accounts]` block:

```toml
[[accounts.records]]
id = "anthropic-key"
name = "Anthropic API key"
harness = "claude"
auth = "api_key"
priority = 200
env = { ANTHROPIC_API_KEY = "sk-ant-..." }
```

Add it to the active combo. The live key is `accounts.combos.accounts.providers`, not `accounts`. Give every record a distinct priority too. `(priority, id)` is already the tiebreak, so two records sharing a priority resolve by an alphabetical accident, not by intent.

```toml
[accounts.combos.accounts]
strategy = "fallback"
sticky_limit = 1
providers = ["readyrule", "makers", "anthropic-key"]

# readyrule: priority = 100   (first choice, unchanged)
# makers:    priority = 150   (second)
# anthropic-key: priority = 200  (last resort, costs per token)
```

Then arm the feature. It stays disarmed until this line is set, by design (see `defer_dispatch`'s default above).

```toml
[accounts.quota]
defer_dispatch = true
pick_on_launch = true
```

Verify: `fno config accounts list` shows `anthropic-key` with a populated `usage=` column and no DISARMED footer. `fno config accounts pick` now names more than one launchable candidate.

## Review policy and assurance

`fno.review.policy` classifies how much assurance a change needs *before* the
review runs, then resolves that against the reviewer *resolved to dispatch* - the
resolved panel routing, not raw capacity. Cross-model being off, an all-claude
pin, a degraded fallback, or an exhausted provider all mean a different-family
review will not happen, so none of them can satisfy a high-assurance policy. It
is a thin, pure layer over the substrate above, not a new provider list. This is
a **preflight availability** signal: it certifies a different-family reviewer is
available to dispatch, not that one later completed cross-family (a dispatch that
times out and falls back is caught by the observed-runtime attestation, a
separate post-hoc layer).

`classify_review_policy(size, risk_surfaces)` is deterministic:

| Input | Policy |
|---|---|
| a named high-assurance surface (`auth`, `security`, `secrets`, `merge-gate`, `review-gate`, `loopcheck`, `migration`, `money`, `payments`) | `high_assurance` |
| size `L` | `full_sigma` |
| size `M` | `diverse_preferred` |
| size `S` / unknown | `portable` |

`assess_assurance(policy, ...)` turns the *effective reviewer kinds* (what the
panel will genuinely dispatch to) into a verdict with a single load-bearing
asymmetry:

- **portable / diverse_preferred / full_sigma never block on capacity.** One
  subscription always reviews via same-family fresh-context (`effective:
  portable`); a different-family reviewer is used when present (`effective:
  diverse`) but its absence is not a failure. Diversity is a preference, never a
  paywall.
- **`high_assurance` is the only blocker.** It stays `satisfied: false`,
  `effective: unresolved` when the implementer family is unknown or no
  different-family capacity exists - the review cannot silently pass.

`fno review --assess-assurance --policy-size <S|M|L> [--risk-surface ...]` prints
the verdict JSON and exits `3` when unsatisfied, so a direct CLI caller is
blocked the same way the `/pr check` skill is (no skill-only guard). See
`skills/pr/references/check.md` Step 0c.
