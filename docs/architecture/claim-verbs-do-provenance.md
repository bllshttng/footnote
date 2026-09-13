# The claim verbs' do-provenance contract

Moved from `cli/src/fno/claims/cli.py` under the file-budget gate's remedy (long prose lives in docs, modules ship code). Content unchanged.

A node's `do` lifecycle row used to be written only at a clean terminal (release `--stamp-do`, the finalize backstop, `/execute` Step 1.5). A session killed mid-phase reaches none of those, so the whole row was lost, including a `started_at` that sat in its claim file the entire time. A node finished to an open, green, attested PR read `sessions=[blueprint only]`, and a groom pass would have redone it.

The claim is the one thing every worker touches at the start of work and again at its end, so the row is bound to the claim's own lifecycle: the acquire verb stamps the do row open beside the claim it takes, and the release verb stamps it closed beside the claim it returns. A reader that wants the full phase history reads the node's session rows; a reader that wants to know why they are trustworthy reads this contract.

## Exit codes

- `0` - success.
- `1` - `ClaimHeldByOther`, or acquire/refresh's own contention-retry exhaustion (both mean "transient, caller should retry later"); also `reap`'s own distinct overload of 1 - a reapable file's archive move could not be confirmed on re-read (see `reap`'s own docstring, not a retry signal).
- `2` - validation / input error.
- `3` - `ClaimCorrupted` or `ClaimGoneAway` (race during operation).
- `4` - `HolderMismatch` (release/refresh wrong holder).

Structured output uses `--json` on each verb. Without `--json`, output is a human-friendly summary on stdout; errors always go to stderr.
