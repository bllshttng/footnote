# Workspace restore

After a reboot or a killed mux server, every worker pane is gone. A pane's pty was a child of the server pid and died with it. What survives is durable identity: the squad store's member records, the agents registry, and each harness's own persisted session state. `fno mux workspace restore` walks the members and brings each one back through its own harness's resume form. The workspace is reconstructed, never survived. An open portal comes back too, as a held seat. Its `(index, row_key)` slot is stored in the tab's tree, and restore puts the entry back in the portal map with a shell in the seat.

## Is this page for you?

You are bringing worker panes back after a reboot or a killed mux server, or choosing `[mux.restore] policy` in your config. This page owns what `hold`, `idle`, and `resume` do to worker members, and what the on-demand verb relaunches. Misreading it picks the wrong knob for your symptom: on 2026-09-04 an operator set `idle` to cut a 28-tab restore, and `idle` does not govern tabs at all.

Not for: the tab count. Tabs rebuild from each squad's stored tab trees under every policy value, so no value restores zero tabs; only `hold` skips tabs whose every slot binds a done worker. For held-pane and idle-row mechanics see [pane-worker-relaunch](pane-worker-relaunch.md); for what a client reconnect preserves versus a server restart see [mux-restart-recovery](mux-restart-recovery.md).

The destructive edge: `resume`, as the startup policy or the on-demand verb, relaunches every worker member that is not tombstoned, gone, or reap-retired. Finished-and-merged work relaunches too. Run `fno mux workspace restore --dry-run --json` first; it classifies and spawns nothing.

## The verb

```bash
fno mux workspace restore [--dry-run] [--harness <harness>] [--json]
```

The verb enumerates every live, non-tombstoned worker member in the squad store. It joins each to its registry row and resumes it: the pane runs the harness's own resume argv with the member's full session id. `--dry-run` classifies every member and spawns nothing. `--harness` narrows the run to one harness. `--json` prints one row per member with its outcome, so a script can branch without parsing prose.

Every member that cannot come back is named, with the reason. The reasons include: no registry row, no session id, a harness the table gives no resume form, an ambiguous name, a failed spawn. Silence is never an outcome. A run that resumes two and refuses one prints all three.

A member the registry forgot and the spawn journal never received does not come back. Restore retires it from the store: no pane, no dim card, no member row on the next persist. The restore receipt names the count. Nothing that does not belong returns.

Two preconditions are refusals, not empty results. The verb refuses before the session's first real attach. At that point startup restore has not run and the persisted squads were never read. Answering "nothing to restore" there is a lie. It also reads the registry file itself before classifying. The off-loop registry reader ticks independently, and a headless restore can otherwise refuse every member with "no such agent" while its row sits on disk.

The server's member list is authoritative while the server runs. The squad store file is that list's persist target, not its source. Every pane event re-writes the file from memory. So any write to `squads.json` from outside a live server, such as `fno mux workspace prune`, must be followed by the `SquadReload` control verb to every answering session. Without the reload, the next pane event writes the old members back over the pruned file. Restore then reads memory-shaped rows, not the file a prune just shrank.

## The declared resume form

The resume argv is not hardcoded. Each harness declares an `interactive_resume` form in the capability table (`cli/src/fno/agents/harness_capabilities.toml`). The server reads that declaration in process through the same reader the attach lane uses (`agents_view::resume_form`, the resume front door over `declared_form`). The bundled table is embedded at build time. An operator can override it per harness with `[harness.<name>.resume]` in `.fno/config.toml` or the global config. An operator can teach fno a new harness's resume form without a release, and correct a bundled one the same way.

The resume lane is stricter than the attach lane, because the resume builder honors less. It fills exactly `{session_id}`. A `{short_id}` form is attach-only. A form promising a `pre_exec` daemon start is refused, never silently ignored. A harness the table gives no form is refused by name ("codex has no resume form; session ... is not resumable"). That is the honest dead row: a button that fails is worse than no button.

Claude is the one special case, and not here. A live claude bg session is owned by its daemon and attaches through the existing path. A dead claude session resumes like everything else, through its declared `claude --resume <session_id>` form. Because a bare `claude --resume` on the main thread is unsafe, claude members first resolve a re-entry plan off the core loop. A member whose plan fails or is absent is refused, never resumed bare.

## Startup policy and the on-demand verb

