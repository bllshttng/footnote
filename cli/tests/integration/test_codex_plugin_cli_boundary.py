"""Hermetic executable-boundary coverage for ``fno setup codex-plugin``."""
from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_SRC = REPO_ROOT / "cli" / "src"


def _source_fixture(tmp_path: Path) -> Path:
    source = tmp_path / "source"
    for manifest in (".codex-plugin/plugin.json", ".claude-plugin/plugin.json"):
        path = source / manifest
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"name": "fno", "version": "0.3.0"}), encoding="utf-8")
    marker = source / "hooks" / "helpers" / "init-target-state.sh"
    marker.parent.mkdir(parents=True)
    marker.write_text("#!/bin/sh\n", encoding="utf-8")
    payload = source / "skills" / "target" / "SKILL.md"
    payload.parent.mkdir(parents=True)
    payload.write_text("canonical payload\n", encoding="utf-8")
    release_check = source / "scripts" / "release" / "sync-version.sh"
    release_check.parent.mkdir(parents=True)
    release_check.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    release_check.chmod(release_check.stat().st_mode | stat.S_IXUSR)
    descriptor = (
        source
        / ".agents/marketplaces/footnote-dev/.agents/plugins/marketplace.json"
    )
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
    return source


def _fake_codex(fake_bin: Path) -> None:
    executable = fake_bin / "codex"
    executable.parent.mkdir(parents=True)
    executable.write_text(
        f'''#!{sys.executable}
import json
import os
import shutil
import sys
from pathlib import Path

args = sys.argv[1:]
state_path = Path(os.environ["FNO_TEST_CODEX_STATE"])
calls_path = Path(os.environ["FNO_TEST_CODEX_CALLS"])
home = Path(os.environ["CODEX_HOME"])
source = Path(os.environ["FNO_TEST_SOURCE"])
state = json.loads(state_path.read_text())
with calls_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(args) + "\\n")

if os.environ.get("FNO_TEST_CODEX_MALFORMED") and args == ["plugin", "marketplace", "list", "--json"]:
    print("not-json")
    raise SystemExit(0)
if os.environ.get("FNO_TEST_CODEX_FAIL") == " ".join(args[:2]):
    print("injected codex failure", file=sys.stderr)
    raise SystemExit(17)
if args == ["plugin", "marketplace", "list", "--json"]:
    print(json.dumps({{"marketplaces": state["marketplaces"]}}))
elif args == ["plugin", "list", "--json"]:
    print(json.dumps({{"installed": state["plugins"], "available": []}}))
elif len(args) == 4 and args[:2] == ["plugin", "remove"] and args[3] == "--json":
    plugin_id = args[2]
    for plugin in state["plugins"]:
        if plugin["pluginId"] == plugin_id:
            cache = home / "plugins" / "cache" / plugin["marketplaceName"] / "fno" / plugin["version"]
            shutil.rmtree(cache, ignore_errors=True)
    state["plugins"] = [p for p in state["plugins"] if p["pluginId"] != plugin_id]
    print(json.dumps({{"removed": True}}))
elif len(args) == 5 and args[:3] == ["plugin", "marketplace", "add"] and args[4] == "--json":
    supplied = args[3]
    if supplied == "bllshttng/footnote":
        row = {{"name": "footnote", "root": str(source), "marketplaceSource": {{"sourceType": "git", "source": supplied}}}}
    else:
        row = {{"name": "footnote-dev", "root": supplied, "marketplaceSource": {{"sourceType": "local", "source": supplied}}}}
    state["marketplaces"] = [m for m in state["marketplaces"] if m["name"] != row["name"]]
    state["marketplaces"].append(row)
    print(json.dumps({{"name": row["name"]}}))
elif len(args) == 5 and args[:3] == ["plugin", "marketplace", "upgrade"] and args[4] == "--json":
    print(json.dumps({{"upgraded": True}}))
elif len(args) == 4 and args[:2] == ["plugin", "add"] and args[3] == "--json":
    plugin_id = args[2]
    marketplace = plugin_id.split("@", 1)[1]
    source_type = "git" if marketplace == "footnote" else "local"
    marketplace_source = "bllshttng/footnote" if marketplace == "footnote" else str(source / ".agents/marketplaces/footnote-dev")
    cache = home / "plugins" / "cache" / marketplace / "fno" / "0.3.0"
    shutil.rmtree(cache, ignore_errors=True)
    for relative in (".codex-plugin", "skills", "hooks", "scripts", ".codex/agents"):
        origin = source / relative
        if origin.exists():
            shutil.copytree(origin, cache / relative)
    state["plugins"].append({{
        "pluginId": plugin_id,
        "marketplaceName": marketplace,
        "version": "0.3.0",
        "installed": True,
        "enabled": True,
        "marketplaceSource": {{"sourceType": source_type, "source": marketplace_source}},
    }})
    print(json.dumps({{"pluginId": plugin_id}}))
else:
    print("unexpected argv: " + repr(args), file=sys.stderr)
    raise SystemExit(23)
state_path.write_text(json.dumps(state))
''',
        encoding="utf-8",
    )
    executable.chmod(executable.stat().st_mode | stat.S_IXUSR)


