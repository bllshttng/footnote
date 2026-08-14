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

# Characters shlex may accumulate into a single operator token.
PUNCT_CHARS = set("();<>|&\n")
# The ones that actually END a command. `|` is deliberately absent: `yes | head
# -c 1M` is ONE pipeline and the `head` bounds it, so splitting there would hide
# the bound from the generator it bounds.
CONTROL_CHARS = set(";&\n")


def _is_separator(tok):
    """True when this token ends one command and starts the next.

    Membership in a fixed set is not enough. shlex accumulates ADJACENT
    punctuation into one token, so `x=$(echo 1); yes > /dev/null` yields `');'`
    - which matched no entry in the old set, left the whole line as a single
    segment, and let every generator after a command substitution through. A
    systematic sweep of 1512 unbounded commands found 84 misses and all 84 were
    that one shape.
    """
    if not tok or any(ch not in PUNCT_CHARS for ch in tok):
        return False
    # A `&` next to a redirect arrow is part of the redirect, never a control
    # operator. shlex merges `2>&1` into `2`, `>&`, `1`, and treating `>&` as a
    # separator split `yes 2>&1 | head -c 1M` into `['yes','2']` and
    # `['1','|','head',...]`, stranding the bound in the wrong segment and
    # refusing a legitimately bounded command.
    if ">" in tok or "<" in tok:
        return False
    # `|&` is bash's pipe-including-stderr, a PIPE and not a control operator.
    # Split here it stranded the reader in a later segment, so `yes |& head -c 1M`
    # read as a last-stage `yes` and was refused. Same carveout as `2>&1` above.
    if tok == "|&":
        return False
    # A bare `)` opens a `case` arm's body, so command position restarts there.
    # Without this the arm body sat mid-segment behind the `case` head and
    # `case $x in a) yes > /dev/null;; esac` walked through. It also closes a
    # subshell, where splitting costs nothing: the contents already had their
    # own segment.
    if tok == ")":
        return True
    return bool(CONTROL_CHARS & set(tok)) or "||" in tok

# Wrappers that are transparent to command position: `nohup yes` still runs
# `yes` first. Without these the generator sits at index 1 and reads as an
# argument, which is exactly how specimen 1 was written.
TRANSPARENT = {
    "nohup", "setsid", "exec", "time", "env", "command", "builtin",
    "nice", "ionice", "taskpolicy", "stdbuf", "caffeinate", "sudo",
    # Shell keywords and grouping that open a body. `;` splits `do yes` into
    # its own segment, and without these the walk stops at the opener and reads
    # the generator as an argument: `for i in 1 2; do yes; done` was allowed.
    # `(` matters most: `( cmd & )` is THE canonical detach idiom, and this
    # repo's own hooks use it, so the likeliest way to write specimen 1 walked
    # straight past the guard.
    "do", "then", "else", "elif", "{", "(", "!",
}

#: Flags that swallow the next token, so skipping the flag alone still leaves a
#: value where the command should be (`sudo -u me yes`). Read PER WRAPPER: one
#: shared set is wrong in both directions, because `-n` takes a value for `nice`
#: and takes none for `sudo`. Shared, `sudo -n yes > /dev/null` skipped the
#: generator and resolved the command to whatever followed it.
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
    # No comment character. shlex swallows from an unquoted `#` to end of line,
    # and `#` is ordinary shell text far more often than it starts a comment:
    # `echo ${#PATH}; yes > /dev/null` lost everything after the `${` and the
    # generator was never seen. A real trailing comment survives as tokens, and
    # that is only safe while every reader works in command position. It was not:
    # `_has_bound` scanned every token, so `yes > /dev/null  # no timeout needed`
    # read its own comment as the bound. Any new reader added here must take its
    # answer from `_head_of`, never from a scan of the token list.
    lex.commenters = ""
    return list(lex)


#: `<<EOF`, `<<-'EOF'`, `<< "EOF"`. The delimiter word is what ends the body.
_HEREDOC = re.compile(r"<<-?\s*([\"']?)([A-Za-z_][A-Za-z0-9_]*)\1")