Startup restore is governed by `[mux.restore] policy`. `hold` (default) rebuilds named held panes and resumes on focus. `idle` leaves members as idle rows. `resume` runs the bulk restore at startup, without being asked. The verb is the same bulk resume, on demand. A session that started under `hold` or `idle` can be restored in one command later. A script can drive the whole reboot-recovery without opening the TUI. See [pane-worker-relaunch](pane-worker-relaunch.md) for the held-pane and idle-row mechanics this builds on. A held portal follows the same shape: it resumes on focus of its seat, or on the row's reach.

## A live proof

The mechanism was proven end to end with two harnesses. A claude-only proof cannot distinguish "restore works" from "the one hardcoded arm works".

1. Two workers were planted in an isolated mux session: a claude pane worker and a codex worker. Each was told to memorize the token `restore-proof-7b5e`. The claude transcript recorded 3 occurrences, the codex rollout 11 - positive markers only the real sessions produce.
2. The mux server was killed with SIGKILL - a reboot, not a graceful exit.
3. A fresh server started from the same state directory. `fno mux workspace restore --json` reported: `d71b515a` (claude) resumed, `01a04efc` (codex) resumed, zero refused.
4. Both workers were asked "What is the token?". The claude pane answered `restore-proof-7b5e`. The codex pane answered `restore-proof-7b5e`. The answers came from context the restore reconstructed. They were read from the panes themselves, never from a pane count or an exit code.

The proof ran on the state just before the reader unification. The unification keeps the same bundled table and the same rules, and the token pinning tests assert them against the TOML text.

## Questions

Will `mux.restore.policy = idle` reduce my tab count?

No, and it can raise it. Tabs rebuild from the stored tab trees under every policy (`crates/fno/src/server.rs:8210`); only `hold` skips a tab whose every slot binds a done worker, because the doneness gate sits inside the hold branch (`crates/fno/src/server.rs:7976`). Under `idle` a finished worker keeps its tab where `hold` would skip it.

*Graduates to:* the doneness gate running before the policy branch, so every policy skips finished workers' tabs.

What happens to my tab trees when the mux server is killed?

The store keeps the layout that the last topology write left. A SIGKILL writes nothing at death. A tab close writes its tree removal first (`closing_a_shell_tab_writes_its_tree_removal`, `crates/fno/src/server.rs:19380`), and `kill-server` captures the layout on the way down (`CoreMsg::Kill` -> `bye_all` -> `Flow::Shutdown`, `crates/fno/src/server.rs:13021`). Tabs open at each death persist, so trees accrete across server deaths. At the next start, `hold` skips tabs whose every slot binds a done worker.

What does `resume` respawn?

Every worker member that is not tombstoned, not gone, and not retired by a reap receipt (`crates/fno/src/server.rs:6414`, `:6521`). That includes done-and-merged workers. Measured 2026-09-11 with `fno mux workspace restore --dry-run --json`: 10 members classified, 2 planned for relaunch, 2 already live, 6 refused (4 retired, 2 not resumable). Dry-run spawns nothing.

Restore printed `never bound`. Is that session gone?

Not necessarily. The label means fno holds no session id for the member and the spawn journal positively records the registry row was removed with an empty session field (`crates/fno/src/restore_liveness.rs:73`, `crates/fno/src/spawn_journal.rs:334`). It describes fno's reach, not the harness transcript's existence. fno cannot resume a session it holds no id for; the harness itself can, given the session id.

## Files

- `crates/fno/src/server.rs` - `declared_resume_form` (the thin view), `resume_one` (the shared gate walk), `workspace_restore_start` / `workspace_restore_apply` (the bulk driver), `restore_candidates`
- `crates/fno/src/agents_view.rs` - `declared_form` (the one reader), `FormLane`, `resume_form`, `attach_form`
- `crates/fno/src/mux_cli.rs` - `workspace` / `workspace_restore` CLI parsing and output
- `crates/fno/src/proto.rs` - `ControlVerb::WorkspaceRestore`, `ServerMsg::WorkspaceRestored`, `RestoreRow`, the `RESTORE_NOT_RUN` error class
- `crates/fno/src/digest_overlay.rs` - `MuxRestorePolicy` (hold | idle | resume)
- `crates/fno/tests/server_spine.rs` - the wire-tolerance arms for the new reply