def _run_setup(env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "fno.cli", "setup", "codex-plugin", *args],
        capture_output=True,
        text=True,
        env=env,
        check=False,
        timeout=30,
    )


def _run_doctor(env: dict[str, str], source: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "fno.cli", "doctor", "--json", "--source", str(source)],
        capture_output=True,
        text=True,
        env=env,
        check=False,
        timeout=30,
    )


def _environment(tmp_path: Path, source: Path, state_path: Path) -> dict[str, str]:
    fake_bin = tmp_path / "bin"
    _fake_codex(fake_bin)
    return {
        **os.environ,
        "PATH": f"{fake_bin}:{os.environ['PATH']}",
        "PYTHONPATH": str(CLI_SRC),
        "CODEX_HOME": str(tmp_path / "codex-home"),
        "FNO_HOME": str(tmp_path / "fno-home"),
        "FNO_REPO_ROOT": str(source),
        "FNO_TEST_SOURCE": str(source),
        "FNO_TEST_CODEX_STATE": str(state_path),
        "FNO_TEST_CODEX_CALLS": str(tmp_path / "calls.jsonl"),
    }


def test_public_cli_refresh_idempotence_and_channel_switch_are_hermetic(
    tmp_path: Path,
) -> None:
    source = _source_fixture(tmp_path)
    home = tmp_path / "codex-home"
    dev_marketplace = source / ".agents/marketplaces/footnote-dev"
    stale = home / "plugins/cache/footnote-dev/fno/0.3.0/skills/target/SKILL.md"
    stale.parent.mkdir(parents=True)
    stale.write_text("stale payload\n", encoding="utf-8")
    state_path = tmp_path / "state.json"
    state_path.write_text(
        json.dumps(
            {
                "marketplaces": [
                    {
                        "name": "footnote-dev",
                        "root": str(dev_marketplace),
                        "marketplaceSource": {
                            "sourceType": "local",
                            "source": str(dev_marketplace),
                        },
                    }
                ],
                "plugins": [
                    {
                        "pluginId": "fno@footnote-dev",
                        "marketplaceName": "footnote-dev",
                        "version": "0.3.0",
                        "installed": True,
                        "enabled": True,
                        "marketplaceSource": {
                            "sourceType": "local",
                            "source": str(dev_marketplace),
                        },
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    env = _environment(tmp_path, source, state_path)
    calls_path = Path(env["FNO_TEST_CODEX_CALLS"])

    refreshed = _run_setup(env, "--channel", "dev", "--refresh")
    assert refreshed.returncode == 0, refreshed.stderr
    assert "channel=dev action=refreshed id=fno@footnote-dev version=0.3.0" in refreshed.stdout
    assert stale.read_bytes() == (source / "skills/target/SKILL.md").read_bytes()
    assert [json.loads(line) for line in calls_path.read_text().splitlines()] == [
        ["plugin", "marketplace", "list", "--json"],
        ["plugin", "list", "--json"],
        ["plugin", "remove", "fno@footnote-dev", "--json"],
        ["plugin", "add", "fno@footnote-dev", "--json"],
        ["plugin", "list", "--json"],
    ]

    doctor_fresh = _run_doctor(env, source)
    fresh_payload = json.loads(doctor_fresh.stdout)
    assert fresh_payload["harness_surface"]["codex_plugin"]["status"] == "fresh"
    source_payload = source / "skills/target/SKILL.md"
    source_payload.write_text("newer canonical payload\n", encoding="utf-8")
    doctor_stale = _run_doctor(env, source)
    stale_payload = json.loads(doctor_stale.stdout)
    assert stale_payload["harness_surface"]["codex_plugin"]["status"] == "stale"
    assert stale_payload["harness_surface"]["codex_plugin"]["issue"] == "payload-drift"
    source_payload.write_text("canonical payload\n", encoding="utf-8")

    calls_path.write_text("", encoding="utf-8")
    repeated = _run_setup(env, "--channel", "dev")
    assert repeated.returncode == 0, repeated.stderr
    assert "action=no-op" in repeated.stdout
    assert [json.loads(line) for line in calls_path.read_text().splitlines()] == [
        ["plugin", "marketplace", "list", "--json"],
        ["plugin", "list", "--json"],
    ]

    calls_path.write_text("", encoding="utf-8")
    release = _run_setup(env, "--channel", "release")
    assert release.returncode == 0, release.stderr
    assert "channel=release action=installed id=fno@footnote version=0.3.0" in release.stdout
    state = json.loads(state_path.read_text())
    assert [plugin["pluginId"] for plugin in state["plugins"]] == ["fno@footnote"]
    marker = json.loads((home / "footnote/plugin-channel.json").read_text())
    assert marker["channel"] == "release"


def test_public_cli_names_malformed_codex_json_without_success(tmp_path: Path) -> None:
    source = _source_fixture(tmp_path)
    state_path = tmp_path / "state.json"
    state_path.write_text(json.dumps({"marketplaces": [], "plugins": []}), encoding="utf-8")
    env = _environment(tmp_path, source, state_path)
    env["FNO_TEST_CODEX_MALFORMED"] = "1"

    result = _run_setup(env, "--channel", "release")

    assert result.returncode == 1
    assert "marketplace-list: malformed JSON" in result.stderr
    assert "verified" not in result.stdout


def test_concurrent_public_channel_selection_serializes_to_one_plugin(
    tmp_path: Path,
) -> None:
    source = _source_fixture(tmp_path)
    state_path = tmp_path / "state.json"
    state_path.write_text(
        json.dumps({"marketplaces": [], "plugins": []}), encoding="utf-8"
    )
    env = _environment(tmp_path, source, state_path)
    command = [sys.executable, "-m", "fno.cli", "setup", "codex-plugin"]
    release = subprocess.Popen(
        [*command, "--channel", "release"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    dev = subprocess.Popen(
        [*command, "--channel", "dev"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )

    release_out, release_err = release.communicate(timeout=30)
    dev_out, dev_err = dev.communicate(timeout=30)

    assert release.returncode == 0, release_err
    assert dev.returncode == 0, dev_err
    assert "verified" in release_out and "verified" in dev_out
    marker = json.loads(
        (tmp_path / "codex-home/footnote/plugin-channel.json").read_text()
    )
    state = json.loads(state_path.read_text())
    expected = "fno@footnote" if marker["channel"] == "release" else "fno@footnote-dev"
    assert [plugin["pluginId"] for plugin in state["plugins"]] == [expected]


def test_public_cli_propagates_real_codex_failure_without_false_success(
    tmp_path: Path,
) -> None:
    source = _source_fixture(tmp_path)
    dev_marketplace = source / ".agents/marketplaces/footnote-dev"
    state_path = tmp_path / "state.json"
    state_path.write_text(
        json.dumps(
            {
                "marketplaces": [
                    {
                        "name": "footnote-dev",
                        "root": str(dev_marketplace),
                        "marketplaceSource": {
                            "sourceType": "local",
                            "source": str(dev_marketplace),
                        },
                    }
                ],
                "plugins": [
                    {
                        "pluginId": "fno@footnote-dev",
                        "marketplaceName": "footnote-dev",
                        "version": "0.3.0",
                        "installed": True,
                        "enabled": True,
                        "marketplaceSource": {
                            "sourceType": "local",
                            "source": str(dev_marketplace),
                        },
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    env = _environment(tmp_path, source, state_path)
    env["FNO_TEST_CODEX_FAIL"] = "plugin add"

    result = _run_setup(env, "--channel", "release")

    assert result.returncode == 1
    assert "plugin-add: injected codex failure" in result.stderr
    assert "verified" not in result.stdout
    assert json.loads(state_path.read_text())["plugins"] == []
    marker = json.loads(
        (tmp_path / "codex-home/footnote/plugin-channel.json").read_text()
    )
    assert marker["channel"] == "release"


def test_setup_wizard_adapter_uses_verified_release_convergence(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    from fno.setup.integration import build_adapters, run_cli_integration

    source = _source_fixture(tmp_path)
    state_path = tmp_path / "state.json"
    state_path.write_text(
        json.dumps({"marketplaces": [], "plugins": []}), encoding="utf-8"
    )
    env = _environment(tmp_path, source, state_path)
    for key, value in env.items():
        monkeypatch.setenv(key, value)
    monkeypatch.setenv("PATH", f"{tmp_path / 'bin'}:/usr/bin:/bin")
    output: list[str] = []

    results = run_cli_integration(
        select_fn=lambda options: [
            str(option["cli"]) for option in options if option["cli"] == "codex"
        ],
        echo_fn=output.append,
        adapters=build_adapters(),
    )

    codex = next(result for result in results if result.cli == "codex")
    assert codex.status == "installed"
    assert any("Codex CLI: installed" in line for line in output)
    state = json.loads(state_path.read_text())
    assert [plugin["pluginId"] for plugin in state["plugins"]] == ["fno@footnote"]
