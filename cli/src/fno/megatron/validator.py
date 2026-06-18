"""Megatron mission manifest validator.

Pure function over a parsed ``Manifest``. Each rule appends to a result
list rather than raising, so a single validate call surfaces every
problem the operator needs to fix before dispatching the mission.

Validator is INTENTIONALLY conservative: it errors on shapes the
commander loop cannot handle (empty waves, chain-of-research, oversize
bodies, wave-cap overflow). It does not enforce style or naming.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

from fno.megatron.manifest import Manifest, Wave
from fno.projects.resolve import (
    DuplicateShortName,
    ProjectNotFound,
    SettingsNotFound,
    resolve_project_name,
)


DEFAULT_MAX_PROJECTS_PER_WAVE = 8
BODY_OVERSIZE_BYTES = 10 * 1024


@dataclass
class ValidationError:
    code: str
    wave_index: Optional[int]
    message: str


def _check_empty_wave(idx: int, wave: Wave, errors: list[ValidationError]) -> None:
    if not wave.projects and not wave.tasks:
        errors.append(
            ValidationError(
                code="empty_wave",
                wave_index=idx,
                message=f"wave {wave.wave}: wave must have at least one project or task",
            )
        )


def _check_wave_project_cap(
    idx: int, wave: Wave, max_projects: int, errors: list[ValidationError]
) -> None:
    n = len(wave.projects)
    if n > max_projects:
        errors.append(
            ValidationError(
                code="wave_project_cap_exceeded",
                wave_index=idx,
                message=(
                    f"wave {wave.wave}: max {max_projects} projects per wave "
                    f"(got {n})"
                ),
            )
        )


def _check_research_chain(
    waves: list[Wave], errors: list[ValidationError]
) -> None:
    for i in range(1, len(waves)):
        prev = waves[i - 1]
        cur = waves[i]
        if cur.wave_type == "research" and prev.wave_type == "research":
            errors.append(
                ValidationError(
                    code="research_chain",
                    wave_index=i,
                    message=(
                        f"wave {cur.wave}: research wave directly follows another "
                        f"research wave; insert a dispatched_from:{prev.wave} wave "
                        f"or merge proposals before continuing"
                    ),
                )
            )


def _check_project_names(
    idx: int, wave: Wave, errors: list[ValidationError]
) -> None:
    for project in wave.projects:
        try:
            resolve_project_name(project.name)
        except ProjectNotFound:
            errors.append(
                ValidationError(
                    code="project_unknown",
                    wave_index=idx,
                    message=(
                        f"wave {wave.wave} project {project.name}: unknown name"
                    ),
                )
            )
        except (SettingsNotFound, DuplicateShortName) as exc:
            # Resolver cannot operate (settings.yaml missing or has an
            # ambiguous short_name). Don't crash validate_manifest -
            # surface a structured error so the caller can act on it.
            errors.append(
                ValidationError(
                    code="resolver_unavailable",
                    wave_index=idx,
                    message=(
                        f"wave {wave.wave} project {project.name}: "
                        f"resolver unavailable ({type(exc).__name__}: {exc})"
                    ),
                )
            )


def _check_no_duplicate_project_names(
    idx: int, wave: Wave, errors: list[ValidationError]
) -> None:
    """Reject a wave that lists the same canonical project name twice.

    Names are resolved through resolve_project_name first so a manifest
    mixing `name: fake-a` and `name: a-short` (both canonical=fake-a) is
    correctly flagged.
    """
    seen: dict[str, str] = {}  # canonical -> first raw name seen
    for project in wave.projects:
        try:
            canonical = resolve_project_name(project.name)
        except (ProjectNotFound, SettingsNotFound):
            # Resolver failed for a recoverable reason (unknown name,
            # missing settings). The existing _check_project_names handler
            # will report those errors. Fall back to raw-name comparison
            # so a manifest with `[unknown, unknown]` is still flagged as
            # a duplicate.
            canonical = project.name
        except DuplicateShortName:
            # The resolver itself surfaced a name-collision intent we
            # cannot safely canonicalize for duplicate detection: the
            # short name resolves ambiguously across workspaces. Skip
            # this project entirely so we do not silently weaken the
            # check to a raw-name match - _check_project_names already
            # reports the structural error.
            continue
        if canonical in seen:
            errors.append(
                ValidationError(
                    code="duplicate_project_in_wave",
                    wave_index=idx,
                    message=(
                        f"wave {wave.wave}: project {project.name!r} duplicates "
                        f"{seen[canonical]!r} (both resolve to canonical "
                        f"{canonical!r}); manifest must declare each project at "
                        f"most once per wave"
                    ),
                )
            )
        else:
            seen[canonical] = project.name


def _check_body_oversize(
    idx: int, wave: Wave, errors: list[ValidationError]
) -> None:
    for project in wave.projects:
        size = len(project.body.encode("utf-8"))
        if size > BODY_OVERSIZE_BYTES:
            errors.append(
                ValidationError(
                    code="body_oversize",
                    wave_index=idx,
                    message=(
                        f"wave {wave.wave} project {project.name}: body is "
                        f"{size} bytes; max {BODY_OVERSIZE_BYTES} bytes "
                        f"(truncate at a section boundary)"
                    ),
                )
            )


def validate_manifest(
    manifest: Manifest,
    *,
    max_projects_per_wave: int = DEFAULT_MAX_PROJECTS_PER_WAVE,
) -> list[ValidationError]:
    """Run every validation rule against ``manifest``; return aggregated errors.

    The list is ordered by wave index and then by check (empty_wave,
    wave_project_cap_exceeded, body_oversize, research_chain). Callers
    that want a single-line summary can join ``[e.message for e in errors]``;
    callers that want to gate dispatch should treat any non-empty list as
    a refusal.
    """
    errors: list[ValidationError] = []
    for idx, wave in enumerate(manifest.waves):
        _check_empty_wave(idx, wave, errors)
        _check_wave_project_cap(idx, wave, max_projects_per_wave, errors)
        _check_body_oversize(idx, wave, errors)
        _check_project_names(idx, wave, errors)
        _check_no_duplicate_project_names(idx, wave, errors)
    _check_research_chain(manifest.waves, errors)
    return errors
