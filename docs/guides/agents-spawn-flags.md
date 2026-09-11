# `fno agents spawn` flag reference

The long-form prose behind the `--help` one-liners. Each section here is the full contract; the CLI carries the one line that decides a pass/refuse.

## Placement flags

`--workspace/-s` sends the new pane to a workspace by its visible name instead of the cwd-derived default. `--split/-x` tiles the new pane left|right|up|down of the squad's focused pane. `--at` pins the new pane next to the calling pane: `--at current` resolves the caller from `FNO_PANE` (run inside a mux pane) and fails closed instead of falling back; it requires `--split`. `--tab` places the pane by tab selector: a bare number is the visible 1-based ordinal, `id:<n>` the stable tab id, `name:<s>/ordinal:<n>/active/new` explicit forms; a bare name is a pane group (reuse the first numbered group tab with room, or create the next). All placement flags imply `--substrate pane`.

`--bounded-placement` (hidden) is the spawn-side lane for automated placement, e.g. the outage handoff's successor spawn: placement is serialized under the mux lease, a stable tab with room is selected, and at most four panes per tab are enforced.

## Identity and routing

`--harness/-H` is the CLI binary to launch. The declared harnesses get the full lane; any other binary on PATH also spawns, into a pane with fno as the viewport (pass its init flags after `--`). `-H` no longer means headless; use `--substrate headless`/`--headless`/`--once`.

`--provider/-P` names the model VENDOR the harness talks to (zai or any `model_routing.providers` name); pair with `--model` to name the route. It is not the CLI binary. `--route provider/model` bypasses the `--role` table and wins over any configured lane, and FAILS CLOSED: an unknown provider, non-anthropic protocol, or missing key refuses the spawn. `--role` routes auxiliary roles (coordinate|tidy|orient|consolidate|post-merge) and the delivery lane to a secondary provider when configured; production roles and the default stay primary.

`--model/-m` is forwarded as-is to the provider's own CLI (exact passthrough). On the default pane substrate every provider honors it; on thread/headless it reaches claude, codex, and agy.

## Credentials

`--account` pins ONE worker to a registered claude account without touching the daemon-wide active `~/.claude` slot. An account with its own config_dir sets `CLAUDE_CONFIG_DIR` (bills right); a managed account rides the shared slot only when it IS the active occupant; a managed non-active account is refused (pointing at config-dir registration: the setup-token env lane bills the wrong account). claude only, fail-closed.

`--dispatch-account` carries a provider RECORD chosen by `fno agents dispatch resolve --autonomous`: its dispatch env rides for ANY harness, which is what a claude-to-codex cutover needs. The record id travels on argv; its credentials never do. Fail-closed.

## Session shape

`--substrate` picks thread (the default where the harness seats one; persistent, viewed through a portal), pane (mux-hosted PTY, the closable fallback), or headless (one-shot). `bg` is a deprecated alias for thread. `--resume/-r` seeds a NEW claude session from an existing transcript: content carries over, the session id does NOT (the result is a new id, a new agent-view row, a new fno binding). To bring a session back under its OWN id use `fno agents resume`. Accepts a full uuid or the 8-hex short id; implies thread. claude + thread only.

`--session-phase` stamps the sessions row a node-bearing spawn opens on the node: empty infers from the message (`/target`-family work stamps do, everything else stamps review); no node resolved means no row.

## Crowns

`--crown/-k` grants an orchestrator crown over the named territory: epic id(s) crown a Director over the set, ONE project name a project king, SEVERAL projects a portfolio. Altitude derives from what you name: there is no `--level`, and a non-epic node is refused, since implementers get no crowns. The grantor derives from THIS session, never self-declared. Works on pane and thread (thread crowns are claude-only until the court plumbing learns the opencode serve lane); refused on headless, whose one-shot exits before it can reign. If the caller already holds the named territory, `--succeed` transfers the crown and strips the caller atomically.

## Misc

Without `--name`, an adjective-noun slug is minted; the one positional is the prompt, not the name. `--sandbox-write-policy` names a JSON policy whose `sandbox` block composes into the worker's ONE `--settings` file alongside the hook layer's `deny_edit` list; refused on the pane substrate, where mux_spawn reserves `--settings` for its hook server. `--node` exports FNO_NODE/FNO_SLUG/FNO_PLAN into the pane so the prompt renders provenance; FNO_SLUG/FNO_PLAN resolve from the graph unless `--slug`/`--plan` override. `--headless/-p` is a shortcut for `--substrate headless`; `-p` mirrors the harnesses' own one-shot short, so the vendor axis takes capital `-P`.
