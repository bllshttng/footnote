from __future__ import annotations

import hashlib
from datetime import UTC, datetime
from pathlib import Path

import pytest

from fno.company.contracts import FunctionRef, RoleRef, WorkOrderRef
from fno.config import ArtifactConfig
from fno.roles import (
    AuthorityCeiling,
    ContextBundleBounds,
    ContextKind,
    ContextSelector,
    DefinitionStatus,
    DeliveryPolicy,
    ReviewPolicy,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoleResolutionBlocked,
    RoleResolutionReason,
    Sensitivity,
    resolve_role,
)
from fno.roles.context import build_artifact_catalog, catalog_revision

NOW = datetime(2026, 8, 4, 12, tzinfo=UTC)
REVISION = "snapshot-edf5"
SENTINEL = lambda path: hashlib.sha256(  # noqa: E731
    f"fno/context/unavailable/{path}".encode()
).hexdigest()


# --------------------------------------------------------------------------- #
# AC18-OBS: the catalog is an honest observer
# --------------------------------------------------------------------------- #


def test_readable_file_carries_real_digest_and_size(tmp_path: Path) -> None:
    product = tmp_path / "product.md"
    payload = b"# Footnote\n\nships delivery graphs.\n"
    product.write_bytes(payload)

    references = build_artifact_catalog(
        {"product-truth": {"path": str(product), "sensitivity": "public"}},
        snapshot_revision=REVISION,
        clock=NOW,
    )

    assert len(references) == 1
    ref = references[0]
    assert ref.identifier == "product-truth"
    assert ref.readable is True
    assert ref.unavailable_reason is None
    assert ref.content_digest == hashlib.sha256(payload).hexdigest()
    assert ref.byte_size == len(payload)
    assert ref.sensitivity is Sensitivity.PUBLIC
    assert ref.kind is ContextKind.ARTIFACT
    assert ref.snapshot_revision == REVISION
    assert ref.work_order_scope is None


def test_absent_file_is_unreadable_with_reason_and_no_fabricated_digest(
    tmp_path: Path,
) -> None:
    missing = tmp_path / "does-not-exist.md"

    references = build_artifact_catalog(
        {"product-truth": {"path": str(missing)}},
        snapshot_revision=REVISION,
        clock=NOW,
    )

    ref = references[0]
    assert ref.readable is False
    assert ref.unavailable_reason is not None
    assert str(missing) in ref.unavailable_reason
    assert ref.byte_size == 0
    # The digest is the path sentinel, provably NOT the file's content (the
    # file was never read): it must equal the documented sentinel formula and
    # cannot equal a content digest because there are no bytes to hash.
    assert ref.content_digest == SENTINEL(Path(str(missing)).expanduser())


def test_directory_is_unreadable(tmp_path: Path) -> None:
    references = build_artifact_catalog(
        {"product-truth": {"path": str(tmp_path)}},
        snapshot_revision=REVISION,
        clock=NOW,
    )
    assert references[0].readable is False
    assert references[0].byte_size == 0
    assert references[0].unavailable_reason is not None


def test_sensitivity_defaults_to_internal() -> None:
    # A spec with no sensitivity resolves to INTERNAL, matching ContextSelector.
    references = build_artifact_catalog(
        {"brand-voice": {"path": "/no/such/file.md"}},
        snapshot_revision=REVISION,
        clock=NOW,
    )
    assert references[0].sensitivity is Sensitivity.INTERNAL


def test_artifactconfig_model_and_plain_dict_both_supported(tmp_path: Path) -> None:
    product = tmp_path / "p.md"
    product.write_bytes(b"x")

    from_model = build_artifact_catalog(
        {"product-truth": ArtifactConfig(path=str(product), sensitivity="public")},
        snapshot_revision=REVISION,
        clock=NOW,
    )
    from_dict = build_artifact_catalog(
        {"product-truth": {"path": str(product), "sensitivity": "public"}},
        snapshot_revision=REVISION,
        clock=NOW,
    )
    assert from_model[0].content_digest == from_dict[0].content_digest
    assert from_model[0].sensitivity is from_dict[0].sensitivity is Sensitivity.PUBLIC


def test_naive_clock_is_rejected() -> None:
    with pytest.raises(ValueError, match="timezone-aware"):
        build_artifact_catalog({}, snapshot_revision=REVISION, clock=datetime(2026, 1, 1))


def test_catalog_revision_is_stable_and_config_keyed(tmp_path: Path) -> None:
    product = tmp_path / "p.md"
    product.write_bytes(b"x")
    spec = {"product-truth": {"path": str(product), "sensitivity": "public"}}
    # Stable across runs (derived from config, not file contents).
    assert catalog_revision(spec) == catalog_revision(spec)
    other = {"product-truth": {"path": str(product), "sensitivity": "internal"}}
    assert catalog_revision(spec) != catalog_revision(other)


