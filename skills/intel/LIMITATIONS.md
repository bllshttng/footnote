# Limitations of /fno:intel

## Known Limitations and Deferred Work

- Operator is witnessed, not inferred: the mux writes an `operator_submit` row when a person presses Enter in a pane or portal, and the fold binds turns to those rows by session and time. Turns outside the witness read `unknown`, never `operator`. Uncovered paths: a bare terminal, the desktop apps, claude.ai jobs (typing that never passes the mux); a hand-started harness in a shell pane (its submit is `resolution: unresolved`, nothing joins it); a submit queued past the 30s bind window. `witness.unwitnessed_sessions` measures how much of the machine's operator typing sits outside the witness.
- The relay delivered-check is a substring read over the session's transcript. A bus body that reaches the transcript through some channel other than this session's turn flow still reads as delivered.
- Opencode is folded now. Its operator class is the same witness join as claude's. Sessions with a `parent_id` (subagent children) are excluded. When no store is readable, `skipped.opencode` carries the sqlite reason.
- Facet files under `~/.fno/intel/facets/` are keyed by session id + mtime + size. A session whose transcript is rewritten in place at the same size defeats the key and is not re-judged.
