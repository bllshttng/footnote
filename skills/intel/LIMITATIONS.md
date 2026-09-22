# Limitations of /fno:intel

## Known Limitations and Deferred Work

- Operator is witnessed, not inferred. When a person presses Enter in a pane or portal, the mux writes an `operator_submit` row. The fold binds turns to those rows by session and time. A turn outside the witness reads `unknown`, never `operator`. Uncovered paths: a bare terminal, the desktop apps, and claude.ai jobs. That typing never passes the mux. A hand-started harness in a shell pane writes `resolution: unresolved`, and nothing joins it. A submit queued past the 30s bind window also reads unknown. `witness.unwitnessed_sessions` counts sessions typed outside the witness.
- The relay delivered-check is a substring read over the session's transcript. A bus body that reaches the transcript through some channel other than this session's turn flow still reads as delivered.
- Opencode is folded now. Its operator class is the same witness join as claude's. Sessions with a `parent_id` (subagent children) are excluded. When no store is readable, `skipped.opencode` carries the sqlite reason.
- Facet files under `~/.fno/intel/facets/` are keyed by session id + mtime + size. A session whose transcript is rewritten in place at the same size defeats the key and is not re-judged.
