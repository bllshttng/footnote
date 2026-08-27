# Law limitations

## Known Limitations and Deferred Work

- The approval gate is Claude-only. Equivalent non-forgeable approval events for other harnesses are deferred.
- The measured UserPromptSubmit payload exposes no human-origin discriminator. The permission-gated fallback is required until the harness exposes one.
- An approved enactment records `authority_source: chat_attested`. That reads in the **law** lane at or after `AUTHORITY_LANE_CUTOVER`. The value stays distinct from `operator`. A reader can always tell a chat approval from a person at a terminal. A chat approval still cannot prove a human origin. The permission decision is what the lane rests on.
- Recording law from chat works. Retracting it does not. On a law-lane row, `retract_decision` requires `authority_source` to be exactly `operator`. A chat-enacted law needs an attended terminal to withdraw. Supersession is not affected and accepts `chat_attested`.