#: Words that leave a loop when they are the COMMAND, not an argument.
_ESCAPES = {"break", "exit", "return"}

#: Loop conditions that never let the loop end, per keyword. Inverted between
#: the two: `while` repeats on success, `until` repeats on failure.
_NEVER_ENDS = {"while": {"true", ":"}, "until": {"false"}}


def _strip_heredocs(text):
    """Drop heredoc BODIES. They are data written to a file, not commands.

    `cat > poll.sh <<'EOF' ... while true; do ...; done ... EOF` writes a
    script; it does not run one. Read as commands the body was refused, so
    writing a file that merely CONTAINS a poll loop was blocked. The sibling
    hook git-protection.py strips them for the same reason.

    Only strips when the terminator is actually found, so a `<<` that was really
    a quoted string or an arithmetic shift cannot swallow the rest of the
    command and hide a generator behind it.
    """
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


def _case_regions(segments):
    """One bool per segment: is it inside a `case ... esac`?

    A `|` between `case` patterns is ALTERNATION, not a pipe. Split as a pipe,
    the last alternative became a pipeline stage with no reader, so the ordinary
    confirm idiom `case $a in y|yes) echo go;; esac` was refused. Worse, it was
    order-dependent: `yes|y)` allowed and `y|yes)` denied.

    Read in COMMAND POSITION. Counting the WORD over every token was a
    token-list scan, and this file's own lexer comment forbids exactly that,
    because comment text survives as tokens. Any comment carrying the bare word
    `case` opened a region that never closed, which both refused the remedy the
    refusal text advertises (`# drained case` then `yes | head -c 1M`) and hid a
    real generator (the same comment then `: | yes > /dev/null`). 43 of 366,573
    real commands open such a region, most of them ordinary English comments.
    """
    out, depth = [], 0
    for segment in segments:
        head = _head_of(segment)[0]
        opens = 1 if head == "case" else 0
        closes = 1 if head == "esac" else 0
        out.append(depth > 0 or opens > 0)
        depth = max(0, depth + opens - closes)
    return out


def _pipe_parts(segment, split=True):
    """The commands of one pipeline, split on `|`.

    Command position resets at every pipe. `_head_of` alone reads only the
    FIRST command of a segment, so a generator downstream of a pipe was never
    examined at all: `: | yes > /dev/null` was allowed.

    `split=False` inside a `case` region, where `|` separates patterns. A real
    pipeline in a case ARM is then read as one part, which is a miss in the
    same fail-open direction as the rest of this file, and a refusal of the
    plain `y|yes)` idiom is the more expensive error.
    """
    return [part for part, _ in _pipe_spans(segment, 0, split=split)]


def _pipe_spans(segment, start, split=True):
    """`_pipe_parts`, but each part carries its absolute index in the token list.

    The index is what lets a `for` head look up its OWN arithmetic header. The
    header is not in the segment (`((;;));` is punctuation, so `_segments`
    drops it as a separator), and matching headers to loops by counting `for`s
    in a second walk produced three separate bugs: a subshell `(`, an
    assignment prefix, and a `case` pattern alternation each made the two walks
    count different sets, and the flag then described one loop while being
    charged to another. Positional pairing is the defect; this removes it.

    Segment tokens are contiguous in the token list, because `_segments` splits
    only on single separator tokens, so `start + offset` is exact.
    """
    if not split:
        return [(segment, start)] if segment else []
    parts, current, cur_start = [], [], start
    for offset, tok in enumerate(segment):
        # `|&` pipes stdout AND stderr. It is a pipe, so it resets command
        # position exactly as `|` does.
        if tok in ("|", "|&"):
            if current:
                parts.append((current, cur_start))
            current, cur_start = [], start + offset + 1
        else:
            if not current:
                cur_start = start + offset
            current.append(tok)
    if current:
        parts.append((current, cur_start))
    return parts


def _segments(tokens):
    """Split a token list on command separators. Redirections stay attached to
    their segment; only control operators split."""
    return [seg for seg, _ in _segment_spans(tokens)]


