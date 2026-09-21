# Scheduled loops

Every scheduled loop the control plane runs, one row each: the scheduler that fires it, the config key that arms it, the journal receipt it ends in, and the verb that reads it in detail. This file is generated; regenerate it with `fno agents loops table --markdown` after changing `KNOWN_ARMS` in `crates/fno-agents/src/tick_ledger.rs`. `fno agents loops table` prints the same rows against the live journals, and exits 1 only when a row reads STALE or FAIL.

| loop | scheduler | interval (s) | armed by | ends with receipt | reads with |
|---|---|---|---|---|---|
| `king_wake` | `launchd:sh.fno.pr-watcher` | 900 | always | `control_plane_tick` | `fno agents loops table` |
| `watchdog` | `launchd:sh.fno.pr-watcher` | 600 | always | `control_plane_tick` | `fno agents loops table` |
| `pr_watch_merge` | `launchd:sh.fno.pr-watcher` | 600 | always | `control_plane_tick` | `fno do pr watch status` |
| `pr_watch_sweep` | `launchd:sh.fno.pr-watcher` | 600 | always | `control_plane_tick` | `fno do pr watch status` |
| `active_backlog` | `daemon` | 300 | `active_backlog.enabled` | `control_plane_tick` | `fno config active-backlog` |
| `auto_continue` | `session` | 1800 | always | `control_plane_tick` | `fno agents loops table` |
| `notify_watch` | `launchd:sh.fno.pr-watcher` | 300 | always | `control_plane_tick` | `fno agents loops table` |
| `stop_hook` | `hook:target-stop-hook` | 0 | always | `control_plane_tick` | `fno agents loops table` |
| `reap` | `daemon` | 60 | always | `control_plane_tick` | `fno agents loops table` |
| `retire` | `daemon` | 300 | always | `control_plane_tick` | `fno agents loops table` |
| `machine_watch` | `daemon` | 300 | always | `control_plane_tick` | `fno agents loops table` |
| `arm_watch` | `daemon` | 300 | always | `control_plane_tick` | `fno agents loops table` |
| `provider_cap` | `daemon` | 120 | always | `control_plane_tick` | `fno agents loops table` |
| `merge_close` | `daemon` | 900 | always | `control_plane_tick` | `fno agents loops table` |
| `crown_ledger` | `daemon` | 300 | always | `control_plane_tick` | `fno agents loops table` |
| `fleet_page` | `daemon` | 1800 | always | `control_plane_tick` | `fno agents loops table` |
| `attention` | `daemon` | 30 | always | `control_plane_tick` | `fno agents loops table` |
| `heal` | `launchd:sh.fno.pr-watcher` | 600 | `auto_heal.enabled` | `pr_heal_tick` | `fno do pr watch status` |
| `blueprinter` | `daemon` | 300 | always | `control_plane_tick` | `fno agents blueprint-feed --scope <s>` |

The launchd labels the pr-watch installer and the autocorrect installer own, as the table reports them: `sh.fno.pr-watcher`, `sh.fno.groom`, `sh.fno.autocontinue`, `sh.fno.sync-backlog`, `sh.fno.board-server`, `com.user.autocorrect`, `com.user.autocorrect-watcher`. A label the fold shows `not loaded` cannot run; a nonzero last exit is one run that failed, and `fno doctor` lists it under `launch_agents`.

### king_wake

Starts when launchd:sh.fno.pr-watcher fires, every 900s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### watchdog

Starts when launchd:sh.fno.pr-watcher fires, every 600s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### pr_watch_merge

Starts when launchd:sh.fno.pr-watcher fires, every 600s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno do pr watch status`, and read the row's `cause=` suffix if it reads red.

### pr_watch_sweep

Starts when launchd:sh.fno.pr-watcher fires, every 600s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno do pr watch status`, and read the row's `cause=` suffix if it reads red.

### active_backlog

Starts when daemon fires, every 300s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno config active-backlog`, and arm it with `fno config set active_backlog.enabled true` if the row reads `unarmed`.

### auto_continue

Starts when session fires, every 1800s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### notify_watch

Starts when launchd:sh.fno.pr-watcher fires, every 300s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### stop_hook

Starts when hook:target-stop-hook fires, every 0s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### reap

Starts when daemon fires, every 60s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### retire

Starts when daemon fires, every 300s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### machine_watch

Starts when daemon fires, every 300s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### arm_watch

Starts when daemon fires, every 300s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### provider_cap

Starts when daemon fires, every 120s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### merge_close

Starts when daemon fires, every 900s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### crown_ledger

Starts when daemon fires, every 300s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### fleet_page

Starts when daemon fires, every 1800s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### attention

Starts when daemon fires, every 30s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents loops table`, and read the row's `cause=` suffix if it reads red.

### heal

Starts when launchd:sh.fno.pr-watcher fires, every 600s. Ends when a `pr_heal_tick` receipt lands in the journal. When it looks wrong, run `fno do pr watch status`, and arm it with `fno config set auto_heal.enabled true` if the row reads `unarmed`.

### blueprinter

Starts when daemon fires, every 300s. Ends when a `control_plane_tick` receipt lands in the journal. When it looks wrong, run `fno agents blueprint-feed --scope <s>`, and read the row's `cause=` suffix if it reads red.
