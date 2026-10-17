"""Capture driver for the coverage-gate golden corpus (cases.json).

Four modes. Default capture builds every case under a throwaway root, runs the
live Python leg in a hermetic child, and freezes exit and streams as goldens
under tests/golden/coverage_gate. `--case DIR` is the child mode: it re-applies
the environment pins, runs one entry, and prints one `{"exit", "stdout",
"stderr"}` envelope as the last stdout line. `--build-only ROOT` builds the
fixture tree and the stub binaries without running any leg and prints a JSON
map the Rust parity test reuses. `--only SLUG` re-freezes one case.

The stubs under `stub-bin/` (fno-agents, gh, fno) answer each call from an
`answers/*.json` file and journal every call to `journals/*.jsonl`. The folded,
masked journals become the `<key>.calls` golden, so the native leg must issue
the same calls in the same order. A stub with no answer for a call fails loud
(stderr note, exit 3) instead of improvising: a missing answer is a corpus bug,
never a silent success. Stubs locate the case tree through
`FNO_COVERAGE_CASE_DIR`, so cwd does not matter.

The `fno` front stub serves the doors the legs walk: `inbox law match`
(payload on stdin, per-subject answers from a map), `backlog decisions`
(the native waiver probe's transport for the same map), `inbox decide`
(argv, exit and stderr from one answer), and `do pr info` (the native
probe's no-slug head fallback). The resolve_front_binary seam is monkeypatched in
the child to point at this stub, because that resolver probes
crates/fno/target before PATH and a CI workspace that built the fno crate
would otherwise answer the law reads with a real binary. The native head
probe's primary read (`gh api repos/<slug>/pulls/<n>`) lands on the gh
stub, which maps it to its own `pr-info` shape answered from the same head
the capture's pr-hop answer froze.

The goldens freeze the Python legs while they live. After the legs retire the
frozen files are the contract, and a rewritten oracle cannot re-derive them: a
golden can only be captured from the leg it characterizes.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

FIXTURE_DIR = Path(__file__).resolve().parent
TESTS_DIR = FIXTURE_DIR.parents[1]
GOLDEN_DIR = TESTS_DIR / "golden" / "coverage_gate"
REPO_ROOT = TESTS_DIR.parents[2]

# Live-session identity and routing variables that must never leak into a
# hermetic child. The child re-pins the FNO_* ones right after stripping.
STRIP_VARS = [
    "CLAUDE_CODE_SESSION_ID",
    "TARGET_SESSION_ID",
    "SESSION_ID",
    "FNO_HARNESS_NAME",
    "FNO_HARNESS_SESSION_ID",
    "CLAUDE_PLUGIN_ROOT",
    "TARGET_MISSION_ID",
    "FNO_AGENTS_BIN",
    "FNO_AGENTS_FRONT",
    "FNO_AGENTS_WORKER",
    "FNO_REPO_ROOT",
    "FNO_CAPTURE_GOLDEN",
    "FNO_TRACKER_BACKEND",
    "FNO_EVENTS_PATH",
    "FNO_STATE_DIR",
    "FNO_SPACES_DIR",
    "FNO_CLAIMS_ROOT",
    "FNO_CONFIG",
    "FNO_AGENTS_HOME",
    "FNO_COVERAGE_CASE_DIR",
    "GIT_DIR",
    "GIT_WORK_TREE",
]

# The head sha every case fixtures against. substitute_placeholders rewrites
# __HEAD_SHA__ in every case file before the leg runs, and build_git returns
# the same commit, so the stub answers, the planted rows and the git state all
# name one sha.
HEAD_SHA = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c"

DEFAULT_ANSWERS = {
    # The PR head read (verb_call authorized-merge op=pr-head). The value
    # rides the __HEAD_SHA__ placeholder so substitute_placeholders aligns it
    # with the seeded commit the planted rows name.
    "pr-head": {"json": {"head": "__HEAD_SHA__"}},
    # The sized self-review invocation render (verb_call review-level).
    "review-level": {"json": {"invocation": "fno do review --level medium --reviewer code-review"}},
}

DEFAULT_GH_ANSWERS = {
    # gh pr view --json labels: the override-label read.
    "view": {"json": {"labels": []}},
}

DEFAULT_FRONT_ANSWERS = {
    # Per-subject law answers: a map keyed by subject with a default for any
    # subject the case does not name. Each value is a stub answer envelope
    # ({"json": payload}), the shape emit() prints.
    "law": {"default": {"json": {"decisions": [], "damaged": 0}}},
    # inbox decide: the waiver write.
    "decide": {"exit": 0},
}

STUB_AGENTS = r"""#!/usr/bin/python3
# Stand-in for the fno-agents binary. Answers every call from an
# answers/op-<op>[-<n>].json file and journals every call, so the capture
# can prove which ops the leg issued and in what order. An answer file may
# carry "events_row": an appended review_coverage row, so the recomputed-read
# cases see the producer's row land in the space journal the gate reads. A
# call with no answer file fails loud: a missing answer is a corpus bug.
import json
import os
import sys


