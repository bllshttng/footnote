"""Megatron Phase 2 Task 2.3: manifest validator tests."""
from __future__ import annotations

import textwrap
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _passthrough_resolver(monkeypatch):
    """Default: make resolve_project_name a pass-through so existing tests
    that use made-up project names (backend, frontend, ops, ...) are not
    broken by the project-name validation check.

    Tests that need specific resolver behavior apply their own
    monkeypatch.setattr AFTER this fixture runs, which overrides it.
    """
    import fno.megatron.validator as validator_mod

    monkeypatch.setattr(
        validator_mod,
        "resolve_project_name",
        lambda s: s,  # always succeeds; returns input unchanged
    )


def _write_and_load(tmp_path: Path, content: str):
    from fno.megatron import load_manifest

    path = tmp_path / "00-INDEX.md"
    path.write_text(textwrap.dedent(content).lstrip(), encoding="utf-8")
    return load_manifest(path)


# ---------------------------------------------------------------------------
# AC1-HP: clean manifest validates with no errors
# ---------------------------------------------------------------------------

def test_clean_manifest_validates(tmp_path):
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-clean1
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: backend
                body: "x"
          - wave: 2
            mode: parallel
            projects:
              - name: frontend
                body: "y"
              - name: ops
                body: "z"
        ---
        """,
    )
    assert validate_manifest(m) == []


# ---------------------------------------------------------------------------
# AC4-EDGE: empty wave (no projects + no tasks) rejected
# ---------------------------------------------------------------------------

def test_empty_wave_rejected(tmp_path):
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-empty1
        waves:
          - wave: 1
            mode: sequential
            projects: []
            tasks: []
        ---
        """,
    )
    errors = validate_manifest(m)
    assert len(errors) == 1
    assert errors[0].code == "empty_wave"
    assert errors[0].wave_index == 0
    assert "at least one project or task" in errors[0].message


# ---------------------------------------------------------------------------
# AC4-EDGE: wave with > max projects rejected
# ---------------------------------------------------------------------------

def test_wave_project_cap_exceeded(tmp_path):
    from fno.megatron import validate_manifest

    project_yaml = "\n".join(
        f"              - name: proj{i}\n                body: 'x'" for i in range(9)
    )
    m = _write_and_load(
        tmp_path,
        f"""
        ---
        mission_type: fleet
        mission_id: ab-bigwave
        waves:
          - wave: 1
            mode: parallel
            projects:
{project_yaml}
        ---
        """,
    )
    errors = validate_manifest(m)
    cap_errors = [e for e in errors if e.code == "wave_project_cap_exceeded"]
    assert len(cap_errors) == 1
    assert "max 8" in cap_errors[0].message
    assert "9" in cap_errors[0].message


# ---------------------------------------------------------------------------
# AC4-EDGE: research-after-research rejected; index points at offender
# ---------------------------------------------------------------------------

def test_research_chain_rejected(tmp_path):
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-chain
        waves:
          - wave: 1
            mode: sequential
            wave_type: research
            projects:
              - name: backend
                body: "research a"
          - wave: 2
            mode: sequential
            wave_type: research
            projects:
              - name: frontend
                body: "research b"
        ---
        """,
    )
    errors = validate_manifest(m)
    chain_errors = [e for e in errors if e.code == "research_chain"]
    assert len(chain_errors) == 1
    assert chain_errors[0].wave_index == 1  # the OFFENDING wave (second research)


# ---------------------------------------------------------------------------
# AC4-EDGE: body oversize (>10KB) rejected
# ---------------------------------------------------------------------------

def test_body_oversize_rejected(tmp_path):
    from fno.megatron import validate_manifest

    big = "x" * (11 * 1024)
    m = _write_and_load(
        tmp_path,
        f"""
        ---
        mission_type: fleet
        mission_id: ab-bigbody
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: backend
                body: "{big}"
        ---
        """,
    )
    errors = validate_manifest(m)
    oversize_errors = [e for e in errors if e.code == "body_oversize"]
    assert len(oversize_errors) == 1
    assert "backend" in oversize_errors[0].message


# ---------------------------------------------------------------------------
# AC2-ERR: multiple errors aggregate (no short-circuit)
# ---------------------------------------------------------------------------

def test_multiple_errors_aggregate(tmp_path):
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-multi
        waves:
          - wave: 1
            mode: sequential
            projects: []
            tasks: []
          - wave: 2
            mode: sequential
            wave_type: research
            projects:
              - name: backend
                body: "research a"
          - wave: 3
            mode: sequential
            wave_type: research
            projects:
              - name: frontend
                body: "research b"
        ---
        """,
    )
    errors = validate_manifest(m)
    codes = sorted({e.code for e in errors})
    assert "empty_wave" in codes
    assert "research_chain" in codes


# ---------------------------------------------------------------------------
# Settings override: max_projects_per_wave can be raised via settings.yaml
# ---------------------------------------------------------------------------

