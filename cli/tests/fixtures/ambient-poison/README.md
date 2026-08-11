# ambient-poison

The synthetic ambient state `fno test smoke --ambient dirty` feeds the runner.

Every file here is derived from a measured specimen, not imagined.
Three tests leaked through three different channels on 2026-08-11, and this tree reproduces all three on a machine that has none of them.

| File | Reproduces |
|------|-----------|
| `config.toml` | a developer with a routed codex and `[dispatch] substrate = "bg"`, the config that reddened a dispatch test locally while CI stayed green |
| `settings.yaml` | a global provider combo, the candidate `Path.home() / .fno / settings.yaml` that the `cli/src` conftest pins to `/dev/null` per test |
| `repo/.fno/carveouts.jsonl` | one unharvested `deferred` carve-out, which made a shell test refuse a node it had just minted |
| `home/.fno/*` | a populated state dir behind a `HOME` that is not the runner's |

Values are sentinels (`fno-poison-*`, `x-poison`) rather than plausible data.
A leak has to fail loudly; a realistic-looking value might quietly pass and teach nobody anything.

`AMBIENT_LEAK_CANARY` is set by `fno.hermetic.poison` rather than by a file here.
It is the positive control: it carries no `FNO_` prefix and is not a session marker, so nothing in `hermetic.py` scrubs it, and a test that reads it must go red in the dirty lane.
That red is the proof the lane can detect anything at all.

Adding a specimen: put its ambient shape here, extend `poison()` to point at it, and confirm `test_hermetic.py::test_the_canary_is_the_only_difference` still passes.
If it does not, the new channel is not yet neutralised, which is the finding.
