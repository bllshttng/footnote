#!/usr/bin/env python3
"""PreToolUse guard: refuse a command that can never end on its own and carries
no time bound.

On 2026-08-13 one session left 73 `yes > /dev/null` processes at PPID 1, 797%
CPU combined, 71 core-hours over 7.5 hours. `yes` writing to /dev/null never
receives SIGPIPE - there is no reader to go away and the sink always accepts -
so the normal death path for an abandoned pipeline does not exist. The load
starved a preflight holder to 0.31 seconds of CPU in 52 minutes, and that lock
steals on `kill -0` death alone, so it never freed.

This guard covers ONE of the three orphan classes that night: the process that
cannot end on its own. Specimen 2 (a `grep -rn` at 64% CPU) and specimen 3
(background tasks that outlived their session) are not refusable at creation
time - a grep is a legitimate command, and nothing in the command text of
specimen 3 marks it. Those are `fno agents orphans`' job, after the fact. See
docs/architecture/background-process-hygiene.md.

Parse-only, stdlib alone. No third-party import, psutil included: a hook runs
under whatever bare interpreter the harness hands it, and an ImportError here
takes the guard down on every Bash call. It never inspects a live process.

Fails OPEN on anything unexpected. A guard that breaks a session on its own bug
is worse than the orphans it prevents.
"""

import json
import re
import shlex
import sys

# Tokens that end one command and start the next. `|` is deliberately absent:
# `yes | head -c 1M` is ONE pipeline and the `head` bounds it, so splitting
# there would hide the bound from the generator that it bounds.
SEGMENT_SEPARATORS = {";", "&&", "||", "&", "\n", "|&"}

# Wrappers that are transparent to command position: `nohup yes` still runs
# `yes` first. Without these the generator sits at index 1 and reads as an
# argument, which is exactly how specimen 1 was written.
TRANSPARENT = {
    "nohup", "setsid", "exec", "time", "env", "command", "builtin",
    "nice", "ionice", "taskpolicy", "stdbuf", "caffeinate", "sudo",
    # Shell keywords that open a body. `;` splits `do yes` into its own
    # segment, and without these the walk stops at the keyword and reads the
    # generator as an argument: `for i in 1 2; do yes; done` was allowed.
    "do", "then", "else", "elif", "{", "!",
}

# `exec -a NAME` renames the process; the NAME is not the command. Skipping the
# flag and its value keeps `exec -a fno-load-x1 yes` resolving to `yes`.
TRANSPARENT_FLAGS_WITH_VALUE = {"-a", "-n", "-c"}

SHELLS = {"sh", "bash", "zsh", "dash", "ksh"}

REPLACEMENT = "timeout 300 bash -c 'exec -a fno-load-<node-or-session> yes > /dev/null'"


def _tokens(text):
    """Shell tokens with operators kept as their own tokens. Raises ValueError
    on unbalanced quotes, which the caller turns into an allow.

    A newline is made punctuation and removed from whitespace, so it emits as
    its own token and ends a command the way `;` does. In stock posix mode
    shlex swallows newlines as whitespace, which flattened a whole multi-line
    command into ONE segment: `cd foo\\nyes > /dev/null` read as a command
    called `cd` and the generator on line 2 was never seen. Multi-line Bash
    calls are routine from this harness, so that hole covered most real uses of
    the guard. Doing it inside the lexer rather than by a string replace keeps a
    newline INSIDE a quoted string part of that string.
    """
    lex = shlex.shlex(text, posix=True, punctuation_chars="();<>|&\n")
    lex.whitespace = " \t\r"
    lex.whitespace_split = True
    return list(lex)


def _segments(tokens):
    """Split a token list on command separators. Redirections stay attached to
    their segment; only control operators split."""
    out, current = [], []
    for tok in tokens:
        if tok in SEGMENT_SEPARATORS:
            out.append(current)
            current = []
        else:
            current.append(tok)
    out.append(current)
    return [seg for seg in out if seg]


def _head_of(segment):
    """The command-position token of `segment`, plus its remaining argv.

    Walks past transparent wrappers and their flag values so the real command
    surfaces. Returns (None, []) for a segment with no command.
    """
    i = 0
    while i < len(segment):
        tok = segment[i]
        base = tok.rsplit("/", 1)[-1]
        if base in TRANSPARENT:
            i += 1
            continue
        if tok in TRANSPARENT_FLAGS_WITH_VALUE and i + 1 < len(segment):
            # Only meaningful directly after a transparent wrapper (`exec -a`).
            i += 2
            continue
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", tok):
            # A leading assignment is a prefix, not the command: `FOO=1 yes` and
            # `env FOO=1 yes` both still run `yes`.
            i += 1
            continue
        return base, segment[i + 1:]
    return None, []


