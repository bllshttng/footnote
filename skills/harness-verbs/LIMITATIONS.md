# harness-verbs limitations

## Known Limitations and Deferred Work

- `fno-agents verbs` renders the packaged table as measured. A harness newer than its measurement date can grow or rename verbs the render does not know. The table's per-row notes carry the measurement dates.
- Bare-form detection reads the session env markers the dispatch seam stamps. A session launched outside that seam must pass the harness by name.
- opencode user-defined commands are invisible to the table. The render lists built-ins only, and every opencode row is marked `built_in = true` for that reason.
