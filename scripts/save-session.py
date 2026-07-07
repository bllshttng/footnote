#!/usr/bin/env python3
"""
PreCompact/SessionEnd hook: saves a readable session transcript with rich metadata.

Reads hook input from stdin (session_id, transcript_path, cwd).
Reads statusline sidecar (~/.claude/.session-context.json) for model, cost, context_window, etc.
Saves to ${FNO_HOME:-~/.fno}/sessions/{session_id}-{timestamp}-{trigger}.md
"""

import json
import os
import subprocess
import sys
from datetime import datetime
from pathlib import Path

# Footnote state lives under ${FNO_HOME:-~/.fno}, never .claude/ (placement
# rule, ab-f063 Wave 2). The sidecar we READ is Claude Code's own statusline
# file and legitimately stays under ~/.claude/.
FNO_HOME = Path(os.environ.get("FNO_HOME") or (Path.home() / ".fno"))
SESSIONS_DIR = FNO_HOME / "sessions"
SIDECAR_PATH = Path.home() / ".claude" / ".session-context.json"
SKIP_TYPES = {"progress", "file-history-snapshot"}
NOISE_PREFIXES = (
    "Base directory for this skill:",
    "<task-notification>",
    "<command-message>",
    "<system-reminder>",
)


def is_noise(text: str) -> bool:
    stripped = text.strip()
    return any(stripped.startswith(p) for p in NOISE_PREFIXES)


def extract_text(message: dict, role: str) -> str | None:
    content = message.get("content", "")
    if isinstance(content, str):
        if not content.strip() or (role == "user" and is_noise(content)):
            return None
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if block.get("type") == "tool_result":
                return None
            if block.get("type") == "text":
                text = block.get("text", "")
                if role == "user" and is_noise(text):
                    return None
                if text.strip():
                    parts.append(text)
            elif block.get("type") == "thinking":
                thinking = block.get("thinking", "")
                if thinking.strip():
                    parts.append(f"<thinking>\n{thinking}\n</thinking>")
        return "\n".join(parts) if parts else None
    return None


def read_sidecar() -> dict:
    """Read the statusline sidecar for rich session metadata."""
    try:
        if SIDECAR_PATH.exists():
            age = datetime.now().timestamp() - SIDECAR_PATH.stat().st_mtime
            if age < 300:  # Only trust if updated in last 5 minutes
                return json.loads(SIDECAR_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        print(f"save-session: sidecar read failed: {e}", file=sys.stderr)
    return {}


def get_git_branch(cwd: str) -> str:
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True, text=True, timeout=5, cwd=cwd,
        )
        return result.stdout.strip() if result.returncode == 0 else ""
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        return ""


