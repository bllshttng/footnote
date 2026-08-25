# Mux restart recovery

A client reconnect and a mux server restart have different guarantees. Client reconnect keeps the server-owned `Core`, PTYs, panes, and layout alive. Server restart destroys pane-substrate processes because their PTYs are children of the server, but the squad store, harness transcripts, worktrees, registry, and append-only spawn receipts remain.

At startup, the existing agents daemon preserves recovery records by default and reports interrupted atomic-write temp files. The mux server rebuilds each persisted pane-substrate worker as a named held shell in its stored tab and tree position. The shell prints its state, so workspace pruning cannot mistake it for a pristine disposable shell. No harness process starts during restoration.

The first focus on a held pane rechecks the current registry row by full harness session id. A live session is refused to prevent a second writer. A dead session resumes through the harness's existing resume command, replaces the held shell in the same tree leaf, and persists the full session id again. `mux.restore.hold_workers = false` retains the legacy idle-row-only behavior.
