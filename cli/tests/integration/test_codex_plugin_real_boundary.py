"""Opt-in regression against the real Codex 0.145 plugin parser."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
pytestmark = pytest.mark.skipif(
    os.environ.get("FNO_REAL_CODEX_PLUGIN_TEST") != "1",
    reason="set FNO_REAL_CODEX_PLUGIN_TEST=1 to run real Codex 0.145 coverage",
)


def _run(home: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["codex", *args],
        capture_output=True,
        text=True,
        check=False,
        timeout=180,
        env={**os.environ, "CODEX_HOME": str(home)},
    )


def test_codex_0145_rejects_escaping_alias_and_installs_canonical_identity(
    tmp_path: Path,
) -> None:
    if shutil.which("codex") is None:
        pytest.fail("real Codex test enabled but codex is not installed")
    version = subprocess.run(
        ["codex", "--version"],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert version.returncode == 0
    assert version.stdout.strip() == "codex-cli 0.145.0"

    home = tmp_path / "codex-home"
    home.mkdir()
    legacy_source = tmp_path / "legacy-source"
    shutil.copytree(REPO_ROOT / ".codex-plugin", legacy_source / ".codex-plugin")
    legacy_marketplace = legacy_source / ".agents/marketplaces/footnote-dev"
    descriptor = legacy_marketplace / ".agents/plugins/marketplace.json"
    descriptor.parent.mkdir(parents=True)
    descriptor.write_text(
        json.dumps(
            {
                "name": "footnote-dev",
                "plugins": [
                    {
                        "name": "fno",
                        "source": {"source": "local", "path": "../../.."},
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    legacy_add = _run(
        home,
        "plugin",
        "marketplace",
        "add",
        str(legacy_marketplace),
        "--json",
    )
    assert legacy_add.returncode == 0, legacy_add.stderr
    legacy_available = _run(
        home,
        "plugin",
        "list",
        "--available",
        "--marketplace",
        "footnote-dev",
        "--json",
    )
    assert legacy_available.returncode == 0, legacy_available.stderr
    assert json.loads(legacy_available.stdout)["available"] == []
    legacy_plugin_add = _run(
        home, "plugin", "add", "fno@footnote-dev", "--json"
    )
    assert legacy_plugin_add.returncode == 1
    assert "not found" in legacy_plugin_add.stderr

    canonical_add = _run(
        home,
        "plugin",
        "marketplace",
        "add",
        str(REPO_ROOT),
        "--json",
    )
    assert canonical_add.returncode == 0, canonical_add.stderr
    canonical_plugin_add = _run(home, "plugin", "add", "fno@footnote", "--json")
    assert canonical_plugin_add.returncode == 0, canonical_plugin_add.stderr
    final = _run(home, "plugin", "list", "--json")
    assert final.returncode == 0, final.stderr
    enabled = [
        row
        for row in json.loads(final.stdout)["installed"]
        if row["installed"] and row["enabled"] and row["pluginId"].startswith("fno@")
    ]
    assert [row["pluginId"] for row in enabled] == ["fno@footnote"]
