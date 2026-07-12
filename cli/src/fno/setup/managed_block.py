"""Offered, marker-fenced footnote block for a host AGENTS.md / CLAUDE.md.

Footnote guidance reaches claude sessions via the SessionStart using-fno
injection, but codex/gemini (and humans) read the host project's AGENTS.md /
CLAUDE.md natively - those audiences get nothing today. This module appends a
small `<!-- fno:begin v=N -->` / `<!-- fno:end -->` block to the host file so
they do.

Three hard rules, all enforced here:

- **Never forced.** ``offer_managed_block`` writes only after ``confirm_fn``
  returns True. A first-time decline is durable (a marker under ``.fno/``), so a
  later ``fno setup`` never re-nags.
- **Only its own fences.** ``stamp_block`` splices the region between the markers
  and preserves every byte outside them; a re-stamp replaces only the fenced
  content.
- **Refuse on malformed.** Exactly one marker present -> there is no valid region
  to replace, so it touches nothing and says so.

``fno doctor`` reads ``stamped_version`` to flag a block older than
``BLOCK_VERSION`` (advisory).
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

# Bump when the block body below changes so `fno doctor` flags stale copies.
BLOCK_VERSION = 1

_BEGIN_PREFIX = "<!-- fno:begin"
_END_MARKER = "<!-- fno:end -->"
_BEGIN_RE = re.compile(r"<!-- fno:begin v=(\d+) -->")

# The pointer set (discretion: keep it a pointer set, not a manual). Covers
# forbidden state surfaces, the two-surfaces model, the W2 fold-in policy,
# worktree-first, and the cancel signal.
_BODY = """\
## footnote

This repo uses the footnote delivery pipeline. Two surfaces that compose:

- **`fno` CLI** - atomic state ops: `fno backlog` (the feature graph), `fno pr`,
  `fno mail`, `fno carveout`. Run `fno help` for the catalog.
- **`/fno:*` commands** - orchestration: `/fno:target` (idea -> shipped PR),
  `/fno:think`, `/fno:review`, `/fno:pr`, `/fno:fix`.

Never hand-edit these state files (a hook rejects it): `~/.fno/graph.json` (use
`fno backlog`) and `.fno/target-state.md` (immutable after `fno target init`).

Worktree-first: for repo work use a dedicated feature worktree; keep the main
checkout pullable. Cancel a running pipeline with `touch .fno/.target-cancelled`.

Spot a small pre-existing bug while building? Fold the fix into the current PR as
its own atomic commit. Capture non-small finds with `fno carveout add`."""


def _begin_marker(version: int) -> str:
    return f"<!-- fno:begin v={version} -->"


def render_block(version: int = BLOCK_VERSION) -> str:
    """The full fenced block, markers included."""
    return f"{_begin_marker(version)}\n{_BODY}\n{_END_MARKER}"


def stamped_version(text: str) -> Optional[int]:
    """The version stamped in a file's block, or None if it carries no block."""
    m = _BEGIN_RE.search(text)
    return int(m.group(1)) if m else None


def marker_state(text: str) -> str:
    """`none` (no markers), `both` (a well-formed pair), or `malformed` (one)."""
    has_begin = _BEGIN_PREFIX in text
    has_end = _END_MARKER in text
    if has_begin and has_end:
        return "both"
    if has_begin or has_end:
        return "malformed"
    return "none"


@dataclass
class StampResult:
    action: str  # created | appended | restamped | current | refused-malformed
    version: Optional[int]
    path: Path


def resolve_target(repo_root: Path) -> Path:
    """The host instruction file to stamp: an existing AGENTS.md, else an existing
    CLAUDE.md, else AGENTS.md (created only on an explicit accept)."""
    repo_root = Path(repo_root)
    for name in ("AGENTS.md", "CLAUDE.md"):
        p = repo_root / name
        if p.is_file():
            return p
    return repo_root / "AGENTS.md"


