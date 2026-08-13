"""The unbounded-background-process guard: what it refuses and what it must not.

Both halves matter. A guard that denies too little is decorative; a guard that
denies a legitimate command is a guard someone disables.
"""
from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / "hooks" / "bg-process-guard.py"


def _load():
    spec = importlib.util.spec_from_file_location("bg_process_guard", HOOK)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


guard = _load()


# The refusal cases: each can run forever and none carries a bound.
DENIED = [
    "yes > /dev/null &",
    "yes > /dev/null",
    "yes",
    "nohup yes > /dev/null 2>&1 &",
    "cd /tmp && yes > /dev/null &",
    "bash -c 'yes > /dev/null' &",
    "while true; do echo hi; done",
    "while :; do sleep 1; done",
    "until false; do echo hi; done",
    "sleep infinity",
    "stress -c 8",
    "dd if=/dev/zero of=/dev/null",
    "cat /dev/urandom > /dev/null",
    "setsid yes > /dev/null &",
    # A leading assignment is a prefix, not the command.
    "FOO=1 yes > /dev/null &",
    "env FOO=1 yes > /dev/null &",
    # Multi-line. shlex in posix mode eats newlines as whitespace, which
    # flattened a whole command into one segment and hid every line after the
    # first. Multi-line Bash calls are routine here, so this WAS most of the
    # guard's real surface.
    "cd foo\nyes > /dev/null",
    "echo hi\nyes > /dev/null &",
    "npm test\nyes > /dev/null &\necho done",
    # Inside a loop or conditional body. `;` splits correctly, but the segment
    # then starts with the keyword and the walk stopped there.
    "for i in 1 2; do yes >/dev/null; done",
    "if [ -f x ]; then yes > /dev/null; fi",
    "while read l; do yes > /dev/null; done < f",
    # Downstream of a pipe. `_head_of` reads only the FIRST command of a
    # segment, and every `|` used to mark the whole segment bounded, so the
    # last stage - the one with no reader to deliver SIGPIPE - was never looked
    # at at all.
    ": | yes > /dev/null",
    "echo hi | yes > /dev/null",
    # An unquoted `#` is ordinary shell text far more often than it opens a
    # comment. shlex swallowed from it to end of line and lost the generator.
    "echo ${#PATH}; yes > /dev/null",
    # A heredoc is stripped only as far as its terminator. Everything after the
    # terminator is still command text, so a generator cannot hide behind one.
    "cat > f.txt <<'EOF'\nhello\nEOF\nyes > /dev/null",
    # An unterminated `<<` is not a heredoc opener; nothing may be dropped, or
    # the strip itself becomes the hiding place.
    "echo 'a << b'\nyes > /dev/null",
    # A loop escape counts only in COMMAND position. `break` as an argument
    # leaves the loop exactly as unbounded as it was, and a text match for the
    # word read both of these as escapes.
    "while true; do echo break; done",
    "while true; do rg break src; done",
    "for ((;;)); do echo break; done",
]

# The allow cases. Two kinds: bounded versions of the same generators, and
# ordinary commands that merely mention a generator word.
ALLOWED = [
    "timeout 300 bash -c 'exec -a fno-load-x1 yes > /dev/null'",
    "timeout 300 yes > /dev/null",
    "gtimeout 60 stress -c 8",
    "yes | head -c 1M > /dev/null",
    "dd if=/dev/zero of=/dev/null count=1000",
    "stress -c 8 -t 60",
    "echo yes",
    "git commit -m 'yes it works'",
    "grep -rn --include=*.py -e nh_operator_members_facil .",
    "npm test && echo yes",
    "rg yes .",
    "printf 'yes\\n' | wc -l",
    # The repo's own bounded detached work. Every stage inside is wrapped by
    # with_timeout, so this must keep working; a tightening that blocks it
    # breaks the eval sweep on every session start.
    "nohup bash -c 'source \"$1/eval-sweep-throttle.sh\" || exit 0; "
    "_eval_sweep_run_stages \"$2\" \"$3\" \"$4\"' _ a b c d >/dev/null 2>&1 &",
    "gh pr checks --watch",
    "cargo test",
    # A newline inside a quoted string is part of that string, not a command
    # separator. This is why the newline is handled in the lexer and not by a
    # string replace before it.
    'git commit -m "fix\nyes it works"',
    'echo "line1\nline2 yes"',
    "npm test\necho done",
    "printf x\ntimeout 5 yes",
    "for i in 1 2; do timeout 5 yes; done",
    # `ulimit -t` is a shell setting, so it can only ever sit in an EARLIER
    # segment than the process it bounds. Read per segment it bounded nothing,
    # while the refusal text advertised it as one of the remedies.
    "ulimit -t 60; yes > /dev/null",
    "ls  # yes > /dev/null",
    # A loop the body can leave. `while true` with a `break` is the standard
    # poll and it ends; refusing it blocked an ordinary shape at the Bash
    # boundary.
    "while true; do sleep 5; if gh pr checks; then break; fi; done",
    "until false; do sleep 1; ls && break; done",
    # A heredoc BODY is data written to a file, not commands. Writing a script
    # that merely contains a poll loop is not running one.
    "cat > poll.sh <<'EOF'\nwhile true; do :; done\nEOF",
    "cat > f.txt <<EOF\nyes > /dev/null\nEOF",
    # A capability probe runs nothing.
    "command -v yes",
]


