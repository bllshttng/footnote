"""Presentation registry for the config schema.

The drift-killer bridge. The Pydantic ``SettingsModel`` owns what a key IS
(type + default + validation); this sidecar owns how each leaf is PRESENTED:
whether ``/fno:setup`` asks about it, the question text, where a smart default
comes from, and a one-line doc blurb for the generated reference.

Presentation lives here, NOT on ``Field(...)``, so the validation model stays
clean and there is exactly one place to answer "what does the wizard ask?".

CI enforces ``FIELD_META`` is COMPLETE: every model leaf (see
``schema_gen.all_leaf_paths``) must have an entry here, so a new field cannot
land without a conscious wizard/doc disposition.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


@dataclass(frozen=True)
class Meta:
    """Presentation disposition for one model leaf.

    wizard: one of "always" (a real per-project decision the wizard asks every
        time), "advanced" (asked only under ``/fno:setup advanced``), or "never"
        (defaulted silently / not surfaced).
    doc: one-line blurb for the generated configuration reference.
    question: the wizard prompt (used for always/advanced).
    default_source: how a smart default is derived, if any (e.g. "repo-slug",
        "readme", "auto-detect"); informational for the wizard.
    """

    wizard: str
    doc: str
    question: str = ""
    default_source: str = ""


# Every leaf maps to exactly one Meta. Keep in rough model order for scanning.
FIELD_META: dict[str, Meta] = {
    "schema_version": Meta("never", "Settings schema version; managed by fno, not hand-set."),
    # --- config.paths.* (all defaulted; advanced) ---
    "state_dir": Meta("advanced", "Root dir for global fno do state.", default_source="default"),
    "plans_dir": Meta("advanced", "Where folder plans are written.", default_source="default"),
    "plans_filename": Meta("advanced", "Plan/design-doc filename template: strftime codes plus {slug} and {node} placeholders; must render to a bare *.md name.", default_source="default"),
    "branch.prefix": Meta("advanced", "Prefix for dispatched worktree branches: <prefix>/<slug>-<node>.", default_source="default"),
    "paths.graph_json": Meta("never", "Override path to the backlog graph.json."),
    "paths.ledger_json": Meta("never", "Override path to ledger.json."),
    "paths.evals_history": Meta("never", "Override path to the evals-history.jsonl bank-run ledger."),
    "paths.briefs_dir": Meta("never", "Override path to the sidecar briefs dir."),
    "paths.fleet_dir": Meta("never", "Override path to the megatron fleet dir."),
    "paths.postmortems_dir": Meta("never", "Override path to the postmortems dir."),
    "paths.worktrees_base": Meta("never", "Override base dir for worktrees."),
    "paths.memory_dir": Meta("never", "Override path to the memory dir."),
    "paths.hook_logs_dir": Meta("never", "Override path to hook logs."),
    "paths.inbox_dir": Meta("never", "Override path to the cross-project messaging inbox dir."),
    "paths.inbox_path": Meta("never", "Override path to the capture-tier inbox/parking-lot file."),
    "paths.agents_registry_path": Meta("never", "Override path to the agents registry.json."),
    "paths.handoffs_dir": Meta("never", "Override path to the handoffs dir."),
    "paths.retro_pending_dir": Meta("never", "Override path to the retro-pending dir."),
    "paths.bus_dir": Meta("never", "Override path to the cross-project mail bus dir."),
    "paths.loops_paused_json": Meta("never", "Override path to the loops pause-all sentinel."),
    "paths.observer_reports_dir": Meta("never", "Override path to the observer harness digest dir."),
    "paths.operator_lane": Meta("never", "Override path to the operator's priorities lane."),
    # --- config.inbox.* ---
    "inbox.unclaimed_ttl": Meta(
        "advanced",
        "Seconds past which a sent-but-unclaimed bus message is surfaced back to its sender (turn-boundary nudge + `fno agents mail status`).",
        default_source="default",
    ),
    # --- config.obsidian.* (a real decision) ---
    "obsidian.enabled": Meta(
        "always", "Whether this project uses an Obsidian vault for plans/docs.",
        question="Use an Obsidian vault for plans and design docs?",
    ),
    "obsidian.vault": Meta(
        "always", "Vault area name (NOT a filesystem path).",
        question="Obsidian vault area name?", default_source="auto-detect",
    ),
    # --- config.project.* ---
    "project.id": Meta("advanced", "Project identifier.", default_source="repo-slug"),
    "project.vision": Meta(
        "always", "One-paragraph statement of what this codebase is and why.",
        question="One-line project vision (what is this and why)?",
        default_source="readme",
    ),
    # --- config.blueprint.* ---
    "blueprint.max_prs_per_epic": Meta("advanced", "Default cap on group PRs per decomposed epic; an epic plan-doc's max_children frontmatter overrides it per-epic and --max-prs may only tighten it."),
    # --- config.backlog.* ---
    "backlog.maintain.staleness_days": Meta("advanced", "Age (days) before an idea is flagged stale."),
    "backlog.maintain.max_failed_attempts": Meta("advanced", "Consecutive failures before a node auto-defers."),
    "backlog.maintain.validity_days": Meta("advanced", "Age (days) before a stale idea enters the validity sweep."),
    "backlog.maintain.validity_batch_size": Meta("advanced", "Oldest-first validity-sweep batch size (clamped to 100)."),
    "backlog.staleness_days": Meta(
        "advanced",
        "Age (days) before an unmoved ready node is quarantined from selection.",
    ),
    "backlog.id_prefix": Meta(
        "always", "Prefix for minted node IDs (<=7 chars; not cv-/fu-/tgt-).",
        question="Backlog node-ID prefix?", default_source="repo-slug",
    ),
    "backlog.id_hex_width": Meta("advanced", "Hex width of minted node IDs (4-8)."),
    # --- config.batch.* ---
    "batch.enabled": Meta("advanced", "Coalesce same-domain nodes into one batch PR (opt-in)."),
    "batch.max_nodes": Meta("advanced", "Nodes per batch before it closes (default 3)."),
    "batch.max_loc": Meta("advanced", "Optional cumulative-diff LOC ceiling for a batch (off by default)."),
    # --- config.post_merge.* ---
    "post_merge.parking_lot_path": Meta(
        "advanced", "Per-repo vault parking-lot path for the post-merge ritual (repo-relative).",
    ),
    "post_merge.maintainer_marker": Meta(
        "advanced",
        "Discriminator tag for maintainer-only post-merge items (e.g. '#maintainer'). "
        "Default empty: omit the tag entirely so a fresh install ships no one's "
        "initials. Honored by both the post-merge ritual and the capture parser; "
        "set per-repo only when the destination is a shared vault.",
    ),
    "post_merge.enabled": Meta("advanced", "Whether the post-merge ritual runs."),
    "post_merge.self_reap": Meta("never", "Whether a post-merge watcher self-reaps."),
    "post_merge.sync_command": Meta(
        "advanced",
        "Canonical-sync incantation run via `bash -lc` after a merge (e.g. "
        "`git checkout main && git pull && fno doctor update && fno agents restart`). Unset = off.",
    ),
    "post_merge.sync_paths": Meta(
        "advanced",
        "Repo-relative fnmatch globs gating the canonical sync (empty = always "
        "run; e.g. `[\"cli/**\", \"crates/**\"]` skips a docs-only merge).",
    ),
    "post_merge.auto_run": Meta(
        "advanced",
        "Let the pr-watch daemon (the sole merge detector) run `fno do pr ritual "
        "<pr> --autonomous` for a newly-merged PR (opt-in; default off). "
        "Reconcile no longer dispatches a ritual.",
    ),
    "post_merge.catchup_window_days": Meta(
        "advanced",
        "How far back the canonical-sync catch-up sweep looks for merges with no "
        "sync marker (default 3 days). Bounds the sweep so a fresh clone never "
        "re-syncs all history.",
    ),
    "post_merge.sync_stale_hours": Meta(
        "advanced",
        "How long the newest merge may sit unsynced before `fno doctor` reports "
        "the canonical checkout stale (default 24h).",
    ),
    "post_merge.model": Meta(
        "advanced",
        "Model for post-merge ritual workers (default claude-opus-5). Routing "
        "wins when a secondary provider is keyed.",
    ),
    # --- config.research.* ---
    "research.output_dir": Meta(
        "advanced",
        "Landing dir for the `fno do research` doc deliverable (brief + sources sidecar); "
        "vault area, not repo-relative. Unset => ship fails loud (never guesses).",
    ),
    "done_probes": Meta(
        "advanced", "Repo-wide ship-gate probes: shell commands loop-check runs (60s each, cap 3 per source) before it will grant DonePRGreen, alongside any a plan declares. Both lists must pass; a plan can add probes and can never silence these. A probe is an OBSERVATION - one that mutates the repo races the session's own edits, and its only backstops are the timeout and the block reason.",
    ),
    # --- config.approvals.* ---
    "approvals.authorized_principals": Meta(
        "advanced", "Effect class -> principal ids allowed to approve it; the key `*` matches every class. Absent or empty means nobody may approve, so a fresh install can inspect pending approvals but cannot decide one. Financial, signature, employment, and destructive classes stay denied regardless of what is listed here.",
    ),
    # --- config.review.* ---
    "review.github_apps": Meta(
        "advanced", "GitHub App bot logins that must have reviewed before the ship gate goes green (the GATE). Legacy alias: required_bots.",
    ),
    "review.required_bots": Meta(
        "never", "Legacy alias for config.review.github_apps (a straight rename); github_apps wins if both are set.",
    ),
    "review.optional_apps": Meta(
        "advanced", "Reviewer logins honored-if-present but NOT required: the gate never waits for them (kills the App-bot usage-limit wedge), but a blocking finding from one still holds it. Also the escape for a required App you no longer want the gate to wait on: move its login here.",
    ),
    "review.nudge": Meta(
        "advanced", "Per-App override for the bot-review nudge, as [review.nudge.<login>] tables with {review_handle, wait_minutes, ceiling, enabled}. A github_apps bot that reviews on MENTION not on push never reviews unless something posts its trigger; the loop-check stop gate posts review_handle (default from the built-in profile), waits wait_minutes (default 15), and after ceiling (default 3) unanswered nudges gives up rather than idling to the budget ceiling. enabled=false opts a repo back into plain block-and-wait. Ships populated for chatgpt-codex-connector, which wears three distinct names for one bot: match the review-author login chatgpt-codex-connector, trigger a fresh review with '@codex review', and address an in-thread reply to '@chatgpt-codex-connector'.",
    ),
    "review.reviewer_registry": Meta(
        "advanced", "Project-registered reviewers, as [review.reviewer_registry.<name>] tables with the built-in descriptor fields (kind, requires, invocation, asserts). Unioned with footnote's own reviewers so config.review.reviewers may name one; built-ins win a name collision. asserts=invocation is the honest rung for a harness skill: it proves the skill ran at the reviewed commit and claims nothing about its verdict.",
    ),
    "review.reviewers": Meta(
        "advanced", "Local-attestation reviewers (sigma | /code-review | declare, or a name from review.reviewer_registry) that produce no GitHub review: loop-check accepts a head-pinned review_attestation event as gate evidence. Lets a solo/claude-only harness express a real gate with no App bot.",
    ),
    "review.self_review_required": Meta(
        "advanced", "When true (default), a code payload floors the harness-resolved self-review reviewer (claude /code-review, codex /review) onto the required set on a stock install, so a /target that ships code is held for a head-pinned attestation instead of asking an epic leader. Set false to opt out and restore unreviewed code-PR shipping.",
    ),
    "review.hold_ttl_minutes": Meta(
        "advanced", "How long a registered review hold blocks a merge before it ages out (default 90). A review that starts registers review:branch:<branch>; fno do pr status and fno do pr merge refuse while it is live, so a merge cannot land on the pre-review code. A wedge bound, not an estimate: an expired hold clears with a receipt rather than holding the lane forever.",
    ),
    "review.peers": Meta(
        "advanced", "Harness peers run locally and gate on a head-pinned clean verdict. Scalar or {provider, model} entries need no second GitHub account; adding identity opts into legacy posted-review mode.",
    ),
    "review.peer_identity": Meta(
        "advanced", "Optional legacy carrier: the distinct machine-account login peers post their review under (must not be the author account).",
    ),
    "review.peer_token_env": Meta(
        "advanced", "Optional legacy carrier: env var holding the PAT for peer_identity used to post peer reviews to the PR.",
    ),
    "review.external_reviewers": Meta(
        "always", "Which AI reviewers /pr requests a review from (the INVOCATION list).",
        question="Which external reviewer(s) should review your PRs (gemini/codex/none)?",
    ),
    "review.agent_harnesses": Meta(
        "never", "Per-agent harness routing (claude/codex/gemini) for the cross-model review panel. Legacy alias: agent_providers.",
    ),
    "review.agent_providers": Meta(
        "never", "Legacy alias for config.review.agent_harnesses (a straight rename); agent_harnesses wins if both are set.",
    ),
    "review.agent_routes": Meta("never", "Opt-in per-agent harness/provider/model routes for named sigma sessions."),
    "review.cross_model.enabled": Meta("advanced", "Enable cross-model (codex/gemini) second-opinion review."),
    # --- config.preflight.* ---
    "preflight.required": Meta(
        "advanced", "Require a full local preflight receipt before opening a PR. Default false: CI is the merge gate and preflight is an opt-in rehearsal.",
    ),
    # --- config.target.* ---
    "target.dedupe_dead_duplicates": Meta("never", "Opt-in cleanup of provably-dead duplicate state files."),
    "target.auto_launch_on_blueprint": Meta(
        "advanced", "Auto-launch a bg /target worker when a node reaches ready via /blueprint.",
    ),
    "target.handoff.enabled": Meta("advanced", "Enable target self-handoff at pipeline boundaries."),
    "target.handoff.used_pct_trigger": Meta("never", "Context-used %% that triggers a wave-boundary handoff."),
    "target.handoff.king_used_pct_trigger": Meta("advanced", "Context-used %% that triggers a king handoff (below used_pct_trigger)."),
    "target.handoff.generation_cap": Meta("never", "Max handoff generations before refusing further delegation."),
    "target.blast.enabled": Meta("never", "Enable blast-radius routing."),
    "target.blast.downgrade": Meta("never", "Allow token-saving downgrades in blast routing."),
    "target.blast.reuse_loc_manifest": Meta("never", "Include loc-ratchet globs in the blast map."),
    "target.blast.high_blast_globs": Meta("never", "Per-project high-blast glob extensions."),
    "target.defaults.no_external": Meta("never", "Session-input default: skip external review (size-profile driven)."),
    "target.defaults.no_docs": Meta("never", "Session-input default: skip docs (size-profile driven)."),
    "target.defaults.max_iterations": Meta("advanced", "Session-input default: max pipeline iterations."),
    # --- config.agents.* ---
    "agents.a2a.auto": Meta("advanced", "Allow agents to auto-open agent-to-agent threads."),
    "agents.a2a.turn_ceiling": Meta("advanced", "Max turns in an agent-to-agent thread."),
    "agents.confirm": Meta("never", "Agent-launch confirmation policy (auto/always/never)."),
    "agents.auto_register_sessions": Meta("advanced", "Auto-join every hand-started session to the roster at SessionStart (default false = opt-in via /fno-me). Spawned workers register regardless.", default_source="default"),
    "agents.happy_routed_panes": Meta("advanced", "Launch routed claude panes through happy for remote monitoring; default false and pane-only.", default_source="default"),
    "agents.defaults.provider": Meta("advanced", "Default HARNESS for bare `fno agents spawn` / `/agent spawn` (claude/codex/gemini/agy/opencode). The key NAME says provider but the values are harness values and an explicit -H flag wins; the vendor axis in config is agents.defaults.route (-P/--provider as a flag). Empty = unset (harness inference then claude). Validated at the spawn seam.", default_source="default"),
    "agents.defaults.model": Meta("advanced", "Default model for bare spawns, forwarded as --model but provider-scoped: an unbound model (no agents.defaults.provider) is scoped to the builtin default provider (claude), NOT the ambient harness, so a spawn resolving to a different provider (e.g. a codex-ambient session, or an explicit -H codex) leaves the model to that harness rather than forcing an incompatible one that 400s after the round-trip. Bind agents.defaults.provider (or set a per-provider model) to apply a model under a non-claude harness. An explicit -m flag always wins; empty = unset (provider default). Passthrough (provider CLIs own model names).", default_source="default"),
    "agents.defaults.effort": Meta("advanced", "Default reasoning effort for bare spawns (minimal|low|medium|high|xhigh|max); an explicit --effort wins, empty = unset. Config-sourced effort degrades open on providers with no effort surface (gemini/agy).", default_source="default"),
    "agents.defaults.substrate": Meta("advanced", "Default substrate for bare spawns (pane|bg|headless); an explicit substrate wins, empty = unset (per-provider default). Config-sourced value degrades open with a warning if incompatible with the resolved provider (e.g. bg is claude-only).", default_source="default"),
    "agents.defaults.permission_mode": Meta("advanced", "Default --permission-mode for bare spawns; an explicit flag wins, empty = unset. Config-sourced value degrades open with a warning if the resolved provider cannot map it (an explicit --permission-mode stays fail-closed).", default_source="default"),
    "agents.defaults.route": Meta("advanced", "Default per-spawn route as vendor/model (e.g. zai/glm-5.3[1m]) for bare spawns, forwarded as --route. Fails closed on an unknown vendor or a missing key rather than silently billing the primary. Sits BESIDE the legacy provider field (which means harness, -H): route is position-carried (vendor/model) and so reaches the vendor axis provider could not. A config route owns the model, so a config model is not injected alongside it; an explicit --route wins, empty = unset.", default_source="default"),
    "agents.defaults.account": Meta("advanced", "Default claude account pin for bare spawns, forwarded as --account; an explicit --account wins, empty = unset.", default_source="default"),
    "agents.profiles": Meta("advanced", "Per-verb spawn-defaults overlay keyed by the seed's leading slash-verb. A profile may set scalar fields or a strict ordered lanes list whose links use {provider,model,effort,substrate,permission_mode,route,account,pane_group}. Lane fields sit one rung above profile scalars; explicit flags still win. Malformed lanes refuse at the spawn seam rather than billing an unintended vendor.", default_source="default"),
    "agents.pane_group_max": Meta("advanced", "Maximum panes placed in one named pane_group tab before a spawn creates the next numbered sibling tab (default 4).", default_source="default"),
    "agents.fallback": Meta("advanced", "Ordered fallback chain per node size, consulted ONLY when a provider refuses and the account queue cannot answer (agents.fallback.<S|M|L|default> = a list of {harness,model,effort,substrate,permission_mode,route,account} links). The operator rule 'simple work to a claude sonnet bg thread, complex work to codex' written where a daemon can read it. Give every size more than one link: a claude weekly cap and a z.ai five-hour cap are different meters with different periods, and either can be the one that is down. A link whose own provider reads EXHAUSTED with an unexpired reset is SKIPPED; UNKNOWN is not exhausted and stays eligible. An all-exhausted chain returns empty rather than link zero, because routing into a known-capped provider is worse than holding. Unlike agents.profiles this block REFUSES a malformed value rather than degrading open: degrading open on the failover path spawns a worker at an unintended vendor and bills it.", default_source="default"),
    "agents.silence_deadline_seconds": Meta("advanced", "Seconds of transcript silence after which `fno agents sweep` reports a worker as silent (default 600). A REPORT and never an action: no stop, no spawn, no claim mutation. A worker whose transcript age is unknowable emits nothing at all, because absence of evidence must not become a finding.", default_source="default"),
    "agents.dead_row_grace": Meta("advanced", "Seconds a finished agent-view row stays before dead-row GC reaps it (default 3600).", default_source="default"),
    "agents.max_live": Meta("advanced", "Cap on concurrent live worker processes (fno registry + claude roster union); spawn queues at cap (default 3).", default_source="default"),
    "agents.max_lanes": Meta("advanced", "Per-provider budget record keyed by model provider: `lanes` is the immediate-refusal cap on concurrent live workers, `subagents` is the in-session fan-out width review route resolution reads (1 means a panel is never dispatched there). A bare integer is still legal and reads as `lanes`. Unlisted providers are uncapped in both dimensions; the built-in zai budget is lanes 5, subagents 1, because that account is shared.", default_source="default"),
    "agents.min_free_gb": Meta("advanced", "Available-RAM floor in GB for spawn preflight; spawn refuses below it (<= 0 disables; default 4).", default_source="default"),
    "agents.worker_qos": Meta("advanced", "Worker CPU/IO priority: utility (background QoS, default) or off.", default_source="default"),
    "agents.spawn_permission_mode": Meta("advanced", "Default --permission-mode for autonomous dispatchers only (dispatch-node.sh / backlog advance / think dispatch); defaults to bypassPermissions so fire-and-forget workers skip the worktree-entry prompt. An explicit flag wins; opt out with an explicit \"\" (forward nothing) or \"default\" (prompt positively). Claude-native, fail-closed at the spawn seam.", default_source="default"),
    "agents.codex.headless_yolo": Meta("advanced", "Use full-yolo (drop sandbox) for headless codex workers."),
    "agents.gemini.headless_yolo": Meta("advanced", "Use full-yolo (drop sandbox) for headless gemini workers."),
    # --- config.dispatch.* (harness-capability map overlay; `fno agents dispatch resolve`) ---
    "dispatch.harness": Meta("advanced", "Default dispatch harness (claude|codex|gemini|agy|opencode); empty = claude. Overlays the harness-capability map.", default_source="default"),
    "dispatch.substrate": Meta("advanced", "Default dispatch substrate (bg|headless|pane); empty = per-harness default (claude=bg, else headless).", default_source="default"),
    "dispatch.command": Meta("advanced", "Dispatch command template with a single {id}. Empty = '/target --no-merge {id}'. Written in canonical claude slash syntax and normalized per-harness at resolve. A leading /verb becomes $fno:verb on codex and /fno:verb on opencode. The deprecated gemini is refused. A non-slash template passes through literally.", default_source="default"),
    "dispatch.allowed_verbs": Meta("advanced", "Verb allowlist a node's dispatch_verb must match or the resolver refuses (default: /target, /think).", default_source="default"),
    "dispatch.auto_merge": Meta("advanced", "DEPRECATED: reads as auto_merge.grant for one release ('dispatch' when true); migrate with `fno config set auto_merge.grant <none|dispatch>`. Formerly the per-project merge posture for autonomous dispatch.", default_source="default"),
    "dispatch.on_exhaustion": Meta("advanced", "On provider exhaustion during autonomous dispatch: 'defer' (default; a fresh install is unchanged) waits for headroom; 'failover' rotates to the next non-exhausted provider in the active combo. A full-combo exhaustion falls back to defer; any unknown value degrades to 'defer'.", default_source="default"),
    "dispatch.cutover_low_after_minutes": Meta("advanced", "Minutes after which a LOW (not yet exhausted) quota window whose reset is FARTHER out than this arms a cross-harness cutover instead of a wait. Default 0 = off (a fresh install is unchanged). The predicate is inverted from the defer horizon on purpose: for deferring a distant reset means wait, for cutover it means leave now. Needs dispatch.on_exhaustion='failover' and a healthy candidate in the active combo; any non-integer or negative value degrades to 0.", default_source="default"),
    # --- config.autonomy.* ---
    "autonomy.enabled": Meta("never", "The one master switch over every autonomous session-starting spawner. Defaults true; shipping this changes nothing until explicitly disabled."),
    # --- config.auto_continue.* ---
    "auto_continue.enabled": Meta("advanced", "Auto-dispatch the next ready node after a PR merges."),
    # --- config.keep_going.* ---
    "keep_going.enabled": Meta("advanced", "Autonomous keep-going: the merged-PR ritual classifies surviving carve-outs and dispatches follow-up /think or /target work (firehose-capped via think_spawn.daily_cap)."),
    # --- config.think_spawn.* ---
    "think_spawn.enabled": Meta(
        "advanced",
        "Born-with-why: spawn/offer a context-carrying /think for a generated idea node; "
        "actual launches use the shared config.dispatch harness and substrate.",
    ),
    "think_spawn.max_per_run": Meta(
        "advanced", "Blast-radius cap on /think spawns per node-generation run."
    ),
    "think_spawn.idle_threshold_s": Meta(
        "advanced", "Idle seconds before an attended operator downgrades to away (0 = off)."
    ),
    "think_spawn.on_work_start": Meta(
        "advanced", "A2: dispatch a context /think when /target claims a node to work it (default OFF)."
    ),
    "think_spawn.on_retro": Meta(
        "advanced", "A2: dispatch a context /think when `fno backlog done` closes a node (default OFF)."
    ),
    "think_spawn.daily_cap": Meta(
        "advanced", "Per-install per-day ceiling on /think spawns (firehose guard; 0 = off)."
    ),
    "think_spawn.attended": Meta(
        "advanced", "Attended born-with-why behavior: 'offer' (default, handoff line) or 'spawn' (real bg /think)."
    ),
    "think_spawn.on_decompose_wave0": Meta(
        "advanced",
        "Dispatch a /think for each WAVE-0 child at `fno backlog decompose` (default OFF; "
        "inherits max_per_run and daily_cap). Worth it only when the epic is large enough "
        "that inline-filling every child blows one session's context budget.",
    ),
    "think_spawn.substrate": Meta(
        "advanced",
        "Deprecated compatibility fallback for existing configs; used only when "
        "dispatch.substrate is unset. Configure new routing under config.dispatch.",
    ),
    # --- config.active_backlog.* ---
    "active_backlog.enabled": Meta(
        "advanced",
        "Always-on backlog drain: true (every project) or a per-project map.",
    ),
    "active_backlog.interval": Meta(
        "advanced", "Poll-floor cadence for the drain daemon (e.g. 5m, 30s)."
    ),
    "active_backlog.failure_limit": Meta(
        "advanced", "Consecutive dispatch failures before a node is parked."
    ),
    "active_backlog.max_concurrent": Meta(
        "never", "In-flight nodes per project per tick (v1 == 1)."
    ),
    "active_backlog.mission": Meta(
        "never", "Scope the drain daemon to a single mission's nodes."
    ),
    # --- config.mux.* ---
    "mux.shell_integration": Meta(
        "advanced",
        "Auto-inject OSC 133 block markers into mux-spawned shells: "
        "mux-panes (default) | off. Never edits your global shell rc.",
    ),
    "mux.board_scope": Meta(
        "advanced",
        "Which projects the mux backlog board shows: repo (default, this "
        "checkout's project.id) | all | workspace:<name>, e.g. "
        "workspace:main for a workspace holding web, backend and marketing. "
        "A scope that cannot resolve falls back to every project, and `fno mux "
        "doctor` reports that as a warn naming the remedy. Latched at mux "
        "server birth; `fno mux kill-server` re-reads.",
        default_source="default",
    ),
    "mux.prefix": Meta("advanced", "The mux prefix key, as C-a / Ctrl-a / ^a or a bare printable character. A digit 1-9 is refused (those select tabs), as is a key an action already holds. Unset keeps the built-in Ctrl-b."),
    "mux.keys": Meta("advanced", "Per-action key rebinds (action -> key), e.g. detach: 'Q'. Action ids are the ones prefix+? lists. An unreadable key, an unknown action, a digit (1-9 select tabs), or a collision is refused and reported rather than silently ignored.", default_source="default"),
    "mux.notify_on_blocked": Meta("advanced", "Fire an OS notification when an agent badge enters 'blocked' (default on).", default_source="default"),
    "mux.notify_on_done": Meta("advanced", "Also notify on a terminal 'done' hook transition (default off).", default_source="default"),
    "mux.attach_digest": Meta("advanced", "Show a 'while you were gone' catch-up digest overlay on attach after an absence (default on).", default_source="default"),
    "mux.attach_digest_threshold_min": Meta("advanced", "Minutes since last detach before the catch-up digest overlay shows (default 10).", default_source="default"),
    "mux.hover_focus": Meta("advanced", "Focus-follows-mouse: hovering a coding pane makes it the keyboard focus after a short settle (default on).", default_source="default"),
    "mux.theme": Meta(
        "advanced",
        "Mux chrome theme: terminal (default, inherits the emulator colors) | catppuccin | tokyo-night | gruvbox. A named palette recolors the chrome while the body stays the emulator's inverse block. Set from the settings picker.",
        default_source="default",
    ),
    # --- config.dev.* (x-88b9: maintainer local-dev) ---
    "dev.source": Meta("never", "Maintainer pin: a checkout root the Rust bootstrap re-provisions from (uv tool install <path>/cli) instead of the PyPI wheel when its tool venv is wiped. Unset = PyPI self-provision (end-user default)."),
    # --- config.context.* (x-edf5: project-supplied context artifacts) ---
    "context.artifacts": Meta(
        "advanced",
        "Project-supplied context artifacts: {identifier: {path, sensitivity}}. A role's context selector names an identifier resolved here (default sensitivity internal), so a pack installed in a second project is reviewed against that project's facts, not the first's. An unconfigured identifier blocks resolution with MISSING_CONTEXT.",
    ),
    # --- config.loops.* (x-ce71: per-loop level + pause-all substrate) ---
    "loops": Meta(
        "advanced",
        "Per-loop level overrides: {<name>: {level: report|assisted|unattended}} (default report).",
    ),
    # --- config.parallel.* ---
    "parallel.max_lanes": Meta(
        "advanced",
        "Max concurrent parallel-mode lanes (0/1 = sequential, >=2 opts in).",
    ),
    # --- config.auto_merge.* ---
    "auto_merge.enabled": Meta(
        "always", "Auto-merge a PR once external review passes.",
        question="Auto-merge PRs after external review passes?",
    ),
    "auto_merge.grant": Meta(
        "advanced",
        "WHO may merge once enabled passes (actor scope): 'none' = humans only "
        "via `fno do pr merge`; 'dispatch' = autonomously dispatched /target workers "
        "may merge too. Replaces the deprecated dispatch.auto_merge bool. Any "
        "unknown value degrades to 'none'.",
    ),
    "auto_merge.merge_strategy": Meta("advanced", "Merge strategy: merge | squash | rebase."),
    "auto_merge.delete_branch_on_merge": Meta("advanced", "Delete the remote branch after a merge. Executor paths only (`fno do pr merge`, pr verify); GitHub's native auto-merge queue has no branch-delete hook."),
    "auto_merge.require_checks_pass": Meta("advanced", "Require CI green before auto-merge."),
    "auto_merge.conflict_resolution": Meta("never", "Conflict-resolution agent for auto-merge rebases."),
    "auto_merge.remediation": Meta("never", "Post-failure remediation policy for auto-merge."),
    # --- config.pr_watch.* ---
    "pr_watch.enabled": Meta("advanced", "Enable the global PR-state watcher daemon."),
    "pr_watch.interval_seconds": Meta("never", "PR-watcher poll interval (seconds)."),
    "pr_watch.retries": Meta("never", "PR-watcher consecutive-failure park threshold."),
    "pr_watch.max_age_days": Meta("never", "PR-watcher: park PRs older than N days."),
    "pr_watch.model": Meta("never", "Claude model used for headless PR-watcher skill fires."),
    "pr_watch.tick_timeout_seconds": Meta(
        "never",
        "Wall-clock ceiling for one PR-watcher tick; unset derives 0.8x the poll interval so a"
        " stalled tick can never suppress its successor.",
    ),
    "pr_watch.graphql_min_remaining": Meta(
        "never",
        "Skip the PR-watcher's per-PR dispatch pass when the shared GraphQL budget falls below"
        " this floor.",
    ),
    # --- config.groom.* ---
    "groom.enabled": Meta("never", "Enable the daily backlog-grooming worker spawn (fno backlog groom). Defaults true."),
    # --- config.restart.* ---
    "restart.enabled": Meta("never", "Enable crash-recovery worker revival after `fno agents restart --mux` kills a server. Defaults true."),
    # --- config.evals.* ---
    "evals.enabled": Meta("never", "Enable the headless eval-suite grading-worker spawn. Defaults true."),
    # --- config.recovery.* ---
    "recovery.enabled": Meta("advanced", "Enable the bg-session recovery sweep: provider failover on swap-class deaths plus close-surfacing for finished-but-lingering sessions (rides the pr_watch tick). Assumes bypass workers (the config.agents.spawn_permission_mode default); a non-bypass worker cannot run autonomously and is not resumed."),
    "recovery.idle_threshold_seconds": Meta("never", "How stale a bg session must be (seconds) before recovery acts on it."),
    "recovery.max_nudges": Meta("never", "Per-session cap on held-by-design surfaces before recovery stops surfacing a stuck session (close notifications are once-only, tracked separately)."),
    "recovery.watchdog": Meta("advanced", "External fleet watchdog lane on the same tick: off (default) | report (classify + emit one event per non-leave verdict) | wake (also apply the wake lane: resume plus a content-verified message). Reap and reroute never fire from a tick; they need a manual `fno agents watchdog --apply-all`."),
    "recovery.watchdog_mail_to": Meta("advanced", "Mail handle the watchdog digest is pushed to when the non-leave verdict set changes (agent name, short id, or project:<slug>). Empty (default) mails nobody."),
    "recovery.watchdog_reap": Meta("advanced", "Whether `fno agents watchdog --apply-all` may EXECUTE the reap lane (default false). Reap runs stop then rm, which deletes the session's worktree; work that exists only there is gone with it. Wake and reroute are recoverable, so they ship on. Reap verdicts are still computed, reported and mailed when this is off - only the destructive action is withheld."),
    "recovery.retire_grace_s": Meta("advanced", "How long a finished worker stays parked before `fno agents watchdog --apply-all` stops it (default 900 seconds; 0 turns the retire lane off). A worker that declares itself done and never exits holds a live slot against config.agents.max_live forever. The grace is the follow-up window, so a worker that just delivered can still be asked one more thing. Unlike reap this ships armed: retire only runs stop, so the worktree, the transcript and the registry row all survive and `fno agents resume` undoes it."),
    # --- config.health_monitor.* ---
    "health_monitor.enabled": Meta("advanced", "Enable backlog health monitoring."),
    "health_monitor.thresholds.idea_pile_depth": Meta("never", "Breach threshold: idea pile depth."),
    "health_monitor.thresholds.stale_ready_days": Meta("never", "Breach threshold: stale-ready age (days)."),
    "health_monitor.thresholds.failure_prone_attempts": Meta("never", "Breach threshold: failure-prone attempts."),
    "health_monitor.thresholds.collision_count": Meta("never", "Breach threshold: collision count."),
    "health_monitor.thresholds.project_cwd_mismatch": Meta("never", "Breach threshold: project/cwd mismatch count."),
    "health_monitor.thresholds.orphan_feature_rate": Meta("never", "Breach threshold: fraction of open features with no mission edge (1.0 = off)."),
    "health_monitor.notifications.surfaces": Meta("never", "Health notification surfaces (terminal/discord/webhook/log_only)."),
    "health_monitor.notifications.discord_channel": Meta("never", "Discord channel for health notifications."),
    "health_monitor.notifications.webhook_url": Meta("never", "Webhook URL for health notifications."),
    "health_monitor.notifications.throttle_minutes": Meta("never", "Health notification throttle (minutes)."),
    "health_monitor.history.enabled": Meta("never", "Append health-history entries."),
    "health_monitor.history.path": Meta("never", "Override health-history path."),
    "health_monitor.history.retain_days": Meta("never", "Health-history retention (days)."),
    # --- config.collision.* ---
    "collision.severity_thresholds.high_count": Meta("never", "Collision scoring: high-severity shared-file count."),
    "collision.severity_thresholds.high_ratio": Meta("never", "Collision scoring: high-severity shared-file ratio."),
    "collision.severity_thresholds.medium_count": Meta("never", "Collision scoring: medium-severity shared-file count."),
    "collision.severity_thresholds.medium_ratio": Meta("never", "Collision scoring: medium-severity shared-file ratio."),
    # --- config.work map ---
    "work.workspaces": Meta(
        "advanced", "Workspace -> project topology map (config.work.workspaces.<slug>.projects[]). "
        "A project entry may carry a `worktree` key (never|harness-native|external) overriding config.worktree.policy.",
        default_source="auto-detect",
    ),
    # --- config.worktree.* ---
    "worktree.policy": Meta(
        "advanced",
        "Global worktree-isolation policy (never|harness-native|external); default harness-native. "
        "`never` launches code payloads in place (e.g. an Obsidian vault checkout). A per-project "
        "work.workspaces.<slug>.projects[].worktree key overrides it.",
    ),
    # --- config.model_routing.* (role-based per-spawn model routing, x-d2fe) ---
    "model_routing.enabled": Meta(
        "advanced", "Route auxiliary roles (coordinate/tidy/orient/consolidate/post-merge) and the opt-in build lane to a secondary provider at spawn.",
        question="Route auxiliary coordination work to a secondary model provider (production stays on Anthropic)?",
    ),
    "model_routing.providers": Meta(
        "never", "Secondary providers (name -> {protocol, base_url, api_key_env, api_key_file, haiku_model, wire_api}); 'zai' is built in."
    ),
    "model_routing.roles": Meta(
        "never", "Per-role target map (role -> 'provider/model', e.g. tidy: 'zai/glm-4.7'; legacy 'provider,model' comma form also accepted); manage via `fno config route set/unset`. The opt-in 'build' lane routes delivery spawns (/target bg)."
    ),
    "model_routing.extra_env": Meta(
        "never", "Extra env merged into routed spawns (e.g. API_TIMEOUT_MS, per-tier model overrides)."
    ),
    # --- config.status_sinks / config.status_fanout (x-2057) ---
    "status_sinks": Meta(
        "advanced", "Status-fanout subscribers: list of {name, type (json-webhook|text-webhook|backlog-progress), events, match, url|url_env, template, field, cloudevents, enabled}.",
    ),
    "status_fanout.interval_secs": Meta("advanced", "Seconds between status-fanout ticks per project (daemon host)."),
    "status_fanout.http_timeout_secs": Meta("advanced", "Bounded per-webhook HTTP timeout for a status sink."),
    "status_fanout.retries": Meta("advanced", "Retry budget per webhook dispatch before drop/short-circuit."),
    # --- config.king.* (the king loop; both default false) ---
    "king.enabled": Meta("advanced", "Arm the king loop: hold a king session open while its board names work it can shrink. Defaults false."),
    "king.autonomous_merge": Meta("advanced", "Let the king merge a green mergeable PR. Defaults false; until set, a mergeable PR is reported and never counted as the king's own work."),
}


def meta_for(path: str) -> Optional[Meta]:
    """Return the presentation Meta for a leaf dotted path, or None if absent."""
    return FIELD_META.get(path)
