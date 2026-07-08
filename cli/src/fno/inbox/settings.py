"""Inbox-related settings.yaml readers, including the watch.enabled flag
introduced in the headless-drain plan and the peer/surface ownership maps
introduced in the cross-project messaging-prompts plan."""
from __future__ import annotations

import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, TypeAlias


NotifyPolicy = Literal["question_only", "all", "off"]
_VALID_POLICIES: tuple[str, ...] = ("question_only", "all", "off")

# Document the axes of the peer/surface maps so callers don't have to
# re-derive what the dict shape means at every read site. These are
# pure documentation aliases; no runtime cost.
PeerSurfaceMap: TypeAlias = dict[str, list[str]]
"""Maps peer name -> list of surface names that peer owns."""

SurfacePatternMap: TypeAlias = dict[str, list[str]]
"""Maps surface name -> list of file glob patterns that constitute that surface."""


@dataclass(frozen=True)
class WatchSettings:
    enabled: bool = False
    notify_on_send: NotifyPolicy = "question_only"


def _load_inbox_config(repo_root: Path | None) -> dict[str, Any] | None:
    """Walk up from repo_root (or cwd) to the first .fno/ config (config.toml,
    else legacy settings.yaml), parse it, and return the `inbox` block as a
    dict. Returns None when no file is found, parse fails, or the inbox block
    is absent or malformed.

    Centralizing the walk + extraction keeps the three readers below honest
    about their semantics (their job is "given the inbox block, give me X")
    and means tests for one reader cannot accidentally cover bugs in another.

    A malformed legacy settings.yaml still surfaces a stderr warning before
    returning None. Without it, a typo on line 200 is indistinguishable from
    "no peers configured" - the fire-and-forget messaging substrate would go
    dark with zero signal.
    """
    from fno.config import read_config_flat

    start = (repo_root if repo_root is not None else Path.cwd()).resolve()
    for candidate in [start, *start.parents]:
        fno_dir = candidate / ".fno"
        toml_file = fno_dir / "config.toml"
        if toml_file.is_file():
            inbox = read_config_flat(toml_file).get("inbox")
            return inbox if isinstance(inbox, dict) else None
        settings_file = fno_dir / "settings.yaml"
        if settings_file.is_file():
            inbox = _load_inbox_from_yaml(settings_file)
            return inbox if isinstance(inbox, dict) else None
    return None


def _load_inbox_from_yaml(settings_file: Path) -> dict[str, Any] | None:
    """Parse a legacy settings.yaml and return its `config.inbox` block, warning
    to stderr on an unreadable/malformed file (the substrate-dark signal)."""
    import yaml  # lazy import to avoid hard dep at module level

    try:
        text = settings_file.read_text(encoding="utf-8")
    except OSError as e:
        print(f"inbox.settings: cannot read {settings_file}: {e}", file=sys.stderr)
        return None
    try:
        data = yaml.safe_load(text)
    except yaml.YAMLError as e:
        print(
            f"inbox.settings: malformed YAML in {settings_file}: {e}",
            file=sys.stderr,
        )
        return None
    if not isinstance(data, dict):
        return None
    # Type-check each level: dict.get(key, default) returns default ONLY when
    # the key is missing, not when its value is null. A user with `config:`
    # written as a bare key (parsed as None) would otherwise crash with
    # AttributeError. Caught by gemini-code-assist on PR #214.
    config = data.get("config")
    if not isinstance(config, dict):
        return None
    inbox = config.get("inbox")
    return inbox if isinstance(inbox, dict) else None


def read_watch_settings(repo_root: Path | None = None) -> WatchSettings:
    """Read config.inbox.watch.* from nearest .fno/settings.yaml.

    Returns WatchSettings() with defaults when no file is found or the block
    is absent. Unknown notify_on_send values log a warning and fall back to
    'off' (fail-closed), so a typo never silently enables the wrong policy.
    """
    inbox = _load_inbox_config(repo_root)
    if inbox is None:
        return WatchSettings()

    watch_cfg = inbox.get("watch", {})
    if not isinstance(watch_cfg, dict):
        return WatchSettings()

    raw_policy = str(watch_cfg.get("notify_on_send", "question_only"))
    if raw_policy not in _VALID_POLICIES:
        print(
            f"inbox.settings: unknown notify_on_send={raw_policy!r};"
            f" falling back to 'off'. Valid values: {_VALID_POLICIES}",
            file=sys.stderr,
        )
        policy: NotifyPolicy = "off"
    else:
        policy = raw_policy  # type: ignore[assignment]

    return WatchSettings(
        enabled=bool(watch_cfg.get("enabled", False)),
        notify_on_send=policy,
    )


