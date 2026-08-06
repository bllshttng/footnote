"""Live routing default for per-agent sigma-review routing.

Run: cd cli && uv run pytest -v ../tests/integration/test_per_agent_routing_bdd_invariants.py

The sigma dispatch instrumentation this suite once exercised - the
``dispatch_sigma_subagent`` context manager, its ``subagent_spawn`` /
``subagent_complete`` events, the ``record_dispatch`` sidecar, and the
``verify-evidence event`` pair verifier - was removed for cause: production
sigma dispatch goes through the raw Task tool and never reached any of it, so
the suite certified a path production never touches. Those invariants went
with the machinery. What remains is the one live routing case: the providers
config that routing resolves against, and its back-compat default.
"""
from __future__ import annotations

from pathlib import Path

import yaml


def _write_settings(tmp_path: Path, settings: dict) -> Path:
    """Write a settings.yaml under tmp_path/.fno/ and return its path."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    p = state_dir / "settings.yaml"
    p.write_text(yaml.safe_dump(settings, sort_keys=False), encoding="utf-8")
    return p


def _baseline_settings(
    active: str = "claude-anthropic",
    agents: dict | None = None,
) -> dict:
    """Return a minimal valid settings.yaml dict."""
    base: dict = {
        "config": {
            "providers": {
                "active": active,
                "records": [
                    {
                        "id": "claude-anthropic",
                        "name": "Claude Anthropic",
                        "cli": "claude",
                        "auth": "oauth_dir",
                        "credentials_source": "~/.claude",
                        "priority": 10,
                    },
                    {
                        "id": "gemini-pro",
                        "name": "Gemini Pro",
                        "cli": "gemini",
                        "auth": "api_key",
                        "env": {"GEMINI_API_KEY": "test-key"},
                        "priority": 20,
                    },
                    {
                        "id": "codex-openai",
                        "name": "Codex OpenAI",
                        "cli": "codex",
                        "auth": "api_key",
                        "env": {"OPENAI_API_KEY": "test-key"},
                        "priority": 30,
                    },
                ],
                "failover": {"max_swaps_per_phase": 5},
            }
        }
    }
    if agents is not None:
        base["config"]["agents"] = agents
    return base


# ---------------------------------------------------------------------------
# Invariant 1: agents.<name>.provider unset -> uses global active provider
# ---------------------------------------------------------------------------


def test_unset_uses_global_default(tmp_path: Path) -> None:
    """config.agents absent -> ProvidersConfig.agents is empty dict.

    When no config.agents block is present, load_providers returns a
    ProvidersConfig where agents == {} (empty dict). Dispatch falls through to
    the global active provider -- the agents map carries no overrides.
    """
    settings = _baseline_settings()  # no 'agents' key at all
    _write_settings(tmp_path, settings)

    from fno.adapters.providers.loader import load_providers

    cfg = load_providers(repo_root=tmp_path)

    assert cfg.agents == {}, (
        f"Expected empty agents dict for back-compat, got {cfg.agents!r}"
    )
    assert cfg.active == "claude-anthropic"
