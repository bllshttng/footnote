# fno (footnote CLI)

CLI for the footnote autonomous delivery pipeline.

## Quickstart

```bash
# Install (from a local checkout of this repo)
uv tool install /path/to/abilities/cli

# Sanity check
fno --version
fno --help

# Walk the backlog via the unified Rust loop (the old `fno loop` verb is removed)
fno-agents loop run --driver megawalk
```

Released wheels also bundle the `fno-agents` Rust supervisor binary, which is the
**default** `fno agents` runtime when installed: the daemon-native verbs plus four of the
five shared verbs (`list`/`stop`/`rm`/`reconcile`, which reached flag + stdout parity in
`ab-544c544e`) auto-route to it. `ask` stays on Python — the daemon only PTY-manages
codex/gemini (claude is a shellout) and returns the whole settled screen rather than the
extracted reply, so routing `ask` would regress claude and reply output. (`=python` pins
everything to Python, `=rust` forces the binary for every verb, `ask` included.) See
[`docs/distribution.md`](../docs/distribution.md) for the runtime-selection table, the
artifacts, the wheel binary-bundling mechanism, and the gated publish runbook.

## Install channels

A channel is a complete front door - you pick one, never "install both". What each delivers:

| Channel | Command | You get |
|---|---|---|
| **PyPI / uv** | `uv tool install fno` or `pip install fno` | The `fno` CLI, all three Rust binaries (`fno-agents`, `fno-agents-daemon`, `fno-agents-worker`) on PATH, and the internalized verbs (`fno plan`, `fno notify`, `fno executor`, `fno phase kill-check`, `fno event verify-evidence`, ...). `fno target` and `fno bundle` need the plugin clone - on a bare install they exit non-zero with an "install the plugin" message rather than a traceback. |
| **Plugin clone** | `claude plugin install footnote`, or clone + `claude --plugin-dir /path/to/footnote` | The full think -> plan -> do -> review -> ship pipeline (skills + hooks), plus a binary-complete `fno`: the plugin postinstall prefers the published PyPI platform wheel (binaries included), falling back to a source build of `cli/` (Python-only; run `fno update --rust` for the binaries) when the wheel is unavailable. |
| **cargo** | `cargo install fno` | The `fno` CLI plus all three Rust binaries on PATH, via a lazy first-run bootstrap (`ab-4040eee8`). `cargo install fno` compiles a tiny shim; the first `fno` run provisions uv (downloading the standalone uv if absent), runs `uv tool install fno` (the same PyPI wheel, binaries bundled), verifies the installed package is this project's, then forwards by absolute path. The provision is a one-time multi-second step; later runs forward instantly via a sentinel. The `fno-agents` crate is published only for name-reservation and to back the bootstrapper; `cargo install fno-agents` (no Python CLI) is not a supported front door. |
| **fno.sh** | `curl -fsSL fno.sh \| sh` | The no-prerequisite front door (`ab-f49b54c1`): the only channel that needs nothing but `curl` (or `wget`) to start. The script ensures `uv` is present (chaining to Astral's standalone installer when absent, reusing an existing `uv` when present), runs `uv tool install fno` (the same PyPI wheel, binaries bundled), and verifies the installed package is this project's before reporting success. `uv` provisions its own Python, so a bare machine with no Python and no `uv` reaches a working `fno`. Pin a release by passing the variable to the shell that runs the script (not to `curl`): `curl -fsSL fno.sh \| FNO_VERSION=1.2.3 sh`. The script is repo-sourced (`scripts/install/fno.sh`), served verbatim over HTTPS, and inspectable in a browser. |
| **brew** | `brew install <owner>/fno/fno` | A Homebrew own-tap formula (`ab-d59d219a`) that installs the same PyPI platform wheel into a brew-managed venv: brew provides Python (`depends_on "python@3.13"`), the wheel provides the `fno` CLI plus all three Rust binaries, and the formula symlinks the binaries onto PATH. `brew upgrade`/`brew uninstall` manage the venv cleanly. Launch-gated on the PyPI publish + tap creation (see below). |

The PyPI/uv channel is binary-complete as of `ab-18563bcc`: the release wheel carries all three Rust binaries, so daemon-backed verbs work with no second install. Publishing the real `fno` package to PyPI (over the reserved `0.0.0` placeholder) is the remaining launch step - until then `uv tool install fno` by name resolves the placeholder, and the plugin postinstall's version guard falls back to the source build. See [`docs/distribution.md`](../docs/distribution.md) for the gated publish runbook.

The cargo channel shares that PyPI gate and adds a crates.io publish (the reserved `fno` and `fno-agents` crates over their `0.0.0` placeholders). Until both land, `cargo install fno` by name resolves the crates.io placeholder and the first run's `uv tool install fno` resolves the PyPI placeholder; the channel's clean-machine smoke (`cli/tests/smoke/cargo_bootstrap_smoke.sh`, run on the release-wheels matrix) proves the bootstrap against the freshly built wheel today, and the by-name `cargo install fno` variant becomes a required check once both publishes are live.

The fno.sh channel shares the same PyPI gate (the one-liner ends in `uv tool install fno` by name) and adds two operator launch steps: wiring DNS for `fno.sh` to Cloudflare with CF-managed TLS, and a repo-to-host deploy of `scripts/install/fno.sh` (Cloudflare Pages git-integration or a `wrangler` CI step) with purge-on-deploy so a script update propagates. Until those land, the channel's clean-machine smoke (`cli/tests/smoke/fno_sh_smoke.sh`, run on the release-wheels matrix) proves the installer against the freshly built wheel via `FNO_INSTALL_WHEEL`, and a PR-time portability gate (`sh -n` + `dash -n` + `shellcheck --shell=sh`) keeps a non-POSIX script from ever reaching the host. The by-name `curl -fsSL fno.sh | sh` becomes live once the PyPI publish and the Cloudflare wiring are both in place.

The brew channel shares the same PyPI gate and is the most standard of the four: a Homebrew formula (`scripts/install/homebrew/fno.rb`) builds a venv from the `python@3.13` dependency and pip-installs the published platform wheel into it (the wheel `url` uses `:nounzip` so it stays a file, since an unpacked wheel directory is not pip-installable; deps resolve from PyPI because an own-tap formula installs with network). The one wrinkle is the shared "py-wheel flywheel" fact - the three `fno-agents*` binaries ride in the wheel as `shared_scripts`, which pip does not link into the keg bin (only the `fno` console_script), so the formula symlinks them onto PATH explicitly (`bin.install_symlink Dir[libexec/"bin/fno-agents*"]`). The formula lives in an own tap (`github.com/<owner>/homebrew-fno`), movable later to an org tap by GitHub repo transfer + redirect (never delete the old repo - that breaks the redirect). Until launch the committed `fno.rb` is a skeleton (placeholder `url`/`sha256`); its clean-machine smoke (`cli/tests/smoke/brew_formula_smoke.sh`, run on the release-wheels macOS matrix) installs a concrete local-wheel formula with the SAME mechanism end to end - `brew install` -> three binaries on the keg bin -> `brew test` -> clean `brew uninstall` - against the freshly built wheel. The by-name `brew install <owner>/fno/fno` and the full `brew audit --strict --new` go live once the PyPI publish and the tap creation are both done.

## Compat alias: `footnote`

The binary was renamed from `footnote` to `fno`. The legacy name still
works for one release but prints a deprecation warning on stderr:

```bash
footnote --version
# footnote: deprecated; use 'fno' instead. The 'footnote' alias will be removed in the next release.
# footnote 0.1.0
```

Prefer `fno` in scripts so upgrades don't break you.

## Removed binary: `megawalk`

Older releases shipped a standalone `megawalk` console script that
pointed at the same backlog-iteration loop now reachable through
`fno megawalk`. The standalone entry point was removed because the
two surfaces had drifted apart in flag declarations and bootstrap
defaults; a single canonical surface eliminates the drift hazard.

After upgrade, `which megawalk` returns nothing and `megawalk --help`
reports `command not found`. The replacement is the `fno megawalk`
subcommand (or its deprecated `footnote megawalk` alias):

```bash
# old
megawalk --json

# new
fno megawalk --json
```

Subcommands `status`, `pause`, `resume`, `bootstrap`, and `reset`
continue to work under `fno megawalk`. The Python module
`fno.megawalk` and the public `run_megawalk` function are
unchanged except for a new optional `scope_all` keyword (default
`False`), so existing importers require no edits.

Selection is **project-scoped by default** (ab-82e65b72): `fno megawalk`
only walks nodes in the current project (derived from cwd, matching
`fno backlog next`). Pass `--all` / `-A` for cross-project selection:

```bash
fno megawalk          # current project only
fno megawalk --all    # all projects (prior global behavior)
```

## v2 state layout (vestigial)

The v2 state layout was a migration-era scheme that separated CLI-native
loop state from the bash pipeline's `.fno/` directory. It never
became the live layout, and the control-plane collapse (ab-d0337fbc)
superseded it: the session manifest is now the inputs-only, immutable
`.fno/target-state.md` (see
[`docs/architecture/control-plane-loop.md`](../docs/architecture/control-plane-loop.md)),
and there is no gate machinery, so the `.fno/v2/artifacts/` gate
directory the scheme described no longer exists anywhere.

What remains is vestigial:

- `fno state show --v2` still accepts the flag and reads
  `.fno/v2/target-state.md` if that file happens to exist, falling
  back to v1 with a stderr note otherwise. Nothing writes that path.
- `config.v2_enabled` is still parseable in settings but no longer gates
  a live code path (the `fno loop --v2` enable path was removed with the
  `fno loop` verb).

Do not treat v2 as a layout you can opt into; use the canonical
`.fno/target-state.md` manifest.

## Subcommand trees

Run `fno --help` for the full list. The main trees are `state`, `graph`
(`backlog`), `runtime`, `worker`, `event`, `agents`, and
`reality-check`.

### Backlog commands (top-level)

- `fno done [id-or-title] [--pr N] [--link URL] [--note TEXT] [--backfill]`
  -- mark a graph node done. Auto-detects from current git branch +
  `gh pr view` for `domain: code` when no flags are given. For non-code
  domains (research, design, trading, etc.) pass `--link` for an artifact
  URL or `--note` for a free-text marker. Also rolls up `session_id`,
  `cost_usd`, `cost_sessions`, and `points` from `~/.fno/ledger.json`
  entries matching the node's `plan_path`. `--backfill` runs only the
  rollup (no status change); without a query it sweeps every node with
  `_status=done`, useful for reconciling nodes marked done before the
  rollup existed.
- `fno find <query> [--domain X] [--project Y] [--status S] [--json]` --
  fuzzy search across graph entries. Output is tab-separated by default
  (id, status, domain, project, title); `--json` emits an array.
- `fno new <title> [--domain X] [--project Y] [--priority P] [--force-domain]`
  -- create a new graph entry from the CLI without a plan file.
  Ambiguous `--domain` values (fuzzy prefix match against history)
  exit 2 with a "did you mean" suggestion; pass `--force-domain` to
  introduce a genuinely new domain.

See `fno done --help`, `fno find --help`, `fno new --help` for the full
flag list. Each of these also works under the backlog/graph subapp
(`fno backlog find`, `fno backlog new`, `fno graph find`, etc.).

Key exit codes (see `fno <sub> --help` for per-command specifics):

| Code | Meaning |
|---|---|
| 0 | Success |
| 2 | Hard error |
| 3 | State file missing/invalid |
| 4 | `triage health --check`: thresholds breached |
| 11 | Resource locked (e.g. `fno agents ask` blocked by a busy worker) |
| 42 | Dispatch required; stdout carries a JSON handoff payload |

### Backlog health monitoring

```bash
# Default report (always exit 0; same shape as before --check landed)
fno backlog triage health [--all] [--json]

# Loop-safe check: exit 0 healthy, exit 4 breach, no stdout when healthy
fno backlog triage health --check --quiet [--all]

# History readout (rolling window)
fno backlog triage trend [--days N] [--json]
```

Configuration lives under `config.health_monitor` in
`~/.fno/settings.yaml` (or the project-local override). Defaults
ship as documented in CLAUDE.md > Backlog Health Monitoring; the
typical hourly invocation is:

```bash
/loop 1h fno backlog triage health --check --quiet
```

`/loop` survives across context compactions and Claude Code session
boundaries. Same command works under `cron`, `launchd`, or GitHub
Actions schedules - non-zero exit pages a monitoring tool.

## `fno mail` (cross-project messaging)

Each agent owns a folder at `~/your-vault/internal/agents/{project}/` with `inbox.md`
as the live inbox inside it. Agents send messages to other agents' inboxes, read
their own inbox at the top of every megawalk iteration, and ack each message after handling.

Set `project: <name>` in your `.fno/settings.yaml` to identify the
sender. Without this field, `fno mail` errors with the fix string.

### Verbs

- `fno mail send --to-project <project> --kind <kind> --body "..."` - send a message
- `fno mail unread [--json]` - list unread messages in own inbox
- `fno mail ack <msg-id> [--triaged-into ab-...]` - mark a message handled
- `fno mail reply --to <msg-id> --kind <kind> --body "..."` - reply to a message
- `fno mail list [--all]` - list all messages in own inbox
- `fno mail lint [<project>]` - find malformed messages
- `fno mail triage <msg-id>` - LLM triage for heads-up kind (returns JSON plan)

### Message kinds

| Kind | Use |
|---|---|
| `question` | Ask another agent something. Interrupts mid-feature work. |
| `answer` | Reply to a question. Threads via `reply_to`. |
| `heads-up` | "This might affect you, you decide what to do." Triggers LLM triage. |
| `notification` | Pure FYI, no action expected. |
| `lesson` | Cross-project memory write (supervisor -> worker). |

### Cross-project behavior

`fno mail` writes to the recipient's inbox file. Each agent reads only its
own inbox. Replies go to the original sender's inbox with `reply_to:` set.

The `com.fno.backlog-sync` launchd job already mirrors
`~/.fno/graph.json` to obsidian; the inbox files live directly in
the obsidian vault, so no separate sync is needed.

### See also

- Design doc: `~/your-vault/internal/fno/design/2026-05-04-cross-project-inbox-fleet.md`
- Megawalk drain integration: `skills/megawalk/references/inbox-handlers.md`

## `fno whoami` / `fno status` (self-introspection)

Two read-only top-level commands that give an agent a curated view of its
own operating context. Distinct from `fno mail` (cross-agent messaging) and
from `fno agents` (the dispatch mesh); these are `man self` for the agent in
its current operating layer. (Formerly `fno agent whoami` / `fno agent status`;
the `fno agent` singular namespace was retired in ab-12dd2a5d when the
never-auto-invoked `suggest` / `capabilities` verbs were trimmed.)

They detect three layers independently and report on all that are present:

| Layer | Source |
|-------|--------|
| fleet | `~/.fno/fleet/*/00-INDEX.md` whose `projects:` cwd matches this repo |
| walker | `.fno/megawalk-state.md` |
| session | `.fno/target-state.md` (preferred) or `.fno/session-state.md` |

### Commands

- `fno whoami` - one-line per layer: project + fleet + walker + session + provider
- `fno status` - session phase + status + bounded events tail + inconsistency flags. The immutable post-wedge manifest carries no gate booleans, so phase shows `n/a (collapsed)` and status is derived from the latest `termination` event in `events.jsonl`; a legacy manifest with `*_passed`-style keys still prints a per-gate section

### Shared options

- `--json` / `-J` - structured output to stdout
- `--state-file PATH` - override session-state detection (file missing -> rc=2)
- `--no-walker` - suppress walker layer in output
- `--no-fleet` - suppress fleet layer in output

### Invariants

Both commands are read-only. No state is mutated, no events are emitted,
no side effects. Tests assert this via repeated-invocation paired md5
hashing across `target-state.md`, `megawalk-state.md`, `events.jsonl`,
and the fleet `00-INDEX.md`. `--json` emits isoformat strings for any
datetime parsed out of the manifest frontmatter, so it never crashes on
a real manifest.

### See also

- Design doc: `internal/fno/plans/2026-05-11-abi-agent-introspection.md`
- Plan: `internal/fno/plans/2026-05-11-abi-agent-introspection/`

## Configuration

### Project name (`project:`)

Each project must declare its identity for cross-project messaging:

```yaml
project: acme-web
```

Without this, `fno mail` cannot resolve the sender. CLI errors loudly.

### Inbox rotation (`config.inbox.*`)

```yaml
config:
  inbox:
    auto_rotate: true
    max_size_bytes: 1048576
    max_read_messages: 200
    keep_recent_read: 50
    triage:
      timeout_sec: 60
      log_decisions: true
```
