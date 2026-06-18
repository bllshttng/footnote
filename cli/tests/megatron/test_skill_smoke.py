"""Megatron Phase 4c Task 4.3: /megatron skill smoke tests.

Skill content lives at ``skills/megatron/SKILL.md`` and the references
folder. These tests guard the contract that links the skill markdown
to the validator and the manifest schema:

- Skill files exist at the expected paths.
- A representative substituted manifest validates clean.
- The discovery-questions reference enumerates all five questions.

We do not exercise the full conversation (AskUserQuestion is a Claude
runtime concept, not Python). The smoke covers the substrate the
skill relies on so a refactor that breaks the validator surface is
caught here.
"""
from __future__ import annotations

import textwrap
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _passthrough_resolver(monkeypatch):
    """Make resolve_project_name a pass-through so skill-rendered manifest
    fixtures don't depend on the user's real ~/.fno/settings.yaml.
    The validator's project-name check is exercised in test_validator.py.
    """
    import fno.megatron.validator as validator_mod

    monkeypatch.setattr(
        validator_mod,
        "resolve_project_name",
        lambda s: s,
    )


# ---------------------------------------------------------------------------
# Skill files exist
# ---------------------------------------------------------------------------

REPO_ROOT = Path(__file__).resolve().parents[3]
SKILL_DIR = REPO_ROOT / "skills" / "megatron"


def test_skill_file_exists():
    skill = SKILL_DIR / "SKILL.md"
    assert skill.exists(), f"missing: {skill}"
    text = skill.read_text(encoding="utf-8")
    assert "name: megatron" in text
    assert "argument-hint" in text
    # The skill must reference both reference docs
    assert "manifest-template.md" in text
    assert "discovery-questions.md" in text


def test_manifest_template_reference_exists():
    template = SKILL_DIR / "references" / "manifest-template.md"
    assert template.exists()
    text = template.read_text(encoding="utf-8")
    # Template must declare the manifest's required fields
    assert "mission_type: fleet" in text
    assert "{{ mission_id }}" in text
    assert "waves:" in text
    assert "validator" in text.lower()


def test_discovery_questions_reference_has_five_questions():
    qs = SKILL_DIR / "references" / "discovery-questions.md"
    assert qs.exists()
    text = qs.read_text(encoding="utf-8")
    # Five Qs - identified by ## Q1: through ## Q5:
    for n in range(1, 6):
        assert f"## Q{n}:" in text, f"discovery questions ref missing Q{n}"


# ---------------------------------------------------------------------------
# A skill-rendered manifest validates clean
# ---------------------------------------------------------------------------

def test_skill_rendered_manifest_validates_clean(tmp_path):
    """A representative mission rendered from the template parses + validates."""
    from fno.megatron import load_manifest, validate_manifest

    manifest_text = textwrap.dedent(
        """
        ---
        mission_type: fleet
        mission_id: ab-skltest1
        slug: 2026-05-06-region-feature
        created: 2026-05-06T15:00:00Z
        goal: |
          Ship the region feature end-to-end across example-pipeline and the records frontend.
        budget:
          cost_cap_usd_per_mission: 50.0
        failure_policy: block
        autonomy_level: cautious
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: example-pipeline
                body: |
                  Add new region source bootstrap and run a single record through.
          - wave: 2
            mode: parallel
            projects:
              - name: fno
                body: |
                  Document the cross-project megatron flow.
              - name: acme-frontend
                body: |
                  Surface region rollout completes in the timeline view.
        ---

        # Mission: Ship the region feature end-to-end
        """
    ).lstrip()

    path = tmp_path / "00-INDEX.md"
    path.write_text(manifest_text, encoding="utf-8")

    manifest = load_manifest(path)
    assert manifest.mission_type == "fleet"
    assert manifest.mission_id == "ab-skltest1"
    assert len(manifest.waves) == 2

    errors = validate_manifest(manifest)
    assert errors == [], f"unexpected validation errors: {errors}"


def test_skill_rendered_manifest_with_chain_research_rejected(tmp_path):
    """The skill must catch research-after-research at draft time."""
    from fno.megatron import load_manifest, validate_manifest

    manifest_text = textwrap.dedent(
        """
        ---
        mission_type: fleet
        mission_id: ab-chainsk
        waves:
          - wave: 1
            mode: sequential
            wave_type: research
            projects:
              - name: alpha
                body: "research a"
          - wave: 2
            mode: sequential
            wave_type: research
            projects:
              - name: beta
                body: "research b"
        ---
        """
    ).lstrip()

    path = tmp_path / "00-INDEX.md"
    path.write_text(manifest_text, encoding="utf-8")
    manifest = load_manifest(path)
    errors = validate_manifest(manifest)
    chain = [e for e in errors if e.code == "research_chain"]
    assert len(chain) == 1, "skill must surface research_chain at draft time"
