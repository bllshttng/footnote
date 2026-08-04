"""Frozen, non-granting declarations for a function-pack manifest.

A pack describes business capability as versioned data. It reuses the merged
identity contracts verbatim rather than defining parallel types, because the
roles it ships are projected into the ``RoleLayer.PLUGIN`` directory the role
registry already walks and the resolver already digests. A divergent role or
function identity type would compute a different manifest digest and break the
identity and overlay checks ``resolve_role`` performs.

Two declaration families are named as declarations, never as facts:

* ``permissions`` is the pack's MAXIMUM EXPECTED EFFECT ceiling, read for review
  and reported by the verifier. Activation grants none of it.
* adapter ``conformance`` mirrors :class:`fno.approvals.models.AdapterCapability`
  field-for-field. ``approvals-and-effects.md`` states the approval store cannot
  verify these, so the registry records which pack digest declared which
  conformance, making a false declaration attributable after the fact. The
  docstrings say "declares", not "proves".

The manifest model is pure data. It does not validate a packaged role's
``default_topology`` vocabulary, because ``RoleManifest`` carries that field as a
bare ``NonEmptyStr`` and the closed four-literal vocabulary lives in
``fno.company.topology``. The verifier checks it there; re-pinning the literals
here would be a second copy of the vocabulary and a scope violation.
"""

from __future__ import annotations

from typing import Self

from pydantic import BaseModel, ConfigDict, model_validator

from fno.company.contracts import EvidenceResult, NonEmptyStr, RoleRef
from fno.roles.models import ContextKind, RoleManifest, canonical_digest
from packaging.version import InvalidVersion, Version

__all__ = [
    "AdapterConformance",
    "AdapterDeclaration",
    "AgentDeclaration",
    "AssetDeclaration",
    "CompatibilityRange",
    "EffectDeclaration",
    "EvaluatorDeclaration",
    "PackDependency",
    "PackManifest",
    "ScenarioDeclaration",
    "SkillDeclaration",
    "WorkflowDeclaration",
    "pack_digest",
]


class _PackModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class CompatibilityRange(_PackModel):
    """Declared Footnote version range a pack is compatible with."""

    minimum: NonEmptyStr
    maximum: NonEmptyStr | None = None

    @model_validator(mode="after")
    def _endpoints_are_pep440(self) -> Self:
        # Both endpoints must be PEP 440 so range comparisons are meaningful; this
        # validator covers footnote_compat ranges and every dependency range.
        for label, value in (("minimum", self.minimum), ("maximum", self.maximum)):
            if value is None:
                continue
            try:
                Version(value)
            except InvalidVersion as exc:
                raise ValueError(f"{label} {value!r} is not a valid (PEP 440) version: {exc}") from exc
        if self.maximum is not None and Version(self.maximum) < Version(self.minimum):
            raise ValueError(
                f"maximum {self.maximum!r} is lower than minimum {self.minimum!r}"
            )
        return self


class PackDependency(_PackModel):
    """Another pack this one declares a dependency on."""

    pack_id: NonEmptyStr
    version_range: CompatibilityRange


class SkillDeclaration(_PackModel):
    """A skill contribution, bundled at build time (no runtime Skill() call)."""

    id: NonEmptyStr
    source: NonEmptyStr


class AgentDeclaration(_PackModel):
    """A subagent contribution, bundled at build time (no runtime Task dispatch by the pack).

    The agent's frontmatter is copied verbatim at bundle time, never synthesized,
    so the source file's ``tools`` list IS the tool list that runs. Two verify
    conditions bind that copied list to the role: ``agent-role-binding`` fails when
    ``role`` names a role this pack does not declare, and ``agent-tools-bounded``
    fails when the copied ``tools`` hold anything outside the bound role's
    authority ceiling. Checking the copied frontmatter is what keeps the agent's
    real tool list honest, since two copies of a tool list is how one drifts.
    """

    id: NonEmptyStr
    source: NonEmptyStr
    role: RoleRef | None = None


class WorkflowDeclaration(_PackModel):
    """A packaged workflow template."""

    id: NonEmptyStr
    source: NonEmptyStr


class AdapterConformance(_PackModel):
    """Adapter self-declaration, mirroring AdapterCapability field-for-field.

    Declares, never proves: ``approvals-and-effects.md`` assigns verifying a
    capability against a real adapter identity to whoever owns the adapter
    registry. This node is that owner, so the registry records which pack digest
    declared these flags; it cannot prevent a false declaration before the fact.
    """

    adapter_id: NonEmptyStr
    adapter_version: NonEmptyStr
    remote_idempotency: bool = False
    reconciliation: bool = False


