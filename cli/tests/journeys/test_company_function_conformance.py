from __future__ import annotations

import ast
import datetime as dt
import os
import shlex
import shutil
import subprocess
from pathlib import Path

import pytest

from fno.approvals import (
    AdapterCapability,
    ApprovalRequest,
    DecisionKind,
    EffectDisposition,
    EffectStore,
    RefusalReason,
    RefusedError,
    action_digest,
    classify_effect,
)
from fno.company.contracts import (
    CompanyWorkRefs,
    DeliverableRef,
    EffectRef,
    EvidenceRef,
    EvidenceResult,
    EvidenceSubjectKind,
    ObservationRef,
    WorkOrderRef,
)
from fno.company.topology import Topology, validate_manifest_topology
from fno.delivery import DeliveryEvidenceFact, evaluate_delivery
from fno.plugins.activate import activate
from fno.plugins.registry import PackRegistryStore
from fno.plugins.verify import load_manifest, verify_pack
from fno.roles.context import build_artifact_catalog
from fno.roles.models import (
    ApprovalFloor,
    CapabilityFact,
    ContextBundleBounds,
    ResolvedRole,
    RoleLayer,
)
from fno.roles.registry import discover_role_definitions
from fno.roles.resolver import resolve_role

REPO_ROOT = Path(__file__).resolve().parents[3]
PACK = REPO_ROOT / "plugins" / "company-conformance" / "plugin.yaml"
GROWTH_PACK = REPO_ROOT / "plugins" / "growth-studio" / "plugin.yaml"
NOW = dt.datetime(2026, 8, 4, 12, 0, tzinfo=dt.UTC)
FOUNDER = "principal:founder"
ROLE_IDS = ("support-response", "sales-outreach", "operations-recurring")
EFFECTS = {
    "support-response": ("external.communication", "support-channel"),
    "sales-outreach": ("external.outreach", "prospect-channel"),
}
OBSERVATION_CADENCE = {
    "sales-outreach": ("14d", "reply-rate"),
    "operations-recurring": ("per-cycle", "cycle-complete"),
}


def _manifest(path: Path):
    manifest, failure = load_manifest(path)
    assert failure is None, failure
    assert manifest is not None
    return manifest


CONFORMANCE = _manifest(PACK)
GROWTH = _manifest(GROWTH_PACK)


def _role_manifest(role_id: str):
    return next(role for role in CONFORMANCE.roles if role.role.id == role_id)


def _subject_kind(evidence_id: str) -> EvidenceSubjectKind:
    if evidence_id.endswith("review"):
        return EvidenceSubjectKind.REVIEW
    return {
        "acknowledgment": EvidenceSubjectKind.ACKNOWLEDGMENT,
        "approval": EvidenceSubjectKind.APPROVAL,
        "probe": EvidenceSubjectKind.PROBE,
    }[evidence_id]


def _company_work(role_id: str, *, attempt_id: str = "attempt-current") -> CompanyWorkRefs:
    role = _role_manifest(role_id)
    work_order = WorkOrderRef(
        node_id=f"wo-{role_id}",
        attempt_id=attempt_id,
        principal_id=FOUNDER,
        role_id=role_id,
    )
    deliverable_id = f"deliverable-{role_id}"
    evidence = tuple(
        EvidenceRef(
            id=f"{role_id}:{evidence_id}",
            work_order_id=work_order.node_id,
            attempt_id=attempt_id,
            subject_kind=_subject_kind(evidence_id),
            subject_id=evidence_id,
            result=EvidenceResult.PASSED,
        )
        for evidence_id in role.delivery_policy.required_evidence
    )
    effect = ()
    effect_id = None
    if role_id in EFFECTS:
        effect_class, destination = EFFECTS[role_id]
        effect_id = f"effect-{role_id}"
        effect = (
            EffectRef(
                id=effect_id,
                work_order_id=work_order.node_id,
                attempt_id=attempt_id,
                deliverable_id=deliverable_id,
                effect_class=effect_class,
                destination=destination,
                idempotency_key=f"key-{role_id}",
            ),
        )
    observations = ()
    if role_id in OBSERVATION_CADENCE:
        _, metric = OBSERVATION_CADENCE[role_id]
        observations = (
            ObservationRef(
                node_id=f"observation-{role_id}",
                delivery_work_order_id=work_order.node_id,
                deliverable_id=deliverable_id,
                metric_ids=(metric,),
            ),
        )
    return CompanyWorkRefs(
        function=role.function,
        role=role.role,
        work_order=work_order,
        deliverables=(
            DeliverableRef(
                id=deliverable_id,
                kind=role.deliverable_kinds[0],
                work_order_id=work_order.node_id,
                attempt_id=attempt_id,
                required_evidence_ids=tuple(item.id for item in evidence),
                effect_id=effect_id,
            ),
        ),
        effects=effect,
        evidence=evidence,
        observations=observations,
    )


