#!/usr/bin/env python3
"""Audit codex rollout transcripts for skill-load by prompt shape.

Answers one question: when a codex worker is spawned with a
wrapped prose prompt ("run $fno:target x-.... <prose>") instead of a bare
verb-first prompt ("$fno:target x-...."), does the skill body reach context
before the first tool call?

Markers are POSITIVE, never absences (AGENTS.md pitfalls), and NAME-SPECIFIC:
the verdict is whether the skill the prompt NAMES (`$fno:target` -> target)
reached context, not whether any skill did. Subagent children legitimately
read their own executor skill first; that must not read as a target load.
  loaded-injected: a user-role message matching <skill> <name>...</name>
      for the named skill arrives before the first tool call. This is codex
      resolving a $command token to the plugin skill cache; no fno code emits
      this wrapper.
  loaded-selfread: a tool input references skills/<named-skill>/SKILL.md
      within the first three tool calls (the worker read it as first action).
  not-loaded: the literal $fno:/fno: token survives verbatim in the prompt
      and neither marker fires. The surviving token is itself positive
      evidence of non-expansion.

Usage:
  python3 scripts/diagnostics/codex-skill-load-audit.py --date 2026-08-18 --hour-from 17
  python3 scripts/diagnostics/codex-skill-load-audit.py FILE.jsonl [FILE.jsonl ...]
  python3 scripts/diagnostics/codex-skill-load-audit.py --self-check
"""

import argparse
import glob
import json
import os
import re
import sys
import tempfile
from pathlib import Path

SKILL_MSG = re.compile(r"<skill>\s*<name>([a-zA-Z0-9:_-]+)</name>")
SKILL_PATH = re.compile(r"skills/[a-z0-9-]+/SKILL\.md")
VERB_TOKEN = re.compile(r"[$/]fno:[a-z0-9-]+")
NOISE_PREFIXES = (
    "# AGENTS.md",
    "<recommended_plugins>",
    "## Memory",
    "You are `/root`",
    "<multi_agent_mode",
    "<EXTREMELY_IMPORTANT",
    "Warning: truncated",
)
TOOL_TYPES = ("custom_tool_call", "function_call", "local_shell_call")


def classify(path):
    """Return (shape, loaded, how, named_skill, prompt_head) for one rollout."""
    prompt = None
    skills_injected = []
    skill_reads = []
    tool_calls = []  # input text, in order
    with open(path, errors="replace") as fh:
        for line in fh:
            try:
                d = json.loads(line)
            except Exception:
                continue
            t, p = d.get("type"), d.get("payload")
            if not isinstance(p, dict):
                continue
            if t == "event_msg" and p.get("type") == "user_message":
                if prompt is None:
                    prompt = p.get("message", "")
                m = SKILL_MSG.search(p.get("message", ""))
                if m and not tool_calls:
                    skills_injected.append(m.group(1))
            elif t == "response_item" and p.get("type") == "message" and p.get("role") == "user":
                txt = " ".join(
                    c.get("text", "") for c in p.get("content", []) if isinstance(c, dict)
                )
                if txt.startswith(NOISE_PREFIXES):
                    continue
                m = SKILL_MSG.search(txt)
                if m:
                    if not tool_calls:
                        skills_injected.append(m.group(1))
                elif prompt is None:
                    prompt = txt
            elif t == "response_item" and p.get("type") in TOOL_TYPES:
                tool_calls.append(str(p.get("input") or p.get("arguments") or ""))

    head = " ".join((prompt or "").split())
    named = VERB_TOKEN.search(prompt or "")
    if (prompt or "").lstrip().startswith(("$", "/")) and named:
        shape = "verb-first"
    elif named:
        shape = "wrapped"
    else:
        shape = "prose"
    named_skill = named.group(0).lstrip("$/") if named else None  # "fno:target" or bare verb
    named_dir = named_skill.split(":")[-1] if named_skill else None
    if named_dir and any(s == named_skill for s in skills_injected):
        return shape, True, "injected", named_skill, head
    if named_dir:
        for inp in tool_calls[:3]:
            if f"skills/{named_dir}/SKILL.md" in inp:
                return shape, True, "selfread", named_skill, head
    return shape, False, "not-loaded", named_skill, head


def audit(paths):
    rows = []
    for path in sorted(paths):
        shape, loaded, how, named, head = classify(path)
        rows.append((os.path.basename(path), shape, loaded, how, named, head))
    counts = {}
    for _, shape, loaded, _, _, _ in rows:
        key = (shape, "loaded" if loaded else "not-loaded")
        counts[key] = counts.get(key, 0) + 1
    for name, shape, loaded, how, named, head in rows:
        print(f"{name[8:24]}  {shape:<10} {'LOADED' if loaded else 'NOLOAD':<7} {how:<11} {str(named) or '-':<12} {head[:60]}")
    print()
    print("summary (shape x load):")
    for (shape, verdict), n in sorted(counts.items()):
        print(f"  {shape:<10} {verdict:<11} {n}")
    return rows, counts


