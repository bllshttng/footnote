# Law limitations

## Known Limitations and Deferred Work

- The approval gate is Claude-only. Equivalent non-forgeable approval events for other harnesses are deferred.
- The measured UserPromptSubmit payload exposes no human-origin discriminator. The permission-gated fallback is required until the harness exposes one.
- An approved enactment records `authority_source: chat_attested`, which reads in the unattributed lane. A chat approval cannot prove a human origin, so it never records operator law.