def _fact(
    evidence: EvidenceRef,
    *,
    fact_revision: str = "facts-current",
    observed_at: dt.datetime = NOW,
    fresh_until: dt.datetime | None = None,
) -> DeliveryEvidenceFact:
    return DeliveryEvidenceFact(
        evidence=evidence,
        producer="company-conformance-fixture",
        observed_at=observed_at,
        source_revision="source-v1",
        fresh_until=fresh_until or NOW + dt.timedelta(hours=1),
        adapter_version="fixture-v1",
        fact_revision=fact_revision,
    )


def _complete_facts(company_work: CompanyWorkRefs) -> tuple[DeliveryEvidenceFact, ...]:
    return tuple(_fact(evidence) for evidence in company_work.evidence)


@pytest.fixture()
def role_root(tmp_path: Path) -> Path:
    root = tmp_path / "roles"
    root.mkdir()
    outcome = activate(
        PACK,
        registry_store=PackRegistryStore(tmp_path / "registry.json"),
        role_root=root,
    )
    assert outcome.unconfigured_artifacts == ()
    assert set(outcome.receipt.written_paths) == {
        f"plugin/company-conformance/{role_id}.json" for role_id in ROLE_IDS
    }
    return root


def test_pack_verification_runs_but_the_journey_carries_conformance() -> None:
    report = verify_pack(PACK, installed={})
    assert report.ok, [condition.name for condition in report.conditions if not condition.ok]
    scenario_conditions = [
        condition for condition in report.conditions if condition.name.startswith("scenario:")
    ]
    assert len(scenario_conditions) == len(ROLE_IDS)
    assert all(condition.checked for condition in scenario_conditions)


def test_declared_scenario_commands_execute_real_journey_tests() -> None:
    uv = shutil.which("uv")
    assert uv is not None
    env = os.environ.copy()
    env["PATH"] = os.pathsep.join((str(Path(uv).parent), "/usr/bin", "/bin"))
    for scenario in CONFORMANCE.scenarios:
        command = shlex.split(scenario.command)
        assert command[0] == "uv"
        command[0] = uv
        result = subprocess.run(
            command,
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            env=env,
        )
        assert result.returncode == 0, (
            scenario.id,
            result.stdout,
            result.stderr,
        )


def test_resolved_roles_match_every_declared_policy_axis(role_root: Path) -> None:
    records = discover_role_definitions(root=role_root)
    for declared in CONFORMANCE.roles:
        definitions = tuple(
            record.definition
            for record in records
            if record.definition is not None and record.role == declared.role
        )
        revision = definitions[0].snapshot_revision
        work_order = WorkOrderRef(
            node_id=f"wo-{declared.role.id}",
            attempt_id="attempt-current",
            role_id=declared.role.id,
        )
        facts = tuple(
            CapabilityFact(
                capability=capability,
                available=True,
                source_id="journey-fixture",
                snapshot_revision=revision,
                work_order_scope=work_order,
            )
            for capability in declared.required_capabilities
        )
        resolved = resolve_role(
            role=declared.role,
            definitions=definitions,
            capability_facts=facts,
            context_catalog=build_artifact_catalog(
                {}, snapshot_revision=revision, clock=NOW
            ),
            work_order=work_order,
            clock=NOW,
            snapshot_revision=revision,
            bundle_bounds=ContextBundleBounds(max_references=8, max_bytes=1024),
            unchecked_definitions=records,
        )
        assert isinstance(resolved, ResolvedRole), (declared.role.id, resolved)
        assert resolved.authority_ceiling is declared.authority_ceiling
        assert resolved.approval_floor is declared.approval_floor
        assert resolved.review_policy == declared.review_policy
        assert resolved.delivery_policy == declared.delivery_policy
        assert resolved.manifest.default_topology == declared.default_topology
        assert resolved.required_capabilities == declared.required_capabilities
        assert all(source.layer is RoleLayer.PLUGIN for source in resolved.source_chain)

    signatures = {
        (
            role.approval_floor,
            role.default_topology,
            role.review_policy.required_review_kinds,
            role.delivery_policy.required_evidence,
        )
        for role in CONFORMANCE.roles
    }
    assert len(signatures) == len(ROLE_IDS)
    assert any(role.approval_floor is ApprovalFloor.NONE for role in CONFORMANCE.roles)
    assert CONFORMANCE.skills == () and CONFORMANCE.agents == ()


