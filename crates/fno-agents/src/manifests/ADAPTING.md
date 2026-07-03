# Adapting a herdr manifest to our schema

herdr (`~/code/tools/herdr/src/detect/manifests/*.toml`) ships ~18 per-agent
screen-detection manifests. Its schema is close to ours but not identical, and
**our parser is fail-closed on unknown keys** (`manifest.rs`), so every herdr
TOML needs a real translation, not a copy. This is a hand-translation over a
handful of rules per file - deliberately not a codegen translator (that would be
more surface than it saves). Validate the result against a live TUI grid with
`cli/scripts/smoke/capture-readiness-grid.sh` before trusting it for a ready
verdict.

## The mechanical rules

| herdr | ours | note |
|---|---|---|
| `[[rules]]` | `[[rule]]` | singular table name |
| rule-level `any = [...]` / `regex` / `line_regex` / `all` | one nested `gate = { ... }` | predicates move INTO `gate` |
| `contains = ["a", "b"]` (array = "any of") | `gate = { any = [{contains="a"}, {contains="b"}] }` | array `contains` is an OR |
| `contains = ["a"]` (single element) | `gate = { contains = "a" }` | collapse a 1-element array to the plain string |
| multiple rule-level predicate keys AND-ed (`contains` + `any` + `all` on one element) | `{ all = [ {contains=...}, {any=...}, {all=...} ] }` | our gate table takes exactly ONE key; AND them under `all` |
| `line_regex = ['...']` | `gate = { line_regex = '...' }` | maps 1:1 (`Gate::LineRegex`); array -> string |
| `regex = ['...']` | `gate = { regex = '...' }` | array -> string |
| `min_engine_version` | keep it | we parse it (root key) |
| `id`, `version`, `updated_at`, `aliases` | DROP | herdr metadata; our root parser rejects unknown keys |
| `visible_blocker`, `visible_working`, `visible_*` | DROP | herdr per-rule metadata; our rule parser rejects unknown keys |

Regions we support: `whole_recent`, `bottom_non_empty_lines(n)`, `prompt_box_body`,
`osc_title`, `osc_progress`. herdr's `after_last_prompt_marker` /
`after_last_horizontal_rule` and `state = "unknown"` are NOT supported and are
deferred (YAGNI) until a target manifest needs them - none of opencode/agy do.

## Procedure

1. Copy the herdr TOML rule bodies; apply the table above.
2. Add a `bundled_manifest("<name>")` arm in `manifest.rs` **and** add the name
   to the `bundled_manifests_all_parse` coverage loop - a bundled TOML that no
   test exercises can carry a herdr-only key and still "pass" (the parse-
   coverage guard is what makes the fail-closed parser load-bearing).
3. `cargo test -p fno-agents manifest` - the parse-coverage test fails loud on
   any leftover herdr key, naming the rule.
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
