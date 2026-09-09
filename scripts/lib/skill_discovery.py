#!/usr/bin/env python3
"""Shared skill-discovery engine for setup.sh, doctor.sh and the load audit.

One resolver probe, one metadata check, one inventory. Every caller renders
from these rows so setup and doctor can never disagree about what is exposed.

The plugin cache is READ-ONLY here: detection only, never repair. Repairs go
through the supported reinstall flow documented in docs/HARNESSES.md.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

ALIAS_PREFIXES = ("fno--", "plugin--fno--")
# The growth-studio pack is Footnote's own; it names no foreign skill today.
# A future explicit dependency is added here, never re-derived from names.
PRESERVE = frozenset()
PLACEHOLDER_MARKERS = ("{{", "placeholder", "<+")
PLACEHOLDER_WORDS = ("todo", "tbd")


@dataclass
class Plugin:
    path: Path
    version: str
    skills_dir: Path


@dataclass
class Entry:
    name: str
    alias: str | None
    source: str | None
    digest: str | None
    owner: str
    status: str
    note: str = ""


def sha256_file(path: Path) -> str | None:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()[:16]
    except OSError:
        return None


def default_plugin_cache() -> Path:
    """Codex installs under CODEX_HOME when set, ~/.codex otherwise."""
    home = os.environ.get("CODEX_HOME") or "~/.codex"
    return Path(home).expanduser() / "plugins" / "cache"


def find_installed_plugin(cache_root: Path | None = None) -> Plugin | None:
    """Newest usable footnote/fno plugin under the codex cache, else None."""
    if cache_root is None:
        cache_root = default_plugin_cache()
    base = cache_root / "footnote" / "fno"
    if not base.is_dir():
        return None
    best: Plugin | None = None
    for child in sorted(base.iterdir()):
        manifest = child / "plugin.json"
        if not manifest.is_file():
            continue
        try:
            data = json.loads(manifest.read_text())
        except (OSError, ValueError):
            continue
        rel = data.get("skills")
        if not rel:
            continue
        skills_dir = (child / rel.lstrip("./")).resolve()
        if not skills_dir.is_dir():
            continue
        candidate = Plugin(path=child, version=child.name, skills_dir=skills_dir)
        if best is None or _version_key(candidate.version) > _version_key(best.version):
            best = candidate
    return best


def _version_key(version: str) -> tuple:
    # Digit-or-not per part keeps prerelease suffixes comparable, never
    # mixed-type (an int/str tuple comparison would raise TypeError).
    return tuple(
        (1, int(p)) if p.isdigit() else (0, p) for p in re.split(r"[.]", version)
    )


def source_skills(repo_root: Path) -> dict[str, Path]:
    skills = repo_root / "skills"
    if not skills.is_dir():
        return {}
    return {
        d.name: d / "SKILL.md"
        for d in sorted(skills.iterdir())
        if (d / "SKILL.md").is_file()
    }


def metadata_problems(md_path: Path, canonical: str | None = None) -> list[str]:
    """Exact reasons a SKILL.md is not a usable discovery entry."""
    try:
        text = md_path.read_text(errors="replace")
    except OSError as exc:
        return [f"unreadable: {exc}"]
    if not text.startswith("---"):
        return ["no frontmatter block"]
    m = re.match(r"^---\n(.*?)\n---\s*(?:\n|$)", text, re.DOTALL)
    if not m:
        return ["frontmatter never closes"]
    raw = m.group(1)
    try:
        import yaml
        data = yaml.safe_load(raw)
    except ImportError:
        data = None  # pyyaml absent: structural checks still run
    except Exception as exc:
        return [f"invalid YAML: {str(exc).splitlines()[0]}"]
    if data is None:
        return ["frontmatter is empty"]
    if not isinstance(data, dict):
        return ["frontmatter is not a mapping"]
    problems: list[str] = []
    desc = data.get("description")
    if desc is None:
        problems.append("description missing")
    else:
        d = str(desc).strip()
        if not d:
            problems.append("description empty")
        elif all(ch in ">-*_" or ch.isspace() for ch in d):
            problems.append(f"description is punctuation ({d!r})")
        elif any(marker in d.lower() for marker in PLACEHOLDER_MARKERS) or any(
            re.search(rf"\b{w}\b", d.lower()) for w in PLACEHOLDER_WORDS
        ):
            problems.append(f"description carries placeholder text ({d[:40]!r})")
    if canonical is not None:
        name = data.get("name")
        if isinstance(name, str) and name.strip() not in ("", canonical):
            problems.append(f"name {name.strip()!r} does not match canonical {canonical!r}")
    return problems


def link_owner(target: Path, repo_root: Path) -> str:
    resolved = target.resolve()
    root = repo_root.resolve()
    if resolved == (root / "skills").resolve() or root in resolved.parents:
        return "footnote"
    return f"external:{resolved}"


def inventory(repo_root: Path, skills_root: Path, plugin: Plugin | None) -> list[Entry]:
    sources = source_skills(repo_root)
    plugin_names = set()
    if plugin is not None:
        plugin_names = {p.name for p in plugin.skills_dir.iterdir() if (p / "SKILL.md").is_file()}
    links: list[tuple[str, Path]] = []
    if skills_root.is_dir():
        for child in sorted(skills_root.iterdir()):
            if child.is_symlink():
                name = child.name
                for prefix in ALIAS_PREFIXES:
                    if name.startswith(prefix):
                        name = name[len(prefix):]
                        break
                links.append((name, child))
    names = sorted(set(sources) | plugin_names | {n for n, _ in links})
    rows: list[Entry] = []
    for name in names:
        linked = [p for n, p in links if n == name]
        if not linked:
            linked = [None]
        for alias in linked:
            alias_status, alias_note = _alias_health(alias, name, sources)
            statuses = [s for s in (alias_status, ) if s not in ("ok", "absent")]
            if name in plugin_names and alias is not None and alias_status != "broken":
                statuses.append("duplicate")
            src = str(alias.resolve()) if alias and alias_status != "broken" else None
            digest = sha256_file(Path(src) / "SKILL.md") if src else None
            owner = "footnote"
            if alias is not None and alias_status != "broken":
                owner = link_owner(alias, repo_root)
            if not statuses:
                if plugin is not None and name not in plugin_names and alias is None:
                    statuses.append("absent-from-plugin")
                else:
                    statuses.append("ok")
            status = statuses[0]
            note = "; ".join(filter(None, [alias_note] + statuses[1:]))
            rows.append(Entry(name, str(alias) if alias else None, src, digest, owner, status, note))
    return rows


def _alias_health(alias: Path | None, name: str, sources: dict[str, Path]) -> tuple[str, str]:
    if alias is None:
        return ("absent", "")
    if not alias.exists():
        return ("broken", "dangling symlink")
    target = alias.resolve()
    md = target / "SKILL.md"
    if not md.is_file():
        return ("stale", f"linked dir has no SKILL.md ({target})")
    problems = metadata_problems(md, name)
    if problems:
        return ("unavailable", "; ".join(problems))
    return ("ok", "")


def curate(skills_root: Path, repo_root: Path, apply: bool = False) -> list[dict]:
    """Remove unrelated FOREIGN symlink exposure; keep real dirs and ours."""
    removals: list[dict] = []
    if not skills_root.is_dir():
        return removals
    for child in sorted(skills_root.iterdir()):
        if not child.is_symlink() or child.name.startswith(ALIAS_PREFIXES):
            continue
        if child.name in PRESERVE:
            continue
        target = Path(os.readlink(child))
        resolved = (child.parent / target).resolve() if not target.is_absolute() else target.resolve()
        in_repo = (repo_root / "skills").resolve() in resolved.parents or resolved == (repo_root / "skills").resolve()
        if in_repo:
            continue
        removals.append({
            "alias": str(child),
            "source": str(resolved),
            "restore": f"ln -sfn '{resolved}' '{child}'",
        })
        if apply:
            child.unlink()
    return removals


def render_table(rows: list[Entry]) -> str:
    header = f"{'name':<22} {'status':<17} {'owner':<10} {'digest':<17} source"
    lines = [header]
    for r in rows:
        src = r.source or (r.note if r.status in ("absent-from-plugin", "unavailable") else "")
        lines.append(f"{r.name:<22} {r.status:<17} {r.owner:<10} {r.digest or '-':<17} {src}")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("probe")
    p.add_argument("--repo", required=True, type=Path)
    p.add_argument("--cache", type=Path, default=None)
    p = sub.add_parser("inventory")
    p.add_argument("--repo", required=True, type=Path)
    p.add_argument("--root", required=True, type=Path)
    p.add_argument("--cache", type=Path, default=None)
    p.add_argument("--json", action="store_true")
    p.add_argument("--tsv", action="store_true", help="name\\tstatus\\towner\\tdigest\\tnote")
    p = sub.add_parser("curate")
    p.add_argument("--repo", required=True, type=Path)
    p.add_argument("--root", required=True, type=Path)
    p.add_argument("--cache", type=Path, default=None)
    p.add_argument("--apply", action="store_true")
    p.add_argument("--json", action="store_true")
    args = ap.parse_args()
    cache = (args.cache or default_plugin_cache()).expanduser()
    plugin = find_installed_plugin(cache)
    if args.cmd == "probe":
        # Absent plugin prints an EMPTY value: consumers select dev mode on -z.
        print(f"plugin={plugin.path if plugin else ''}")
        if plugin is not None:
            print(f"version={plugin.version}")
        return 0
    if args.cmd == "inventory":
        rows = inventory(args.repo, args.root, plugin)
        if args.json:
            print(json.dumps([r.__dict__ for r in rows], indent=2))
        elif getattr(args, "tsv", False):
            for r in rows:
                print(f"{r.name}\t{r.status}\t{r.owner}\t{r.digest or '-'}\t{r.note}")
        else:
            print(render_table(rows))
        return 0
    removals = curate(args.root, args.repo, apply=args.apply)
    if args.json:
        print(json.dumps(removals, indent=2))
    else:
        for r in removals:
            print(f"removed {r['alias']} (foreign source {r['source']})")
            print(f"  restore: {r['restore']}")
        if not removals:
            print("nothing to remove")
    return 0


if __name__ == "__main__":
    sys.exit(main())
