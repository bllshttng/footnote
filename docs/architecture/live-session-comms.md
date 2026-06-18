# Live-session comms + a2a relay toggle

Two independent subsystems shipped together. They share no code path (Locked
Decision 1): Group A makes live, hand-started `claude --bg` sessions
discoverable and addressable; Group B gives the unrelated adopted-thread a2a
relay a discoverable config surface.

## Group A — live-session comms

### P1: discover + name (transport-free)

`fno agents list` grows a distinct "discovered live sessions" lane for
host-local, un-adopted Claude Code sessions, addressable by a legible handle
without a UUID. It is a pure read over Claude Code's own on-disk registry
(`~/.claude/sessions/<pid>.json`) — no MCP / `register-channel` dependency
(Locked Decision 3), host-local because PID liveness is per-machine (Locked
Decision 8).

- **Engine:** `cli/src/fno/agents/discover.py`.
  - Strict `^\d+\.json$` filename guard + explicit `.sync-conflict-*` skip, so
    a 7000-entry directory of iCloud conflict files and `<uuid>-*.md`
    transcripts is never parsed (bounded to a readdir + ~N stat/parse).
  - **Reuse-safe liveness:** the PID must be alive on this host AND its OS
    create-time must match the registry's `procStart`. Claude Code renders
    `procStart` in **UTC** (verified on 2.1.169), while Python `time.ctime`
    renders local time, so `_ctime_matches` compares against both the UTC
    (`asctime(gmtime)`) and local renderings (+/-1s). A reused PID (different
    create-time) is never shown live — the exact bug the claim hardening fixed.
  - **Project resolution** (`resolve_project_for_cwd`): a worktree cwd
    (`<root>/.claude/worktrees/<name>` or `~/conductor/workspaces/<repo>/<name>`)
    resolves to its parent repo's settings project, not an orphan.
  - **Friendly-name overlay:** `~/.fno/session-names.json` maps each live
    `sessionId` to a stable, unique alias (default `<project>-<short-id>`),
    written atomically under a file lock. Dead `sessionId`s are retired on the
    next scan that still sees a live session; a fully-empty scan never rewrites
    the map (so a transient probe miss cannot wipe hand-edited aliases).

The real `fno agents list` auto-routes to the Rust client (`os.execv`), which
owns the rendered surface. The Rust `list` render therefore shells out
fail-open to an internal Python helper, `fno agents discovered-json`
(`FNO_AGENTS_RUNTIME=python` pins it so it cannot recurse), and folds the lane
into both `render_list_json` (additive `discovered_sessions` /
`discovered_count` keys, `schema_version` 2) and a distinct
`render_list_table` section. Discovery lives in Python because it needs
`psutil`'s cross-platform process create-time; the Rust-native liveness
degrades to existence-only on macOS. `--no-discovered` skips the scan.

### US2: send by handle

`fno mail send <handle> <msg>` accepts a discovered handle (alias or hex
short-id), not just a registered agent name. On the unknown-agent path only,
`discover.resolve_or_suggest` maps the handle to its session's project and the
send rides the EXISTING `--to-project` durable bus (Locked Decision 2:
live-to-live comms is async over the bus, never a live injection into a
human-driven session). An unresolved handle errors with the closest live
handles, sending nothing.

### P2: surface at the loop-yield boundary

An autonomous `/target` loop submits no user prompt and starts no new session
between iterations, so neither inbox-reminder hook fires; a message sits unseen
until the loop yields to a human. When `fno-agents loop-check` returns a
`block` decision, it now appends a one-line nudge for the oldest unread message
addressed to the session's project to the decision `message`. **Vehicle
(AC3-VERIFY):** the stop hook surfaces a `block` message to the continuing
model via `exit 2` + stderr (Claude Code's documented Stop-hook block
protocol) — the footnote loop already relies on it, so P2 needs no new
substrate.

- `crates/fno-agents/src/nudge.rs` (deliberately OUTSIDE the `loop*`
  loc-ratchet glob) shells out fail-open to `fno agents nudge-peek`;
  `loopcheck.rs` only wraps its two block-message returns (+6 LOC, the budgeted
  exception).
- `cli/src/fno/agents/nudge.py` finds the oldest unread for the project
  and records it in a per-session nudged-id set (pruned to the current unread
  set) so it surfaces exactly once (AC3-FR). It never acks the bus cursor — the
  durable copy stays for the drain / human. A reply (`in_reply_to`) is
  attributed as such, closing the US4 round-trip.

## Group B — a2a relay toggle

`config.agents.a2a.{auto,turn_ceiling}` governs the autonomous relay between
adopted headless threads. It previously had zero command surface.

- **`fno config set <key> <value>`** (`cli/src/fno/config/writer.py`): a
  write companion to the read-only `get`. The value is coerced to the leaf
  field's schema type and validated by constructing the changed block in
  isolation (so `turn_ceiling >= 1` fires while unrelated keys like `work:` are
  untouched), then written atomically (temp + `os.replace`) under a file lock.
  An invalid value leaves the file unchanged and exits non-zero.
- **First-use confirm** (`dispatch._a2a_first_use_gate`): the first time the
  relay would fire its first autonomous hop, an interactive run asks once and
  persists the answer (so it never re-asks). A headless / no-TTY run applies
  the CONSERVATIVE fallback (autonomous relay OFF, a single observed hop)
  **regardless of the configured `auto` default** and is never blocked (Locked
  Decision 7 / F4) — headless must never inherit `auto:true` and silently burn
  plan credit. Only the autonomous relay is gated; the single first hop still
  runs.
