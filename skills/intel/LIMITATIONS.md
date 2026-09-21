# Limitations of /fno:intel

## Known Limitations and Deferred Work

- The operator class is a residual, not a witnessed one. Claude records no positive typed-turn marker, so a turn counts as operator only after every known injected shape fails to match. A session driven from a bare terminal can still misattribute injected text that matches no envelope shape in the classifier's list. The mux `operator_submit` event is the designed close; until it ships, the residual is named, not eliminated.
- The relay delivered-check is a substring read over the session's transcript. A bus body that reaches the transcript through some channel other than this session's turn flow still reads as delivered.
- Opencode is folded now. Its operator class is the same residual as claude's; sessions with a `parent_id` (subagent children) are excluded; and `skipped.opencode` appears only when no store is readable, carrying the sqlite reason.
- Facet files under `~/.fno/intel/facets/` are keyed by session id + mtime + size. A session whose transcript is rewritten in place at the same size defeats the key and is not re-judged.
