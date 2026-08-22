"""Deterministic Codex plugin channel convergence."""

from __future__ import annotations

import copy
import fcntl
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import tomllib
import uuid
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterator

import fno.paths as paths

MARKETPLACE = "footnote"
PLUGIN_ID = "fno@footnote"
RELEASE_MARKETPLACE = MARKETPLACE
RELEASE_SOURCE = "bllshttng/footnote"
RELEASE_PLUGIN_ID = PLUGIN_ID
DEV_MARKETPLACE = MARKETPLACE
DEV_PLUGIN_ID = PLUGIN_ID
LEGACY_DEV_MARKETPLACE = "footnote-dev"
LEGACY_DEV_PLUGIN_ID = "fno@footnote-dev"
LEGACY_PLUGIN_ID = "fno@footnote-local"
OWNED_PLUGIN_IDS = frozenset({PLUGIN_ID, LEGACY_DEV_PLUGIN_ID, LEGACY_PLUGIN_ID})

Runner = Callable[..., subprocess.CompletedProcess[str]]
_OUTPUT_LIMIT = 500


@dataclass(frozen=True)
class Marketplace:
    name: str
    source_type: str
    source: str
    root: str


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


@dataclass(frozen=True)
class _Snapshot:
    state: CodexState
    marker: bytes | None
    rollback_receipt: bytes | None
    config: _OwnedConfig
    cache_names: frozenset[str]


@dataclass(frozen=True)
class _OwnedConfig:
    marketplaces: dict[str, object]
    plugins: dict[str, object]
    document: dict[str, object]
    payload: bytes | None


@dataclass(frozen=True)
class _ValidatedCandidate:
    version: str
    payload_digest: str