def _segment_spans(tokens):
    """`_segments`, but each segment carries its start index in `tokens`."""
    out, current, start = [], [], 0
    for idx, tok in enumerate(tokens):
        if _is_separator(tok):
            if current:
                out.append((current, start))
            current, start = [], idx + 1
        else:
            if not current:
                start = idx
            current.append(tok)
    if current:
        out.append((current, start))
    return out


def _head_of(segment, greedy=False):
    """The command-position token of `segment`, plus its remaining argv.

    Walks past transparent wrappers and their flag values so the real command
    surfaces. Returns (None, []) for a segment with no command.

    `greedy` reads an UNLISTED wrapper flag as value-taking. `VALUE_FLAGS` is
    hand-written, so any flag missing from it consumed one token and handed
    command position to its own value: `caffeinate -t 3600 yes > /dev/null`
    resolved to a command called `3600` and was allowed, and `timeout 3` on it
    exits 124, so the `yes` really does run forever. Both readings are tested,
    because neither is safe alone. Greedy alone loses `sudo -E yes > /dev/null`,
    where `-E` is a real boolean and the greedy skip eats the generator.
    """
    i = 0
    saw_wrapper = False
    wrapper = ""
    while i < len(segment):
        tok = segment[i]
        # A leading all-punctuation token is shell grammar, not a command. `(`
        # is transparent but `)` was not, so a `case` arm's closing paren sat in
        # command position and `case $x in a) yes > /dev/null;; esac` walked
        # through. Skipped rather than listed, since the lexer can leave several
        # such shapes here.
        if tok and all(ch in PUNCT_CHARS for ch in tok):
            i += 1
            continue
        base = tok.rsplit("/", 1)[-1]
        if base in TRANSPARENT:
            saw_wrapper = True
            wrapper = base
            i += 1
            continue
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", tok):
            # A leading assignment is a prefix, not the command: `FOO=1 yes` and
            # `env FOO=1 yes` both still run `yes`.
            i += 1
            continue
        if saw_wrapper and tok in {"-v", "-V"}:
            # `command -v yes` / `type -V yes` PRINT a path, they run nothing.
            # The generic flag skip below walked past the `-v` and resolved the
            # lookup target as the command, so an ordinary capability probe was
            # refused.
            return None, []
        if saw_wrapper and tok.startswith("-"):
            # A wrapper's own options, not the command. Skipping only a fixed
            # trio left `sudo -u me yes` resolving to a command called `-u`,
            # which defeated the `sudo` and `env` entries above.
            takes_value = greedy or tok in VALUE_FLAGS.get(wrapper, ())
            i += 2 if (takes_value and i + 1 < len(segment)) else 1
            continue
        return base, segment[i + 1:]
    return None, []


def _has_bound(segment):
    """True when this segment carries something that makes the process end.

    A bound is read per segment rather than per whole command: a `timeout` on
    an unrelated earlier segment must not license an unbounded generator later
    in the same line.

    Read in COMMAND POSITION, like everything else here. Scanning every token
    let any ARGUMENT spelling `timeout` count as one, and three commands that
    are literally specimen 1 walked through: `yes timeout > /dev/null`,
    `yes -t 5 > /dev/null` (`-t` is a stress flag; `yes` just prints it
    forever), and worst of all `yes > /dev/null  # no timeout needed`. Comment
    text survives as tokens here on purpose, so the most ordinary thing an
    agent writes was also the easiest bypass in the file.
    """
    head, argv = _head_of(segment)
    if head is None:
        return False
    if head in {"timeout", "gtimeout"}:
        return True
    # `-t <seconds>` bounds `stress`, and means nothing to anything else. Both
    # tools also spell it `--timeout`, and both accept a unit suffix. Reading
    # only bare digits after `-t` refused `stress --timeout 60` and
    # `stress-ng --cpu 1 -t 30s`, which are the spellings their own manuals use.
    # The refusal text advertises this remedy, so a rejected spelling of it is
    # the worst kind of refusal: the guard naming a fix it will not accept.
    if head in {"stress", "stress-ng"}:
        for i, tok in enumerate(argv):
            if tok in {"-t", "--timeout"} and i + 1 < len(argv):
                if re.fullmatch(r"\d+[smhdSMHD]?", argv[i + 1]):
                    return True
            if re.fullmatch(r"-t\d+[smhdSMHD]?", tok):
                return True
            if re.fullmatch(r"--timeout=\d+[smhdSMHD]?", tok):
                return True
    # `count=` is dd's own operand, so it is safe to read positionally: it
    # cannot be a redirect target and it names no other command.
    return head == "dd" and any(a.startswith("count=") for a in _operands(argv))


