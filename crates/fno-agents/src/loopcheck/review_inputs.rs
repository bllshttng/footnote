//! Which review inputs apply? Settings, harness and events resolved into one ReviewInputs.

use super::*;

/// The manifest-independent inputs every coverage/review evaluation needs:
/// event-log paths, repo identity, the merged settings, and the reviewer sets
/// derived from them. Extracted from `decide()` so the standalone
/// `review-coverage` verb resolves EXACTLY what the stop hook resolves - one
/// resolver, no second precedence implementation (the N-implementations trap).
pub(crate) struct ReviewInputs {
    pub(crate) project_events: PathBuf,
    pub(crate) global_events: PathBuf,
    /// Full `host/owner/repo` from the git remote; empty when unresolvable.
    pub(crate) repo_slug: String,
    pub(crate) settings: Settings,
    /// The ambient author harness (env markers, or the explicit override).
    pub(crate) author_harness: Option<String>,
    /// Whether the caller explicitly pinned `--author-harness none` (the
    /// hermetic opt-out). Distinct from an UNRESOLVED harness, which floors.
    pub(crate) author_harness_pinned_none: bool,
    pub(crate) required_bots: Vec<String>,
    pub(crate) required_reviewers: Vec<String>,
    pub(crate) optional_bots: Vec<String>,
    /// Lane CONFIGURATION is explicit config only (a non-empty
    /// `review.optional_apps`), never the built-in default that fills unset
    /// configs: the default logins are honored-if-present where a lane
    /// exists, but must not light the login gate, the publisher's lane
    /// predicate, or the self-review floor on a stock install.
    pub(crate) optional_lane_configured: bool,
    pub(crate) nudge_configs: Vec<NudgeConfig>,
}

