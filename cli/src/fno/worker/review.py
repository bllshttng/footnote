"""fno worker review - internal sigma-review orchestrator entrypoint.

Dispatches orchestrate_review_parallel (6-agent sigma panel), passes
findings through score_findings, and writes a gate-schema-compliant
quality_check artifact at <artifacts_dir>/review-<session_id>.md.

The external GitHub polling logic has been moved to worker/external.py
(renamed external_review). This module is the internal path only.

Exit codes used by the CLI wrapper:
  0  Normal (reviewed or cached)
  11 Review lock busy (another process is running the same session)
  130 SIGINT - workers were reaped
"""

from __future__ import annotations

import sys
import subprocess
import uuid
from pathlib import Path
from typing import Any, Optional

import yaml

from fno.config import AgentRouteBlock
from fno.review.runners.claude_runner import (
    make_async_runner,
)
from fno.review.orchestrator import (
    load_prompts,
    orchestrate_review_parallel,
    OrchestratorResult,
)


class ReviewInputError(RuntimeError):
    """The revision or exact diff for a review could not be captured."""


def _resolve_agent_route(route: AgentRouteBlock, resolver: Any) -> tuple[dict[str, str] | None, Any]:
    """Resolve one validated route identically for diagnostics and dispatch."""
    from fno.review import provider_resolution as pr

    route_env = resolver(route.provider, route.model)
    if not route_env:
        return None, pr.ResolvedProvider(
            provider="claude",
            degraded=True,
            reason=f"{route.provider}/{route.model} unavailable: ran on claude",
        )
    return dict(route_env), pr.ResolvedProvider(
        provider=route.harness,
        route=pr.RouteRef(
            harness=route.harness,
            provider=route.provider,
            model=route.model,
        ),
    )


def capture_review_input(diff_path: Path | None = None) -> tuple[str, str]:
    """Capture one immutable reviewed SHA and the diff for exactly that SHA."""
    head_result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
    )
    if head_result.returncode != 0 or not head_result.stdout.strip():
        raise ReviewInputError(
            f"git rev-parse HEAD failed (rc={head_result.returncode}): "
            f"{head_result.stderr.strip()}"
        )
    reviewed_head = head_result.stdout.strip()

    if diff_path is not None:
        try:
            return reviewed_head, diff_path.read_text(encoding="utf-8")
        except OSError as exc:
            raise ReviewInputError(f"could not read review diff {diff_path}: {exc}") from exc

    git_result = subprocess.run(
        ["git", "diff", f"{reviewed_head}~1", reviewed_head],
        capture_output=True,
        text=True,
    )
    if git_result.returncode != 0:
        raise ReviewInputError(
            f"git diff HEAD~1 failed for pinned head {reviewed_head} "
            f"(rc={git_result.returncode}): {git_result.stderr.strip()}\n"
            "Pass --diff path/to/manual.diff to review an explicit diff "
            "(e.g. first-commit branches without a HEAD~1 parent)."
        )
    return reviewed_head, git_result.stdout


def _read_state(state_path: Path) -> dict[str, Any]:
    """Read YAML frontmatter from target-state.md."""
    text = state_path.read_text(encoding="utf-8") if state_path.exists() else ""
    if not text.startswith("---"):
        return {}
    rest = text[3:].lstrip("\n")
    end = rest.find("\n---")
    if end == -1:
        return {}
    return yaml.safe_load(rest[:end]) or {}


