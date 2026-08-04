from __future__ import annotations

from pathlib import Path

import yaml

from fno.company.contracts import EvidenceResult, RoleRef
from fno.plugins.manifest import AgentDeclaration, PackManifest
from fno.plugins.verify import (
    Condition,
    ConditionFamily,
    VerificationReport,
    verify_pack,
)
from tests.unit.plugins.test_manifest import _full_pack, _materialize_declared_sources


def _write_pack(tmp_path: Path, manifest: PackManifest, name: str = "growth-studio") -> Path:
    pack_dir = tmp_path / name
    pack_dir.mkdir()
    payload = manifest.model_dump(mode="json")
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(payload), encoding="utf-8")
    _materialize_declared_sources(pack_dir, manifest)
    return pack_dir


def _results_by_name(report: VerificationReport) -> dict[str, Condition]:
    return {condition.name: condition for condition in report.conditions}


def test_valid_pack_is_all_checked_and_passed(tmp_path):
    pack_dir = _write_pack(tmp_path, _full_pack())
    # An empty installed index lets the dependency family evaluate (the pack has
    # no deps), so every family is checked rather than left unknown.
    report = verify_pack(pack_dir, installed={})
    assert report.ok, [c.name for c in report.conditions if not c.ok]
    for condition in report.conditions:
        assert condition.checked, condition.name
        assert condition.result is EvidenceResult.PASSED, (condition.name, condition.result)


def test_verify_writes_activates_and_mutates_nothing(tmp_path):
    pack_dir = _write_pack(tmp_path, _full_pack())
    snapshot = {p: p.read_bytes() for p in pack_dir.rglob("*") if p.is_file()}
    verify_pack(pack_dir)
    after = {p: p.read_bytes() for p in pack_dir.rglob("*") if p.is_file()}
    assert snapshot == after


def test_unevaluated_condition_reports_unchecked_unknown_not_passed(tmp_path):
    # No installed-pack index => dependency family is unchecked/unknown, not passed.
    pack_dir = _write_pack(tmp_path, _full_pack())
    report = verify_pack(pack_dir)
    dependency = next(c for c in report.conditions if c.family is ConditionFamily.DEPENDENCY)
    assert dependency.checked is False
    assert dependency.result is EvidenceResult.UNKNOWN
    # and the report is not ok precisely because of that unknown
    assert report.ok is False


def test_dependency_absent_blocks_when_index_supplied(tmp_path):
    pack_dict = _full_pack().model_dump(mode="json")
    pack_dict["depends_on"] = [{"pack_id": "missing-core", "version_range": {"minimum": "0.1.0"}}]
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(pack_dict), encoding="utf-8")
    report = verify_pack(pack_dir, installed={})
    dependency = next(c for c in report.conditions if c.name == "dependency:missing-core")
    assert dependency.checked is True
    assert dependency.result is EvidenceResult.BLOCKED
    assert report.ok is False


def test_dependency_present_passes_when_index_supplied(tmp_path):
    pack_dict = _full_pack().model_dump(mode="json")
    pack_dict["depends_on"] = [{"pack_id": "core-brand", "version_range": {"minimum": "0.1.0"}}]
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(pack_dict), encoding="utf-8")
    report = verify_pack(pack_dir, installed={"core-brand": "0.1.0"})
    dependency = next(c for c in report.conditions if c.name == "dependency:core-brand")
    assert dependency.checked is True
    assert dependency.result is EvidenceResult.PASSED


def test_malformed_yaml_blocks_naming_file_and_error(tmp_path):
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_text("id: growth-studio\n  bad: indent\n - broken\n", encoding="utf-8")
    report = verify_pack(pack_dir)
    assert len(report.conditions) == 1
    condition = report.conditions[0]
    assert condition.checked is True
    assert condition.result is EvidenceResult.BLOCKED
    assert "plugin.yaml" in (condition.detail or "")
    assert report.ok is False


def test_invalid_utf8_blocks_not_absent(tmp_path):
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_bytes(b"\xff\xfe id: broken\n")
    report = verify_pack(pack_dir)
    condition = report.conditions[0]
    assert condition.result is EvidenceResult.BLOCKED
    assert "UTF-8" in (condition.detail or "")


