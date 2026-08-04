"""Pure two-axis verification of a function-pack before activation.

Every condition reports on two independent axes:

* ``checked`` -- was this condition evaluated at all?
* ``result`` -- one of ``passed``, ``failed``, ``blocked``, ``unknown`` (reusing
  :class:`fno.company.contracts.EvidenceResult`).

Collapsing the axes into one boolean is how a verifier starts reporting green
for a condition it never evaluated. An unchecked condition reporting ``unknown``
is the honest shape for a check that could not run; an all-``passed`` report is
the only one that exits zero.

Verification is pure: it reads the manifest and an installed-pack index and
writes nothing, activates nothing, mutates no graph, and touches no effect
journal. It is runnable against a pack that is not installed, which is the whole
point of verifying before activation.

A malformed, unreadable, or invalid-UTF-8 manifest reports ``blocked`` naming
the file and the parse error and never reports the pack as absent, mirroring the
role registry's rule that a corrupt source blocks rather than reading as missing.
"""

from __future__ import annotations

import hashlib
import os
import shutil
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from enum import Enum
from pathlib import Path
from typing import Any, Mapping

import yaml
from pydantic import ValidationError

from fno.company.contracts import EvidenceResult, WorkOrderRef
from fno.plugins.manifest import PackManifest, pack_digest
from fno.roles.models import (
    CapabilityFact,
    ContextBundleBounds,
    ContextReference,
    DefinitionStatus,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoleResolutionBlocked,
    Sensitivity,
)
from fno.roles.resolver import resolve_role

__all__ = [
    "Condition",
    "ConditionFamily",
    "VerificationReport",
    "load_manifest",
    "resolve_manifest_path",
    "verify_manifest",
    "verify_pack",
]


class ConditionFamily(str, Enum):
    IDENTITY = "identity"
    SCHEMA = "schema"
    DEPENDENCY = "dependency"
    CAPABILITY = "capability"
    COMPATIBILITY = "compatibility"
    DECLARED_TEST = "declared-test"


@dataclass(frozen=True)
class Condition:
    family: ConditionFamily
    name: str
    checked: bool
    result: EvidenceResult
    detail: str | None = None

    @property
    def ok(self) -> bool:
        return self.checked and self.result is EvidenceResult.PASSED


@dataclass(frozen=True)
class VerificationReport:
    pack_path: str
    conditions: tuple[Condition, ...] = field(default_factory=tuple)

    @property
    def ok(self) -> bool:
        return bool(self.conditions) and all(condition.ok for condition in self.conditions)

    def by_family(self) -> dict[ConditionFamily, tuple[Condition, ...]]:
        grouped: dict[ConditionFamily, tuple[Condition, ...]] = {}
        for condition in self.conditions:
            grouped.setdefault(condition.family, ())
            grouped[condition.family] = (*grouped[condition.family], condition)
        return grouped

    def as_dict(self) -> dict[str, Any]:
        return {
            "pack_path": self.pack_path,
            "ok": self.ok,
            "conditions": [
                {
                    "family": condition.family.value,
                    "name": condition.name,
                    "checked": condition.checked,
                    "result": condition.result.value,
                    "detail": condition.detail,
                }
                for condition in self.conditions
            ],
        }


def resolve_manifest_path(path: Path) -> Path:
    """Resolve a directory or file argument to the plugin.yaml manifest file."""
    if path.is_dir():
        return path / "plugin.yaml"
    return path


def load_manifest(path: Path) -> tuple[PackManifest | None, Condition | None]:
    """Load and validate a manifest, or return a single blocked condition.

    A missing, unreadable, invalid-UTF-8, malformed, or schema-invalid manifest
    yields a ``blocked`` condition naming the file and the error. The caller
    never sees the pack reported as absent.
    """
    manifest_file = resolve_manifest_path(path)
    label = str(manifest_file)
    try:
        text = manifest_file.read_text(encoding="utf-8")
    except OSError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"unreadable manifest {label}: {type(exc).__name__}: {exc}",
        )
    except UnicodeDecodeError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"invalid UTF-8 in manifest {label}: {exc}",
        )
    try:
        raw = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"malformed YAML in manifest {label}: {exc}",
        )
    if not isinstance(raw, Mapping):
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"manifest {label} is not a mapping",
        )
    try:
        manifest = PackManifest.model_validate(raw)
    except ValidationError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-schema",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"manifest {label} failed schema validation: {exc}",
        )
    return manifest, None


