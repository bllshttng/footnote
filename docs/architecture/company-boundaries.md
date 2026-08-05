# Company Boundaries

## Enforced layer direction

The sole layer map lives in `scripts/ci/check-company-boundaries.sh`, and this document intentionally does not duplicate it.
Dependencies may point within a layer or to a lower-numbered layer, never upward.
The map is module-granular because `fno.company.contracts` is core vocabulary while `fno.company.campaign`, `coordinator`, `topology`, `execution`, `join`, and `cli` compose that vocabulary with roles.
A package-granular `fno.company` layer would misclassify that deliberate split and report the shipped topology-to-roles import as a core dependency.

The check scans Python imports with a known-present roles-to-contracts edge as its positive control and reports the importing file, line, layers, and statement for every prohibited dependency.
The current repository does not satisfy the declared map: the check reports an import from platform config into graph, three imports from platform events into agents, five imports from graph CLI into agents, and a platform/core layer cycle.
Those results are findings in shipped code, not defects repaired by the conformance pass.

## Audit and CI modes

`bash scripts/ci/check-company-boundaries.sh --strict` is the full audit and remains red while any prohibited dependency or declared-layer cycle exists.
The strict result currently names nine prohibited imports and the platform-to-core-to-platform cycle, so it preserves the falsification finding.

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
