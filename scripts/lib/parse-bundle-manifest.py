#!/usr/bin/env python3
"""parse-bundle-manifest.py -- emit TSV rows from skill-bundles.yaml.

Output format: one TSV row per bundle entry. Columns:

    <type>\\t<skill>\\t<source>\\t<dest>\\t<meta_json>

Where ``type`` is ``file``, ``reference``, or ``agent``. ``meta_json`` is
the empty string for ``file`` / ``reference`` rows and a compact JSON
encoding of the ``subagent_meta`` block for ``agent`` rows.

Stdlib-only for the historical ``files:`` shape so the parser runs in
fresh CI environments before any deps are installed. When the manifest
contains ``references:`` or ``agents:`` blocks, PyYAML is required (the
agents block has nested dicts + lists that don't fit the hand-rolled
fallback parser's shape).

Usage:
    python3 scripts/lib/parse-bundle-manifest.py path/to/skill-bundles.yaml
"""
from __future__ import annotations

import json
import sys
from pathlib import Path


def _load_yaml(path: Path, require_pyyaml: bool):
    """Try PyYAML when available; otherwise hand-roll for the files-only
    shape. ``require_pyyaml=True`` is set by the caller when it detects
    that the manifest declares references: or agents: blocks (which the
    fallback parser cannot handle).
    """
    text = path.read_text(encoding="utf-8")
    try:
        import yaml  # type: ignore
    except ImportError:
        if require_pyyaml:
            raise ValueError(
                "manifest uses references: or agents: blocks; install pyyaml"
            )
        return _parse_minimal_yaml(text)
    try:
        return yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise ValueError(f"YAML parse error: {exc}") from exc


def _manifest_needs_pyyaml(text: str) -> bool:
    """Cheap pre-scan: does the manifest declare references: or agents:?
    Looks for the block header at any non-zero indent (4-space, 2-space,
    or tab — all the indent styles a contributor might use). Comment lines
    are skipped so commented-out block headers don't trigger. False positive
    on a multiline-string literal containing the key is theoretical but the
    bundle manifest doesn't have multiline strings, so the heuristic holds.
    A false positive only costs an unnecessary PyYAML require; a false
    negative would cause confusing parse errors when the fallback parser
    hits an unexpected block header.
    """
    for raw in text.splitlines():
        # Skip commented lines so commented-out examples don't trigger.
        # `#` inside string values is impossible at the block-header layer
        # we care about (block headers are bare keys, never quoted).
        stripped = raw.split("#", 1)[0]
        if stripped != stripped.lstrip() and stripped.lstrip().startswith(
            ("references:", "agents:")
        ):
            return True
    return False


def _parse_minimal_yaml(text: str) -> dict:
    """Minimal parser for the files-only shape:

        bundles:
          - skill: <name>
            files:
              - source: <path>
                dest: <path>

    Comments (#) and blank lines are skipped. Indentation is two-space.
    Anything outside that shape raises ValueError.
    """
    lines = []
    for raw in text.splitlines():
        stripped = raw.split("#", 1)[0].rstrip()
        if not stripped.strip():
            continue
        lines.append(stripped)

    bundles: list[dict] = []
    current_skill: dict | None = None
    current_file: dict | None = None
    in_bundles = False
    in_files = False

    for ln in lines:
        if ln == "bundles:":
            in_bundles = True
            continue
        if not in_bundles:
            continue

        # 2-space indent: "  - skill: name"
        if ln.startswith("  - skill:"):
            current_skill = {"skill": ln.split(":", 1)[1].strip(), "files": []}
            bundles.append(current_skill)
            in_files = False
            current_file = None
            continue
        if ln.startswith("    files:"):
            in_files = True
            continue
        if in_files and ln.startswith("      - source:"):
            if current_skill is None:
                raise ValueError(f"source: line outside any skill block: {ln!r}")
            current_file = {"source": ln.split(":", 1)[1].strip()}
            current_skill["files"].append(current_file)
            continue
        if in_files and ln.startswith("        dest:"):
            if current_file is None:
                raise ValueError(f"dest: line without preceding source: {ln!r}")
            current_file["dest"] = ln.split(":", 1)[1].strip()
            continue
        # Tolerate blank/unknown but report rather than silently skip.
        raise ValueError(f"unexpected line in manifest: {ln!r}")

    return {"bundles": bundles}


