# Path configuration

footnote stores all user-data files under `~/.fno/` by default. Every path is configurable via `~/.fno/config.toml`. This page documents the full schema, environment variables, template variables, and the migration flow.

## Quick start

Run `fno config doctor` to see your current resolved paths and detect common problems:

```
[doctor] settings source: /Users/you/.fno/config.toml
[doctor] schema_version: 1
[doctor] state_dir: /Users/you/.fno

[doctor] OK; no suspicious paths detected.
```

Exit code 0 means clean. Non-zero means at least one path needs attention.

## Settings file location

The CLI reads every candidate that exists and **deep-merges** them, with higher-priority files overriding lower-priority ones key-by-key. Candidate priority, highest first:

1. `$FNO_CONFIG` (explicit path override; when set, the only candidate)
2. `<repo-root>/.fno/config.toml` (project-local to this checkout)
3. `<canonical-root>/.fno/config.toml` (the main checkout's config, reached via `git worktree list`, so a linked worktree reads shared project config; deduped when it equals candidate 2)
4. `~/.fno/config.toml` (per-user global)
5. Built-in defaults (when no file exists)

The merge is per key: nested maps merge recursively, while scalars and lists replace wholesale (a project-level `config.external_reviewers` list fully replaces the global one rather than appending). A key absent from a higher-priority file falls through to the next file down. So the per-user global can hold shared defaults (for example `config.obsidian.vault`) while each repo's project-local file sets only its deltas (for example `config.post_merge.parking_lot_path`). The project-local file no longer shadows the entire global; it overrides only the keys it actually sets.

This matches the shell reader (`scripts/lib/config.sh`, which already does per-key local-over-global fallback) and the provider loader, so all three config surfaces agree on precedence. Note `fno config get` only resolves schema-modeled keys (`config.{state_dir, plans_dir, paths.*, obsidian, project, blueprint, post_merge, target}`); unmodeled keys such as `external_reviewers`, `auto_merge`, `gates`, and `budget_cap` are read only by the shell reader.

## Full schema

```yaml
schema_version: 1   # required; currently only 1 is valid

config:
  # Root directory for all global state. Every paths.* below
  # derives from this when not set explicitly.
  state_dir: ~/.fno/

  # Per-project plans directory. Relative paths are anchored to the
  # git repo root (or cwd if not in a git repo).
  plans_dir: .fno/plans/

  # Per-resource overrides. Omit a key to derive from state_dir.
  paths:
    graph_json: null         # default: <state_dir>/graph.json
    ledger_json: null        # default: <state_dir>/ledger.json
    briefs_dir: null         # default: <state_dir>/briefs/
    fleet_dir: null          # default: <state_dir>/fleet/
    postmortems_dir: null    # default: <state_dir>/postmortems/
    worktrees_base: null     # default: <state_dir>/worktrees/
    memory_dir: null         # default: <state_dir>/memory/
    hook_logs_dir: null      # default: <state_dir>/hook-logs/
    inbox_dir: null          # default: <project_root>/.fno/inbox/
    handoffs_dir: null       # default: <vault>/internal/{project}/handoffs/ when obsidian.enabled,
                             # else <state_dir>/handoffs/<project>/

  obsidian:
    enabled: false  # set true to use {vault} template variable
    vault: null     # absolute path to your Obsidian vault root

  project:
    id: null  # used by {project} template variable; falls back to git repo name

  backlog:
    # Node-ID minting scheme. New backlog node IDs are minted as
    # <id_prefix><hex> (e.g. fno-a3f9). Resolution is format-agnostic, so any
    # node ID resolves regardless of the prefix/width it was minted under.
    id_prefix: null     # e.g. fno-, xy-; lowercase, <=7 chars, not cv-/fu-/tgt-.
                        # Set at `fno setup`. null falls back to the ab- prefix.
    id_hex_width: 8     # hex chars in a minted id, 4-8. The setup wizard offers 4;
                        # an absent key resolves to 8.
```

Unknown keys are silently ignored for forward compatibility. Glob characters (`*`, `?`, `[`) and paths longer than 4096 bytes are rejected at load time.

## Environment variables

| Variable | Effect |
|----------|--------|
| `FNO_CONFIG` | Absolute path to a specific config.toml file. Overrides all path-based discovery. |
| `FNO_GLOBAL_SETTINGS_PATH` | Overrides the per-user global candidate (`~/.fno/config.toml`) without affecting the project-local candidate. Set to `/dev/null` to disable the global candidate entirely. Used by `cli/src/fno/conftest.py` to keep unit tests isolated from the developer's real global config. Empty-string value is treated as unset. |
| `HOME` | Determines `~` expansion in path values. Set by the OS; override in tests only. |
| `FNO_REPO_ROOT` | Overrides `git rev-parse --show-toplevel` for tests and CI environments where git is unavailable. Scopes **project/config resolution only**, not events-schema resolution (the schema self-locates inside the plugin; see [architecture/schema-config-resolution.md](architecture/schema-config-resolution.md)). Do NOT export it to fix a `schema unavailable` miss: pinning it to the footnote checkout from inside another repo silently repoints `fno config get` at the wrong project. `fno` warns (non-fatal) when it detects that foreign pin. |
| `FNO_SKIP_MIGRATION` | Set to `1` to skip the automatic startup migration check. Useful in CI. |

## Template variables

Path values in `state_dir`, `plans_dir`, and any `paths.*` field may contain template variables:

| Variable | Expands to | Requires |
|----------|-----------|---------|
| `{vault}` | The value of `obsidian.vault` | `obsidian.enabled: true` and `obsidian.vault` set |
| `{project}` | The value of `project.id`, or the git repo basename | `project.id` in settings OR a git repo at the project root |
| `{{` | Literal `{` | - |
| `}}` | Literal `}` | - |

Template variables are processed after `$VAR` shell expansion and before `~` expansion. An unknown `{foo}` raises an error at resolve time.

### Vault user example

```yaml
schema_version: 1
config:
  plans_dir: "{vault}/fno/{project}/plans/"
  obsidian:
    enabled: true
    vault: ~/Documents/my-vault
  project:
    id: my-project
```

`plans_dir` resolves to `~/Documents/my-vault/footnote/my-project/plans/`.

### Fresh-install example (no Obsidian)

```yaml
schema_version: 1
config:
  state_dir: ~/.fno/
  plans_dir: .fno/plans/
  obsidian:
    enabled: false
```

All paths derive from `~/.fno/`. This is also what `fno setup migrate-paths` writes on first run.

## Lookup order for path resolution

For each accessor call (e.g. `paths.graph_json()`):

1. If `config.paths.graph_json` is set in settings, resolve that value.
2. Otherwise, derive from `state_dir` (e.g. `state_dir / "graph.json"`).
3. Apply `$VAR` expansion, template substitution, `~` expansion, then `Path.resolve()`.

Project-relative paths (`plans_dir`, `inbox_dir`) anchor relative strings to the git repo root (or `FNO_REPO_ROOT` in tests).

## `fno config doctor` output

On a clean install:

```
[doctor] settings source: /Users/you/.fno/config.toml
[doctor] schema_version: 1
[doctor] state_dir: /Users/you/.fno

[doctor] OK; no suspicious paths detected.
```

When a suspicious path is found:

```
[doctor] settings source: /Users/you/.fno/config.toml
[doctor] schema_version: 1
[doctor] state_dir: /private/tmp/fno

[doctor] 1 suspicious path(s) detected:
  - state_dir = /private/tmp/fno: temp directory; data will not survive reboot

Run 'fno setup migrate-paths --force' to regenerate paths.
```

Suspicious patterns and their reasons:

| Pattern | Reason |
|---------|--------|
| `/tmp/`, `/var/tmp/`, `/private/tmp/` | Temp directory; data will not survive reboot |
| `~/Dropbox/` | Dropbox sync; conflicted copies on multi-machine setups |
| `~/iCloud/` | iCloud sync; conflicted copies on multi-machine setups |
| `~/Library/Mobile Documents/` | iCloud sync (macOS iCloud Drive path) |
| `~/OneDrive/` | OneDrive sync; conflicted copies on multi-machine setups |
| `.git/` | Git internal; may be cleaned by git gc |

## `fno setup migrate-paths` flow

On first run after install, the CLI automatically writes `~/.fno/config.toml` with built-in defaults. The sentinel `~/.fno/.path-migration-done` prevents re-running.

To regenerate settings explicitly:

```bash
fno setup migrate-paths --force
```

This rewrites `config.toml` from the built-in defaults. Any custom values are overwritten; back up the file first if you have customizations.

## Deprecation pathway

Before the path-config substrate was introduced, hardcoded `~/.fno/` references were scattered across shell scripts and Python code. The resolver layer (`fno.paths`) is the migration target. Shell scripts use `fno paths emit-shell` to source path variables; Python code imports `from fno import paths`.

If you find a script that still hardcodes `~/.fno/`, file an issue or use `FNO_CONFIG` to point it at the right file while the migration is underway.

## Testing isolation

Use `from fno.paths_testing import use_tmpdir` in pytest fixtures to isolate all path resolution under `tmp_path`:

```python
def test_my_feature(tmp_path, monkeypatch):
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    # paths.graph_json() now resolves under tmp_path; real ~/.fno untouched
```

The helper writes a minimal `config.toml`, sets `FNO_CONFIG`, and clears both `load_settings` and `_settings` caches.
