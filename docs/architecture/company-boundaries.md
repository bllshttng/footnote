# Company Boundaries

## Enforced layer direction

The sole layer map lives in `scripts/ci/check-company-boundaries.sh`, and this document intentionally does not duplicate it.
Dependencies may point within a layer or to a lower-numbered layer, never upward.
The map is module-granular because `fno.company.contracts` is core vocabulary while `fno.company.campaign`, `coordinator`, `topology`, `execution`, `join`, and `cli` compose that vocabulary with roles.
A package-granular `fno.company` layer would misclassify that deliberate split and report the shipped topology-to-roles import as a core dependency.

The check scans Python imports with a known-present roles-to-contracts edge as its positive control and reports the importing file, line, layers, and statement for every prohibited dependency.
The current repository does not satisfy the declared map: the check reports two imports from the platform events CLI into the agents runtime, and a platform-to-runtime-to-platform layer cycle those two imports keep alive.

### Map coverage is 35%, so a cleared finding is not automatically a removed dependency

The map declares 152 of the 430 Python modules under `cli/src/fno/`.
`fno.backlog`, `fno.harness_identity`, `fno.mail`, `fno.done`, `fno.target_cli` and 273 others are unmapped, and `layer_for` returns `None` for an unmapped module, so the scan skips those edges entirely.

The consequence is load-bearing when reading a burndown: routing a call through an unmapped module clears a finding without removing the upward dependency, and so does relocating a module without also declaring it here.
Judge a boundary change by whether the dependency disappeared, never by whether the line left the baseline.
The three modules moved to the platform layer during the burndown below are named in the map for exactly this reason; declaring them is what puts them back under enforcement rather than into the blind spot.

### Findings retired, and the two that remain

Seven of the original nine prohibited imports were misplaced leaf utilities, and moving each one removed a real edge:

| Was | Now | Why it could move |
|---|---|---|
| `fno.graph._constants.RESERVED_PREFIXES` read by `fno.config` | owned by `fno.config` | a three-element frozenset whose only reader was the importing validator |
| `fno.agents.events.emit` called by `fno backlog undefer` / `unsupersede` | `fno.graph.failure.emit_undefer_boundary` | the log is graph-owned and `fno.graph.failure` already reads it; the agents emitter was serving as a bare append-a-JSON-line helper |
| `fno.agents.provider_resolve` | `fno.dispatch_flags` | stdlib plus the harness-marker table, with seven callers spanning every layer |
| `fno.agents.drive_authority` | `fno.drive_authority` | a read-only `state.json` reader whose only dependency is `fno.paths` |
| `fno.agents.rust_runtime.resolve_binary` | `fno.rust_binary` | a stdlib-only filesystem lookup; `rust_runtime` keeps the dispatch half and depends downward on it |

None of the moves left a re-export shim at the old path.
A shim would keep the upward import spelling available and hide the move from the next reader.

The two survivors are one finding: the `fno event emit` envelope builder stamps the sending session's identity (`fno.agents.self_stamp`) and resolves its spawn lineage from the agents registry (`fno.agents.registry`).
That is platform-layer code reading agent-runtime state, and it is a genuine architectural finding rather than a misplaced file.
Neither callee can follow the others down: `self_stamp` imports `fno.claims.session_pid` at L1 core, so moving it to L0 would trade one upward edge for another, and `agents.registry` is the agents registry.
The remaining options are larger than a burndown: invert the stamp behind a registered hook, which silently stops stamping in any process that never imports `fno.agents`, or move envelope construction into the runtime layer and leave `fno event emit` a thin caller.

Two shortcuts were considered and rejected on measurement rather than taste.
Hoisting `fno.graph.cli` and `fno.events.cli` to a higher layer, on the precedent that this map already places `fno.company.cli` at composition, clears nothing: at L3 those modules still import L5 `fno.agents`.
It only bites if the CLI modules are declared L5, and `graph/cli.py` is roughly 10k lines of logic rather than a composition root, so declaring it unconstrained would empty the check.
Reclassifying `fno.events` itself from platform to runtime fails a harder test: `fno.config`, `fno.claims`, `fno.graph` and `fno.approvals` all import it, so the move would manufacture nine new upward violations while erasing three.

## Audit and CI modes

`bash scripts/ci/check-company-boundaries.sh --strict` is the full audit and remains red while any prohibited dependency or declared-layer cycle exists.
The strict result currently names two prohibited imports and the platform-to-runtime-to-platform cycle, so it preserves the falsification finding.

The cycle line reports the first cycle the detector finds, not every cycle present.
Retiring the config-to-graph import did not clear "the" cycle; it revealed the next one, which the two surviving events-CLI imports keep alive.
Expect a burndown to re-root the reported cycle rather than remove it, until the last upward edge out of a layer is gone.

`bash scripts/ci/check-company-boundaries.sh --baseline` is the gate registered in `fno test`.
Its checked-in source of truth is `scripts/ci/company-boundary-baseline.txt`, which records each known finding as an importing file and line, layer pair, and import statement, plus the cycle.
The baseline gate passes only when the exact finding set is unchanged.
A new or changed finding fails, and a removed finding also fails until its human-readable baseline entry is removed in the same pull request.
Therefore a green baseline verdict means the known debt did not grow; it does not mean the architecture is clean or supersede the red strict audit.

The check enforces no edges for `fno-skills`, which is Markdown and shell content, or for `fno-mux`, which is Rust in `crates/fno/src/mux_cli.rs`.
They remain named as uncovered seams instead of being counted as clean Python boundaries.

## Extraction gate

The extraction threshold is two real consumers; the observed count is one, so the gap is one and the gate is closed.
The growth-studio pack delivered in PR #732 is the one real consumer.
The planned second consumer in the ReadyRule web repository was never built: there is no branch and no pull request.
The three company-conformance scenarios are internal, share an author and test harness with the contracts they exercise, and therefore do not count as the second consumer.
No physical package split, repository split, or published extraction is authorized by this work.

## Pack projections

A repository-root agent or skill whose first frontmatter block contains `pack: <id>` is a build-time projection of `plugins/<id>/plugin.yaml`, not an independent surface.
The boundary check reads the marker with the same frontmatter rule as the skill-bundle freshness gate, attributes a valid projection to its pack, and fails a marker whose pack manifest does not exist.
A root file with no `pack:` marker remains an independent surface, which is the fail-safe direction when frontmatter is malformed.