def _generator_reason(head, argv):
    """Why this segment can never end, or None when it can."""
    if head == "yes":
        return ("`yes` never ends. Writing to /dev/null it never even receives "
                "SIGPIPE, because there is no reader to go away and the sink "
                "always accepts.")
    # The truth table INVERTS between the two keywords: `while` repeats while the
    # test succeeds, `until` repeats while it fails. One shared condition set
    # refused `while false` and `until true`, both of which exit immediately,
    # and a user disabling a loop with `while false` got a refusal citing a
    # 71-core-hour incident.
    if argv and argv[0] in _NEVER_ENDS.get(head, ()):
        return "`%s %s` is an unbounded loop header." % (head, argv[0])
    if head == "sleep" and argv and argv[0] in {"infinity", "inf"}:
        return "`sleep infinity` never returns."
    if head in {"stress", "stress-ng"}:
        return "`%s` runs until killed unless given `-t`." % head
    operands = _operands(argv)
    if head == "dd" and any(a in {"if=/dev/zero", "if=/dev/urandom"} for a in operands):
        return "`dd` from an endless device never reaches EOF without `count=`."
    if head in {"cat", "sha256sum", "shasum", "md5", "md5sum", "base64", "wc"}:
        if any(a in {"/dev/zero", "/dev/urandom", "/dev/random"} for a in operands):
            return "reading an endless device never reaches EOF."
    return None


def _operands(argv):
    """`argv` without redirect operators and the filenames they name.

    WRITING to an endless device is ordinary; only READING from one never ends.
    Read as argv, `cat report.txt > /dev/zero` looked like a read and was
    refused.

    So this drops `>` targets ONLY. Dropping `<` sources too inverted the very
    distinction the paragraph above draws: `<` IS the read, and skipping it let
    `cat < /dev/zero`, `base64 < /dev/urandom` and `wc -l < /dev/zero` through,
    every one of them a specimen-1-class process that never ends.
    """
    out = []
    for i, tok in enumerate(argv):
        if ">" in tok:
            continue  # the operator itself
        if i and ">" in argv[i - 1]:
            continue  # what it writes to
        if "<" in tok:
            # The operator, but never its source: that file is the operand.
            stripped = tok.split("<")[-1]
            if stripped:
                out.append(stripped)  # `cat </dev/zero`, no space
            continue
        out.append(tok)
    return out


def _payload_of(head, argv):
    """The script text a shell was handed with -c, or None.

    Short options bundle: `bash -lc '...'` and `sh -ec '...'` are the same call
    as `-c`, and an exact `-c` match walked past both.
    """
    if head in SHELLS:
        for idx, tok in enumerate(argv):
            if re.fullmatch(r"-[A-Za-z]*c", tok) and idx + 1 < len(argv):
                return argv[idx + 1]
    return None


