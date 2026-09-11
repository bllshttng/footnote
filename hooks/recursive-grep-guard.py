#!/usr/bin/env python3
"""PreToolUse guard: refuse a recursive `grep` when this repository contains a
Cargo build cache.

The fault is specific to this repo layout. Harness-native worktrees live below
`.claude/worktrees/`, and each carries its own `crates/*/target` build cache.
`grep -r`/`-R` ignore `.gitignore`, so a search scoped to `crates/` - or with
no path at all - descends into every nested worktree's object files. On
2026-09-09 nine concurrent `grep -r` processes ran on one 12-core box, three at
45-51 percent CPU each, and the worst (`grep -rn ownership_defect cli/src/fno/
crates/ scripts/ tests/`) ran for 1 hour 56 minutes without returning. The
AGENTS.md advice to prefer `rg` was already in context for every one of those
sessions; prose does not fire at the moment of a tool call, a refusal does.

A directory named `target` is NOT evidence. This repo holds legitimate source
at `cli/src/fno/target`, `skills/target`, and `tests/target`, and a 2026-09-02
name-based sweep deleted 66 source directories across 26 worktrees. A cache is
confirmed only by Cargo's own `CACHEDIR.TAG` marker, resolved through
symlinks, and the walk prunes a confirmed cache instead of entering it - the
guard never traverses the artifacts it exists to protect.

Parse-only, stdlib alone, fails OPEN on anything unexpected: a guard that
breaks a session on its own bug is worse than the greps it prevents.
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


# Wrappers that are transparent to command position: `sudo grep -r ...` still
# runs the grep. Shell keywords that open a body are here too, so `do grep -r
# x .; done` inside a `for` resolves to the grep.
TRANSPARENT = {
    "nohup", "setsid", "exec", "time", "env", "command", "builtin",
    "nice", "ionice", "taskpolicy", "stdbuf", "caffeinate", "sudo",
    # `timeout` bounds a process but does not un-make a cache walk, so it is
    # transparent HERE (bg-process-guard treats it as a bound; this guard has
    # no bound that rescues the refusal).
    "timeout", "gtimeout",
    "do", "then", "else", "elif", "{", "(", "!",
}

#: Wrappers whose own positional operand sits BEFORE the wrapped command:
#: `timeout 300 grep ...` spends `300` before the grep ever appears.
_POSITIONAL_LEAD = {"timeout", "gtimeout"}

# Wrapper flags that swallow the next token, so the value is not mistaken for
# the command (`sudo -u me grep -r x .`). Read PER WRAPPER: `-n` takes a value
# for `nice` and none for `sudo`.
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

#: Long recursive spellings. `-r`/`-R` and bundles like `-rn` are matched in
#: the argv scan below; `--recursive` and `--dereference-recursive` here.
_RECURSIVE_LONG = ("--recursive", "--dereference-recursive")

#: grep options whose NEXT token is a value, never a flag: `grep -e -r file`
#: searches for the literal string "-r" and must not read as recursive.
_VALUE_OPTS = {
    "-e", "-f", "-m", "-A", "-B", "-C", "-d",
    "--regexp", "--file", "--max-count", "--after-context",
    "--before-context", "--context", "--include", "--exclude",
    "--exclude-dir", "--label", "--directories", "--color", "--colour",
}


def _tokens(text):
    """Shell tokens with operators kept as their own tokens. Raises ValueError
    on unbalanced quotes, which the caller turns into an allow.

    Newline is made punctuation so it ends a command the way `;` does; in
    stock posix mode shlex swallows it as whitespace and flattens a multi-line
    Bash call into ONE segment. Commenters stay empty: shlex swallows from an
    unquoted `#` to end of line, and `#` is ordinary shell text far more often
    than it starts a comment.
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
    a script that merely CONTAINS `grep -r` must not read as one that runs it.
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


def _head_of(segment, greedy=False):
    """The command-position token of `segment`, plus its remaining argv.

    Walks past transparent wrappers, their flag values, and assignment
    prefixes. `greedy` reads an UNLISTED wrapper flag as value-taking
    (`caffeinate -t 3600 grep -r x .`), which skips one token further.
    Neither reading is safe alone: greedy alone loses `sudo -E grep -r x .`
    to the skipped `-E`, non-greedy alone loses the unlisted flag. The caller
    reads BOTH and refuses when either lands on a recursive grep.
    """
    i = 0
    saw_wrapper = False
    wrapper = ""
    while i < len(segment):
        tok = segment[i]
        if tok and all(ch in PUNCT_CHARS for ch in tok):
            i += 1
            continue
        base = tok.rsplit("/", 1)[-1]
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


