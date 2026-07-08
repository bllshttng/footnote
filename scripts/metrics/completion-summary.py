#!/usr/bin/env python3
"""Generate a human-readable completion summary from target state files.

Called by the stop hook after artifact archival. Reads target-state.md,
HANDOFF.md, and git log to produce a single markdown file visible in Obsidian.

Usage:
    python3 completion-summary.py <target-state-path> [--plan-dir DIR] [--output PATH]

Output path resolution (first match wins):
    1. --output CLI argument
    2. config.completions_path in .fno/config.toml (project)
    3. config.completions_path in ~/.fno/config.toml (global)
    4. {plan_dir}/COMPLETION.md (fallback)

Prints the generated summary path to stdout for the stop hook to capture.
"""

import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


def parse_frontmatter(path: str) -> dict:
    """Parse YAML frontmatter from a markdown file into a dict."""
    result = {}
    try:
        with open(path) as f:
            content = f.read()
    except (FileNotFoundError, PermissionError):
        return result

    parts = content.split("---")
    if len(parts) < 3:
        return result

    for line in parts[1].strip().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        match = re.match(r"^(\w[\w_]*):\s*(.*)", line)
        if match:
            key, value = match.group(1), match.group(2).strip()
            if value.startswith('"') and value.endswith('"'):
                value = value[1:-1]
            elif value.startswith("'") and value.endswith("'"):
                value = value[1:-1]
            if value in ("null", ""):
                value = None
            elif value == "true":
                value = True
            elif value == "false":
                value = False
            result[key] = value

    return result


