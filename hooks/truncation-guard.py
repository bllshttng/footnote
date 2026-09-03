#!/usr/bin/env python3
"""PreToolUse guard: refuse a count-or-existence read that is piped into
``head`` or ``tail``.

The trap this closes, from the AGENTS.md pitfalls corpus ("Assert a positive
marker, never an absence"): a truncated listing answers a DIFFERENT question
than the one asked, and the answer looks the same. On 2026-09-03 a session ran
``pgrep -fl fno-agents | head -4`` and reported "no daemon running". The daemon
was running. ``head`` had cut the matching line off a listing whose first four
rows were something else, and nothing in the output said so.

The pitfall was in that session's context when it happened. Prose does not fire
at the moment of a tool call; a refusal does. That is the whole reason this
guard exists as a hook rather than a rule.

Only truncation of a PRODUCER whose output is read as a count or an existence
claim is refused. A plain log tail is not that: ``tail -f app.log`` and
``head -c 200 file`` are byte or stream bounds on text nobody is counting, and
they pass. Parse-only, stdlib alone, fails OPEN on anything unexpected.
"""

import json
import os
import re
import subprocess
import sys
import time

# Producers whose output gets read as "how many" or "does it exist". Each is
# matched at the start of a pipeline, so `echo pgrep | head` is not a producer.
PRODUCERS = re.compile(
    r"""^(?:
          pgrep
        | ps\s+(?:-\S+\s+)*(?:aux|ax|ef)\b
        | ls
        | find
        | rg\s+(?:\S+\s+)*(?:-l|--files-with-matches|--files)\b
        | grep\s+(?:\S+\s+)*-\w*l\w*\b
        | gh\s+run\s+list
        | gh\s+pr\s+list
        | git\s+worktree\s+list
        | fno\s+backlog\s+(?:find|list)
        | fno\s+agents\s+(?:list|ls)
    )\b""",
    re.VERBOSE,
)

REASON = (
    "[fno truncation guard] `{cmd}` truncates a count-or-existence read. A "
    "truncated listing answers a different question than the one you asked, and "
    "the answer looks identical: on 2026-09-03 `pgrep -fl fno-agents | head -4` "
    "produced the false claim 'no daemon running' while the daemon was live. "
    "For a COUNT use `| wc -l`. For an EXISTENCE claim read the full listing, or "
    "narrow the producer's own filter until it is short. Never truncate a zero "
    "you intend to trust (AGENTS.md, 'Assert a positive marker, never an "
    "absence')."
)


def _truncates(stage):
    """True when this pipeline stage is a head/tail that CUTS ROWS.

    ``-c`` is a byte bound and ``-f`` is a follow, so neither drops a matching
    row from a finished listing. Both pass.
    """
    words = stage.split()
    if not words or words[0] not in ("head", "tail"):
        return False
    for word in words[1:]:
        if word.startswith("-") and not word.startswith("--"):
            if "c" in word[1:] or "f" in word[1:]:
                return False
        elif word in ("--bytes", "--follow") or word.startswith(
            ("--bytes=", "--follow=")
        ):
            return False
    return True


def decide(command):
    """The whole verdict for one command string: a refusal, or None to allow.

    Separated from main() so the test suite exercises the same function the
    hook does, rather than a second implementation of the same predicate.
    """
    try:
        for segment in re.split(r"&&|\|\||;|\n", command):
            stages = [s.strip() for s in re.split(r"(?<!\|)\|(?!\|)", segment)]
            if len(stages) < 2:
                continue
            if not PRODUCERS.match(stages[0]):
                continue
            if _truncates(stages[-1]):
                return REASON.format(cmd=segment.strip())
    except Exception:  # noqa: BLE001 -- fail open, always
        return None
    return None


def _guard_mark(decision):
    """One guard_decision row per run: the positive liveness signal that this
    guard ran and what it decided. Row shape matches hooks/lib/guard-mark.sh so
    bash and python guards write indistinguishable rows. Best-effort by
    contract: any failure is swallowed and can never change a decision."""
    try:
        pin = os.environ.get("FNO_EVENTS_PATH")
        if pin:
            path = pin
        elif os.path.isdir(".git") or os.path.isdir(".fno"):
            path = os.path.join(".fno", "events.jsonl")
        else:
            root = subprocess.run(
                ["git", "rev-parse", "--show-toplevel"],
                capture_output=True,
                text=True,
                timeout=5,
            ).stdout.strip()
            path = os.path.join(root or os.getcwd(), ".fno", "events.jsonl")
        row = (
            '{"ts":"%s","type":"guard_decision","data":{"guard":"truncation-guard",'
            '"decision":"%s","tool":"Bash"},"source":"hook"}'
            % (time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), decision)
        )
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(row + "\n")
    except Exception:
        pass


def main():
    try:
        input_data = json.load(sys.stdin)
    except Exception:  # noqa: BLE001
        _guard_mark("allow")
        sys.exit(0)

    if input_data.get("tool_name", "") != "Bash":
        _guard_mark("allow")
        sys.exit(0)

    command = (input_data.get("tool_input", {}) or {}).get("command", "")
    if not isinstance(command, str) or not command.strip():
        _guard_mark("allow")
        sys.exit(0)

    refusal = decide(command.strip())
    if refusal:
        _guard_mark("block")
        print(
            json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": refusal,
                    }
                }
            )
        )
        sys.exit(0)
    _guard_mark("allow")
    sys.exit(0)


if __name__ == "__main__":
    main()