def self_check():
    """Synthetic transcripts pinning the classifier; exits nonzero on mismatch."""
    def rollout(events):
        return "".join(json.dumps(e) + "\n" for e in events)

    def usermsg(text):
        return {"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}]}}

    def tool(inp):
        return {"type": "response_item", "payload": {"type": "custom_tool_call",
                "name": "exec", "input": inp}}

    cases = [
        # wrapped + injected before first tool -> loaded
        ("wrapped-inject", [usermsg("run $fno:target x-1 do the thing"),
                            usermsg("<skill> <name>fno:target</name> <path>p</path> body"),
                            tool("fno backlog get x-1")], ("wrapped", True, "injected")),
        # wrapped, token survives verbatim, straight to work -> not loaded
        ("wrapped-noinject", [usermsg("run $fno:target x-2 read node first"),
                              tool("fno backlog get x-2")], ("wrapped", False, "not-loaded")),
        # wrapped, first action reads the skill -> loaded by self-read
        ("wrapped-selfread", [usermsg("run $fno:target x-3 read node"),
                              tool("sed -n 1p /cache/skills/target/SKILL.md")], ("wrapped", True, "selfread")),
        # bare verb-first prompt
        ("verb-first", [usermsg("$fno:target x-4"), tool("fno whoami")], ("verb-first", False, "not-loaded")),
        # noise messages must not become the prompt
        ("noise", [usermsg("# AGENTS.md instructions <INSTRUCTIONS>x"),
                   usermsg("run $fno:target x-5 go"), tool("git status")], ("wrapped", False, "not-loaded")),
    ]
    failures = 0
    with tempfile.TemporaryDirectory() as td:
        for label, events, want in cases:
            p = os.path.join(td, label + ".jsonl")
            with open(p, "w") as fh:
                fh.write(rollout(events))
            shape, loaded, how = classify(p)[:3]
            got = (shape, loaded, how)
            status = "ok" if got == want else "FAIL"
            if got != want:
                failures += 1
            print(f"self-check {label:<18} {status}  want={want} got={got}")
    return failures


def discovery(repo, skills_root, plugin_cache):
    """Compare actually exposed names and resolved content per canonical skill.

    Distinguishes the three surfaces a name can load from: the installed
    plugin's cache copy (a generated distribution bundle), a project alias
    symlink (resolves to source), and the source skills/ tree itself. A
    stale cache is named with its digest gap and the supported repair;
    a private cache is never repaired here (docs/HARNESSES.md).
    """
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
    import skill_discovery as sd

    repo, skills_root, plugin_cache = Path(repo), Path(skills_root), Path(plugin_cache)
    plugin = sd.find_installed_plugin(plugin_cache.expanduser())
    sources = sd.source_skills(repo)
    rows = sd.inventory(repo, skills_root, plugin)
    cache_digs: dict[str, str] = {}
    if plugin is not None:
        for p in sorted(plugin.skills_dir.iterdir()):
            if (p / "SKILL.md").is_file():
                cache_digs[p.name] = sd.sha256_file(p / "SKILL.md") or "-"
    print(f"repo: {repo}")
    print(f"plugin: {plugin.path if plugin else 'none'}")
    print(f"{'name':<22} {'verdict':<18} {'source-digest':<18} loaded-from")
    for r in rows:
        cache_dig = cache_digs.get(r.name)
        if r.name not in sources:
            verdict, loaded = "plugin-only", str(plugin.skills_dir / r.name) if plugin else "-"
        elif plugin and cache_dig is not None and cache_dig != r.digest and r.owner == "footnote":
            verdict = "stale-cache"
            loaded = f"{plugin.skills_dir / r.name} (cache {cache_dig} != source {r.digest}); repair: reinstall via the supported flow (docs/HARNESSES.md)"
        elif r.alias:
            verdict = "one-source(alias)" if r.status == "ok" else r.status
            loaded = f"{r.source} via {r.alias}"
        elif plugin and r.name in cache_digs:
            verdict, loaded = "one-source(plugin)", str(plugin.skills_dir / r.name)
        else:
            verdict, loaded = "unexposed", "-"
        print(f"{r.name:<22} {verdict:<18} {r.digest or '-':<18} {loaded}")
    print()
    print("other harness discovery (claude, opencode, ...): unmeasured - this audit reads codex surfaces only")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("files", nargs="*", help="rollout .jsonl files (overrides --date)")
    ap.add_argument("--date", help="YYYY-MM-DD; scans ~/.codex/sessions/YYYY/MM/DD/")
    ap.add_argument("--hour-from", type=int, default=0, help="only files at/after this local hour")
    ap.add_argument("--self-check", action="store_true", help="run synthetic classifier checks")
    ap.add_argument("--discovery", action="store_true", help="compare exposed skill names and content against source")
    ap.add_argument("--repo", help="repository root whose skills/ tree is the source of truth")
    ap.add_argument("--skills-root", help="project discovery root holding the alias links")
    ap.add_argument("--plugin-cache", default="~/.codex/plugins/cache", help="codex plugin cache root (read-only)")
    args = ap.parse_args()

    if args.self_check:
        sys.exit(1 if self_check() else 0)

    if args.discovery:
        if not (args.repo and args.skills_root):
            ap.error("--discovery needs --repo and --skills-root")
        discovery(args.repo, args.skills_root, args.plugin_cache)
        return

    if args.files:
        paths = args.files
    elif args.date:
        y, m, d = args.date.split("-")
        base = os.path.expanduser(f"~/.codex/sessions/{y}/{m}/{int(d):02d}")
        paths = glob.glob(os.path.join(base, "*.jsonl"))
        if args.hour_from:
            hh = f"rollout-{args.date}T{args.hour_from:02d}"
            paths = [p for p in paths if os.path.basename(p) >= hh]
    else:
        ap.error("give FILE(s), --date, or --self-check")
    if not paths:
        print("no transcripts matched", file=sys.stderr)
        sys.exit(1)
    audit(paths)


if __name__ == "__main__":
    main()
