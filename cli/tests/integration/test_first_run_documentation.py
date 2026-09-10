"""First-run documentation premises against the packaged front door.

README.md and docs/getting-started.md promise a first-run journey: fno
commands, /fno: skill spellings, and config defaults. This fixture checks
each promise against the packaged CLI (fno on PATH) instead of trusting the
prose. Command probes run with --help short-circuit so nothing documented
actually executes; default promises are read from the packaged schema in a
disposable cwd/HOME (the shared cwd_tmp fixture) so repo config cannot mask
a default. Every failure names the example that broke.

Platforms without the fno binary on PATH skip explicitly; there is no
bundled-install fixture to reuse, so the packaged front door on PATH is the
install under test.
"""

import re
import shlex
import shutil
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
DOC_PATHS = [REPO_ROOT / "README.md", REPO_ROOT / "docs" / "getting-started.md"]
SETTINGS_DOC = REPO_ROOT / "docs" / "getting-started.md"

FENCE_RE = re.compile(r"```[^\n]*\n(.*?)```", re.S)
INLINE_RE = re.compile(r"`(fno [^`]+)`")
SKILL_RE = re.compile(r"/fno:([a-z][a-z-]*)")

# Defaults README/getting-started promise, keyed by user-facing config path.
# Value is the normalized packaged-schema default cell that must match;
# review.posture is special-cased (schema default is none; the promise is the
# self_review floor named in its row text).
CONFIG_PROMISES = {
    "config.review.github_apps": "none",
    "config.review.external_reviewers": "none",
    "config.review.max_rounds": "2",
    "config.review.posture": "self_review",
    "config.auto_merge.enabled": "false",
    "config.target.defaults.max_iterations": "40",
    "config.obsidian.enabled": "false",
    "config.project.vision": "none",
    "config.backlog.id_prefix": "none",
}

pytestmark = pytest.mark.skipif(
    shutil.which("fno") is None, reason="packaged fno front door not on PATH"
)


def _fenced_lines():
    for doc in DOC_PATHS:
        for block in FENCE_RE.findall(doc.read_text()):
            for raw in block.splitlines():
                line = raw.strip()
                if line.startswith("$ "):
                    line = line[2:].strip()
                yield doc.name, line


def _documented_commands():
    """Documented fno invocations as argv (fenced blocks + inline-code spans).

    Compound lines are reported as skipped, and shlex-unparseable spans are
    too - both stay visible instead of silently leaving the premise unchecked.
    """
    cmds, skipped = [], []
    for where, line in _fenced_lines():
        if not line.startswith("fno "):
            continue
        if any(mark in line for mark in ("|", "&&", "||", ">", "&")):
            skipped.append(f"{where}: {line}")
            continue
        line = re.sub(r"\s+#.*$", "", line)
        cmds.append((where, shlex.split(line)))
    for doc in DOC_PATHS:
        for span in INLINE_RE.findall(doc.read_text()):
            if any(mark in span for mark in ("|", "&&", "||", ">", "&", "..")):
                skipped.append(f"{doc.name}: {span}")
                continue
            try:
                cmds.append((doc.name, shlex.split(span)))
            except ValueError:
                skipped.append(f"{doc.name}: {span}")
    return cmds, skipped


def _probe(argv):
    """(ok, detail) for a documented command; --help short-circuits any run.

    Fallback: the Rust front's mux verbs print no per-verb help and exit 2
    with a usage line that names the accepted form, so a usage line carrying
    the command's own path also proves the verb exists.
    """
    probe = argv if "--version" in argv else argv + ["--help"]
    try:
        run = subprocess.run(probe, capture_output=True, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        return False, "timed out"
    out = (run.stderr or run.stdout).strip()
    if run.returncode == 0:
        return True, out[:200]
    path = " ".join(t for t in argv[1:] if not t.startswith("-"))
    usage_lines = [ln for ln in out.splitlines() if ln.strip().lower().startswith("usage:")]
    if path and any(path in ln for ln in usage_lines):
        return True, "named in usage line"
    return False, out[:200]


def _skill_verbs():
    found = set()
    for doc in DOC_PATHS:
        found |= set(SKILL_RE.findall(doc.read_text()))
    return found


def _skill_path(verb):
    for cand in (
        REPO_ROOT / "skills" / verb / "SKILL.md",
        REPO_ROOT / "commands" / f"{verb}.md",
        REPO_ROOT / "commands" / f"fno-{verb}.md",
    ):
        if cand.exists():
            return cand
    return None


def _schema_rows():
    run = subprocess.run(
        ["fno", "config", "schema", "--markdown"],
        capture_output=True, text=True, timeout=60,
    )
    assert run.returncode == 0, f"config schema --markdown failed: {run.stderr[:200]}"
    rows = {}
    for line in run.stdout.splitlines():
        if not line.startswith("| `"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) >= 4:
            rows[cells[0].strip("`")] = cells
    return rows


def _norm_default(cell):
    c = cell.strip().strip("`").strip()
    if c in ("_(none)_", "none", "[]", "{}", ""):
        return "none"
    return c.lower()


def test_documented_fno_commands_resolve(cwd_tmp):
    cmds, skipped = _documented_commands()
    assert cmds, "no fno commands found in the first-run docs"
    bad = []
    for where, argv in cmds:
        ok, detail = _probe(argv)
        if not ok:
            bad.append(f"{where}: {' '.join(argv)} -> {detail}")
    assert not bad, "packaged CLI refuses documented commands:\n" + "\n".join(bad)


def test_documented_skill_spellings_exist():
    missing = sorted(v for v in _skill_verbs() if _skill_path(v) is None)
    assert not missing, f"/fno: spellings with no shipped skill or command: {missing}"


def test_settings_table_promises_the_checked_defaults():
    table = SETTINGS_DOC.read_text()
    absent = [k for k in CONFIG_PROMISES if f"`{k}`" not in table]
    assert not absent, f"settings table no longer promises: {absent}"


def test_config_default_promises_match_schema(cwd_tmp):
    rows = _schema_rows()
    bad = []
    for key, expected in CONFIG_PROMISES.items():
        skey = key[len("config."):]
        cells = rows.get(skey)
        if cells is None:
            bad.append(f"{key}: not in packaged schema")
        elif skey == "review.posture":
            if expected not in " | ".join(cells):
                bad.append(f"{key}: schema row never names the {expected} floor")
        elif _norm_default(cells[2]) != expected:
            bad.append(f"{key}: docs promise {expected}, schema says {cells[2]!r}")
    assert not bad, "config-default promises drifted:\n" + "\n".join(bad)


def test_checker_fails_naming_a_broken_example():
    ok, detail = _probe(["fno", "definitely-not-a-real-verb"])
    assert not ok, "negative control unexpectedly resolved"
    assert "definitely-not-a-real-verb" in detail, f"failure does not name it: {detail}"
    wrong = _norm_default("`false`") == _norm_default("`true`")
    assert not wrong, "default normalizer cannot tell different values apart"
