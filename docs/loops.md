# Scheduled loops

The control plane runs every scheduled loop below. One row names its scheduler, its arming key, its journal receipt, and the verb that reads it in detail. This file is generated. Regenerate it with `fno agents loops table --markdown` after changing `KNOWN_ARMS` in `crates/fno-agents/src/tick_ledger.rs`. When a row reads STALE or FAIL, `fno agents loops table` exits 1. Its plain form prints the same rows against the live journals.

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
| `king_settle` | `daemon` | 300 | always | `control_plane_tick` | `fno agents court --nodes` |
| `provider_cap` | `daemon` | 120 | always | `control_plane_tick` | `fno agents loops table` |
| `slot_cutover` | `daemon` | 120 | `slot_cutover.enabled` | `control_plane_tick` | `fno agents loops table` |
| `merge_close` | `daemon` | 900 | always | `control_plane_tick` | `fno agents loops table` |
| `crown_ledger` | `daemon` | 300 | always | `control_plane_tick` | `fno agents loops table` |
| `fleet_page` | `daemon` | 1800 | always | `control_plane_tick` | `fno agents loops table` |
| `attention` | `daemon` | 30 | always | `control_plane_tick` | `fno agents loops table` |
| `heal` | `launchd:sh.fno.pr-watcher` | 600 | `auto_heal.enabled` | `pr_heal_tick` | `fno do pr watch status` |

The launchd labels the pr-watch installer and the autocorrect installer own, as the table reports them: `sh.fno.pr-watcher`, `sh.fno.groom`, `sh.fno.autocontinue`, `sh.fno.sync-backlog`, `sh.fno.board-server`, `com.user.autocorrect`, `com.user.autocorrect-watcher`. A label the fold shows as `not loaded` cannot run. A nonzero last exit is one run that failed. `fno doctor` lists it under `launch_agents`.

### king_wake

Start: launchd:sh.fno.pr-watcher fires, every 900s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### watchdog

Start: launchd:sh.fno.pr-watcher fires, every 600s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### pr_watch_merge

Start: launchd:sh.fno.pr-watcher fires, every 600s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno do pr watch status`. If the row reads red, its `cause=` suffix names the next read.

### pr_watch_sweep

Start: launchd:sh.fno.pr-watcher fires, every 600s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno do pr watch status`. If the row reads red, its `cause=` suffix names the next read.

### active_backlog

Start: daemon fires, every 300s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno config active-backlog`. If the row reads `unarmed`, arm it with `fno config set active_backlog.enabled true`.

### auto_continue

Start: session fires, every 1800s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### notify_watch

Start: launchd:sh.fno.pr-watcher fires, every 300s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### stop_hook

Start: hook:target-stop-hook fires, every 0s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### reap

Start: daemon fires, every 60s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### retire

Start: daemon fires, every 300s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### machine_watch

Start: daemon fires, every 300s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### arm_watch

Start: daemon fires, every 300s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### king_settle

Start: daemon fires, every 300s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents court --nodes`. If the row reads red, its `cause=` suffix names the next read.

### provider_cap

Start: daemon fires, every 120s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### slot_cutover

Start: daemon fires, every 120s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads `unarmed`, add `[slot_cutover] enabled = true` to the daemon's `config.toml`.

### merge_close

Start: daemon fires, every 900s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### crown_ledger

Start: daemon fires, every 300s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### fleet_page

Start: daemon fires, every 1800s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### attention

Start: daemon fires, every 30s. End: a `control_plane_tick` receipt lands in the journal. If it looks wrong, run `fno agents loops table`. If the row reads red, its `cause=` suffix names the next read.

### heal

Start: launchd:sh.fno.pr-watcher fires, every 600s. End: a `pr_heal_tick` receipt lands in the journal. If it looks wrong, run `fno do pr watch status`. If the row reads `unarmed`, arm it with `fno config set auto_heal.enabled true`.