def root():
    return os.environ.get("FNO_COVERAGE_CASE_DIR") or os.getcwd()


def space_events_path(case_dir):
    # Mirror of paths.space_slug: the canonical path with / swapped for -,
    # under <state_dir>/spaces (FNO_STATE_DIR is pinned to the case dir).
    slug = case_dir.replace("/", "-")
    return os.path.join(case_dir, "spaces", slug, "events.jsonl")


def resolve_op(argv):
    payload = None
    for arg in argv:
        if arg.lstrip()[:1] != "{":
            continue
        try:
            data = json.loads(arg)
        except ValueError:
            continue
        if isinstance(data, dict):
            payload = data
            break
    if payload is None:
        try:
            data = json.loads(sys.stdin.read())
        except Exception:
            data = None
        if isinstance(data, dict):
            payload = data
    verb = argv[0] if argv else ""
    if payload is None:
        return verb, None
    return payload.get("op") or verb, None


def append_event(row):
    path = space_events_path(root())
    os.makedirs(os.path.dirname(path), exist_ok=True)
    envelope = {"type": "review_coverage", "ts": row.get("ts", ""), "data": row}
    with open(path, "a") as fh:
        fh.write(json.dumps(envelope, sort_keys=True) + "\n")


def main():
    argv = sys.argv[1:]
    op, key = resolve_op(argv)
    journals = os.path.join(root(), "journals")
    os.makedirs(journals, exist_ok=True)
    counter = os.path.join(journals, "counters.json")
    counts = {}
    if os.path.exists(counter):
        with open(counter) as fh:
            try:
                counts = json.load(fh)
            except ValueError:
                counts = {}
    n = int(counts.get(op, 0)) + 1
    counts[op] = n
    with open(counter, "w") as fh:
        fh.write(json.dumps(counts, sort_keys=True))
    row = {"op": op, "key": key, "n": n}
    with open(os.path.join(journals, "fno-agents.jsonl"), "a") as fh:
        fh.write(json.dumps(row, sort_keys=True) + "\n")
    answers = os.path.join(root(), "answers")
    answer = None
    for name in ("op-%s-%d.json" % (op, n), "op-%s.json" % op):
        path = os.path.join(answers, name)
        if os.path.exists(path):
            with open(path) as fh:
                answer = json.load(fh)
            break
    if answer is None:
        sys.stderr.write("stub fno-agents: no answer for op %s (call %d)\n" % (op, n))
        sys.exit(3)
    if isinstance(answer.get("events_row"), dict):
        append_event(answer["events_row"])
    stdout = answer.get("stdout") or ""
    if stdout:
        sys.stdout.write(stdout)
    if "json" in answer:
        sys.stdout.write(json.dumps(answer["json"], sort_keys=True) + "\n")
    sys.stdout.flush()
    stderr = answer.get("stderr") or ""
    if stderr:
        sys.stderr.write(stderr)
    sys.stderr.flush()
    sys.exit(int(answer.get("exit") or 0))


main()
"""

STUB_GH = r"""#!/usr/bin/python3
# Stand-in for the gh CLI. Maps each argv shape to a named shape, journals
# it with the argv, then answers from answers/gh-<shape>-<n>.json falling
# back to answers/gh-<shape>.json. Shapes: view, view-jq (a `pr view` whose
# --jq filter real gh would evaluate server-side; the minted answer carries
# the jq result), pr-info (the exact real-slug pulls path), api-jq, api.
# Unmapped argv fails loud.
import json
import os
import re
import sys


def root():
    return os.environ.get("FNO_COVERAGE_CASE_DIR") or os.getcwd()


