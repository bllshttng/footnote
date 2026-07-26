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
        path.write_text(
            json.dumps({"name": "fno", "version": "0.3.0"}), encoding="utf-8"
        )
    for relative, payload in (
        ("skills/target/SKILL.md", "canonical skill\n"),
        ("agents/reviewer.md", "canonical agent\n"),
        ("commands/target.md", "canonical command\n"),
        ("hooks/helpers/init-target-state.sh", "#!/bin/sh\n"),
        (".codex/agents/reviewer.toml", 'name = "reviewer"\n'),
    ):
        path = source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(payload, encoding="utf-8")
    release_check = source / "scripts" / "release" / "sync-version.sh"
    release_check.parent.mkdir(parents=True)
    release_check.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    release_check.chmod(release_check.stat().st_mode | stat.S_IXUSR)
    return source


def _plugin(*, source: str, source_type: str, plugin_id: str = "fno@footnote") -> dict[str, object]:
    return {
        "pluginId": plugin_id,
        "marketplaceName": plugin_id.split("@", 1)[1],
        "version": "0.3.0",
        "installed": True,
        "enabled": True,
        "marketplaceSource": {"sourceType": source_type, "source": source},
    }


def _marketplace(*, source: str, source_type: str, name: str = "footnote") -> dict[str, object]:
    return {
        "name": name,
        "root": source,
        "marketplaceSource": {"sourceType": source_type, "source": source},
    }


def _write_state(path: Path, *, marketplaces: list[dict[str, object]], plugins: list[dict[str, object]]) -> None:
    path.write_text(
        json.dumps({"marketplaces": marketplaces, "plugins": plugins}),
        encoding="utf-8",
    )


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
home = Path(os.environ["CODEX_HOME"])
live_home = Path(os.environ["FNO_TEST_LIVE_CODEX_HOME"])
is_live = home == live_home
state_path = Path(os.environ["FNO_TEST_CODEX_STATE"]) if is_live else home / "fake-state.json"
calls_path = Path(os.environ["FNO_TEST_CODEX_CALLS"])
source = Path(os.environ["FNO_TEST_SOURCE"])
home.mkdir(parents=True, exist_ok=True)
if not state_path.exists():
    state_path.write_text(json.dumps({{"marketplaces": [], "plugins": []}}))
state = json.loads(state_path.read_text())
with calls_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps({{"live": is_live, "args": args}}) + "\\n")

source_mutation = live_home / "mutated-source-once"
if is_live and os.environ.get("FNO_TEST_MUTATE_SOURCE_ON_LIVE") and not source_mutation.exists():
    source_mutation.touch()
    payload = source / "skills" / "target" / "SKILL.md"
    payload.write_text(payload.read_text() + "changed after validation\\n")

if is_live and os.environ.get("FNO_TEST_CODEX_MALFORMED") and args == ["plugin", "marketplace", "list", "--json"]:
    print("not-json")
    raise SystemExit(0)
if args == ["plugin", "marketplace", "list", "--json"]:
    print(json.dumps({{"marketplaces": state["marketplaces"]}}))
elif args == ["plugin", "list", "--json"]:
    print(json.dumps({{"installed": state["plugins"], "available": []}}))
elif len(args) == 6 and args[:2] == ["plugin", "list"] and args[2:4] == ["--available", "--marketplace"] and args[5] == "--json":
    available = [] if os.environ.get("FNO_TEST_BAD_CANDIDATE") else [{{"pluginId": "fno@footnote", "version": "0.3.0"}}]
    print(json.dumps({{"installed": state["plugins"], "available": available}}))
elif len(args) == 4 and args[:2] == ["plugin", "remove"] and args[3] == "--json":
    state["plugins"] = [row for row in state["plugins"] if row["pluginId"] != args[2]]
    failure_marker = live_home / "fail-plugin-remove-after-mutation-once"
    if is_live and os.environ.get("FNO_TEST_FAIL_LIVE_PLUGIN_REMOVE_AFTER_MUTATION") and not failure_marker.exists():
        failure_marker.touch()
        state_path.write_text(json.dumps(state))
        if os.environ.get("FNO_TEST_MUTATE_CONFIG_ON_FAILURE"):
            config = home / "config.toml"
            config.write_text(
                "[unrelated]\\nvalue = 1\\n\\n"
                "[marketplaces.footnote]\\nsource_type = \\\"git\\\"\\nsource = \\\"bllshttng/footnote\\\"\\n"
            )
        print("injected post-mutation plugin remove failure", file=sys.stderr)
        raise SystemExit(17)
    print(json.dumps({{"removed": True}}))
elif len(args) == 5 and args[:3] == ["plugin", "marketplace", "remove"] and args[4] == "--json":
    state["marketplaces"] = [row for row in state["marketplaces"] if row["name"] != args[3]]
    print(json.dumps({{"removed": True}}))