def _emit_files(skill: str, files: list) -> int:
    """Emit ``file`` rows. Returns 0 on success, 1 on malformed entry."""
    for f in files:
        if not isinstance(f, dict):
            print(f"ERROR: file entry not a map: {f!r}", file=sys.stderr)
            return 1
        source = f.get("source")
        dest = f.get("dest")
        if not source or not dest:
            print(f"ERROR: file entry missing source/dest: {f!r}", file=sys.stderr)
            return 1
        print(f"file\t{skill}\t{source}\t{dest}\t")
    return 0


def _emit_references(skill: str, references: list) -> int:
    """Emit ``reference`` rows. Returns 0 on success, 1 on malformed entry."""
    for r in references:
        if not isinstance(r, dict):
            print(f"ERROR: reference entry not a map: {r!r}", file=sys.stderr)
            return 1
        source = r.get("source")
        dest = r.get("dest")
        if not source or not dest:
            print(
                f"ERROR: reference entry missing source/dest: {r!r}",
                file=sys.stderr,
            )
            return 1
        print(f"reference\t{skill}\t{source}\t{dest}\t")
    return 0


def _emit_agents(skill: str, agents: list) -> int:
    """Emit ``agent`` rows with JSON-encoded subagent_meta. Returns 0 on
    success, 1 on malformed entry."""
    for a in agents:
        if not isinstance(a, dict):
            print(f"ERROR: agent entry not a map: {a!r}", file=sys.stderr)
            return 1
        source = a.get("source")
        dest = a.get("dest")
        if not source or not dest:
            print(f"ERROR: agent entry missing source/dest: {a!r}", file=sys.stderr)
            return 1
        rewrite = a.get("rewrite_frontmatter")
        if rewrite != "subagent":
            print(
                f"ERROR: agent entry must declare rewrite_frontmatter: subagent, got {rewrite!r}",
                file=sys.stderr,
            )
            return 1
        meta = a.get("subagent_meta")
        if not isinstance(meta, dict):
            print(
                f"ERROR: agent entry missing subagent_meta mapping: {a!r}",
                file=sys.stderr,
            )
            return 1
        # Compact JSON: no whitespace, single line; safe to ship as a TSV column.
        meta_json = json.dumps(meta, separators=(",", ":"), sort_keys=False)
        # Defensive: TSV columns cannot contain tab characters. Reject if
        # the JSON ends up with embedded tabs (impossible for compact dumps
        # of plain strings, but pin the invariant).
        if "\t" in meta_json or "\n" in meta_json:
            print(
                f"ERROR: agent meta JSON contains tab/newline; refusing to emit",
                file=sys.stderr,
            )
            return 1
        print(f"agent\t{skill}\t{source}\t{dest}\t{meta_json}")
    return 0


def emit_rows(manifest_path: Path) -> int:
    if not manifest_path.is_file():
        print(f"ERROR: manifest not found: {manifest_path}", file=sys.stderr)
        return 1
    text = manifest_path.read_text(encoding="utf-8")
    needs_pyyaml = _manifest_needs_pyyaml(text)
    try:
        data = _load_yaml(manifest_path, require_pyyaml=needs_pyyaml)
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    if not isinstance(data, dict) or "bundles" not in data:
        print("ERROR: manifest missing top-level 'bundles' key", file=sys.stderr)
        return 1

    for entry in data["bundles"]:
        if not isinstance(entry, dict):
            print(f"ERROR: bundle entry not a map: {entry!r}", file=sys.stderr)
            return 1
        skill = entry.get("skill")
        if not skill:
            print(f"ERROR: bundle entry missing skill: {entry!r}", file=sys.stderr)
            return 1
        files = entry.get("files") or []
        references = entry.get("references") or []
        agents = entry.get("agents") or []
        for block, name in (
            (files, "files"),
            (references, "references"),
            (agents, "agents"),
        ):
            if not isinstance(block, list):
                print(
                    f"ERROR: bundle {skill!r} block {name!r} not a list: {block!r}",
                    file=sys.stderr,
                )
                return 1
        if _emit_files(skill, files) != 0:
            return 1
        if _emit_references(skill, references) != 0:
            return 1
        if _emit_agents(skill, agents) != 0:
            return 1
    return 0


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: parse-bundle-manifest.py <skill-bundles.yaml>", file=sys.stderr)
        return 2
    return emit_rows(Path(argv[1]))


if __name__ == "__main__":
    sys.exit(main(sys.argv))