# --------------------------------------------------------------------------- #
# AC16-CTX: a project-supplied artifact drives resolution; no fallback to a
# pack asset. The catalog's contribution is the resolved reference's
# provenance; the factual-check script itself is exercised in 2.3 / 6.1.
# --------------------------------------------------------------------------- #


def _artifact_manifest(
    *,
    selector: ContextSelector,
    role: RoleRef,
) -> RoleManifest:
    return RoleManifest(
        role=role,
        function=FunctionRef(id=role.function_id),
        mission="Draft grounded in verified product truth.",
        deliverable_kinds=("campaign-plan",),
        delegation_targets=(),
        required_capabilities=(),
        authority_ceiling=AuthorityCeiling.INTERNAL,
        context_selectors=(selector,),
        review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
        delivery_policy=DeliveryPolicy(required_evidence=("factual-review",)),
        default_topology="pipeline",
    )


def _plugin_definition(manifest: RoleManifest) -> RoleDefinitionSource:
    return RoleDefinitionSource(
        layer=RoleLayer.PLUGIN,
        source_id=f"plugin/growth-studio/{manifest.role.id}.json",
        snapshot_revision=REVISION,
        role=manifest.role,
        manifest=manifest,
        status=DefinitionStatus.VALID,
    )


def test_project_artifact_resolves_against_project_file_not_pack_asset(
    tmp_path: Path,
) -> None:
    role = RoleRef(id="marketing", function_id="growth-studio")
    selector = ContextSelector(
        kind=ContextKind.ARTIFACT,
        identifier="product-truth",
        max_sensitivity=Sensitivity.PUBLIC,
    )
    definition = _plugin_definition(_artifact_manifest(selector=selector, role=role))
    work_order = WorkOrderRef(node_id="x-edf5", attempt_id="att-1", role_id=role.id)

    project_truth = tmp_path / "PRODUCT.md"
    project_truth.write_bytes(b"# Project truth\n\nthe project's facts.\n")
    pack_asset = tmp_path / "pack-asset.md"
    pack_asset.write_bytes(b"# Pack asset\n\nthe pack's facts.\n")

    catalog = build_artifact_catalog(
        {"product-truth": {"path": str(project_truth), "sensitivity": "public"}},
        snapshot_revision=REVISION,
        clock=NOW,
    )

    result = resolve_role(
        role=role,
        definitions=(definition,),
        capability_facts=(),
        context_catalog=catalog,
        work_order=work_order,
        clock=NOW,
        snapshot_revision=REVISION,
        bundle_bounds=ContextBundleBounds(max_references=32, max_bytes=10_000_000),
    )

    assert not isinstance(result, RoleResolutionBlocked)
    (reference,) = result.context_bundle.references
    # The resolved provenance is the PROJECT file, never the pack asset path.
    assert reference.provenance == str(project_truth)
    assert reference.provenance != str(pack_asset)


def test_missing_artifact_blocks_with_missing_context_not_fallback(
    tmp_path: Path,
) -> None:
    role = RoleRef(id="marketing", function_id="growth-studio")
    selector = ContextSelector(
        kind=ContextKind.ARTIFACT,
        identifier="product-truth",
        max_sensitivity=Sensitivity.PUBLIC,
    )
    definition = _plugin_definition(_artifact_manifest(selector=selector, role=role))
    work_order = WorkOrderRef(node_id="x-edf5", attempt_id="att-1", role_id=role.id)

    # Empty catalog: no artifact supplied. Resolution must block rather than
    # silently read any pack asset.
    result = resolve_role(
        role=role,
        definitions=(definition,),
        capability_facts=(),
        context_catalog=(),
        work_order=work_order,
        clock=NOW,
        snapshot_revision=REVISION,
        bundle_bounds=ContextBundleBounds(max_references=32, max_bytes=10_000_000),
    )

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason is RoleResolutionReason.MISSING_CONTEXT


def test_configured_but_unreadable_artifact_blocks_unreadable(tmp_path: Path) -> None:
    role = RoleRef(id="marketing", function_id="growth-studio")
    selector = ContextSelector(
        kind=ContextKind.ARTIFACT,
        identifier="product-truth",
        max_sensitivity=Sensitivity.PUBLIC,
    )
    definition = _plugin_definition(_artifact_manifest(selector=selector, role=role))
    work_order = WorkOrderRef(node_id="x-edf5", attempt_id="att-1", role_id=role.id)

    catalog = build_artifact_catalog(
        {"product-truth": {"path": str(tmp_path / "absent.md")}},
        snapshot_revision=REVISION,
        clock=NOW,
    )

    result = resolve_role(
        role=role,
        definitions=(definition,),
        capability_facts=(),
        context_catalog=catalog,
        work_order=work_order,
        clock=NOW,
        snapshot_revision=REVISION,
        bundle_bounds=ContextBundleBounds(max_references=32, max_bytes=10_000_000),
    )

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason is RoleResolutionReason.UNREADABLE_CONTEXT
