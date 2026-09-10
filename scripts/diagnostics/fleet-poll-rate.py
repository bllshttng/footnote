#!/usr/bin/env python3
"""Sample the process table for whole-fleet `agents truth --handles` children.

Counting rule: split each `ps` row into argv and TOKEN-match, never
substring-match the raw line. A wrapper whose own command line carries the
pattern (the sampler's own grep/pipeline shape) splits into tokens glued to
quotes or backslashes, so exact token equality does not count it. A positive
control (any fno-agents* or python-shaped process observation) is counted in
the same pass: when the control is also zero the instrument did not run, and
a clean 0 would be a lie, so the run refuses rather than reports it.

Exit 0 when hit samples stay at or below --max-hits, 1 above it, 2 when the
control is empty.
"""

import argparse
import subprocess
import sys
import time

PS_BIN = "/bin/ps"


def is_control(argv):
    basename = argv[0].rsplit("/", 1)[-1].lower()
    return basename.startswith(("fno-agents", "fno-py", "python"))


def count_ps_lines(lines, pattern):
    """One pass over ps rows: (hit ppids, control observation count)."""
    hits, control = [], 0
    for line in lines:
        parts = line.split()
        if len(parts) < 3:
            continue
        argv = parts[2:]
        if is_control(argv):
            control += 1
        if all(tok in argv for tok in pattern):
            hits.append(int(parts[1]))
    return hits, control


def sample_once(pattern):
    out = subprocess.run(
        [PS_BIN, "-Ao", "pid=,ppid=,command="], capture_output=True, text=True
    ).stdout
    return count_ps_lines(out.splitlines(), pattern)


def resolve_parents(ppids):
    if not ppids:
        return ["distinct parent pids: none"]
    out = subprocess.run(
        [PS_BIN, "-p", ",".join(map(str, sorted(ppids))), "-o", "pid=,command="],
        capture_output=True,
        text=True,
    ).stdout
    rows = [
        f"parent ppid {parts[0]}: {parts[1]}"
        for line in out.splitlines()
        if len(parts := line.split(None, 1)) == 2
    ]
    return rows or [f"distinct parent pids (unresolved): {sorted(ppids)}"]


def verdict(hit_samples, control, max_hits):
    if control == 0:
        return 2
    return 1 if hit_samples > max_hits else 0


def self_check():
    child = "  101   500 /usr/local/bin/fno-py agents truth --handles aa,bb --json"
    # The wrapper carries the pattern inside a quoted shell word, so the
    # whitespace split glues it to punctuation: token equality must reject it.
    wrapper = (
        "  102   500 /bin/zsh -c 'ps -Ao command | grep \"truth --handles\"'"
    )
    daemon = "  201     1 fno-agents-daemon --home /Users/op/.fno/agents"
    hits, control = count_ps_lines([child, wrapper, daemon], ("truth", "--handles"))
    assert hits == [500], f"wrapper must not count, child must: {hits}"
    assert control == 2, f"child and daemon are both control rows: {control}"
    hits0, control0 = count_ps_lines([], ("truth", "--handles"))
    assert control0 == 0 and hits0 == []
    assert (
        verdict(len(hits0), control0, 3) == 2
    ), "an empty control must exit non-zero, never report a clean 0"
    print("self-check: ok")
    return 0


def main():
    ap = argparse.ArgumentParser(
        description="Measure how often a process-table pattern is alive, with a positive control."
    )
    ap.add_argument("--samples", type=int, default=25)
    ap.add_argument("--interval", type=float, default=0.4)
    ap.add_argument("--max-hits", type=int, default=3)
    ap.add_argument(
        "--pattern",
        nargs="+",
        default=["truth", "--handles"],
        help="argv tokens naming the child (default: the whole-fleet truth batch)",
    )
    ap.add_argument("--self-check", action="store_true")
    args = ap.parse_args()
    if args.self_check:
        return self_check()
    ppids, control = [], 0
    hit_samples = 0
    for i in range(args.samples):
        h, c = sample_once(tuple(args.pattern))
        ppids.extend(h)
        control += c
        hit_samples += 1 if h else 0
        if i < args.samples - 1:
            time.sleep(args.interval)
    print(f"samples: {args.samples}")
    print(f"hit samples: {hit_samples} (max allowed: {args.max_hits})")
    print(f"control observations: {control}")
    for line in resolve_parents(ppids):
        print(line)
    v = verdict(hit_samples, control, args.max_hits)
    if v == 2:
        print("control empty; the instrument did not run", file=sys.stderr)
    return v


if __name__ == "__main__":
    sys.exit(main())
