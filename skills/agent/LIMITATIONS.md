# Agent limitations

## Known Limitations and Deferred Work

- A spawn request can reach the fleet slot cap without creating a worker. Treat the refusal receipt as the source of truth instead of an exit code alone.
- A targeted loop halt takes effect at the next turn end; its matching watchdog wake and PR nudge refuse until clear or expiry.
