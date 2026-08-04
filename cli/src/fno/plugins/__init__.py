"""Function-pack substrate: versioned, verifiable capability as data, not code.

A pack declares roles, skills, workflows, adapters, evaluators, assets, a
maximum-expected effect ceiling, and benchmark scenarios. Verification reports
each condition on two independent axes (checked/unchecked and a four-state
result) before any activation. Activation projects packaged roles into the
``RoleLayer.PLUGIN`` directory the role registry already walks, making them
resolvable through the untouched resolver; it grants no capability and no
effect, never writes ``config.approvals.authorized_principals``, and never mints
a ``CapabilityFact``.

This package contains no branch on a pack id or a function name. A pack is
content under ``plugins/``, discovered by path and validated by schema.
"""

from fno.plugins.manifest import (
    AdapterConformance,
    AdapterDeclaration,
    AgentDeclaration,
    AssetDeclaration,
    CompatibilityRange,
    EffectDeclaration,
    EvaluatorDeclaration,
    PackDependency,
    PackManifest,
    ScenarioDeclaration,
    SkillDeclaration,
    WorkflowDeclaration,
    pack_digest,
)

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