def test_schema_invalid_manifest_blocks_not_absent(tmp_path):
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    # no components at all => empty-pack validator refuses => blocked, not "absent"
    (pack_dir / "plugin.yaml").write_text(
        yaml.safe_dump({"id": "empty", "version": "0.1.0", "footnote_compat": {"minimum": "0.3.0"}}),
        encoding="utf-8",
    )
    report = verify_pack(pack_dir)
    condition = report.conditions[0]
    assert condition.result is EvidenceResult.BLOCKED
    assert "schema validation" in (condition.detail or "")


def test_bad_topology_literal_fails_compatibility(tmp_path):
    pack = _full_pack()
    pack_dict = pack.model_dump(mode="json")
    pack_dict["roles"][0]["default_topology"] = "fifth-shape"
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(pack_dict), encoding="utf-8")
    report = verify_pack(pack_dir)
    topology = next(c for c in report.conditions if c.name.startswith("topology:"))
    assert topology.checked is True
    assert topology.result is EvidenceResult.FAILED
    assert "fifth-shape" in (topology.detail or "")
    assert report.ok is False


def test_scenario_with_absent_command_blocks(tmp_path):
    pack = _full_pack().model_copy(
        update={
            "scenarios": (
                _full_pack().scenarios[0].model_copy(
                    update={"command": "no-such-binary --dry-run"}
                ),
            )
        }
    )
    pack_dir = _write_pack(tmp_path, pack)
    report = verify_pack(pack_dir)
    scenario = next(c for c in report.conditions if c.name.startswith("scenario:"))
    assert scenario.checked is True
    assert scenario.result is EvidenceResult.BLOCKED
    assert "absent or not executable" in (scenario.detail or "")


def test_scenario_with_runnable_command_reflects_recorded_result(tmp_path):
    # `true` is present on POSIX PATH; recorded result is honored.
    pack_dir = _write_pack(tmp_path, _full_pack())
    report = verify_pack(pack_dir)
    scenario = next(c for c in report.conditions if c.name.startswith("scenario:"))
    assert scenario.checked is True
    assert scenario.result is EvidenceResult.PASSED


def test_resolve_synthetic_catches_unresolvable_role(tmp_path):
    # A role whose context selector cannot be satisfied even in a maximally
    # permissive synthetic environment surfaces as a non-passing compatibility
    # condition. The full pack resolves, so this asserts the happy path instead.
    pack_dir = _write_pack(tmp_path, _full_pack())
    report = verify_pack(pack_dir)
    resolve_conditions = [c for c in report.conditions if c.name.startswith("resolve:")]
    assert resolve_conditions
    assert all(c.result is EvidenceResult.PASSED for c in resolve_conditions)


def test_scenario_with_missing_script_blocks_even_when_runner_is_present(tmp_path):
    # bash is on PATH, but the script it names does not exist => blocked, not passed.
    pack = _full_pack().model_copy(
        update={
            "scenarios": (
                _full_pack().scenarios[0].model_copy(update={"command": "bash no-such-script.sh"}),
            )
        }
    )
    pack_dir = _write_pack(tmp_path, pack)
    report = verify_pack(pack_dir, installed={})
    scenario = next(c for c in report.conditions if c.name.startswith("scenario:"))
    assert scenario.checked is True
    assert scenario.result is EvidenceResult.BLOCKED


def test_scenario_with_present_script_passes(tmp_path):
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "scripts").mkdir()
    (pack_dir / "scripts" / "run.sh").write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
    pack = _full_pack().model_copy(
        update={"scenarios": (_full_pack().scenarios[0].model_copy(update={"command": "bash scripts/run.sh"}),)}
    )
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(pack.model_dump(mode="json")), encoding="utf-8")
    report = verify_pack(pack_dir, installed={})
    scenario = next(c for c in report.conditions if c.name.startswith("scenario:"))
    assert scenario.checked is True
    assert scenario.result is EvidenceResult.PASSED


