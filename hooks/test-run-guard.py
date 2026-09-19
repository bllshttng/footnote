#!/usr/bin/env python3
"""PreToolUse guard: refuse a raw pytest or `cargo test` run in a footnote checkout.

`fno doctor test` is the only test door with admission: it holds the
machine-wide `test:suite` claim, waits behind a live cargo build through
the rustc wrapper's build admission, pins PYTHONPATH to the worktree so the
right fno is imported, and owns a process group that is always reaped. A
raw `pytest` or `cargo test` takes none of that. Measured 2026-09-18: two
workers running raw suites crushed the machine, which is what this guard
was filed from. Prose does not fire at the moment of a tool call; a
refusal does.

A refusal is read in SHELL COMMAND POSITION, per pipeline stage, the same
way recursive-grep-guard.py reads a recursive grep: transparent wrappers
(`env pytest`, `timeout 30 cargo test`), env-assignment prefixes, full
paths, one level of `bash -c` payloads, and heredoc-stripped bodies. The
other uv doors are covered because they are the same raw run:
`uv run pytest`, `uvx pytest`, `uv tool run pytest`, and `python -m pytest`.

Parse-only, stdlib alone, fails OPEN on anything unexpected: a guard that
breaks a session on its own bug is worse than the runs it prevents. Scoped
to footnote checkouts (`cli/src/fno` plus `hooks/hooks.json` at the repo
root); every other repository allows. Scripts that call pytest internally
are a known fail-open, the accepted shape for this class of guard.
"""

import json
import os
import re
import shlex
import subprocess
import sys
import time

# Characters shlex may accumulate into a single operator token, and the ones
# that actually END a command. `|` is absent: it starts a new pipeline stage,
# not a new command, and is split separately.
PUNCT_CHARS = set("();<>|&\n")
CONTROL_CHARS = set(";&\n")


def _is_separator(tok):
    """True when this token ends one command and starts the next."""
    if not tok or any(ch not in PUNCT_CHARS for ch in tok):
        return False
    # `2>&1` and `|&` are redirect/pipe machinery, never control operators;
    # splitting there strands the tail of the command in a phantom segment.
    if ">" in tok or "<" in tok or tok == "|&":
        return False
    # A bare `)` closes a subshell or opens a `case` arm body; either way the
    # next token is back in command position.
    if tok == ")":
        return True
    return bool(CONTROL_CHARS & set(tok)) or "||" in tok


# Wrappers that are transparent to command position: `sudo pytest ...` still
# runs the pytest. Shell keywords that open a body are here too, so
# `do pytest ...; done` inside a `for` resolves to the pytest.
TRANSPARENT = {
    "nohup", "setsid", "exec", "time", "env", "command", "builtin",
    "nice", "ionice", "taskpolicy", "stdbuf", "caffeinate", "sudo",
    # `timeout` bounds a process but does not admit it, so it is transparent
    # HERE: `timeout 30 pytest` still leaves the machine with a raw suite.
    "timeout", "gtimeout",
    "do", "then", "else", "elif", "{", "(", "!",
}

#: Wrappers whose own positional operand sits BEFORE the wrapped command:
#: `timeout 300 pytest ...` spends `300` before the pytest ever appears.
_POSITIONAL_LEAD = {"timeout", "gtimeout"}

# Wrapper flags that swallow the next token, so the value is not mistaken for
# the command (`sudo -u me pytest ...`). Read PER WRAPPER.
VALUE_FLAGS = {
    "nice": {"-n"},
    "ionice": {"-c", "-n", "-p"},
    "sudo": {"-u", "-g", "-C", "-U", "-p", "-D", "-R", "-T", "-h"},
    "env": {"-u", "-S"},
    "exec": {"-a"},
    "stdbuf": {"-i", "-o", "-e"},
    "taskpolicy": {"-c"},
}

SHELLS = {"sh", "bash", "zsh", "dash", "ksh"}

_PY_NAME = re.compile(r"python(?:3(?:\.\d+)?)?")

#: uv flags whose NEXT token is a value, never the dispatched command.
#: Anything unlisted is read as boolean, which fails open: `--someday x pytest`
#: would read `x` as the command and allow.
_UV_VALUE_FLAGS = {
    "--with", "--without", "--from", "--python", "--env-file", "--config-file",
    "--project", "--directory", "--default-index", "--index",
    "--exclude-newer", "--python-preference", "-p",
}

#: Cargo global flags whose NEXT token is a value, never the subcommand.
_CARGO_VALUE_FLAGS = {
    "-C", "--config", "-Z", "--manifest-path", "--target", "-j", "--jobs",
}


def _tokens(text):
    """Shell tokens with operators kept as their own tokens. Raises ValueError
    on unbalanced quotes, which the caller turns into an allow.

    Newline is made punctuation so it ends a command the way `;` does, and
    commenters stay empty: shlex swallows from an unquoted `#` to end of line,
    and `#` is ordinary shell text far more often than it starts a comment.
    """
    lex = shlex.shlex(text, posix=True, punctuation_chars="();<>|&\n")
    lex.whitespace = " \t\r"
    lex.whitespace_split = True
    lex.commenters = ""
    return list(lex)


