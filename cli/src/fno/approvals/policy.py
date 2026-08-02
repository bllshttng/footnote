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

from collections.abc import Mapping, Sequence

__all__ = ["ConfigAuthority", "WILDCARD_EFFECT_CLASS", "load_authority"]

#: Key matching every effect class. The solo-founder case: one principal decides
#: everything that is not denied outright by core policy.
WILDCARD_EFFECT_CLASS = "*"


class ConfigAuthority:
    """Authority backed by an effect-class -> principal-ids mapping."""

    source = "config.approvals.authorized_principals"

    def __init__(self, authorized: Mapping[str, Sequence[str]] | None = None) -> None:
        self._authorized: dict[str, frozenset[str]] = {
            effect_class: frozenset(principals)
            for effect_class, principals in (authorized or {}).items()
        }

    def may_approve(self, *, principal_id: str, effect_class: str, destination: str) -> bool:
        """Return True only for a principal named by policy for this effect class.

        ``destination`` is part of the interface because a richer policy will
        need it (an approval for one mailing list is not one for another). This
        implementation does not read it, so a policy that must discriminate by
        destination should replace this class rather than extend the config
        shape in place.
        """
        if principal_id in self._authorized.get(effect_class, frozenset()):
            return True
        return principal_id in self._authorized.get(WILDCARD_EFFECT_CLASS, frozenset())

    @property
    def is_configured(self) -> bool:
        return any(self._authorized.values())


def load_authority() -> ConfigAuthority:
    """Build the authority from project config. Fails closed when unset."""
    from fno.config import load_settings

    settings = load_settings()
    return ConfigAuthority(settings.approvals.authorized_principals)
