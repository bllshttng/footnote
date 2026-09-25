# Summary: port the retask transaction into fno-agents

Shipped in three commits on `feature/x-1312`: the pure transaction module
(`crates/fno-agents/src/retask.rs`, detect + execute over a seam trait, all
refusal words byte-identical), the live seams and the empty-argv `rename`
payload door (`retask/transport.rs`, `state.rs` rename-with-node, the
intercept at net-zero lines in `client.rs`), and the thin Python front
(`run_retask` over `verb_call`; the Python transaction, `rename_agent` and
`project_verified_tier` deleted). All six CI gates pass locally; 49 Rust
tests ported against 42 Python test functions removed; `cli/src/fno` added
+30 of the 30-line ceiling.

## Deviations from the plan

- `verb_call` timeout is 1200s, not the plan's 300s. The plan's own restamp
  poll had a near-17-minute worst case; a 300s clip could kill a legitimate
  transaction after `/clear` and the Python-side except arm would report the
  pane untouched. The higher cap keeps the old worst case covered.
- The payload schema carries two optional keys the plan did not list:
  `graph` (so the source-node join honors `config.paths.graph_json`) and
  `registry` (so `run_retask`'s `registry_path` parameter stays meaningful).
  Both fall back to ambient resolution when absent.

## Skipped

- The plan's verify step 6 (live pane proof on a real worker) did not run:
  it spawns a real pane on the shared mux and was not authorized for this
  session. The transaction is covered end to end by the ported Rust tests
  with real registry, graph, manifest and name-mint state.