@dataclass(frozen=True)
class _CacheQuarantine:
    original: Path
    quarantine: Path


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
        marketplaces.append(Marketplace(name, source_type, source, str(item.get("root", ""))))

    plugins: list[Plugin] = []
    for row in plugin_rows:
        item = _object(row, "plugin-list")
        plugin_id = item.get("pluginId")
        if not isinstance(plugin_id, str) or not plugin_id:
            raise CodexPluginError("plugin-list", "installed plugin missing pluginId")
        source_type, source = _source(item.get("marketplaceSource"))
        installed = item.get("installed")
        enabled = item.get("enabled")
        if not isinstance(installed, bool) or not isinstance(enabled, bool):
            raise CodexPluginError("plugin-list", "installed and enabled must be booleans")
        if enabled and not installed:
            raise CodexPluginError("plugin-list", "enabled plugin is not installed")
        plugins.append(
            Plugin(
                plugin_id=plugin_id,
                marketplace_name=str(item.get("marketplaceName", "")),
                version=str(item.get("version", "")),
                installed=installed,
                enabled=enabled,
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


def _default_runner(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
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
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    kwargs: dict[str, object] = {"timeout": 120}
    if cwd is not None:
        kwargs["cwd"] = cwd
    if env is not None:
        kwargs["env"] = env
    try:
        result = runner(argv, **kwargs)
    except Exception as exc:  # noqa: BLE001 - adapters must fail as a named stage
        raise CodexPluginError(stage, str(exc)) from exc
    if result.returncode != 0:
        raise CodexPluginError(stage, result.stderr or result.stdout or f"exit {result.returncode}")
    return result


def _collect(runner: Runner, *, env: dict[str, str] | None = None) -> CodexState:
    marketplaces = _run(
        runner,
        "marketplace-list",
        ["codex", "plugin", "marketplace", "list", "--json"],
        env=env,
    )
    plugins = _run(
        runner,
        "plugin-list",
        ["codex", "plugin", "list", "--json"],
        env=env,
    )
    return parse_state(marketplaces.stdout, plugins.stdout)


def _validate_candidate(runner: Runner, *, source: str) -> _ValidatedCandidate:
    """Prove Codex can parse and install a candidate without touching live state."""
    with tempfile.TemporaryDirectory(prefix="fno-codex-candidate-") as raw_home:
        candidate_home = Path(raw_home)
        env = {**os.environ, "CODEX_HOME": str(candidate_home)}
        _run(
            runner,
            "candidate-marketplace-add",
            ["codex", "plugin", "marketplace", "add", source, "--json"],
            env=env,
        )
        available_result = _run(
            runner,
            "candidate-plugin-list",
            [
                "codex",
                "plugin",
                "list",
                "--available",
                "--marketplace",
                MARKETPLACE,
                "--json",
            ],
            env=env,
        )
        try:
            available_doc = _object(
                json.loads(available_result.stdout), "candidate-plugin-list"
            )
        except (ValueError, TypeError) as exc:
            raise CodexPluginError(
                "candidate-plugin-list", f"malformed JSON: {exc}"
            ) from exc
        rows = available_doc.get("available")
        if not isinstance(rows, list):
            raise CodexPluginError("candidate-plugin-list", "missing available array")
        matches = [
            row
            for row in rows
            if isinstance(row, dict) and row.get("pluginId") == PLUGIN_ID
        ]
        if len(matches) != 1 or not matches[0].get("version"):
            raise CodexPluginError(
                "candidate-plugin-list",
                f"expected one available {PLUGIN_ID}; found {len(matches)}",
            )
        _run(
            runner,
            "candidate-plugin-add",
            ["codex", "plugin", "add", PLUGIN_ID, "--json"],
            env=env,
        )
        state = _collect(runner, env=env)
        selected = [
            plugin
            for plugin in state.plugins
            if plugin.installed and plugin.enabled and plugin.plugin_id == PLUGIN_ID
        ]
        marketplace = next(
            (item for item in state.marketplaces if item.name == MARKETPLACE), None
        )
        source_matches = (
            _same_release_source
            if source == RELEASE_SOURCE
            else lambda source_type, installed_source: _same_local_source(
                source_type, installed_source, Path(source)
            )
        )
        if (
            len(selected) != 1
            or marketplace is None
            or not marketplace.root
            or not source_matches(marketplace.source_type, marketplace.source)
            or not source_matches(
                selected[0].source_type, selected[0].marketplace_source
            )
        ):
            raise CodexPluginError(
                "candidate-final-verify", f"expected one enabled {PLUGIN_ID}"
            )
        cache = (
            candidate_home
            / "plugins"
            / "cache"
            / MARKETPLACE
            / "fno"
            / selected[0].version
        )
        root = Path(marketplace.root)
        source_digest = plugin_payload_digest(root)
        if not cache.is_dir() or source_digest != plugin_payload_digest(cache):
            raise CodexPluginError(
                "candidate-final-verify", "candidate cache payload differs from source"
            )
        return _ValidatedCandidate(selected[0].version, source_digest)


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


def _write_marker(home: Path, *, channel: str, marketplace: str, source: str) -> Path:
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


def _marker_bytes(home: Path) -> bytes | None:
    try:
        return (home / "footnote" / "plugin-channel.json").read_bytes()
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise CodexPluginError("desired-channel-marker", str(exc)) from exc


def _restore_marker(home: Path, payload: bytes | None) -> None:
    marker = home / "footnote" / "plugin-channel.json"
    temp: Path | None = None
    try:
        if payload is None:
            marker.unlink(missing_ok=True)
            return
        marker.parent.mkdir(parents=True, exist_ok=True)
        temp = marker.parent / f".{marker.name}.rollback.{uuid.uuid4().hex}.tmp"
        temp.write_bytes(payload)
        os.replace(temp, marker)
    except OSError as exc:
        try:
            if temp is not None:
                temp.unlink(missing_ok=True)
        except OSError:
            pass
        raise CodexPluginError("rollback-marker", str(exc)) from exc


def _rollback_receipt(
    home: Path, original: CodexPluginError, detail: str, *, channel: str
) -> None:
    path = home / "footnote" / "rollback-failure.json"
    temp: Path | None = None
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        temp = path.parent / f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp"
        temp.write_text(
            json.dumps(
                {
                    "channel": channel,
                    "stage": original.stage,
                    "detail": detail[-_OUTPUT_LIMIT:],
                },
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        os.replace(temp, path)
    except OSError as exc:
        try:
            if temp is not None:
                temp.unlink(missing_ok=True)
        except OSError:
            pass
        raise CodexPluginError("rollback-receipt", str(exc)) from exc


def _clear_rollback_receipt(home: Path) -> None:
    try:
        (home / "footnote" / "rollback-failure.json").unlink(missing_ok=True)
    except OSError as exc:
        raise CodexPluginError("rollback-receipt-clear", str(exc)) from exc


def _rollback_receipt_bytes(home: Path) -> bytes | None:
    path = home / "footnote" / "rollback-failure.json"
    try:
        return path.read_bytes()
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise CodexPluginError("rollback-receipt-snapshot", str(exc)) from exc


def _restore_rollback_receipt(home: Path, payload: bytes | None) -> None:
    path = home / "footnote" / "rollback-failure.json"
    temp: Path | None = None
    try:
        if payload is None:
            path.unlink(missing_ok=True)
            return
        path.parent.mkdir(parents=True, exist_ok=True)
        temp = path.parent / f".{path.name}.rollback.{uuid.uuid4().hex}.tmp"
        temp.write_bytes(payload)
        os.replace(temp, path)
    except OSError as exc:
        try:
            if temp is not None:
                temp.unlink(missing_ok=True)
        except OSError:
            pass
        raise CodexPluginError("rollback-receipt-restore", str(exc)) from exc


def _read_owned_config(home: Path) -> _OwnedConfig:
    path = home / "config.toml"
    try:
        payload = path.read_bytes() if path.is_file() else None
        document = tomllib.loads(payload.decode("utf-8")) if payload is not None else {}
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        raise CodexPluginError("codex-config-snapshot", str(exc)) from exc
    marketplaces = document.get("marketplaces", {})
    plugins = document.get("plugins", {})
    if not isinstance(marketplaces, dict) or not isinstance(plugins, dict):
        raise CodexPluginError("codex-config-snapshot", "owned tables are not objects")
    return _OwnedConfig(
        marketplaces=copy.deepcopy(
            {
                key: value
                for key, value in marketplaces.items()
                if key in {MARKETPLACE, LEGACY_DEV_MARKETPLACE}
            }
        ),
        plugins=copy.deepcopy(
            {key: value for key, value in plugins.items() if key in OWNED_PLUGIN_IDS}
        ),
        document=copy.deepcopy(document),
        payload=payload,
    )


def _without_owned_config(document: dict[str, object]) -> dict[str, object]:
    result = copy.deepcopy(document)
    marketplaces = result.get("marketplaces")
    if isinstance(marketplaces, dict):
        for key in {MARKETPLACE, LEGACY_DEV_MARKETPLACE}:
            marketplaces.pop(key, None)
        if not marketplaces:
            result.pop("marketplaces", None)
    plugins = result.get("plugins")
    if isinstance(plugins, dict):
        for key in OWNED_PLUGIN_IDS:
            plugins.pop(key, None)
        if not plugins:
            result.pop("plugins", None)
    return result


def _restore_owned_config(home: Path, snapshot: _OwnedConfig) -> None:
    path = home / "config.toml"
    temp: Path | None = None
    try:
        current_payload = path.read_bytes() if path.is_file() else None
        document = (
            tomllib.loads(current_payload.decode("utf-8"))
            if current_payload is not None
            else {}
        )
        marketplaces = document.get("marketplaces", {})
        plugins = document.get("plugins", {})
        if not isinstance(marketplaces, dict) or not isinstance(plugins, dict):
            raise TypeError("owned tables are not objects")
        current_marketplaces = {
            key: value
            for key, value in marketplaces.items()
            if key in {MARKETPLACE, LEGACY_DEV_MARKETPLACE}
        }
        current_plugins = {
            key: value
            for key, value in plugins.items()
            if key in OWNED_PLUGIN_IDS
        }
        if (
            current_marketplaces == snapshot.marketplaces
            and current_plugins == snapshot.plugins
        ):
            return
        if _without_owned_config(document) != _without_owned_config(snapshot.document):
            raise CodexPluginError(
                "rollback-config", "non-Footnote config changed during convergence"
            )
        if snapshot.payload is None:
            path.unlink(missing_ok=True)
            return
        path.parent.mkdir(parents=True, exist_ok=True)
        temp = path.parent / f".{path.name}.rollback.{uuid.uuid4().hex}.tmp"
        temp.write_bytes(snapshot.payload)
        os.replace(temp, path)
    except (OSError, UnicodeError, TypeError, tomllib.TOMLDecodeError) as exc:
        try:
            if temp is not None:
                temp.unlink(missing_ok=True)
        except OSError:
            pass
        raise CodexPluginError("rollback-config", str(exc)) from exc


def _quarantine_cache(home: Path, marketplace: str) -> _CacheQuarantine | None:
    cache = home / "plugins" / "cache" / marketplace
    if cache.is_symlink():
        raise CodexPluginError("cache-quarantine", f"refusing non-directory {cache}")
    if not cache.exists():
        return None
    if not cache.is_dir():
        raise CodexPluginError("cache-quarantine", f"refusing non-directory {cache}")
    quarantine = home / "footnote" / f".{marketplace}.{uuid.uuid4().hex}"
    try:
        os.replace(cache, quarantine)
    except OSError as exc:
        raise CodexPluginError("cache-quarantine", str(exc)) from exc
    return _CacheQuarantine(cache, quarantine)


def _restore_cache(item: _CacheQuarantine) -> None:
    if not item.quarantine.exists():
        return
    try:
        if item.original.exists():
            shutil.rmtree(item.original)
        os.replace(item.quarantine, item.original)
    except OSError as exc:
        raise CodexPluginError("rollback-cache", str(exc)) from exc


def _discard_cache_backup_best_effort(item: _CacheQuarantine) -> None:
    try:
        shutil.rmtree(item.quarantine)
    except FileNotFoundError:
        pass
    except OSError:
        pass


def _owned_marketplace(marketplace: Marketplace) -> bool:
    return marketplace.name in {MARKETPLACE, LEGACY_DEV_MARKETPLACE}


def _normalized_source(source_type: str, source: str) -> tuple[str, str]:
    if _same_release_source(source_type, source):
        return "git", RELEASE_SOURCE
    if source_type == "local":
        try:
            return "local", str(Path(source).expanduser().resolve())
        except OSError:
            pass
    return source_type, source


def _owned_state_fingerprint(state: CodexState) -> tuple[tuple[object, ...], ...]:
    marketplaces = sorted(
        ("marketplace", item.name, *_normalized_source(item.source_type, item.source))
        for item in state.marketplaces
        if _owned_marketplace(item)
    )
    plugins = sorted(
        (
            "plugin",
            item.plugin_id,
            item.marketplace_name,
            item.version,
            item.installed,
            item.enabled,
            *_normalized_source(item.source_type, item.marketplace_source),
        )
        for item in state.plugins
        if item.plugin_id in OWNED_PLUGIN_IDS
    )
    return tuple([*marketplaces, *plugins])


def _restore_snapshot(
    runner: Runner,
    home: Path,
    snapshot: _Snapshot,
    cache_quarantines: tuple[_CacheQuarantine, ...],
    *,
    env: dict[str, str],
) -> None:
    current = _collect(runner, env=env)
    expected = _owned_state_fingerprint(snapshot.state)
    if _owned_state_fingerprint(current) == expected:
        if snapshot.config.marketplaces or snapshot.config.plugins:
            _restore_owned_config(home, snapshot.config)
        for item in cache_quarantines:
            _restore_cache(item)
        _restore_marker(home, snapshot.marker)
        _restore_rollback_receipt(home, snapshot.rollback_receipt)
        restored = _collect(runner, env=env)
        if _owned_state_fingerprint(restored) != expected:
            raise CodexPluginError(
                "rollback-final-verify", "owned state changed during rollback"
            )
        if _marker_bytes(home) != snapshot.marker:
            raise CodexPluginError("rollback-final-verify", "marker bytes differ")
        if _rollback_receipt_bytes(home) != snapshot.rollback_receipt:
            raise CodexPluginError("rollback-final-verify", "rollback receipt bytes differ")
        return
    for plugin in current.plugins:
        if plugin.installed and plugin.plugin_id in OWNED_PLUGIN_IDS:
            _run(
                runner,
                "rollback-plugin-remove",
                ["codex", "plugin", "remove", plugin.plugin_id, "--json"],
                env=env,
            )
    current = _collect(runner, env=env)
    for marketplace in current.marketplaces:
        if _owned_marketplace(marketplace):
            _run(
                runner,
                "rollback-marketplace-remove",
                [
                    "codex",
                    "plugin",
                    "marketplace",
                    "remove",
                    marketplace.name,
                    "--json",
                ],
                env=env,
            )
    for marketplace in snapshot.state.marketplaces:
        if _owned_marketplace(marketplace):
            _run(
                runner,
                "rollback-marketplace-add",
                [
                    "codex",
                    "plugin",
                    "marketplace",
                    "add",
                    marketplace.source,
                    "--json",
                ],
                env=env,
            )
    config_restore_needed = False
    legacy_restore_error: CodexPluginError | None = None
    for plugin in snapshot.state.plugins:
        if plugin.installed and plugin.plugin_id in OWNED_PLUGIN_IDS:
            try:
                _run(
                    runner,
                    "rollback-plugin-add",
                    ["codex", "plugin", "add", plugin.plugin_id, "--json"],
                    env=env,
                )
            except CodexPluginError as exc:
                if plugin.plugin_id != LEGACY_DEV_PLUGIN_ID:
                    raise
                config_restore_needed = True
                legacy_restore_error = exc
    if (
        snapshot.config.marketplaces
        or snapshot.config.plugins
        or config_restore_needed
    ):
        _restore_owned_config(home, snapshot.config)
    for cache_name in {MARKETPLACE, LEGACY_DEV_MARKETPLACE} - snapshot.cache_names:
        introduced_cache = home / "plugins" / "cache" / cache_name
        if introduced_cache.is_symlink():
            raise CodexPluginError(
                "rollback-cache", f"refusing non-directory {introduced_cache}"
            )
        if not introduced_cache.exists():
            continue
        if not introduced_cache.is_dir():
            raise CodexPluginError(
                "rollback-cache", f"refusing non-directory {introduced_cache}"
            )
        try:
            shutil.rmtree(introduced_cache)
        except OSError as exc:
            raise CodexPluginError("rollback-cache", str(exc)) from exc
    for item in cache_quarantines:
        _restore_cache(item)
    _restore_marker(home, snapshot.marker)
    _restore_rollback_receipt(home, snapshot.rollback_receipt)
    restored = _collect(runner, env=env)
    found = _owned_state_fingerprint(restored)
    if found != expected:
        detail = f"expected state={expected}; found={found}"
        if legacy_restore_error is not None:
            detail = f"{detail}; legacy plugin restore failed: {legacy_restore_error}"
        raise CodexPluginError(
            "rollback-final-verify", detail
        )
    if _marker_bytes(home) != snapshot.marker:
        raise CodexPluginError("rollback-final-verify", "marker bytes differ")
    if _rollback_receipt_bytes(home) != snapshot.rollback_receipt:
        raise CodexPluginError("rollback-final-verify", "rollback receipt bytes differ")


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


def plugin_payload_digest(plugin_root: Path) -> str:
    """Hash only files Codex can load from the Footnote plugin payload."""
    candidates = [plugin_root / ".codex-plugin" / "plugin.json"]
    for relative in (
        "skills",
        "agents",
        "commands",
        "hooks",
        "scripts",
        ".codex/agents",
    ):
        base = plugin_root / relative
        if base.is_dir():
            candidates.extend(
                path
                for path in base.rglob("*")
                if path.is_file()
                and "__pycache__" not in path.parts
                and path.suffix not in {".pyc", ".pyo"}
                and path.name != ".DS_Store"
            )
    digest = hashlib.sha256()
    candidates = [path for path in candidates if path.is_file()]
    for path in sorted(candidates, key=lambda item: item.relative_to(plugin_root).as_posix()):
        relative_bytes = path.relative_to(plugin_root).as_posix().encode()
        try:
            payload = path.read_bytes()
        except OSError as exc:
            raise CodexPluginError("plugin-digest", str(exc)) from exc
        digest.update(len(relative_bytes).to_bytes(8, "big"))
        digest.update(relative_bytes)
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
    return digest.hexdigest()


def inspect_freshness(
    *,
    runner: Runner = _default_runner,
    codex_home: Path | None = None,
    source_root: Path | None = None,
) -> dict[str, object]:
    """Return an advisory plugin verdict independent of CLI freshness."""
    home = resolve_codex_home(codex_home)
    live_env = {**os.environ, "CODEX_HOME": str(home)}
    marker_path = home / "footnote" / "plugin-channel.json"
    rollback_path = home / "footnote" / "rollback-failure.json"
    remedy_channel = "dev"
    if rollback_path.is_file():
        try:
            rollback = _object(
                json.loads(rollback_path.read_text(encoding="utf-8")),
                "rollback-failure",
            )
            detail = str(rollback.get("detail", "rollback did not complete"))
            rollback_channel = rollback.get("channel")
            if rollback_channel not in {"release", "dev"}:
                rollback_channel = "release"
        except (OSError, ValueError, TypeError, CodexPluginError) as exc:
            detail = str(exc)
            rollback_channel = "release"
        return {
            "status": "error",
            "issue": "rollback-failure",
            "detail": detail[-_OUTPUT_LIMIT:],
            "remedy": f"fno config setup codex-plugin --channel {rollback_channel} --refresh",
        }
    marker: dict[str, object] | None = None
    marker_error: Exception | None = None
    try:
        marker = _object(
            json.loads(marker_path.read_text(encoding="utf-8")), "desired-channel-marker"
        )
        channel = marker.get("channel")
        if channel not in {"release", "dev"}:
            raise CodexPluginError("desired-channel-marker", "missing or unsupported channel")
        remedy_channel = str(channel)
        expected_marketplace = MARKETPLACE
        expected_source = str(marker.get("source", ""))
        marker_source_ok = (
            _same_release_source("git", expected_source)
            if channel == "release"
            else bool(expected_source)
        )
        if marker.get("marketplace") != expected_marketplace or not marker_source_ok:
            raise CodexPluginError(
                "desired-channel-marker", "channel contract does not match marker"
            )
    except (FileNotFoundError, OSError, ValueError, TypeError, CodexPluginError) as exc:
        marker_error = exc
    try:
        state = _collect(runner, env=live_env)
    except CodexPluginError as exc:
        detail = str(exc)
        return {
            "status": "unknown",
            "issue": "state-unreadable",
            "detail": detail[-_OUTPUT_LIMIT:],
            "remedy": f"fno config setup codex-plugin --channel {remedy_channel} --refresh",
        }

    installed = [p for p in state.plugins if p.installed and p.plugin_id in OWNED_PLUGIN_IDS]
    enabled = [p for p in installed if p.enabled]
    owned_marketplaces = [
        marketplace for marketplace in state.marketplaces if _owned_marketplace(marketplace)
    ]
    ambiguous = (
        len(installed) > 1
        or len(enabled) > 1
        or any(plugin.plugin_id != PLUGIN_ID for plugin in installed)
        or len(owned_marketplaces) > 1
        or any(
            marketplace.name == LEGACY_DEV_MARKETPLACE
            for marketplace in owned_marketplaces
        )
    )
    if ambiguous:
        conflict_channel = str(marker.get("channel", "unknown")) if marker else "unknown"
        return {
            "channel": conflict_channel,
            "installed_plugin_ids": [p.plugin_id for p in installed],
            "enabled_plugin_ids": [p.plugin_id for p in enabled],
            "status": "conflict",
            "issue": "ambiguous-duplicate-state",
            "remedy": f"fno config setup codex-plugin --channel {remedy_channel} --refresh",
        }
    if marker_error is not None:
        if isinstance(marker_error, FileNotFoundError):
            return {
                "status": "unknown",
                "issue": "desired-channel-missing",
                "remedy": "fno config setup codex-plugin --channel release",
            }
        return {
            "status": "unknown",
            "issue": "state-unreadable",
            "detail": str(marker_error)[-_OUTPUT_LIMIT:],
            "remedy": f"fno config setup codex-plugin --channel {remedy_channel} --refresh",
        }
    assert marker is not None
    channel = str(marker["channel"])
    expected_marketplace = MARKETPLACE
    expected_plugin = PLUGIN_ID
    expected_source = str(marker["source"])
    base: dict[str, object] = {
        "channel": channel,
        "marketplace": expected_marketplace,
        "marketplace_source": expected_source,
        "installed_plugin_ids": [p.plugin_id for p in installed],
        "enabled_plugin_ids": [p.plugin_id for p in enabled],
        "remedy": f"fno config setup codex-plugin --channel {channel} --refresh",
    }
    if not enabled:
        return {**base, "status": "missing", "issue": "plugin-missing"}
    selected = enabled[0]
    if selected.plugin_id != expected_plugin or selected.marketplace_name != expected_marketplace:
        return {**base, "status": "wrong-channel", "issue": "wrong-channel"}

    marketplace = next(
        (item for item in state.marketplaces if item.name == expected_marketplace), None
    )
    source_ok = (
        _same_release_source(selected.source_type, selected.marketplace_source)
        if channel == "release"
        else _same_local_source(
            selected.source_type, selected.marketplace_source, Path(expected_source)
        )
    )
    marketplace_ok = marketplace is not None and (
        _same_release_source(marketplace.source_type, marketplace.source)
        if channel == "release"
        else _same_local_source(marketplace.source_type, marketplace.source, Path(expected_source))
    )
    if not source_ok or not marketplace_ok:
        return {**base, "status": "stale", "issue": "wrong-marketplace-source"}

    canonical = source_root or _canonical_source_root()
    if channel == "release" and marketplace is not None and marketplace.root:
        canonical = Path(marketplace.root)
    try:
        source_version = _manifest_version(canonical)
    except CodexPluginError as exc:
        return {**base, "status": "unknown", "issue": "source-unreadable", "detail": str(exc)}
    cache_version = selected.version
    cache = home / "plugins" / "cache" / expected_marketplace / "fno" / cache_version
    version_fields = {"source_version": source_version, "cache_version": cache_version}
    if source_version != cache_version:
        return {**base, **version_fields, "status": "stale", "issue": "version-mismatch"}
    if not cache.is_dir():
        return {**base, **version_fields, "status": "missing", "issue": "cache-missing"}
    try:
        source_digest = plugin_payload_digest(canonical)
        cache_digest = plugin_payload_digest(cache)
    except CodexPluginError as exc:
        return {
            **base,
            **version_fields,
            "status": "unknown",
            "issue": "digest-unreadable",
            "detail": str(exc),
        }
    digest_fields = {"source_digest": source_digest, "cache_digest": cache_digest}
    if source_digest != cache_digest:
        return {
            **base,
            **version_fields,
            **digest_fields,
            "status": "stale",
            "issue": "payload-drift",
        }
    return {**base, **version_fields, **digest_fields, "status": "fresh", "issue": None}


def converge(
    *,
    channel: str,
    refresh: bool = False,
    runner: Runner = _default_runner,
    codex_home: Path | None = None,
    source_root: Path | None = None,
    validate_candidate: bool | None = None,
) -> ConvergenceResult:
    if channel not in {"release", "dev"}:
        raise CodexPluginError("channel", f"unsupported channel: {channel}")
    home = resolve_codex_home(codex_home)
    live_env = {**os.environ, "CODEX_HOME": str(home)}
    source = source_root or _canonical_source_root()
    source_matches: Callable[[str, str], bool]
    if channel == "release":
        local_manifest = source / ".codex-plugin" / "plugin.json"
        release_check = source / "scripts" / "release" / "sync-version.sh"
        version = (
            _manifest_version(source)
            if local_manifest.is_file() and release_check.is_file()
            else ""
        )
        marketplace_name = RELEASE_MARKETPLACE
        marketplace_source = RELEASE_SOURCE
        plugin_id = RELEASE_PLUGIN_ID
        source_matches = _same_release_source
    else:
        version = _manifest_version(source)
        marketplace_name = DEV_MARKETPLACE
        marketplace_source = str(source.resolve())
        plugin_id = DEV_PLUGIN_ID

        def dev_source_matches(source_type: str, installed_source: str) -> bool:
            return _same_local_source(source_type, installed_source, source)

        source_matches = dev_source_matches

    with _convergence_lock(home):
        state = _collect(runner, env=live_env)
        snapshot = _Snapshot(
            state=state,
            marker=_marker_bytes(home),
            rollback_receipt=_rollback_receipt_bytes(home),
            config=_read_owned_config(home),
            cache_names=frozenset(
                name
                for name in (MARKETPLACE, LEGACY_DEV_MARKETPLACE)
                if (home / "plugins" / "cache" / name).is_dir()
            ),
        )
        selected = next((p for p in state.plugins if p.plugin_id == plugin_id), None)
        footnote_plugins = [
            plugin
            for plugin in state.plugins
            if plugin.installed and plugin.plugin_id in OWNED_PLUGIN_IDS
        ]
        marketplace = next(
            (item for item in state.marketplaces if item.name == marketplace_name), None
        )
        marketplace_ok = marketplace is not None and source_matches(
            marketplace.source_type, marketplace.source
        )
        legacy_marketplaces = [
            item for item in state.marketplaces if item.name == LEGACY_DEV_MARKETPLACE
        ]
        selected_ok = (
            selected is not None
            and selected.installed
            and selected.enabled
            and bool(selected.version)
            and selected.marketplace_name == marketplace_name
            and source_matches(selected.source_type, selected.marketplace_source)
        )
        only_selected = (
            len(footnote_plugins) == 1
            and footnote_plugins[0].plugin_id == plugin_id
            and not legacy_marketplaces
        )
        channel_stable = marketplace_ok and selected_ok and only_selected and not refresh
        marker_matches = False
        if snapshot.marker is not None:
            try:
                marker = _object(
                    json.loads(snapshot.marker.decode("utf-8")),
                    "desired-channel-marker",
                )
                marker_matches = (
                    marker.get("channel") == channel
                    and marker.get("marketplace") == marketplace_name
                    and marker.get("source") == marketplace_source
                )
            except (UnicodeError, ValueError, TypeError, CodexPluginError):
                pass
        legacy_cache = home / "plugins" / "cache" / LEGACY_DEV_MARKETPLACE
        legacy_cache_present = legacy_cache.exists() or legacy_cache.is_symlink()
        no_op = (
            channel_stable
            and marker_matches
            and not legacy_cache_present
            and snapshot.rollback_receipt is None
        )
        if no_op:
            assert selected is not None  # no_op implies channel_stable -> selected_ok
            # ponytail: short-circuit before any preflight. no_op already proves
            # the live one-identity state matches the marker, so the
            # release-version-check and isolated candidate validation would only
            # risk failing an already-converged host (offline / version drift).
            return ConvergenceResult(
                channel, "no-op", selected.plugin_id, selected.version or version
            )
        if channel == "release" and version:
            _run(
                runner,
                "release-version-check",
                [str(release_check), "--check"],
                cwd=source,
                env=live_env,
            )
        if validate_candidate is None:
            validate_candidate = True
        candidate = (
            _validate_candidate(runner, source=marketplace_source)
            if validate_candidate
            else None
        )
        needs_install = selected is None or refresh or not selected_ok or not marketplace_ok
        changed = False
        cache_quarantines: list[_CacheQuarantine] = []
        try:
            if channel == "dev" and candidate is not None:
                if (
                    _manifest_version(source) != candidate.version
                    or plugin_payload_digest(source) != candidate.payload_digest
                ):
                    raise CodexPluginError(
                        "candidate-recheck", "dev source changed after candidate validation"
                    )
            if snapshot.rollback_receipt is not None:
                changed = True
                _clear_rollback_receipt(home)
            for cache_name in (
                LEGACY_DEV_MARKETPLACE,
                *(() if not needs_install else (MARKETPLACE,)),
            ):
                quarantine = _quarantine_cache(home, cache_name)
                if quarantine is None:
                    continue
                changed = True
                cache_quarantines.append(quarantine)
            for plugin in sorted(
                footnote_plugins,
                key=lambda item: item.plugin_id == plugin_id,
            ):
                remove_selected = plugin.plugin_id == plugin_id and (
                    refresh or not selected_ok or not marketplace_ok
                )
                if no_op or (plugin.plugin_id == plugin_id and not remove_selected):
                    continue
                changed = True
                _run(
                    runner,
                    "plugin-remove",
                    ["codex", "plugin", "remove", plugin.plugin_id, "--json"],
                    env=live_env,
                )

            for legacy_marketplace in legacy_marketplaces:
                changed = True
                _run(
                    runner,
                    "legacy-marketplace-remove",
                    [
                        "codex",
                        "plugin",
                        "marketplace",
                        "remove",
                        legacy_marketplace.name,
                        "--json",
                    ],
                    env=live_env,
                )

            if needs_install:
                if marketplace is not None and not marketplace_ok:
                    changed = True
                    _run(
                        runner,
                        "marketplace-remove",
                        [
                            "codex",
                            "plugin",
                            "marketplace",
                            "remove",
                            marketplace_name,
                            "--json",
                        ],
                        env=live_env,
                    )
                if not marketplace_ok:
                    changed = True
                    _run(
                        runner,
                        "marketplace-add",
                        [
                            "codex",
                            "plugin",
                            "marketplace",
                            "add",
                            marketplace_source,
                            "--json",
                        ],
                        env=live_env,
                    )
                if channel == "release":
                    changed = True
                    _run(
                        runner,
                        "marketplace-upgrade",
                        [
                            "codex",
                            "plugin",
                            "marketplace",
                            "upgrade",
                            marketplace_name,
                            "--json",
                        ],
                        env=live_env,
                    )
                changed = True
                _run(
                    runner,
                    "plugin-add",
                    ["codex", "plugin", "add", plugin_id, "--json"],
                    env=live_env,
                )

            final = _collect(runner, env=live_env)
            installed = [
                plugin
                for plugin in final.plugins
                if plugin.installed and plugin.plugin_id in OWNED_PLUGIN_IDS
            ]
            enabled = [
                plugin
                for plugin in installed
                if plugin.enabled
            ]
            final_marketplaces = [
                item for item in final.marketplaces if _owned_marketplace(item)
            ]
            if (
                len(installed) != 1
                or len(enabled) != 1
                or enabled[0].plugin_id != plugin_id
                or enabled[0].marketplace_name != marketplace_name
                or not enabled[0].version
                or not source_matches(
                    enabled[0].source_type, enabled[0].marketplace_source
                )
                or len(final_marketplaces) != 1
                or final_marketplaces[0].name != marketplace_name
                or not source_matches(
                    final_marketplaces[0].source_type, final_marketplaces[0].source
                )
            ):
                found = ", ".join(plugin.plugin_id for plugin in enabled) or "none"
                raise CodexPluginError(
                    "final-verify", f"expected {plugin_id}; enabled={found}"
                )
            if candidate is not None or refresh:
                canonical = (
                    source if channel == "dev" else Path(final_marketplaces[0].root)
                )
                final_version = _manifest_version(canonical)
                cache = (
                    home
                    / "plugins"
                    / "cache"
                    / marketplace_name
                    / "fno"
                    / enabled[0].version
                )
                if not cache.is_dir():
                    raise CodexPluginError(
                        "final-verify", f"cache missing after refresh: {cache}"
                    )
                if enabled[0].version != final_version:
                    raise CodexPluginError(
                        "final-verify",
                        f"refreshed version {enabled[0].version} != source {final_version}",
                    )
                source_digest = plugin_payload_digest(canonical)
                if source_digest != plugin_payload_digest(cache):
                    raise CodexPluginError(
                        "final-verify", "cache payload differs after refresh"
                    )
                if candidate is not None and (
                    final_version != candidate.version
                    or source_digest != candidate.payload_digest
                ):
                    raise CodexPluginError(
                        "final-verify", "live payload differs from validated candidate"
                    )
            changed = True
            _write_marker(
                home,
                channel=channel,
                marketplace=marketplace_name,
                source=marketplace_source,
            )
            written = _object(
                json.loads(
                    (home / "footnote" / "plugin-channel.json").read_text(
                        encoding="utf-8"
                    )
                ),
                "desired-channel-marker",
            )
            if written.get("channel") != channel or written.get("source") != marketplace_source:
                raise CodexPluginError(
                    "desired-channel-marker", "marker readback differs after write"
                )
            for item in cache_quarantines:
                _discard_cache_backup_best_effort(item)
            action = (
                "refreshed"
                if refresh
                else "installed"
                if selected is None
                else "repaired"
            )
            return ConvergenceResult(
                channel, action, enabled[0].plugin_id, enabled[0].version or version
            )
        except BaseException as raw_error:  # noqa: BLE001 - rollback on any abort
            original = (
                raw_error
                if isinstance(raw_error, CodexPluginError)
                else CodexPluginError("transaction", str(raw_error))
            )
            if not changed:
                if isinstance(raw_error, Exception):
                    raise original
                raise
            try:
                _restore_snapshot(
                    runner,
                    home,
                    snapshot,
                    tuple(cache_quarantines),
                    env=live_env,
                )
            except BaseException as raw_rollback:  # noqa: BLE001 - any rollback failure earns a receipt
                rollback_error = (
                    raw_rollback
                    if isinstance(raw_rollback, CodexPluginError)
                    else CodexPluginError("rollback", str(raw_rollback))
                )
                detail = f"{original}; rollback failed: {rollback_error}"
                try:
                    _rollback_receipt(home, original, detail, channel=channel)
                except CodexPluginError as receipt_error:
                    detail = f"{detail}; receipt failed: {receipt_error}"
                raise CodexPluginError("rollback-failure", detail) from raw_rollback
            if isinstance(raw_error, Exception):
                raise original
            raise
