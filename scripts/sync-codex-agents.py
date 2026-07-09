#!/usr/bin/env python3
"""Generate Codex custom-agent TOML from canonical agents/*.md files."""

from __future__ import annotations

import argparse
import difflib
import re
import sys
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError as exc:  # pragma: no cover - environment guard
    raise SystemExit("PyYAML is required: install pyyaml or run via the fno CLI env") from exc


ROOT = Path(__file__).resolve().parents[1]
AGENTS_DIR = ROOT / "agents"
OUT_DIR = ROOT / ".codex" / "agents"
CLAUDE_MODEL_TIERS = {"haiku", "sonnet", "opus", "inherit"}
AGENT_FRONTMATTER_KEYS = {
    "name",
    "description",
    "model",
    "color",
    "tools",
    "disallowedTools",
    "skills",
    "sandbox_mode",
    "nickname_candidates",
}


def split_frontmatter(text: str, path: Path) -> tuple[dict[str, Any], str]:
    if not text.startswith("---\n"):
        raise ValueError(f"{path}: missing YAML frontmatter")
    end = text.find("\n---\n", 4)
    if end == -1:
        raise ValueError(f"{path}: unclosed YAML frontmatter")
    raw = text[4:end]
    body = text[end + len("\n---\n") :].lstrip("\n")
    return parse_agent_frontmatter(raw, path), body


def parse_agent_frontmatter(raw: str, path: Path) -> dict[str, Any]:
    data: dict[str, Any] = {}
    lines = raw.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        if not line.strip():
            i += 1
            continue
        if line.startswith((" ", "\t")) or ":" not in line:
            raise ValueError(f"{path}: malformed frontmatter line: {line!r}")
        key, rest = line.split(":", 1)
        key = key.strip()
        rest = rest.strip()
        if rest in {"|", ">"}:
            block: list[str] = []
            i += 1
            while i < len(lines) and (lines[i].startswith(" ") or not lines[i].strip()):
                block.append(lines[i][2:] if lines[i].startswith("  ") else lines[i].lstrip())
                i += 1
            data[key] = "\n".join(block).rstrip()
            continue
        if rest == "":
            items: list[str] = []
            i += 1
            while i < len(lines) and (lines[i].startswith(" ") or not lines[i].strip()):
                stripped = lines[i].strip()
                if stripped.startswith("- "):
                    items.append(parse_scalar(stripped[2:]))
                elif stripped:
                    raise ValueError(f"{path}: unsupported nested frontmatter line: {lines[i]!r}")
                i += 1
            data[key] = items
            continue
        data[key] = parse_scalar(rest)
        i += 1
        if isinstance(data[key], str):
            continuation: list[str] = []
            while i < len(lines) and not is_key_line(lines[i]):
                continuation.append(lines[i])
                i += 1
            if continuation:
                data[key] = "\n".join([data[key], *continuation]).rstrip()
        continue
    return data


def is_key_line(line: str) -> bool:
    if not line or line.startswith((" ", "\t")) or ":" not in line:
        return False
    key, _rest = line.split(":", 1)
    return key.strip() in AGENT_FRONTMATTER_KEYS


def parse_scalar(value: str) -> Any:
    if value.startswith("["):
        return yaml.safe_load(value)
    if (value.startswith('"') and value.endswith('"')) or (
        value.startswith("'") and value.endswith("'")
    ):
        return yaml.safe_load(value)
    return value


def toml_string(value: str) -> str:
    escaped = (
        value.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\b", "\\b")
        .replace("\f", "\\f")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )
    return f'"{escaped}"'