def shape_of(argv):
    if len(argv) >= 2 and argv[0] == "pr" and argv[1] == "view":
        # The native override valve reads the label with a server-side
        # filter (`pr view --json labels --jq ...`); real gh prints the jq
        # result, so that call takes its own shape and a minted "true"
        # answer, while the capture's plain labels reads keep view.
        if "--jq" in argv:
            return "view-jq"
        return "view"
    if argv and argv[0] == "api":
        # The native gate's PR facts read (`gh api repos/<slug>/pulls/<n>`)
        # gets its own shape and counter so it never consumes a per-call
        # `api` answer minted for the capture's other REST calls. The match
        # is the exact pulls path with a REAL slug: the floor's diff read
        # (repos/<slug>/pulls/<n>/files) shares the prefix and must keep the
        # plain api shape, and the override valve's author read rides gh's
        # `:owner/:repo` placeholders onto the same api counter as the
        # capture's call did.
        if any(
            a.startswith("repos/")
            and ":" not in a
            and re.fullmatch(r"repos/[^/]+/[^/]+/pulls/\d+", a)
            for a in argv[1:3]
        ):
            return "pr-info"
        if "--jq" in argv:
            return "api-jq"
        return "api"
    return None


def emit(answer):
    stdout = answer.get("stdout") or ""
    if stdout:
        sys.stdout.write(stdout)
    if "json" in answer:
        sys.stdout.write(json.dumps(answer["json"], sort_keys=True) + "\n")
    sys.stdout.flush()
    stderr = answer.get("stderr") or ""
    if stderr:
        sys.stderr.write(stderr)
    sys.stderr.flush()
    sys.exit(int(answer.get("exit") or 0))


def main():
    argv = sys.argv[1:]
    shape = shape_of(argv)
    journals = os.path.join(root(), "journals")
    os.makedirs(journals, exist_ok=True)
    with open(os.path.join(journals, "gh.jsonl"), "a") as fh:
        fh.write(
            json.dumps({"op": shape, "argv": argv[:10]}, sort_keys=True) + "\n"
        )
    if shape is None:
        sys.stderr.write("stub gh: unhandled argv: %s\n" % " ".join(argv))
        sys.exit(3)
    counter = os.path.join(journals, "gh-counters.json")
    counts = {}
    if os.path.exists(counter):
        with open(counter) as fh:
            try:
                counts = json.load(fh)
            except ValueError:
                counts = {}
    n = int(counts.get(shape, 0)) + 1
    counts[shape] = n
    with open(counter, "w") as fh:
        fh.write(json.dumps(counts, sort_keys=True))
    answers = os.path.join(root(), "answers")
    for name in ("%s-%d" % (shape, n), shape):
        path = os.path.join(answers, "gh-%s.json" % name)
        if os.path.exists(path):
            with open(path) as fh:
                emit(json.load(fh))
    sys.stderr.write("stub gh: no answer for shape %s (call %d)\n" % (shape, n))
    sys.exit(3)


main()
"""

STUB_FRONT = r"""#!/usr/bin/python3
# Stand-in for the fno front binary. Four doors:
#   `inbox law match`    payload on stdin; the payload's argv[0] is the law
#                        subject; answered from answers/front-law.json, a map
#                        keyed by subject with a "default" fallback.
#   `backlog decisions`  argv only; the native waiver probe's transport for
#                        the same law map (journals as inbox-law).
#   `inbox decide ...`   argv only; answered from answers/front-decide.json.
#   `do pr info <n>`     the head probe's no-slug fallback (a case world
#                        whose origin does not resolve to a slug); answered
#                        from answers/front-pr-info.json.
# Every call is journaled with its door and subject.
import json
import os
import sys


def root():
    return os.environ.get("FNO_COVERAGE_CASE_DIR") or os.getcwd()


def emit(answer):
    stdout = answer.get("stdout") or ""
    if stdout:
        sys.stdout.write(stdout)
    if "json" in answer:
        sys.stdout.write(json.dumps(answer["json"], sort_keys=True) + "\n")
    sys.stdout.flush()
    stderr = answer.get("stderr") or ""
    if stderr:
        sys.stderr.write(stderr)
    sys.stderr.flush()
    sys.exit(int(answer.get("exit") or 0))


