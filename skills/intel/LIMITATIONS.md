# Limitations of /fno:intel

## Known Limitations and Deferred Work

- Operator is witnessed, not inferred. When a person presses Enter in a pane or portal, the mux writes an `operator_submit` row. The fold binds turns to those rows by session and time. A turn outside the witness reads `unknown`, never `operator`. Uncovered paths: a bare terminal, the desktop apps, and claude.ai jobs. That typing never passes the mux. A hand-started harness in a shell pane writes `resolution: unresolved`, and nothing joins it. A submit queued past the 30s bind window also reads unknown. `witness.unwitnessed_sessions` counts sessions typed outside the witness.
- The relay delivered-check is a substring read over the session's transcript. A bus body that reaches the transcript through some channel other than this session's turn flow still reads as delivered.
- Opencode is folded now. Its operator class is the same witness join as claude's. Sessions with a `parent_id` (subagent children) are excluded. When no store is readable, `skipped.opencode` carries the sqlite reason.
- Facet files under `~/.fno/intel/facets/` are keyed by session id + mtime + size. A session whose transcript is rewritten in place at the same size defeats the key and is not re-judged.
- Category shares rest on the sample, not the window. The Executive summary and Categories percentages speak for the `judged` population only.
- A session still active at report time is idle-gated out of the sample and is judged on a later run.
- Opencode rows carry no activity counters: their tokens, lines, tool errors, and languages read null, and `activity.unmeasured` names the harness.
- Hours, response time, and the parallel measure are empty for history without witness coverage. Witnessed operator turns are the only input, and sessions folded before the witness existed have none.
- Line counts come from edit arguments, so a rejected edit still counts its lines, the same choice Claude's `/insights` makes.
- The HTML scrub is pattern-based. A secret with no known shape under 40 characters, and paraphrased transcript text with no quotes, pass through. A quoted span outside Operator corrections is dropped, so a needed quote outside that section must move into it.
- The HTML copy is a snapshot. It does not update when the markdown or the fold JSON changes; `--render` re-renders it. The retention rule prunes stamped copies past the 12 newest, and a hand-renamed HTML outside the stamp pattern is never pruned.