def build_review_runner(
    *,
    agent_providers: dict[str, str],
    agent_routes: Optional[dict[str, AgentRouteBlock]] = None,
    cross_model_enabled: bool,
    implementer_provider: str,
    available_providers: list[str],
    base_prompts: dict[str, str],
    worker_pids: Optional[list[int]] = None,
    cwd: Optional[Path] = None,
    timeout: float = 600.0,
    dispatch: Optional[Any] = None,
    claude_adapter: Optional[Any] = None,
    route_resolver: Optional[Any] = None,
) -> tuple[Optional[Any], Optional[dict[str, str]], Optional[list[str]]]:
    """Build the per-agent cross-model review runner (ab-6c8f4c61).

    Returns ``(runner, prompts, provider_set)``. When cross-model is NOT engaged
    (``cross_model_enabled`` False AND ``agent_providers`` empty) returns
    ``(None, None, None)`` so the caller runs today's all-claude path unchanged.

    When engaged, every agent is resolved to a provider kind; the returned
    composite async runner dispatches each agent via the claude runner (claude)
    or the codex/gemini ``agents_spawn_runner``. A retryable spawn failure falls
    through to claude (AC5-FR); a degraded/fallback run is annotated on the
    outcome's ``note`` for the report. The returned ``prompts`` carry the JSON
    contract appended to each base prompt; ``provider_set`` is the sorted set of
    resolved kinds (the cache dimension). ``dispatch`` / ``claude_adapter`` are
    injection seams for tests; production leaves them None.
    """
    agent_routes = agent_routes or {}
    engaged = bool(cross_model_enabled) or bool(agent_providers) or bool(agent_routes)
    if not engaged:
        return None, None, None

    from fno.review import provider_resolution as pr
    from fno.review.findings_parser import json_findings_prompt
    from fno.review.orchestrator import AGENT_NAMES
    from fno.review.runners import agents_spawn_runner, claude_runner

    _warn_unknown_agent_keys(agent_providers)
    unknown_routes = sorted(set(agent_routes) - set(base_prompts))
    if unknown_routes:
        raise ValueError(f"agent route names unknown panel agent(s): {unknown_routes}")

    resolved = pr.resolve_panel_providers(
        list(base_prompts),
        agent_providers=agent_providers,
        implementer_provider=implementer_provider,
        available_providers=available_providers,
        known_agents=AGENT_NAMES,
    )
    if route_resolver is None:
        from fno.agents.model_routing import resolve_explicit_route

        resolved_route = resolve_explicit_route
    else:
        resolved_route = route_resolver

    route_envs: dict[str, dict[str, str]] = {}
    route_specs: dict[str, AgentRouteBlock] = {}
    for agent, route in agent_routes.items():
        route_env, route_resolution = _resolve_agent_route(route, resolved_route)
        resolved[agent] = route_resolution
        if route_env is None:
            continue
        from fno.agents.model_routing import bind_route_provider

        route_envs[agent] = bind_route_provider(route_env, route.provider)
        route_specs[agent] = route
    # Cache dimension = the per-agent REQUESTED routing (not just the set of
    # kinds), so two configs that assign the same kinds to different agents
    # (e.g. {code_reviewer: codex} vs {silent_failure_hunter: codex}) never
    # collide on one cache key for the same SHA (codex review P2). Pairs are
    # sorted for a stable key.
    provider_set = sorted(
        f"{agent}={rp.route.harness}/{rp.route.provider}/{rp.route.model}"
        if rp.route
        else f"{agent}={rp.provider}"
        for agent, rp in resolved.items()
    )
    prompts = {agent: json_findings_prompt(body) for agent, body in base_prompts.items()}
    run_cwd = cwd or Path.cwd()

    claude_async = claude_runner.make_async_runner(
        worker_pids=worker_pids, json_findings=True, adapter=claude_adapter
    )

    async def _runner(agent: str, prompt: str, diff_context: str):
        rp = resolved.get(agent)
        if agent in route_specs:
            route = route_specs[agent]
            spawn_async = agents_spawn_runner.make_async_runner(
                provider=route.harness,
                cwd=run_cwd,
                timeout=timeout,
                dispatch=dispatch,
                route_env=route_envs[agent],
                route_provider=route.provider,
                model=route.model,
                named_agent=f"fno:{agent.replace('_', '-')}",
                headless=True,
            )
            outcome = await spawn_async(agent, prompt, diff_context)
            if not outcome.ok and agents_spawn_runner.is_retryable_failure(outcome):
                fallback = await claude_async(agent, prompt, diff_context)
                fallback.note = (
                    f"{route.provider}/{route.model} unavailable: fell back to claude"
                    if fallback.ok
                    else f"{route.provider}/{route.model} unavailable; "
                    "claude fallback also failed"
                )
                return fallback
            return outcome
        if rp is None or rp.provider == "claude":
            outcome = await claude_async(agent, prompt, diff_context)
            if rp is not None and rp.degraded and rp.reason and outcome.ok:
                outcome.note = rp.reason
            return outcome
        spawn_async = agents_spawn_runner.make_async_runner(
            provider=rp.provider, cwd=run_cwd, timeout=timeout, dispatch=dispatch
        )
        outcome = await spawn_async(agent, prompt, diff_context)
        if not outcome.ok and agents_spawn_runner.is_retryable_failure(outcome):
            # AC5-FR: dispatch/lockout/timeout on the pinned provider -> claude.
            fallback = await claude_async(agent, prompt, diff_context)
            fallback.note = (
                f"{rp.provider} unavailable: fell back to claude"
                if fallback.ok
                else f"{rp.provider} unavailable; claude fallback also failed"
            )
            return fallback
        return outcome

    return _runner, prompts, provider_set


