# `fno.handoff` - exit codes and shared CLI output helpers

Two responsibilities live here:

1. `ExitCode` - the single source of truth for every CLI exit code. Every subcommand returns one of the codes declared in `exit_codes.py`; the coverage test in `cli/tests/test_exit_codes.py` refuses to let an unused code accumulate undocumented.
2. `output` - the JSON-mode output helpers (`merge_json_flag`, `json_mode`, `emit`, `emit_error`, `write_output_file`) shared by the sub-apps so `--json` behaves identically everywhere.

## History: the exit-42 dispatch protocol (removed)

This package used to host `DispatchPayload` / `build_dispatch_payload` / `emit_and_exit` - the exit-42 handoff the pre-wedge `fno loop` used when a reasoning phase needed the host LLM, with `fno loop --continue --session <id>` as the resume command. The control-plane collapse removed the consumers in stages: the wedge (ab-d0337fbc) deleted the gate/phase machinery, and step-5 group 3 (ab-9fd662c6) deleted the exit-12 `fno loop` stub itself, so the dispatch payload builder went with it. The unified loop (`fno-agents loop run`) resumes from world state (graph + journal + claims), not from dispatch payloads.

`ExitCode.DISPATCH_REQUIRED` (42) remains declared: the exit-42 convention is still used by skill-side shims (see `references/ship-phase.md` in the target skill), just no longer minted by this package.

## Related

- `docs/architecture/unified-loop.md` - the loop runtime that replaced the dispatch protocol.
- `docs/architecture/control-plane-loop.md` - the post-wedge stop-hook architecture.
