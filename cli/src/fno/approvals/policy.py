"""The authority seam: who may approve which effect class.

This package does not own the answer. It owns only the question, so that the
store has exactly one place to ask and no transport, role, plugin, or CLI caller
can answer on its own behalf. The concrete answer here comes from project config
(``config.approvals.authorized_principals``), which is independent of every
runtime object that might want to approve something.

Unconfigured means unauthorized. A fresh install can inspect approvals but
cannot decide one until a human writes the policy down.
"""

from __future__ import annotations

from collections.abc import Callable, Mapping, Sequence

__all__ = ["ConfigAuthority", "WILDCARD_EFFECT_CLASS", "load_authority"]

#: Key matching every effect class. The solo-founder case: one principal decides
#: everything that is not denied outright by core policy.
WILDCARD_EFFECT_CLASS = "*"


def _load_from_config() -> Mapping[str, Sequence[str]]:
    from fno.config import load_settings

    return load_settings().approvals.authorized_principals


class ConfigAuthority:
    """Authority backed by an effect-class -> principal-ids mapping.

    The store revalidates authority at execution time as well as at decision
    time, which is only meaningful if the answer can actually change in between.
    So the mapping is resolved per call through ``source_loader`` rather than
    frozen at construction: a principal revoked in config after this object was
    built is genuinely revoked, not merely revoked-looking.

    Passing ``authorized`` explicitly pins a fixed mapping, which is for tests
    and for callers that already hold resolved policy. That form IS a snapshot,
    and does not observe later config edits.
    """

    source = "config.approvals.authorized_principals"

    def __init__(
        self,
        authorized: Mapping[str, Sequence[str]] | None = None,
        *,
        source_loader: Callable[[], Mapping[str, Sequence[str]]] | None = None,
    ) -> None:
        if authorized is not None and source_loader is not None:
            raise ValueError("pass a fixed mapping or a loader, not both")
        self._pinned: Mapping[str, Sequence[str]] | None = authorized
        self._loader = source_loader

    def _resolve(self) -> dict[str, frozenset[str]]:
        if self._pinned is not None:
            raw: Mapping[str, Sequence[str]] = self._pinned
        elif self._loader is not None:
            raw = self._loader()
        else:
            raw = {}
        return {
            effect_class: frozenset(principals) for effect_class, principals in raw.items()
        }

    def may_approve(self, *, principal_id: str, effect_class: str, destination: str) -> bool:
        """Return True only for a principal named by policy for this effect class.

        ``destination`` is part of the interface because a richer policy will
        need it (an approval for one mailing list is not one for another). This
        implementation does not read it, so a policy that must discriminate by
        destination should replace this class rather than extend the config
        shape in place.
        """
        authorized = self._resolve()
        if principal_id in authorized.get(effect_class, frozenset()):
            return True
        return principal_id in authorized.get(WILDCARD_EFFECT_CLASS, frozenset())

    @property
    def is_configured(self) -> bool:
        return any(self._resolve().values())


def load_authority() -> ConfigAuthority:
    """Build the authority from project config. Fails closed when unset."""
    return ConfigAuthority(source_loader=_load_from_config)
