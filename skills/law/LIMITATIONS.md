# Law limitations

## Known Limitations and Deferred Work

- The approval gate is Claude-only. Equivalent non-forgeable approval events for other harnesses are deferred.
- The measured UserPromptSubmit payload exposes no human-origin discriminator. The permission-gated fallback is required until the harness exposes one.
- An approved enactment records `authority_source: chat_attested`, which reads in the **law** lane for any row at or after `AUTHORITY_LANE_CUTOVER`. It stays a distinct value from `operator`, so a reader can always tell a chat approval from a person at a terminal. A chat approval still cannot prove a human origin; the permission decision is what the lane rests on.