def stamp_block(path: Path, *, version: int = BLOCK_VERSION) -> StampResult:
    """Stamp the block into ``path``, preserving every byte outside the markers.

    Missing file -> created. No markers -> appended. A well-formed pair ->
    replaced in place (or ``current`` if already byte-identical). Exactly one
    marker -> ``refused-malformed``, touching nothing.
    """
    path = Path(path)
    block = render_block(version)
    if not path.exists():
        path.write_text(block + "\n", encoding="utf-8")
        return StampResult("created", version, path)

    text = path.read_text(encoding="utf-8")
    state = marker_state(text)
    if state == "malformed":
        return StampResult("refused-malformed", None, path)

    if state == "none":
        if not text.strip():
            # Empty / whitespace-only file: no prose to separate from.
            path.write_text(block + "\n", encoding="utf-8")
        else:
            # Append with exactly one blank line of separation, whatever the
            # file's trailing whitespace was.
            sep = "" if text.endswith("\n\n") else "\n" if text.endswith("\n") else "\n\n"
            path.write_text(text + sep + block + "\n", encoding="utf-8")
        return StampResult("appended", version, path)

    # state == "both": splice the fenced region, preserving outside bytes exactly.
    start = text.index(_BEGIN_PREFIX)
    end = text.index(_END_MARKER) + len(_END_MARKER)
    if end < start:  # markers out of order -> no valid region
        return StampResult("refused-malformed", None, path)
    if text[start:end] == block:
        return StampResult("current", version, path)
    path.write_text(text[:start] + block + text[end:], encoding="utf-8")
    return StampResult("restamped", version, path)


def _decline_marker(repo_root: Path) -> Path:
    return Path(repo_root) / ".fno" / ".managed-block-declined"


def offer_managed_block(
    repo_root: Path,
    *,
    confirm_fn: Callable[[str], bool],
    echo_fn: Callable[[str], None] = lambda _m: None,
    version: int = BLOCK_VERSION,
) -> dict[str, str]:
    """Offer the managed block for the host's AGENTS.md/CLAUDE.md (opt-in).

    Returns a ``{"status": ..., "path": ...}`` receipt. Never writes without an
    explicit ``confirm_fn`` yes; a first-time decline is remembered so setup does
    not re-ask.
    """
    repo_root = Path(repo_root)
    target = resolve_target(repo_root)
    decline = _decline_marker(repo_root)
    text = target.read_text(encoding="utf-8") if target.is_file() else ""
    state = marker_state(text)

    if state == "malformed":
        echo_fn(
            f"  {target.name}: has an unmatched fno:begin/fno:end marker - "
            "fix it by hand; nothing was written."
        )
        return {"status": "refused-malformed", "path": str(target)}

    stamped = stamped_version(text)
    if state == "both" and stamped == version:
        return {"status": "current", "path": str(target)}

    # No block yet AND a durable decline on record -> honor it, don't re-ask.
    if state == "none" and decline.exists():
        return {"status": "declined-remembered", "path": str(target)}

    if stamped is None:
        prompt = (
            f"Add a footnote guidance block to {target.name}? "
            "(marker-fenced; only ever touches its own fences)"
        )
    else:
        prompt = f"Update the footnote guidance block in {target.name} to v{version}?"

    if not confirm_fn(prompt):
        # Only a first-time add is remembered; a declined update leaves the
        # existing (older) block, which doctor flags advisorily instead.
        if stamped is None:
            try:
                decline.parent.mkdir(parents=True, exist_ok=True)
                decline.write_text("", encoding="utf-8")
            except OSError:
                pass
        return {"status": "declined", "path": str(target)}

    res = stamp_block(target, version=version)
    if res.action == "refused-malformed":
        echo_fn(
            f"  {target.name}: has an unmatched fno:begin/fno:end marker - "
            "fix it by hand; nothing was written."
        )
        return {"status": "refused-malformed", "path": str(target)}
    # An accept makes any prior decline moot.
    try:
        if decline.exists():
            decline.unlink()
    except OSError:
        pass
    echo_fn(f"  {target.name}: footnote guidance block {res.action} (v{version}).")
    return {"status": res.action, "path": str(target)}