def test_short_name_resolves_clean(tmp_path, monkeypatch):
    """AC1-UI: manifest using a short_name value in project.name validates clean
    when the resolver normalizes it to a known canonical.
    """
    import fno.megatron.validator as validator_mod
    from fno.megatron import validate_manifest
    from fno.projects.resolve import ProjectNotFound

    # Override the autouse pass-through with a realistic short-name resolver.
    def fake_resolve(s: str) -> str:
        if s == "etl":
            return "example-pipeline"
        raise ProjectNotFound(
            f"unknown project name {s!r}; known canonical names: ['example-pipeline']"
        )

    monkeypatch.setattr(validator_mod, "resolve_project_name", fake_resolve)

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-shortname
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: etl
                body: "deploy pipeline"
        ---
        """,
    )
    errors = validate_manifest(m)
    project_unknown_errors = [e for e in errors if e.code == "project_unknown"]
    assert project_unknown_errors == []


def test_project_unknown_surfaces_in_errors(tmp_path, monkeypatch):
    """AC1-UI: manifest with an unknown project name produces project_unknown error."""
    import fno.megatron.validator as validator_mod
    from fno.megatron import validate_manifest
    from fno.projects.resolve import ProjectNotFound

    def fake_resolve(s: str) -> str:
        raise ProjectNotFound(
            f"unknown project name {s!r}; known canonical names: []"
        )

    monkeypatch.setattr(validator_mod, "resolve_project_name", fake_resolve)

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-unknown
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: nonexistent-xyz
                body: "some work"
        ---
        """,
    )
    errors = validate_manifest(m)
    project_unknown_errors = [e for e in errors if e.code == "project_unknown"]
    assert len(project_unknown_errors) == 1
    assert project_unknown_errors[0].wave_index == 0
    assert "nonexistent-xyz" in project_unknown_errors[0].message


# ---------------------------------------------------------------------------
# AC1-HP / AC1-ERR / AC1-UI / AC1-EDGE: duplicate canonical project names
# ---------------------------------------------------------------------------

def test_duplicate_canonical_names_flagged(tmp_path):
    """AC1-HP: duplicate canonical names in same wave produce an error."""
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-dup1
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: fake-a
                body: "x"
              - name: fake-a
                body: "y"
        ---
        """,
    )
    errors = validate_manifest(m)
    dup_errors = [e for e in errors if e.code == "duplicate_project_in_wave"]
    assert len(dup_errors) == 1
    assert "fake-a" in dup_errors[0].message
    assert "wave 1" in dup_errors[0].message
    assert dup_errors[0].wave_index == 0


def test_short_name_resolving_to_same_canonical_flagged(tmp_path, monkeypatch):
    """AC1-ERR: name + short_name both resolving to same canonical is flagged."""
    import fno.megatron.validator as validator_mod

    # Override autouse pass-through: a-short resolves to fake-a so both
    # entries collide on the canonical name even though raw names differ.
    monkeypatch.setattr(
        validator_mod,
        "resolve_project_name",
        lambda s: "fake-a" if s in ("fake-a", "a-short") else s,
    )
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-dup2
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: fake-a
                body: "x"
              - name: a-short
                body: "y"
        ---
        """,
    )
    errors = validate_manifest(m)
    dup_errors = [e for e in errors if e.code == "duplicate_project_in_wave"]
    assert len(dup_errors) == 1
    assert "fake-a" in dup_errors[0].message


def test_same_project_in_different_waves_not_flagged(tmp_path):
    """AC1-UI: wave-N and wave-N+1 may both include the same project."""
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-dup3
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: fake-a
                body: "x"
          - wave: 2
            mode: sequential
            projects:
              - name: fake-a
                body: "y"
              - name: fake-b
                body: "z"
        ---
        """,
    )
    errors = validate_manifest(m)
    assert not any(e.code == "duplicate_project_in_wave" for e in errors)


def test_duplicate_short_name_resolver_error_does_not_silently_degrade(
    tmp_path, monkeypatch
):
    """DuplicateShortName raise must NOT fall through to raw-name compare.

    A manifest entry whose short name resolves ambiguously across
    workspaces cannot be safely canonicalized for duplicate detection.
    The check must skip the project entirely rather than weaken to a
    raw-name compare that could either false-positive a duplicate or
    false-negative a real one.
    """
    import fno.megatron.validator as validator_mod
    from fno.megatron import validate_manifest
    from fno.projects.resolve import DuplicateShortName

    def fake_resolve(s: str) -> str:
        if s == "ambiguous":
            raise DuplicateShortName(
                f"short_name {s!r} resolves to multiple canonical names"
            )
        return s  # other names pass through

    monkeypatch.setattr(validator_mod, "resolve_project_name", fake_resolve)

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-dupshort
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: ambiguous
                body: "x"
              - name: ambiguous
                body: "y"
        ---
        """,
    )
    errors = validate_manifest(m)
    # No duplicate_project_in_wave emitted because the ambiguous resolver
    # error makes raw-name comparison meaningless; the prior
    # _check_project_names reports the ambiguity via resolver_unavailable.
    dup_errors = [e for e in errors if e.code == "duplicate_project_in_wave"]
    assert dup_errors == []


def test_single_project_wave_no_duplicate_error(tmp_path):
    """AC1-EDGE: wave with one project produces no duplicate error."""
    from fno.megatron import validate_manifest

    m = _write_and_load(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-dup4
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: fake-a
                body: "x"
        ---
        """,
    )
    errors = validate_manifest(m)
    assert not any(e.code == "duplicate_project_in_wave" for e in errors)


def test_settings_override_raises_cap(tmp_path, monkeypatch):
    from fno.megatron import validate_manifest

    project_yaml = "\n".join(
        f"              - name: proj{i}\n                body: 'x'" for i in range(9)
    )
    m = _write_and_load(
        tmp_path,
        f"""
        ---
        mission_type: fleet
        mission_id: ab-cap
        waves:
          - wave: 1
            mode: parallel
            projects:
{project_yaml}
        ---
        """,
    )

    errors = validate_manifest(m, max_projects_per_wave=12)
    cap_errors = [e for e in errors if e.code == "wave_project_cap_exceeded"]
    assert cap_errors == []