def main():
    argv = sys.argv[1:]
    door = "-".join(argv[:2])
    journals = os.path.join(root(), "journals")
    os.makedirs(journals, exist_ok=True)
    answers_dir = os.path.join(root(), "answers")
    if door == "inbox-law":
        try:
            payload = json.loads(sys.stdin.read())
        except Exception:
            payload = {}
        argv_list = payload.get("argv") or []
        subject = argv_list[0] if argv_list else ""
        with open(os.path.join(journals, "front.jsonl"), "a") as fh:
            fh.write(json.dumps({"op": "inbox-law", "subject": subject}, sort_keys=True) + "\n")
        path = os.path.join(answers_dir, "front-law.json")
        if not os.path.exists(path):
            sys.stderr.write("stub fno: no answers/front-law.json\n")
            sys.exit(3)
        with open(path) as fh:
            mapping = json.load(fh)
        answer = mapping.get(subject, mapping.get("default"))
        if answer is None:
            sys.stderr.write("stub fno: no law answer for subject %s\n" % subject)
            sys.exit(3)
        emit(answer)
    if door == "inbox-decide":
        subject = argv[2] if len(argv) > 2 else ""
        with open(os.path.join(journals, "front.jsonl"), "a") as fh:
            fh.write(json.dumps({"op": "inbox-decide", "subject": subject}, sort_keys=True) + "\n")
        path = os.path.join(answers_dir, "front-decide.json")
        if not os.path.exists(path):
            sys.stderr.write("stub fno: no answer for inbox decide\n")
            sys.exit(3)
        with open(path) as fh:
            emit(json.load(fh))
    if door == "backlog-decisions":
        # The waiver probe's law read (`fno backlog decisions <subject>
        # --lane law --state live --json`). The capture drove the same probe
        # through `inbox law match`, so the call journals under the inbox-law
        # door with its subject and the fold vocabulary survives the
        # transport move.
        subject = argv[2] if len(argv) > 2 else ""
        with open(os.path.join(journals, "front.jsonl"), "a") as fh:
            fh.write(json.dumps({"op": "inbox-law", "subject": subject}, sort_keys=True) + "\n")
        path = os.path.join(answers_dir, "front-law.json")
        if not os.path.exists(path):
            sys.stderr.write("stub fno: no answers/front-law.json\n")
            sys.exit(3)
        with open(path) as fh:
            mapping = json.load(fh)
        answer = mapping.get(subject, mapping.get("default"))
        if answer is None:
            sys.stderr.write("stub fno: no law answer for subject %s\n" % subject)
            sys.exit(3)
        emit(answer)
    if door == "do-pr":
        # The head probe's no-slug fallback (`fno do pr info <n>`): the
        # primary door is the gh stub's pr-info shape once the case world's
        # origin resolves to a slug. The call journals under the agents fold
        # with the hop's original name so the fold vocabulary survives the
        # transport move.
        pr = argv[3] if len(argv) > 3 else ""
        with open(os.path.join(journals, "fno-agents.jsonl"), "a") as fh:
            fh.write(json.dumps({"op": "pr-head", "pr": pr}, sort_keys=True) + "\n")
        path = os.path.join(answers_dir, "front-pr-info.json")
        if not os.path.exists(path):
            sys.stderr.write("stub fno: no answers/front-pr-info.json\n")
            sys.exit(3)
        with open(path) as fh:
            # The file holds the bare facts payload, not an answer envelope:
            # emit() would write nothing for it (no "json"/"stdout" key) and
            # the probe would read an empty success. Wrap it here.
            emit({"json": json.load(fh)})
        return
    sys.stderr.write("stub fno: unhandled argv: %s\n" % " ".join(argv))
    sys.exit(3)