def _is_safe_path_component(value: str) -> bool:
    # Same rule activation enforces: an id becomes a path segment under
    # <root>/plugin/<pack>/<role>.json, so it must be a single safe component.
    return bool(
        value
        and "/" not in value
        and "\\" not in value
        and value not in (".", "..")
        and "\x00" not in value
    )


def _identity_conditions(manifest: PackManifest, path: str) -> list[Condition]:
    pack_id_safe = _is_safe_path_component(manifest.id)
    conditions: list[Condition] = [
        Condition(
            ConditionFamily.IDENTITY,
            "pack-id",
            checked=True,
            result=EvidenceResult.PASSED if pack_id_safe else EvidenceResult.FAILED,
            detail=(
                f"id={manifest.id} version={manifest.version}"
                if pack_id_safe
                else f"unsafe pack id {manifest.id!r}: must be a single path component"
            ),
        )
    ]
    first = pack_digest(manifest)
    second = pack_digest(manifest)
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "digest-stability",
            checked=True,
            result=EvidenceResult.PASSED if first == second else EvidenceResult.FAILED,
            detail=first,
        )
    )
    role_ids = [role.role.id for role in manifest.roles]
    function_ids = {role.function.id for role in manifest.roles}
    unsafe_role_ids = [rid for rid in role_ids if not _is_safe_path_component(rid)]
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "role-ids-safe",
            checked=True,
            result=EvidenceResult.PASSED if not unsafe_role_ids else EvidenceResult.FAILED,
            detail=None if not unsafe_role_ids else f"unsafe role id {unsafe_role_ids[0]!r}",
        )
    )
    duplicate = next((value for value in role_ids if role_ids.count(value) > 1), None)
    lower_ids = [rid.lower() for rid in role_ids]
    case_duplicate = next((rid for rid in role_ids if lower_ids.count(rid.lower()) > 1), None)
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "role-ids-case-unique",
            checked=True,
            result=EvidenceResult.PASSED if case_duplicate is None else EvidenceResult.FAILED,
            detail=(
                None
                if case_duplicate is None
                else f"role id {case_duplicate!r} collides case-insensitively"
            ),
        )
    )
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "role-and-function-ids-unique",
            checked=True,
            result=EvidenceResult.PASSED if duplicate is None else EvidenceResult.FAILED,
            detail=None if duplicate is None else f"duplicate role id {duplicate!r}",
        )
    )
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "function-set",
            checked=True,
            result=EvidenceResult.PASSED,
            detail=",".join(sorted(function_ids)) or "no roles",
        )
    )
    return conditions


def _schema_conditions(manifest: PackManifest) -> list[Condition]:
    # Every declaration already parsed into a frozen model rejecting unknown
    # fields; reaching here means the schema held.
    component_counts = {
        "roles": len(manifest.roles),
        "skills": len(manifest.skills),
        "workflows": len(manifest.workflows),
        "adapters": len(manifest.adapters),
        "evaluators": len(manifest.evaluators),
        "assets": len(manifest.assets),
        "permissions": len(manifest.permissions),
        "scenarios": len(manifest.scenarios),
    }
    return [
        Condition(
            ConditionFamily.SCHEMA,
            "declarations-parse",
            checked=True,
            result=EvidenceResult.PASSED,
            detail="; ".join(f"{name}={count}" for name, count in component_counts.items()),
        )
    ]


def _in_range(resolved: str, minimum: str, maximum: str | None) -> bool:
    """True when ``resolved`` is within [minimum, maximum], strict PEP 440.

    Raises ``packaging.version.InvalidVersion`` (a ``ValueError``) when a version
    is not PEP 440 parseable, so a malformed version is surfaced as a failure
    rather than silently satisfying a range via a permissive fallback.
    """
    from packaging.version import Version

    resolved_version = Version(resolved)
    if resolved_version < Version(minimum):
        return False
    if maximum is not None and resolved_version > Version(maximum):
        return False
    return True


