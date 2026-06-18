#!/usr/bin/env python3
"""Generate a discovery brief from a completed task's artifacts.

Compresses HANDOFF.md + git history into a ~500-token summary that
informs subsequent task execution in a do-roadmap campaign.

Usage:
    python3 discovery-brief.py --task-id 3 --handoff .fno/HANDOFF.md
    python3 discovery-brief.py --task-id 3 --handoff .fno/HANDOFF.md --roadmap-state .fno/roadmap-state.md

Output: JSON with task_id, brief, files_changed, decisions
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


def git_cmd(*args: str) -> str:
    """Run a git command and return stripped output."""
    try:
        return subprocess.check_output(
            ["git"] + list(args), stderr=subprocess.DEVNULL, text=True
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def read_handoff(path: str) -> dict:
    """Parse HANDOFF.md into sections."""
    sections = {}
    current = None
    lines = []

    try:
        content = Path(path).read_text()
    except FileNotFoundError:
        return {}

    for line in content.splitlines():
        if line.startswith("## ") or line.startswith("### "):
            if current:
                sections[current] = "\n".join(lines).strip()
            current = line.lstrip("#").strip().lower()
            lines = []
        else:
            lines.append(line)

    if current:
        sections[current] = "\n".join(lines).strip()

    return sections


def extract_decisions(handoff: dict) -> list[str]:
    """Extract key decisions from handoff sections."""
    decisions = []

    for key in ["key decisions", "decisions", "key decisions made"]:
        text = handoff.get(key, "")
        if text:
            for line in text.splitlines():
                line = line.strip().lstrip("- *")
                if line and len(line) > 10:
                    decisions.append(line)

    return decisions[:5]  # Top 5 decisions


def extract_files(handoff: dict, max_files: int = 10) -> list[str]:
    """Extract key files from handoff or git diff."""
    files = []

    # Try handoff first
    for key in ["files created/modified", "files changed", "what was built"]:
        text = handoff.get(key, "")
        if text:
            # Extract file paths from markdown
            for match in re.finditer(r"`([^`]+\.\w+)`", text):
                files.append(match.group(1))
            for match in re.finditer(r"[-*]\s+(\S+\.\w+)", text):
                files.append(match.group(1))

    if not files:
        # Fall back to git diff
        diff_stat = git_cmd("diff", "--name-only", "HEAD~5..HEAD")
        if diff_stat:
            files = [f.strip() for f in diff_stat.splitlines() if f.strip()]

    # Deduplicate and limit
    seen = set()
    unique = []
    for f in files:
        if f not in seen:
            seen.add(f)
            unique.append(f)
    return unique[:max_files]


def extract_goal(handoff: dict) -> str:
    """Extract the original goal / what was built."""
    for key in ["original goal", "what was built", "summary"]:
        text = handoff.get(key, "")
        if text:
            # Take first 2 sentences
            sentences = re.split(r"(?<=[.!?])\s+", text)
            return " ".join(sentences[:2]).strip()

    return ""


def extract_verify(handoff: dict) -> str:
    """Extract verification commands."""
    for key in ["how to test", "verification", "how to verify"]:
        text = handoff.get(key, "")
        if text:
            # Extract code blocks or commands
            commands = re.findall(r"`([^`]+)`", text)
            if commands:
                return "; ".join(commands[:3])
            # Take first line
            first_line = text.splitlines()[0].strip().lstrip("- ")
            if first_line:
                return first_line

    return ""


def generate_brief(task_id: str, handoff_path: str) -> dict:
    """Generate a discovery brief from task artifacts."""
    handoff = read_handoff(handoff_path)

    goal = extract_goal(handoff)
    files = extract_files(handoff)
    decisions = extract_decisions(handoff)
    verify = extract_verify(handoff)

    # If handoff is empty, fall back to git log
    if not goal:
        log = git_cmd("log", "--oneline", "-5")
        if log:
            goal = f"Completed work: {log.splitlines()[0]}"

    # Compose brief text (~500 tokens target = ~375 words)
    parts = []

    if goal:
        parts.append(goal)

    if files:
        parts.append(f"Key files: {', '.join(files[:5])}")

    if decisions:
        parts.append("Decisions: " + "; ".join(decisions[:3]))

    if verify:
        parts.append(f"Verify: {verify}")

    brief_text = "\n".join(parts)

    # Rough token estimate: words / 0.75
    word_count = len(brief_text.split())
    token_estimate = int(word_count / 0.75)

    if token_estimate > 500:
        # Trim: reduce files and decisions
        parts = []
        if goal:
            parts.append(goal)
        if files:
            parts.append(f"Key files: {', '.join(files[:3])}")
        if decisions:
            parts.append("Decisions: " + "; ".join(decisions[:2]))
        if verify:
            parts.append(f"Verify: {verify}")
        brief_text = "\n".join(parts)

    return {
        "task_id": task_id,
        "brief": brief_text,
        "files_changed": files,
        "decisions": decisions,
        "token_estimate": int(len(brief_text.split()) / 0.75),
    }


def append_to_roadmap_state(state_path: str, task_id: str | int, title: str, brief_text: str) -> None:
    """Append or replace discovery brief in roadmap-state.md."""
    path = Path(state_path)
    if not path.exists():
        return

    try:
        content = path.read_text()
    except OSError as e:
        print(f"Warning: could not read {state_path}: {e}", file=sys.stderr)
        return

    # Find or create Discovery Briefs section
    if "## Discovery Briefs" not in content:
        content += "\n## Discovery Briefs\n"

    # Replace existing brief for this task if present (dedup on retry)
    header = f"### Task {task_id}:"
    if header in content:
        content = re.sub(
            rf"### Task {task_id}:.*?(?=### Task \d+:|## |\Z)",
            "",
            content,
            flags=re.DOTALL,
        )

    brief_section = f"\n### Task {task_id}: {title}\n{brief_text}\n"
    content += brief_section

    try:
        path.write_text(content)
    except OSError as e:
        print(f"Warning: could not write {state_path}: {e}", file=sys.stderr)


def save_sidecar_brief(task_id: str, brief_text: str) -> Path | None:
    """Save brief as a sidecar file at ~/.fno/briefs/{id}.md."""
    briefs_dir = Path.home() / ".fno" / "briefs"
    briefs_dir.mkdir(parents=True, exist_ok=True)
    brief_path = briefs_dir / f"{task_id}.md"
    try:
        brief_path.write_text(brief_text)
        return brief_path
    except OSError as e:
        print(f"Warning: could not write brief to {brief_path}: {e}", file=sys.stderr)
        return None


def main():
    parser = argparse.ArgumentParser(description="Generate discovery brief for a completed task")
    parser.add_argument("--task-id", required=True, help="Task ID (ab-XXXXXXXX or integer)")
    parser.add_argument("--handoff", default=".fno/HANDOFF.md", help="Path to HANDOFF.md")
    parser.add_argument("--title", help="Task title (for roadmap-state heading)")
    parser.add_argument("--roadmap-state", help="Path to roadmap-state.md (append brief)")

    args = parser.parse_args()

    result = generate_brief(args.task_id, args.handoff)

    # Save as sidecar file for graph entries (ab- IDs)
    is_graph = isinstance(args.task_id, str) and args.task_id.startswith("ab-")
    if is_graph:
        saved = save_sidecar_brief(args.task_id, result["brief"])
        if saved:
            print(f"Brief saved to {saved}", file=sys.stderr)

    # Update graph node or legacy task
    script_dir = Path(__file__).parent
    roadmap_tasks = script_dir / "roadmap-tasks.py"
    if roadmap_tasks.exists():
        update_cmd = [
            sys.executable, str(roadmap_tasks),
            "update", str(args.task_id),
            "--has-brief", "true",
        ]
        proc = subprocess.run(update_cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            print(f"Warning: failed to update task: {proc.stderr.strip()}", file=sys.stderr)

    # Append to roadmap-state.md if provided (legacy flow, still useful for campaign context)
    if args.roadmap_state and args.title:
        append_to_roadmap_state(args.roadmap_state, args.task_id, args.title, result["brief"])

    # Output JSON
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