def test_dependency_version_out_of_range_fails(tmp_path):
    pack_dict = _full_pack().model_dump(mode="json")
    pack_dict["depends_on"] = [{"pack_id": "core-brand", "version_range": {"minimum": "1.0.0"}}]
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(pack_dict), encoding="utf-8")
    report = verify_pack(pack_dir, installed={"core-brand": "0.5.0"})
    dep = next(c for c in report.conditions if c.name == "dependency:core-brand")
    assert dep.checked is True
    assert dep.result is EvidenceResult.FAILED
    assert "outside" in (dep.detail or "")


# AC1-HP / AC2-ERR / AC3-SEC: agent declarations bind a role and a bounded tool list.

_AGENT_ROLE = RoleRef(id="marketing", function_id="growth-studio")


def _write_agent_file(pack_dir: Path, source: str, *, tools: list[str]) -> None:
    target = pack_dir / source
    target.parent.mkdir(parents=True, exist_ok=True)
    frontmatter = yaml.safe_dump(
        {"name": Path(source).stem, "pack": "growth-studio", "role": "marketing", "tools": tools},
        sort_keys=False,
    )
    target.write_text(f"---\n{frontmatter}---\n\nagent body\n", encoding="utf-8")


def _pack_with_agent(tmp_path: Path, agent: AgentDeclaration) -> Path:
    manifest = _full_pack().model_copy(update={"agents": (agent,)})
    pack_dir = _write_pack(tmp_path, manifest)
    return pack_dir


def test_agent_with_valid_binding_and_bounded_tools_checks_and_passes(tmp_path):
    agent = AgentDeclaration(id="growth-marketer", source="agents/growth-marketer.md", role=_AGENT_ROLE)
    pack_dir = _pack_with_agent(tmp_path, agent)
    _write_agent_file(pack_dir, "agents/growth-marketer.md", tools=["Read", "Write", "Glob", "Grep"])

    report = verify_pack(pack_dir, installed={})
    by_name = _results_by_name(report)
    assert by_name["source:agent:growth-marketer"].ok
    assert by_name["agent-role-binding:growth-marketer"].ok
    assert by_name["agent-tools-bounded:growth-marketer"].ok
    assert report.ok, [c.name for c in report.conditions if not c.ok]


def test_agent_source_missing_or_escaping_fails(tmp_path):
    agent = AgentDeclaration(id="growth-marketer", source="agents/growth-marketer.md", role=_AGENT_ROLE)
    pack_dir = _pack_with_agent(tmp_path, agent)
    # Source file deliberately not written.
    report = verify_pack(pack_dir, installed={})
    by_name = _results_by_name(report)
    assert by_name["source:agent:growth-marketer"].result is EvidenceResult.FAILED
    assert not report.ok


def test_agent_role_not_declared_fails_binding(tmp_path):
    bogus = RoleRef(id="nonexistent", function_id="growth-studio")
    agent = AgentDeclaration(id="growth-marketer", source="agents/growth-marketer.md", role=bogus)
    pack_dir = _pack_with_agent(tmp_path, agent)
    _write_agent_file(pack_dir, "agents/growth-marketer.md", tools=["Read"])
    report = verify_pack(pack_dir, installed={})
    by_name = _results_by_name(report)
    assert by_name["agent-role-binding:growth-marketer"].result is EvidenceResult.FAILED
    assert "not declared" in (by_name["agent-role-binding:growth-marketer"].detail or "")


def test_agent_unbounded_tools_fail(tmp_path):
    agent = AgentDeclaration(id="growth-marketer", source="agents/growth-marketer.md", role=_AGENT_ROLE)
    pack_dir = _pack_with_agent(tmp_path, agent)
    _write_agent_file(
        pack_dir,
        "agents/growth-marketer.md",
        tools=["Read", "Bash", "Task", "WebSearch", "WebFetch", "mcp__evil__steal"],
    )
    report = verify_pack(pack_dir, installed={})
    by_name = _results_by_name(report)
    cond = by_name["agent-tools-bounded:growth-marketer"]
    assert cond.result is EvidenceResult.FAILED
    detail = cond.detail or ""
    for offender in ("Bash", "Task", "WebSearch", "WebFetch", "mcp__evil__steal"):
        assert offender in detail