def _has_bound(segment):
    """True when this segment carries something that makes the process end.

    A bound is read per segment rather than per whole command: a `timeout` on
    an unrelated earlier segment must not license an unbounded generator later
    in the same line.
    """
    for i, tok in enumerate(segment):
        base = tok.rsplit("/", 1)[-1]
        if base in {"timeout", "gtimeout"}:
            return True
        if base == "head":  # `yes | head -c 1M` ends on its own
            return True
        if tok.startswith("count="):  # dd
            return True
        if base == "ulimit" and "-t" in segment[i + 1:]:
            return True
        if tok == "-t" and i + 1 < len(segment) and segment[i + 1].isdigit():
            return True  # stress -t 60
        if re.fullmatch(r"-t\d+", tok):
            return True
    return False


def _generator_reason(head, argv):
    """Why this segment can never end, or None when it can."""
    if head == "yes":
        return ("`yes` never ends. Writing to /dev/null it never even receives "
                "SIGPIPE, because there is no reader to go away and the sink "
                "always accepts.")
    if head in {"while", "until"} and argv and argv[0] in {"true", ":", "false"}:
        return "`%s %s` is an unbounded loop header." % (head, argv[0])
    if head == "for" and any(";;" in tok for tok in argv):
        return "`for ((;;))` is an unbounded loop header."
    if head == "sleep" and argv and argv[0] in {"infinity", "inf"}:
        return "`sleep infinity` never returns."
    if head in {"stress", "stress-ng"}:
        return "`%s` runs until killed unless given `-t`." % head
    if head == "dd" and any(a in {"if=/dev/zero", "if=/dev/urandom"} for a in argv):
        return "`dd` from an endless device never reaches EOF without `count=`."
    if head in {"cat", "sha256sum", "shasum", "md5", "md5sum", "base64", "wc"}:
        if any(a in {"/dev/zero", "/dev/urandom", "/dev/random"} for a in argv):
            return "reading an endless device never reaches EOF."
    return None


def _payload_of(head, argv):
    """The script text a shell was handed with -c, or None."""
    if head in SHELLS and "-c" in argv:
        idx = argv.index("-c")
        if idx + 1 < len(argv):
            return argv[idx + 1]
    return None


def _find_unbounded(text, inherited_bound=False, depth=0):
    """First (reason, segment_text) that can never end and is unbounded.

    Recurses one level into `bash -c '...'` payloads, carrying the enclosing
    segment's bound down: `timeout 300 bash -c 'yes'` IS bounded, and the
    bound lives outside the payload it bounds.
    """
    if depth > 2:
        return None
    for segment in _segments(_tokens(text)):
        bounded = inherited_bound or _has_bound(segment)
        head, argv = _head_of(segment)
        if head is None:
            continue
        reason = _generator_reason(head, argv)
        if reason and not bounded:
            return reason, " ".join(segment)
        payload = _payload_of(head, argv)
        if payload:
            found = _find_unbounded(payload, inherited_bound=bounded, depth=depth + 1)
            if found:
                return found
    return None


def _refusal(reason, segment):
    return (
        "Refusing an unbounded process that cannot end on its own.\n\n"
        "  %s\n\n"
        "%s\n"
        "On 2026-08-13 that shape cost 71 core-hours and wedged a preflight "
        "lock for 50+ minutes.\n\n"
        "Bounded and named instead:\n"
        "  %s\n\n"
        "`timeout` is the death path. `exec -a fno-...` is the name, so a "
        "survivor answers \"whose is this?\" in `top` instead of by lsof "
        "archaeology, and `fno agents orphans --reap` may kill it unattended. "
        "Use bash explicitly: zsh has no `exec -a`.\n"
        "Any of `timeout`, `gtimeout`, `head -c`, `count=`, `ulimit -t`, or "
        "`-t <seconds>` satisfies this guard."
        % (segment, reason, REPLACEMENT)
    )


def _emit(decision, reason):
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }))


def decide(command):
    """The whole verdict for one command string: a refusal, or None to allow.

    Separated from main() so the test suite exercises the same function the
    hook does, rather than a second implementation of the same predicate.
    """
    try:
        found = _find_unbounded(command)
    except ValueError:
        return None  # unbalanced quotes: cannot tell command position, allow
    except Exception:  # noqa: BLE001 -- fail open, always
        return None
    if not found:
        return None
    return _refusal(found[0], found[1])


def main():
    try:
        input_data = json.load(sys.stdin)
    except Exception:  # noqa: BLE001
        sys.exit(0)

    if input_data.get("tool_name", "") != "Bash":
        sys.exit(0)

    command = (input_data.get("tool_input", {}) or {}).get("command", "")
    if not isinstance(command, str) or not command.strip():
        sys.exit(0)

    # Denied whether or not run_in_background is set. A foreground unbounded
    # `yes` is orphaned just as surely when the session exits - that is exactly
    # what the harness reported for specimen 3 - and it is never the right
    # command either way. One branch instead of two.
    refusal = decide(command.strip())
    if refusal:
        _emit("deny", refusal)
    sys.exit(0)


if __name__ == "__main__":
    main()
