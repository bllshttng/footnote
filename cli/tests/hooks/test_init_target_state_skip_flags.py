"""Test: size-profile skip flags are present in the immutable session manifest.

After control-plane collapse (ab-d0337fbc, Task 2.2), target-state.md is an
immutable inputs-only manifest:

- skip_flags_initial: REMOVED (no drift-check needed; manifest is write-once)
- phase_init event emission: REMOVED (loop-check reads manifest directly)
- skip flags (no_external, no_docs, ...) still present as top-level inputs

This file tests the new behavior: flat skip flags present per size profile,
skip_flags_initial absent, and no phase_init event emitted.

Old tests that asserted skip_flags_initial and phase_init existence are
replaced here (BUG-LOOP-001 follow-up is complete: the immutable contract
supersedes the drift-check).
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest
import yaml


_REPO_ROOT = Path(__file__).parent.parent.parent.parent
_INIT_SCRIPT = _REPO_ROOT / "hooks" / "helpers" / "init-target-state.sh"

# Expected flat skip_flag values per size profile (matches init-target-state.sh)
_PROFILE_FLAGS: dict[str, dict[str, bool]] = {
    "S": {
        "no_external": True,
        "no_docs": True,
        "no_ship": False,
        "no_verify": True,
        "no_goals": True,
        "no_browser": True,
        "no_clean": True,
        "no_how_to": True,
        "no_memory": True,
    },
    "M": {
        "no_external": False,
        "no_docs": False,
        "no_ship": False,
        "no_verify": True,
        "no_goals": False,
        "no_browser": False,
        "no_clean": True,
        "no_how_to": False,
        "no_memory": False,
    },
    "L": {
        "no_external": False,
        "no_docs": False,
        "no_ship": False,
        "no_verify": False,
        "no_goals": False,
        "no_browser": False,
        "no_clean": False,
        "no_how_to": False,
        "no_memory": False,
    },
}


def _run_init_script(tmpdir: Path, extra_env: dict[str, str]) -> subprocess.CompletedProcess:
    """Run init-target-state.sh in an isolated tmpdir with the given env overrides."""
    plan_file = tmpdir / "plan.md"
    plan_file.write_text("# Test plan\n")

    state_dir = tmpdir / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)

    env = {
        "HOME": str(tmpdir),
        "PATH": os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin"),
        "TARGET_START": "1",
        "TARGET_INPUT": str(plan_file),
        "TARGET_AUTO_MERGE": "false",
    }
    env.update(extra_env)

    proc = subprocess.run(
        ["bash", str(_INIT_SCRIPT)],
        cwd=str(tmpdir),
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    return proc


def _parse_target_state_frontmatter(state_file: Path) -> dict:
    """Extract YAML frontmatter from target-state.md (between --- delimiters)."""
    content = state_file.read_text()
    lines = content.splitlines()
    if not lines or lines[0].strip() != "---":
        raise ValueError(f"No opening --- in {state_file}")
    end = next(i for i, ln in enumerate(lines[1:], 1) if ln.strip() == "---")
    frontmatter_text = "\n".join(lines[1:end])
    return yaml.safe_load(frontmatter_text)


@pytest.mark.parametrize("target_size", ["S", "M", "L"])
def test_flat_skip_flags_per_size_profile(target_size, tmp_path):
    """AC1-HP: immutable manifest contains correct flat skip flags for each size profile.

    After control-plane collapse (ab-d0337fbc): skip_flags_initial is GONE;
    flat flags (no_external, no_docs, ...) remain as top-level inputs.
    """
    assert _INIT_SCRIPT.exists(), f"init script missing: {_INIT_SCRIPT}"

    proc = _run_init_script(tmp_path, {"TARGET_SIZE": target_size})
    assert proc.returncode == 0, (
        f"init-target-state.sh exited {proc.returncode} for TARGET_SIZE={target_size}\n"
        f"stdout: {proc.stdout[:500]}\nstderr: {proc.stderr[:500]}"
    )

    state_file = tmp_path / ".fno" / "target-state.md"
    assert state_file.exists(), "target-state.md not created"

    fm = _parse_target_state_frontmatter(state_file)

    # Immutability: skip_flags_initial must NOT be present
    assert "skip_flags_initial" not in fm, (
        "skip_flags_initial must not be in the immutable manifest "
        "(control-plane collapse, ab-d0337fbc)"
    )

    # Flat flags must match the size profile
    expected = _PROFILE_FLAGS[target_size]
    for flag, expected_val in expected.items():
        actual_val = fm.get(flag)
        assert actual_val == expected_val, (
            f"TARGET_SIZE={target_size}: manifest.{flag} "
            f"expected {expected_val}, got {actual_val}"
        )


def test_flat_skip_flags_reflect_env_override(tmp_path):
    """AC1-FR: per-flag env override is reflected in flat skip flags.

    M profile normally has no_external=false; override to true.
    """
    assert _INIT_SCRIPT.exists(), f"init script missing: {_INIT_SCRIPT}"

    proc = _run_init_script(tmp_path, {
        "TARGET_SIZE": "M",
        "TARGET_NO_EXTERNAL": "true",
    })
    assert proc.returncode == 0, (
        f"init-target-state.sh exited {proc.returncode}\n"
        f"stdout: {proc.stdout[:500]}\nstderr: {proc.stderr[:500]}"
    )

    state_file = tmp_path / ".fno" / "target-state.md"
    fm = _parse_target_state_frontmatter(state_file)

    # Flat flag must reflect the override
    assert fm.get("no_external") is True, (
        f"manifest.no_external should be True after TARGET_NO_EXTERNAL=true, "
        f"got: {fm.get('no_external')}"
    )

    # skip_flags_initial must be absent (immutable manifest)
    assert "skip_flags_initial" not in fm, (
        "skip_flags_initial must not be in the immutable manifest"
    )


def test_no_phase_init_event_emitted(tmp_path):
    """AC1-EDGE: no phase_init event is emitted (loop-check reads manifest directly).

    After control-plane collapse, phase_init event emission is removed.
    The manifest is the authoritative input record; events.jsonl gets
    termination events from loop-check, not init events from this script.
    """
    assert _INIT_SCRIPT.exists(), f"init script missing: {_INIT_SCRIPT}"

    proc = _run_init_script(tmp_path, {"TARGET_SIZE": "M"})
    assert proc.returncode == 0, (
        f"init-target-state.sh must succeed; stderr: {proc.stderr}"
    )

    state_file = tmp_path / ".fno" / "target-state.md"
    assert state_file.exists(), "State file must still be created"

    events_file = tmp_path / ".fno" / "events.jsonl"
    if events_file.exists():
        phase_init_found = any(
            '"type":"phase_init"' in line or '"event":"phase_init"' in line
            for line in events_file.read_text().splitlines()
        )
        assert not phase_init_found, (
            "phase_init event must NOT be emitted by the new immutable init "
            "(loop-check reads manifest directly; ab-d0337fbc)"
        )
    # If events.jsonl doesn't exist at all, that's also correct.


def test_authority_absent_without_yolo(tmp_path):
    """x-6390: no TARGET_YOLO -> the `authority` key is absent entirely.

    Absence (not `authority: none`) is the default posture, so every existing
    manifest reader is byte-for-byte unaffected by this feature.
    """
    proc = _run_init_script(tmp_path, {"TARGET_SIZE": "M"})
    assert proc.returncode == 0, f"stderr: {proc.stderr[:500]}"

    fm = _parse_target_state_frontmatter(tmp_path / ".fno" / "target-state.md")
    assert "authority" not in fm, f"authority must be absent without yolo; got {fm.get('authority')!r}"


def test_authority_full_with_yolo(tmp_path):
    """x-6390: TARGET_YOLO=1 -> `authority: full` round-trips through the manifest."""
    proc = _run_init_script(tmp_path, {"TARGET_SIZE": "M", "TARGET_YOLO": "1"})
    assert proc.returncode == 0, f"stderr: {proc.stderr[:500]}"

    fm = _parse_target_state_frontmatter(tmp_path / ".fno" / "target-state.md")
    assert fm.get("authority") == "full", f"expected authority=full, got {fm.get('authority')!r}"
    # The grant is orthogonal to auto-merge: yolo spends judgment, never
    # irreversibles (epic G8).
    assert fm.get("auto_merge_approved") is False


def test_immutable_manifest_has_no_mutable_fields(tmp_path):
    """AC2-HP: the manifest must not contain any mutable control-plane fields."""
    assert _INIT_SCRIPT.exists(), f"init script missing: {_INIT_SCRIPT}"

    proc = _run_init_script(tmp_path, {"TARGET_SIZE": "M"})
    assert proc.returncode == 0

    state_file = tmp_path / ".fno" / "target-state.md"
    fm = _parse_target_state_frontmatter(state_file)

    forbidden = [
        "status", "current_phase", "iteration",
        "quality_check_passed", "output_validated", "artifact_shipped",
        "external_review_passed", "goal_verification_passed", "docs_generated",
        "memory_pass_passed", "browser_testing_passed", "deferrals_captured",
        "ledger_updated", "provenance_nonce", "skip_flags_initial",
        "coordinator_phase", "session_start_context_loaded",
        "merged_prs", "merge_auto_queued", "merge_failed", "conflicts_resolved",
    ]
    found = [f for f in forbidden if f in fm]
    assert not found, (
        f"Immutable manifest must not contain mutable fields; found: {found}"
    )
