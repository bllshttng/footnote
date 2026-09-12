"""The advisory five-question blueprint judge (x-9983).

One isolated model call per question (:data:`JUDGE_DIMENSIONS`), sequential -
the usage window allows one lane. Every failure path (timeout, nonzero exit,
unparseable reply, an explicit ``unknown``) returns a ``None`` verdict: a
coverage gap, never a fabricated fail. Pass criteria live in
``evals/blueprint-judge/lenses.md`` and nowhere else, so plans cannot learn
to write to the judge.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path
from typing import Callable, Optional

from fno.observer.fold import JUDGE_DIMENSIONS

# ponytail: tier is unproven until the --labels calibration run; the rates
# pick the survivor, not this constant.
JUDGE_MODEL = "sonnet"

_EVIDENCE_MAX = 500
_CONTEXT_MAX = 4000
_VERDICT_RE = re.compile(r"^VERDICT:\s*(pass|fail|unknown)\s*$", re.MULTILINE | re.IGNORECASE)

Spawn = Callable[..., "tuple[int, str, str]"]

_SHARED_INSTRUCTION = (
    "You are a reader with no stake in this plan. You grade one question only. "
    "First quote the plan lines you rely on. Then give a reason in one or two "
    "sentences. End with one line: VERDICT: pass, fail or unknown. Do not grade "
    "format, headings, length or style. If the plan makes the question moot, "
    "pass and say why. An answer of none is a claim; judge it like any other."
)


def _lenses_path() -> Path:
    try:
        from fno.paths import resolve_repo_root

        return resolve_repo_root() / "evals" / "blueprint-judge" / "lenses.md"
    except Exception:
        return Path("evals/blueprint-judge/lenses.md")


def load_lenses(path: Optional[Path] = None) -> dict[str, str]:
    """Parse lenses.md into ``{dimension: section body}``.

    The shared instruction (prose before the first ``## ``) is not included;
    ``judge_plan`` adds it separately. Unreadable file or a section for an
    unknown dimension is tolerated: the prompt simply loses that section and
    the judge answers from the shared instruction alone.
    """
    try:
        text = (path or _lenses_path()).read_text(encoding="utf-8")
    except OSError:
        return {}
    sections: dict[str, str] = {}
    dimension: Optional[str] = None
    body: list[str] = []
    for line in text.splitlines() + ["## __end__"]:
        m = re.match(r"^## (\w+)\s*$", line)
        if m:
            if dimension is not None:
                sections[dimension] = "\n".join(body).strip()
            dimension, body = m.group(1), []
        elif dimension is not None:
            body.append(line)
    return {d: s for d, s in sections.items() if d in JUDGE_DIMENSIONS}


def _backticked_symbols(plan_text: str) -> list[str]:
    seen: list[str] = []
    for raw in re.findall(r"`([^`\n]+)`", plan_text):
        sym = raw.strip()
        # Paths and prose-length spans are context, not symbols; keep the
        # dotted/lowercase code shapes a symbol search can actually hit.
        if 3 < len(sym) <= 60 and re.fullmatch(r"[\w./-]+", sym) and "." not in sym[:1]:
            if sym not in seen:
                seen.append(sym)
    return seen[:12]


def _gather_context(dimension: str, plan_text: str) -> str:
    """Code-gathered context, best-effort: ``''`` on any fault. Only
    surface_fit and duplication get context; the other three lenses judge
    prose alone."""
    if dimension not in ("surface_fit", "duplication"):
        return ""
    chunks: list[str] = []
    if dimension == "surface_fit":
        try:
            out = subprocess.run(
                ["fno", "help", "--all"], capture_output=True, text=True, timeout=30
            )
            if out.returncode == 0 and out.stdout.strip():
                chunks.append("$ fno help --all\n" + out.stdout[:_CONTEXT_MAX])
        except (OSError, subprocess.TimeoutExpired):
            pass
        return "\n\n".join(chunks)
    symbols = _backticked_symbols(plan_text)
    for sym in symbols:
        try:
            out = subprocess.run(
                ["rg", "-l", "-F", sym, "--glob", "!.claude/**", "--glob", "!graphify-out/**", "."],
                capture_output=True, text=True, timeout=20,
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        if out.returncode == 0 and out.stdout.strip():
            hits = [ln for ln in out.stdout.splitlines() if ln.strip()][:8]
            chunks.append(f"$ rg -l -F {sym}\n" + "\n".join(hits))
    try:
        from fno.paths import resolve_repo_root

        inventory = resolve_repo_root() / "docs" / "architecture" / "dual-implementation-inventory.md"
        lines = inventory.read_text(encoding="utf-8").splitlines()
        rows = [ln for ln in lines if any(sym in ln for sym in symbols)]
        if rows:
            chunks.append("## dual-implementation-inventory rows\n" + "\n".join(rows[:20]))
    except Exception:
        pass
    return "\n\n".join(chunks)[:_CONTEXT_MAX]


def _build_prompt(
    dimension: str, plan_text: str, node_text: str, lenses: dict[str, str], context: str
) -> str:
    parts = [
        _SHARED_INSTRUCTION,
        f"## The node\n{node_text.strip() or '(no node text supplied)'}",
        f"## The plan\n{plan_text}",
    ]
    if context:
        parts.append(f"## Context from code\n{context}")
    section = lenses.get(dimension, "").strip()
    if section:
        parts.append(f"## Your question: {dimension}\n{section}")
    return "\n\n".join(parts) + "\n"


def parse_verdict(text: str) -> tuple[Optional[str], str]:
    """The LAST ``VERDICT:`` line wins. Returns ``(verdict, reason)`` where
    verdict is ``pass``/``fail`` or ``None`` (unknown or unparseable - a
    coverage gap either way) and reason is the reply body cut to
    ``_EVIDENCE_MAX``."""
    text = text or ""
    matches = list(_VERDICT_RE.finditer(text))
    if not matches:
        return None, text.strip()[:_EVIDENCE_MAX]
    verdict = matches[-1].group(1).lower()
    reason = text[: matches[-1].start()].strip()
    return (None if verdict == "unknown" else verdict), reason[:_EVIDENCE_MAX]


def judge_plan(
    plan_text: str,
    node_text: str,
    dimension: str,
    *,
    spawn: Spawn,
    lenses: Optional[dict[str, str]] = None,
) -> tuple[Optional[str], str]:
    """Grade one dimension of one plan with one isolated model call.

    ``spawn`` takes ``(name, prompt)`` and returns ``(rc, stdout, stderr)``;
    the caller binds cwd/timeout/model (the verb partials ``_default_spawn``).
    Returns ``(verdict_or_None, evidence)``; ``None`` is a coverage gap, a
    judge error is never a fail.
    """
    if dimension not in JUDGE_DIMENSIONS:
        return None, f"unknown judge dimension {dimension!r}"
    if lenses is None:
        lenses = load_lenses()
    prompt = _build_prompt(dimension, plan_text, node_text, lenses, _gather_context(dimension, plan_text))
    try:
        rc, out, err = spawn(f"blueprint-judge-{dimension}", prompt)
    except Exception as exc:  # any spawn fault -> coverage gap
        return None, f"judge spawn fault: {exc}"[:_EVIDENCE_MAX]
    if rc != 0:
        return None, f"judge spawn rc={rc}: {(err or out).strip()[:200]}".strip()
    return parse_verdict(out)


def tally(
    rows: list[dict],
    *,
    plan_text: "object",
    spawn: Spawn,
    repeat: int = 1,
) -> dict:
    """Calibration tally over labeled rows (AC5-*).

    ``plan_text`` maps a row's ``plan`` path to its text (dict) or is a
    callable of the same shape. Each labeled (row, dimension) pair is judged
    ``repeat`` times (first verdict counts; a split counts as a flip). A
    ``None`` judge verdict is a coverage gap in the wild but a WRONG verdict
    on a control: a control must be gradeable.

    Returns ``{dimensions: {dim: {n, tp_rate, tn_rate}}, disagreements: [...],
    controls_wrong: int, flips: [...]}``.
    """
    lookup = plan_text.get if hasattr(plan_text, "get") else plan_text
    dims_out: dict[str, dict] = {}
    disagreements: list[dict] = []
    flips: list[dict] = []
    controls_wrong = 0
    for row in rows:
        text = lookup(row["plan"])
        node = row.get("node_text") or ""
        for dimension, label in (row.get("labels") or {}).items():
            verdicts: list[Optional[str]] = []
            reason = ""
            for _ in range(max(1, repeat)):
                verdict, reason = judge_plan(text, node, dimension, spawn=spawn)
                verdicts.append(verdict)
            verdict = verdicts[0]
            if len(set(verdicts)) > 1:
                flips.append({"plan": row["plan"], "dimension": dimension, "verdicts": verdicts})
            stats = dims_out.setdefault(dimension, {"n": 0, "tp": 0, "tn": 0})
            stats["n"] += 1
            if verdict == "fail" and label == "fail":
                stats["tp"] += 1
            elif verdict == "pass" and label == "pass":
                stats["tn"] += 1
            if verdict != label:
                disagreements.append(
                    {
                        "plan": row["plan"],
                        "dimension": dimension,
                        "label": label,
                        "judge": verdict,
                        "reason": reason,
                    }
                )
                if row.get("control"):
                    controls_wrong += 1
    return {
        "dimensions": {
            d: {
                "n": s["n"],
                "tp_rate": round(s["tp"] / s["n"], 3) if s["n"] else None,
                "tn_rate": round(s["tn"] / s["n"], 3) if s["n"] else None,
            }
            for d, s in dims_out.items()
        },
        "disagreements": disagreements,
        "controls_wrong": controls_wrong,
        "flips": flips,
    }
