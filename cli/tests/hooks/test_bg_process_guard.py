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
    # An escape BEFORE the loop cannot leave it. Read over the whole command,
    # a `|| exit 1` precondition cleared the keepalive that follows it.
    "cd /tmp || exit 1; while true; do sleep 60; done &",
    "[ -f x ] || exit 1\nwhile true; do sleep 1; done",
    # A bound belongs to the stage it wraps, never to its pipeline siblings.
    # Read per segment, this was allowed and leaves specimen 1 behind after
    # one second.
    "timeout 1 true | yes > /dev/null",
    # A bound counts only in COMMAND position. Scanning every token let any
    # ARGUMENT spelling `timeout` license the generator, and the worst case is
    # the most ordinary thing an agent writes: comment text survives as tokens
    # here on purpose, so a trailing note mentioning the fix disabled the guard.
    "yes > /dev/null  # no timeout needed",
    "yes timeout > /dev/null",
    # `-t` bounds `stress`. `yes` just prints it forever.
    "yes -t 5 > /dev/null",
    # `<` IS the read. Dropping `<` sources alongside `>` targets inverted the
    # very distinction the carveout exists to draw, and every endless-device
    # generator delivered by redirect walked through.
    "cat < /dev/zero",
    "base64 < /dev/urandom",
    "wc -l < /dev/zero",
    "md5 </dev/zero",
    "cat 0< /dev/zero",
    # A `case` arm's `)` opens the body, so command position restarts there.
    # Left mid-segment, the body sat behind the `case` head and was never read.
    "case $x in a) yes > /dev/null;; esac",
    # An escape AFTER `done` is outside the loop and can never run. These two
    # are specimen 1 exactly, and they are the canonical way to write a
    # detached keepalive: the mirror image of the precondition hole below.
    "while true; do sleep 60; done; exit 0",
    "while true; do sleep 60; done & exit",
    "until false; do sleep 60; done; break",
    # An empty middle expression is what makes a C-style `for` endless.
    "for ((i=0;;)); do sleep 1; done",
    "for (( ; ; )); do sleep 1; done",
    # An escape belongs to the loop it sits inside. One shared boolean let an
    # escape in an EARLIER loop license every later one, and specimen 1 is
    # sitting in the second half of each of these.
    "for f in *; do [ -e $f ] && break; done; while true; do sleep 60; done &",
    "while true; do gh pr view && break; done; while true; do sleep 60; done &",
    "while true; do break; done; for ((i=0;;)); do sleep 1; done &",
    # `VALUE_FLAGS` is hand-written, so an unlisted wrapper flag handed command
    # position to its own value. Measured, not inferred: under `timeout 3` each
    # of these exits 124, so the `yes` really does run forever.
    "/usr/bin/time -o /tmp/t.log yes > /dev/null",
    "caffeinate -t 3600 yes > /dev/null",
    "env -P /usr/bin yes > /dev/null",
    # The non-greedy reading has to survive too: `-E` is a real boolean, and
    # reading it as value-taking eats the generator behind it.
    "sudo -E yes > /dev/null",
    # A comment carrying the word `case` opened a region that never closed, and
    # a disabled pipe split then hid the generator downstream of the pipe.
    "# handle the drained case\n: | yes > /dev/null",
    "echo case; yes > /dev/null",
    # `(` and `{` are transparent to the head walk but are not separators, so a
    # backward test that accepted only a separator counted a different set of
    # `for`s and the arithmetic flags drifted out of step. `( cmd & )` is the
    # canonical detach idiom, so it is the likeliest spelling of specimen 1.
    "( for ((;;)); do :; done ) &",
    "{ for ((;;)); do :; done; } &",
    # The drift also mis-attributed: the flag landed on the innocent loop, and
    # its `break` then cleared the endless one.
    "( for f in a b; do echo $f; done ); for ((;;)); do :; done &",
    "( for f in a b; do break; done ); for ((;;)); do :; done &",
    # The command that caused the incident, verbatim from the transcript.
    "for i in $(seq 1 24); do yes > /dev/null & done",
    "for j in 1 2 3 4 5 6; do ( while :; do :; done ) & done",
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
    # `|&` is bash's pipe-including-stderr. Split as a control operator it
    # stranded the reader in a later segment and refused a bounded pipeline.
    "yes |& head -c 1M",
    # The truth table inverts between the keywords. Both of these exit at once,
    # and one shared condition set refused a loop deliberately disabled.
    "while false; do echo hi; done",
    "until true; do echo hi; done",
    # Reading an ordinary file by redirect is not reading an endless device.
    # The `<` fix must not deny every redirect, only the endless sources.
    "wc -l < report.txt",
    "cat < report.txt",
    "case $x in a) echo hi;; esac",
    # A C-style `for` with a real condition counts and stops. A shape test that
    # reads only the parentheses calls this one endless too.
    "for ((i=0;i<10;i++)); do sleep 1; done",
    # None of these has a shell loop in it. Matched on the raw command text,
    # `for ((;;))` denied every one, and the third blocked writing a commit
    # message about this guard in the repo that ships it.
    'echo "for ((;;))"',
    "rg 'for ((;;))' hooks/",
    "git commit -m 'guard: refuse for ((;;)) loops'",
    # A quoted argument is one token, and its TEXT still landed in the joined
    # arithmetic header. Moving the read off raw text onto tokens did not fix
    # this on its own. Both are real commands from the transcript corpus.
    'for f in "for ((;;))"; do echo "$f"; done',
    "for pat in 'while true' 'for ((;;))'; do rg \"$pat\" hooks/; done",
    # `|` between `case` patterns is alternation, not a pipe. Split as a pipe,
    # the last alternative became a stage with no reader, and the refusal was
    # order-dependent: `yes|y)` allowed while `y|yes)` denied.
    "case $a in y|yes) echo go;; esac",
    "case $t in cpu|stress) echo load;; esac",
    "case $x in a) foo;; y|yes) bar;; esac",
    # A nested or preceding `done` must not truncate the escape scan. Both of
    # these are ordinary poll loops taken verbatim from the corpus.
    "while true; do for n in 1 2; do echo $n; done; gh pr view && break; sleep 30; done",
    "until gh pr view; do sleep 5; done; while true; do gh pr checks && break; sleep 30; done",
    # The spellings these tools' own manuals use. The refusal text advertises
    # `-t <seconds>` as a remedy, so refusing a real spelling of it is the guard
    # naming a fix it will not accept.
    "stress --timeout 60",
    "stress-ng --cpu 4 --timeout 60s",
    "stress-ng --cpu 1 -t 30s",
    "stress --timeout=60 --cpu 1",
    # A comment must change no verdict. Counting the WORD `case` over every
    # token refused the exact remedy the refusal text advertises, and comment
    # text survives as tokens here on purpose. 43 of 366,594 real commands open
    # such a region, most of them ordinary English.
    "# handle the drained case\nyes | head -c 1M",
    "echo case; yes | head -c 1M",
    "grep case notes.txt; yes | head -c 1M",
    # An `esac` in a comment must not close a real region early.
    "case $a in # y|yes esac\n y|yes) echo go;; esac",
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