def toml_multiline(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"""', '\\"\\"\\"')
    return f'"""\n{escaped.rstrip()}\n"""'


def toml_array(values: list[str]) -> str:
    return "[" + ", ".join(toml_string(v) for v in values) + "]"


def list_of_strings(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, list):
        return [str(v) for v in value]
    return [str(value)]


def nickname_candidates(name: str) -> list[str]:
    candidates = [name]
    compact = name.replace("-", "")
    if compact != name:
        candidates.append(compact)
    last = name.split("-")[-1]
    if last not in candidates and len(last) >= 4:
        candidates.append(last)
    return candidates[:3]


def sandbox_mode(tools: list[str]) -> str:
    write_tools = {"Write", "Edit", "MultiEdit", "Bash", "NotebookEdit"}
    return "workspace-write" if write_tools.intersection(tools) else "read-only"


def codex_model(value: Any) -> str | None:
    if not value:
        return None
    model = str(value)
    if model in CLAUDE_MODEL_TIERS:
        return None
    return model


def codex_description(value: Any) -> str:
    text = str(value or "").replace("\\n", "\n").strip()
    for marker in ("\nExamples:", "\n<example>"):
        before, sep, _after = text.partition(marker)
        if sep:
            text = before
            break
    text = re.sub(r"\s+", " ", text).strip()
    if text.startswith("Use this agent when"):
        first_sentence, sep, _rest = text.partition(". ")
        if sep:
            text = first_sentence + "."
    if len(text) <= 360:
        return text
    return text[:357].rsplit(" ", 1)[0].rstrip(".,;:") + "..."


def generated_toml(source: Path) -> str:
    frontmatter, body = split_frontmatter(source.read_text(encoding="utf-8"), source)
    name = str(frontmatter.get("name") or source.stem)
    description = codex_description(frontmatter.get("description"))
    if not description:
        raise ValueError(f"{source}: missing description")

    tools = list_of_strings(frontmatter.get("tools"))
    skills = list_of_strings(frontmatter.get("skills"))
    disallowed = list_of_strings(frontmatter.get("disallowedTools"))
    developer = body.rstrip()
    if skills:
        developer += "\n\n## Source Skills\n\n" + "\n".join(f"- {skill}" for skill in skills)
    if disallowed:
        developer += "\n\n## Disallowed Source Tools\n\n" + "\n".join(
            f"- {tool}" for tool in disallowed
        )

    sandbox = str(frontmatter.get("sandbox_mode") or sandbox_mode(tools))
    nicknames = list_of_strings(frontmatter.get("nickname_candidates")) or nickname_candidates(name)

    lines = [
        "# Generated by scripts/sync-codex-agents.py; do not edit by hand.",
        f"# Source: agents/{source.name}",
        f"name = {toml_string(name)}",
        f"description = {toml_string(description)}",
        f"sandbox_mode = {toml_string(sandbox)}",
        f"nickname_candidates = {toml_array(nicknames)}",
    ]
    model = codex_model(frontmatter.get("model"))
    if model:
        lines.append(f"model = {toml_string(model)}")
    lines.append(f"developer_instructions = {toml_multiline(developer)}")
    return "\n".join(lines) + "\n"


def generated_files() -> dict[Path, str]:
    files: dict[Path, str] = {}
    for source in sorted(AGENTS_DIR.glob("*.md")):
        out = OUT_DIR / f"{source.stem}.toml"
        files[out] = generated_toml(source)
    return files


def stale_files(expected: dict[Path, str]) -> list[Path]:
    if not OUT_DIR.exists():
        return []
    expected_paths = set(expected)
    return sorted(path for path in OUT_DIR.rglob("*.toml") if path not in expected_paths)


def check(expected: dict[Path, str]) -> int:
    failures: list[str] = []
    for path, content in expected.items():
        if not path.exists():
            failures.append(f"missing: {path.relative_to(ROOT)}")
            continue
        actual = path.read_text(encoding="utf-8")
        if actual != content:
            diff = difflib.unified_diff(
                actual.splitlines(),
                content.splitlines(),
                fromfile=str(path.relative_to(ROOT)),
                tofile=f"{path.relative_to(ROOT)} (expected)",
                lineterm="",
            )
            failures.append("\n".join(diff))
    for path in stale_files(expected):
        failures.append(f"stale: {path.relative_to(ROOT)}")
    if failures:
        print("Codex agents are out of sync. Run: python scripts/sync-codex-agents.py", file=sys.stderr)
        print("\n\n".join(failures), file=sys.stderr)
        return 1
    print(f"Codex agents up to date ({len(expected)} files).")
    return 0


def write(expected: dict[Path, str]) -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for path, content in expected.items():
        path.write_text(content, encoding="utf-8")
    for path in stale_files(expected):
        path.unlink()
    print(f"Wrote {len(expected)} Codex agents to {OUT_DIR.relative_to(ROOT)}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if generated files are stale")
    args = parser.parse_args()
    expected = generated_files()
    if args.check:
        return check(expected)
    write(expected)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