def _dependency_conditions(
    manifest: PackManifest,
    installed: Mapping[str, str] | None,
) -> list[Condition]:
    if installed is None:
        return [
            Condition(
                ConditionFamily.DEPENDENCY,
                "installed-pack-index",
                checked=False,
                result=EvidenceResult.UNKNOWN,
                detail="no installed-pack index supplied; dependencies not resolved",
            )
        ]
    conditions: list[Condition] = []
    for dependency in manifest.depends_on:
        resolved = installed.get(dependency.pack_id)
        if resolved is None:
            conditions.append(
                Condition(
                    ConditionFamily.DEPENDENCY,
                    f"dependency:{dependency.pack_id}",
                    checked=True,
                    result=EvidenceResult.BLOCKED,
                    detail=(
                        f"declared dependency {dependency.pack_id} "
                        f"({dependency.version_range.minimum}.."
                        f"{dependency.version_range.maximum or 'open'}) is not installed"
                    ),
                )
            )
            continue
        try:
            in_range = _in_range(
                resolved, dependency.version_range.minimum, dependency.version_range.maximum
            )
        except ValueError as exc:
            conditions.append(
                Condition(
                    ConditionFamily.DEPENDENCY,
                    f"dependency:{dependency.pack_id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=f"{dependency.pack_id} version {resolved!r} is not parseable: {exc}",
                )
            )
            continue
        if not in_range:
            conditions.append(
                Condition(
                    ConditionFamily.DEPENDENCY,
                    f"dependency:{dependency.pack_id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=(
                        f"{dependency.pack_id} resolved to {resolved}, outside "
                        f"{dependency.version_range.minimum}..{dependency.version_range.maximum or 'open'}"
                    ),
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.DEPENDENCY,
                    f"dependency:{dependency.pack_id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"{dependency.pack_id} resolved to {resolved}",
                )
            )
    if not manifest.depends_on:
        conditions.append(
            Condition(
                ConditionFamily.DEPENDENCY,
                "no-declared-dependencies",
                checked=True,
                result=EvidenceResult.PASSED,
                detail=None,
            )
        )
    return conditions


def _capability_conditions(manifest: PackManifest) -> list[Condition]:
    # The structural guarantee that no declaration is a grant is enforced by the
    # manifest model itself: permissions is a ceiling of effect_class+destination
    # pairs with no approval field, and adapter conformance mirrors
    # AdapterCapability as declaration fields. There is no capability catalog in
    # this node, so the check is structural rather than catalog-driven.
    grant_leak = any(
        getattr(declaration, "approval_id", None) is not None
        or getattr(declaration, "principal_id", None) is not None
        for declaration in manifest.permissions
    )
    return [
        Condition(
            ConditionFamily.CAPABILITY,
            "declarations-are-not-grants",
            checked=True,
            result=EvidenceResult.FAILED if grant_leak else EvidenceResult.PASSED,
            detail=None if not grant_leak else "a permission declaration carries an approval/principal field",
        )
    ]


def _topology_conditions(manifest: PackManifest) -> list[Condition]:
    try:
        from fno.company.topology import TopologyRefusal, validate_manifest_topology
    except ImportError as exc:
        return [
            Condition(
                ConditionFamily.COMPATIBILITY,
                "topology-vocabulary",
                checked=True,
                result=EvidenceResult.BLOCKED,
                detail=f"topology vocabulary module absent: {exc}",
            )
        ]
    conditions: list[Condition] = []
    for role in manifest.roles:
        outcome = validate_manifest_topology(role.default_topology, source_layer=RoleLayer.PLUGIN)
        if isinstance(outcome, TopologyRefusal):
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"topology:{role.role.id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=f"{role.role.id} default_topology {outcome.value!r}: {outcome.recovery}",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"topology:{role.role.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"{role.role.id} default_topology={outcome.value}",
                )
            )
    return conditions