main()
"""

STUBS = {
    "fno-agents": STUB_AGENTS,
    "gh": STUB_GH,
    "fno": STUB_FRONT,
}


def slug(label):
    """Mirror of common::slug in tests/common/mod.rs."""
    out = []
    prev_us = False
    for ch in label:
        if ch.isascii() and ch.isalnum():
            out.append(ch.lower())
            prev_us = False
        elif not prev_us:
            out.append("_")
            prev_us = True
    return "".join(out).strip("_")


def load_cases():
    return json.loads((FIXTURE_DIR / "cases.json").read_text())


def write_stubs(stubbin):
    stubbin.mkdir(parents=True, exist_ok=True)
    for name, body in STUBS.items():
        path = stubbin / name
        path.write_text(body)
        os.chmod(str(path), 0o755)
    return stubbin


def git_env():
    env = dict(os.environ)
    env.update(
        {
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_SYSTEM": "/dev/null",
            "GIT_AUTHOR_DATE": "2026-10-13T00:00:00Z",
            "GIT_COMMITTER_DATE": "2026-10-13T00:00:00Z",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    return env


def git_run(argv, cwd):
    proc = subprocess.run(
        ["git", *argv],
        cwd=str(cwd),
        env=git_env(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if proc.returncode != 0:
        raise SystemExit("git %s failed: %s" % (" ".join(argv), proc.stderr.strip()))
    return proc.stdout.strip()


def build_git(case_dir):
    git_run(["init", "-q", "-b", "main"], case_dir)
    git_run(["config", "user.email", "fixture@example.com"], case_dir)
    git_run(["config", "user.name", "Fixture"], case_dir)
    git_run(["commit", "--allow-empty", "-q", "-m", "seed"], case_dir)
    # The diff-sized hint branches (stale head, reviewer refused, the
    # code-review no-pass line) size from merge-base(origin/main, HEAD).
    # Plant the ref at the seed and commit a medium-tier diff on top (6
    # files x 50 lines: over low's 3/150 cap, under medium's 10/600), so
    # the level resolves inside every case world instead of degrading to
    # the levelless line.
    git_run(["update-ref", "refs/remotes/origin/main", "HEAD"], case_dir)
    diff_dir = case_dir / "sized"
    diff_dir.mkdir()
    for n in range(6):
        (diff_dir / ("sized-%d.txt" % n)).write_text(
            "".join("line %d\n" % i for i in range(50))
        )
    git_run(["add", "sized"], case_dir)
    git_run(["commit", "-q", "-m", "sized diff"], case_dir)
    return git_run(["rev-parse", "HEAD"], case_dir)


def build_origin(case_dir):
    # The public URL is left UNREWRITTEN: the slug readers run
    # `git remote get-url origin` literally, and an insteadOf rewrite would
    # hand them the bare-clone path. Nothing in these legs fetches, so the
    # URL is configuration, never a network endpoint.
    git_run(
        ["remote", "add", "origin", "https://github.com/acme/widget.git"], case_dir
    )


def substitute_placeholders(case_dir, head_sha):
    markers = (b"__CASE_DIR__", b"__HEAD_SHA__")
    for dirpath, dirnames, filenames in os.walk(str(case_dir)):
        dirnames[:] = [d for d in dirnames if d not in (".git", "origin.git")]
        for name in filenames:
            path = Path(dirpath) / name
            data = path.read_bytes()
            if not any(marker in data for marker in markers):
                continue
            try:
                text = data.decode("utf-8")
            except UnicodeDecodeError:
                continue
            text = text.replace("__CASE_DIR__", str(case_dir))
            if head_sha:
                text = text.replace("__HEAD_SHA__", head_sha)
            path.write_text(text)


def space_events_path(case_dir):
    """The journal the gate actually reads: paths.project_log for the case
    root, with FNO_STATE_DIR pinned to the case dir. space_slug is the
    CANONICAL path with / swapped for -; resolve() matches the resolver, so
    the planted file is the file the leg reads on symlinked tmpdirs."""
    resolved = Path(case_dir).resolve()
    slug = str(resolved).replace("/", "-")
    return resolved / "spaces" / slug / "events.jsonl"


def write_events_row(path, ev_type, ts, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    envelope = {"type": ev_type, "ts": ts, "data": data}
    with open(path, "a") as fh:
        fh.write(json.dumps(envelope, sort_keys=True) + "\n")


def build_case(root_path, case, index):
    case_dir = root_path / ("case-%02d" % index)
    case_dir.mkdir(parents=True)
    # state_dir is a TOP-LEVEL key: under a [table] header the config loader
    # reads it as a table field and warns it is unmodeled.
    (case_dir / "config.toml").write_text('state_dir = "%s"\n' % case_dir)
    fno_dir = case_dir / ".fno"
    fno_dir.mkdir()
    review = case.get("review") or {}
    if review:
        lines = ["[review]"]
        for key in sorted(review):
            lines.append("%s = %s" % (key, json.dumps(review[key])))
        (fno_dir / "config.toml").write_text("\n".join(lines) + "\n")
    answers_dir = case_dir / "answers"
    answers_dir.mkdir()
    for key, answer in dict(DEFAULT_ANSWERS, **(case.get("answers") or {})).items():
        (answers_dir / ("op-%s.json" % key)).write_text(
            json.dumps(answer, sort_keys=True) + "\n"
        )
    gh_map = dict(DEFAULT_GH_ANSWERS)
    gh_map.update(case.get("gh") or {})
    # The native probe's pulls read (`gh api repos/<slug>/pulls/<n>`) is
    # answered pulls-shaped from the same head the capture's pr-hop answer
    # froze: answers.pr-head is the single source of truth for the head, so
    # the cases whose goldens read a failed fetch keep no answer here and
    # the stub's missing-answer exit 3 IS the fetch failure.
    pr_head = dict(DEFAULT_ANSWERS, **(case.get("answers") or {})).get("pr-head") or {}
    head = (pr_head.get("json") or {}).get("head")
    if head:
        gh_map.setdefault("pr-info", {
            "json": {
                "number": case.get("pr", 7),
                "head": {"sha": head, "ref": "feature/x"},
                "base": {"ref": "main"},
                "html_url": "https://github.com/acme/widget/pull/7",
                "state": "open",
                "auto_merge": None,
            }
        })
    for shape, answer in gh_map.items():
        (answers_dir / ("gh-%s.json" % shape)).write_text(
            json.dumps(answer, sort_keys=True) + "\n"
        )
    front = dict(DEFAULT_FRONT_ANSWERS)
    front_law = dict(front["law"])
    front_law.update((case.get("front") or {}).get("law") or {})
    (answers_dir / "front-law.json").write_text(
        json.dumps(front_law, sort_keys=True) + "\n"
    )
    front["decide"] = (case.get("front") or {}).get("decide") or front["decide"]
    (answers_dir / "front-decide.json").write_text(
        json.dumps(front["decide"], sort_keys=True) + "\n"
    )
    # The native gate's PR facts read (`fno do pr info <n>`) lands on the
    # front door. Served from the pr-head facts payload, per-case overridable
    # through front.pr_info; the __HEAD_SHA__ placeholder keeps it aligned
    # with the seeded commit the planted rows name.
    front["pr_info"] = ((case.get("front") or {}).get("pr_info")) or {
        "pr": case.get("pr", 7),
        "head_sha": "__HEAD_SHA__",
        "head_ref": "feature/x",
        "base_ref": "main",
    }
    (answers_dir / "front-pr-info.json").write_text(
        json.dumps(front["pr_info"], sort_keys=True) + "\n"
    )
    (case_dir / "journals").mkdir()
    (case_dir / "home" / "agents-home").mkdir(parents=True)
    (case_dir / "claims").mkdir()
    events = case.get("events") or []
    events_path = space_events_path(case_dir)
    for row in events:
        write_events_row(events_path, row["type"], row["ts"], row["data"])
    head_sha = None
    if case.get("git", True):
        head_sha = build_git(case_dir)
    if case.get("origin"):
        build_origin(case_dir)
    (case_dir / "case.json").write_text(
        json.dumps(case, sort_keys=True, indent=2) + "\n"
    )
    substitute_placeholders(case_dir, head_sha)
    return case_dir


def run_check(case):
    from fno.pr import _coverage_gate

    cwd = case.get("cwd")
    return _coverage_gate.run_coverage_check(
        int(case["pr"]), recompute=bool(case.get("recompute")), cwd=cwd
    )


def run_verdict(case):
    from fno.pr import _coverage_gate

    cwd = case.get("cwd") or os.getcwd()
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        int(case["pr"]), cwd, recompute=bool(case.get("recompute"))
    )
    payload = {
        "state": state,
        "refusal": refusal,
        "covered_head": covered_head,
        "note": note,
        "line": _coverage_gate.refusal_line(refusal, note),
    }
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    return 0


def run_publish(case):
    from fno.pr import _reviews

    posted, note = _reviews.publish_coverage_status(
        int(case["pr"]), case.get("head"), cwd=os.getcwd()
    )
    sys.stdout.write(
        json.dumps(
            {"pr": int(case["pr"]), "posted": posted, "note": note},
            separators=(",", ":"),
        )
        + "\n"
    )
    return 0 if posted else 1


def run_waive(case):
    from fno.pr import _coverage_gate

    return _coverage_gate.run_coverage_waive(
        int(case["pr"]), case.get("reason") or "", cwd=os.getcwd()
    )


def run_chain(case):
    from fno.pr import _coverage_gate

    cwd = os.getcwd()
    chain = _coverage_gate.attestation_chain(
        cwd, head_branch=case.get("branch") or "", head=case.get("head") or ""
    )
    last = None
    if chain:
        row = chain[-1]
        blocking = sum(
            1
            for f in (row.get("findings") or [])
            if isinstance(f, dict) and f.get("blocking") is True
        )
        last = {
            "head_sha": row.get("head_sha", ""),
            "reviewed_base_sha": row.get("reviewed_base_sha", ""),
            "blocking": blocking,
            "findings_truncated": row.get("findings_truncated") is True,
        }
    payload = {
        "rounds_used": _coverage_gate.rounds_since_last_pass(chain),
        "max_rounds": _coverage_gate.resolved_max_rounds(cwd),
        "last": last,
    }
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    return 0


def run_entry(case):
    entry = case.get("entry")
    if entry == "check":
        return run_check(case)
    if entry == "verdict":
        return run_verdict(case)
    if entry == "publish":
        return run_publish(case)
    if entry == "waive":
        return run_waive(case)
    if entry == "chain":
        return run_chain(case)
    raise SystemExit("unknown entry: %r" % entry)


def cmd_case(case_dir):
    case_dir = Path(case_dir).resolve()
    case = json.loads((case_dir / "case.json").read_text())
    stubbin = case_dir.parent / "stub-bin"
    env = dict(os.environ)
    for var in STRIP_VARS:
        env.pop(var, None)
    env["FNO_CONFIG"] = str(case_dir / "config.toml")
    env["FNO_STATE_DIR"] = str(case_dir)
    env["FNO_AGENTS_HOME"] = str(case_dir / "home" / "agents-home")
    env["HOME"] = str(case_dir / "home")
    env["FNO_CLAIMS_ROOT"] = str(case_dir / "claims")
    env["FNO_COVERAGE_CASE_DIR"] = str(case_dir)
    env["FNO_AGENTS_BIN"] = str(stubbin / "fno-agents")
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_CONFIG_GLOBAL"] = "/dev/null"
    env["GIT_CONFIG_SYSTEM"] = "/dev/null"
    if case.get("path") == "empty":
        env["PATH"] = ""
    elif case.get("path") == "gitbin":
        env["PATH"] = str(case_dir / "gitbin")
    else:
        env["PATH"] = "%s:/usr/bin:/bin" % stubbin
    env.update(case.get("env") or {})
    os.environ.clear()
    os.environ.update(env)
    os.chdir(str(case_dir))

    # The front-binary seam must answer from the stub on every machine: the
    # real resolver probes crates/fno/target before PATH, and a workspace that
    # built the fno crate would put a live binary under the law reads.
    import fno.rust_binary as rust_binary

    rust_binary.resolve_front_binary = lambda: stubbin / "fno"

    # The POST retry pause is a wall-clock courtesy, not behavior: zero it so
    # the failing-POST cases freeze without sleeping ten seconds.
    from fno.pr import _reviews

    _reviews._POST_RETRY_SLEEP_SECS = 0.0

    saved_out, saved_err = os.dup(1), os.dup(2)
    out_fd = os.open(
        str(case_dir / "leg-out.txt"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600
    )
    err_fd = os.open(
        str(case_dir / "leg-err.txt"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600
    )
    sys.stdout.flush()
    sys.stderr.flush()
    os.dup2(out_fd, 1)
    os.dup2(err_fd, 2)
    os.close(out_fd)
    os.close(err_fd)
    try:
        rc = run_entry(case)
    except SystemExit as exc:
        # A leg verdict may arrive as SystemExit; the envelope must still
        # print or the parent reads a missing envelope as the failure.
        rc = 0 if exc.code is None else exc.code
        if not isinstance(rc, int):
            rc = 1
    sys.stdout.flush()
    sys.stderr.flush()
    os.dup2(saved_out, 1)
    os.dup2(saved_err, 2)
    os.close(saved_out)
    os.close(saved_err)
    envelope = {
        "exit": rc,
        "stdout": (case_dir / "leg-out.txt").read_text(),
        "stderr": (case_dir / "leg-err.txt").read_text(),
    }
    sys.stdout.write(json.dumps(envelope) + "\n")
    sys.stdout.flush()


def collapse(rows):
    """First-appearance order; a consecutive run longer than one becomes
    `<op>*` so a retry loop freezes no attempt count the native leg need not
    reproduce."""
    out = []
    for row in rows:
        if out and out[-1][0] == row:
            out[-1][1] += 1
        else:
            out.append([row, 1])
    return [op if n == 1 else "%s*" % op for op, n in out]


def fold_calls(case_dir):
    def read_rows(name):
        path = case_dir / "journals" / name
        if not path.exists():
            return []
        rows = []
        for line in path.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except ValueError:
                rows.append({"op": "<unparsed>"})
        return rows

    agents = read_rows("fno-agents.jsonl")
    return {
        "agents": collapse([row.get("op") for row in agents]),
        "gh": collapse([row.get("op") for row in read_rows("gh.jsonl")]),
        "front": collapse([row.get("op") for row in read_rows("front.jsonl")]),
        "law_subjects": collapse(
            [row.get("subject") for row in read_rows("front.jsonl")]
        ),
        "events": collapse(
            [row.get("type") for row in read_rows("events.jsonl")]
        ),
    }


def mask_text(text, case_dir):
    # The resolved spelling first: a sentence carrying the realpath
    # (/private/var/... on macOS) contains the unresolved spelling as a
    # substring, and replacing the shorter one first would strand the
    # /private prefix as machine-specific golden bytes. The dashed spellings
    # cover paths.space_slug, which rewrites the separator in the refusal's
    # searched-sources list; the tempdir name leaks through it otherwise.
    for base in (os.path.realpath(str(case_dir)), str(case_dir)):
        text = text.replace(base, "<CASE>")
        text = text.replace(base.replace("/", "-"), "<CASE>")
    text = re.sub(r"\(pid=[^)]*\)", "(pid=<ERR>)", text)
    return text


def run_child(case_dir):
    proc = subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), "--case", str(case_dir)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        stdin=subprocess.DEVNULL,
        env={**os.environ, "PYTHONPATH": str(REPO_ROOT / "cli" / "src")},
        timeout=420,
    )
    if proc.returncode != 0:
        leg_err = case_dir / "leg-err.txt"
        raise SystemExit(
            "child failed for %s (rc %s)\nchild stderr:\n%s\nleg-err:\n%s"
            % (
                case_dir.name,
                proc.returncode,
                proc.stderr.decode(errors="replace"),
                leg_err.read_text() if leg_err.exists() else "<no leg-err.txt>",
            )
        )
    out_lines = [
        line for line in proc.stdout.decode().splitlines() if line.strip()
    ]
    if not out_lines:
        raise SystemExit("child printed no envelope for %s" % case_dir.name)
    return json.loads(out_lines[-1])


def capture(only):
    root_path = Path(tempfile.mkdtemp(prefix="coverage-gate-"))
    print("fixture root: %s" % root_path, flush=True)
    write_stubs(root_path / "stub-bin")
    cases = load_cases()
    frozen = 0
    for index, case in enumerate(cases):
        key = slug(case["label"])
        if only and key != only:
            continue
        case_dir = build_case(root_path, case, index)
        for stale in (case_dir / "journals").glob("*.jsonl"):
            stale.unlink()
        envelope = run_child(case_dir)
        calls = mask_text(json.dumps(fold_calls(case_dir), sort_keys=True), case_dir)
        out = mask_text(envelope.get("stdout", ""), case_dir)
        err = mask_text(envelope.get("stderr", ""), case_dir)
        GOLDEN_DIR.mkdir(parents=True, exist_ok=True)
        (GOLDEN_DIR / (key + ".exit")).write_text("%d\n" % envelope["exit"])
        (GOLDEN_DIR / (key + ".out")).write_text(out)
        if err:
            (GOLDEN_DIR / (key + ".err")).write_text(err)
        if case.get("freeze_calls", True):
            (GOLDEN_DIR / (key + ".calls")).write_text(calls + "\n")
        frozen += 1
        print(
            "froze %s -> %s (exit %s)" % (case["label"], key, envelope["exit"]),
            flush=True,
        )
    if only and frozen == 0:
        raise SystemExit("--only %s matched no case" % only)


def build_only(root_arg):
    root_path = Path(root_arg).resolve()
    root_path.mkdir(parents=True, exist_ok=True)
    stubbin = write_stubs(root_path / "stub-bin")
    listing = []
    for index, case in enumerate(load_cases()):
        case_dir = build_case(root_path, case, index)
        listing.append(
            {"label": case["label"], "slug": slug(case["label"]), "dir": str(case_dir)}
        )
    print(
        json.dumps(
            {"root": str(root_path), "stubbin": str(stubbin), "cases": listing},
            indent=2,
        )
    )


def main():
    parser = argparse.ArgumentParser(
        description="capture the coverage-gate golden corpus from the live Python legs"
    )
    parser.add_argument("--case", help="child mode: run one built case dir")
    parser.add_argument(
        "--build-only",
        metavar="ROOT",
        help="build the fixture tree under ROOT and print a JSON map",
    )
    parser.add_argument("--only", metavar="SLUG", help="capture one case by golden key")
    args = parser.parse_args()
    if args.case:
        cmd_case(args.case)
    elif args.build_only:
        build_only(args.build_only)
    else:
        capture(args.only)


if __name__ == "__main__":
    main()
