#!/usr/bin/env python3
"""Audit a session transcript: did the agent invoke a capability on its own?

The trap this retires: a capability probe delivered over the mail bus can
only ever return yes. `fno agents mail send` types user-shaped text into
the worker, indistinguishable from the operator typing, so a probe that
arrives by mail exercises the USER-TRIGGERED path and cannot fail. Reading
its success as proof of autonomy is a receipt lying.

The valid test is a run whose transcript holds no user-shaped prompt before
the invocation. This scan is that test. It walks a claude or codex jsonl
transcript in order, finds the first tool call whose input matches
--pattern, and classifies:

  autonomous      the matched call appears and no user-shaped prompt
                  precedes it. The only verdict that evidences the agent
                  invoking the capability unaided.
  user-triggered  a user-shaped prompt precedes the matched call. The
                  transcript cannot tell the operator from a mail
                  injection, and neither is autonomy evidence.
  not-invoked     the transcript parsed but no call matched. The honest no.

Markers are positive, never absences: `user-triggered` rests on the prompt
line itself, `autonomous` on the matched call. A file with no parsable
lines fails loud as `unreadable` instead of reading as any verdict.
Harness-injected user turns (system reminders, AGENTS.md preambles, tool
results riding the user role) are excluded by marker, so they never count
as a prompt.

Usage:
  python3 scripts/diagnostics/autonomy-probe-audit.py --pattern 'code-review' FILE.jsonl [FILE.jsonl ...]
  python3 scripts/diagnostics/autonomy-probe-audit.py --self-check

Exit 0 only when every transcript reads autonomous; otherwise 1 with the
verdicts printed.
"""

import argparse
import json
import os
import re
import sys
import tempfile

# Harness-injected text that rides the user turn channel. A message starting
# with any of these is not a prompt and can never carry a probe.
NOISE_PREFIXES = (
    "# AGENTS.md",
    "<recommended_plugins>",
    "## Memory",
    "You are `/root`",
    "<multi_agent_mode",
    "<EXTREMELY_IMPORTANT",
    "Warning: truncated",
    "<system-reminder>",
    "<task-notification>",
    "Caveat:",
    "<user_instructions>",
    "<environment_context>",
    "<local-command-stdout>",
    "<persisted-output>",
)

CODEX_TOOL_TYPES = ("custom_tool_call", "function_call", "local_shell_call")


def _is_noise(text: str) -> bool:
    return text.lstrip().startswith(NOISE_PREFIXES)


def _serial(obj) -> str:
    if isinstance(obj, (dict, list)):
        return json.dumps(obj, sort_keys=True)
    return str(obj or "")


def _claude_prompt_texts(content) -> list:
    """User-turn texts from a claude message content field.

    Tool results ride the user role too; their blocks are skipped, so only
    genuine prompt text is returned.
    """
    if isinstance(content, str):
        return [content]
    if not isinstance(content, list):
        return []
    return [
        b.get("text", "")
        for b in content
        if isinstance(b, dict) and b.get("type") == "text"
    ]