def _synthetic_resolution_inputs(
    manifest: RoleManifest,
    *,
    revision: str,
    source_id: str,
) -> tuple[
    tuple[RoleDefinitionSource, ...],
    tuple[CapabilityFact, ...],
    tuple[ContextReference, ...],
    WorkOrderRef,
]:
    role = manifest.role
    work_order = WorkOrderRef(node_id="verify-pack", attempt_id="verify-attempt", role_id=role.id)
    definition = RoleDefinitionSource(
        layer=RoleLayer.PLUGIN,
        source_id=source_id,
        snapshot_revision=revision,
        role=role,
        manifest=manifest,
        status=DefinitionStatus.VALID,
    )
    capability_facts = tuple(
        CapabilityFact(
            capability=capability,
            available=True,
            source_id="verify-synthetic",
            snapshot_revision=revision,
        )
        for capability in manifest.required_capabilities
    )
    references: list[ContextReference] = []
    for selector in manifest.context_selectors:
        content_digest = hashlib.sha256(
            f"verify-context:{selector.kind.value}:{selector.identifier or '*'}".encode()
        ).hexdigest()
        references.append(
            ContextReference(
                kind=selector.kind,
                identifier=selector.identifier or "verify-synthetic",
                provenance="verify-synthetic",
                work_order_scope=None,
                content_digest=content_digest,
                content_revision=revision,
                snapshot_revision=revision,
                fresh_until=datetime.now(UTC) + timedelta(hours=1) if selector.requires_freshness else None,
                sensitivity=Sensitivity.PUBLIC,
                byte_size=1,
                readable=True,
            )
        )
    return (definition,), capability_facts, tuple(references), work_order


def _resolution_conditions(manifest: PackManifest) -> list[Condition]:
    revision = "verify-snapshot"
    conditions: list[Condition] = []
    clock = datetime.now(UTC)
    for role_manifest in manifest.roles:
        definitions, facts, catalog, work_order = _synthetic_resolution_inputs(
            role_manifest, revision=revision, source_id=f"plugin/{manifest.id}/{role_manifest.role.id}.json"
        )
        outcome = resolve_role(
            role=role_manifest.role,
            definitions=definitions,
            capability_facts=facts,
            context_catalog=catalog,
            work_order=work_order,
            clock=clock,
            snapshot_revision=revision,
            bundle_bounds=ContextBundleBounds(max_references=32, max_bytes=10_000_000),
        )
        if isinstance(outcome, RoleResolutionBlocked):
            structural = outcome.reason.value in {"invalid_manifest", "invalid_overlay", "not_found"}
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"resolve:{role_manifest.role.id}",
                    checked=True,
                    result=EvidenceResult.FAILED if structural else EvidenceResult.UNKNOWN,
                    detail=f"{role_manifest.role.id} blocked at {outcome.reason.value}: {outcome.detail}",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"resolve:{role_manifest.role.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"{role_manifest.role.id} manifest={outcome.manifest_digest[:12]}",
                )
            )
    return conditions


def _declared_source_conditions(manifest: PackManifest, base: Path) -> list[Condition]:
    # Each declared workflow, skill, and asset source must resolve to an existing
    # entry beneath the pack directory, so a pack that references a missing or
    # escaping file cannot verify (and then activate) clean. Evaluator command
    # runnability is checked separately by _evaluator_conditions.
    conditions: list[Condition] = []
    declared: list[tuple[str, str, str]] = []
    declared += [("workflow", w.id, w.source) for w in manifest.workflows]
    declared += [("skill", s.id, s.source) for s in manifest.skills]
    declared += [("agent", a.id, a.source) for a in manifest.agents]
    declared += [("asset", a.id, a.source) for a in manifest.assets]
    base_resolved = base.resolve()
    for kind, ident, source in declared:
        try:
            resolved = (base / source).resolve()
            contained = resolved != base_resolved and base_resolved in resolved.parents
            exists = contained and resolved.exists()
        except (ValueError, RuntimeError, OSError):
            exists = False
        if exists:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"source:{kind}:{ident}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"{source} present",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"source:{kind}:{ident}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=f"{source} missing or escapes the pack directory",
                )
            )
    return conditions


