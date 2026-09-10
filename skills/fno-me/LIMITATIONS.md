# fno-me limitations

## Known Limitations and Deferred Work

- Registers only the session that invokes it. Every other hand-started session needs its own run, or the SessionStart knob.
- A session with no addressable harness identity exits 3 and joins nothing. The report to the user is the only record.