def scan(path: str, pattern: str):
    """Return (verdict, detail) for one transcript."""
    rx = re.compile(pattern)
    prompt_seen = False
    parsed = 0
    with open(path, errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                d = json.loads(line)
            except Exception:
                continue
            if not isinstance(d, dict):
                continue
            t = d.get("type")
            p = d.get("payload")
            parsed += 1

            # claude turns
            if t in ("user", "assistant") and isinstance(d.get("message"), dict):
                if t == "user":
                    if d.get("isMeta"):
                        continue
                    for text in _claude_prompt_texts(d["message"].get("content")):
                        if text.strip() and not _is_noise(text):
                            prompt_seen = True
                else:
                    for b in d["message"].get("content") or []:
                        if isinstance(b, dict) and b.get("type") == "tool_use":
                            hay = f"{b.get('name', '')} {_serial(b.get('input'))}"
                            if rx.search(hay):
                                verdict = "user-triggered" if prompt_seen else "autonomous"
                                return verdict, f"matched tool_use {b.get('name', '')}"
                continue

            if not isinstance(p, dict):
                continue

            # codex rollout turns
            if t == "event_msg" and p.get("type") == "user_message":
                text = p.get("message", "")
                if text.strip() and not _is_noise(text):
                    prompt_seen = True
            elif t == "response_item" and p.get("type") == "message" and p.get("role") == "user":
                text = " ".join(
                    c.get("text", "") for c in p.get("content", []) if isinstance(c, dict)
                )
                if text.strip() and not _is_noise(text):
                    prompt_seen = True
            elif t == "response_item" and p.get("type") in CODEX_TOOL_TYPES:
                hay = _serial(p.get("input") or p.get("arguments"))
                if rx.search(hay):
                    verdict = "user-triggered" if prompt_seen else "autonomous"
                    return verdict, f"matched {p.get('type')}"

    if parsed == 0:
        return "unreadable", "no parsable transcript lines"
    return "not-invoked", f"no call matched {pattern!r}"


def audit(paths, pattern) -> int:
    counts: dict[str, int] = {}
    failures = 0
    for path in paths:
        verdict, detail = scan(path, pattern)
        counts[verdict] = counts.get(verdict, 0) + 1
        ok = verdict == "autonomous"
        if not ok:
            failures += 1
        print(f"{'PASS' if ok else 'FAIL':<5} {verdict:<15} {path}  ({detail})")
        if verdict == "user-triggered":
            print(
                "      a user-shaped prompt precedes the call; operator or mail,"
                " the transcript cannot tell, and neither proves autonomy"
            )
    print()
    for verdict, n in sorted(counts.items()):
        print(f"  {verdict:<15} {n}")
    return 1 if failures else 0


def self_check() -> int:
    """Synthetic transcripts pinning the classifier; exits nonzero on mismatch."""

    def write(events):
        return "".join(json.dumps(e) + "\n" for e in events)

    def cl_user(text, meta=False):
        e = {"type": "user", "message": {"content": [{"type": "text", "text": text}]}}
        if meta:
            e["isMeta"] = True
        return e

    def cl_tool_result():
        return {"type": "user", "message": {"content": [{"type": "tool_result", "content": "out"}]}}

    def cl_tool(name, inp):
        return {"type": "assistant", "message": {"content": [{"type": "tool_use", "name": name, "input": inp}]}}

    def cx_usermsg(text):
        return {"type": "event_msg", "payload": {"type": "user_message", "message": text}}

    def cx_role_user(text):
        return {"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}]}}

    def cx_tool(ttype, inp):
        return {"type": "response_item", "payload": {"type": ttype, "input": inp}}

    PATTERN = "code-review"
    cases = [
        # claude: the invocation with no prompt before it is the one valid yes
        ("claude-autonomous", [cl_tool("Bash", {"command": "/code-review high"})], "autonomous"),
        ("claude-user-first", [cl_user("please run /code-review"),
                               cl_tool("Bash", {"command": "/code-review high"})], "user-triggered"),
        # the trap: a mail injection is plain user text; it must read user-triggered
        ("claude-mail-shaped", [cl_user("can you run /code-review yourself? try it"),
                                cl_tool("Bash", {"command": "/code-review high"})], "user-triggered"),
        # harness rides are not prompts: tool results and system reminders never count
        ("claude-tool-result-noise", [cl_tool_result(),
                                      cl_tool("Bash", {"command": "/code-review high"})], "autonomous"),
        ("claude-system-reminder-noise", [cl_user("<system-reminder>context low</system-reminder>"),
                                          cl_tool("Bash", {"command": "/code-review high"})], "autonomous"),
        ("claude-meta-noise", [cl_user("Caveat: the messages below were generated", meta=True),
                               cl_tool("Bash", {"command": "/code-review high"})], "autonomous"),
        ("claude-task-notification-noise", [cl_user("<task-notification>bg task done</task-notification>"),
                                            cl_tool("Bash", {"command": "/code-review high"})], "autonomous"),
        # codex shapes
        ("codex-user-first", [cx_usermsg("run /code-review"),
                              cx_tool("custom_tool_call", "exec /code-review")], "user-triggered"),
        ("codex-preamble-noise", [cx_role_user("<user_instructions># AGENTS.md</user_instructions>"),
                                  cx_tool("function_call", {"arguments": "run /code-review"})], "autonomous"),
        ("codex-role-user-first", [cx_role_user("run /code-review please"),
                                   cx_tool("local_shell_call", "code-review")], "user-triggered"),
        # the honest no, and the loud unreadable
        ("not-invoked", [cx_usermsg("hello")], "not-invoked"),
    ]

    failures = 0
    with tempfile.TemporaryDirectory() as td:
        empty = os.path.join(td, "empty.jsonl")
        open(empty, "w").close()
        for label, events, want in cases:
            path = os.path.join(td, label + ".jsonl")
            with open(path, "w") as fh:
                fh.write(write(events))
            got, _ = scan(path, PATTERN)
            status = "ok" if got == want else "FAIL"
            if got != want:
                failures += 1
            print(f"self-check {label:<28} {status}  want={want} got={got}")
        got, detail = scan(empty, PATTERN)
        if got == "unreadable":
            print("self-check unreadable-empty-file             ok")
        else:
            failures += 1
            print(f"self-check unreadable-empty-file             FAIL  want=unreadable got={got} ({detail})")
    return failures


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("files", nargs="*", help="claude or codex .jsonl transcripts")
    ap.add_argument("--pattern", help="regex matched against tool name plus input")
    ap.add_argument("--self-check", action="store_true", help="run synthetic classifier checks")
    args = ap.parse_args()

    if args.self_check:
        sys.exit(1 if self_check() else 0)

    if not args.files or not args.pattern:
        ap.error("give FILE(s) plus --pattern, or --self-check")
    sys.exit(audit(args.files, args.pattern))


if __name__ == "__main__":
    main()