# A packaged agent's tool list is bounded: no ceiling grants a network, shell,
# or delegation tool, so Bash, Task, WebSearch, WebFetch, and any mcp__ tool are
# refused at every authority ceiling. The set is uniform today; the bound role's
# ceiling is the hook a future, tighter per-ceiling policy slots into.
_AGENT_TOOL_ALLOWLIST = frozenset({"Read", "Write", "Edit", "Glob", "Grep"})


def _read_agent_frontmatter(source: Path) -> dict[str, Any] | None:
    """Read an agent source's YAML frontmatter, or None if it cannot be read."""
    try:
        text = source.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None
    if not text.startswith("---"):
        return {}
    lines = text.splitlines()
    end = next((i for i in range(1, len(lines)) if lines[i].strip() == "---"), None)
    if end is None:
        return {}
    try:
        loaded = yaml.safe_load("\n".join(lines[1:end]))
    except yaml.YAMLError:
        return None
    return loaded if isinstance(loaded, dict) else {}


def _agent_binding_conditions(manifest: PackManifest, base: Path) -> list[Condition]:
    # Two SCHEMA conditions per declared agent. ``agent-role-binding`` fails when
    # ``role`` names a role this pack does not declare; ``agent-tools-bounded``
    # parses the copied source frontmatter and fails when its ``tools`` hold
    # anything outside the bounded allowlist. Frontmatter is copied verbatim at
    # bundle time, so this check is the only thing binding the agent's real tool
    # list to its role.
    conditions: list[Condition] = []
    declared_roles = {role.role for role in manifest.roles}
    for agent in manifest.agents:
        if agent.role is None:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-role-binding:{agent.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail="no role binding declared",
                )
            )
        elif agent.role in declared_roles:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-role-binding:{agent.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"role {agent.role.id} declared by this pack",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-role-binding:{agent.id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=f"role {agent.role.id} is not declared by this pack",
                )
            )

        # Containment before read: an agent source that escapes the pack
        # directory is already failed by source:agent, and verification must not
        # read (or hang on) an arbitrary external path to inspect its tools.
        agent_resolved = (base / agent.source).resolve()
        base_resolved = base.resolve()
        contained = agent_resolved == base_resolved or base_resolved in agent_resolved.parents
        if not contained:
            for cond_name in (
                f"agent-tools-bounded:{agent.id}",
                f"agent-frontmatter-identity:{agent.id}",
            ):
                conditions.append(
                    Condition(
                        ConditionFamily.SCHEMA,
                        cond_name,
                        checked=True,
                        result=EvidenceResult.BLOCKED,
                        detail=f"{agent.source} escapes the pack directory; not read",
                    )
                )
            continue
        frontmatter = _read_agent_frontmatter(agent_resolved)
        if frontmatter is None:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-tools-bounded:{agent.id}",
                    checked=True,
                    result=EvidenceResult.BLOCKED,
                    detail=f"cannot read {agent.source} to check tools",
                )
            )
            continue
        # The copied frontmatter's identity fields must match the declaration
        # and the pack, so a different bounded agent cannot pass role binding and
        # be bundled verbatim under another agent's id.
        fm_name = str(frontmatter.get("name"))
        fm_pack = str(frontmatter.get("pack"))
        fm_role = frontmatter.get("role")
        role_ok = agent.role is None or str(fm_role) == agent.role.id
        if fm_name == agent.id and fm_pack == manifest.id and role_ok:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-frontmatter-identity:{agent.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"frontmatter name/pack/role match the declaration",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-frontmatter-identity:{agent.id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=(
                        f"frontmatter identity mismatch: name={fm_name!r} pack={fm_pack!r} "
                        f"role={fm_role!r} vs declaration id={agent.id!r} pack={manifest.id!r} "
                        f"role={agent.role.id if agent.role else None!r}"
                    ),
                )
            )
        if "tools" not in frontmatter:
            # An omitted tools field inherits the harness default tool set
            # (shell, web, the lot), so it is unbounded, not vacuously bounded.
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-tools-bounded:{agent.id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail="no tools frontmatter; the agent would inherit the harness default tool set",
                )
            )
            continue
        raw_tools = frontmatter["tools"]
        if not raw_tools:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-tools-bounded:{agent.id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail="empty tools list; the agent would inherit the harness default tool set",
                )
            )
            continue
        tool_list = raw_tools if isinstance(raw_tools, list) else [raw_tools]
        offenders = sorted({str(t) for t in tool_list if str(t) not in _AGENT_TOOL_ALLOWLIST})
        if offenders:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-tools-bounded:{agent.id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=f"tools outside the bounded allowlist: {', '.join(offenders)}",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"agent-tools-bounded:{agent.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"all {len(tool_list)} tool(s) within the bounded allowlist",
                )
            )
    return conditions


