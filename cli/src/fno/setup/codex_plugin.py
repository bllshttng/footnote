"""Deterministic Codex plugin channel convergence."""
from __future__ import annotations

import fcntl
import json
import os
import subprocess
import uuid
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterator

import fno.paths as paths

RELEASE_MARKETPLACE = "footnote"
RELEASE_SOURCE = "bllshttng/footnote"
RELEASE_PLUGIN_ID = "fno@footnote"
DEV_MARKETPLACE = "footnote-dev"
DEV_PLUGIN_ID = "fno@footnote-dev"

Runner = Callable[..., subprocess.CompletedProcess[str]]
_OUTPUT_LIMIT = 500


@dataclass(frozen=True)
class Marketplace:
    name: str
    source_type: str
    source: str


@dataclass(frozen=True)
class Plugin:
    plugin_id: str
    marketplace_name: str
    version: str
    installed: bool
    enabled: bool
    source_type: str
    marketplace_source: str


@dataclass(frozen=True)
class CodexState:
    marketplaces: tuple[Marketplace, ...]
    plugins: tuple[Plugin, ...]


@dataclass(frozen=True)
class ConvergenceResult:
    channel: str
    action: str
    plugin_id: str
    version: str


class CodexPluginError(RuntimeError):
    def __init__(self, stage: str, detail: str) -> None:
        self.stage = stage
        self.detail = (detail.strip() or "no output")[-_OUTPUT_LIMIT:]
        super().__init__(f"{stage}: {self.detail}")


def _object(raw: object, stage: str) -> dict[str, object]:
    if not isinstance(raw, dict):
        raise CodexPluginError(stage, "expected a JSON object")
    return raw


def _source(raw: object) -> tuple[str, str]:
    if not isinstance(raw, dict):
        return "", ""
    return str(raw.get("sourceType", "")), str(raw.get("source", ""))


def parse_state(marketplaces_json: str, plugins_json: str) -> CodexState:
    try:
        marketplace_doc = _object(json.loads(marketplaces_json), "marketplace-list")
    except (ValueError, TypeError) as exc:
        raise CodexPluginError("marketplace-list", f"malformed JSON: {exc}") from exc
    try:
        plugin_doc = _object(json.loads(plugins_json), "plugin-list")
    except (ValueError, TypeError) as exc:
        raise CodexPluginError("plugin-list", f"malformed JSON: {exc}") from exc
    marketplace_rows = marketplace_doc.get("marketplaces")
    plugin_rows = plugin_doc.get("installed")
    if not isinstance(marketplace_rows, list):
        raise CodexPluginError("marketplace-list", "missing marketplaces array")
    if not isinstance(plugin_rows, list):
        raise CodexPluginError("plugin-list", "missing installed array")

    marketplaces: list[Marketplace] = []
    for row in marketplace_rows:
        item = _object(row, "marketplace-list")
        name = item.get("name")
        if not isinstance(name, str) or not name:
            raise CodexPluginError("marketplace-list", "marketplace missing name")
        source_type, source = _source(item.get("marketplaceSource"))
        marketplaces.append(Marketplace(name, source_type, source))

    plugins: list[Plugin] = []
    for row in plugin_rows:
        item = _object(row, "plugin-list")
        plugin_id = item.get("pluginId")
        if not isinstance(plugin_id, str) or not plugin_id:
            raise CodexPluginError("plugin-list", "installed plugin missing pluginId")
        source_type, source = _source(item.get("marketplaceSource"))
        plugins.append(
            Plugin(
                plugin_id=plugin_id,
                marketplace_name=str(item.get("marketplaceName", "")),
                version=str(item.get("version", "")),
                installed=item.get("installed") is True,
                enabled=item.get("enabled") is True,
                source_type=source_type,
                marketplace_source=source,
            )
        )
    return CodexState(tuple(marketplaces), tuple(plugins))