def _read_cross_model_config() -> tuple[dict[str, str], bool]:
    """Read ``(agent_providers, cross_model_enabled)``. Fail-safe to ``({}, False)``.

    The one place both the panel runner and the ``--print-providers`` accessor
    read the cross-model config, so they cannot disagree on whether cross-model
    is engaged.
    """
    try:
        from fno.config import load_settings

        review_cfg = load_settings().review
        return dict(review_cfg.agent_harnesses or {}), bool(review_cfg.cross_model.enabled)
    except Exception as exc:  # noqa: BLE001 - never let config break review
        print(
            f"[review] cross-model config read failed; running all-claude: {exc}",
            file=sys.stderr,
        )
        return {}, False


def _read_agent_routes_config() -> dict[str, AgentRouteBlock]:
    """Read validated explicit per-agent route tuples."""
    from fno.config import load_settings

    return dict(load_settings().review.agent_routes or {})


def _warn_unknown_agent_keys(agent_providers: dict[str, str]) -> None:
    """Warn (stderr) on agent_providers keys that name no real panel agent.

    A key naming an unknown agent is an operator typo - most often the
    hyphenated `code-reviewer` instead of the underscore `code_reviewer`. The
    map is still truthy (cross-model engages) but the bad key matches nothing,
    so without this warning sigma would silently route all-claude. Both the
    panel runner and the --print-providers accessor call this, so neither path
    swallows the typo (parity).
    """
    from fno.review.orchestrator import AGENT_NAMES

    unknown = [k for k in agent_providers if k not in AGENT_NAMES]
    if unknown:
        print(
            "[review] cross-model: config.review.agent_harnesses names unknown "
            f"agent(s) {unknown}; ignoring (known agents: {list(AGENT_NAMES)})",
            file=sys.stderr,
        )


def resolve_session_id(session_id: Optional[str], state_path: Path) -> Optional[str]:
    """Resolve the session nonce: explicit arg, else the state file's value.

    The ONE place both the panel run (``review``) and the ``--print-providers``
    accessor resolve the session, so the implementer-provider read - which
    ``alternate`` cross-model routing excludes - is identical across the two
    surfaces (no drift). Returns ``None`` when neither source has it; callers
    decide whether that is fatal (the panel raises; routing defaults to claude).
    """
    if session_id:
        return session_id
    return _read_state(state_path).get("session_id")


