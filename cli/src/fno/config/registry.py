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
    "config.state_dir": Meta("advanced", "Root dir for global fno state.", default_source="default"),
    "config.plans_dir": Meta("advanced", "Where folder plans are written.", default_source="default"),
    "config.branch.prefix": Meta("advanced", "Prefix for dispatched worktree branches: <prefix>/<slug>-<node>.", default_source="default"),
    "config.paths.graph_json": Meta("never", "Override path to the backlog graph.json."),
    "config.paths.ledger_json": Meta("never", "Override path to ledger.json."),
    "config.paths.briefs_dir": Meta("never", "Override path to the sidecar briefs dir."),
    "config.paths.fleet_dir": Meta("never", "Override path to the megatron fleet dir."),
    "config.paths.postmortems_dir": Meta("never", "Override path to the postmortems dir."),
    "config.paths.worktrees_base": Meta("never", "Override base dir for worktrees."),
    "config.paths.memory_dir": Meta("never", "Override path to the memory dir."),
    "config.paths.hook_logs_dir": Meta("never", "Override path to hook logs."),
    "config.paths.inbox_dir": Meta("never", "Override path to the cross-project messaging inbox dir."),
    "config.paths.inbox_path": Meta("never", "Override path to the capture-tier inbox/parking-lot file."),
    "config.paths.agents_registry_path": Meta("never", "Override path to the agents registry.json."),
    "config.paths.handoffs_dir": Meta("never", "Override path to the handoffs dir."),
    "config.paths.retro_pending_dir": Meta("never", "Override path to the retro-pending dir."),
    "config.paths.bus_dir": Meta("never", "Override path to the cross-project mail bus dir."),
    # --- config.obsidian.* (a real decision) ---
    "config.obsidian.enabled": Meta(
        "always", "Whether this project uses an Obsidian vault for plans/docs.",
        question="Use an Obsidian vault for plans and design docs?",
    ),
    "config.obsidian.vault": Meta(
        "always", "Vault area name (NOT a filesystem path).",
        question="Obsidian vault area name?", default_source="auto-detect",
    ),
    # --- config.project.* ---
    "config.project.id": Meta("advanced", "Project identifier.", default_source="repo-slug"),
    "config.project.vision": Meta(
        "always", "One-paragraph statement of what this codebase is and why.",
        question="One-line project vision (what is this and why)?",
        default_source="readme",
    ),
    # --- config.blueprint.* ---
    "config.blueprint.max_prs_per_epic": Meta("advanced", "Cap on group PRs per decomposed epic."),
    # --- config.backlog.* ---
    "config.backlog.maintain.staleness_days": Meta("advanced", "Age (days) before an idea is flagged stale."),
    "config.backlog.maintain.max_failed_attempts": Meta("advanced", "Consecutive failures before a node auto-defers."),
    "config.backlog.id_prefix": Meta(
        "always", "Prefix for minted node IDs (<=7 chars; not cv-/fu-/tgt-).",
        question="Backlog node-ID prefix?", default_source="repo-slug",
    ),
    "config.backlog.id_hex_width": Meta("advanced", "Hex width of minted node IDs (4-8)."),
    # --- config.batch.* ---
    "config.batch.enabled": Meta("advanced", "Coalesce same-domain nodes into one batch PR (opt-in)."),
    "config.batch.max_nodes": Meta("advanced", "Nodes per batch before it closes (default 3)."),
    "config.batch.max_loc": Meta("advanced", "Optional cumulative-diff LOC ceiling for a batch (off by default)."),
    # --- config.post_merge.* ---
    "config.post_merge.parking_lot_path": Meta(
        "advanced", "Per-repo vault parking-lot path for the post-merge ritual (repo-relative).",
    ),
    "config.post_merge.enabled": Meta("advanced", "Whether the post-merge ritual runs."),
    "config.post_merge.self_reap": Meta("never", "Whether a post-merge watcher self-reaps."),
    # --- config.research.* ---
    "config.research.output_dir": Meta(
        "advanced",
        "Landing dir for the `fno research` doc deliverable (brief + sources sidecar); "
        "vault area, not repo-relative. Unset => ship fails loud (never guesses).",
    ),
    # --- config.review.* ---
    "config.review.required_bots": Meta(
        "advanced", "GitHub bot logins that must have reviewed before the ship gate goes green (the GATE).",
    ),
    "config.review.external_reviewers": Meta(
        "always", "Which AI reviewers /pr requests a review from (the INVOCATION list).",
        question="Which external reviewer(s) should review your PRs (gemini/codex/none)?",
    ),
    "config.review.agent_providers": Meta("never", "Per-agent provider routing for the cross-model review panel."),
    "config.review.cross_model.enabled": Meta("advanced", "Enable cross-model (codex/gemini) second-opinion review."),
    # --- config.target.* ---
    "config.target.dedupe_dead_duplicates": Meta("never", "Opt-in cleanup of provably-dead duplicate state files."),
    "config.target.auto_launch_on_blueprint": Meta(
        "advanced", "Auto-launch a bg /target worker when a node reaches ready via /blueprint.",
    ),
    "config.target.handoff.enabled": Meta("advanced", "Enable target self-handoff at pipeline boundaries."),
    "config.target.handoff.used_pct_trigger": Meta("never", "Context-used %% that triggers a wave-boundary handoff."),
    "config.target.handoff.generation_cap": Meta("never", "Max handoff generations before refusing further delegation."),
    "config.target.blast.enabled": Meta("never", "Enable blast-radius routing."),
    "config.target.blast.downgrade": Meta("never", "Allow token-saving downgrades in blast routing."),
    "config.target.blast.reuse_loc_manifest": Meta("never", "Include loc-ratchet globs in the blast map."),
    "config.target.blast.high_blast_globs": Meta("never", "Per-project high-blast glob extensions."),
    "config.target.defaults.no_external": Meta("never", "Session-input default: skip external review (size-profile driven)."),
    "config.target.defaults.no_docs": Meta("never", "Session-input default: skip docs (size-profile driven)."),
    "config.target.defaults.max_iterations": Meta("advanced", "Session-input default: max pipeline iterations."),
    # --- config.agents.* ---
    "config.agents.a2a.auto": Meta("advanced", "Allow agents to auto-open agent-to-agent threads."),
    "config.agents.a2a.turn_ceiling": Meta("advanced", "Max turns in an agent-to-agent thread."),
    "config.agents.confirm": Meta("never", "Agent-launch confirmation policy (auto/always/never)."),
    "config.agents.dead_row_grace": Meta("advanced", "Seconds a finished agent-view row stays before dead-row GC reaps it (default 3600).", default_source="default"),
    "config.agents.codex.headless_yolo": Meta("advanced", "Use full-yolo (drop sandbox) for headless codex workers."),
    "config.agents.gemini.headless_yolo": Meta("advanced", "Use full-yolo (drop sandbox) for headless gemini workers."),
    # --- config.auto_continue.* ---
    "config.auto_continue.enabled": Meta("advanced", "Auto-dispatch the next ready node after a PR merges."),
    # --- config.think_spawn.* ---
    "config.think_spawn.enabled": Meta(
        "advanced", "Born-with-why: spawn/offer a context-carrying /think for a generated idea node."
    ),
    "config.think_spawn.max_per_run": Meta(
        "advanced", "Blast-radius cap on /think spawns per node-generation run."
    ),
    "config.think_spawn.idle_threshold_s": Meta(
        "advanced", "Idle seconds before an attended operator downgrades to away (0 = off)."
    ),
    "config.think_spawn.on_work_start": Meta(
        "advanced", "A2: dispatch a context /think when /target claims a node to work it (default OFF)."
    ),
    "config.think_spawn.on_retro": Meta(
        "advanced", "A2: dispatch a context /think when `fno backlog done` closes a node (default OFF)."
    ),
    "config.think_spawn.daily_cap": Meta(
        "advanced", "Per-install per-day ceiling on /think spawns (firehose guard; 0 = off)."
    ),
    "config.think_spawn.attended": Meta(
        "advanced", "Attended born-with-why behavior: 'offer' (default, handoff line) or 'spawn' (real bg /think)."
    ),
    # --- config.active_backlog.* ---
    "config.active_backlog.enabled": Meta(
        "advanced",
        "Always-on backlog drain: true (every project) or a per-project map.",
    ),
    "config.active_backlog.interval": Meta(
        "advanced", "Poll-floor cadence for the drain daemon (e.g. 5m, 30s)."
    ),
    "config.active_backlog.failure_limit": Meta(
        "advanced", "Consecutive dispatch failures before a node is parked."
    ),
    "config.active_backlog.max_concurrent": Meta(
        "never", "In-flight nodes per project per tick (v1 == 1)."
    ),
    "config.active_backlog.mission": Meta(
        "never", "Scope the drain daemon to a single mission's nodes."
    ),
    # --- config.mux.* ---
    "config.mux.shell_integration": Meta(
        "advanced",
        "Auto-inject OSC 133 block markers into mux-spawned shells: "
        "mux-panes (default) | off. Never edits your global shell rc.",
    ),
    # --- config.parallel.* ---
    "config.parallel.max_lanes": Meta(
        "advanced",
        "Max concurrent parallel-mode lanes (0/1 = sequential, >=2 opts in).",
    ),
    # --- config.auto_merge.* ---
    "config.auto_merge.enabled": Meta(
        "always", "Auto-merge a PR once external review passes.",
        question="Auto-merge PRs after external review passes?",
    ),
    "config.auto_merge.merge_strategy": Meta("advanced", "Merge strategy: merge | squash | rebase."),
    "config.auto_merge.delete_branch_on_merge": Meta("advanced", "Delete the branch after an auto-merge."),
    "config.auto_merge.require_checks_pass": Meta("advanced", "Require CI green before auto-merge."),
    "config.auto_merge.conflict_resolution": Meta("never", "Conflict-resolution agent for auto-merge rebases."),
    "config.auto_merge.allowed_invokers": Meta("never", "Who may trigger auto-merge."),
    "config.auto_merge.remediation": Meta("never", "Post-failure remediation policy for auto-merge."),
    # --- config.logs.* ---
    "config.logs.convo_signals_max_mb": Meta("never", "Rotation cap (MB) for convo-signals.jsonl."),
    # --- config.pr_watch.* ---
    "config.pr_watch.enabled": Meta("advanced", "Enable the global PR-state watcher daemon."),
    "config.pr_watch.interval_seconds": Meta("never", "PR-watcher poll interval (seconds)."),
    "config.pr_watch.retries": Meta("never", "PR-watcher consecutive-failure park threshold."),
    "config.pr_watch.max_age_days": Meta("never", "PR-watcher: park PRs older than N days."),
    "config.pr_watch.model": Meta("never", "Claude model used for headless PR-watcher skill fires."),
    # --- config.recovery.* ---
    "config.recovery.enabled": Meta("advanced", "Enable the session auto-recovery watchdog (resumes idle-but-incomplete bg sessions; rides the pr_watch tick)."),
    "config.recovery.idle_threshold_seconds": Meta("never", "How stale a bg session must be before a resume nudge fires (seconds)."),
    "config.recovery.max_nudges": Meta("never", "Per-session cap on resume nudges before the watchdog gives up."),
    # --- config.health_monitor.* ---
    "config.health_monitor.enabled": Meta("advanced", "Enable backlog health monitoring."),
    "config.health_monitor.thresholds.idea_pile_depth": Meta("never", "Breach threshold: idea pile depth."),
    "config.health_monitor.thresholds.stale_ready_days": Meta("never", "Breach threshold: stale-ready age (days)."),
    "config.health_monitor.thresholds.failure_prone_attempts": Meta("never", "Breach threshold: failure-prone attempts."),
    "config.health_monitor.thresholds.collision_count": Meta("never", "Breach threshold: collision count."),
    "config.health_monitor.thresholds.project_cwd_mismatch": Meta("never", "Breach threshold: project/cwd mismatch count."),
    "config.health_monitor.notifications.surfaces": Meta("never", "Health notification surfaces (terminal/discord/webhook/log_only)."),
    "config.health_monitor.notifications.discord_channel": Meta("never", "Discord channel for health notifications."),
    "config.health_monitor.notifications.webhook_url": Meta("never", "Webhook URL for health notifications."),
    "config.health_monitor.notifications.throttle_minutes": Meta("never", "Health notification throttle (minutes)."),
    "config.health_monitor.history.enabled": Meta("never", "Append health-history entries."),
    "config.health_monitor.history.path": Meta("never", "Override health-history path."),
    "config.health_monitor.history.retain_days": Meta("never", "Health-history retention (days)."),
    # --- config.collision.* ---
    "config.collision.severity_thresholds.high_count": Meta("never", "Collision scoring: high-severity shared-file count."),
    "config.collision.severity_thresholds.high_ratio": Meta("never", "Collision scoring: high-severity shared-file ratio."),
    "config.collision.severity_thresholds.medium_count": Meta("never", "Collision scoring: medium-severity shared-file count."),
    "config.collision.severity_thresholds.medium_ratio": Meta("never", "Collision scoring: medium-severity shared-file ratio."),
    # --- config.work map ---
    "config.work.workspaces": Meta(
        "advanced", "Workspace -> project topology map (config.work.workspaces.<slug>.projects[]).",
        default_source="auto-detect",
    ),
    # --- config.model_routing.* (role-based per-spawn model routing, x-d2fe) ---
    "config.model_routing.enabled": Meta(
        "advanced", "Route auxiliary roles (coordinate/tidy/orient/consolidate) to a secondary provider at spawn.",
        question="Route auxiliary coordination work to a secondary model provider (production stays on Anthropic)?",
    ),
    "config.model_routing.providers": Meta(
        "never", "Secondary providers (name -> {protocol, base_url, api_key_env, api_key_file}); 'zai' is built in."
    ),
    "config.model_routing.roles": Meta(
        "never", "Per-role target map (role -> 'provider,model', e.g. tidy: 'zai,glm-4.7')."
    ),
    "config.model_routing.extra_env": Meta(
        "never", "Extra env merged into routed spawns (e.g. API_TIMEOUT_MS, per-tier model overrides)."
    ),
}


def meta_for(path: str) -> Optional[Meta]:
    """Return the presentation Meta for a leaf dotted path, or None if absent."""
    return FIELD_META.get(path)
