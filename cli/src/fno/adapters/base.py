"""RuntimeAdapter Protocol and supporting types."""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal, Protocol, TypedDict, Union, runtime_checkable


class AdapterCallResult(TypedDict):
    """Return shape of :meth:`RuntimeAdapter.call_api`."""

    ok: bool
    stdout: str
    stderr: str
    returncode: int


class SpawnSucceeded(TypedDict):
    """External-spawn success envelope from :meth:`RuntimeAdapter.spawn_worker`."""

    action: Literal["spawned"]
    worker_id: str
    pid: int
    started_at: str


class SkillDispatchRequired(TypedDict):
    """In-session sentinel from :meth:`RuntimeAdapter.spawn_worker`.

    Caller must dispatch the worker via the Agent tool rather than spawning
    a subprocess; in-session shell spawn is forbidden.
    """

    action: Literal["skill_dispatch_required"]
    next_step: str
    worker_id: str
    reason: str


class _SpawnFailedRequired(TypedDict):
    """Required keys on every :class:`SpawnFailed` envelope."""

    action: Literal["spawn_failed"]
    worker_id: str


class SpawnFailed(_SpawnFailedRequired, total=False):
    """Spawn-failure envelope from :meth:`RuntimeAdapter.spawn_worker`.

    ``action`` and ``worker_id`` are always present (inherited from the
    required base). The remaining fields are populated only when the
    relevant diagnostic data is available (``FileNotFoundError`` carries
    only ``error``; an early-exit child carries ``pid`` / ``returncode`` /
    ``stdout`` / ``stderr`` / ``early_exit``).
    """

    pid: int
    returncode: int
    stdout: str
    stderr: str
    error: str
    early_exit: bool


SpawnResult = Union[SpawnSucceeded, SkillDispatchRequired, SpawnFailed]
"""Discriminated union of :meth:`RuntimeAdapter.spawn_worker` return shapes.

Every member carries an ``action`` discriminator so consumers can ``match``
on it without ``KeyError``.
"""


@dataclass
class AdapterHealth:
    """Health report returned by :meth:`RuntimeAdapter.health`.

    When ``ok`` is ``False`` the ``details`` dict MUST carry a non-empty
    ``"reason"`` string so callers can render an actionable message
    without spelunking call-site-specific keys. When ``ok`` is ``True``
    the ``details`` dict is unconstrained.
    """

    ok: bool
    details: dict = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.ok:
            return
        reason = self.details.get("reason")
        if not isinstance(reason, str) or not reason.strip():
            raise ValueError(
                "AdapterHealth(ok=False) requires details['reason'] "
                "to be a non-empty (non-whitespace) string"
            )


@runtime_checkable
class RuntimeAdapter(Protocol):
    """Contract for runtime adapter implementations.

    Exactly three primitives (+ health). Adding a fourth primitive is a
    deviation event - stop and reconsider the abstraction.
    """

    name: str

    def spawn_worker(self, *, prompt: str, **kwargs) -> SpawnResult:
        """Spawn a new worker agent for the given prompt.

        Returns a :data:`SpawnResult` discriminated by the ``action`` key:
        ``"spawned"`` (external success), ``"skill_dispatch_required"``
        (in-session sentinel), or ``"spawn_failed"`` (binary missing or
        early child exit).
        """
        ...

    def create_worktree(self, *, name: str, base: str = "main") -> dict:
        """Create a git worktree at ``~/.fno/worktrees/{proj}-{name}/`` on branch ``feature/{name}``.

        ``proj`` is the project id resolved from ``.fno/settings.yaml``
        (or derived from the git remote basename); see
        :func:`fno.worktree_paths.resolve_project_id`.

        Returns: {"worktree_path": str, "branch": str, "status": str}
        """
        ...

    def call_api(self, *, command: list[str], retries: int = 3) -> AdapterCallResult:
        """Invoke an adapter API command with retry logic.

        Returns an :class:`AdapterCallResult` with the ``ok`` boolean
        derived from ``returncode == 0`` so callers can branch without
        re-checking the exit code shape.
        """
        ...

    def health(self) -> AdapterHealth:
        """Return health status of this adapter."""
        ...