def _arith_endless(tokens, for_index):
    """Can the `for` at `for_index` never end?

    Bash reads three `;`-separated arithmetic expressions and the MIDDLE one is
    the condition. An empty condition is what never ends: `for ((i=0;i<10;i++))`
    counts to ten and stops, `for ((i=0;;))` does not. A shape test that reads
    only `((;;))` calls the first one infinite too.

    Read from tokens, never from the raw text. A text match denied
    `echo "for ((;;))"`, `rg 'for ((;;))' hooks/`, and a commit message naming
    the shape. None of those has a shell loop in it, and the third blocked
    writing about this guard in the repo that ships it.

    The header is rebuilt by joining tokens, because it is not in the segment:
    `((;;));` is punctuation, so `_segments` drops it as a separator and by the
    time there are segments the condition is gone.

    Two rules keep that join from reaching past the header. The arithmetic must
    start IMMEDIATELY after `for`, and a token carrying whitespace ends it. A
    plain join over every following token read the CONTENT of a quoted argument:
    `for f in "for ((;;))"` is one word list and one quoted string, and it was
    denied. Moving the read off raw text onto tokens did not fix that on its
    own, because the quoted text is still inside a token.

    Asked PER LOOP, at the index the caller already resolved. The previous
    design built a positional list of flags in a second walk and paired them to
    loops by counting. That pairing produced three separate bugs, because the
    two walks disagreed about which `for`s were in command position: a subshell
    `(`, an assignment prefix, and a `case` pattern alternation each shifted the
    count, and a flag then described one loop while being charged to another. In
    the last of those, `case $x in a|for) echo hi;; esac; for ((;;)); do :; done`
    was ALLOWED, because the pattern word `for` consumed the real loop's flag.
    There is no counter now, so there is nothing left to drift.
    """
    rest = tokens[for_index + 1:]
    if not rest or not rest[0].startswith("(("):
        return False  # `for f in *`, a word list and not arithmetic
    header = []
    for nxt in rest:
        if nxt == "do" or any(ch.isspace() for ch in nxt):
            break
        header.append(nxt)
        if "))" in nxt:
            break
    joined = "".join(header)
    end = joined.find("))", 2)
    if end == -1:
        return False  # a header this walk cannot read; fail open
    parts = joined[2:end].split(";")
    return len(parts) == 3 and not parts[1].strip()


