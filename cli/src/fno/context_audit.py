"""Read-only census and packet compiler for Footnote-authored context."""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Iterable, Sequence, cast

from fno.setup.cli_hooks import SESSION_START_CONTEXT_CARRIERS
from fno.setup.managed_block import extract_block, marker_state


SUPPORTED_HARNESSES = ("claude", "codex", "gemini")
SUPPORTED_ENTRY_STATES = ("startup", "resume", "clear", "post_compact")
DEFAULT_PACKET_BUDGET_BYTES = 32_768

_HOST_FILES = {
    "claude": "CLAUDE.md",
    "codex": "AGENTS.md",
    "gemini": "GEMINI.md",
}
_FOLDED_CONTEXT_SCRIPTS = (
    "session-start-using-fno.sh",
    "inject-project-vision.sh",
    "inject-fno-agent-whoami.sh",
    "setup-nudge-session-start.sh",
    "inject-mail-drain-session-start.sh",
)
_OPERATIONAL_SESSION_SOURCES = {
    "attest-model",
    "eval-sweep-session-start",
    "groom-self-heal-session-start",
    "register-session-start",
}
_PLUGIN_PATH_RE = re.compile(
    r"\$\{(?:CLAUDE_PLUGIN_ROOT|PLUGIN_ROOT)(?::-[^}]*)?\}/"
    r"(?P<path>[A-Za-z0-9_./-]+)"
)
_INCLUDE_RE = re.compile(
    r"^@(?P<path>[^\r\n]+?)[ \t]*(?P<newline>\r?\n|$)",
    re.MULTILINE,
)
_SENTINEL_RE = re.compile(r"\b[A-Za-z0-9-]*sentinel[A-Za-z0-9-]*\b")


class MeasurementKind(str, Enum):
    """The byte domain a source record describes."""

    DIRECTIVE_BYTES = "directive_bytes"
    CARRIER_TEMPLATE_BYTES = "carrier_template_bytes"


@dataclass(frozen=True)
class ContextSource:
    source_id: str
    harness: str
    entry_state: str
    lifecycle: str
    layer: str
    provenance: str
    carrier: str
    reachability_condition: str
    content: bytes | None
    ordinal: int
    status: str = "reachable"
    error: str | None = None
    packet_eligible: bool = True
    measurement: MeasurementKind | str = MeasurementKind.DIRECTIVE_BYTES
    anchors: dict[str, tuple[str, ...]] = field(default_factory=dict)

    def __post_init__(self) -> None:
        try:
            measurement = MeasurementKind(self.measurement)
        except ValueError as exc:
            raise ValueError(f"unknown context measurement: {self.measurement!r}") from exc
        object.__setattr__(self, "measurement", measurement)
        if self.packet_eligible and measurement is not MeasurementKind.DIRECTIVE_BYTES:
            raise ValueError("carrier template bytes cannot be packet eligible")


