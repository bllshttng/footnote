# Node cwd Authority: work-map root over recorded cwd

## Problem

graph.json stores two fields that encode the same fact - which repo owns this work: `project` (a name) and `cwd` (a path). No invariant ties them together. Every filing site resolves them through independent chains, so `fno backlog idea --project X` run from repo Y stores X as project and Y's root as cwd. Launchers then trusted `cwd`.

Incident: a node filed with `project=footnote` from a session whose PWD was a sibling repo recorded that foreign cwd. `dispatch-node.sh` booted its target worker there - the session grouped under the wrong project and early `.fno/` state writes landed in the foreign repo. Same misscope class as the 2026-06-02/04 backfills; this fixes the consumer of that bad data plus the remaining explicit-`--project` producer.

## Authority model

**Work-map root is the authoritative launch cwd whenever `node.project` is set and mapped. Recorded `cwd` is fallback data, not authority.** A node with a mapped project cannot direct launchers to a different cwd; the escape hatch is filing under a project name absent from the work-map (or a null project).

Historical contradictions are resolved at read time forever - no backfill, no mutation-time heal. Raw cwd values remain as audit evidence.

## Three legs

### 1. Read side: derived `_resolved_cwd`

`project_root_from_settings(project)` in `cli/src/fno/graph/_intake.py` is the inverse of `detect_project_from_settings`: project name -> work-map path. Both directions consume the same `_settings_candidate_paths()` (project-local, `config_file()`, global - the shared candidate list), so forward and inverse cannot drift. It walks both schemas (`work.workspaces.{ws}.projects[]` and legacy flat `work.projects.{name}`), returns `abspath(expanduser(path))` of the first match, and is a pure map lookup - no `stat()`, no git calls. A mapped-but-nonexistent path fails loudly at launch (`fno agents ask --cwd <missing>` errors, surfaced as a `failed` outcome line) rather than silently substituting a possibly-wrong recorded cwd.

`fno backlog get` emits the derived field at the serialization boundary:

```python
root = project_root_from_settings(e["project"]) if e.get("project") else None
e["_resolved_cwd"] = root or e.get("cwd")
```

Underscore prefix = derived, matching `status`. Unlike `status` it is **never persisted** to graph.json - a persisted copy would go stale when the work-map changes; read-time derivation stays current forever.

### 2. Launch consumers

`skills/target/scripts/dispatch-node.sh` resolves the worker cwd with the exact expression:

```bash
node_cwd="$(printf '%s' "$node_json" | jq -r '._resolved_cwd // .cwd // empty' 2>/dev/null)"
```

The `// .cwd` fallback is a locked back-compat contract, not an implementation detail: an older *installed* fno (no `_resolved_cwd` in `get` output) degrades to the pre-fix behavior instead of breaking. The shell script upgrades with the repo checkout; `fno` upgrades on `fno update`; the fallback makes the two unsynchronized upgrade paths safe in either order. Both the `launched` and dry-run outcome lines carry `cwd=<path>` so a wrong-repo boot is visible at dispatch time.

The `/agents` skill (spawn verb) passes the same `_resolved_cwd`-with-fallback to `spawn.sh --cwd`. `autolaunch-on-ready.sh` delegates to dispatch-node.sh and is fixed transitively. Megawalk is unaffected - the walker is project-scoped and never relocates via node.cwd.

### 3. Write side: filing derives cwd from explicit project

All five filing sites apply one rule when the project is EXPLICIT (CLI flag or plan frontmatter) and no explicit `--cwd` was given: derive cwd via `project_root_from_settings`, falling back to the previous behavior (`repo_root()` / `canonical_root`) when unmapped. Explicit `--cwd` always wins at write - stored verbatim (abspath'd). Auto-detected projects are unchanged (cwd was the match source; already consistent).

| Site | Behavior |
|---|---|
| `cmd_add` / `cmd_idea` | derive between the explicit-`--cwd` branch and the `repo_root()` fallback |
| `cmd_new` (capture family) | explicit `--project` derives regardless of `--unscoped` (an explicit project outranks the unscoped default) |
| `resolve_node_project_and_cwd` (intake) | derives only when project came from `cli_project`/frontmatter and frontmatter `cwd` is absent |
| `cmd_update --project X` (no `--cwd`) | re-derives when mapped (reprojects become one-flag operations); when unmapped, leaves cwd unchanged with one stderr warning |

Settings reads happen outside `locked_mutate_graph` - never under the graph lock.

## Observability: `project_cwd_mismatch`

`fno backlog triage health` counts pending nodes whose project is mapped but whose normalized cwd (`abspath(expanduser(...))` on both sides - normpath alone leaves relative/absolute mismatches) differs from the work-map root. `--json` lists offending node ids under `project_cwd_mismatch_nodes`; zero renders explicitly as `project_cwd_mismatch: 0`. Unmapped projects are never counted (the work-map has no opinion), nor are done/superseded nodes (harmless history).

Threshold lives under `config.health_monitor.thresholds.project_cwd_mismatch`, default 0: with filing fixed, any new mismatch is a producer regression and `--check` exits 4 at alert severity (zero-threshold breaches classify as full severity). The metric also appears in `fno backlog triage trend` deltas.

## Failure semantics

| Scenario | Behavior |
|---|---|
| config.toml missing/malformed at `get` time | resolver returns None, `_resolved_cwd` falls back to recorded cwd, existing reader warnings on stderr - never worse than pre-fix |
| Work-map root missing on disk | loud launch failure (`failed <node>` outcome line), operator fixes the work-map |
| Stale installed fno | jq `// .cwd` fallback = pre-fix behavior |
| Settings edited mid dispatch loop | each `get` re-reads; a torn read degrades to fallback and self-corrects on the next node |
| Deliberate `--project X --cwd Y` divergence | stored verbatim, but launchers ignore Y while X is mapped (accepted consequence; escape hatch = unmapped project name) |

## Operational note

After merging, run `fno update` - the Python-side behavior (derived field, filing derivation, health metric) is gated on the installed fno. Until then the jq fallback keeps dispatch on pre-fix behavior, which is safe.