elif len(args) == 5 and args[:3] == ["plugin", "marketplace", "add"] and args[4] == "--json":
    supplied = args[3]
    source_type = "git" if supplied == "bllshttng/footnote" else "local"
    root = str(source) if source_type == "git" else supplied
    name = "footnote-dev" if Path(supplied).name == "footnote-dev" else "footnote"
    row = {{"name": name, "root": root, "marketplaceSource": {{"sourceType": source_type, "source": supplied}}}}
    state["marketplaces"] = [item for item in state["marketplaces"] if item["name"] != name]
    state["marketplaces"].append(row)
    print(json.dumps({{"name": name}}))
elif len(args) == 5 and args[:3] == ["plugin", "marketplace", "upgrade"] and args[4] == "--json":
    print(json.dumps({{"upgraded": True}}))
elif len(args) == 4 and args[:2] == ["plugin", "add"] and args[3] == "--json":
    failure_marker = live_home / "fail-plugin-add-once"
    if is_live and os.environ.get("FNO_TEST_FAIL_LIVE_PLUGIN_ADD") and not failure_marker.exists():
        failure_marker.touch()
        print("injected live plugin add failure", file=sys.stderr)
        raise SystemExit(17)
    plugin_id = args[2]
    marketplace_name = plugin_id.split("@", 1)[1]
    marketplace = next(row for row in state["marketplaces"] if row["name"] == marketplace_name)
    registration = marketplace["marketplaceSource"]
    cache = home / "plugins" / "cache" / marketplace_name / "fno" / "0.3.0"
    shutil.rmtree(cache, ignore_errors=True)
    for relative in (".codex-plugin", "skills", "agents", "commands", "hooks", "scripts", ".codex/agents"):
        origin = source / relative
        if origin.exists():
            shutil.copytree(origin, cache / relative)
    state["plugins"] = [row for row in state["plugins"] if row["pluginId"] != plugin_id]
    state["plugins"].append({{
        "pluginId": plugin_id,
        "marketplaceName": marketplace_name,
        "version": "0.3.0",
        "installed": True,
        "enabled": True,
        "marketplaceSource": registration,
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


def _environment(tmp_path: Path, source: Path, state_path: Path) -> dict[str, str]:
    fake_bin = tmp_path / "bin"
    _fake_codex(fake_bin)
    live_home = tmp_path / "codex-home"
    return {
        **os.environ,
        "PATH": f"{fake_bin}:{os.environ['PATH']}",
        "PYTHONPATH": str(CLI_SRC),
        "CODEX_HOME": str(live_home),
        "FNO_HOME": str(tmp_path / "fno-home"),
        "FNO_REPO_ROOT": str(source),
        "FNO_TEST_SOURCE": str(source),
        "FNO_TEST_LIVE_CODEX_HOME": str(live_home),
        "FNO_TEST_CODEX_STATE": str(state_path),
        "FNO_TEST_CODEX_CALLS": str(tmp_path / "calls.jsonl"),
    }


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


def test_public_cli_migrates_refreshes_and_switches_one_identity(tmp_path: Path) -> None:
    source = _source_fixture(tmp_path)
    home = tmp_path / "codex-home"
    state_path = tmp_path / "state.json"
    legacy = tmp_path / "footnote-dev"
    legacy.mkdir()
    _write_state(
        state_path,
        marketplaces=[_marketplace(source=str(legacy), source_type="local", name="footnote-dev")],
        plugins=[_plugin(source=str(legacy), source_type="local", plugin_id="fno@footnote-dev")],
    )
    legacy_cache = home / "plugins/cache/footnote-dev/fno/0.3.0"
    legacy_cache.mkdir(parents=True)
    env = _environment(tmp_path, source, state_path)

    refreshed = _run_setup(env, "--channel", "dev", "--refresh")
    assert refreshed.returncode == 0, refreshed.stderr
    assert "channel=dev action=refreshed id=fno@footnote version=0.3.0" in refreshed.stdout
    state = json.loads(state_path.read_text())
    assert [row["name"] for row in state["marketplaces"]] == ["footnote"]
    assert [row["pluginId"] for row in state["plugins"]] == ["fno@footnote"]
    assert state["marketplaces"][0]["marketplaceSource"] == {
        "sourceType": "local",
        "source": str(source),
    }
    assert not legacy_cache.exists()
    for relative in ("skills", "agents", "commands", "hooks"):
        assert (home / "plugins/cache/footnote/fno/0.3.0" / relative).is_dir()

    doctor = _run_doctor(env, source)
    assert json.loads(doctor.stdout)["harness_surface"]["codex_plugin"]["status"] == "fresh"
    repeated = _run_setup(env, "--channel", "dev")
    assert repeated.returncode == 0, repeated.stderr
    assert "action=no-op" in repeated.stdout

    release = _run_setup(env, "--channel", "release")
    assert release.returncode == 0, release.stderr
    state = json.loads(state_path.read_text())
    assert [row["name"] for row in state["marketplaces"]] == ["footnote"]
    assert [row["pluginId"] for row in state["plugins"]] == ["fno@footnote"]
    assert state["marketplaces"][0]["marketplaceSource"]["sourceType"] == "git"
    marker = json.loads((home / "footnote/plugin-channel.json").read_text())
    assert marker["channel"] == "release"
    assert marker["source"] == "bllshttng/footnote"

    dev_again = _run_setup(env, "--channel", "dev")
    assert dev_again.returncode == 0, dev_again.stderr
    state = json.loads(state_path.read_text())
    assert state["marketplaces"][0]["marketplaceSource"] == {
        "sourceType": "local",
        "source": str(source),
    }
    marker = json.loads((home / "footnote/plugin-channel.json").read_text())
    assert marker == {
        "channel": "dev",
        "marketplace": "footnote",
        "source": str(source),
    }
    calls = [json.loads(line) for line in Path(env["FNO_TEST_CODEX_CALLS"]).read_text().splitlines()]
    assert any(not call["live"] for call in calls)
    assert any(call["live"] for call in calls)


def test_invalid_candidate_never_replaces_live_channel(tmp_path: Path) -> None:
    source = _source_fixture(tmp_path)
    home = tmp_path / "codex-home"
    state_path = tmp_path / "state.json"
    _write_state(
        state_path,
        marketplaces=[_marketplace(source=str(source), source_type="local")],
        plugins=[_plugin(source=str(source), source_type="local")],
    )
    marker = home / "footnote/plugin-channel.json"
    marker.parent.mkdir(parents=True)
    marker_bytes = b'{"channel":"dev","marketplace":"footnote","source":"working"}\n'
    marker.write_bytes(marker_bytes)
    before = state_path.read_bytes()
    env = _environment(tmp_path, source, state_path)
    env["FNO_TEST_BAD_CANDIDATE"] = "1"

    result = _run_setup(env, "--channel", "release")

    assert result.returncode == 1
    assert "candidate-plugin-list" in result.stderr
    assert state_path.read_bytes() == before
    assert marker.read_bytes() == marker_bytes
    calls = [json.loads(line) for line in Path(env["FNO_TEST_CODEX_CALLS"]).read_text().splitlines()]
    assert calls and all(not call["live"] for call in calls)


def test_failed_live_switch_rolls_back_state_and_marker(tmp_path: Path) -> None:
    source = _source_fixture(tmp_path)
    home = tmp_path / "codex-home"
    state_path = tmp_path / "state.json"
    _write_state(
        state_path,
        marketplaces=[_marketplace(source=str(source), source_type="local")],
        plugins=[_plugin(source=str(source), source_type="local")],
    )
    marker = home / "footnote/plugin-channel.json"
    marker.parent.mkdir(parents=True)
    marker_bytes = b'{"channel":"dev","marketplace":"footnote","source":"working"}\n'
    marker.write_bytes(marker_bytes)
    prior_cache = home / "plugins/cache/footnote/fno/0.3.0"
    prior_cache.mkdir(parents=True)
    prior_cache_bytes = b"previous working cache\n"
    (prior_cache / "working").write_bytes(prior_cache_bytes)
    env = _environment(tmp_path, source, state_path)
    env["FNO_TEST_FAIL_LIVE_PLUGIN_ADD"] = "1"

    result = _run_setup(env, "--channel", "release")

    assert result.returncode == 1
    assert "plugin-add: injected live plugin add failure" in result.stderr
    assert "verified" not in result.stdout
    state = json.loads(state_path.read_text())
    assert [row["name"] for row in state["marketplaces"]] == ["footnote"]
    assert state["marketplaces"][0]["marketplaceSource"] == {
        "sourceType": "local",
        "source": str(source),
    }
    assert [row["pluginId"] for row in state["plugins"]] == ["fno@footnote"]
    assert marker.read_bytes() == marker_bytes
    assert (prior_cache / "working").read_bytes() == prior_cache_bytes


def test_post_mutation_process_failure_rolls_back_state_and_marker(tmp_path: Path) -> None:
    source = _source_fixture(tmp_path)
    home = tmp_path / "codex-home"
    state_path = tmp_path / "state.json"
    _write_state(
        state_path,
        marketplaces=[_marketplace(source=str(source), source_type="local")],
        plugins=[_plugin(source=str(source), source_type="local")],
    )
    marker = home / "footnote/plugin-channel.json"
    marker.parent.mkdir(parents=True)
    marker_bytes = b'{"channel":"dev","marketplace":"footnote","source":"working"}\n'
    marker.write_bytes(marker_bytes)
    config = home / "config.toml"
    config_bytes = (
        b"# preserve this user heading\n"
        b"[unrelated]\n"
        b"value = 1 # preserve this user comment\n\n"
        b"[marketplaces.footnote]\n"
        b'source_type = "local"\n'
        + f'source = "{source}"\n\n'.encode()
        + b'[plugins."fno@footnote"]\n'
        b"enabled = true\n"
    )
    config.write_bytes(config_bytes)
    env = _environment(tmp_path, source, state_path)
    env["FNO_TEST_FAIL_LIVE_PLUGIN_REMOVE_AFTER_MUTATION"] = "1"
    env["FNO_TEST_MUTATE_CONFIG_ON_FAILURE"] = "1"

    result = _run_setup(env, "--channel", "release")

    assert result.returncode == 1
    assert "plugin-remove: injected post-mutation plugin remove failure" in result.stderr
    state = json.loads(state_path.read_text())
    assert [row["pluginId"] for row in state["plugins"]] == ["fno@footnote"]
    assert state["marketplaces"][0]["marketplaceSource"] == {
        "sourceType": "local",
        "source": str(source),
    }
    assert marker.read_bytes() == marker_bytes
    assert config.read_bytes() == config_bytes


def test_candidate_payload_change_never_commits_live_switch(tmp_path: Path) -> None:
    source = _source_fixture(tmp_path)
    home = tmp_path / "codex-home"
    state_path = tmp_path / "state.json"
    _write_state(
        state_path,
        marketplaces=[_marketplace(source=str(source), source_type="local")],
        plugins=[_plugin(source=str(source), source_type="local")],
    )
    marker = home / "footnote/plugin-channel.json"
    marker.parent.mkdir(parents=True)
    marker_bytes = b'{"channel":"dev","marketplace":"footnote","source":"working"}\n'
    marker.write_bytes(marker_bytes)
    env = _environment(tmp_path, source, state_path)
    env["FNO_TEST_MUTATE_SOURCE_ON_LIVE"] = "1"

    result = _run_setup(env, "--channel", "release")

    assert result.returncode == 1
    assert "live payload differs from validated candidate" in result.stderr
    state = json.loads(state_path.read_text())
    assert state["marketplaces"][0]["marketplaceSource"]["sourceType"] == "local"
    assert [row["pluginId"] for row in state["plugins"]] == ["fno@footnote"]
    assert marker.read_bytes() == marker_bytes


def test_concurrent_channel_selection_serializes_to_one_identity(tmp_path: Path) -> None:
    source = _source_fixture(tmp_path)
    state_path = tmp_path / "state.json"
    _write_state(state_path, marketplaces=[], plugins=[])
    env = _environment(tmp_path, source, state_path)
    command = [sys.executable, "-m", "fno.cli", "setup", "codex-plugin"]
    processes = [
        subprocess.Popen(
            [*command, "--channel", channel],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
        for channel in ("release", "dev")
    ]
    outputs = [process.communicate(timeout=30) for process in processes]

    for process, (stdout, stderr) in zip(processes, outputs, strict=True):
        assert process.returncode == 0, stderr
        assert "verified" in stdout
    state = json.loads(state_path.read_text())
    assert [row["name"] for row in state["marketplaces"]] == ["footnote"]
    assert [row["pluginId"] for row in state["plugins"]] == ["fno@footnote"]
    marker = json.loads((Path(env["CODEX_HOME"]) / "footnote/plugin-channel.json").read_text())
    registration = state["marketplaces"][0]["marketplaceSource"]
    assert registration["sourceType"] == ("git" if marker["channel"] == "release" else "local")
    assert registration["source"] == marker["source"]
    doctor = _run_doctor(env, source)
    assert json.loads(doctor.stdout)["harness_surface"]["codex_plugin"]["status"] == "fresh"
    cache_root = Path(env["CODEX_HOME"]) / "plugins/cache"
    assert [path.name for path in cache_root.iterdir() if path.is_dir()] == ["footnote"]


def test_setup_wizard_adapter_uses_verified_release_convergence(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    from fno.setup.integration import build_adapters, run_cli_integration

    source = _source_fixture(tmp_path)
    state_path = tmp_path / "state.json"
    _write_state(state_path, marketplaces=[], plugins=[])
    env = _environment(tmp_path, source, state_path)
    for key, value in env.items():
        monkeypatch.setenv(key, value)
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
    assert [row["pluginId"] for row in state["plugins"]] == ["fno@footnote"]
    calls = [json.loads(line) for line in Path(env["FNO_TEST_CODEX_CALLS"]).read_text().splitlines()]
    assert any(not call["live"] for call in calls)
    assert any(call["live"] for call in calls)
