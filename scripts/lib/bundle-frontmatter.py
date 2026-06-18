#!/usr/bin/env python3
"""bundle-frontmatter.py -- strip or rewrite YAML frontmatter on bundled content.

Invoked by ``scripts/generate-skill-bundles.sh`` when bundling content under
``references:`` (strip frontmatter) or ``agents:`` (rewrite frontmatter as
subagent prompt). Reads source from disk, writes transformed body to stdout.

Commands:

    bundle-frontmatter.py strip <source_path>
        Print everything after the closing ``---`` of the source file's
        frontmatter. If the source has no frontmatter, print the whole file.

    bundle-frontmatter.py rewrite <source_path> --as subagent \\
        --meta-file <yaml_file>
        Strip the source's frontmatter, render the YAML at ``--meta-file`` as
        a new frontmatter block, prepend it to the body, print to stdout.
        Required meta fields for ``--as subagent``: name, description, model,
        tools. Missing fields raise non-zero exit and an error on stderr.

PyYAML is required (CLI dep). Hand-rolling YAML for the subagent_meta block
would be inappropriate for nested dicts + lists.
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

try:
    import yaml  # type: ignore
except ImportError:
    print("ERROR: PyYAML not available; install pyyaml", file=sys.stderr)
    sys.exit(2)


_REQUIRED_SUBAGENT_FIELDS = ("name", "description", "model", "tools")


class UnterminatedFrontmatterError(ValueError):
    """Source begins with --- but has no closing fence. Treating the body
    as 'everything after the fence' is wrong, and treating it as 'whole file'
    silently produces broken bundles. Raise so callers fail loudly with the
    source path included in the error.
    """


def _split_frontmatter(text: str) -> tuple[str, str]:
    """Return (frontmatter_text, body_text). frontmatter_text is empty if
    the source has no leading frontmatter block. Raises
    UnterminatedFrontmatterError on a source that opens with --- but never
    closes - the caller is expected to surface the source path.
    """
    if not text.startswith("---\n"):
        return "", text
    end = text.find("\n---\n", 4)
    if end == -1:
        raise UnterminatedFrontmatterError(
            "unterminated frontmatter (source begins with --- but has no closing fence)"
        )
    frontmatter = text[4:end]
    body = text[end + len("\n---\n") :]
    return frontmatter, body


def _render_subagent_frontmatter(meta: dict) -> str:
    """Render a subagent frontmatter block from the meta dict. Validates
    required fields and the shape of value types that downstream agent
    loaders care about. Returns the full ``---\\n...\\n---\\n`` block.
    """
    missing = [f for f in _REQUIRED_SUBAGENT_FIELDS if f not in meta]
    if missing:
        raise ValueError(
            f"subagent_meta missing required fields: {', '.join(missing)}"
        )
    # `tools` must be a list. Agent loaders (Claude Code, Codex, etc.) parse
    # this as a YAML sequence; a bare scalar like `tools: Read` would render
    # as a string and silently break agent registration. Catch at bundle
    # time with a clear error rather than at runtime with a cryptic one.
    tools = meta.get("tools")
    if not isinstance(tools, list):
        raise ValueError(
            f"subagent_meta 'tools' must be a YAML list, got {type(tools).__name__}"
        )
    if not all(isinstance(t, str) for t in tools):
        raise ValueError(
            "subagent_meta 'tools' entries must be strings"
        )
    # Render with PyYAML for consistent formatting. default_flow_style=False
    # gives block-style output; sort_keys=False preserves authoring order.
    body = yaml.safe_dump(
        meta,
        default_flow_style=False,
        sort_keys=False,
        allow_unicode=True,
        width=10_000,  # avoid line-wrapping in long descriptions
    )
    return f"---\n{body}---\n"


def cmd_strip(args: argparse.Namespace) -> int:
    src = Path(args.source)
    if not src.is_file():
        print(f"ERROR: source not found: {src}", file=sys.stderr)
        return 1
    text = src.read_text(encoding="utf-8")
    try:
        _, body = _split_frontmatter(text)
    except UnterminatedFrontmatterError as exc:
        print(f"ERROR: {exc} in {src}", file=sys.stderr)
        return 1
    sys.stdout.write(body)
    return 0


def cmd_json_to_yaml(args: argparse.Namespace) -> int:
    """Convert a compact JSON object (as passed by the generator from the
    parser's meta_json TSV column) to block-style YAML. Same dump
    parameters as ``_render_subagent_frontmatter`` so the round-trip
    through bash preserves the rendering invariants (width=10_000 to
    avoid long-description line-wrap, sort_keys=False to preserve
    authoring order, allow_unicode=True).
    """
    try:
        import json
        meta = json.loads(args.json)
    except json.JSONDecodeError as exc:
        print(f"ERROR: json parse error: {exc}", file=sys.stderr)
        return 1
    if not isinstance(meta, dict):
        print(
            f"ERROR: json must decode to a mapping, got {type(meta).__name__}",
            file=sys.stderr,
        )
        return 1
    sys.stdout.write(
        yaml.safe_dump(
            meta,
            default_flow_style=False,
            sort_keys=False,
            allow_unicode=True,
            width=10_000,
        )
    )
    return 0


def cmd_rewrite(args: argparse.Namespace) -> int:
    if args.as_ != "subagent":
        print(
            f"ERROR: only --as subagent is supported (got {args.as_!r})",
            file=sys.stderr,
        )
        return 1
    src = Path(args.source)
    if not src.is_file():
        print(f"ERROR: source not found: {src}", file=sys.stderr)
        return 1
    meta_file = Path(args.meta_file)
    if not meta_file.is_file():
        print(f"ERROR: meta file not found: {meta_file}", file=sys.stderr)
        return 1
    try:
        meta = yaml.safe_load(meta_file.read_text(encoding="utf-8"))
    except yaml.YAMLError as exc:
        print(f"ERROR: meta YAML parse error: {exc}", file=sys.stderr)
        return 1
    if not isinstance(meta, dict):
        print(
            f"ERROR: meta file must contain a YAML mapping, got {type(meta).__name__}",
            file=sys.stderr,
        )
        return 1
    try:
        new_frontmatter = _render_subagent_frontmatter(meta)
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    text = src.read_text(encoding="utf-8")
    try:
        _, body = _split_frontmatter(text)
    except UnterminatedFrontmatterError as exc:
        print(f"ERROR: {exc} in {src}", file=sys.stderr)
        return 1
    sys.stdout.write(new_frontmatter + body)
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="bundle-frontmatter.py")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_strip = sub.add_parser("strip", help="print body without frontmatter")
    p_strip.add_argument("source", help="source markdown file")
    p_strip.set_defaults(func=cmd_strip)

    p_j2y = sub.add_parser(
        "json-to-yaml",
        help="convert compact JSON to block-style YAML (for bundler shell-out)",
    )
    p_j2y.add_argument("json", help="compact JSON string (e.g. parser meta column)")
    p_j2y.set_defaults(func=cmd_json_to_yaml)

    p_rewrite = sub.add_parser("rewrite", help="strip + prepend new frontmatter")
    p_rewrite.add_argument("source", help="source markdown file")
    p_rewrite.add_argument(
        "--as",
        dest="as_",
        choices=["subagent"],
        required=True,
        help="frontmatter rewrite mode",
    )
    p_rewrite.add_argument(
        "--meta-file",
        required=True,
        help="path to YAML file containing subagent_meta",
    )
    p_rewrite.set_defaults(func=cmd_rewrite)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
