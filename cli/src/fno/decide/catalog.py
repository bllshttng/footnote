"""Checked-in repository law catalog and canonical subject aliases."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import Any, Mapping

import yaml

CATALOG_RELATIVE_PATH = Path("docs/architecture/decisions.yaml")
_DECISION_ID_RE = re.compile(r"^d-[0-9a-f]{4,32}$", re.IGNORECASE)


class DecisionCatalogError(ValueError):
    """A present repository catalog cannot be read as authoritative law."""


@dataclass(frozen=True)
class DecisionCatalog:
    rows: tuple[Mapping[str, Any], ...]
    aliases: Mapping[str, str]

    def canonical_subject(self, subject: str) -> str:
        normalized = subject.strip()
        return self.aliases.get(normalized.casefold(), normalized)


def _error(path: Path, detail: str) -> DecisionCatalogError:
    return DecisionCatalogError(f"decision catalog {path}: {detail}")


def _required_text(row: Mapping[str, Any], key: str, path: Path, index: int) -> str:
    value = row.get(key)
    if not isinstance(value, str) or not value.strip():
        raise _error(path, f"decisions[{index}].{key} must be a non-blank string")
    return value.strip()


def _validate_supersession_graph(rows: list[dict[str, Any]], ids: set[str], path: Path) -> None:
    edges: dict[str, str] = {}
    for row in rows:
        target = row.get("supersedes")
        if target is None:
            continue
        if target not in ids:
            raise _error(
                path,
                f"decision {row['decision_id']} supersedes missing decision {target}",
            )
        edges[row["decision_id"]] = target

    for start in edges:
        seen: set[str] = set()
        current = start
        while current in edges:
            if current in seen:
                raise _error(path, f"supersession cycle includes {current}")
            seen.add(current)
            current = edges[current]


def load_catalog(root: Path | None = None) -> DecisionCatalog:
    """Load repository law; an absent file is an explicit empty source."""
    if root is None:
        from fno.paths import resolve_repo_root

        root = resolve_repo_root()
    path = root / CATALOG_RELATIVE_PATH
    if not path.exists():
        return DecisionCatalog((), MappingProxyType({}))
    try:
        raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, yaml.YAMLError) as exc:
        raise _error(path, f"cannot read YAML: {exc}") from exc
    if not isinstance(raw, dict):
        raise _error(path, "top level must be a mapping")
    if raw.get("version") != 1:
        raise _error(path, "version must be 1")
    decisions = raw.get("decisions")
    if not isinstance(decisions, list):
        raise _error(path, "decisions must be a list")

    normalized_rows: list[dict[str, Any]] = []
    decision_ids: set[str] = set()
    canonical_subjects: dict[str, str] = {}
    aliases: dict[str, str] = {}
    for index, candidate in enumerate(decisions):
        if not isinstance(candidate, dict):
            raise _error(path, f"decisions[{index}] must be a mapping")
        decision_id = _required_text(candidate, "decision_id", path, index)
        if not _DECISION_ID_RE.fullmatch(decision_id):
            raise _error(path, f"decisions[{index}].decision_id is invalid: {decision_id}")
        if decision_id.casefold() in decision_ids:
            raise _error(path, f"duplicate decision id {decision_id}")
        decision_ids.add(decision_id.casefold())

        subject = _required_text(candidate, "subject", path, index)
        subject_key = subject.casefold()
        previous_subject = canonical_subjects.get(subject_key)
        if previous_subject is not None and previous_subject != subject:
            raise _error(path, f"duplicate canonical subject {subject!r}")
        canonical_subjects[subject_key] = subject

        raw_aliases = candidate.get("aliases", [])
        if not isinstance(raw_aliases, list) or any(
            not isinstance(alias, str) or not alias.strip() for alias in raw_aliases
        ):
            raise _error(path, f"decisions[{index}].aliases must be non-blank strings")
        for alias in (subject, *raw_aliases):
            alias_key = alias.strip().casefold()
            previous = aliases.get(alias_key)
            if previous is not None and previous != subject:
                raise _error(
                    path,
                    f"alias {alias.strip()!r} maps to both {previous!r} and {subject!r}",
                )
            aliases[alias_key] = subject

        supersedes = candidate.get("supersedes")
        if supersedes is not None:
            if not isinstance(supersedes, str) or not _DECISION_ID_RE.fullmatch(supersedes.strip()):
                raise _error(path, f"decisions[{index}].supersedes must be a decision id")
            supersedes = supersedes.strip().casefold()

        normalized: dict[str, Any] = {
            "decision_id": decision_id.casefold(),
            "subject": subject,
            "aliases": tuple(alias.strip() for alias in raw_aliases),
            "decision": _required_text(candidate, "decision", path, index),
            "rationale": _required_text(candidate, "rationale", path, index),
            "authority_source": "repository",
            "_source": "repository",
        }
        if supersedes is not None:
            normalized["supersedes"] = supersedes
        normalized_rows.append(normalized)

    _validate_supersession_graph(normalized_rows, decision_ids, path)
    return DecisionCatalog(
        tuple(MappingProxyType(row) for row in normalized_rows),
        MappingProxyType(aliases),
    )