#: `<<EOF`, `<<-'EOF'`, `<< "EOF"`. The delimiter word ends the body.
_HEREDOC = re.compile(r"<<-?\s*([\"']?)([A-Za-z_][A-Za-z0-9_]*)\1")


def _strip_heredocs(text):
    """Drop heredoc BODIES. They are data written to a file, not commands, and
    a script that merely CONTAINS `pytest` must not read as one that runs it.
    Only strips when the terminator is found, so a `<<` that was really a
    quoted string cannot swallow the rest of the command."""
    lines = text.split("\n")
    out, i = [], 0
    while i < len(lines):
        line = lines[i]
        out.append(line)
        i += 1
        match = _HEREDOC.search(line)
        if not match:
            continue
        delim = match.group(2)
        end = i
        while end < len(lines) and lines[end].strip() != delim:
            end += 1
        if end < len(lines):
            i = end + 1
    return "\n".join(out)


def _basename(tok):
    return tok.rsplit("/", 1)[-1]


def _is_python(name):
    return bool(_PY_NAME.fullmatch(name))


def _has_dash_m_module(argv, target):
    """True when argv carries `-m <target>` in either spelling: the split
    `python -m pytest -q` and the attached `python -mpytest`, which CPython
    accepts identically."""
    for i, tok in enumerate(argv):
        if tok == "-m":
            if i + 1 < len(argv) and _basename(argv[i + 1]) == target:
                return True
        elif tok.startswith("-m") and tok[2:] and _basename(tok[2:]) == target:
            return True
    return False


def _first_positional(tokens):
    """Index of the first token that is neither a flag nor a flag value."""
    skip = False
    for i, tok in enumerate(tokens):
        if skip:
            skip = False
            continue
        if tok.startswith("-") and len(tok) > 1:
            skip = tok in _UV_VALUE_FLAGS
            continue
        return i
    return None


def _uv_target(argv):
    """(basename, rest) of the command `uv run ...` or `uv tool run ...`
    dispatches. rest is the argv AFTER the dispatched command, so a
    `python -m pytest` behind `uv run python` is still visible."""
    i = _first_positional(argv)
    if i is None:
        return None, []
    sub = _basename(argv[i])
    rest = argv[i + 1:]
    if sub == "tool":
        j = _first_positional(rest)
        if j is None:
            return sub, []
        sub = _basename(rest[j])
        rest = rest[j + 1:]
    if sub != "run":
        return sub, rest
    m = _first_positional(rest)
    if m is None:
        return "run", []
    return _basename(rest[m]), rest[m + 1:]


def _uvx_target(argv):
    """(basename, rest) for `uvx ...` (uv tool run shorthand)."""
    m = _first_positional(argv)
    if m is None:
        return None, []
    return _basename(argv[m]), argv[m + 1:]


def _cargo_subcommand(argv):
    """The first positional of a cargo invocation: `cargo -C wt test` -> test.
    A `+toolchain` token is neither a flag nor the subcommand."""
    skip = False
    for tok in argv:
        if skip:
            skip = False
            continue
        if tok == "--":
            return None
        if tok.startswith("+"):
            continue
        if tok.startswith("-") and len(tok) > 1:
            skip = tok in _CARGO_VALUE_FLAGS
            continue
        return _basename(tok)
    return None


def _refused_head(head, argv):
    """'pytest' or 'cargo' when this command position is a raw run, else None."""
    if head == "pytest":
        return "pytest"
    if _is_python(head) and _has_dash_m_module(argv, "pytest"):
        return "pytest"
    if head == "uv":
        target, rest = _uv_target(argv)
        if target == "pytest":
            return "pytest"
        if _is_python(target) and _has_dash_m_module(rest, "pytest"):
            return "pytest"
    if head == "uvx":
        target, _rest = _uvx_target(argv)
        if target == "pytest":
            return "pytest"
    if head == "cargo" and _cargo_subcommand(argv) == "test":
        return "cargo"
    return None


def _head_of(segment, greedy=False):
    """The command-position token of `segment`, plus its remaining argv.

    Walks past transparent wrappers, their flag values, and assignment
    prefixes. `greedy` reads an UNLISTED wrapper flag as value-taking.
    Neither reading is safe alone, so the caller reads BOTH and refuses when
    either lands on a raw run.
    """
    i = 0
    saw_wrapper = False
    wrapper = ""
    while i < len(segment):
        tok = segment[i]
        if tok and all(ch in PUNCT_CHARS for ch in tok):
            i += 1
            continue
        base = _basename(tok)
        if base in TRANSPARENT:
            saw_wrapper = True
            wrapper = base
            i += 1
            if (
                base in _POSITIONAL_LEAD
                and i < len(segment)
                and not segment[i].startswith("-")
            ):
                i += 1
            continue
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", tok):
            i += 1
            continue
        if saw_wrapper and tok.startswith("-"):
            takes_value = greedy or tok in VALUE_FLAGS.get(wrapper, ())
            i += 2 if (takes_value and i + 1 < len(segment)) else 1
            continue
        return base, segment[i + 1:]
    return None, []


