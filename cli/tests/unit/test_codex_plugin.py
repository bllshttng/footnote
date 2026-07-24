from __future__ import annotations

import json
import shutil
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.setup.codex_plugin import (
    DEV_MARKETPLACE,
    DEV_PLUGIN_ID,
    RELEASE_PLUGIN_ID,
    CodexPluginError,
    ConvergenceResult,
    converge,
    parse_state,
)


def _cp(
    argv: list[str], payload: object, *, rc: int = 0, err: str = ""
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(argv, rc, json.dumps(payload), err)


def _plugin(plugin_id: str, *, source: str, source_type: str) -> dict[str, object]:
    return {
        "pluginId": plugin_id,
        "marketplaceName": plugin_id.split("@", 1)[1],
        "version": "0.3.0",
        "installed": True,
        "enabled": True,
        "marketplaceSource": {"sourceType": source_type, "source": source},
    }


def _source(tmp_path: Path) -> Path:
    root = tmp_path / "source"
    (root / ".codex-plugin").mkdir(parents=True)
    (root / ".codex-plugin" / "plugin.json").write_text(
        json.dumps({"name": "fno", "version": "0.3.0"}), encoding="utf-8"
    )
    script = root / "scripts" / "release" / "sync-version.sh"
    script.parent.mkdir(parents=True)
    script.write_text("#!/bin/sh\n", encoding="utf-8")
    marketplace = root / ".agents" / "marketplaces" / DEV_MARKETPLACE
    (marketplace / ".agents" / "plugins").mkdir(parents=True)
    (marketplace / ".agents" / "plugins" / "marketplace.json").write_text(
        json.dumps(
            {
                "name": DEV_MARKETPLACE,
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
    return root


def _dev_marketplace(source: Path) -> Path:
    return source / ".agents" / "marketplaces" / DEV_MARKETPLACE


def test_parse_current_codex_json_state() -> None:
    state = parse_state(
        json.dumps(
            {
                "marketplaces": [
                    {
                        "name": "footnote",
                        "marketplaceSource": {
                            "sourceType": "git",
                            "source": "https://github.com/bllshttng/footnote.git",
                        },
                    }
                ]
            }
        ),
        json.dumps(
            {
                "installed": [
                    _plugin(
                        "fno@footnote",
                        source="https://github.com/bllshttng/footnote.git",
                        source_type="git",
                    )
                ],
                "available": [],
            }
        ),
    )
    assert state.marketplaces[0].name == "footnote"
    assert state.plugins[0].plugin_id == "fno@footnote"
    assert state.plugins[0].enabled is True


def test_release_convergence_removes_dev_then_installs_and_verifies(tmp_path: Path) -> None:
    source = _source(tmp_path)
    codex_home = tmp_path / "codex-home"
    marketplaces: list[dict[str, object]] = []
    plugins = [_plugin("fno@footnote-dev", source=str(source), source_type="local")]
    calls: list[tuple[list[str], dict[str, object]]] = []

    def runner(argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append((argv, kwargs))
        if argv == ["codex", "plugin", "marketplace", "list", "--json"]:
            return _cp(argv, {"marketplaces": marketplaces})
        if argv == ["codex", "plugin", "list", "--json"]:
            return _cp(argv, {"installed": plugins, "available": []})
        if argv == ["codex", "plugin", "remove", "fno@footnote-dev", "--json"]:
            marker = codex_home / "footnote" / "plugin-channel.json"
            assert json.loads(marker.read_text(encoding="utf-8"))["channel"] == "release"
            plugins.clear()
            return _cp(argv, {"removed": True})
        if argv == [
            "codex", "plugin", "marketplace", "add", "bllshttng/footnote", "--json"
        ]:
            marketplaces.append(
                {
                    "name": "footnote",
                    "marketplaceSource": {
                        "sourceType": "git",
                        "source": "https://github.com/bllshttng/footnote.git",
                    },
                }
            )
            return _cp(argv, {"name": "footnote"})
        if argv == [
            "codex", "plugin", "marketplace", "upgrade", "footnote", "--json"
        ]:
            return _cp(argv, {"upgraded": True})
        if argv == ["codex", "plugin", "add", "fno@footnote", "--json"]:
            plugins.append(
                _plugin(
                    "fno@footnote",
                    source="https://github.com/bllshttng/footnote.git",
                    source_type="git",
                )
            )
            return _cp(argv, {"pluginId": "fno@footnote"})
        if argv == [str(source / "scripts/release/sync-version.sh"), "--check"]:
            return _cp(argv, {"checked": True})
        return _cp(argv, {}, rc=1, err=f"unexpected {argv}")

    result = converge(
        channel="release",
        refresh=False,
        runner=runner,
        codex_home=codex_home,
        source_root=source,
    )

    assert (result.action, result.plugin_id, result.version) == (
        "installed",
        RELEASE_PLUGIN_ID,
        "0.3.0",
    )
    assert [call[0] for call in calls] == [
        ["codex", "plugin", "marketplace", "list", "--json"],
        ["codex", "plugin", "list", "--json"],
        ["codex", "plugin", "remove", "fno@footnote-dev", "--json"],
        [str(source / "scripts/release/sync-version.sh"), "--check"],
        ["codex", "plugin", "marketplace", "add", "bllshttng/footnote", "--json"],
        ["codex", "plugin", "marketplace", "upgrade", "footnote", "--json"],
        ["codex", "plugin", "add", "fno@footnote", "--json"],
        ["codex", "plugin", "list", "--json"],
    ]
    assert calls[3][1]["cwd"] == source


def test_release_convergence_is_source_aware_noop(tmp_path: Path) -> None:
    source = _source(tmp_path)
    calls: list[list[str]] = []

    def runner(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        if "marketplace" in argv:
            return _cp(
                argv,
                {
                    "marketplaces": [
                        {
                            "name": "footnote",
                            "marketplaceSource": {
                                "sourceType": "git",
                                "source": "https://github.com/bllshttng/footnote.git",
                            },
                        }
                    ]
                },
            )
        return _cp(
            argv,
            {
                "installed": [
                    _plugin(
                        "fno@footnote",
                        source="https://github.com/bllshttng/footnote.git",
                        source_type="git",
                    )
                ],
                "available": [],
            },
        )

    result = converge(
        channel="release",
        runner=runner,
        codex_home=tmp_path / "codex-home",
        source_root=source,
    )
    assert result.action == "no-op"
    assert calls == [
        ["codex", "plugin", "marketplace", "list", "--json"],
        ["codex", "plugin", "list", "--json"],
    ]


def test_dev_convergence_switches_from_release_and_writes_requested_marker(
    tmp_path: Path,
) -> None:
    source = _source(tmp_path)
    dev_marketplace = _dev_marketplace(source)
    codex_home = tmp_path / "codex-home"
    marketplaces: list[dict[str, object]] = []
    plugins = [
        _plugin(
            RELEASE_PLUGIN_ID,
            source="https://github.com/bllshttng/footnote.git",
            source_type="git",
        )
    ]
    calls: list[list[str]] = []

    def runner(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        if argv == ["codex", "plugin", "marketplace", "list", "--json"]:
            return _cp(argv, {"marketplaces": marketplaces})
        if argv == ["codex", "plugin", "list", "--json"]:
            return _cp(argv, {"installed": plugins, "available": []})
        if argv == ["codex", "plugin", "remove", RELEASE_PLUGIN_ID, "--json"]:
            marker = json.loads(
                (codex_home / "footnote" / "plugin-channel.json").read_text(
                    encoding="utf-8"
                )
            )
            assert marker == {
                "channel": "dev",
                "marketplace": DEV_MARKETPLACE,
                "source": str(dev_marketplace),
            }
            plugins.clear()
            return _cp(argv, {"removed": True})
        if argv == [
            "codex",
            "plugin",
            "marketplace",
            "add",
            str(dev_marketplace),
            "--json",
        ]:
            marketplaces.append(
                {
                    "name": DEV_MARKETPLACE,
                    "marketplaceSource": {
                        "sourceType": "local",
                        "source": str(dev_marketplace),
                    },
                }
            )
            return _cp(argv, {"name": DEV_MARKETPLACE})
        if argv == ["codex", "plugin", "add", DEV_PLUGIN_ID, "--json"]:
            plugins.append(
                _plugin(
                    DEV_PLUGIN_ID,
                    source=str(dev_marketplace),
                    source_type="local",
                )
            )
            return _cp(argv, {"pluginId": DEV_PLUGIN_ID})
        return _cp(argv, {}, rc=1, err=f"unexpected {argv}")

    result = converge(
        channel="dev",
        runner=runner,
        codex_home=codex_home,
        source_root=source,
    )

    assert (result.channel, result.action, result.plugin_id, result.version) == (
        "dev",
        "installed",
        DEV_PLUGIN_ID,
        "0.3.0",
    )
    assert calls == [
        ["codex", "plugin", "marketplace", "list", "--json"],
        ["codex", "plugin", "list", "--json"],
        ["codex", "plugin", "remove", RELEASE_PLUGIN_ID, "--json"],
        ["codex", "plugin", "marketplace", "add", str(dev_marketplace), "--json"],
        ["codex", "plugin", "add", DEV_PLUGIN_ID, "--json"],
        ["codex", "plugin", "list", "--json"],
    ]


def test_dev_refresh_replaces_same_version_cache_without_release_sync(tmp_path: Path) -> None:
    source = _source(tmp_path)
    dev_marketplace = _dev_marketplace(source)
    codex_home = tmp_path / "codex-home"
    source_payload = source / "skills" / "target" / "SKILL.md"
    source_payload.parent.mkdir(parents=True)
    source_payload.write_text("canonical dev payload\n", encoding="utf-8")
    cache = codex_home / "plugins" / "cache" / DEV_MARKETPLACE / "fno" / "0.3.0"
    cached_payload = cache / "skills" / "target" / "SKILL.md"
    cached_payload.parent.mkdir(parents=True)
    cached_payload.write_text("stale payload\n", encoding="utf-8")
    manifest_before = (source / ".codex-plugin" / "plugin.json").read_bytes()
    marketplace_row = {
        "name": DEV_MARKETPLACE,
        "marketplaceSource": {
            "sourceType": "local",
            "source": str(dev_marketplace),
        },
    }
    plugins = [
        _plugin(DEV_PLUGIN_ID, source=str(dev_marketplace), source_type="local")
    ]
    calls: list[list[str]] = []

    def runner(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        if argv == ["codex", "plugin", "marketplace", "list", "--json"]:
            return _cp(argv, {"marketplaces": [marketplace_row]})
        if argv == ["codex", "plugin", "list", "--json"]:
            return _cp(argv, {"installed": plugins, "available": []})
        if argv == ["codex", "plugin", "remove", DEV_PLUGIN_ID, "--json"]:
            plugins.clear()
            shutil.rmtree(cache)
            return _cp(argv, {"removed": True})
        if argv == ["codex", "plugin", "add", DEV_PLUGIN_ID, "--json"]:
            cached_payload.parent.mkdir(parents=True)
            cached_payload.write_bytes(source_payload.read_bytes())
            plugins.append(
                _plugin(DEV_PLUGIN_ID, source=str(dev_marketplace), source_type="local")
            )
            return _cp(argv, {"pluginId": DEV_PLUGIN_ID})
        return _cp(argv, {}, rc=1, err=f"unexpected {argv}")

    result = converge(
        channel="dev",
        refresh=True,
        runner=runner,
        codex_home=codex_home,
        source_root=source,
    )

    assert result.action == "refreshed"
    assert cached_payload.read_bytes() == source_payload.read_bytes()
    assert (source / ".codex-plugin" / "plugin.json").read_bytes() == manifest_before
    assert calls == [
        ["codex", "plugin", "marketplace", "list", "--json"],
        ["codex", "plugin", "list", "--json"],
        ["codex", "plugin", "remove", DEV_PLUGIN_ID, "--json"],
        ["codex", "plugin", "add", DEV_PLUGIN_ID, "--json"],
        ["codex", "plugin", "list", "--json"],
    ]


def test_dev_convergence_is_source_aware_noop_and_records_authority(
    tmp_path: Path,
) -> None:
    source = _source(tmp_path)
    dev_marketplace = _dev_marketplace(source)
    codex_home = tmp_path / "codex-home"
    calls: list[list[str]] = []

    def runner(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        if argv == ["codex", "plugin", "marketplace", "list", "--json"]:
            return _cp(
                argv,
                {
                    "marketplaces": [
                        {
                            "name": DEV_MARKETPLACE,
                            "marketplaceSource": {
                                "sourceType": "local",
                                "source": str(dev_marketplace),
                            },
                        }
                    ]
                },
            )
        return _cp(
            argv,
            {
                "installed": [
                    _plugin(
                        DEV_PLUGIN_ID,
                        source=str(dev_marketplace),
                        source_type="local",
                    )
                ],
                "available": [],
            },
        )

    result = converge(
        channel="dev",
        runner=runner,
        codex_home=codex_home,
        source_root=source,
    )

    assert result.action == "no-op"
    assert calls == [
        ["codex", "plugin", "marketplace", "list", "--json"],
        ["codex", "plugin", "list", "--json"],
    ]
    assert json.loads(
        (codex_home / "footnote" / "plugin-channel.json").read_text(encoding="utf-8")
    ) == {
        "channel": "dev",
        "marketplace": DEV_MARKETPLACE,
        "source": str(dev_marketplace),
    }


def test_external_failure_is_named_and_bounded(tmp_path: Path) -> None:
    source = _source(tmp_path)

    def runner(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(argv, 17, "", "prefix-" + "x" * 900)

    with pytest.raises(CodexPluginError) as caught:
        converge(
            channel="release",
            runner=runner,
            codex_home=tmp_path / "codex-home",
            source_root=source,
        )
    assert caught.value.stage == "marketplace-list"
    assert len(caught.value.detail) == 500


def test_public_cli_reports_verified_release_and_restart_posture(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno.setup_cli import app

    monkeypatch.setattr(
        "fno.setup.codex_plugin.converge",
        lambda **_kwargs: ConvergenceResult("release", "installed", "fno@footnote", "0.3.0"),
    )
    result = CliRunner().invoke(app, ["codex-plugin", "--channel", "release"])
    assert result.exit_code == 0, result.output
    assert "channel=release action=installed id=fno@footnote version=0.3.0" in result.output
    assert "hook approval may be needed" in result.output
    assert "new Codex session is required after mutation" in result.output


def test_public_cli_exits_nonzero_without_success_on_stage_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno.setup_cli import app

    def fail(**_kwargs: object) -> ConvergenceResult:
        raise CodexPluginError("plugin-add", "network unavailable")

    monkeypatch.setattr("fno.setup.codex_plugin.converge", fail)
    result = CliRunner().invoke(app, ["codex-plugin", "--channel", "release"])
    assert result.exit_code == 1
    assert "plugin-add: network unavailable" in result.output
    assert "verified" not in result.output
