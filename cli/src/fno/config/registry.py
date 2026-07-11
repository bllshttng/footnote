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
    "state_dir": Meta("advanced", "Root dir for global fno state.", default_source="default"),
    "plans_dir": Meta("advanced", "Where folder plans are written.", default_source="default"),
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
    "blueprint.max_prs_per_epic": Meta("advanced", "Cap on group PRs per decomposed epic."),
    # --- config.backlog.* ---
    "backlog.maintain.staleness_days": Meta("advanced", "Age (days) before an idea is flagged stale."),
    "backlog.maintain.max_failed_attempts": Meta("advanced", "Consecutive failures before a node auto-defers."),
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
    "post_merge.enabled": Meta("advanced", "Whether the post-merge ritual runs."),
    "post_merge.self_reap": Meta("never", "Whether a post-merge watcher self-reaps."),
    "post_merge.sync_command": Meta(
        "advanced",
        "Canonical-sync incantation run via `bash -lc` after a merge (e.g. "
        "`git checkout main && git pull && fno update && fno restart`). Unset = off.",
    ),
    "post_merge.sync_paths": Meta(
        "advanced",
        "Repo-relative fnmatch globs gating the canonical sync (empty = always "
        "run; e.g. `[\"cli/**\", \"crates/**\"]` skips a docs-only merge).",
    ),
    "post_merge.auto_run": Meta(
        "advanced",
        "Let merge-detection auto-dispatch the /fno:pr merged ritual for a "
        "newly-merged PR (opt-in; default off).",
    ),
    "post_merge.model": Meta(
        "advanced",
        "Model for post-merge ritual workers (default claude-sonnet-5). Routing "
        "wins when a secondary provider is keyed.",
    ),
    # --- config.research.* ---
    "research.output_dir": Meta(
        "advanced",
        "Landing dir for the `fno research` doc deliverable (brief + sources sidecar); "
        "vault area, not repo-relative. Unset => ship fails loud (never guesses).",
    ),
    # --- config.review.* ---
    "review.github_apps": Meta(
        "advanced", "GitHub App bot logins that must have reviewed before the ship gate goes green (the GATE). Legacy alias: required_bots.",
    ),
    "review.required_bots": Meta(
        "never", "Legacy alias for config.review.github_apps (a straight rename); github_apps wins if both are set.",
    ),
    "review.optional_apps": Meta(
        "advanced", "Reviewer logins honored-if-present but NOT required: the gate never waits for them (kills the App-bot usage-limit wedge), but a blocking finding from one still holds it.",
    ),
    "review.reviewers": Meta(
        "advanced", "Local-attestation reviewers (sigma | /code-review | declare) that produce no GitHub review: loop-check accepts a head-pinned review_attestation event as gate evidence. Lets a solo/claude-only harness express a real gate with no App bot.",
    ),
    "review.peers": Meta(
        "advanced", "Harness peers (codex/gemini/...) run locally that post a real PR review under peer_identity and gate like github_apps. Scalar or {provider, identity, token_env} map entries.",
    ),
    "review.peer_identity": Meta(
        "advanced", "The distinct machine-account login peers post their review under (must not be the author account).",
    ),
    "review.peer_token_env": Meta(
        "advanced", "Env var holding the PAT for peer_identity used to post peer reviews to the PR.",
    ),
    "review.external_reviewers": Meta(
        "always", "Which AI reviewers /pr requests a review from (the INVOCATION list).",
        question="Which external reviewer(s) should review your PRs (gemini/codex/none)?",
    ),
    "review.agent_providers": Meta("never", "Per-agent provider routing for the cross-model review panel."),
    "review.cross_model.enabled": Meta("advanced", "Enable cross-model (codex/gemini) second-opinion review."),
    # --- config.target.* ---
    "target.dedupe_dead_duplicates": Meta("never", "Opt-in cleanup of provably-dead duplicate state files."),
    "target.auto_launch_on_blueprint": Meta(
        "advanced", "Auto-launch a bg /target worker when a node reaches ready via /blueprint.",
    ),
    "target.handoff.enabled": Meta("advanced", "Enable target self-handoff at pipeline boundaries."),
    "target.handoff.used_pct_trigger": Meta("never", "Context-used %% that triggers a wave-boundary handoff."),
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
    "agents.defaults.provider": Meta("advanced", "Default provider for bare `fno agents spawn` / `/agent spawn` (claude/codex/gemini/agy/opencode); an explicit -p flag wins, empty = unset (harness inference then claude). Validated at the spawn seam.", default_source="default"),
    "agents.defaults.model": Meta("advanced", "Default model for bare spawns, forwarded as --model; an explicit -m flag wins, empty = unset (provider default). Passthrough (provider CLIs own model names).", default_source="default"),
    "agents.defaults.effort": Meta("advanced", "Default reasoning effort for bare spawns (minimal|low|medium|high|xhigh|max); an explicit --effort wins, empty = unset. Config-sourced effort degrades open on providers with no effort surface (gemini/agy).", default_source="default"),
    "agents.dead_row_grace": Meta("advanced", "Seconds a finished agent-view row stays before dead-row GC reaps it (default 3600).", default_source="default"),
    "agents.max_live": Meta("advanced", "Cap on concurrent live worker processes (fno registry + claude roster union); spawn queues at cap (default 3).", default_source="default"),
    "agents.min_free_gb": Meta("advanced", "Available-RAM floor in GB for spawn preflight; spawn refuses below it (<= 0 disables; default 4).", default_source="default"),
    "agents.worker_qos": Meta("advanced", "Worker CPU/IO priority: utility (background QoS, default) or off.", default_source="default"),
    "agents.spawn_permission_mode": Meta("advanced", "Default --permission-mode for autonomous dispatchers only (dispatch-node.sh / backlog advance / think dispatch); an explicit flag wins, empty = unset. Provider-native, fail-closed at the spawn seam.", default_source="default"),
    "agents.codex.headless_yolo": Meta("advanced", "Use full-yolo (drop sandbox) for headless codex workers."),
    "agents.gemini.headless_yolo": Meta("advanced", "Use full-yolo (drop sandbox) for headless gemini workers."),
    # --- config.auto_continue.* ---
    "auto_continue.enabled": Meta("advanced", "Auto-dispatch the next ready node after a PR merges."),
    # --- config.keep_going.* ---
    "keep_going.enabled": Meta("advanced", "Autonomous keep-going: the merged-PR ritual classifies surviving carve-outs and dispatches follow-up /think or /target work (firehose-capped via think_spawn.daily_cap)."),
    # --- config.think_spawn.* ---
    "think_spawn.enabled": Meta(
        "advanced", "Born-with-why: spawn/offer a context-carrying /think for a generated idea node."
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
    "mux.notify_on_blocked": Meta("advanced", "Fire an OS notification when an agent badge enters 'blocked' (default on).", default_source="default"),
    "mux.notify_on_done": Meta("advanced", "Also notify on a terminal 'done' hook transition (default off).", default_source="default"),
    "mux.attach_digest": Meta("advanced", "Show a 'while you were gone' catch-up digest overlay on attach after an absence (default on).", default_source="default"),
    "mux.attach_digest_threshold_min": Meta("advanced", "Minutes since last detach before the catch-up digest overlay shows (default 10).", default_source="default"),
    "mux.hover_focus": Meta("advanced", "Focus-follows-mouse: hovering a coding pane makes it the keyboard focus after a short settle (default on).", default_source="default"),
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
    "auto_merge.merge_strategy": Meta("advanced", "Merge strategy: merge | squash | rebase."),
    "auto_merge.delete_branch_on_merge": Meta("advanced", "Delete the branch after an auto-merge."),
    "auto_merge.require_checks_pass": Meta("advanced", "Require CI green before auto-merge."),
    "auto_merge.conflict_resolution": Meta("never", "Conflict-resolution agent for auto-merge rebases."),
    "auto_merge.allowed_invokers": Meta("never", "Who may trigger auto-merge."),
    "auto_merge.remediation": Meta("never", "Post-failure remediation policy for auto-merge."),
    # --- config.pr_watch.* ---
    "pr_watch.enabled": Meta("advanced", "Enable the global PR-state watcher daemon."),
    "pr_watch.interval_seconds": Meta("never", "PR-watcher poll interval (seconds)."),
    "pr_watch.retries": Meta("never", "PR-watcher consecutive-failure park threshold."),
    "pr_watch.max_age_days": Meta("never", "PR-watcher: park PRs older than N days."),
    "pr_watch.model": Meta("never", "Claude model used for headless PR-watcher skill fires."),
    # --- config.recovery.* ---
    "recovery.enabled": Meta("advanced", "Enable the session auto-recovery watchdog (resumes idle-but-incomplete bg sessions; rides the pr_watch tick)."),
    "recovery.idle_threshold_seconds": Meta("never", "How stale a bg session must be before a resume nudge fires (seconds)."),
    "recovery.max_nudges": Meta("never", "Per-session cap on resume nudges before the watchdog gives up."),
    # --- config.health_monitor.* ---
    "health_monitor.enabled": Meta("advanced", "Enable backlog health monitoring."),
    "health_monitor.thresholds.idea_pile_depth": Meta("never", "Breach threshold: idea pile depth."),
    "health_monitor.thresholds.stale_ready_days": Meta("never", "Breach threshold: stale-ready age (days)."),
    "health_monitor.thresholds.failure_prone_attempts": Meta("never", "Breach threshold: failure-prone attempts."),
    "health_monitor.thresholds.collision_count": Meta("never", "Breach threshold: collision count."),
    "health_monitor.thresholds.project_cwd_mismatch": Meta("never", "Breach threshold: project/cwd mismatch count."),
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
        "advanced", "Workspace -> project topology map (config.work.workspaces.<slug>.projects[]).",
        default_source="auto-detect",
    ),
    # --- config.model_routing.* (role-based per-spawn model routing, x-d2fe) ---
    "model_routing.enabled": Meta(
        "advanced", "Route auxiliary roles (coordinate/tidy/orient/consolidate/post-merge) to a secondary provider at spawn.",
        question="Route auxiliary coordination work to a secondary model provider (production stays on Anthropic)?",
    ),
    "model_routing.providers": Meta(
        "never", "Secondary providers (name -> {protocol, base_url, api_key_env, api_key_file, haiku_model, wire_api}); 'zai' is built in."
    ),
    "model_routing.roles": Meta(
        "never", "Per-role target map (role -> 'provider,model', e.g. tidy: 'zai,glm-4.7')."
    ),
    "model_routing.extra_env": Meta(
        "never", "Extra env merged into routed spawns (e.g. API_TIMEOUT_MS, per-tier model overrides)."
    ),
}


def meta_for(path: str) -> Optional[Meta]:
    """Return the presentation Meta for a leaf dotted path, or None if absent."""
    return FIELD_META.get(path)
