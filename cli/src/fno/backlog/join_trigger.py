"""The read-only probes `fno do target init` shells out to for join.

Init's ``join: auto`` trigger needs two facts before it fires
`fno backlog join`: the plan's ready-graph width, and whether auto-continue
is armed. Both facts have exactly one canonical implementation each
(``_width_from_graph`` and ``_auto_continue_resolve``, which layers the
autonomy master switch over the env override over config), and bash
re-implementing either precedence chain is how the two drift. So init asks
this module instead:

    python -m fno.backlog.join_trigger width <plan-path>
    python -m fno.backlog.join_trigger armed <project-root>

``width`` prints the measured int and exits 0, or exits 1 with no output
when the plan cannot be measured - an absent answer is never a width of
one, so the caller can refuse to fire on anything it did not measure.
``armed`` always answers: ``armed=<true|false> rank=<name>``, exit 0.
"""
from __future__ import annotations

import sys
from pathlib import Path


def _width(plan_path: str) -> int:
    from fno.backlog.advance import _plan_task_graph, _width_from_graph

    graph = _plan_task_graph(Path(plan_path))
    return _width_from_graph(graph)


def _armed(project_root: str) -> tuple[bool, str]:
    from fno.backlog.advance import _auto_continue_resolve

    return _auto_continue_resolve(Path(project_root) if project_root else None)


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) != 2 or args[0] not in {"width", "armed"}:
        print(
            "usage: python -m fno.backlog.join_trigger width <plan-path>\n"
            "       python -m fno.backlog.join_trigger armed <project-root>",
            file=sys.stderr,
        )
        return 2

    if args[0] == "width":
        try:
            width = _width(args[1])
        except Exception:
            # Unreadable plan: the init gate surfaces real refusals
            # elsewhere. Here an unmeasured width must not read as 1.
            return 1
        if width <= 0:
            return 1
        print(width)
        return 0

    armed, rank = _armed(args[1])
    print(f"armed={'true' if armed else 'false'} rank={rank}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
