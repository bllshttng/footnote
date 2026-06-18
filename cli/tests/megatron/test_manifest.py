"""Megatron Phase 2 Task 2.1: manifest schema + parser tests."""
from __future__ import annotations

import textwrap
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

def _write_manifest(tmp_path: Path, content: str) -> Path:
    path = tmp_path / "00-INDEX.md"
    path.write_text(textwrap.dedent(content).lstrip(), encoding="utf-8")
    return path


# ---------------------------------------------------------------------------
# AC1-HP: Two-wave manifest parses into typed structure
# ---------------------------------------------------------------------------

def test_ac1_hp_two_wave_manifest_parses(tmp_path):
    from fno.megatron import load_manifest

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-deadbeef
        budget:
          cost_cap_usd_per_mission: 50.0
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: backend
                body: "kick off"
          - wave: 2
            mode: parallel
            projects:
              - name: frontend
                body: "fan-out a"
              - name: ops
                body: "fan-out b"
              - name: data
                body: "fan-out c"
              - name: ml
                body: "fan-out d"
        ---

        # Mission body prose ignored by the parser
        """,
    )

    m = load_manifest(path)
    assert m.mission_id == "ab-deadbeef"
    assert m.mission_type == "fleet"
    assert m.budget.cost_cap_usd_per_mission == 50.0
    assert len(m.waves) == 2
    assert m.waves[0].mode == "sequential"
    assert m.waves[0].wave == 1
    assert len(m.waves[0].projects) == 1
    assert m.waves[0].projects[0].name == "backend"
    assert m.waves[0].projects[0].kind == "heads-up"  # default
    assert m.waves[1].mode == "parallel"
    assert len(m.waves[1].projects) == 4


# ---------------------------------------------------------------------------
# AC2-ERR: Missing mission_type rejects with clear error
# ---------------------------------------------------------------------------

def test_ac2_err_missing_mission_type(tmp_path):
    from fno.megatron import load_manifest, ManifestError

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_id: ab-deadbeef
        waves:
          - wave: 1
            projects:
              - name: backend
                body: "x"
        ---
        """,
    )

    with pytest.raises(ManifestError, match="mission_type"):
        load_manifest(path)


# ---------------------------------------------------------------------------
# AC4-EDGE: Single-project wave parses cleanly
# ---------------------------------------------------------------------------

def test_ac4_edge_single_project_wave(tmp_path):
    from fno.megatron import load_manifest

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-cafebabe
        waves:
          - wave: 1
            mode: sequential
            projects:
              - name: backend
                body: "x"
        ---
        """,
    )

    m = load_manifest(path)
    assert len(m.waves) == 1
    assert len(m.waves[0].projects) == 1
    assert m.waves[0].mode == "sequential"


# ---------------------------------------------------------------------------
# AC4-EDGE: All-research manifest parses (validator catches chain, not parser)
# ---------------------------------------------------------------------------

def test_ac4_edge_all_research_parses(tmp_path):
    from fno.megatron import load_manifest

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-research1
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

    m = load_manifest(path)
    assert m.waves[0].wave_type == "research"
    assert m.waves[1].wave_type == "research"


# ---------------------------------------------------------------------------
# AC5-FR: Malformed YAML raises with line number context
# ---------------------------------------------------------------------------

def test_ac5_fr_malformed_yaml_raises_with_line(tmp_path):
    from fno.megatron import load_manifest, ManifestError

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-broken
        waves:
          - wave: 1
            projects:
              - name: [
        ---
        """,
    )

    with pytest.raises(ManifestError, match="YAML parse error"):
        load_manifest(path)


# ---------------------------------------------------------------------------
# Defaults: mode defaults to sequential when absent; budget optional
# ---------------------------------------------------------------------------

def test_mode_defaults_to_sequential(tmp_path):
    from fno.megatron import load_manifest

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-defmode
        waves:
          - wave: 1
            projects:
              - name: backend
                body: "x"
        ---
        """,
    )

    m = load_manifest(path)
    assert m.waves[0].mode == "sequential"


def test_budget_defaults_when_absent(tmp_path):
    from fno.megatron import load_manifest

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-nobudget
        waves:
          - wave: 1
            projects:
              - name: backend
                body: "x"
        ---
        """,
    )

    m = load_manifest(path)
    assert m.budget.cost_cap_usd_per_mission is None


# ---------------------------------------------------------------------------
# CG7 (Plan B, ab-0e5a921e): combo: key in manifest frontmatter
# ---------------------------------------------------------------------------


def test_combo_key_parses_into_manifest(tmp_path):
    """AC7.1-HP: a manifest with combo: my-stack populates Manifest.combo."""
    from fno.megatron import load_manifest

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-cafef00d
        combo: my-stack
        waves:
          - wave: 1
            projects:
              - name: backend
                body: "do thing"
        ---
        # body
        """,
    )
    m = load_manifest(path)
    assert m.combo == "my-stack"


def test_combo_key_absent_keeps_combo_none(tmp_path):
    """No combo: key in frontmatter keeps Manifest.combo = None (back-compat)."""
    from fno.megatron import load_manifest

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-aaaaaaaa
        waves:
          - wave: 1
            projects:
              - name: backend
                body: "do thing"
        ---
        # body
        """,
    )
    m = load_manifest(path)
    assert m.combo is None


def test_combo_key_non_string_rejected(tmp_path):
    """combo must be a non-empty string when present."""
    from fno.megatron import load_manifest, ManifestError

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-bbbbbbbb
        combo: 42
        waves:
          - wave: 1
            projects:
              - name: backend
                body: "do thing"
        ---
        """,
    )
    with pytest.raises(ManifestError) as exc_info:
        load_manifest(path)
    assert "combo" in str(exc_info.value).lower()


def test_manifest_sha256_missing_file_raises(tmp_path):
    """manifest_sha256 raises ManifestError on a missing path.

    Pins the docstring contract: "Raises ManifestError when the file is
    unreadable; never returns empty."
    """
    from fno.megatron.manifest import ManifestError, manifest_sha256

    missing = tmp_path / "does-not-exist.md"
    with pytest.raises(ManifestError):
        manifest_sha256(missing)


def test_load_manifest_rejects_non_utf8(tmp_path):
    """load_manifest raises ManifestError on a non-UTF-8 byte sequence
    (parity with load_manifest_and_sha's decode handling)."""
    from fno.megatron import load_manifest, ManifestError

    path = tmp_path / "00-INDEX.md"
    # Latin-1-only byte that's invalid as UTF-8 start of a continuation
    path.write_bytes(b"---\nmission_id: ab\nstatus: r\n\xff\n---\n")
    with pytest.raises(ManifestError):
        load_manifest(path)


def test_load_manifest_and_sha_atomic_from_same_bytes(tmp_path):
    """load_manifest_and_sha returns a Manifest + sha hashed from the
    SAME bytes read, eliminating the TOCTOU window between separate
    load + hash reads."""
    import hashlib

    from fno.megatron.manifest import load_manifest_and_sha

    path = _write_manifest(
        tmp_path,
        """
        ---
        mission_type: fleet
        mission_id: ab-atomic1
        waves:
          - wave: 1
            projects:
              - name: backend
                body: "do thing"
        ---
        """,
    )

    m, sha = load_manifest_and_sha(path)
    assert m.mission_id == "ab-atomic1"
    expected = hashlib.sha256(path.read_bytes()).hexdigest()
    assert sha == expected
    assert len(sha) == 64