def _publish_durable_sigma(
    report_path: Path,
    *,
    state_path: Path,
    session_id: str,
    reviewed_head: str,
) -> None:
    """Publish direct ``fno do review`` output through the shared sigma writer."""
    state = _read_state(state_path)
    from fno.worker.ship import _read_graph_node_id

    node = _read_graph_node_id(state_path)
    pr_number = state.get("pr_number")
    if not isinstance(node, str):
        print(
            "[review] durable sigma artifact skipped: graph node unavailable",
            file=sys.stderr,
        )
        return

    if not isinstance(pr_number, int) or pr_number < 1:
        try:
            current_pr = subprocess.run(
                ["gh", "pr", "view", "--json", "number", "--jq", ".number"],
                capture_output=True,
                text=True,
            )
            pr_number = int(current_pr.stdout.strip()) if current_pr.returncode == 0 else None
        except (OSError, ValueError):
            pr_number = None
        if pr_number is None:
            print(
                "[review] durable sigma artifact skipped: no PR is bound to this branch",
                file=sys.stderr,
            )
            return

    from fno.config import load_settings
    from fno.paths import vault_root
    from fno.review.artifact import publish_sigma_artifact

    settings = load_settings()
    vault = vault_root()
    project = settings.project.id
    if vault is None or not project:
        print(
            "[review] durable sigma artifact skipped: configured vault/project unavailable",
            file=sys.stderr,
        )
        return

    try:
        current = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(pr_number),
                "--json",
                "headRefOid",
                "--jq",
                ".headRefOid",
            ],
            capture_output=True,
            text=True,
        )
        current_head = current.stdout.strip() if current.returncode == 0 else None
    except OSError:
        current_head = None
    round_id = f"{reviewed_head[:12]}-{session_id[-8:]}-{uuid.uuid4().hex[:8]}"
    result = publish_sigma_artifact(
        report_path,
        reviews_root=vault / "internal",
        project=project,
        node=node,
        pr_number=pr_number,
        reviewed_head=reviewed_head,
        current_head=current_head,
        round_id=round_id,
    )
    print(
        f"[review] sigma artifact: {result.current_path} head={reviewed_head} "
        f"findings={result.finding_count} "
        f"published={str(result.published).lower()} reason={result.reason}",
        file=sys.stderr,
    )


def panel_provider_routing(session_id: Optional[str]) -> dict[str, Any]:
    """Resolve every panel agent -> provider via the SAME path the panel uses.

    The accessor behind ``fno do review --print-providers``: it gives the
    ``/review sigma`` skill the identical per-agent routing the ``fno do review``
    panel would dispatch, so the two surfaces never drift (the "one resolution
    path" invariant). Returns an empty dict when cross-model is OFF (all-claude).
    Never raises.
    """
    agent_providers, enabled = _read_cross_model_config()
    agent_routes = _read_agent_routes_config()
    if not (enabled or agent_providers or agent_routes):
        return {}

    _warn_unknown_agent_keys(agent_providers)

    from fno.review import provider_resolution as pr
    from fno.review.orchestrator import AGENT_NAMES

    implementer = pr.load_implementer_provider(session_id or "")
    available = pr.available_provider_kinds()
    resolved = pr.resolve_panel_providers(
        list(AGENT_NAMES),
        agent_providers=agent_providers,
        implementer_provider=implementer,
        available_providers=available,
        known_agents=AGENT_NAMES,
    )
    if agent_routes:
        from fno.agents.model_routing import resolve_explicit_route

        for agent, route in agent_routes.items():
            _env, resolved[agent] = _resolve_agent_route(
                route, resolve_explicit_route
            )
    return resolved