/// Resolve [`ReviewInputs`]: event paths, repo slug, the GLOBAL-then-local
/// settings overlay, and the bot/reviewer sets derived from it. This is the
/// block `decide()` ran inline; it moves here unchanged (including the
/// fail-closed unparseable-settings branch, which is why this cannot be a
/// naive copy) so `decide()` and `run_review_coverage` share one resolver.
pub(crate) fn resolve_review_inputs(
    cwd: &Path,
    events_path: Option<&Path>,
    global_events_path: Option<&Path>,
    settings_path: Option<&Path>,
    global_settings_path: Option<&Path>,
    author_harness_override: Option<&str>,
) -> ReviewInputs {
    let project_events = events_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::paths::events_path(cwd));

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let global_events = global_events_path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_global_events_path);

    // Scopes the review_coverage event written into the cross-project global
    // log. The git remote is the one identifier canonical and every one of its
    // worktrees agree on, which is exactly the agreement the coverage reader
    // needs. It is the FULL `host/owner/repo`, not the last path
    // segment: this key gates auto-merge, so `org-a/widget` aliasing
    // `org-b/widget` would let one repo's coverage clear the other's guard.
    // Empty when there is no remote; the payload then omits `repo` and no
    // reader will claim the event.
    let repo_slug = crate::finalize::repo_identity_from_git_remote(cwd).unwrap_or_default();

    // Parse settings: GLOBAL first, then overlay the project-local file's
    // populated fields (codex P1 on #447: budgets normally live in the
    // global file; a project-local settings.yaml with unrelated content
    // must not silently uncap the session). An explicit --settings path
    // replaces the merge entirely (tests rely on full isolation).
    //
    // (c): a genuinely unparseable settings.yaml fails CLOSED (the login
    // gate is pinned unsatisfiable) and emits loop_check_settings_unparseable,
    // rather than silently zeroing the required bots and shipping unreviewed.
    let parse_or_emit = |content: &str, path: &Path| -> Settings {
        match parse_settings_result(content) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "loop-check: config.toml unparseable ({}): {e} - failing the login gate closed",
                    path.display()
                );
                emit_to_both(
                    &project_events,
                    &global_events,
                    "loop_check_settings_unparseable",
                    serde_json::json!({"path": path.display().to_string(), "error": e}),
                );
                fail_closed_settings()
            }
        }
    };
    let settings = if let Some(explicit) = settings_path {
        if let Ok(sc) = std::fs::read_to_string(explicit) {
            parse_or_emit(&sc, explicit)
        } else {
            Settings::default()
        }
    } else {
        let global_path = global_settings_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(&home).join(".fno/config.toml"));
        let mut merged = std::fs::read_to_string(&global_path)
            .map(|sc| parse_or_emit(&sc, &global_path))
            .unwrap_or_default();
        let local_path = cwd.join(".fno/config.toml");
        if let Ok(sc) = std::fs::read_to_string(&local_path) {
            let local = parse_or_emit(&sc, &local_path);
            if local.attended_wall_cap_minutes.is_some() {
                merged.attended_wall_cap_minutes = local.attended_wall_cap_minutes;
            }
            if local.attended_cost_cap_usd.is_some() {
                merged.attended_cost_cap_usd = local.attended_cost_cap_usd;
            }
            if local.unattended_wall_cap_minutes.is_some() {
                merged.unattended_wall_cap_minutes = local.unattended_wall_cap_minutes;
            }
            if local.unattended_cost_cap_usd.is_some() {
                merged.unattended_cost_cap_usd = local.unattended_cost_cap_usd;
            }
            if local.flat_budget_cap.is_some() {
                merged.flat_budget_cap = local.flat_budget_cap;
            }
            if local.ci_declared_none {
                merged.ci_declared_none = true;
            }
            if !local.external_reviewers.is_empty() {
                merged.external_reviewers = local.external_reviewers;
            }
            if local.required_bots.is_some() {
                // Some([]) is a meaningful project-local override (declared
                // no-review-gate), so presence - not non-emptiness - wins.
                merged.required_bots = local.required_bots;
            }
            if local.github_apps.is_some() {
                merged.github_apps = local.github_apps;
            }
            if local.optional_apps.is_some() {
                merged.optional_apps = local.optional_apps;
            }
            if !local.reviewers.is_empty() {
                merged.reviewers = local.reviewers;
            }
            if local.self_review_required.is_some() {
                // Presence, not value: `self_review_required = false` is the
                // documented repo opt-out, so a local Some(false) must override
                // a global Some(true). Same overlay rule as required_bots.
                merged.self_review_required = local.self_review_required;
            }
            if local.posture.is_some() {
                // Same presence rule: the rung is a project policy
                // leaf, so a local explicit posture must
                // override the global file and the inference must not silently
                // read the global-only view.
                merged.posture = local.posture;
            }
            if !local.nudge_overrides.is_empty() {
                // Without this line a project-local `[review.nudge]` (including
                // `enabled = false`) is read from the GLOBAL file only and the
                // repo's own overrides vanish - loop-check would post a nudge a
                // repo explicitly opted out of. Same per-field-overlay trap the
                // done_probes line below documents.
                merged.nudge_overrides = local.nudge_overrides;
            }
            if !local.peers.is_empty() {
                merged.peers = local.peers;
            }
            if local.peer_identity.is_some() {
                merged.peer_identity = local.peer_identity;
            }
            if local.done_probes.is_some() {
                // Presence, not non-emptiness: a project-local `done_probes = []`
                // is a deliberate "this repo declares none", same rule as
                // required_bots. Omitting this line entirely is the silent
                // guardrail bypass this list keeps re-inviting - the field would
                // be read from the GLOBAL file only and the project's own gate
                // would never run.
                merged.done_probes = local.done_probes;
            }
        }
        merged
    };

    // Resolve the must-have-reviewed list once (code default when unset). The
    // author harness (from the ambient env markers, shared with claims.rs) drives
    // the same-model peer guard; None leaves the set unchanged.
    // `--author-harness none` pins the no-harness case, which an absent flag
    // cannot express, and an absent flag keeps reading the ambient markers.
    // The pin is recorded separately: a PINNED none is the hermetic opt-out,
    // while an UNRESOLVED None (absent or ambiguous markers) floors the
    // self-review reviewer instead of dropping it.
    let author_harness_pinned_none = matches!(author_harness_override, Some("none") | Some(""));
    let author_harness = match author_harness_override {
        Some("none") | Some("") => None,
        Some(h) => Some(h.to_string()),
        None => crate::claims::resolve_harness(),
    };
    let required_bots = resolved_required_bots_for_author(&settings, author_harness.as_deref());
    let mut required_reviewers = settings.reviewers.clone();
    for reviewer in resolved_local_peer_reviewers_for_author(&settings, author_harness.as_deref()) {
        if !required_reviewers.contains(&reviewer) {
            required_reviewers.push(reviewer);
        }
    }
    let optional_bots = resolved_optional_bots(&settings);
    let optional_lane_configured = settings
        .optional_apps
        .as_ref()
        .is_some_and(|v| !v.is_empty());
    let nudge_configs = resolved_nudge_configs(&settings);

    ReviewInputs {
        project_events,
        global_events,
        repo_slug,
        settings,
        author_harness,
        author_harness_pinned_none,
        required_bots,
        required_reviewers,
        optional_bots,
        // Lane CONFIGURATION is explicit config, never the built-in default:
        // the default logins are honored-if-present where a lane exists, but
        // they must not light up the login gate, the coverage publisher's
        // lane predicate, or the self-review floor on a stock install - that
        // flip skips the floor and forces gh review reads for installs that
        // configured nothing (review round 2).
        optional_lane_configured,
        nudge_configs,
    }
}