def _payload_of(head, argv):
    """The script text a shell was handed with -c, or None.

    Short options bundle: `bash -lc '...'` is the same call as `-c`.
    """
    if head in SHELLS:
        for idx, tok in enumerate(argv):
            if re.fullmatch(r"-[A-Za-z]*c", tok) and idx + 1 < len(argv):
                return argv[idx + 1]
    return None


def _segments(tokens):
    """Split a token list on command separators."""
    out, current = [], []
    for tok in tokens:
        if _is_separator(tok):
            if current:
                out.append(current)
            current = []
        else:
            current.append(tok)
    if current:
        out.append(current)
    return out


def _refused_segment(tokens, depth=0):
    """(kind, command text) of the first raw run in command position, or None.

    Read per pipeline stage, with both wrapper-flag readings. A `bash -c`
    payload recurses as its own command text, bounded by depth. A pytest fed
    to `xargs` is a known fail-open: it is not in this shell's command
    position.
    """
    if depth > 2:
        return None
    for segment in _segments(tokens):
        parts, current = [], []
        for tok in segment:
            if tok in ("|", "|&"):
                if current:
                    parts.append(current)
                current = []
            else:
                current.append(tok)
        if current:
            parts.append(current)
        for part in parts:
            for head, argv in (_head_of(part), _head_of(part, greedy=True)):
                if head is None:
                    continue
                kind = _refused_head(head, argv)
                if kind:
                    return kind, " ".join(part)
                payload = _payload_of(head, argv)
                if payload:
                    try:
                        found = _refused_segment(
                            _tokens(_strip_heredocs(payload)), depth + 1
                        )
                    except ValueError:
                        continue
                    if found:
                        return found
    return None


def _repo_root(cwd):
    """The enclosing git repository root, or None when unresolvable."""
    try:
        proc = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=cwd or None,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except Exception:  # noqa: BLE001 -- no repo we can prove, allow
        return None
    root = proc.stdout.strip()
    return root or None


def _is_footnote_checkout(root):
    """True when root is a footnote checkout or worktree of one. Both markers
    are tracked files, so every worktree carries them; a random other repo
    the plugin happens to be active in is out of scope."""
    root = os.path.realpath(root)
    return os.path.isdir(os.path.join(root, "cli", "src", "fno")) and (
        os.path.isfile(os.path.join(root, "hooks", "hooks.json"))
    )


PYTEST_REASON = (
    "[fno test-run guard] `{cmd}` runs pytest outside the suite admission. "
    "A raw suite takes no test:suite slot, waits behind no live cargo build, "
    "and imports whichever fno is first on PYTHONPATH - on this machine two "
    "raw suites at once are the measured crush behind this guard.\n\n"
    "Run `fno doctor test [paths...]` instead: it takes the test:suite "
    "claim, holds while another suite or cargo build is live, and pins "
    "PYTHONPATH to this worktree."
)

CARGO_REASON = (
    "[fno test-run guard] `{cmd}` runs the crates suite unadmitted. Raw "
    "`cargo test` takes no test:suite slot and waits behind no live cargo "
    "build.\n\n"
    "Run `fno doctor test rust` instead: it admits the crates suite under "
    "the same claim and bounds it to one run on this machine."
)


def decide(command, cwd=None, depth=0):
    """The whole verdict for one command string in one directory: a refusal,
    or None to allow.

    Separated from main() so the test suite exercises the same function the
    hook does, rather than a second implementation of the same predicate.
    Two stages, cheapest first: find a raw run in command position (pure
    token walk, every call), and only then resolve the repository (a git
    subprocess).
    """
    try:
        tokens = _tokens(_strip_heredocs(command))
        hit = _refused_segment(tokens, depth)
    except ValueError:
        return None  # unbalanced quotes: cannot tell command position, allow
    except Exception:  # noqa: BLE001 -- fail open, always
        return None
    if not hit:
        return None
    root = _repo_root(cwd)
    if not root or not _is_footnote_checkout(root):
        return None
    kind, shown = hit
    return (CARGO_REASON if kind == "cargo" else PYTEST_REASON).format(cmd=shown)


def _guard_mark(decision):
    """One guard_decision row per run: the positive liveness signal that this
    guard ran and what it decided. Best-effort by contract: any failure is
    swallowed and can never change a decision."""
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
            '{"ts":"%s","type":"guard_decision","data":{"guard":"test-run-guard",'
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

    refusal = decide(command.strip(), cwd=os.getcwd())
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