def resolve_codex_home(explicit: Path | None = None) -> Path:
    if explicit is not None:
        return explicit.expanduser()
    configured = os.environ.get("CODEX_HOME")
    return Path(configured).expanduser() if configured else Path.home() / ".codex"


def _default_runner(argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            argv,
            capture_output=True,
            text=True,
            errors="replace",
            check=False,
            **kwargs,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return subprocess.CompletedProcess(argv, 1, "", str(exc))


def _run(
    runner: Runner,
    stage: str,
    argv: list[str],
    *,
    cwd: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    kwargs: dict[str, object] = {"timeout": 120}
    if cwd is not None:
        kwargs["cwd"] = cwd
    try:
        result = runner(argv, **kwargs)
    except Exception as exc:  # noqa: BLE001 - adapters must fail as a named stage
        raise CodexPluginError(stage, str(exc)) from exc
    if result.returncode != 0:
        raise CodexPluginError(stage, result.stderr or result.stdout or f"exit {result.returncode}")
    return result


def _collect(runner: Runner) -> CodexState:
    marketplaces = _run(
        runner,
        "marketplace-list",
        ["codex", "plugin", "marketplace", "list", "--json"],
    )
    plugins = _run(runner, "plugin-list", ["codex", "plugin", "list", "--json"])
    return parse_state(marketplaces.stdout, plugins.stdout)


def _same_release_source(source_type: str, source: str) -> bool:
    if source_type != "git":
        return False
    normalized = source.removesuffix(".git").removeprefix("https://github.com/")
    return normalized == RELEASE_SOURCE


def _same_local_source(source_type: str, source: str, expected: Path) -> bool:
    if source_type != "local":
        return False
    try:
        return Path(source).expanduser().resolve() == expected.resolve()
    except OSError:
        return False


def _write_marker(
    home: Path, *, channel: str, marketplace: str, source: str
) -> Path:
    owned = home / "footnote"
    owned.mkdir(parents=True, exist_ok=True)
    marker = owned / "plugin-channel.json"
    temp = owned / f".{marker.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp"
    payload = {
        "channel": channel,
        "marketplace": marketplace,
        "source": source,
    }
    try:
        temp.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(temp, marker)
    except OSError as exc:
        try:
            temp.unlink(missing_ok=True)
        except OSError:
            pass
        raise CodexPluginError("desired-channel-marker", str(exc)) from exc
    return marker


@contextmanager
def _convergence_lock(home: Path) -> Iterator[None]:
    owned = home / "footnote"
    try:
        owned.mkdir(parents=True, exist_ok=True)
        with (owned / "plugin-channel.lock").open("a+") as lock:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            yield
    except OSError as exc:
        raise CodexPluginError("convergence-lock", str(exc)) from exc


def _canonical_source_root() -> Path:
    return paths.resolve_plugin_script_durable(".codex-plugin/plugin.json").parents[1]


def _manifest_version(source_root: Path) -> str:
    try:
        payload = json.loads(
            (source_root / ".codex-plugin" / "plugin.json").read_text(encoding="utf-8")
        )
        version = payload["version"]
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise CodexPluginError("plugin-manifest", str(exc)) from exc
    if not isinstance(version, str) or not version:
        raise CodexPluginError("plugin-manifest", "missing version")
    return version


def converge(
    *,
    channel: str,
    refresh: bool = False,
    runner: Runner = _default_runner,
    codex_home: Path | None = None,
    source_root: Path | None = None,
) -> ConvergenceResult:
    if channel not in {"release", "dev"}:
        raise CodexPluginError("channel", f"unsupported channel: {channel}")
    home = resolve_codex_home(codex_home)
    source = source_root or _canonical_source_root()
    version = _manifest_version(source)
    if channel == "release":
        marketplace_name = RELEASE_MARKETPLACE
        marketplace_source = RELEASE_SOURCE
        plugin_id = RELEASE_PLUGIN_ID
        source_matches = _same_release_source
    else:
        marketplace_name = DEV_MARKETPLACE
        dev_marketplace = source / ".agents" / "marketplaces" / DEV_MARKETPLACE
        manifest = dev_marketplace / ".agents" / "plugins" / "marketplace.json"
        if not manifest.is_file():
            raise CodexPluginError("dev-marketplace", f"missing {manifest}")
        marketplace_source = str(dev_marketplace.resolve())
        plugin_id = DEV_PLUGIN_ID

        def source_matches(source_type: str, installed_source: str) -> bool:
            return _same_local_source(source_type, installed_source, dev_marketplace)

    with _convergence_lock(home):
        state = _collect(runner)
        selected = next((p for p in state.plugins if p.plugin_id == plugin_id), None)
        footnote_plugins = [
            plugin
            for plugin in state.plugins
            if plugin.installed and plugin.plugin_id.startswith("fno@")
        ]
        marketplace = next(
            (item for item in state.marketplaces if item.name == marketplace_name), None
        )
        marketplace_ok = marketplace is not None and source_matches(
            marketplace.source_type, marketplace.source
        )
        selected_ok = (
            selected is not None
            and selected.installed
            and selected.enabled
            and source_matches(selected.source_type, selected.marketplace_source)
        )
        only_selected = (
            len(footnote_plugins) == 1
            and footnote_plugins[0].plugin_id == plugin_id
        )
        _write_marker(
            home,
            channel=channel,
            marketplace=marketplace_name,
            source=marketplace_source,
        )
        if marketplace_ok and selected_ok and only_selected and not refresh:
            return ConvergenceResult(channel, "no-op", plugin_id, selected.version)

        changed = False
        for plugin in sorted(
            footnote_plugins,
            key=lambda item: item.plugin_id == plugin_id,
        ):
            remove_selected = plugin.plugin_id == plugin_id and (
                refresh or not selected_ok or not marketplace_ok
            )
            if plugin.plugin_id != plugin_id or remove_selected:
                _run(
                    runner,
                    "plugin-remove",
                    ["codex", "plugin", "remove", plugin.plugin_id, "--json"],
                )
                changed = True

        needs_install = selected is None or refresh or not selected_ok or not marketplace_ok
        if needs_install:
            if channel == "release":
                _run(
                    runner,
                    "release-version-check",
                    [str(source / "scripts" / "release" / "sync-version.sh"), "--check"],
                    cwd=source,
                )
            if marketplace is not None and not marketplace_ok:
                _run(
                    runner,
                    "marketplace-remove",
                    ["codex", "plugin", "marketplace", "remove", marketplace_name, "--json"],
                )
            if not marketplace_ok:
                _run(
                    runner,
                    "marketplace-add",
                    ["codex", "plugin", "marketplace", "add", marketplace_source, "--json"],
                )
            if channel == "release":
                _run(
                    runner,
                    "marketplace-upgrade",
                    ["codex", "plugin", "marketplace", "upgrade", marketplace_name, "--json"],
                )
            _run(
                runner,
                "plugin-add",
                ["codex", "plugin", "add", plugin_id, "--json"],
            )
            changed = True

        final_result = _run(runner, "final-plugin-list", ["codex", "plugin", "list", "--json"])
        final = parse_state('{"marketplaces": []}', final_result.stdout)
        enabled = [
            plugin
            for plugin in final.plugins
            if plugin.installed and plugin.enabled and plugin.plugin_id.startswith("fno@")
        ]
        if (
            len(enabled) != 1
            or enabled[0].plugin_id != plugin_id
            or not source_matches(enabled[0].source_type, enabled[0].marketplace_source)
        ):
            found = ", ".join(plugin.plugin_id for plugin in enabled) or "none"
            raise CodexPluginError("final-verify", f"expected {plugin_id}; enabled={found}")
        action = "refreshed" if refresh else ("installed" if selected is None else "repaired")
        if changed and selected is not None and not refresh:
            action = "repaired"
        return ConvergenceResult(
            channel, action, enabled[0].plugin_id, enabled[0].version or version
        )
