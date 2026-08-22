"""The `doc` deliverable (Group 2, US3): ship a cited brief from the evidence store.

`fno do research` retrieves to a cache `sources.jsonl` (Group 1). The ship step
turns that into a *deliverable*: a markdown brief `<slug>.md` plus its evidence
sidecar `<slug>.sources.jsonl`, both written to `config.research.output_dir`.

Three contracts the eval (US5) leans on:

- Every claim in the brief cites a `[Sn]` marker that resolves, in the brief's
  ``## Sources`` section, to a URL that is a row in the sidecar (AC1, AC2a).
- The sidecar carries the *cited evidence* - the verified rows that back claims
  - so a row is present only after a successful self-fetch (AC2b's clean floor).
- The brief frontmatter always records *why it stopped* (`declared` vs `cap N`),
  so a truncated brief is never silently passed off as complete (AC4).

`output_dir` is fail-loud when unset: the ship step never guesses a landing path
(the `parking_lot_path` lesson, AC5).

This is the Python-level deliverable. A standalone `fno do research` run is its own
terminal: it reports `DoneAdvisory` (the non-PR completion state) rather than
opening a PR.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from fno.events import SchemaUnavailableError, ValidationError, _build, append_event
from fno.research.core import Source, read_sources, slugify

# A claim snippet is short enough to scan, long enough to carry meaning.
_SNIPPET_CHARS = 240


class OutputDirUnset(RuntimeError):
    """`config.research.output_dir` is unset - the ship step fails loud (AC5)."""


@dataclass
class DeliverResult:
    topic: str
    slug: str
    brief_path: str
    sidecar_path: str
    found: int
    verified: int
    terminated: str = "DoneAdvisory"
    # None when no node was in scope, or the bind succeeded. Set to the reason
    # when a node WAS in scope but the bind failed - the deliverable ships
    # either way (x-953b: an unbound doc is the hole this closes, a lost
    # deliverable would be a worse one).
    bind_warning: Optional[str] = None


def resolve_output_dir(configured: Optional[str]) -> Path:
    """Expand + create the configured output dir, or raise OutputDirUnset.

    `configured` is `config.research.output_dir` (None / empty when unset).
    """
    if configured is None or not configured.strip():
        raise OutputDirUnset(
            "config.research.output_dir is unset. Set it (e.g. "
            "`fno config set config.research.output_dir <vault-area>`) - the "
            "research ship step never guesses a landing path."
        )
    out = Path(configured.strip()).expanduser()
    out.mkdir(parents=True, exist_ok=True)
    return out


def _snippet(text: str) -> str:
    s = " ".join((text or "").split())[:_SNIPPET_CHARS].strip()
    return s


def build_brief(
    topic: str,
    slug: str,
    sources: list[Source],
    *,
    stopped: str,
    now: Optional[datetime] = None,
) -> str:
    """Render the brief markdown. One cited claim per verified source.

    `stopped` is recorded verbatim in frontmatter ("declared" or "cap N").
    """
    verified = [s for s in sources if s.verified]
    ts = (now or datetime.now(timezone.utc)).isoformat()

    fm = [
        "---",
        f'topic: "{topic}"',
        f"slug: {slug}",
        f"created: {ts}",
        f"stopped: {stopped}",
        f"sources: {slug}.sources.jsonl",
        f"found: {len(sources)}",
        f"verified: {len(verified)}",
    ]
    if not verified:
        fm.append("note: no sources found")
    fm.append("---")

    body = ["", f"# {topic}", ""]
    if not verified:
        body += [
            "## Findings",
            "",
            "No sources found for this topic. The evidence store is empty; this "
            "brief is stamped accordingly so the eval reports the gap rather than "
            "treating a thin result as complete.",
            "",
        ]
        return "\n".join(fm + body) + "\n"

    body += ["## Findings", ""]
    refs = []
    for i, s in enumerate(verified, start=1):
        snippet = _snippet(s.extract) or "(extracted source; see citation)"
        body.append(f"- {snippet} [S{i}]")
        refs.append(f"[S{i}]: {s.url}")
    body += ["", "## Sources", ""] + refs + [""]
    return "\n".join(fm + body) + "\n"


def _bind_plan_path(node: str, brief_path: Path) -> Optional[str]:
    """Bind the shipped brief to `node` as its plan_path. Returns a warning
    string on failure, None on success.

    x-953b: an advisory brief with no bound node is unreachable from the work
    it answers - the exact hole a second, worse spike walked into. One
    `fno backlog update <id> --plan-path <doc>` call closes it, the same way
    it was closed by hand after the fact. Shells out to the CLI rather than a
    graph-internal call: `backlog update` is the one choke point every write
    to plan_path already goes through (graph/cli.py:713), and duplicating its
    locking/validation here would be a second implementation to diverge from.
    """
    try:
        result = subprocess.run(
            ["fno", "backlog", "update", node, "--plan-path", str(brief_path)],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return f"could not bind {brief_path} to {node}: {exc}"
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        return f"could not bind {brief_path} to {node}: {detail}"
    return None


def deliver(
    topic: str,
    *,
    sources_path: Path,
    stopped: str,
    output_dir: Optional[str],
    now: Optional[datetime] = None,
    node: Optional[str] = None,
) -> DeliverResult:
    """Ship the brief + evidence sidecar to output_dir. Reports DoneAdvisory.

    Reads the cache sources from `sources_path`. The brief cites only *verified*
    rows (claims), but the sidecar carries the **full evidence store** - every
    fetched row, verified or not. That is deliberate: the eval's dead-URL
    assertion reads this sidecar, so dropping failed fetches here would hide
    404s and make the assertion vacuous (the plan's Failure Mode: "404 on
    self-fetch -> mark verified=false; the eval's dead-URL assertion catches
    it"). A brief goes green only if its dead sources were Wayback-archived.

    `node`, when given, is the graph node this deliverable finishes; it gets
    bound to the brief as `plan_path` (best-effort, see `_bind_plan_path`).
    `node=None` (no node in scope) binds nothing, unchanged from before.
    """
    out = resolve_output_dir(output_dir)  # raises OutputDirUnset (AC5)
    slug = slugify(topic)
    sources = read_sources(Path(sources_path))
    verified = [s for s in sources if s.verified]

    brief_path = out / f"{slug}.md"
    sidecar_path = out / f"{slug}.sources.jsonl"

    brief_path.write_text(
        build_brief(topic, slug, sources, stopped=stopped, now=now), encoding="utf-8"
    )
    with sidecar_path.open("w", encoding="utf-8") as fh:
        for s in sources:  # full store, not just verified - keeps dead URLs visible
            fh.write(s.to_json_line() + "\n")

    bind_warning = _bind_plan_path(node, brief_path) if node else None

    return DeliverResult(
        topic=topic,
        slug=slug,
        brief_path=str(brief_path),
        sidecar_path=str(sidecar_path),
        found=len(sources),
        verified=len(verified),
        bind_warning=bind_warning,
    )


def emit_done_advisory(events_path: Path, *, slug: str) -> None:
    """Best-effort: append a DoneAdvisory termination event (mirrors the loop).

    Non-fatal: a missing parent dir or a write error is swallowed - the
    deliverable already landed; the event is an audit convenience, not a gate.
    """
    try:
        if not events_path.parent.is_dir():
            return
        event = _build(
            "termination",
            "hook",
            {
                "session_id": "",
                "reason": "DoneAdvisory",
                "message": f"research deliverable {slug} shipped",
                "slug": slug,
            },
        )
        append_event(event, events_path)
    except (OSError, RuntimeError, SchemaUnavailableError, TimeoutError, ValidationError):
        pass


__all__ = [
    "OutputDirUnset",
    "DeliverResult",
    "resolve_output_dir",
    "build_brief",
    "deliver",
    "emit_done_advisory",
]