#: Each remedy the refusal advertises, and a command that takes the advice.
#: ADD A ROW when you add a remedy to the refusal text.
_ADVERTISED_REMEDIES = [
    ("timeout", "timeout 300 yes > /dev/null"),
    ("gtimeout", "gtimeout 300 yes > /dev/null"),
    ("count=", "dd if=/dev/zero of=/dev/null count=10"),
    ("ulimit -t", "ulimit -t 60; yes > /dev/null"),
    ("-t <seconds>", "stress -c 8 -t 60"),
    ("yes | head -c 1M", "yes | head -c 1M"),
]


@pytest.mark.parametrize("phrase,remedy", _ADVERTISED_REMEDIES)
def test_every_advertised_remedy_actually_works(phrase: str, remedy: str) -> None:
    """A refusal that names a fix it then refuses is worse than saying nothing.

    Both have happened here. `ulimit -t` was advertised while its check was
    dead code, and `head -c` was advertised as a standalone bound when only a
    downstream reader bounds anything, so following the advice literally earned
    a second refusal. This pins the text to the behaviour in both directions.
    """
    reason = guard.decide("yes > /dev/null &")
    assert reason is not None and phrase in reason, f"no longer advertised: {phrase}"
    assert guard.decide(remedy) is None, f"advertised but refused: {remedy}"


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