def git_cmd(*args: str) -> str:
    """Run a git command and return stripped output."""
    try:
        return subprocess.check_output(
            ["git"] + list(args), stderr=subprocess.DEVNULL, text=True
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def read_handoff_summary(handoff_path: str) -> str:
    """Extract the main content from HANDOFF.md as a summary."""
    try:
        with open(handoff_path) as f:
            lines = f.readlines()
    except (FileNotFoundError, PermissionError):
        return ""

    # Skip the title line and metadata, get the first real paragraph
    content_lines = []
    past_header = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("# "):
            past_header = True
            continue
        if past_header and stripped.startswith("**") and ":" in stripped:
            continue  # Skip metadata lines like **Status:** ...
        if past_header and stripped:
            content_lines.append(stripped)
            if len(content_lines) >= 3:
                break

    return " ".join(content_lines) if content_lines else ""


def resolve_output_path(
    cli_output: str | None,
    plan_dir: str | None,
    state: dict,
) -> Path:
    """Resolve where to write the completion summary."""
    # 1. CLI argument
    if cli_output:
        return Path(cli_output)

    # 2. Settings config (project then global)
    for settings_path in [".fno/config.toml", os.path.expanduser("~/.fno/config.toml")]:
        if os.path.exists(settings_path):
            try:
                with open(settings_path) as f:
                    content = f.read()
                match = re.search(r"^\s*completions_path\s*=\s*(.+)", content, re.MULTILINE)
                if match:
                    completions_dir = Path(match.group(1).strip().strip('"').strip("'"))
                    completions_dir.mkdir(parents=True, exist_ok=True)
                    slug = _make_slug(state)
                    return completions_dir / f"{slug}.md"
            except Exception:
                continue

    # 3. Fallback: sidecar for file plans, folder itself for folder plans.
    #    Prefer the raw plan_path from state (authoritative) over the plan_dir
    #    argument so a stale or wrong --plan-dir CLI value cannot collapse
    #    sibling quick plans onto each other.
    plan_path_raw = state.get("plan_path", "")
    if plan_path_raw:
        p = Path(plan_path_raw).expanduser()
        if p.is_file():
            sidecar = p.with_name(p.name + ".artifacts")
            try:
                sidecar.mkdir(parents=True, exist_ok=True)
            except OSError:
                pass
            return sidecar / "COMPLETION.md"
        if p.is_dir():
            return p / "COMPLETION.md"

    # 3b. Compatibility fallback: honor the caller-supplied plan_dir when the
    #     state has no plan_path (out-of-band invocations).
    if plan_dir and os.path.isdir(plan_dir):
        return Path(plan_dir) / "COMPLETION.md"

    # 4. Last resort: .fno/.completed/ (not the .fno/ root). Plan-less,
    #    config-less invocations used to drop completion-*.md straight into the
    #    .fno/ root where they accumulate; route them into the existing
    #    .completed/ subfolder instead, creating it if absent.
    slug = _make_slug(state)
    completed_dir = Path(".fno") / ".completed"
    try:
        completed_dir.mkdir(parents=True, exist_ok=True)
    except OSError:
        # Never let a directory-creation hiccup lose the summary; fall back to
        # the .fno/ root rather than crashing the writer.
        return Path(".fno") / f"completion-{slug}.md"
    return completed_dir / f"completion-{slug}.md"


def _make_slug(state: dict) -> str:
    """Generate a filename slug from state."""
    title = state.get("input", "untitled")
    date = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    # Slugify: lowercase, replace spaces/special chars with hyphens
    slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")[:50]
    return f"{date}-{slug}"


def generate_summary(state: dict, plan_dir: str | None, state_path: str = ".fno/target-state.md") -> str:
    """Generate the completion summary markdown content."""
    title = state.get("input", "Untitled")
    pr_number = state.get("pr_number")
    cost = state.get("total_cost", "N/A")
    tokens = state.get("total_tokens", "N/A")
    model = state.get("model", "N/A")
    branch = git_cmd("branch", "--show-current") or "N/A"
    plan_path = state.get("plan_path", plan_dir or "N/A")

    # PR URL
    pr_url = ""
    if pr_number:
        remote = git_cmd("remote", "get-url", "origin")
        gh_match = re.search(r"github\.com[:/](.+?)(?:\.git)?$", remote) if remote else None
        if gh_match:
            pr_url = f"https://github.com/{gh_match.group(1)}/pull/{pr_number}"

    # What was built - from HANDOFF.md or git log
    handoff_summary = read_handoff_summary(".fno/HANDOFF.md")
    if not handoff_summary:
        # Fall back to git log summary
        created_at = state.get("created_at", "")
        if created_at:
            handoff_summary = git_cmd("log", "--oneline", f"--since={created_at}")
            if handoff_summary:
                lines = handoff_summary.splitlines()
                handoff_summary = f"{len(lines)} commits since session start."

    # Git log
    created_at = state.get("created_at", "")
    commits = ""
    if created_at:
        commits = git_cmd("log", "--oneline", f"--since={created_at}")
    if not commits:
        commits = git_cmd("log", "--oneline", "-10")

    # Pipeline progress + learnings from state file (read once)
    pipeline = ""
    learnings = ""
    try:
        with open(state_path) as f:
            state_content = f.readlines()
        current_section = None
        for line in state_content:
            if "## Pipeline Progress" in line:
                current_section = "pipeline"
                continue
            elif "## Learnings" in line:
                current_section = "learnings"
                continue
            elif line.startswith("## "):
                current_section = None
                continue
            if current_section == "pipeline" and line.strip():
                pipeline += line
            elif current_section == "learnings" and line.strip():
                learnings += line
    except (FileNotFoundError, PermissionError):
        pipeline = "N/A"

    # Build the summary
    completed_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    pr_link = f"[#{pr_number}]({pr_url})" if pr_url else f"#{pr_number}" if pr_number else "N/A"

    def esc(s: str) -> str:
        """Escape double quotes for YAML string values."""
        return str(s).replace('"', '\\"') if s else ""

    lines = [
        "---",
        f'title: "{esc(title)}"',
        f"date: {completed_at}",
        f"pr: {pr_number or 'null'}",
        f'pr_url: "{pr_url}"' if pr_url else "pr_url: null",
        f"cost: {cost}",
        f"tokens: {tokens}",
        f'branch: "{esc(branch)}"',
        f'plan: "{esc(plan_path)}"' if plan_path else "plan: null",
        "---",
        "",
        f"# {title}",
        "",
        f"**PR:** {pr_link} | **Cost:** ${cost} | **Tokens:** {tokens} | **Model:** {model}",
        "",
    ]

    if handoff_summary:
        lines.extend([
            "## What Was Built",
            "",
            handoff_summary,
            "",
        ])

    if commits:
        lines.extend([
            "## Commits",
            "",
            "```",
            commits,
            "```",
            "",
        ])

    if pipeline.strip():
        lines.extend([
            "## Pipeline",
            "",
            pipeline.rstrip(),
            "",
        ])

    if learnings.strip():
        lines.extend([
            "## Decisions",
            "",
            learnings.rstrip(),
            "",
        ])
    else:
        lines.extend([
            "## Decisions",
            "",
            "None recorded.",
            "",
        ])

    return "\n".join(lines)


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Generate completion summary")
    parser.add_argument("state_path", help="Path to target-state.md")
    parser.add_argument("--plan-dir", help="Plan directory path")
    parser.add_argument("--output", help="Explicit output path")

    args = parser.parse_args()

    if not os.path.exists(args.state_path):
        print(f"Error: {args.state_path} not found", file=sys.stderr)
        sys.exit(1)

    state = parse_frontmatter(args.state_path)
    plan_dir = args.plan_dir or state.get("plan_path")
    if plan_dir and os.path.isfile(plan_dir):
        plan_dir = os.path.dirname(plan_dir)

    output_path = resolve_output_path(args.output, plan_dir, state)
    summary = generate_summary(state, plan_dir, state_path=args.state_path)

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(summary)

    # Print path to stdout for the stop hook
    print(str(output_path))


if __name__ == "__main__":
    main()
