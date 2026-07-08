"""fno.plan._locate - resolve a plan input to its concrete shape.

A plan may be either:
- A folder plan: a directory containing ``00-INDEX.md``
- A single-doc plan: a ``.md`` file

Usage::

    from fno.plan._locate import locate_plan, PlanNotFound, ResolvedPlan

    resolved = locate_plan("path/to/my-feature")
    if resolved.kind == "folder":
        # read resolved.index_path for execution strategy
        ...
    else:
        # read resolved.root_path (the .md file) directly
        ...
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Literal

PlanKind = Literal["folder", "single"]


@dataclass(frozen=True)
class ResolvedPlan:
    """Concrete plan shape resolved from an input path.

    Attributes:
        kind: ``"folder"`` for folder plans (directory + 00-INDEX.md) or
              ``"single"`` for single-doc plans (``.md`` file).
        root_path: For folder plans, the directory path.  For single-doc
                   plans, the ``.md`` file path.
        index_path: For folder plans, the ``00-INDEX.md`` path inside
                    *root_path*.  Always ``None`` for single-doc plans.
    """

    kind: PlanKind
    root_path: Path
    index_path: Path | None


class PlanNotFound(FileNotFoundError):
    """Raised when locate_plan cannot find a plan at the input path.

    Inherits from ``FileNotFoundError`` so callers can catch either.
    """


def locate_plan(input_path: str | Path) -> ResolvedPlan:
    """Resolve a plan input to its concrete shape.

    Resolution rules:
    - *input_path* is a directory containing ``00-INDEX.md`` ->
      ``ResolvedPlan(kind="folder", root_path=dir, index_path=dir/00-INDEX.md)``
    - *input_path* is a file (any extension) ->
      ``ResolvedPlan(kind="single", root_path=file, index_path=None)``
    - *input_path* does not exist OR is a directory without ``00-INDEX.md`` ->
      raise ``PlanNotFound``

    Args:
        input_path: Path to the plan directory or ``.md`` file.  Accepts
                    both ``str`` and ``Path``; ``Path`` is returned in the
                    result regardless of input type.

    Returns:
        A frozen :class:`ResolvedPlan` describing the detected shape.

    Raises:
        PlanNotFound: When the plan cannot be located at *input_path*.
    """
    path = Path(input_path)

    if not path.exists():
        raise PlanNotFound(f"Plan not found: {path}")

    if path.is_dir():
        index = path / "00-INDEX.md"
        if not index.exists():
            raise PlanNotFound(
                f"Directory exists but contains no 00-INDEX.md: {path}"
            )
        return ResolvedPlan(kind="folder", root_path=path, index_path=index)

    # File path (single-doc)
    return ResolvedPlan(kind="single", root_path=path, index_path=None)
