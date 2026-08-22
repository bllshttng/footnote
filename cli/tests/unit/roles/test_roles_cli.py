from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from fno.company.contracts import FunctionRef, RoleRef
from fno.roles import (
    AuthorityCeiling,
    DefinitionStatus,
    DeliveryPolicy,
    ReviewPolicy,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
)


runner = CliRunner()


def _invoke(*args: str):
    from fno.cli import app

    return runner.invoke(app, list(args))


def _manifest(role_id: str, function_id: str) -> RoleManifest:
    role = RoleRef(id=role_id, function_id=function_id)
    return RoleManifest(
        role=role,
        function=FunctionRef(id=function_id),
        mission="Produce one bounded artifact.",
        deliverable_kinds=("brief",),
        authority_ceiling=AuthorityCeiling.INTERNAL,
        review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
        delivery_policy=DeliveryPolicy(required_evidence=("artifact-exists",)),
        default_topology="direct",
    )


def _write_source(
    root: Path,
    *,
    layer: RoleLayer,
    name: str,
    role_id: str,
    function_id: str,
    status: DefinitionStatus = DefinitionStatus.VALID,
    error: str | None = None,
) -> Path:
    path = root / layer.value / f"{name}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    role = RoleRef(id=role_id, function_id=function_id)
    source = RoleDefinitionSource(
        layer=layer,
        source_id=f"{layer.value}/{name}.json",
        snapshot_revision="snapshot-1",
        role=role,
        manifest=_manifest(role_id, function_id) if status is DefinitionStatus.VALID else None,
        status=status,
        error=error,
    )
    path.write_text(json.dumps(source.model_dump(mode="json")), encoding="utf-8")
    return path


def _snapshot_tree(root: Path) -> dict[str, bytes | None]:
    return {
        str(path.relative_to(root)): path.read_bytes() if path.is_file() else None
        for path in sorted(root.rglob("*"))
    }


def _assert_pretty_sorted_json(output: str) -> Any:
    parsed = json.loads(output)
    assert output == json.dumps(parsed, indent=2, sort_keys=True) + "\n"
    return parsed


