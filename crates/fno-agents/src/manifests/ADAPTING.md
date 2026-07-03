# Adapting a reference manifest to our schema

The reference detection manifests (an external screen-detection corpus we adapt
from) cover ~18 agents. Their schema is close to ours but not identical, and
**our parser is fail-closed on unknown keys** (`manifest.rs`), so every reference
TOML needs a real translation, not a copy. This is a hand-translation over a
handful of rules per file - deliberately not a codegen translator (that would be
more surface than it saves). Validate the result against a live TUI grid with
`cli/scripts/smoke/capture-readiness-grid.sh` before trusting it for a ready
verdict.

## The mechanical rules

| reference | ours | note |
|---|---|---|
| `[[rules]]` | `[[rule]]` | singular table name |
| rule-level `any = [...]` / `regex` / `line_regex` / `all` | one nested `gate = { ... }` | predicates move INTO `gate` |
| `contains = ["a", "b"]` (array of needles) | `gate = { all = [{contains="a"}, {contains="b"}] }` | array `contains` is an **AND** - see the correction below |
| `contains = ["a"]` (single element) | `gate = { contains = "a" }` | collapse a 1-element array to the plain string |
| multiple rule-level predicate keys AND-ed (`contains` + `any` + `all` on one element) | `{ all = [ {contains=...}, {any=...}, {all=...} ] }` | our gate table takes exactly ONE key; AND them under `all` |
| `line_regex = ['...']` | `gate = { line_regex = '...' }` | maps 1:1 (`Gate::LineRegex`); array -> string |
| `regex = ['...']` | `gate = { regex = '...' }` | array -> string |
| `min_engine_version` | keep it | we parse it (root key) |
| `id`, `version`, `updated_at`, `aliases` | DROP | reference metadata; our root parser rejects unknown keys |
| `visible_blocker`, `visible_working`, `visible_*` | DROP | reference per-rule metadata; our rule parser rejects unknown keys |

Regions we support: `whole_recent`, `bottom_non_empty_lines(n)`, `prompt_box_body`,
`osc_title`, `osc_progress`. the reference's `after_last_prompt_marker` /
`after_last_horizontal_rule` and `state = "unknown"` are NOT supported and are
deferred (YAGNI) until a target manifest needs them - none of opencode/agy do.

## Two corrections verified against the reference engine (x-83e7)

The two rules the roster port (amp..qodercli) proved out against the reference
engine's source, load-bearing enough to state plainly:

1. **A rule-level `contains = [a, b]` array is AND, not OR.** The reference
   engine's `compiled_gate_matches` runs `contains.iter().all(|n| text.contains(n))` - the
   whole array must be present. An earlier draft of the table above called it
   "any of"; that was wrong and would have inverted, e.g., devin's idle rules
   into false-ready detectors (the one direction readiness OQ#9 forbids).
   Translate a multi-element `contains` to `all = [{contains=a}, {contains=b}]`.
   the reference's `regex` / `line_regex` arrays are AND too (each pattern must match).

2. **The reference engine matches case-insensitively; ours does not.** It lowercases
   both the screen text and every needle (`needle.to_lowercase()`); our
   `Gate::Contains` is an exact substring (`text.contains(s)`). the reference manifests
   are therefore authored in lowercase, and a verbatim port under-fires against a
   capitalized live TUI ("Esc to interrupt" won't match `esc to interrupt`). That
   miss is the SAFE direction - a rule that fails to fire is a false-NOT-ready,
   never a false-ready - so the roster ports the lowercase needles verbatim and
   leans on it until each agent is smoke-pinned. Where the reference source needle is
   already capitalized (pi's `Working...`), keep the case: it IS load-bearing for
   us even though the reference engine ignores it. When pinning a manifest for real, add the
   on-screen casing (as claude.toml does with both-case `any` variants).

   Corollary: **drop the reference's `(?i)` `line_regex` flags on port.** the reference engine can afford
   a case-insensitive regex because everything is case-folded anyway; keeping it
   in our port would make that one rule case-tolerant while every `contains` in
   the same file stays case-sensitive - an inconsistent, speculative
   case-tolerance the roster does not earn until smoke-pinning. Port the pattern
   without `(?i)`; re-add it (or the on-screen casing) at pin time.

## Procedure

1. Copy the reference TOML rule bodies; apply the table above.
2. Add a `bundled_manifest("<name>")` arm in `manifest.rs` **and** add the name
   to the `bundled_manifests_all_parse` coverage loop - a bundled TOML that no
   test exercises can carry an unknown source key and still "pass" (the parse-
   coverage guard is what makes the fail-closed parser load-bearing).
3. `cargo test -p fno-agents manifest` - the parse-coverage test fails loud on
   any leftover reference key, naming the rule.
4. Validate markers against a real grid: `capture-readiness-grid.sh`. Until then
   the manifest is UNVALIDATED and must not be trusted for an idle/working
   (ready) verdict - only the conservative blocked/working direction (readiness
   OQ#9: a wrong glyph is a false-NOT-ready, never a false-ready).

## The hosting gate (why a manifest can be inert)

A bundled manifest only *fires* once its harness clears three rungs:

1. it is in `READABLE_PROVIDERS` (`cli/.../providers/__init__.py`) so
   `load_registry` accepts its row;
2. it occupies a mux pane (`build_pane_argv` has an arm + a `for_name` provider
   impl) so `scrape_targets` selects it;
3. only then does its bundled manifest match.

opencode has a bundled manifest (this dir) but none of rungs 1-2, so it sits
inert until node x-51f6 hosts it - staged, not live. agy cleared rungs 1-2 in
x-8f7f (READABLE + `AgyProvider` + a `build_pane_argv` agy arm), so its manifest
can fire.
