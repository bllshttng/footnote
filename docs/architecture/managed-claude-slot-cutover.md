# Managed Claude slot cutover

`slot_cutover` is an opt-in daemon arm that moves the shared `~/.claude` account before its usage window is exhausted. It runs every 120 seconds and defaults to off.

The arm reads managed Claude records that use the shared slot. When the current account identity is proven and usage is low or exhausted, it acts. It chooses the first other shared-slot record with a fresh usage window below the low threshold. A five-minute cooldown limits repeated switches.

The arm switches with `fno config accounts use <account-id> --scope global`, then checks that the slot now proves the selected account. Failed switches and failed identity checks produce a tick receipt and an operator notification. A successful switch records its timestamp for the cooldown.

The arm does not stop the daemon or restart Claude workers. Claude sessions that use the shared slot resolve the account from the managed credential slot.

To enable it, add this table to the daemon's project or global `config.toml`:

```toml
[slot_cutover]
enabled = true
```

The curated `fno config set` schema does not expose this Rust-owned setting, so edit the TOML file directly. Keep it off until the managed credential vault can refresh and prove every target account. Check the arm with `fno agents loops table`. When the arm is disabled, its row reads `unarmed`.
