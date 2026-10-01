# Ship limitations

## Known Limitations and Deferred Work

- Merge consent and review must hold at merge time. Changing auto-merge configuration after initialization does not rewrite the run's resolved approval.
- The merged-PR ritual reconciles only state it can read. An unavailable mailbox or hosted API leaves follow-up disposition unresolved. (Inherited from the retired `/fno:pr` skill.)
