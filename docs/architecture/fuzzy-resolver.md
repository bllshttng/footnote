# Fuzzy Resolver: ab-ID Prefix Matching

## Problem

Two graph-id resolvers had drifted out of step. The Python resolver
`cli/src/fno/graph/fuzzy.py::resolve_id` already supported title-token
substring matching (used by `fno backlog find`), but its `ab-` branch did
exact equality only. The shell resolver `scripts/lib/graph-resolve.sh`
(used by `/target`, `/blueprint`, `/megawalk`) had its own embedded Python
heredoc that also did exact equality. Users typing instead of
the full got "no match" everywhere.

## Solution

Two changes, both transparent to existing call sites.

### Step A: prefix matching in `resolve_id`

`resolve_id` now classifies the suffix after `ab-`:

| Suffix shape | Behavior |
| --- | --- |
| Exact match in `entries` (any chars) | `kind=exact`, fast path |
| 8 hex chars, no exact match | `kind=none` (no false-positive prefix hits when the user typed a complete id) |
| 4-7 hex chars | `startswith()` against entries; `kind=fuzzy` if unique, `kind=ambiguous` if multi, `kind=none` if zero |
| Anything else (3 chars, non-hex, trailing punctuation) | `kind=none` with a `malformed ab- query` note |

The exact-match fast path runs first regardless of suffix shape, which
preserves the historical contract for legacy non-hex IDs (`ab-tr000001`
in older test fixtures).

The malformed-suffix path explicitly returns `kind=none` rather than
falling through to title fuzzy. An `ab-` prefix is a strong user signal
that they want id resolution; silently fuzzing onto a title that happens
to mention the literal substring would be a hard-to-diagnose wrong match.

### Step B: shell resolver wraps `resolve_id`

`scripts/lib/graph-resolve.sh::resolve_arg` replaces its inline equality
heredoc with a call into `fno.graph.fuzzy.resolve_id`. The shell
filter widens from `^ab-[0-9a-f]{8}$` to `^ab-[0-9a-f]{4,8}$` so partial
prefixes reach the Python module.

Three behavior changes:

1. **Prefix matches** resolve via `kind=fuzzy`.
2. **Ambiguous prefixes** print candidate IDs to stderr and soft-fail
   (echo arg unchanged). `RESOLVE_STRICT=1` makes the rc nonzero.
3. **Title fuzzy match** is opt-in via `RESOLVE_FUZZY=1`. Default-off
   because `/target` passes raw feature descriptions that must NOT
   collapse onto an existing graph node.

### Legacy fallback

If the `footnote` Python package can't be imported (no `uv`, no venv,
no `PYTHONPATH`), the heredoc exits 5 and the shell falls back to a
pre-fuzzy exact-match path. The fallback prints an explicit notice so
the import-error stderr is contextualized. Partial prefixes hitting
the legacy path warn the user that prefix queries cannot resolve in
this environment.

### `_find_node` symmetry

`cli/src/fno/graph/_intake.py::_find_node` (used by `fno backlog
done`, `defer`, `undefer`, `supersede`) now routes any `ab-` input
shorter than 11 chars (i.e. less than the full `ab-XXXXXXXX` form)
through `resolve_id`. Ambiguous matches return `None` to preserve the
"not found" caller contract, but stderr names the candidate IDs so
the user can disambiguate.

## Out of Scope

- **Bare-hex queries** (`9728` without the `ab-` prefix). Hex words
  collide with English (`cafe`, `face`, `dead`). Defer to a separate
  node with a length-6 floor if anyone asks for it.
- **Tab completion** for shell users. Different mechanism; separate
  node.
- **Title fuzzy matching by default in `/target` etc.** Opt-in via
  `RESOLVE_FUZZY=1` only.

## Files Touched

| Surface | Change |
| --- | --- |
| `cli/src/fno/graph/fuzzy.py` | Prefix block in `resolve_id`; explicit `kind=none` for malformed `ab-` queries |
| `cli/src/fno/graph/_intake.py` | `_find_node` wrapper with prefix routing + ambiguous-stderr surfacing |
| `scripts/lib/graph-resolve.sh` | Heredoc rewired to call `resolve_id`; legacy fallback path with explicit notice |
| `cli/tests/unit/test_graph_fuzzy.py` | 9 new prefix tests |
| `cli/tests/unit/test_intake_find_node.py` | New file, 8 tests covering full/partial/ambiguous paths |
| `tests/test-graph-resolve.sh` | New shell test, 17 assertions covering exact, partial, ambiguous, RESOLVE_FUZZY, missing graph, legacy fallback, shell-metacharacter safety |
| `tests/fixtures/graph-fuzzy.json` | Test fixture with prefix-collision nodes |

## Coordination

The claims-on-intake feature reads
`claims: ab-XXX` from plan frontmatter. With this resolver shipped,
that field accepts partial IDs (`claims:) and resolves
correctly. No code coordination needed; the resolver is a transparent
upgrade for all existing call sites.