def _sha256(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def _relative(path: Path | None, root: Path) -> str:
    # manifest is None on the gemini branch; the _relative(manifest, ...) calls
    # that could receive it sit on claude/codex-only paths, but the type must
    # hold for mypy. None never ships: those code paths are unreachable for gemini.
    if path is None:
        return ""
    try:
        return path.resolve().relative_to(root.resolve()).as_posix()
    except (OSError, ValueError):
        return str(path)


def _source_record(source: ContextSource) -> dict:
    size = len(source.content) if source.content is not None else 0
    measurement = cast(MeasurementKind, source.measurement)
    directive = measurement is MeasurementKind.DIRECTIVE_BYTES
    digest = _sha256(source.content) if source.content is not None else None
    return {
        "source_id": source.source_id,
        "harness": source.harness,
        "entry_state": source.entry_state,
        "lifecycle": source.lifecycle,
        "layer": source.layer,
        "provenance": source.provenance,
        "carrier": source.carrier,
        "reachability_condition": source.reachability_condition,
        "status": source.status,
        "error": source.error,
        "bytes": size if directive else 0,
        "estimated_tokens": (size + 3) // 4 if directive else 0,
        "content_hash": digest if directive else None,
        "carrier_bytes": size if not directive else None,
        "carrier_hash": digest if not directive else None,
        "measurement": measurement.value,
        "packet_eligible": source.packet_eligible,
        "anchors": {key: list(values) for key, values in sorted(source.anchors.items())},
    }


def measure_file_source(
    *,
    source_id: str,
    path: Path,
    harness: str,
    entry_state: str,
    lifecycle: str,
    layer: str,
    carrier: str,
    reachability_condition: str,
    ordinal: int,
    repo_root: Path,
    status: str = "reachable",
    error: str | None = None,
    packet_eligible: bool = True,
    measurement: MeasurementKind | str = MeasurementKind.DIRECTIVE_BYTES,
    anchors: dict[str, tuple[str, ...]] | None = None,
) -> ContextSource:
    path = Path(path)
    try:
        content = path.read_bytes()
    except (OSError, UnicodeError) as exc:
        content = None
        status = "unreadable"
        error = f"{type(exc).__name__}: {exc}"
    return ContextSource(
        source_id=source_id,
        harness=harness,
        entry_state=entry_state,
        lifecycle=lifecycle,
        layer=layer,
        provenance=_relative(path, Path(repo_root)),
        carrier=carrier,
        reachability_condition=reachability_condition,
        content=content,
        ordinal=ordinal,
        status=status,
        error=error,
        packet_eligible=packet_eligible,
        measurement=measurement,
        anchors=anchors or {},
    )


def _duplicate_groups(records: Sequence[dict]) -> list[dict]:
    by_hash: dict[str, list[dict]] = {}
    for record in records:
        content_hash = record.get("content_hash")
        if content_hash and record.get("bytes", 0) > 0:
            by_hash.setdefault(content_hash, []).append(record)
    groups = []
    for content_hash, occurrences in sorted(by_hash.items()):
        ids = sorted({str(item["source_id"]) for item in occurrences})
        if len(occurrences) > 1:
            groups.append(
                {
                    "content_hash": content_hash,
                    "source_ids": ids,
                    "occurrences": len(occurrences),
                }
            )
    return groups


def _conflict_groups(records: Sequence[dict]) -> list[dict]:
    by_id: dict[str, list[dict]] = {}
    for record in records:
        if record.get("content_hash"):
            by_id.setdefault(str(record["source_id"]), []).append(record)
    groups = []
    for source_id, occurrences in sorted(by_id.items()):
        hashes = sorted({str(item["content_hash"]) for item in occurrences})
        if len(hashes) > 1:
            groups.append(
                {
                    "source_id": source_id,
                    "content_hashes": hashes,
                    "provenance": list(
                        dict.fromkeys(str(item["provenance"]) for item in occurrences)
                    ),
                }
            )
    return groups


def _merged_anchors(sources: Iterable[ContextSource]) -> dict[str, list[str]]:
    merged: dict[str, list[str]] = {}
    for source in sources:
        for key, values in source.anchors.items():
            bucket = merged.setdefault(key, [])
            for value in values:
                if value not in bucket:
                    bucket.append(value)
    return merged


def compile_context(
    sources: Sequence[ContextSource],
    *,
    packet_budget_bytes: int,
    node_count: int,
) -> dict:
    """Compile metadata only; it never changes hook delivery or graph behavior."""
    if packet_budget_bytes < 1:
        raise ValueError("packet_budget_bytes must be at least 1")
    if node_count < 1:
        raise ValueError("node_count must be at least 1")

    ordered = sorted(enumerate(sources), key=lambda pair: (pair[1].ordinal, pair[0]))
    ordered_sources = [source for _, source in ordered]
    records = [_source_record(source) for source in ordered_sources]

    unique: list[tuple[ContextSource, dict]] = []
    seen_hashes: set[str] = set()
    for source, record in zip(ordered_sources, records):
        content_hash = record["content_hash"]
        if (
            source.status != "reachable"
            or not source.packet_eligible
            or content_hash is None
            or content_hash in seen_hashes
        ):
            continue
        seen_hashes.add(content_hash)
        unique.append((source, record))

    unique.sort(key=lambda pair: (0 if pair[0].layer == "kernel" else 1, pair[0].ordinal))
    included: list[tuple[ContextSource, dict]] = []
    budget_omitted: list[tuple[ContextSource, dict]] = []
    kernel_candidates = [pair for pair in unique if pair[0].layer == "kernel"]
    progressive_candidates = [pair for pair in unique if pair[0].layer != "kernel"]
    kernel_total = sum(record["bytes"] for _, record in kernel_candidates)
    if kernel_total > packet_budget_bytes:
        budget_omitted.extend(kernel_candidates)
        budget_omitted.extend(progressive_candidates)
        used = 0
    else:
        included.extend(kernel_candidates)
        used = kernel_total
        continuation_started = False
        for source, record in progressive_candidates:
            if continuation_started or used + record["bytes"] > packet_budget_bytes:
                continuation_started = True
                budget_omitted.append((source, record))
                continue
            included.append((source, record))
            used += record["bytes"]

    kernel_pairs = [pair for pair in included if pair[0].layer == "kernel"]
    progressive_pairs = [pair for pair in included if pair[0].layer != "kernel"]
    kernel_sources = [source for source, _ in kernel_pairs]
    kernel_hash_input = "\n".join(record["content_hash"] for _, record in kernel_pairs)
    kernel_bytes = sum(record["bytes"] for _, record in kernel_pairs)
    packet_hashes = [record["content_hash"] for _, record in included]
    packet_hash_input = "\n".join(packet_hashes)

    omitted = [
        {
            "source_id": source.source_id,
            "provenance": record["provenance"],
            "reason": f"{source.status}:{source.error or 'not reachable'}",
            "bytes": record["bytes"],
        }
        for source, record in zip(ordered_sources, records)
        if source.status in {"omitted", "unreadable"}
    ]
    omitted.extend(
        {
            "source_id": source.source_id,
            "provenance": record["provenance"],
            "reason": "packet_budget",
            "bytes": record["bytes"],
        }
        for source, record in budget_omitted
    )
    remaining = [record for _, record in budget_omitted]
    continuation = None
    if remaining:
        continuation = {
            "after_source_id": included[-1][0].source_id if included else None,
            "remaining_source_ids": [record["source_id"] for record in remaining],
            "remaining_bytes": sum(record["bytes"] for record in remaining),
        }

    execution = (
        {
            "mode": "existing_single_loop",
            "graph_compiled": False,
            "dispatch_changed": False,
        }
        if node_count == 1
        else {
            "mode": "observational_only",
            "graph_compiled": False,
            "dispatch_changed": False,
        }
    )
    return {
        "schema_version": 1,
        "execution": execution,
        "packet": {
            "budget_bytes": packet_budget_bytes,
            "bytes": used,
            "estimated_tokens": (used + 3) // 4,
            "within_budget": used <= packet_budget_bytes,
            "source_hashes": packet_hashes,
            "content_hash": _sha256(packet_hash_input.encode()) if packet_hash_input else None,
        },
        "kernel": {
            "source_ids": [source.source_id for source in kernel_sources],
            "bytes": kernel_bytes,
            "estimated_tokens": (kernel_bytes + 3) // 4,
            "content_hash": _sha256(kernel_hash_input.encode()) if kernel_hash_input else None,
            "anchors": _merged_anchors(kernel_sources),
        },
        "progressive_sources": [record for _, record in progressive_pairs],
        "source_manifest": records,
        "duplicates": _duplicate_groups(records),
        "conflicts": _conflict_groups(records),
        "omitted_sources": omitted,
        "continuation": continuation,
    }


def _pitfall_section(text: str) -> str:
    start = text.find("## Pitfalls corpus")
    if start == -1:
        return ""
    end = text.find("\n## ", start + len("## Pitfalls corpus"))
    return text[start:] if end == -1 else text[start:end]


def active_pitfall_headings(repo_root: Path) -> list[str]:
    try:
        text = (Path(repo_root) / "AGENTS.md").read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return []
    return re.findall(r"^### (.+)$", _pitfall_section(text), re.MULTILINE)


def _instruction_anchors(content: bytes) -> dict[str, tuple[str, ...]]:
    text = content.decode("utf-8", errors="replace")
    pitfalls = tuple(re.findall(r"^### (.+)$", _pitfall_section(text), re.MULTILINE))
    sentinels = tuple(dict.fromkeys(_SENTINEL_RE.findall(_pitfall_section(text))))
    return {"pitfalls": pitfalls, "sentinels": sentinels}


def _expand_host_content(path: Path, repo_root: Path, seen: frozenset[Path]) -> str:
    resolved = path.resolve()
    root = repo_root.resolve()
    if resolved != root and root not in resolved.parents:
        raise ValueError(f"instruction include escapes repository: {resolved}")
    if resolved in seen:
        raise ValueError(f"instruction include cycle: {resolved}")
    text = resolved.read_text(encoding="utf-8")
    next_seen = seen | {resolved}

    def expand(match: re.Match[str]) -> str:
        target = (resolved.parent / match.group("path").strip()).resolve()
        expanded = _expand_host_content(target, repo_root, next_seen)
        newline = match.group("newline")
        if newline and not expanded.endswith(("\n", "\r")):
            expanded += newline
        return expanded

    return _INCLUDE_RE.sub(expand, text)


def _resolve_host_content(path: Path, repo_root: Path) -> tuple[Path, bytes]:
    return path, _expand_host_content(path, repo_root, frozenset()).encode()


def _is_footnote_source_repository(repo_root: Path) -> bool:
    try:
        manifest = json.loads(
            (repo_root / ".claude-plugin" / "plugin.json").read_text(encoding="utf-8")
        )
    except (OSError, UnicodeError, json.JSONDecodeError):
        return False
    return isinstance(manifest, dict) and manifest.get("name") == "fno"


def _host_sources(
    repo_root: Path, harness: str, entry_state: str, ordinal: int
) -> list[ContextSource]:
    source_repository = _is_footnote_source_repository(repo_root)
    carrier_path = repo_root / _HOST_FILES[harness]
    try:
        if harness == "codex":
            resolved, content = carrier_path, carrier_path.read_bytes()
        else:
            resolved, content = _resolve_host_content(carrier_path, repo_root)
        source = ContextSource(
            source_id="project-instructions",
            harness=harness,
            entry_state=entry_state,
            lifecycle="harness_native",
            layer="kernel",
            provenance=_relative(resolved, repo_root),
            carrier=_relative(carrier_path, repo_root),
            reachability_condition=f"{harness} loads repository instructions",
            content=content,
            ordinal=ordinal,
        measurement=MeasurementKind.DIRECTIVE_BYTES,
            anchors=_instruction_anchors(content),
        )
    except (OSError, UnicodeError, ValueError) as exc:
        source = ContextSource(
            source_id="project-instructions",
            harness=harness,
            entry_state=entry_state,
            lifecycle="harness_native",
            layer="kernel",
            provenance=_relative(carrier_path, repo_root),
            carrier=_relative(carrier_path, repo_root),
            reachability_condition=f"{harness} loads repository instructions",
            content=None,
            ordinal=ordinal,
            status="unreadable",
            error=f"{type(exc).__name__}: {exc}",
            measurement="directive_bytes",
        )
    sources = [source] if source_repository else []
    if not source_repository and source.content is None and carrier_path.exists():
        sources.append(
            ContextSource(
                source_id="managed-footnote-block",
                harness=harness,
                entry_state=entry_state,
                lifecycle="harness_native",
                layer="kernel",
                provenance=f"{source.provenance}#fno-managed-block",
                carrier=source.carrier,
                reachability_condition="managed block carrier is readable",
                content=None,
                ordinal=ordinal + 1,
                status="unreadable",
                error=source.error,
                anchors={"pitfalls": (), "sentinels": ()},
            )
        )
    if source.content is not None:
        host_text = source.content.decode("utf-8", errors="replace")
        managed = extract_block(host_text)
        if managed is not None:
            sources.append(
                ContextSource(
                    source_id="managed-footnote-block",
                    harness=harness,
                    entry_state=entry_state,
                    lifecycle="harness_native",
                    layer="kernel",
                    provenance=f"{source.provenance}#fno-managed-block",
                    carrier=source.carrier,
                    reachability_condition="managed block is present in host instructions",
                    content=managed.encode(),
                    ordinal=ordinal + 1,
                    packet_eligible=not source_repository,
                    measurement=MeasurementKind.DIRECTIVE_BYTES,
                    anchors=_instruction_anchors(managed.encode()),
                )
            )
        elif not source_repository and marker_state(host_text) == "malformed":
            sources.append(
                ContextSource(
                    source_id="managed-footnote-block",
                    harness=harness,
                    entry_state=entry_state,
                    lifecycle="harness_native",
                    layer="kernel",
                    provenance=f"{source.provenance}#fno-managed-block",
                    carrier=source.carrier,
                    reachability_condition="managed block fences are well formed",
                    content=None,
                    ordinal=ordinal + 1,
                    status="unreadable",
                    error="malformed Footnote managed block fences",
                    anchors={"pitfalls": (), "sentinels": ()},
                )
            )

    if harness == "claude" and source_repository:
        for index, rule in enumerate(sorted((repo_root / ".claude" / "rules").glob("*.md"))):
            sources.append(
                measure_file_source(
                    source_id=f"claude-rule:{rule.name}",
                    path=rule,
                    harness=harness,
                    entry_state=entry_state,
                    lifecycle="harness_native",
                    layer="progressive",
                    carrier=rule.as_posix(),
                    reachability_condition="Claude repository rule discovery",
                    ordinal=ordinal + index + 2,
                    repo_root=repo_root,
                    measurement="directive_bytes",
                )
            )
    return sources


def runtime_native_context_manifest(
    repo_root: Path,
    *,
    harness: str,
    entry_state: str,
) -> list[dict]:
    """Measure Footnote-owned harness-native directives on the runtime path."""
    manifest = []
    for source in _host_sources(repo_root, harness, entry_state, 0):
        measurement = cast(MeasurementKind, source.measurement)
        if not source.packet_eligible or measurement is not MeasurementKind.DIRECTIVE_BYTES:
            continue
        record = _source_record(source)
        record["status"] = "observed" if source.status == "reachable" else source.status
        manifest.append(
            {
                key: record.get(key)
                for key in (
                    "source_id",
                    "carrier",
                    "status",
                    "error",
                    "bytes",
                    "estimated_tokens",
                    "content_hash",
                )
            }
        )
    return manifest


def _load_hook_commands(
    repo_root: Path,
    manifest: Path,
    event_names: Sequence[str],
    matchers: set[str] | None = None,
) -> tuple[list[str], str | None]:
    try:
        data = json.loads(manifest.read_text(encoding="utf-8"))
        hooks = data["hooks"]
        commands: list[str] = []
        for event_name in event_names:
            groups = hooks.get(event_name, [])
            if not isinstance(groups, list):
                raise ValueError(f"hooks.{event_name} is not an array")
            for group_index, group in enumerate(groups):
                if not isinstance(group, dict):
                    raise ValueError(
                        f"hooks.{event_name}[{group_index}] is not an object"
                    )
                # SessionStart groups carry a "matcher" that filters by source
                # (startup|resume|clear|compact|fork); "" fires for all sources.
                # When a caller requests specific sources, honor it so a
                # compact-only recorder is not counted among startup recorders.
                if matchers is not None and group.get("matcher", "") not in matchers:
                    continue
                entries = group.get("hooks", [])
                if not isinstance(entries, list):
                    raise ValueError(
                        f"hooks.{event_name}[{group_index}].hooks is not an array"
                    )
                for entry_index, entry in enumerate(entries):
                    if not isinstance(entry, dict):
                        raise ValueError(
                            f"hooks.{event_name}[{group_index}].hooks"
                            f"[{entry_index}] is not an object"
                        )
                    command = entry.get("command")
                    if isinstance(command, str) and command:
                        commands.append(command)
        return commands, None
    except (OSError, UnicodeError, ValueError, KeyError, TypeError) as exc:
        return [], f"{_relative(manifest, repo_root)}: {type(exc).__name__}: {exc}"


def _command_path(command: str, repo_root: Path) -> Path | None:
    matches = list(_PLUGIN_PATH_RE.finditer(command))
    for match in reversed(matches):
        path = repo_root / match.group("path")
        if path.name != "context-observe-hook.sh":
            return path
    return None


def _logical_id(path: Path) -> str:
    return path.stem.replace("_", "-")


def _measure_hook(
    path: Path,
    *,
    repo_root: Path,
    harness: str,
    entry_state: str,
    lifecycle: str,
    ordinal: int,
    status: str = "reachable",
    error: str | None = None,
) -> ContextSource:
    source_id = _logical_id(path)
    content_path = path
    measurement = MeasurementKind.CARRIER_TEMPLATE_BYTES
    if path.name == "session-start-using-fno.sh":
        source_id = "using-fno"
        content_path = repo_root / "skills" / "using-fno" / "SKILL.md"
        measurement = MeasurementKind.DIRECTIVE_BYTES
    elif status == "reachable":
        status = "registered"
    packet_eligible = (
        measurement is MeasurementKind.DIRECTIVE_BYTES
        and source_id not in _OPERATIONAL_SESSION_SOURCES
    )
    return measure_file_source(
        source_id=source_id,
        path=content_path,
        harness=harness,
        entry_state=entry_state,
        lifecycle=lifecycle,
        layer="kernel" if source_id == "using-fno" else "progressive",
        carrier=_relative(path, repo_root),
        reachability_condition=f"{lifecycle} registration",
        ordinal=ordinal,
        repo_root=repo_root,
        status=status,
        error=error,
        packet_eligible=packet_eligible,
        measurement=measurement,
    )


def _folded_sources(
    repo_root: Path,
    *,
    harness: str,
    entry_state: str,
    wrapper: Path,
    ordinal: int,
) -> list[ContextSource]:
    try:
        wrapper_text = wrapper.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        return [
            ContextSource(
                source_id="session-start-wrapper",
                harness=harness,
                entry_state=entry_state,
                lifecycle="session_start",
                layer="progressive",
                provenance=_relative(wrapper, repo_root),
                carrier=_relative(wrapper, repo_root),
                reachability_condition=f"{harness} folded SessionStart",
                content=None,
                ordinal=ordinal,
                status="unreadable",
                error=f"{type(exc).__name__}: {exc}",
                packet_eligible=False,
            )
        ]

    sources: list[ContextSource] = []
    for offset, filename in enumerate(_FOLDED_CONTEXT_SCRIPTS):
        path = repo_root / "hooks" / filename
        reachable = f"${{SCRIPT_DIR}}/{filename}" in wrapper_text
        sources.append(
            _measure_hook(
                path,
                repo_root=repo_root,
                harness=harness,
                entry_state=entry_state,
                lifecycle="session_start",
                ordinal=ordinal + offset,
                status="reachable" if reachable else "omitted",
                error=None if reachable else "not_reached_by_folded_session_start",
            )
        )

    start = wrapper_text.find("# 4. worktree-scope hygiene")
    end = wrapper_text.find("# 5. first-run setup nudge", start)
    if start != -1 and end != -1:
        hygiene = wrapper_text[start:end].encode()
        sources.append(
            ContextSource(
                source_id="worktree-hygiene",
                harness=harness,
                entry_state=entry_state,
                lifecycle="session_start",
                layer="progressive",
                provenance="hooks/session-start.sh#worktree-hygiene",
                carrier="hooks/session-start.sh",
                reachability_condition=f"{harness} folded SessionStart",
                content=hygiene,
                ordinal=ordinal + len(_FOLDED_CONTEXT_SCRIPTS),
                status="registered",
                measurement=MeasurementKind.CARRIER_TEMPLATE_BYTES,
                packet_eligible=False,
            )
        )
    return sources


def _discover_cell_sources(
    host_root: Path,
    plugin_root: Path,
    harness: str,
    entry_state: str,
) -> list[ContextSource]:
    sources = _host_sources(host_root, harness, entry_state, 0)
    ordinal = len(sources) + 10
    # Each spec is (event_name, matchers, lifecycle). matchers=None loads every
    # group; a set restricts to SessionStart groups whose "matcher" is in it.
    # Post-compaction context rides different events per harness - PostCompact on
    # Codex, SessionStart(source=compact) on Claude - so the post_compact census
    # enumerates both carriers. Startup enumerates only matcher="" recorders so a
    # compact-only recorder is not miscounted as a startup one.
    if entry_state == "post_compact":
        specs = [
            ("PostCompact", None, "post_compact"),
            ("SessionStart", {"compact"}, "session_start"),
        ]
    else:
        specs = [("SessionStart", {"", entry_state}, "session_start")]

    commands: list[tuple[str, str]] = []  # (command, lifecycle)
    manifest_error: str | None = None
    manifest: Path | None = None
    if harness in ("claude", "codex"):
        manifest = (
            plugin_root / "hooks" / "hooks.json"
            if harness == "claude"
            else plugin_root / "hooks" / "codex-hooks.json"
        )
        for event_name, matchers, lifecycle in specs:
            loaded, err = _load_hook_commands(
                plugin_root, manifest, [event_name], matchers
            )
            if err:
                manifest_error = err
                break
            commands.extend((cmd, lifecycle) for cmd in loaded)
    else:
        carrier = next(item for item in SESSION_START_CONTEXT_CARRIERS if item.harness == "gemini")
        gemini_cmds = (
            [f"${{PLUGIN_ROOT}}/{carrier.script}"] if entry_state in carrier.entry_states else []
        )
        if gemini_cmds:
            commands.extend((cmd, "session_start") for cmd in gemini_cmds)
        else:
            sources.append(
                measure_file_source(
                    source_id="session-start-wrapper",
                    path=plugin_root / carrier.script,
                    harness=harness,
                    entry_state=entry_state,
                    lifecycle="session_start",
                    layer="progressive",
                    carrier=carrier.script,
                    reachability_condition="Gemini SessionStart registration",
                    ordinal=ordinal,
                    repo_root=plugin_root,
                    status="omitted",
                    error="entry_state_not_registered",
                    packet_eligible=False,
                )
            )
            ordinal += 1
            if entry_state == "post_compact":
                for _postcompact_hook in (
                    "target-postcompact-reinject.sh",
                    "king-postcompact-reinject.sh",
                ):
                    sources.append(
                        _measure_hook(
                            plugin_root / "hooks" / _postcompact_hook,
                            repo_root=plugin_root,
                            harness=harness,
                            entry_state=entry_state,
                            lifecycle="post_compact",
                            ordinal=ordinal,
                            status="omitted",
                            error="no_post_compact_registration",
                        )
                    )
                    ordinal += 1

    if manifest_error:
        sources.append(
            ContextSource(
                source_id="hook-manifest",
                harness=harness,
                entry_state=entry_state,
                lifecycle="session_start",
                layer="progressive",
                provenance=_relative(manifest, plugin_root),
                carrier=_relative(manifest, plugin_root),
                reachability_condition=f"{harness} hook manifest parses",
                content=None,
                ordinal=ordinal,
                status="unreadable",
                error=manifest_error,
                packet_eligible=False,
            )
        )
        return sources

    for command_index, (command, lifecycle) in enumerate(commands):
        path = _command_path(command, plugin_root)
        if path is None:
            sources.append(
                ContextSource(
                    source_id=f"unresolved-hook:{command_index}",
                    harness=harness,
                    entry_state=entry_state,
                    lifecycle=lifecycle,
                    layer="progressive",
                    provenance=_relative(manifest, plugin_root),
                    carrier=command,
                    reachability_condition=f"{harness} registered hook resolves",
                    content=None,
                    ordinal=ordinal,
                    status="unreadable",
                    error="registered command has no Footnote plugin path",
                    packet_eligible=False,
                )
            )
            ordinal += 1
            continue
        if path.name == "session-start.sh":
            sources.extend(
                _folded_sources(
                    plugin_root,
                    harness=harness,
                    entry_state=entry_state,
                    wrapper=path,
                    ordinal=ordinal,
                )
            )
            ordinal += len(_FOLDED_CONTEXT_SCRIPTS) + 1
        else:
            sources.append(
                _measure_hook(
                    path,
                    repo_root=plugin_root,
                    harness=harness,
                    entry_state=entry_state,
                    lifecycle=lifecycle,
                    ordinal=ordinal,
                )
            )
            ordinal += 1
    return sources


def build_context_report(
    repo_root: Path,
    *,
    plugin_root: Path | None = None,
    harnesses: Sequence[str] = SUPPORTED_HARNESSES,
    entry_states: Sequence[str] = SUPPORTED_ENTRY_STATES,
    packet_budget_bytes: int = DEFAULT_PACKET_BUDGET_BYTES,
    node_count: int = 1,
) -> dict:
    repo_root = Path(repo_root)
    plugin_root = Path(plugin_root) if plugin_root is not None else repo_root
    bad_harnesses = sorted(set(harnesses) - set(SUPPORTED_HARNESSES))
    bad_states = sorted(set(entry_states) - set(SUPPORTED_ENTRY_STATES))
    if bad_harnesses:
        raise ValueError(f"unsupported harness: {', '.join(bad_harnesses)}")
    if bad_states:
        raise ValueError(f"unsupported entry state: {', '.join(bad_states)}")

    cells = []
    all_records: list[dict] = []
    for harness in harnesses:
        for entry_state in entry_states:
            sources = _discover_cell_sources(
                repo_root,
                plugin_root,
                harness,
                entry_state,
            )
            compiled = compile_context(
                sources,
                packet_budget_bytes=packet_budget_bytes,
                node_count=node_count,
            )
            all_records.extend(compiled["source_manifest"])
            cells.append(
                {
                    "harness": harness,
                    "entry_state": entry_state,
                    "compiled": compiled,
                }
            )
    return {
        "schema_version": 1,
        "repo_root": str(repo_root.resolve()),
        "plugin_root": str(plugin_root.resolve()),
        "matrix": {
            "harnesses": list(harnesses),
            "entry_states": list(entry_states),
        },
        "packet_budget_bytes": packet_budget_bytes,
        "cells": cells,
        "duplicates": _duplicate_groups(all_records),
        "conflicts": _conflict_groups(all_records),
    }
