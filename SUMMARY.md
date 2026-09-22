# Summary: a claude reap receipt pins model or route

## What landed

- `crates/fno-agents/src/receipt.rs`: a claude receipt's resume line now asks
  resume_pin (`claude_resume_recipe`): it pins `--model`/`--effort` from the
  row, or names `fno agents spawn --resume <sid> -P <provider> -m <model>`
  when a non-Anthropic route served the row. The claude `resume` string is
  shell-quoted; `receipt_file_name` extracted; `decide_reap_receipt` answers
  the spawn-axes `reap_receipt` field.
- `crates/fno-agents/src/resume_receipt.rs`: the preserved-session hint prints
  the receipt's own rendered line (`resume:`), and adds a new-session note
  when the line is the route door.
- `crates/fno-agents/src/spawn_axes.rs`: the `reap_receipt` field routes to
  `decide_reap_receipt` (same field-on-a-verb shape as `resume_pin`).
- `cli/src/fno/agents/registry.py`: `_stage_removal_receipt` asks the Rust
  builder and writes the answered file (14 added lines).
- `cli/src/fno/agents/resume_cli.py`: the claude exact-predecessor lane
  refuses with exit 13 and names the spawn door (10 added lines).
- Docs: `retirement-receipts.md` recipe paragraph + Python door row;
  `dual-implementation-inventory.md` removal-receipt-writers row marked
  builder-retired.

## Deviation from the plan

The plan's codex expectation ("keeps `codex resume <sid> --remote unix://` in
both fields") assumed no pre_exec composition. The codex capability form
composes a `sh -c '<pre>; exec …'` wrapper, so per-token shlex quoting
re-quotes an already-quoted script and corrupts it. Fix: quoting applies to
the claude branch only; non-claude harnesses keep the raw `argv.join(" ")`.
Recorded in `dual-implementation-inventory.md` via the same row.

## Verification

- `cargo test --lib` filters `receipt`, `resume_receipt`, `gc_receipts`,
  `spawn_axes`: green (227 passed; `the_live_eighteen_split_fifteen_and_three`
  passes in isolation; its one broad-filter failure is the documented
  lock-free env race in `paths.rs`, not this diff).
- `--test retirement_e2e`: green.
- `cargo fmt --check`: clean. `clippy --all-targets`: zero findings in the
  touched files (690 pre-existing crate-wide under local clippy 1.94; CI's
  pinned toolchain arbitrates).
- Python: `test_registry.py` + `test_lineage_resolution.py` +
  `test_agents_history.py`: 121 passed. `check-python-static.sh`: clean.
- `check-file-budget.sh`: cli/src/fno +24 (grant cap 26); no over-budget file
  grew. `check-no-internal-refs.sh`: clean.
- Live binary probe with planted rows: zai row prints
  `fno agents spawn --resume <sid> -P zai -m 'glm-5.3-flash[1m]'` with
  `removal_trigger: session`; anthropic row prints
  `claude --resume <sid> --model claude-opus-5`.