def _footnote_version() -> str | None:
    try:
        from importlib.metadata import version

        return version("fno")
    except Exception:
        return None


def _compatibility_conditions(manifest: PackManifest) -> list[Condition]:
    conditions: list[Condition] = []
    running = _footnote_version()
    if running is None:
        conditions.append(
            Condition(
                ConditionFamily.COMPATIBILITY,
                "footnote-compat-range",
                checked=False,
                result=EvidenceResult.UNKNOWN,
                detail="running footnote version unavailable; range not evaluated",
            )
        )
    else:
        try:
            ok = _in_range(
                running, manifest.footnote_compat.minimum, manifest.footnote_compat.maximum
            )
            detail = f"running footnote {running} within declared range"
            result = EvidenceResult.PASSED if ok else EvidenceResult.FAILED
            if not ok:
                detail = (
                    f"running footnote {running} outside "
                    f"{manifest.footnote_compat.minimum}..{manifest.footnote_compat.maximum or 'open'}"
                )
        except ValueError as exc:
            result = EvidenceResult.FAILED
            detail = f"declared compat range is not parseable: {exc}"
        conditions.append(
            Condition(
                ConditionFamily.COMPATIBILITY,
                "footnote-compat-range",
                checked=True,
                result=result,
                detail=detail,
            )
        )
    conditions.append(
        Condition(
            ConditionFamily.COMPATIBILITY,
            "role-manifest-schema",
            checked=True,
            result=EvidenceResult.PASSED,
            detail=f"{len(manifest.roles)} role(s) parsed as RoleManifest",
        ),
    )
    conditions.extend(_topology_conditions(manifest))
    conditions.extend(_resolution_conditions(manifest))
    return conditions


_INTERPRETERS = frozenset({"bash", "sh", "zsh", "python", "python3", "ruby", "node", "perl"})
_SCRIPT_SUFFIXES = frozenset((".sh", ".py", ".rb", ".js", ".pl"))


def _token_resolves(token: str, base: Path) -> bool:
    if "/" in token or "\\" in token:
        candidate = base / token
        return candidate.is_file() and os.access(candidate, os.X_OK)
    return shutil.which(token) is not None


def _command_is_runnable(command: str, base: Path) -> bool:
    """Static runnability check that never executes the command.

    Verification is pure (it never spawns), so this confirms the command is
    structurally runnable: the runner resolves on PATH or as an executable
    repo-relative path, and when an interpreter names a script file, that script
    exists relative to the pack. The interpreter is recognized by basename so an
    absolute path like ``/bin/bash`` still triggers the script-existence check.
    """
    parts = command.split()
    if not parts:
        return False
    runner = parts[0]
    if not _token_resolves(runner, base):
        return False
    if runner.rsplit("/", 1)[-1] in _INTERPRETERS:
        script = next((p for p in parts[1:] if not p.startswith("-")), None)
        if script is not None and ("/" in script or script.endswith(tuple(_SCRIPT_SUFFIXES))):
            if not (base / script).is_file():
                return False
    return True


