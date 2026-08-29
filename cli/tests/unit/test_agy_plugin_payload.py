"""Payload boundary for the agy plugin install (x-70a1).

`agy plugin install` deep-copies whatever directory it is handed, dereferencing
symlinks, so the script must stage an explicit allowlist instead of the repo
root - the unbounded install produced a 9.1 GB corrupt half-copy. This module
runs the script's --build-only lane (no agy binary needed, so it runs in CI)
and asserts the boundary it promises: only the allowlist entries, a manifest
that fits agy's additionalProperties:false schema, and a total size ceiling
that catches the next path added to the allowlist by accident.
"""

import json
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "scripts" / "install" / "agy-plugin.sh"

FORBIDDEN_ENTRIES = {"internal", "crates", "cli", ".git", ".fno", "docs", "scripts"}
MANIFEST_KEYS = {"$schema", "name", "description"}


@pytest.fixture(scope="module")
def payload(tmp_path_factory) -> Path:
    target = tmp_path_factory.mktemp("agy-payload")
    proc = subprocess.run(
        ["bash", str(SCRIPT), "--build-only", str(target)],
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert proc.returncode == 0, f"--build-only failed: {proc.stderr}"
    built = target / "footnote"
    assert built.is_dir(), proc.stderr
    return built


def test_payload_holds_exactly_the_allowlist(payload: Path):
    # Positive markers only: each allowed entry is present by name...
    assert (payload / "plugin.json").is_file()
    assert (payload / "skills" / "using-fno" / "SKILL.md").is_file()
    agents = list((payload / "agents").rglob("*"))
    assert any(p.is_file() for p in agents)
    # ...and the top level names nothing outside it.
    entries = {p.name for p in payload.iterdir()}
    assert entries == {"plugin.json", "skills", "agents"}
    leaked = entries & FORBIDDEN_ENTRIES
    assert not leaked, f"forbidden path(s) rode into the payload: {sorted(leaked)}"


def test_manifest_fits_agys_additional_properties_false_schema(payload: Path):
    manifest = json.loads((payload / "plugin.json").read_text(encoding="utf-8"))
    extra = set(manifest) - MANIFEST_KEYS
    assert not extra, f"plugin.json carries keys agy's schema rejects: {sorted(extra)}"
    assert manifest.get("name"), "plugin.json must carry a non-empty name"


def test_payload_stays_under_50_mb(payload: Path):
    total = sum(p.stat().st_size for p in payload.rglob("*") if p.is_file())
    assert total < 50 * 1024 * 1024, f"payload is {total} bytes; the allowlist grew"


def test_build_only_refuses_a_missing_allowlist_entry(tmp_path):
    # A missing skills/ must fail loudly and leave no partial payload behind.
    # The script resolves ROOT_DIR from its own location, so copying it into a
    # fake repo layout points it at the fixture.
    fake = tmp_path / "repo"
    (fake / "scripts" / "install").mkdir(parents=True)
    script = fake / "scripts" / "install" / "agy-plugin.sh"
    script.write_text(SCRIPT.read_text(encoding="utf-8"), encoding="utf-8")
    (fake / "plugin.json").write_text('{"name": "footnote"}\n', encoding="utf-8")
    target = tmp_path / "out"
    proc = subprocess.run(
        ["bash", str(script), "--build-only", str(target)],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert proc.returncode != 0
    assert "skills" in proc.stderr
    assert not target.exists()