def review_assurance(
    session_id: Optional[str],
    *,
    size: Optional[str],
    risk_surfaces: Optional[list[str]] = None,
) -> dict[str, Any]:
    """Classify this review's policy and resolve it against real capacity.

    The production accessor behind ``fno do review --assess-assurance``: it assesses
    the policy against the reviewer that will ACTUALLY run, not raw capacity.

    - implementer family + whether it was established come from real ledger
      provenance (``load_implementer_identity``); a session id with no ledger row
      is *unknown*, not defaulted-claude.
    - the effective reviewer kinds are read off the SAME resolved routing the
      panel dispatches (``panel_provider_routing``): cross-model OFF or an
      all-claude pin yields only claude, degraded fallbacks collapse to claude,
      and exhausted kinds are removed. So a codex record that is disabled,
      pinned away, or out of quota cannot certify diversity.

    Never raises.
    """
    from fno.review import provider_resolution as pr
    from fno.review.policy import assess_assurance, classify_review_policy

    implementer, identity_known = pr.load_implementer_identity(session_id or "")

    # exhausted_provider_kinds returns None when the headroom read FAILED. For
    # this gate an unreadable headroom fails CLOSED: we cannot trust a non-claude
    # kind can actually serve, so it does not count toward diversity. Treating a
    # read error as "nothing exhausted" would silently re-open the exact hole.
    exhausted = pr.exhausted_provider_kinds()
    headroom_unknown = exhausted is None
    exhausted = exhausted or set()

    # What will genuinely dispatch. panel_provider_routing() honors cross_model
    # on/off + per-agent pins; {} means all-claude. Degraded -> ran on claude.
    resolved = panel_provider_routing(session_id)
    effective_kinds: set[str] = {"claude"}  # local runtime is always effective
    for rp in resolved.values():
        kind = "claude" if rp.degraded else str(rp.provider).strip().lower()
        if kind in exhausted:
            continue
        # Unreadable headroom -> only claude is a trusted-serveable reviewer.
        if headroom_unknown and kind != "claude":
            continue
        effective_kinds.add(kind)

    policy = classify_review_policy(size=size, risk_surfaces=risk_surfaces)
    verdict = assess_assurance(
        policy,
        effective_reviewer_kinds=sorted(effective_kinds),
        implementer_provider=implementer,
        identity_known=identity_known,
    )
    return {
        "policy": verdict.policy.value,
        "satisfied": verdict.satisfied,
        "effective": verdict.effective,
        "reason": verdict.reason,
        "implementer_provider": implementer,
        "identity_known": identity_known,
        "effective_reviewer_kinds": sorted(effective_kinds),
        "exhausted_kinds": sorted(exhausted),
        "headroom_unknown": headroom_unknown,
    }


def _resolve_cross_model_runner(
    session_id: str, *, worker_pids: list[int]
) -> tuple[Optional[Any], Optional[dict[str, str]], Optional[list[str]]]:
    """Read config + resolve implementer/available, then build the selector.

    Returns ``(None, None, None)`` when cross-model is OFF, on any config-read
    failure (fail-safe to all-claude), or when the panel has no prompts.
    """
    agent_providers, enabled = _read_cross_model_config()
    agent_routes = _read_agent_routes_config()
    if not (enabled or agent_providers or agent_routes):
        return None, None, None

    from fno.review import provider_resolution as pr

    implementer = pr.load_implementer_provider(session_id)
    available = pr.available_provider_kinds()
    base_prompts = load_prompts()
    return build_review_runner(
        agent_providers=agent_providers,
        agent_routes=agent_routes,
        cross_model_enabled=enabled,
        implementer_provider=implementer,
        available_providers=available,
        base_prompts=base_prompts,
        worker_pids=worker_pids,
    )


def _resolve_node_size(state_path: Path) -> Optional[str]:
    """Best-effort read of the bound node's size pin (S/M/L). -> None.

    size is footnote-minted metadata the five-field read contract does not
    carry and the sidecar must not mirror, so the pin is read from the
    default store through the guarded metadata reader: advisory None under an
    external backend (footnote's sizing verbs never ran for such a node).
    """
    try:
        from fno.worker.ship import _read_graph_node_id

        node_id = _read_graph_node_id(state_path)
        if not node_id:
            return None
        from fno.tracker import metadata

        for node in metadata.read_entries("worker.review"):
            if node.get("id") == node_id:
                size = node.get("size")
                return str(size) if size else None
        return None
    except Exception:  # noqa: BLE001 - size is advisory; never break the panel
        return None