def test_ac_r7_ui_ls_and_show_are_stable_and_keep_invalid_sources_visible(
    tmp_path: Path,
    monkeypatch,
) -> None:
    roles_root = tmp_path / "roles"
    _write_source(
        roles_root,
        layer=RoleLayer.BUILT_IN,
        name="owner",
        role_id="owner",
        function_id="marketing",
    )
    invalid = roles_root / RoleLayer.PROJECT.value / "broken.json"
    invalid.parent.mkdir(parents=True)
    invalid.write_text(
        json.dumps(
            {
                "layer": "project",
                "source_id": "project/broken.json",
                "snapshot_revision": "snapshot-1",
                "role": {"id": "broken", "function_id": "support"},
                "manifest": {"mission": "missing required manifest fields"},
                "status": "valid",
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(roles_root))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")
    before_roles = _snapshot_tree(roles_root)

    first = _invoke("agents", "roles", "ls")
    second = _invoke("agents", "roles", "ls")
    assert first.exit_code == second.exit_code == 0
    assert first.output == second.output
    assert first.output == (
        "owner function=marketing layer=built-in "
        "source=built-in/owner.json status=valid\n"
        "broken function=support layer=project "
        "source=project/broken.json status=invalid\n"
    )

    local_json = _invoke("agents", "roles", "ls", "-J")
    root_json = _invoke("-J", "agents", "roles", "ls")
    assert local_json.exit_code == root_json.exit_code == 0
    assert local_json.output == root_json.output
    rows = _assert_pretty_sorted_json(local_json.output)
    assert [(row["role_id"], row["status"]) for row in rows] == [
        ("owner", "valid"),
        ("broken", "invalid"),
    ]

    shown = _invoke("agents", "roles", "show", "broken", "-J")
    assert shown.exit_code == 0
    definitions = _assert_pretty_sorted_json(shown.output)
    assert len(definitions) == 1
    assert definitions[0]["layer"] == "project"
    assert definitions[0]["source"] == "project/broken.json"
    assert definitions[0]["status"] == "invalid"
    assert definitions[0]["disposition"] == "unchecked"
    assert definitions[0]["error"]
    assert definitions[0]["raw_definition"]["role"]["id"] == "broken"
    shown_text = _invoke("agents", "roles", "show", "broken")
    shown_text_again = _invoke("agents", "roles", "show", "broken")
    assert shown_text.exit_code == shown_text_again.exit_code == 0
    assert shown_text.output == shown_text_again.output
    assert "status=invalid disposition=unchecked" in shown_text.output
    assert "error:" in shown_text.output
    assert _snapshot_tree(roles_root) == before_roles


def test_ac_r7_ui_resolve_is_typed_inert_and_has_stable_exit_codes(
    tmp_path: Path,
    monkeypatch,
) -> None:
    roles_root = tmp_path / "roles"
    _write_source(
        roles_root,
        layer=RoleLayer.BUILT_IN,
        name="owner",
        role_id="owner",
        function_id="marketing",
    )
    _write_source(
        roles_root,
        layer=RoleLayer.PLAN,
        name="unavailable",
        role_id="unavailable",
        function_id="support",
        status=DefinitionStatus.UNREADABLE,
        error="source unavailable; definition unchecked",
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(roles_root))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")

    project_root = tmp_path / "redirected-project"
    graph_root = project_root / ".fno"
    claims_root = graph_root / "claims"
    (graph_root / "nested").mkdir(parents=True)
    claims_root.mkdir()
    (graph_root / "graph.json").write_bytes(b'{"entries":[]}\n')
    (graph_root / "nested" / "receipt.bin").write_bytes(b"graph-sentinel\x00")
    (claims_root / "node.lock").write_bytes(b"claim-sentinel\n")
    before_graph = _snapshot_tree(graph_root)
    before_claims = _snapshot_tree(claims_root)
    monkeypatch.setenv("FNO_REPO_ROOT", str(project_root))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(project_root))

    dispatch_calls: list[tuple[object, ...]] = []

    def _dispatch_tripwire(*args: object, **kwargs: object) -> None:
        dispatch_calls.append((*args, kwargs))
        raise AssertionError("roles inspection must not dispatch a process")

    monkeypatch.setattr(subprocess, "Popen", _dispatch_tripwire)
    monkeypatch.setattr(subprocess, "run", _dispatch_tripwire)

    args = ["resolve", "owner", "--work-order", "x-a8c0", "--attempt", "attempt-1"]
    first = _invoke("agents", "roles", *args, "-J")
    second = _invoke("agents", "roles", *args, "-J")
    root_json = _invoke("-J", "agents", "roles", *args)
    assert first.exit_code == second.exit_code == root_json.exit_code == 0
    assert first.output == second.output == root_json.output
    resolved = _assert_pretty_sorted_json(first.output)
    assert resolved["role"] == {"function_id": "marketing", "id": "owner"}
    assert resolved["work_order"] == {
        "attempt_id": "attempt-1",
        "node_id": "x-a8c0",
        "principal_id": None,
        "role_id": "owner",
    }
    assert len(resolved["manifest_digest"]) == 64
    assert len(resolved["context_bundle"]["digest"]) == 64

    blocked_result = _invoke(
        "agents",
        "roles",
        "resolve",
        "unavailable",
        "--work-order",
        "x-a8c0",
        "--attempt",
        "attempt-1",
        "-J",
    )
    assert blocked_result.exit_code == 1
    blocked = _assert_pretty_sorted_json(blocked_result.output)
    assert blocked == {
        "detail": "source unavailable; definition unchecked",
        "reason": "invalid_manifest",
        "reference": "unavailable",
        "role": {"function_id": "support", "id": "unavailable"},
        "source_id": "plan/unavailable.json",
        "source_layer": "plan",
    }

    not_found_result = _invoke(
        "agents",
        "roles",
        "resolve",
        "missing",
        "--work-order",
        "x-a8c0",
        "--attempt",
        "attempt-1",
        "-J",
    )
    assert not_found_result.exit_code == 1
    not_found = _assert_pretty_sorted_json(not_found_result.output)
    assert not_found["reason"] == "not_found"
    assert not_found["role"] == {"function_id": "unavailable", "id": "missing"}

    malformed = _invoke("agents", "roles", "resolve", "owner")
    assert malformed.exit_code == 2
    assert dispatch_calls == []
    assert _snapshot_tree(graph_root) == before_graph
    assert _snapshot_tree(claims_root) == before_claims


@pytest.mark.parametrize("failure", ["malformed", "invalid_utf8", "unreadable"])
def test_resolve_fails_closed_when_an_unchecked_source_has_no_role_identity(
    tmp_path: Path,
    monkeypatch,
    failure: str,
) -> None:
    roles_root = tmp_path / "roles"
    _write_source(
        roles_root,
        layer=RoleLayer.BUILT_IN,
        name="owner",
        role_id="owner",
        function_id="marketing",
    )
    if failure in {"malformed", "invalid_utf8"}:
        unchecked = roles_root / RoleLayer.PROJECT.value / "broken.json"
        unchecked.parent.mkdir(parents=True)
        if failure == "malformed":
            unchecked.write_text("{not-json", encoding="utf-8")
        else:
            unchecked.write_bytes(b"\xff\xfe")
        source_args: tuple[str, ...] = ()
        expected_source = "project/broken.json"
    else:
        unchecked = tmp_path / "does-not-exist.json"
        source_args = ("--source", f"project={unchecked}")
        expected_source = str(unchecked)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(roles_root))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")

    resolved = _invoke(
        "agents",
        "roles",
        *source_args,
        "resolve",
        "owner",
        "--work-order",
        "x-a8c0",
        "--attempt",
        "attempt-1",
        "-J",
    )

    assert resolved.exit_code == 1
    blocked = _assert_pretty_sorted_json(resolved.output)
    assert blocked["reason"] == "invalid_manifest"
    assert blocked["source_layer"] == "project"
    assert blocked["source_id"] == expected_source
    assert blocked["detail"]

    listed = _invoke("agents", "roles", *source_args, "ls", "-J")
    assert listed.exit_code == 0
    rows = _assert_pretty_sorted_json(listed.output)
    assert any(
        row["source"] == expected_source
        and row["role_id"] is None
        and row["status"] in {"invalid", "unreadable"}
        for row in rows
    )

    shown = _invoke("agents", "roles", *source_args, "show", "owner", "-J")
    assert shown.exit_code == 0
    shown_rows = _assert_pretty_sorted_json(shown.output)
    assert any(row["source"] == expected_source for row in shown_rows)