def _is_recursive_argv(argv):
    """True when this grep argv carries a recursive option.

    Scans the whole option region rather than stopping at the pattern: grep
    permutes options after operands (`grep foo -r .`). Options that take a
    value consume the next token, so a PATTERN of "-r" (`grep -e -r file`)
    never reads as a flag, and `--` ends the region.
    """
    skip_next = False
    for tok in argv:
        if skip_next:
            skip_next = False
            continue
        if tok == "--":
            return False
        if tok in _VALUE_OPTS:
            skip_next = True
            continue
        if tok in _RECURSIVE_LONG:
            return True
        if tok.startswith("-") and not tok.startswith("--") and len(tok) > 1:
            # A bundle LED by a value-taking option is that flag plus its
            # attached value, never a flag list: `-fr patterns.txt` reads a
            # pattern FILE named r, and `-er` searches for the string "r".
            # Neither is recursive, and scanning the whole token would deny
            # both. Bundles led by a boolean letter (`-rn`, `-nr`) are lists.
            if tok[1] in "efmABCD":
                continue
            if "r" in tok[1:] or "R" in tok[1:]:
                return True
    return False


def _payload_of(head, argv):
    """The script text a shell was handed with -c, or None.

    Short options bundle: `bash -lc '...'` is the same call as `-c`.
    """
    if head in SHELLS:
        for idx, tok in enumerate(argv):
            if re.fullmatch(r"-[A-Za-z]*c", tok) and idx + 1 < len(argv):
                return argv[idx + 1]
    return None


def _recursive_grep_segment(tokens, depth=0):
    """First command string that runs a recursive grep, or None.

    Read in command position, per pipeline stage, with both wrapper-flag
    readings. A `bash -c` payload recurses as its own command text, bounded
    by depth. `xargs grep -r` is a known fail-open: the grep is not in this
    shell's command position, and the find that feeds it names its own scope.
    """
    if depth > 2:
        return None
    for segment in _segments(tokens):
        # Splitting on `|` inside a `case` pattern alternation misreads the
        # arm, but neither misread part resolves to a grep head, so the miss
        # is in the fail-open direction this file accepts.
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
                if head == "grep" and _is_recursive_argv(argv):
                    return " ".join(part)
                payload = _payload_of(head, argv)
                if payload:
                    try:
                        found = _recursive_grep_segment(
                            _tokens(_strip_heredocs(payload)), depth + 1
                        )
                    except ValueError:
                        continue
                    if found:
                        return found
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


def _repo_has_cargo_cache(root):
    """True when a directory below `root` carries Cargo's CACHEDIR.TAG.

    Identity is the MARKER at the resolved directory, never the basename:
    this repo ships source named `target` (`cli/src/fno/target`,
    `skills/target`, `tests/target`). A confirmed cache is pruned before
    descending, so the guard never walks the object files it exists to
    protect. Symlinks are confirmed at the resolved directory's marker and
    must stay inside the repository to count.
    """
    root = os.path.realpath(root)
    if os.path.isfile(os.path.join(root, "CACHEDIR.TAG")):
        return True
    try:
        for dirpath, dirnames, _files in os.walk(root, followlinks=False):
            keep = []
            for name in dirnames:
                resolved = os.path.realpath(os.path.join(dirpath, name))
                inside = resolved == root or resolved.startswith(root + os.sep)
                if inside and os.path.isfile(os.path.join(resolved, "CACHEDIR.TAG")):
                    continue  # confirmed cache: never descend into it
                keep.append(name)
            if len(keep) != len(dirnames):
                return True
            dirnames[:] = keep
    except Exception:  # noqa: BLE001 -- unreadable context invents no cache
        return False
    return False


REASON = (
    "[fno recursive-grep guard] `{cmd}` is a recursive grep, and this "
    "repository contains a CACHEDIR.TAG-confirmed Cargo cache. `grep -r` and "
    "`grep -R` ignore .gitignore, so from the repo root they descend into "
    "every nested worktree's build artifacts: on 2026-09-09 nine concurrent "
    "`grep -r` processes ran here and the worst ran for 1 hour 56 minutes "
    "without returning.\n\n"
    "Use the harness Grep tool for ordinary searches (it is ripgrep and "
    "honors .gitignore), or `rg <pattern> <path>`. For an intentional "
    "ignored-file sweep, use `RIPGREP_CONFIG_PATH= rg -uu <pattern>` with an "
    "explicit scope."
)


def decide(command, cwd=None, depth=0):
    """The whole verdict for one command string in one directory: a refusal,
    or None to allow.

    Separated from main() so the test suite exercises the same function the
    hook does, rather than a second implementation of the same predicate.
    Two stages, cheapest first: find a recursive grep in command position
    (pure token walk, every call), and only then inventory the repository
    for a marker-confirmed cache (a pruned filesystem walk).
    """
    try:
        tokens = _tokens(_strip_heredocs(command))
        segment = _recursive_grep_segment(tokens, depth)
    except ValueError:
        return None  # unbalanced quotes: cannot tell command position, allow
    except Exception:  # noqa: BLE001 -- fail open, always
        return None
    if not segment:
        return None
    root = _repo_root(cwd)
    if not root or not _repo_has_cargo_cache(root):
        return None
    return REASON.format(cmd=segment)


def _guard_mark(decision):
    """One guard_decision row per run: the positive liveness signal that this
    guard ran and what it decided. Row shape matches hooks/lib/guard-mark.sh
    so bash and python guards write indistinguishable rows. Best-effort by
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
            '{"ts":"%s","type":"guard_decision","data":{"guard":"recursive-grep-guard",'
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