@pytest.mark.parametrize("command", DENIED)
def test_denies_unbounded_generators(command: str) -> None:
    assert guard.decide(command) is not None, f"should have refused: {command}"


@pytest.mark.parametrize("command", ALLOWED)
def test_allows_bounded_and_ordinary_commands(command: str) -> None:
    assert guard.decide(command) is None, f"should have allowed: {command}"


# ─── combinatorial sweep ────────────────────────────────────────────────────
#
# Two review rounds found two separate holes in the same command-position walk,
# and a systematic sweep then found a third that neither reviewer reached. A
# hand-written case list only covers shapes someone thought of, so this crosses
# every wrapper with every prefix with every generator instead. It is the check
# that makes hole number four fail CI rather than wait for a reviewer.

_WRAPS = [
    "{cmd}", "{cmd} &", "( {cmd} )", "( {cmd} & )", "{{ {cmd}; }}",
    'bash -c "{cmd}"', "bash -c '{cmd}'", 'sh -c "{cmd}"',
    "cd /tmp && {cmd}", "cd /tmp; {cmd}", "true || {cmd}",
    "echo hi\n{cmd}", "echo hi\n{cmd} &",
    "for i in 1 2; do {cmd}; done", "if true; then {cmd}; fi",
    "while read l; do {cmd}; done", "until false; do {cmd}; done",
    # Command substitution. shlex merges adjacent punctuation into ONE token,
    # so this produced `');'`, which matched no separator by equality and left
    # the whole line as a single segment. 84 of 1512 cases walked through here.
    "x=$(echo 1); {cmd}",
    # Holes four, five and six reached a reviewer instead of this sweep, which
    # is the claim two lines above falsified. All three were segmentation, not
    # generator logic, so they enter as AXES rather than three point cases: the
    # generator downstream of a pipe (nothing reads it, so no SIGPIPE arrives)
    # and an unquoted `#` that shlex read as a comment and swallowed.
    ": | {cmd}",
    "echo hi | {cmd}",
    "echo ${{#PATH}}; {cmd}",
]

#: Wrappers that legitimately BOUND whatever they wrap, so every generator
#: under one must be allowed. `ulimit -t` is a shell setting and can only sit
#: in an earlier segment than the process it bounds, which is why reading it
#: per segment made the refusal advertise a remedy it then refused.
_BOUNDING_WRAPS = [
    "ulimit -t 60; {cmd}",
    "timeout 300 bash -c '{cmd}'",
    "{cmd} | head -c 1M",
]

_PREFIXES = [
    "", "nohup ", "setsid ", "env ", "env FOO=1 ", "FOO=1 ", "sudo ",
    "sudo -u me ", "nice -n 5 ", "time ", "command ", "exec ",
    "exec -a fno-load-x ", "stdbuf -o0 ",
    # Boolean flags on a wrapper that ALSO has value flags. Read from one shared
    # value-flag set, `-n` and `-p` swallowed the generator behind them and the
    # command resolved to whatever came next.
    "sudo -n ", "command -p ",
]

_GENERATORS = [
    "yes > /dev/null", "yes", "sleep infinity", "dd if=/dev/zero of=/dev/null",
    "stress -c 8", "cat /dev/urandom > /dev/null",
]

_LEGAL = [
    "timeout 300 yes > /dev/null", "gtimeout 60 stress -c 8",
    "yes | head -c 1M", "dd if=/dev/zero of=/dev/null count=10",
    "stress -c 8 -t 60", 'timeout 5 bash -c "yes > /dev/null"',
    "echo yes", "npm test", "cargo build", 'git commit -m "yes it works"',
    "rg yes .", "grep -rn yes .", "gh pr checks --watch", "ls -la",
    "fno agents orphans --reap",
]


@pytest.mark.parametrize("wrap", _WRAPS)
def test_no_wrapper_hides_an_unbounded_generator(wrap: str) -> None:
    missed = [
        wrap.format(cmd=prefix + gen)
        for prefix in _PREFIXES
        for gen in _GENERATORS
        if guard.decide(wrap.format(cmd=prefix + gen)) is None
    ]
    assert not missed, f"{len(missed)} unbounded commands allowed, e.g. {missed[0]!r}"


#: The legal-direction wrappers. `until false; do ... ; done` is deliberately
#: absent: that header never ends whatever its body, so refusing it is the
#: right answer and not a false positive.
_LEGAL_WRAPS = [w for w in _WRAPS if not w.startswith("until false")]


