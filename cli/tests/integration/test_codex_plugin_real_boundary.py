"""Opt-in regression against the real Codex 0.145 plugin parser."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tomllib
from pathlib import Path

import pytest

from fno import __version__ as FNO_VERSION
from fno.setup.codex_plugin import CodexPluginError, converge


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
    legacy_remove = _run(
        home,
        "plugin",
        "marketplace",
        "remove",
        "footnote-dev",
        "--json",
    )
    assert legacy_remove.returncode == 0, legacy_remove.stderr

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
    marketplaces = _run(home, "plugin", "marketplace", "list", "--json")
    assert marketplaces.returncode == 0, marketplaces.stderr
    assert [
        row["name"] for row in json.loads(marketplaces.stdout)["marketplaces"]
    ] == ["footnote"]
    # Codex names the cache dir after the manifest version, so pinning a literal
    # here turns every release bump red. sync-version.sh keeps __version__ and
    # the plugin manifests in lockstep (and --check gates the drift), so the
    # wheel version is the one true name of this directory.
    cache = home / f"plugins/cache/footnote/fno/{FNO_VERSION}"
    for relative in ("skills", "agents", "commands", "hooks"):
        assert (cache / relative).is_dir(), relative


def test_codex_0145_failed_legacy_migration_restores_config_cache_and_marker(
    tmp_path: Path,
) -> None:
    home = tmp_path / "codex-home"
    home.mkdir()
    legacy_source = tmp_path / "legacy-source"
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
    config = home / "config.toml"
    config.write_text(
        "# unrelated user comment must survive rollback\n"
        "\n".join(
            (
                "[marketplaces.footnote-dev]",
                'source_type = "local"',
                f'source = "{legacy_marketplace}"',
                "",
                '[plugins."fno@footnote-dev"]',
                "enabled = true",
                "",
            )
        ),
        encoding="utf-8",
    )
    expected_config_bytes = config.read_bytes()
    expected_config = tomllib.loads(config.read_text(encoding="utf-8"))
    legacy_cache = home / "plugins/cache/footnote-dev/fno/0.3.0"
    legacy_cache.mkdir(parents=True)
    cache_bytes = b"legacy working payload\n"
    (legacy_cache / "payload").write_bytes(cache_bytes)
    marker = home / "footnote/plugin-channel.json"
    marker.parent.mkdir(parents=True)
    marker_bytes = b'{"channel":"dev","marketplace":"footnote-dev","source":"legacy"}\n'
    marker.write_bytes(marker_bytes)
    failed_once = False

    def runner(
        argv: list[str], **kwargs: object
    ) -> subprocess.CompletedProcess[str]:
        nonlocal failed_once
        result = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            errors="replace",
            check=False,
            **kwargs,
        )
        env = kwargs.get("env")
        is_live = env is None or Path(str(env["CODEX_HOME"])) == home
        if (
            is_live
            and argv == ["codex", "plugin", "add", "fno@footnote", "--json"]
            and result.returncode == 0
            and not failed_once
        ):
            failed_once = True
            return subprocess.CompletedProcess(
                argv,
                17,
                result.stdout,
                "injected post-mutation plugin add failure",
            )
        return result

    with pytest.raises(CodexPluginError) as caught:
        converge(
            channel="dev",
            runner=runner,
            codex_home=home,
            source_root=REPO_ROOT,
        )

    assert caught.value.stage == "plugin-add"
    assert config.read_bytes() == expected_config_bytes
    assert tomllib.loads(config.read_text(encoding="utf-8")) == expected_config
    assert (legacy_cache / "payload").read_bytes() == cache_bytes
    assert marker.read_bytes() == marker_bytes
    assert not (home / "plugins/cache/footnote").exists()
    marketplaces = _run(home, "plugin", "marketplace", "list", "--json")
    assert marketplaces.returncode == 0, marketplaces.stderr
    assert [
        row["name"] for row in json.loads(marketplaces.stdout)["marketplaces"]
    ] == ["footnote-dev"]