def _find_unbounded(text, inherited_bound=False, depth=0):
    """First (reason, segment_text) that can never end and is unbounded.

    Recurses one level into `bash -c '...'` payloads, carrying the enclosing
    segment's bound down: `timeout 300 bash -c 'yes'` IS bounded, and the
    bound lives outside the payload it bounds.
    """
    if depth > 2:
        return None
    text = _strip_heredocs(text)
    all_tokens = _tokens(text)
    all_segments = _segments(all_tokens)
    # A loop header runs forever only if nothing inside leaves the loop.
    # `while true; do sleep 5; gh pr view && break; done` is the standard poll
    # and it ends; refusing it blocked an ordinary shape at the Bash boundary.
    # Read in COMMAND POSITION, through the same walk the rest of this function
    # uses. A text match here read `echo break` and `rg break src` as escapes,
    # which is a hole with no shell in it at all.
    #
    # Only AFTER the first loop header counts. An escape can leave a loop it is
    # inside; one in a precondition that runs first cannot. Read over the whole
    # command, `cd /tmp || exit 1; while true; do sleep 60; done &` cleared its
    # own loop from a line that had already run, and that is an ordinary way to
    # write the keepalive this guard exists to refuse.
    # An escape belongs to the loop it sits INSIDE, which needs a stack, not a
    # window. Three cheaper rules were each tried and each was wrong. Reading
    # the whole command let `cd /tmp || exit 1; while true; do sleep 60; done &`
    # clear its own loop from a line that had already run. Stopping at the first
    # `done` broke on a NESTED loop, so the ordinary poll
    # `while true; do for n in 1 2; do echo $n; done; gh pr view && break; done`
    # was refused: the inner `done` truncated the scan before the real `break`.
    # And one shared boolean let an escape in an EARLIER loop license every
    # later one, so `while true; do break; done; while true; do sleep 60; done &`
    # was allowed with specimen 1 sitting in the second half.
    #
    # ponytail: `break` leaves one loop and `exit` leaves the shell, but both
    # mark every open loop here. Distinguishing them costs another rule and
    # buys a refusal, and this file fails open on purpose.
    spans = _segment_spans(all_tokens)
    all_segments = [seg for seg, _ in spans]
    in_case = _case_regions(all_segments)
    # (head, absolute token index of that head) per pipe stage, per segment. The
    # index is what lets a `for` ask about its OWN header instead of being
    # paired to one by position in a second walk.
    heads = [
        [
            (_head_of(part)[0], part_start + (part.index("for") if "for" in part else 0))
            for part, part_start in _pipe_spans(segment, seg_start, split=not cased)
        ]
        for (segment, seg_start), cased in zip(spans, in_case)
    ]
    escaped_at = {}
    endless_for = []
    stack = []
    for idx, hs in enumerate(heads):
        for head, for_index in hs:
            if head in {"while", "until", "for"}:
                endless = head == "for" and _arith_endless(all_tokens, for_index)
                stack.append({"idx": idx, "endless": endless, "escaped": False})
            elif head == "done" and stack:
                closed = stack.pop()
                escaped_at[closed["idx"]] = closed["escaped"]
                if closed["endless"] and not closed["escaped"]:
                    endless_for.append(closed["idx"])
            elif head in _ESCAPES:
                for open_loop in stack:
                    open_loop["escaped"] = True
    # A loop with no `done` is malformed; read it as escaped rather than refuse.
    for open_loop in stack:
        escaped_at[open_loop["idx"]] = True
    # A bound anywhere in the command clears an endless `for` header, so
    # `for ((;;)); do timeout 5 x; done` still passes.
    command_bounded = inherited_bound or any(
        _has_bound(seg) for seg in all_segments
    )
    if endless_for and not command_bounded:
        return (
            "`for ((;;))` is an unbounded loop header.",
            " ".join(all_segments[endless_for[0]]),
        )
    for seg_index, segment in enumerate(all_segments):
        parts = _pipe_parts(segment, split=not in_case[seg_index])
        for idx, part in enumerate(parts):
            # A downstream reader bounds the stage feeding it: `yes | head -c 1M`
            # dies of SIGPIPE when head exits, and `yes | apt-get install foo` is
            # the standard auto-confirm idiom that a guard must not refuse. The
            # LAST stage has no reader, so the pipe bounds nothing for it.
            # ponytail: a reader that never exits (`yes | wc -l`) still hangs.
            # Deciding that statically needs a model of every consumer, so this
            # accepts the false negative rather than break the common case.
            #
            # The bound is read PER STAGE. Read per segment, a `timeout` on one
            # stage licensed every other: `timeout 1 true | yes > /dev/null` was
            # allowed and leaves specimen 1 behind after one second. `timeout`
            # bounds the process it wraps, never its pipeline siblings.
            bounded = (
                inherited_bound or _has_bound(part) or idx < len(parts) - 1
            )
            head, argv = _head_of(part)
            if head is None:
                continue
            if head == "ulimit":
                # `ulimit -t` applies to the whole shell, so it can only ever sit
                # in an EARLIER segment than the process it bounds. Read per
                # segment it bounded nothing at all, and the refusal text below
                # advertises it as a remedy: `ulimit -t 60; yes` was refused.
                if "-t" in argv:
                    inherited_bound = True
                continue
            if head in {"while", "until"} and escaped_at.get(seg_index):
                continue
            reason = _generator_reason(head, argv)
            if reason and not bounded:
                return reason, " ".join(segment)
            # The same part read with an unlisted wrapper flag consuming its
            # value. Neither reading is safe alone, so a generator under either
            # one is a refusal. See `_head_of`.
            greedy_head, greedy_argv = _head_of(part, greedy=True)
            if greedy_head is not None and greedy_head != head:
                greedy_reason = _generator_reason(greedy_head, greedy_argv)
                if greedy_reason and not bounded:
                    return greedy_reason, " ".join(segment)
            payload = _payload_of(head, argv)
            if payload:
                found = _find_unbounded(
                    payload, inherited_bound=bounded, depth=depth + 1
                )
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
        # Every remedy named here must actually satisfy the guard. `head -c`
        # used to sit in this list as if it were a bound on its own, but only a
        # DOWNSTREAM reader bounds anything, so following the advice literally
        # earned a second refusal. `ulimit -t` was in the list while its check
        # was dead code. A refusal that advertises a remedy it then refuses is
        # worse than one that says nothing.
        "Any of `timeout`, `gtimeout`, `count=`, `ulimit -t` in an earlier "
        "command, or `-t <seconds>` satisfies this guard. So does piping INTO "
        "a reader that exits, as in `yes | head -c 1M`."
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
