# Schema and config resolution from a foreign cwd

How `fno` finds its bundled events schema and resolves project/config paths when
invoked from a directory that is not the footnote checkout, and why those two
concerns must stay decoupled.

## The problem

`fno` ships inside the footnote plugin but is meant to run from any repo. Two
resolution paths broke when run from a foreign cwd (surfaced during an
acme-web target run):

1. **Schema resolution.** `fno gate set` routes through
   `scripts/lib/set-gate.sh` -> `scripts/lib/events-validate.sh`, which loads
   `cli/src/fno/events/schema.yaml`. From a repo that does not vendor that
   file, the bash resolver could only find it through `FNO_REPO_ROOT` or
   `CLAUDE_PLUGIN_ROOT`, neither of which is set in a plain terminal outside a
   Claude Code session. Result: `schema unavailable`, and the gate refused to flip.

2. **Config resolution.** The workaround an operator reaches for,
   `export FNO_REPO_ROOT=<footnote checkout>`, is an *overloaded* lever.
   `paths.py:resolve_repo_root()` reads `FNO_REPO_ROOT` first, and project/config
   resolution depends on it. So exporting it to satisfy the schema resolver also
   silently repoints every `fno config get` at the footnote project instead of
   the cwd repo. This fails silently (wrong-project read, no error), which made
   it the nastiest part of the cluster. It compounds with the two-reader config
   landmine documented in `docs/path-config.md`.

The fix makes schema resolution self-sufficient (no env var required) and
documents that `FNO_REPO_ROOT` is for project/config scoping only.

## Bash schema resolution (`scripts/lib/events-validate.sh`)

`_ev_resolve_schema_path` resolves the first readable path in this order:

1. `EVENTS_SCHEMA_PATH` - explicit operator override.
2. `${git toplevel}/cli/src/fno/events/schema.yaml` - a repo that vendors
   its own schema (local override).
3. **lib-relative** `$(dirname BASH_SOURCE)/../../cli/src/fno/events/schema.yaml`
   - the schema bundled beside this lib inside the plugin. `BASH_SOURCE[0]` is
   this file regardless of cwd or who sourced it, so the bundled schema resolves
   with *no env var set*. This is the tier that fixes the foreign-cwd miss.
4. `${FNO_REPO_ROOT}/cli/src/fno/events/schema.yaml` - legacy fallback.
5. `${CLAUDE_PLUGIN_ROOT}/cli/src/fno/events/schema.yaml` - legacy fallback.

The lib-relative tier sits *above* the env-var tiers so an operator never needs
`FNO_REPO_ROOT` to fix a schema miss. `BASH_SOURCE[0]` is read through a `:-`
guard so a zsh caller (which does not populate `BASH_SOURCE`) falls through to
the env tiers under `set -u` rather than crashing. The real `fno gate set` path
runs under bash, where the lib-relative tier always resolves. This mirrors the
self-location pattern in `scripts/lib/phase-verifier.sh`.

When every tier misses, the original `schema unavailable: <path>` diagnostic
(rc 2) is preserved.

## Python schema (`cli/src/fno/events/schema.yaml`)

The schema lives INSIDE the package, beside the validator that loads it
(`cli/src/fno/events/__init__.py`). `_resolve_manifest_path` reads the sibling
`schema.yaml` directly - no walk-up, no env var. Because the file sits under the
`src/fno` package, it ships as ordinary package data in both the wheel and the
sdist, so an installed `fno` resolves the schema from any cwd with no `docs/`
tree present. A built wheel carrying `fno/events/schema.yaml` is therefore the
canonical schema verbatim.

This replaced an earlier force-include scheme: the schema used to live in
`docs/architecture/` (outside the package), so a `hatch_build.py` hook
force-included it into the wheel as `fno/events/_schema.yaml` and an sdist
`force-include` vendored a `_schema_vendor.yaml` copy for the
sdist-then-wheel-from-sdist build mode. Colocating the schema with its loader
removed that machinery entirely - the package now carries its own schema.

## FNO_REPO_ROOT overload warning (`cli/src/fno/paths.py`)

`resolve_repo_root()` still reads `FNO_REPO_ROOT` first (the test/CI hook is
unchanged). Now that schema resolution no longer needs it, a non-fatal warning
fires when `FNO_REPO_ROOT` pins the footnote checkout (basename `footnote`)
while the cwd is a *different* git repo:

```
fno: warning: FNO_REPO_ROOT pins project/config resolution to <root>, but cwd is
a different repo (<cwd-root>); `fno config get` will read the footnote project,
not this repo. Unset FNO_REPO_ROOT unless that is intended.
```

It is best-effort (swallows all errors, never blocks resolution), the git probe
carries a 2s timeout so it cannot hang a CLI invocation on a slow filesystem, and
it fires at most once per process (`resolve_repo_root` is `@cache`-d).

## Files

| File | Role |
|------|------|
| `scripts/lib/events-validate.sh` | bash resolver: lib-relative tier above env tiers |
| `cli/src/fno/events/schema.yaml` | the schema itself - package data, ships in wheel + sdist |
| `cli/src/fno/events/__init__.py` | reads its sibling `schema.yaml` (`_resolve_manifest_path`) |
| `cli/src/fno/paths.py` | `FNO_REPO_ROOT` foreign-project warning |

## Verification

- From a non-footnote repo with no schema env vars: `cd /tmp && fno gate set ...`
  resolves the bundled schema (bash tier 3); `tests/events/test-bash-validator.sh`
  asserts this with a foreign tmp git repo.
- `uv build` (sdist + wheel-from-sdist) and `uv build --wheel` (direct) both ship
  `fno/events/schema.yaml` as package data; `cli/tests/smoke/test_build.sh`
  installs the wheel into a clean venv and imports it from an empty cwd.
- `cli/tests/unit/test_paths.py` covers the warning (fires / not-footnote / same-repo).