def test_all_four_topologies_use_the_same_identity_and_policy_contracts() -> None:
    validated = {
        validate_manifest_topology(role.default_topology, source_layer=RoleLayer.PLUGIN)
        for role in (*CONFORMANCE.roles, *GROWTH.roles)
    }
    assert validated == set(Topology)

    for role_id in ROLE_IDS:
        role = _role_manifest(role_id)
        company_work = _company_work(role_id)
        before = (
            company_work.work_order,
            company_work.deliverables[0].required_evidence_ids,
            role.approval_floor,
        )
        assert validate_manifest_topology(role.default_topology) is Topology(
            role.default_topology
        )
        after = (
            company_work.work_order,
            company_work.deliverables[0].required_evidence_ids,
            role.approval_floor,
        )
        assert after == before


@pytest.mark.parametrize("role_id", ROLE_IDS)
def test_complete_delivery_evidence_passes(role_id: str) -> None:
    company_work = _company_work(role_id)
    verdict = evaluate_delivery(company_work, _complete_facts(company_work), evaluated_at=NOW)
    assert verdict.aggregate is EvidenceResult.PASSED, (role_id, verdict.diagnostics)


def test_support_response_scenario(role_root: Path) -> None:
    test_resolved_roles_match_every_declared_policy_axis(role_root)
    test_complete_delivery_evidence_passes("support-response")


def test_sales_outreach_scenario(role_root: Path) -> None:
    test_resolved_roles_match_every_declared_policy_axis(role_root)
    test_complete_delivery_evidence_passes("sales-outreach")


MISSING_CASES = tuple(
    (role.role.id, evidence_id)
    for role in CONFORMANCE.roles
    for evidence_id in role.delivery_policy.required_evidence
)


@pytest.mark.parametrize(("role_id", "missing_name"), MISSING_CASES)
def test_each_missing_requirement_never_reports_passed(
    role_id: str, missing_name: str
) -> None:
    company_work = _company_work(role_id)
    missing_id = f"{role_id}:{missing_name}"
    facts = tuple(
        fact for fact in _complete_facts(company_work) if fact.evidence.id != missing_id
    )
    verdict = evaluate_delivery(company_work, facts, evaluated_at=NOW)
    assert verdict.aggregate in {EvidenceResult.UNKNOWN, EvidenceResult.BLOCKED}
    assert verdict.aggregate is not EvidenceResult.PASSED


class _FounderAuthority:
    source = "journey-fixture"

    def may_approve(
        self, *, principal_id: str, effect_class: str, destination: str
    ) -> bool:
        return principal_id == FOUNDER


@pytest.mark.parametrize("role_id", ("support-response", "sales-outreach"))
def test_approval_for_a_different_action_digest_cannot_authorize(
    role_id: str, tmp_path: Path
) -> None:
    company_work = _company_work(role_id)
    effect = company_work.effects[0]
    assert classify_effect(effect.effect_class) is EffectDisposition.REQUIRE_APPROVAL
    approved = ApprovalRequest(
        request_id=f"request-{role_id}",
        principal_id=FOUNDER,
        work_order_id=effect.work_order_id,
        attempt_id=effect.attempt_id,
        effect_id=effect.id,
        effect_class=effect.effect_class,
        destination=effect.destination,
        action_digest=action_digest({"body": "approved"}),
        created_at=NOW,
        expires_at=NOW + dt.timedelta(hours=1),
    )
    altered = approved.model_copy(
        update={"action_digest": action_digest({"body": "edited"})}
    )
    with EffectStore(
        tmp_path / f"{role_id}.db",
        authority=_FounderAuthority(),
        events_path=tmp_path / f"{role_id}.jsonl",
        now=lambda: NOW,
    ) as store:
        store.submit(approved)
        store.decide(
            request_digest=approved.request_digest,
            deciding_principal_id=FOUNDER,
            decision=DecisionKind.APPROVED,
        )
        with pytest.raises(RefusedError) as excinfo:
            store.prepare(
                request_digest=altered.request_digest,
                idempotency_key=effect.idempotency_key or f"key-{role_id}",
                adapter=AdapterCapability(
                    adapter_id="fixture", adapter_version="fixture-v1"
                ),
            )
    assert excinfo.value.refusal.reason is RefusalReason.DIGEST_MISMATCH
    assert "digest" in str(excinfo.value).lower()
    assert classify_effect(effect.effect_class) is EffectDisposition.REQUIRE_APPROVAL