class AdapterDeclaration(_PackModel):
    """An external destination adapter the pack declares it may reach."""

    id: NonEmptyStr
    destination: NonEmptyStr
    conformance: AdapterConformance


class EvaluatorDeclaration(_PackModel):
    """A delivery evaluator the pack ships as a declaration.

    Packs ship evaluator declarations and never compute an aggregate verdict;
    ``fno.delivery`` evaluates.
    """

    id: NonEmptyStr
    command: NonEmptyStr
    required: bool = True


class AssetDeclaration(_PackModel):
    """A packaged asset, referenced as a ContextKind.ARTIFACT context reference.

    The merged ContextKind enum is untouched: a packaged asset is an artifact,
    not a new context kind.
    """

    id: NonEmptyStr
    source: NonEmptyStr
    kind: ContextKind = ContextKind.ARTIFACT


class EffectDeclaration(_PackModel):
    """One maximum-expected effect in the pack's review ceiling.

    Reuses the effect-class and destination vocabulary that
    :func:`fno.approvals.models.classify_effect` and the approval authority
    consult at decision time. A pack cannot know the work order and attempt a
    concrete :class:`fno.company.contracts.EffectRef` is bound to, so the ceiling
    carries only the vocabulary the review and authority surfaces read. It
    declares an expected effect; it grants nothing, and activation never acts on
    it.
    """

    effect_class: NonEmptyStr
    destination: NonEmptyStr


class ScenarioDeclaration(_PackModel):
    """A benchmark scenario supplying declared-test evidence.

    ``command`` is the runnable check and ``recorded_result`` is the last result
    captured from running it. The verifier reports whether the command is
    present and executable and reflects the recorded result honestly.
    """

    id: NonEmptyStr
    command: NonEmptyStr
    recorded_result: EvidenceResult


class PackManifest(_PackModel):
    """One versioned function-pack: identity plus component declarations.

    ``roles`` carries full :class:`RoleManifest` objects, reused unchanged; they
    are what activation projects into the plugin role layer. A pack declaring
    zero roles is legal (assets and evaluators only); a pack declaring zero
    components at all is refused as empty.
    """

    id: NonEmptyStr
    version: NonEmptyStr
    footnote_compat: CompatibilityRange
    roles: tuple[RoleManifest, ...] = ()
    skills: tuple[SkillDeclaration, ...] = ()
    agents: tuple[AgentDeclaration, ...] = ()
    workflows: tuple[WorkflowDeclaration, ...] = ()
    adapters: tuple[AdapterDeclaration, ...] = ()
    evaluators: tuple[EvaluatorDeclaration, ...] = ()
    assets: tuple[AssetDeclaration, ...] = ()
    permissions: tuple[EffectDeclaration, ...] = ()
    scenarios: tuple[ScenarioDeclaration, ...] = ()
    depends_on: tuple[PackDependency, ...] = ()

    @model_validator(mode="after")
    def _declarations_are_unique(self) -> Self:
        self._check_unique("roles", [role.role.id for role in self.roles])
        self._check_unique(
            "permissions",
            [(item.effect_class, item.destination) for item in self.permissions],
        )
        for field_name, items in (
            ("skills", self.skills),
            ("agents", self.agents),
            ("workflows", self.workflows),
            ("adapters", self.adapters),
            ("evaluators", self.evaluators),
            ("assets", self.assets),
            ("scenarios", self.scenarios),
        ):
            self._check_unique(field_name, [item.id for item in items])
        return self

    @staticmethod
    def _check_unique(field_name: str, keys: list[object]) -> None:
        seen: set[object] = set()
        for key in keys:
            if key in seen:
                raise ValueError(f"duplicate {field_name} declaration {key!r}")
            seen.add(key)

    @model_validator(mode="after")
    def _pack_is_not_empty(self) -> Self:
        component_counts = (
            self.roles,
            self.skills,
            self.agents,
            self.workflows,
            self.adapters,
            self.evaluators,
            self.assets,
            self.permissions,
            self.scenarios,
        )
        if not any(component_counts):
            raise ValueError("a pack must declare at least one component")
        return self

    @model_validator(mode="after")
    def _version_is_pep440(self) -> Self:
        # The pack version must be PEP 440 so range comparisons are meaningful.
        # Range endpoints are validated on CompatibilityRange itself.
        try:
            Version(self.version)
        except InvalidVersion as exc:
            raise ValueError(f"version {self.version!r} is not a valid (PEP 440) version: {exc}") from exc
        return self


def pack_digest(manifest: PackManifest) -> str:
    """Stable sha256 over the canonical pack-manifest serialization."""
    return canonical_digest("pack-manifest", manifest.model_dump(mode="json"))