def _declared_test_conditions(manifest: PackManifest, base: Path) -> list[Condition]:
    conditions: list[Condition] = []
    for scenario in manifest.scenarios:
        if _command_is_runnable(scenario.command, base):
            conditions.append(
                Condition(
                    ConditionFamily.DECLARED_TEST,
                    f"scenario:{scenario.id}",
                    checked=True,
                    result=scenario.recorded_result,
                    detail=f"{scenario.id} command runnable; recorded={scenario.recorded_result.value}",
                )
            )
        else:
            token = scenario.command.split()[0] if scenario.command.split() else ""
            conditions.append(
                Condition(
                    ConditionFamily.DECLARED_TEST,
                    f"scenario:{scenario.id}",
                    checked=True,
                    result=EvidenceResult.BLOCKED,
                    detail=f"{scenario.id} command {token!r} is absent or not executable",
                )
            )
    if not manifest.scenarios:
        conditions.append(
            Condition(
                ConditionFamily.DECLARED_TEST,
                "no-declared-scenarios",
                checked=True,
                result=EvidenceResult.PASSED,
                detail="pack declares no benchmark scenarios; nothing to fail",
            )
        )
    return conditions


def _evaluator_conditions(manifest: PackManifest, base: Path) -> list[Condition]:
    # Evaluator commands are required evidence producers, so a declared command
    # that does not resolve on PATH or as an executable pack-relative path blocks
    # verification - closing the gap the substrate's own comment named, where a
    # pack reported green while its evidence producers were vaporware. Reuses the
    # same static _command_is_runnable check the declared-test family does.
    conditions: list[Condition] = []
    for evaluator in manifest.evaluators:
        if _command_is_runnable(evaluator.command, base):
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"evaluator-runnable:{evaluator.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"{evaluator.id} command runnable: {evaluator.command}",
                )
            )
        else:
            token = evaluator.command.split()[0] if evaluator.command.split() else ""
            conditions.append(
                Condition(
                    ConditionFamily.SCHEMA,
                    f"evaluator-runnable:{evaluator.id}",
                    checked=True,
                    result=EvidenceResult.BLOCKED,
                    detail=f"{evaluator.id} command {token!r} is absent or not executable",
                )
            )
    if not manifest.evaluators:
        conditions.append(
            Condition(
                ConditionFamily.SCHEMA,
                "no-declared-evaluators",
                checked=True,
                result=EvidenceResult.PASSED,
                detail="pack declares no evaluators; nothing to fail",
            )
        )
    return conditions


def verify_manifest(
    manifest: PackManifest,
    *,
    installed: Mapping[str, str] | None = None,
    base: Path | str,
) -> VerificationReport:
    """Verify an already-parsed manifest against an installed-pack index.

    Takes the parsed manifest directly so a caller that already loaded it (notably
    activation) verifies the exact object it will project, with no second read of
    the file in between. ``base`` is the pack directory used to resolve declared
    source files and scenario commands.
    """
    base_path = Path(base)
    conditions: list[Condition] = []
    conditions.extend(_identity_conditions(manifest, str(base_path)))
    conditions.extend(_schema_conditions(manifest))
    conditions.extend(_declared_source_conditions(manifest, base_path))
    conditions.extend(_agent_binding_conditions(manifest, base_path))
    conditions.extend(_dependency_conditions(manifest, installed))
    conditions.extend(_capability_conditions(manifest))
    conditions.extend(_compatibility_conditions(manifest))
    conditions.extend(_declared_test_conditions(manifest, base_path))
    conditions.extend(_evaluator_conditions(manifest, base_path))
    return VerificationReport(pack_path=str(base_path), conditions=tuple(conditions))


def verify_pack(
    path: Path | str,
    *,
    installed: Mapping[str, str] | None = None,
) -> VerificationReport:
    """Verify a pack at ``path`` (directory or plugin.yaml) and return a report.

    ``installed`` maps an installed pack id to its resolved version; when it is
    None the dependency family reports unchecked/unknown rather than guessing.
    """
    target = Path(path).expanduser()
    manifest, load_failure = load_manifest(target)
    if load_failure is not None:
        return VerificationReport(pack_path=str(target), conditions=(load_failure,))
    assert manifest is not None
    return verify_manifest(manifest, installed=installed, base=target.parent if target.is_file() else target)