@pytest.mark.parametrize(
    ("role_id", "result"),
    (
        ("sales-outreach", EvidenceResult.PASSED),
        ("sales-outreach", EvidenceResult.FAILED),
        ("operations-recurring", EvidenceResult.PASSED),
        ("operations-recurring", EvidenceResult.FAILED),
    ),
)
def test_observation_facts_cannot_satisfy_delivery_or_change_its_verdict(
    role_id: str, result: EvidenceResult
) -> None:
    company_work = _company_work(role_id)
    cadence, metric = OBSERVATION_CADENCE[role_id]
    assert cadence in {"14d", "per-cycle"}
    assert company_work.observations[0].metric_ids == (metric,)
    missing_id = company_work.deliverables[0].required_evidence_ids[0]
    facts = tuple(
        fact for fact in _complete_facts(company_work) if fact.evidence.id != missing_id
    )
    baseline = evaluate_delivery(company_work, facts, evaluated_at=NOW)
    observation_fact = _fact(
        EvidenceRef(
            id=missing_id,
            work_order_id=company_work.work_order.node_id,
            attempt_id=company_work.work_order.attempt_id,
            subject_kind=EvidenceSubjectKind.OBSERVATION,
            subject_id=company_work.observations[0].node_id,
            result=result,
        )
    )
    with_observation = evaluate_delivery(
        company_work, (*facts, observation_fact), evaluated_at=NOW
    )
    assert with_observation == baseline
    assert with_observation.aggregate is EvidenceResult.UNKNOWN


def test_cadence_rejects_previous_cycle_residue_but_accepts_it_in_its_own_cycle() -> None:
    current = _company_work("operations-recurring", attempt_id="attempt-current")
    previous = _company_work("operations-recurring", attempt_id="attempt-previous")
    observed_at = NOW - dt.timedelta(hours=2)
    fresh_until = NOW - dt.timedelta(hours=1)
    previous_facts = tuple(
        _fact(
            evidence,
            fact_revision="facts-previous",
            observed_at=observed_at,
            fresh_until=fresh_until,
        )
        for evidence in previous.evidence
    )

    residue = evaluate_delivery(current, previous_facts, evaluated_at=NOW)
    diagnostics = "\n".join(
        diagnostic
        for requirement in residue.requirements
        for diagnostic in requirement.diagnostics
    )
    assert residue.aggregate is EvidenceResult.UNKNOWN
    assert "attempt attempt-previous" in diagnostics
    assert "stale after" in diagnostics

    own_cycle = evaluate_delivery(
        previous,
        previous_facts,
        evaluated_at=observed_at + dt.timedelta(minutes=30),
    )
    assert own_cycle.aggregate is EvidenceResult.PASSED
    assert all(
        requirement.result is EvidenceResult.PASSED
        for requirement in own_cycle.requirements
    )


def test_cadence_mixed_fact_revisions_make_every_requirement_unknown() -> None:
    current = _company_work("operations-recurring", attempt_id="attempt-current")
    previous = _company_work("operations-recurring", attempt_id="attempt-previous")
    previous_facts = tuple(
        _fact(
            evidence,
            fact_revision="facts-previous",
            observed_at=NOW - dt.timedelta(hours=2),
            fresh_until=NOW - dt.timedelta(hours=1),
        )
        for evidence in previous.evidence
    )

    verdict = evaluate_delivery(
        current,
        (*_complete_facts(current), *previous_facts),
        evaluated_at=NOW,
    )

    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert any("mixed fact revisions" in diagnostic for diagnostic in verdict.diagnostics)
    assert all(
        requirement.result is EvidenceResult.UNKNOWN
        for requirement in verdict.requirements
    )
    assert not any(
        requirement.result is EvidenceResult.PASSED
        for requirement in verdict.requirements
    )


def test_function_agnostic_guards_pass_over_both_packs() -> None:
    for script in (
        "scripts/ci/check-company-function-agnostic.sh",
        "scripts/ci/check-plugins-function-agnostic.sh",
    ):
        result = subprocess.run(
            ["bash", str(REPO_ROOT / script)],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
        )
        assert result.returncode == 0, f"{script}: {result.stdout}\n{result.stderr}"

    exact_names = {
        role.function.id
        for manifest in (CONFORMANCE, GROWTH)
        for role in manifest.roles
    } | {
        role.role.id
        for manifest in (CONFORMANCE, GROWTH)
        for role in manifest.roles
    }
    prohibited_roots = (
        "company",
        "roles",
        "delivery",
        "approvals",
        "plugins",
    )
    hits = []
    for package in prohibited_roots:
        for path in sorted((REPO_ROOT / "cli" / "src" / "fno" / package).rglob("*.py")):
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            docstrings = {
                id(body[0].value)
                for node in ast.walk(tree)
                if isinstance(
                    node,
                    (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef),
                )
                and (body := node.body)
                and isinstance(body[0], ast.Expr)
                and isinstance(body[0].value, ast.Constant)
                and isinstance(body[0].value.value, str)
            }
            for node in ast.walk(tree):
                if (
                    isinstance(node, ast.Constant)
                    and isinstance(node.value, str)
                    and id(node) not in docstrings
                ):
                    for name in exact_names:
                        if name in node.value:
                            hits.append(f"{path.relative_to(REPO_ROOT)}:{node.lineno}: {name}")
    assert hits == [], "function or role literal in core:\n" + "\n".join(hits)