def get_model_from_transcript(transcript_path: str) -> str:
    """Extract model name from the transcript JSONL (first assistant message)."""
    try:
        with open(transcript_path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                message = entry.get("message", {})
                if message.get("role") == "assistant":
                    return message.get("model", "")
    except OSError:
        pass
    return ""


def parse_rounds(transcript_path: str) -> list[tuple[str | None, str]]:
    """Parse transcript into rounds of (user_text, assistant_text)."""
    rounds = []
    pending_user = None
    pending_assistant_parts = []

    def flush():
        nonlocal pending_user, pending_assistant_parts
        if pending_assistant_parts:
            rounds.append((pending_user, "\n".join(pending_assistant_parts)))
        pending_user = None
        pending_assistant_parts = []

    with open(transcript_path, "r") as f:
        for raw_line in f:
            raw_line = raw_line.strip()
            if not raw_line:
                continue
            try:
                entry = json.loads(raw_line)
            except json.JSONDecodeError:
                continue

            if entry.get("type", "") in SKIP_TYPES:
                continue
            if entry.get("isCompactSummary"):
                continue

            if entry.get("type") == "system":
                subtype = entry.get("subtype", "")
                if subtype in ("turn_duration", "stop_hook_summary"):
                    flush()
                continue

            message = entry.get("message", {})
            role = message.get("role", "")

            if role == "user":
                text = extract_text(message, "user")
                if text:
                    if pending_assistant_parts:
                        flush()
                    pending_user = text

            elif role == "assistant":
                text = extract_text(message, "assistant")
                if text:
                    pending_assistant_parts.append(text)

    flush()
    return rounds


def format_transcript(rounds: list[tuple[str | None, str]], frontmatter: str) -> str:
    lines = [frontmatter]

    for user_text, assistant_text in rounds:
        lines.append("---")
        lines.append("")
        if user_text:
            lines.append("## User")
            lines.append(user_text)
            lines.append("")
        lines.append("## Assistant")
        lines.append(assistant_text)
        lines.append("")

    return "\n".join(lines)


def build_frontmatter(
    session_id: str, cwd: str, trigger: str,
    transcript_path: str, sidecar: dict,
) -> str:
    now = datetime.now()

    # Model: prefer sidecar, fallback to JSONL extraction
    model_obj = sidecar.get("model", {})
    model_id = model_obj.get("id", "") if model_obj else ""
    if not model_id:
        model_id = get_model_from_transcript(transcript_path)

    # Workspace
    workspace = sidecar.get("workspace", {})
    project_dir = workspace.get("project_dir", "")

    # Git branch: prefer sidecar worktree, fallback to git command
    worktree = sidecar.get("worktree", {})
    git_branch = worktree.get("branch", "") or (get_git_branch(cwd) if cwd else "")

    # Version
    version = sidecar.get("version", "")

    # Cost
    cost = sidecar.get("cost", {})

    # Context window
    ctx = sidecar.get("context_window", {})

    parts = ["---"]
    parts.append(f"sessionId: {session_id}")
    parts.append(f"timestamp: {now.isoformat()}")
    parts.append(f"trigger: {trigger}")

    if cwd:
        parts.append(f"cwd: {cwd}")
    if project_dir and project_dir != cwd:
        parts.append(f"projectDir: {project_dir}")
    if model_id:
        parts.append(f"model: {model_id}")
    if git_branch:
        parts.append(f"gitBranch: {git_branch}")
    if version:
        parts.append(f"version: \"{version}\"")

    # Worktree info
    if worktree.get("name"):
        parts.append(f"worktree:")
        parts.append(f"  name: {worktree['name']}")
        if worktree.get("path"):
            parts.append(f"  path: {worktree['path']}")
        if worktree.get("original_branch"):
            parts.append(f"  originalBranch: {worktree['original_branch']}")

    # Cost
    if cost.get("total_cost_usd"):
        parts.append(f"cost:")
        parts.append(f"  usd: {cost['total_cost_usd']}")
        if cost.get("total_duration_ms"):
            duration_s = round(cost["total_duration_ms"] / 1000)
            parts.append(f"  durationSeconds: {duration_s}")
        if cost.get("total_lines_added"):
            parts.append(f"  linesAdded: {cost['total_lines_added']}")
        if cost.get("total_lines_removed"):
            parts.append(f"  linesRemoved: {cost['total_lines_removed']}")

    # Context window
    if ctx.get("used_percentage"):
        parts.append(f"contextWindow:")
        parts.append(f"  usedPercent: {ctx['used_percentage']}")
        parts.append(f"  totalInputTokens: {ctx.get('total_input_tokens', 0)}")
        parts.append(f"  totalOutputTokens: {ctx.get('total_output_tokens', 0)}")
        if ctx.get("current_usage"):
            cu = ctx["current_usage"]
            parts.append(f"  currentUsage:")
            parts.append(f"    inputTokens: {cu.get('input_tokens', 0)}")
            parts.append(f"    outputTokens: {cu.get('output_tokens', 0)}")
            parts.append(f"    cacheCreation: {cu.get('cache_creation_input_tokens', 0)}")
            parts.append(f"    cacheRead: {cu.get('cache_read_input_tokens', 0)}")

    parts.append("---")
    parts.append("")

    return "\n".join(parts)


def main():
    try:
        input_data = json.load(sys.stdin)
    except (json.JSONDecodeError, EOFError) as e:
        print(f"save-session: invalid stdin JSON: {e}", file=sys.stderr)
        sys.exit(0)

    transcript_path = input_data.get("transcript_path", "")
    session_id = input_data.get("session_id", "")
    cwd = input_data.get("cwd", "")
    trigger = input_data.get("trigger") or input_data.get("reason") or "unknown"

    if not transcript_path or not os.path.isfile(transcript_path):
        print(f"save-session: no transcript at {transcript_path!r}", file=sys.stderr)
        sys.exit(0)

    # Parse rounds first — skip empty sessions
    try:
        rounds = parse_rounds(transcript_path)
    except OSError as e:
        print(f"save-session: failed to read transcript: {e}", file=sys.stderr)
        sys.exit(0)

    if not rounds:
        sys.exit(0)

    SESSIONS_DIR.mkdir(parents=True, exist_ok=True)
    os.chmod(SESSIONS_DIR, 0o700)

    # Sanitize inputs used in filenames to prevent path traversal
    session_prefix = "".join(c for c in session_id if c.isalnum()) or "session"
    safe_trigger = "".join(c for c in trigger if c.isalnum() or c in ("-", "_"))
    safe_trigger = safe_trigger or "unknown"

    # Dedup: skip if a transcript for this session was saved in the last 60 seconds
    now = datetime.now()
    for existing in SESSIONS_DIR.glob(f"{session_prefix}-*.md"):
        age = now.timestamp() - existing.stat().st_mtime
        if age < 60:
            sys.exit(0)

    timestamp = now.strftime("%Y%m%d-%H%M%S")
    filename = f"{session_prefix}-{timestamp}-{safe_trigger}.md"
    output_path = SESSIONS_DIR / filename

    try:
        sidecar = read_sidecar()
        frontmatter = build_frontmatter(session_id, cwd, trigger, transcript_path, sidecar)
        content = format_transcript(rounds, frontmatter)
        output_path.write_text(content, encoding="utf-8")
        os.chmod(output_path, 0o600)
        print(f"Session saved: {output_path}", file=sys.stderr)
    except Exception as e:
        print(f"Failed to save session: {e}", file=sys.stderr)

    sys.exit(0)


if __name__ == "__main__":
    main()
