# `fno agents spawn` flag reference

The long-form prose behind the `--help` one-liners. The CLI carries the one line that decides a pass or a refuse. This page carries the full contract.

## Placement flags

`--workspace/-s` sends the new pane to a workspace by its visible name. Without it the pane lands in the cwd-derived default.

`--split/-x` tiles the new pane left, right, up, or down of the squad's focused pane.

`--at` pins the new pane next to the calling pane. `--at current` resolves the caller from `FNO_PANE`, so run it inside a mux pane. It fails closed instead of falling back. It requires `--split`.

`--tab` places the pane by tab selector. A bare number is the visible 1-based ordinal. `id:<n>` is the stable tab id. `name:<s>`, `ordinal:<n>`, `active`, and `new` are explicit forms. A bare name is a pane group: reuse the first numbered group tab with room, or create the next.

Every placement flag implies `--substrate pane`.

`--bounded-placement` (hidden) is the spawn-side lane for automated placement, such as the outage handoff's successor spawn. Placement serializes under the mux lease. The selector picks a stable tab with room. At most four panes per tab are enforced.

## Identity and routing

`--harness/-H` names the CLI binary to launch. The default is the invoking harness, then claude. The declared harnesses get the full lane. Any other binary on PATH also spawns, into a pane with fno as the viewport. Pass its init flags after `--`. `-H` no longer means headless. For a one-shot use `--substrate headless`.

`--provider/-P` names the model vendor the harness talks to: zai or any `model_routing.providers` name. Pair it with `--model` to name the route. It is not the CLI binary. The CLI binary is `--harness/-H`.

`--route provider/model` bypasses the `--role` table and wins over any configured lane. It fails closed: an unknown provider, a non-anthropic protocol, or a missing key refuses the spawn. claude only.

`--role` selects a routing role. If configuration names a secondary provider, the auxiliary roles (coordinate, tidy, orient, consolidate, post-merge) and the delivery lane route to it. Production roles and the default stay primary.

`--model/-m` is forwarded as-is to the provider's own CLI. There is no fuzzy resolution. On the default pane substrate every provider honors it. On thread or headless it reaches claude, codex, and agy.

## Credentials

`--account` pins ONE worker to a registered claude account. The daemon-wide active `~/.claude` slot stays untouched. An account with its own config_dir sets `CLAUDE_CONFIG_DIR`, which bills right. A managed account that IS the active occupant rides the shared slot. Any other managed account is refused, with a pointer to config-dir registration: the setup-token env lane bills the wrong account. claude only, fail-closed.

`--dispatch-account` carries a provider RECORD chosen by `fno agents dispatch resolve --autonomous`. Its dispatch env rides for ANY harness, which is what a claude-to-codex cutover needs. The record id travels on argv. Its credentials never do. An unknown or unstageable record spawns nothing.

## Session shape

`--substrate` picks the session shape. `thread` is the default where the harness seats one: persistent, viewed through a portal. `pane` is the mux-hosted PTY, the closable fallback. `headless` is a one-shot. `bg` is a deprecated alias for thread.

`--resume/-r` seeds a NEW claude session from an existing transcript. The content carries over. The session id does NOT: the result is a new id, a new agent-view row, and a new fno binding. To revive a session under its OWN id use `fno agents resume`. The value is a full uuid or the 8-hex short id. It implies thread. claude and thread only.

`--session-phase` stamps the sessions row that a node-bearing spawn opens on the node. Empty infers from the message: `/target`-family work stamps do, everything else stamps review. No resolved node means no row.

## Crowns

`--crown/-k` grants an orchestrator crown over the named territory. Repeat it with epic id(s) to crown a Director over the set. One project name crowns a project king. Several projects crown a portfolio. The altitude derives from what you name. There is no `--level`. A node that is not an epic is refused, because implementers get no crowns. The grantor derives from THIS session and is never self-declared. Crowns work on the pane and thread substrates. Thread crowns are claude-only until the court plumbing learns the opencode serve lane. Headless refuses, because its one-shot exits before it can reign. If the caller already holds the named territory, add `--succeed`: that flag transfers the crown and strips the caller atomically.

## Misc

Without `--name`, an adjective-noun slug is minted. The one positional is the prompt, not the name.

`--sandbox-write-policy` names a JSON policy file. Its `sandbox` block composes into the worker's ONE `--settings` file, beside the hook layer's `deny_edit` list. The pane substrate refuses it, because mux_spawn reserves `--settings` for its hook server.

`--node` exports FNO_NODE, FNO_SLUG, and FNO_PLAN into the pane, so the prompt renders provenance. Ad-hoc spawns omit it. FNO_SLUG and FNO_PLAN resolve from the graph. `--slug` and `--plan` override that read.

`--headless/-p` is a shortcut for `--substrate headless`. `-p` mirrors the harnesses' own one-shot short. The vendor axis therefore takes the capital `-P`.
