"""
DriverWithFallback - wraps a Driver and retries with alternate models.

On rate_limit or overloaded error, tries the next model in the configured
chain before surfacing failure to the caller.

Chain source (in priority order):
1. Explicit `chain` constructor arg
2. `~/.fno/settings.yaml` key `model_fallback.chain`
3. Built-in default: ["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5"]
"""
from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

from .base import InvokeResult, NoCapableDriver, UnsupportedDriverMode

if TYPE_CHECKING:
    from .base import Driver

# Errors that warrant a model fallback (transient capacity issues).
_RETRYABLE_ERRORS = frozenset({"rate_limit", "overloaded"})

_DEFAULT_CHAIN = ["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5"]

def _settings_path() -> Path:
    """Return the settings.yaml path via the typed paths accessor."""
    try:
        from fno import paths as _paths
        return _paths.config_file()
    except Exception:
        return Path.home() / ".fno" / "settings.yaml"


def _load_chain_from_settings() -> list[str] | None:
    """Read model_fallback.chain from settings.yaml.

    Returns None if the file is absent, unreadable, or missing the key.
    Uses yaml.safe_load to avoid an extra dependency (pyyaml already used
    elsewhere in the fno CLI).
    """
    try:
        import yaml  # pyyaml - already a project dependency
    except ImportError:
        return None

    settings_path = _settings_path()
    if not settings_path.exists():
        return None

    try:
        with settings_path.open() as fh:
            data = yaml.safe_load(fh) or {}
        fallback_cfg = data.get("model_fallback", {})
        chain = fallback_cfg.get("chain")
        if isinstance(chain, list) and chain:
            return [str(m) for m in chain]
    except Exception:
        pass

    return None


class DriverWithFallback:
    """Wrap a Driver and retry with alternate models on transient errors."""

    def __init__(self, driver: "Driver", *, chain: list[str] | None = None) -> None:
        self._driver = driver
        if chain is not None:
            self._chain = list(chain)
        else:
            self._chain = _load_chain_from_settings() or list(_DEFAULT_CHAIN)
        self.total_fallbacks: int = 0

    # Expose driver identity so isinstance(wrapper, Driver) works via Protocol.
    @property
    def name(self) -> str:
        return self._driver.name

    def is_available(self) -> bool:
        return self._driver.is_available()

    def invoke(
        self,
        *,
        prompt: str,
        max_turns: int = 15,
        budget_usd: float = 25.0,
        model: str | None = None,
        cwd: str | None = None,
        timeout_seconds: int = 5400,
    ) -> InvokeResult:
        """Invoke with fallback model chain on retryable errors."""
        last_result: InvokeResult | None = None

        for idx, chain_model in enumerate(self._chain):
            # Use explicitly requested model for the very first attempt
            # (respects caller's choice). On retries, use chain models.
            effective_model = model if (idx == 0 and model is not None) else chain_model

            result = self._driver.invoke(
                prompt=prompt,
                max_turns=max_turns,
                budget_usd=budget_usd,
                model=effective_model,
                cwd=cwd,
                timeout_seconds=timeout_seconds,
            )

            if result.error_class not in _RETRYABLE_ERRORS:
                # Success or non-retryable error - return immediately.
                return result

            # Retryable error: count and continue.
            last_result = result
            self.total_fallbacks += 1

        # All models exhausted - return the last failure.
        assert last_result is not None
        return last_result

    def invoke_review(
        self,
        *,
        prompt: str,
        max_turns: int = 50,
        budget_usd: float = 50.0,
        model: str | None = None,
        cwd: str | None = None,
        timeout_seconds: int = 1800,
    ) -> InvokeResult:
        """Delegate invoke_review to the inner driver.

        If the inner driver raises UnsupportedDriverMode, re-raise as
        NoCapableDriver so the host/walker can park the node with reason
        "review_unsupported" rather than propagating a confusing inner error.
        """
        try:
            return self._driver.invoke_review(
                prompt=prompt,
                max_turns=max_turns,
                budget_usd=budget_usd,
                model=model,
                cwd=cwd,
                timeout_seconds=timeout_seconds,
            )
        except UnsupportedDriverMode as exc:
            raise NoCapableDriver(
                f"Driver '{self._driver.name}' does not support review mode. "
                "Configure claude-code as the primary driver to enable sigma-review "
                "inside megawalk's headless invocation."
            ) from exc
