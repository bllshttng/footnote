# tests/

Shell harnesses. `fno test smoke` runs every one of them; the globs in `cli/src/fno/test_cmd.py` discover new files with no registry edit.

## Running one directly is not hermetic

**`bash tests/whatever.sh` reads your real `HOME`, your real config chain, and your real carve-out ledger.**
Hermeticity is applied by the runner when it builds each step's environment (`fno.hermetic.neutralise`, called from `_child_env`), so invoking the script yourself skips it entirely.

What you lose: a pass proves nothing about whether the test is hermetic, and a failure may be your machine rather than the code.
That asymmetry is exactly what produced three separate specimens on 2026-08-11, each leaking through a different channel, each found by a human noticing a red suite.

Run the same step hermetically instead:

```bash
fno test smoke --only 'backlog aliases'    # the step, isolated
fno test smoke                             # everything
fno test smoke --ambient both              # clean and dirty lanes, to catch a leak
```

Full mechanism: [docs/architecture/test-hermeticity.md](../docs/architecture/test-hermeticity.md).

## Writing one

Do not sandbox ambient state by hand.
The per-test pin is what this design replaces: the first author of the pytest sandbox pinned four things and missed the fifth, and each of the three specimens leaked through a channel its author had not thought of.
If your test needs a channel neutralised that `fno/hermetic.py` does not cover, add it there so every test gets it, and `cli/tests/unit/test_ambient_surface.py` will hold the decision.