def review(
    *,
    diff_context: str,
    state_path: Path,
    artifacts_dir: Optional[Path] = None,
    session_id: Optional[str] = None,
    runner: Optional[Any] = None,
    git_sha_value: Optional[str] = None,
    no_cache: bool = False,
) -> dict[str, Any]:
    """Run the internal sigma-review orchestrator and write a quality_check artifact.

    Args:
        diff_context: Diff text to pass to the review panel.
        state_path: Path to target-state.md (used to resolve session_id when
            not explicitly provided).
        artifacts_dir: Directory to write the review artifact. Defaults to
            <state_path.parent>/artifacts.
        session_id: Explicit session nonce. Falls back to state file value.
        runner: Async WorkerRunner callable for the orchestrator. Required.
            Signature: async (agent, prompt, diff) -> WorkerOutcome.
        git_sha_value: Override the git HEAD SHA for cache key derivation.
            When None, the cache module reads HEAD from git.
        no_cache: When True, bypass both cache read and write.

    Returns:
        {
          "action": "reviewed" | "cached",
          "verdict": str,
          "findings": int (count of kept findings),
          "artifact_path": str,
          "cached": bool,
        }

    Raises:
        fno.review.locking.ReviewLockBusy: when another process holds
            the review lock for this session (caller should exit 11).
        ValueError: when session_id cannot be resolved.
    """
    state_path = Path(state_path)

    # Resolve session_id
    session_id = resolve_session_id(session_id, state_path)
    if not session_id:
        raise ValueError("session_id must be provided or present in state file")

    # Classify assurance on EVERY panel run (size-only here: there is no
    # diff->risk-surface detection, so the high-assurance trigger is reached only
    # via /pr check Step 0c, which hand-derives --risk-surface). This is advisory
    # (the block lives at /pr check + the CLI exit code); emitting it means the
    # classification always happens and travels in the result. Never raises.
    assurance = review_assurance(session_id, size=_resolve_node_size(state_path))
    if not assurance.get("satisfied", True):
        print(
            f"[review] assurance UNRESOLVED (policy={assurance['policy']}): "
            f"{assurance['reason']} - the /pr check gate blocks this PR until a "
            f"different-family reviewer is configured.",
            file=sys.stderr,
        )

    # Resolve artifacts directory
    if artifacts_dir is None:
        artifacts_dir = state_path.parent / "artifacts"
    artifacts_dir = Path(artifacts_dir)
    artifacts_dir.mkdir(parents=True, exist_ok=True)

    artifact_path = artifacts_dir / f"review-{session_id}.md"

    from fno.review import cache as _cache
    from fno.review.confidence_scorer import score_findings
    from fno.review.report_builder import write_artifact

    # --- Cross-model engagement (ab-6c8f4c61) --------------------------------
    # When cross-model is opted in (and no explicit runner was passed) build the
    # per-agent provider selector. OFF (default) -> all three stay None and the
    # all-claude path below runs byte-for-byte unchanged. tracked_pids is
    # defined here (not at the cache-miss block) so the claude sub-runner can
    # register PIDs for SIGINT reaping.
    tracked_pids: list[int] = []
    cross_model_runner = None
    run_prompts: Optional[dict[str, str]] = None
    provider_set: Optional[list[str]] = None
    if runner is None:
        cross_model_runner, run_prompts, provider_set = _resolve_cross_model_runner(
            session_id, worker_pids=tracked_pids
        )

    # --- Worker-level cache: check by session_id + git_sha + prompt_hash ----
    # This layer runs BEFORE calling orchestrate_review_parallel so we can
    # return "cached" correctly even for suspicious (all-clean) results that
    # the orchestrator's inner cache declines to memoize.
    if not no_cache:
        resolved_sha = git_sha_value if git_sha_value is not None else _cache.git_sha()
        # Use the cross-model prompts (JSON-contract-appended) + provider-set
        # when engaged so the cache read/write key matches the actual run and
        # cross-model entries never collide with all-claude entries. Falsy ->
        # legacy 3-arg key (back-compat).
        prompts = run_prompts if run_prompts is not None else load_prompts()
        ph = _cache.prompt_hash(prompts)
        key = _cache.cache_key(session_id, resolved_sha, ph, provider_set)
        cache_body = _cache.read_cache(key, artifacts_dir=artifacts_dir)
        if cache_body is not None:
            try:
                cached_result = _cache.reconstruct_result(cache_body)
            except Exception as exc:
                print(
                    f"[review] cache deserialization failed, re-running: {exc}",
                    file=sys.stderr,
                )
            else:
                raw_findings_count = len(cached_result.findings)
                kept_findings = score_findings(cached_result.findings)
                threshold_drop_suspicious = (
                    raw_findings_count > 0 and len(kept_findings) == 0
                )
                artifact_path, verdict = write_artifact(
                    session_id,
                    OrchestratorResult(
                        findings=kept_findings,
                        workers_completed=cached_result.workers_completed,
                        workers_failed=cached_result.workers_failed,
                        suspicious=(
                            cached_result.suspicious or threshold_drop_suspicious
                        ),
                        duration_seconds=cached_result.duration_seconds,
                        outcomes=cached_result.outcomes,
                    ),
                    artifacts_dir=artifacts_dir,
                )
                _publish_durable_sigma(
                    artifact_path,
                    state_path=state_path,
                    session_id=session_id,
                    reviewed_head=resolved_sha,
                )
                return {
                    "action": "cached",
                    "verdict": verdict,
                    "findings": len(kept_findings),
                    "artifact_path": str(artifact_path),
                    "cached": True,
                    "assurance": assurance,
                }

    # --- Cache miss: run the orchestrator ------------------------------------
    if runner is None:
        runner = (
            cross_model_runner
            if cross_model_runner is not None
            else make_async_runner(worker_pids=tracked_pids)
        )

    result = orchestrate_review_parallel(
        diff_context,
        runner=runner,
        prompts=run_prompts,
        session_id=session_id,
        artifacts_dir=artifacts_dir,
        cache_enabled=not no_cache,
        git_sha_value=git_sha_value,
        worker_pids=tracked_pids,
        provider_set=provider_set,
    )

    # Score findings through the confidence scorer. Default resolver picks
    # the batched `claude -p` scorer when the CLI is on PATH, otherwise
    # falls back to pass-through with a one-shot stderr warning.
    raw_findings_count = len(result.findings)
    kept_findings = score_findings(result.findings)

    # H2 fix: if raw findings existed but all were dropped below threshold,
    # that is suspicious - reviewers found issues but confidence filtered them out.
    # Mark as suspicious so the verdict reflects the uncertainty.
    threshold_drop_suspicious = raw_findings_count > 0 and len(kept_findings) == 0

    scored_result = OrchestratorResult(
        findings=kept_findings,
        workers_completed=result.workers_completed,
        workers_failed=result.workers_failed,
        suspicious=result.suspicious or threshold_drop_suspicious,
        duration_seconds=result.duration_seconds,
        outcomes=result.outcomes,
    )

    artifact_path_final, verdict = write_artifact(
        session_id,
        scored_result,
        artifacts_dir=artifacts_dir,
    )
    reviewed_head = git_sha_value if git_sha_value is not None else _cache.git_sha()
    _publish_durable_sigma(
        artifact_path_final,
        state_path=state_path,
        session_id=session_id,
        reviewed_head=reviewed_head,
    )

    # H6 fix: worker layer does NOT write cache. The orchestrator owns cache
    # writes with its correct skip-suspicious policy. Removing the worker-level
    # write eliminates the double-write conflict where suspicious results were
    # memoized by the worker despite the orchestrator's skip.

    return {
        "action": "reviewed",
        "verdict": verdict,
        "findings": len(kept_findings),
        "artifact_path": str(artifact_path_final),
        "cached": False,
        "assurance": assurance,
    }


def _read_verdict_from_artifact(path: Path) -> str:
    """Read verdict from an existing artifact's frontmatter."""
    try:
        text = path.read_text(encoding="utf-8")
        if not text.startswith("---"):
            return "unknown"
        rest = text[3:].lstrip("\n")
        end = rest.find("\n---")
        if end == -1:
            return "unknown"
        fm = yaml.safe_load(rest[:end]) or {}
        return fm.get("verdict", "unknown")
    except Exception:
        return "unknown"
