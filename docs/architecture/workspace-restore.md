# Workspace restore

After a reboot or a killed mux server, every worker pane is gone. A pane's pty was a child of the server pid and died with it. What survives is durable identity: the squad store's member records, the agents registry, and each harness's own persisted session state. `fno mux workspace restore` walks the members and brings each one back through its own harness's resume form. The workspace is reconstructed, never survived.

## The verb

```bash
fno mux workspace restore [--dry-run] [--harness <harness>] [--json]
```

The verb enumerates every live, non-tombstoned worker member in the squad store. It joins each to its registry row and resumes it: the pane runs the harness's own resume argv with the member's full session id. `--dry-run` classifies every member and spawns nothing. `--harness` narrows the run to one harness. `--json` prints one row per member with its outcome, so a script can branch without parsing prose.

Every member that cannot come back is named, with the reason. The reasons include: no registry row, no session id, a harness the table gives no resume form, an ambiguous name, a failed spawn. Silence is never an outcome. A run that resumes two and refuses one prints all three.

Two preconditions are refusals, not empty results. The verb refuses before the session's first real attach. At that point startup restore has not run and the persisted squads were never read. Answering "nothing to restore" there is a lie. It also reads the registry file itself before classifying. The off-loop registry reader ticks independently, and a headless restore can otherwise refuse every member with "no such agent" while its row sits on disk.

The server's member list is authoritative while the server runs. The squad store file is that list's persist target, not its source. Every pane event re-writes the file from memory. So any write to `squads.json` from outside a live server, such as `fno mux workspace prune`, must be followed by the `SquadReload` control verb to every answering session. Without the reload, the next pane event writes the old members back over the pruned file. Restore then reads memory-shaped rows, not the file a prune just shrank.

## The declared resume form

The resume argv is not hardcoded. Each harness declares an `interactive_resume` form in the capability table (`cli/src/fno/agents/harness_capabilities.toml`). The server reads that declaration in process through the same reader the attach lane uses (`agents_view::resume_form`, the resume front door over `declared_form`). The bundled table is embedded at build time. An operator can override it per harness with `[harness.<name>.resume]` in `.fno/config.toml` or the global config. An operator can teach fno a new harness's resume form without a release, and correct a bundled one the same way.

The resume lane is stricter than the attach lane, because the resume builder honors less. It fills exactly `{session_id}`. A `{short_id}` form is attach-only. A form promising a `pre_exec` daemon start is refused, never silently ignored. A harness the table gives no form is refused by name ("codex has no resume form; session ... is not resumable"). That is the honest dead row: a button that fails is worse than no button.

Claude is the one special case, and not here. A live claude bg session is owned by its daemon and attaches through the existing path. A dead claude session resumes like everything else, through its declared `claude --resume <session_id>` form. Because a bare `claude --resume` on the main thread is unsafe, claude members first resolve a re-entry plan off the core loop. A member whose plan fails or is absent is refused, never resumed bare.

## Startup policy and the on-demand verb

Startup restore is governed by `[mux.restore] policy`. `hold` (default) rebuilds named held panes and resumes on focus. `idle` leaves members as idle rows. `resume` runs the bulk restore at startup, without being asked. The verb is the same bulk resume, on demand. A session that started under `hold` or `idle` can be restored in one command later. A script can drive the whole reboot-recovery without opening the TUI. See [pane-worker-relaunch](pane-worker-relaunch.md) for the held-pane and idle-row mechanics this builds on.

## A live proof

The mechanism was proven end to end with two harnesses. A claude-only proof cannot distinguish "restore works" from "the one hardcoded arm works".

1. Two workers were planted in an isolated mux session: a claude pane worker and a codex worker. Each was told to memorize the token `restore-proof-7b5e`. The claude transcript recorded 3 occurrences, the codex rollout 11 - positive markers only the real sessions produce.
2. The mux server was killed with SIGKILL - a reboot, not a graceful exit.
3. A fresh server started from the same state directory. `fno mux workspace restore --json` reported: `d71b515a` (claude) resumed, `01a04efc` (codex) resumed, zero refused.
4. Both workers were asked "What is the token?". The claude pane answered `restore-proof-7b5e`. The codex pane answered `restore-proof-7b5e`. The answers came from context the restore reconstructed. They were read from the panes themselves, never from a pane count or an exit code.

The proof ran on the state just before the reader unification. The unification keeps the same bundled table and the same rules, and the token pinning tests assert them against the TOML text.

## Files

- `crates/fno/src/server.rs` - `declared_resume_form` (the thin view), `resume_one` (the shared gate walk), `workspace_restore_start` / `workspace_restore_apply` (the bulk driver), `restore_candidates`
- `crates/fno/src/agents_view.rs` - `declared_form` (the one reader), `FormLane`, `resume_form`, `attach_form`
- `crates/fno/src/mux_cli.rs` - `workspace` / `workspace_restore` CLI parsing and output
- `crates/fno/src/proto.rs` - `ControlVerb::WorkspaceRestore`, `ServerMsg::WorkspaceRestored`, `RestoreRow`, the `RESTORE_NOT_RUN` error class
- `crates/fno/src/digest_overlay.rs` - `MuxRestorePolicy` (hold | idle | resume)
- `crates/fno/tests/server_spine.rs` - the wire-tolerance arms for the new reply