def read_peer_surfaces(repo_root: Path | None = None) -> PeerSurfaceMap:
    """Read config.inbox.peers.<name>.surfaces from settings.yaml.

    Returns a dict keyed by peer name, valued as the list of surface names
    that peer owns. Drop semantics:

    - Peers whose entry is not a mapping (e.g. ``- foo``) -> drop, warn.
    - Peers whose ``surfaces:`` value is present but NOT a list (e.g.
      ``surfaces: api-server`` as a string) -> drop, warn loudly. This is
      a config typo that should be observable.
    - Peers whose ``surfaces:`` key is absent (e.g. ``empty-peer: {}``)
      -> drop SILENTLY. A peer block with no surfaces is "not yet
      configured", not a bug.

    Mirrors the existing ``read_watch_settings`` pattern of "warn loudly
    on bad data, return safe default" so a config typo is observable
    rather than silently dimming the substrate.
    """
    inbox = _load_inbox_config(repo_root)
    if inbox is None:
        return {}

    peers_cfg = inbox.get("peers", {})
    if not isinstance(peers_cfg, dict):
        return {}

    out: PeerSurfaceMap = {}
    for peer_name, peer_block in peers_cfg.items():
        if not isinstance(peer_block, dict):
            print(
                f"inbox.settings: peer {peer_name!r} entry is not a mapping;"
                f" dropping (got {type(peer_block).__name__})",
                file=sys.stderr,
            )
            continue
        surfaces = peer_block.get("surfaces")
        if not isinstance(surfaces, list):
            if surfaces is not None:
                print(
                    f"inbox.settings: peer {peer_name!r} surfaces is not a list;"
                    f" dropping (got {type(surfaces).__name__})",
                    file=sys.stderr,
                )
            continue
        out[str(peer_name)] = [str(s) for s in surfaces]
    return out


def read_peer_projects(repo_root: Path | None = None) -> dict[str, str]:
    """Read config.inbox.peers.<name>.project from settings.yaml.

    Returns a dict keyed by peer name, valued as the project that peer serves.
    This is the resolver *hint* for ``--to-project`` anycast: it lets an
    operator declare "peer foo works on project X" even when foo's registry
    cwd does not resolve to X. The registry cwd->project mapping stays the
    authoritative source; this hint only adds associations.

    Drop semantics mirror ``read_peer_surfaces``: a peer whose entry is not a
    mapping is dropped with a warning; a peer with no ``project:`` key is
    dropped silently (not configured). A missing/malformed inbox block returns
    {} so ``--to-project`` resolution degrades to the registry mapping alone
    and never crashes on config shape (AC6-FR).
    """
    inbox = _load_inbox_config(repo_root)
    if inbox is None:
        return {}

    peers_cfg = inbox.get("peers", {})
    if not isinstance(peers_cfg, dict):
        return {}

    out: dict[str, str] = {}
    for peer_name, peer_block in peers_cfg.items():
        if not isinstance(peer_block, dict):
            print(
                f"inbox.settings: peer {peer_name!r} entry is not a mapping;"
                f" dropping (got {type(peer_block).__name__})",
                file=sys.stderr,
            )
            continue
        project = peer_block.get("project")
        if not isinstance(project, str) or not project:
            continue
        out[str(peer_name)] = project
    return out


def read_surface_patterns(repo_root: Path | None = None) -> SurfacePatternMap:
    """Read config.inbox.surface_patterns.<name> from settings.yaml.

    Returns a dict keyed by surface name, valued as the list of glob patterns
    that map to that surface. Missing block returns {}. Surfaces whose value
    is not a list emit a stderr warning and are dropped. Used by /spec to
    intersect a plan's Files-to-Modify table with peer-owned surfaces.
    """
    inbox = _load_inbox_config(repo_root)
    if inbox is None:
        return {}

    patterns_cfg = inbox.get("surface_patterns", {})
    if not isinstance(patterns_cfg, dict):
        return {}

    out: SurfacePatternMap = {}
    for surface_name, globs in patterns_cfg.items():
        if not isinstance(globs, list):
            print(
                f"inbox.settings: surface_patterns[{surface_name!r}] is not a list;"
                f" dropping (got {type(globs).__name__})",
                file=sys.stderr,
            )
            continue
        out[str(surface_name)] = [str(g) for g in globs]
    return out