def test_resolve_fails_closed_when_identified_invalid_overlay_has_no_revision(
    tmp_path: Path,
    monkeypatch,
) -> None:
    roles_root = tmp_path / "roles"
    _write_source(
        roles_root,
        layer=RoleLayer.BUILT_IN,
        name="owner",
        role_id="owner",
        function_id="marketing",
    )
    invalid = roles_root / RoleLayer.PROJECT.value / "owner.json"
    invalid.parent.mkdir(parents=True)
    invalid.write_text(
        json.dumps(
            {
                "layer": "project",
                "source_id": "project/owner.json",
                "role": {"id": "owner", "function_id": "marketing"},
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(roles_root))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")

    resolved = _invoke(
        "agents",
        "roles",
        "resolve",
        "owner",
        "--work-order",
        "x-a8c0",
        "--attempt",
        "attempt-1",
        "-J",
    )

    assert resolved.exit_code == 1
    blocked = _assert_pretty_sorted_json(resolved.output)
    assert blocked["reason"] == "invalid_manifest"
    assert blocked["source_layer"] == "project"
    assert blocked["source_id"] == "project/owner.json"


@pytest.mark.parametrize("option", ["--capabilities", "--context"])
def test_resolve_rejects_invalid_utf8_auxiliary_inputs(
    tmp_path: Path,
    monkeypatch,
    option: str,
) -> None:
    roles_root = tmp_path / "roles"
    _write_source(
        roles_root,
        layer=RoleLayer.BUILT_IN,
        name="owner",
        role_id="owner",
        function_id="marketing",
    )
    invalid = tmp_path / "invalid.json"
    invalid.write_bytes(b"\xff\xfe")
    monkeypatch.setenv("FNO_ROLES_ROOT", str(roles_root))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")

    result = _invoke(
        "agents",
        "roles",
        "resolve",
        "owner",
        "--work-order",
        "x-a8c0",
        "--attempt",
        "attempt-1",
        option,
        str(invalid),
    )

    assert result.exit_code == 2
    assert "cannot validate" in result.output


def test_resolve_fails_closed_when_a_layer_directory_cannot_be_enumerated(
    tmp_path: Path,
    monkeypatch,
) -> None:
    roles_root = tmp_path / "roles"
    _write_source(
        roles_root,
        layer=RoleLayer.BUILT_IN,
        name="owner",
        role_id="owner",
        function_id="marketing",
    )
    unreadable_layer = roles_root / RoleLayer.PROJECT.value
    unreadable_layer.mkdir()
    from fno.roles import registry

    real_walk = registry.os.walk

    def deny_project_walk(directory, **kwargs):
        if Path(directory) == unreadable_layer:
            kwargs["onerror"](
                PermissionError(13, "Permission denied", str(unreadable_layer))
            )
            return iter(())
        return real_walk(directory, **kwargs)

    monkeypatch.setattr(registry.os, "walk", deny_project_walk)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(roles_root))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")

    resolved = _invoke(
        "agents",
        "roles",
        "resolve",
        "owner",
        "--work-order",
        "x-a8c0",
        "--attempt",
        "attempt-1",
        "-J",
    )

    assert resolved.exit_code == 1
    blocked = _assert_pretty_sorted_json(resolved.output)
    assert blocked["reason"] == "invalid_manifest"
    assert blocked["source_layer"] == "project"
    assert blocked["source_id"] == "project"
    assert "PermissionError" in blocked["detail"]


def test_ac_r7_ui_roles_is_hidden_lazy_and_discoverable() -> None:
    from fno.cli import LAZY_SUBCOMMANDS

    normal = _invoke("--help")
    full = _invoke("help", "--all")

    assert normal.exit_code == full.exit_code == 0
    assert "roles" not in normal.output
    assert "roles" in full.output
    assert LAZY_SUBCOMMANDS["roles"] == (
        "fno.roles.cli:roles_app",
        "Inspect bounded business-role definitions and resolutions.",
        {"hidden": True, "collapse_keep": []},
    )


def test_default_discovery_is_anchored_to_repository_root(
    tmp_path: Path,
    monkeypatch,
) -> None:
    project_root = tmp_path / "project"
    roles_root = project_root / ".fno" / "roles"
    _write_source(
        roles_root,
        layer=RoleLayer.BUILT_IN,
        name="owner",
        role_id="owner",
        function_id="marketing",
    )
    nested = project_root / "cli" / "src"
    nested.mkdir(parents=True)
    monkeypatch.chdir(nested)
    monkeypatch.delenv("FNO_ROLES_ROOT", raising=False)
    monkeypatch.setattr("fno.roles.registry.resolve_repo_root", lambda: project_root)

    listed = _invoke("agents", "roles", "ls", "-J")

    assert listed.exit_code == 0
    rows = _assert_pretty_sorted_json(listed.output)
    assert rows[0]["role_id"] == "owner"