@pytest.mark.parametrize("wrap", _LEGAL_WRAPS)
def test_no_wrapper_turns_a_legal_command_into_a_refusal(wrap: str) -> None:
    refused = [
        wrap.format(cmd=cmd)
        for cmd in _LEGAL
        if guard.decide(wrap.format(cmd=cmd)) is not None
    ]
    assert not refused, f"{len(refused)} legal commands refused, e.g. {refused[0]!r}"


@pytest.mark.parametrize("wrap", _BOUNDING_WRAPS)
def test_a_bounding_wrapper_rescues_every_generator(wrap: str) -> None:
    """The other direction of the same sweep.

    A bound the refusal text advertises must actually work, under every prefix
    and every generator. Two of the three bounds here were broken while their
    names sat in the refusal message telling people to use them.
    """
    refused = [
        wrap.format(cmd=prefix + gen)
        for prefix in _PREFIXES
        for gen in _GENERATORS
        if guard.decide(wrap.format(cmd=prefix + gen)) is not None
    ]
    assert not refused, f"{len(refused)} bounded commands refused, e.g. {refused[0]!r}"


def test_an_unbounded_loop_header_is_refused_whatever_its_body() -> None:
    """The body cannot rescue the header. `until false; do timeout 300 yes;
    done` runs forever even though every command inside it ends."""
    assert guard.decide("until false; do timeout 300 yes; done") is not None
    assert guard.decide("until false; do echo hi; done") is not None
    assert guard.decide("while true; do sleep 1; done") is not None


def test_an_escape_before_the_loop_does_not_clear_it() -> None:
    """An escape leaves the loop it is INSIDE, never one it precedes.

    Read over the whole command, an ordinary precondition cleared the loop that
    followed it, and `cd /tmp || exit 1; while true; do sleep 60; done &` is a
    very ordinary way to write the keepalive this guard exists to refuse.
    """
    assert guard.decide("cd /tmp || exit 1; while true; do sleep 60; done &")
    assert guard.decide("[ -f x ] || exit 1\nwhile true; do sleep 1; done")
    # The escape that IS inside the loop still clears it.
    assert guard.decide("while true; do sleep 5; gh pr view && break; done") is None


def test_a_bundled_shell_flag_still_opens_the_payload() -> None:
    """`bash -lc '...'` is the same call as `bash -c '...'`; an exact `-c`
    match walked past every bundled spelling."""
    assert guard.decide("bash -lc 'yes > /dev/null'")
    assert guard.decide("sh -ec 'while true; do sleep 1; done'")
    assert guard.decide("timeout 5 bash -lc 'yes > /dev/null'") is None


def test_a_redirect_target_is_not_a_bound() -> None:
    """A filename is not a command. `yes 2> /tmp/gtimeout` claimed a bound it
    never runs, and naming an output file is a bypass anyone reaches by
    accident."""
    assert guard.decide("yes 2> /tmp/gtimeout")
    assert guard.decide("yes > /tmp/timeout")
    assert guard.decide("timeout 300 yes > /dev/null") is None


def test_refusal_carries_the_replacement_verbatim() -> None:
    """The refusal string IS the naming layer. Prevention layer 4 ships here
    and nowhere else, so a reason without the bounded+named form is a silent
    regression of a whole layer of the design."""
    reason = guard.decide("yes > /dev/null &")
    assert reason is not None
    assert "timeout" in reason
    assert "exec -a fno-" in reason
    assert "SIGPIPE" in reason


def test_unbalanced_quotes_fail_open() -> None:
    assert guard.decide("echo \"oops\nyes > /dev/null &") is None


def _run_hook(payload: object) -> tuple[int, str]:
    proc = subprocess.run(
        [sys.executable, str(HOOK)],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
    )
    return proc.returncode, proc.stdout


def test_end_to_end_deny_envelope() -> None:
    code, out = _run_hook(
        {"tool_name": "Bash", "tool_input": {"command": "yes > /dev/null &"}}
    )
    assert code == 0
    decision = json.loads(out)["hookSpecificOutput"]
    assert decision["permissionDecision"] == "deny"
    assert "exec -a fno-" in decision["permissionDecisionReason"]


def test_end_to_end_allow_is_silent() -> None:
    code, out = _run_hook(
        {"tool_name": "Bash", "tool_input": {"command": "echo yes"}}
    )
    assert code == 0
    assert out.strip() == ""


def test_non_bash_tool_is_ignored() -> None:
    code, out = _run_hook(
        {"tool_name": "Write", "tool_input": {"command": "yes > /dev/null &"}}
    )
    assert code == 0
    assert out.strip() == ""


def test_malformed_stdin_allows() -> None:
    proc = subprocess.run(
        [sys.executable, str(HOOK)], input="not json", capture_output=True, text=True
    )
    assert proc.returncode == 0
    assert proc.stdout.strip() == ""
