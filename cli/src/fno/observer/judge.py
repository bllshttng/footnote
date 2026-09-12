"""The advisory five-question blueprint judge (x-9983).

One isolated model call per question (fold.JUDGE_DIMENSIONS); every fault
path (timeout, nonzero exit, unparseable reply, ``unknown``) returns None,
a coverage gap, never a fabricated fail. Pass criteria live only in
``evals/blueprint-judge/lenses.md`` so plans cannot learn to write to the
judge.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path
from typing import Callable, Optional

from fno.observer.fold import JUDGE_DIMENSIONS

# ponytail: tier unproven until the --labels calibration run; the rates pick the survivor.
JUDGE_MODEL = "sonnet"

_VERDICT_RE = re.compile(r"^VERDICT:\s*(pass|fail|unknown)\s*$", re.MULTILINE | re.IGNORECASE)
Spawn = Callable[..., "tuple[int, str, str]"]


def _lenses_path() -> Path:
    try:
        from fno.paths import resolve_repo_root

        return resolve_repo_root() / "evals/blueprint-judge/lenses.md"
    except Exception:
        return Path("evals/blueprint-judge/lenses.md")


def load_lenses(path: Optional[Path] = None) -> tuple[str, dict[str, str]]:
    """(shared instruction, {dimension: section}); unreadable file -> ("", {})."""
    try:
        text = (path or _lenses_path()).read_text(encoding="utf-8")
    except OSError:
        return "", {}
    m = re.search(r"(?m)^## ", text)
    sections: dict[str, str] = {}
    for chunk in re.split(r"(?m)^## ", text[m.end() :] if m else ""):
        name, _, body = chunk.partition("\n")
        if name.strip() in JUDGE_DIMENSIONS:
            sections[name.strip()] = body.strip()
    return (text[: m.start()].strip() if m else ""), sections


def _gather_context(dimension: str, plan_text: str) -> str:
    """Code context for surface_fit/duplication; '' for other lenses, '' on fault."""
    if dimension not in ("surface_fit", "duplication"):
        return ""
    try:
        if dimension == "surface_fit":
            r = subprocess.run(["fno", "help", "--all"], capture_output=True, text=True, timeout=30)
            return r.stdout[:4000] if r.returncode == 0 else ""
        syms = list(dict.fromkeys(re.findall(r"`([\w./-]{4,60})`", plan_text)))[:12]
        chunks = []
        for sym in syms:
            h = subprocess.run(
                ["rg", "-l", "-F", sym, "--glob", "!.claude/**", "--glob", "!graphify-out/**", "."],
                capture_output=True, text=True, timeout=20,
            )
            if h.returncode == 0:
                chunks.append(f"{sym}: {', '.join(h.stdout.split()[:8])}")
        try:
            from fno.paths import resolve_repo_root

            inv = (resolve_repo_root() / "docs/architecture/dual-implementation-inventory.md").read_text("utf-8")
            rows = [ln for ln in inv.splitlines() if any(s in ln for s in syms)]
            if rows:
                chunks.append("inventory rows:\n" + "\n".join(rows[:10]))
        except Exception:
            pass
        return "\n".join(chunks)[:4000]
    except (OSError, subprocess.TimeoutExpired):
        return ""


def parse_verdict(text: str) -> tuple[Optional[str], str]:
    """(verdict_or_None, reason<=500). The LAST ``VERDICT:`` line wins;
    unknown and unparseable are both None."""
    text = text or ""
    hits = list(_VERDICT_RE.finditer(text))
    if not hits:
        return None, text.strip()[:500]
    verdict = hits[-1].group(1).lower()
    return (None if verdict == "unknown" else verdict), text[: hits[-1].start()].strip()[:500]


def judge_plan(
    plan_text: str,
    node_text: str,
    dimension: str,
    *,
    spawn: Spawn,
    lenses: Optional[tuple[str, dict[str, str]]] = None,
) -> tuple[Optional[str], str]:
    """Grade one dimension through one ``spawn(name, prompt) -> (rc, out, err)``
    (the caller binds cwd/timeout/model). ``(verdict_or_None, evidence)``."""
    if dimension not in JUDGE_DIMENSIONS:
        return None, f"unknown judge dimension {dimension!r}"
    preamble, sections = lenses if lenses is not None else load_lenses()
    parts = [
        preamble
        or "Grade one question. Quote the plan lines you rely on, reason briefly, end with VERDICT: pass, fail or unknown.",
        f"## The node\n{node_text.strip() or '(none supplied)'}",
        f"## The plan\n{plan_text}",
    ]
    ctx = _gather_context(dimension, plan_text)
    if ctx:
        parts.append(f"## Context from code\n{ctx}")
    if sections.get(dimension):
        parts.append(f"## Your question: {dimension}\n{sections[dimension]}")
    try:
        rc, out, err = spawn(f"blueprint-judge-{dimension}", "\n\n".join(parts) + "\n")
    except Exception as exc:  # any spawn fault -> coverage gap
        return None, f"judge spawn fault: {exc}"[:500]
    if rc != 0:
        return None, f"judge spawn rc={rc}: {(err or out).strip()[:200]}".strip()
    return parse_verdict(out)


def tally(rows: list[dict], *, plan_text, spawn: Spawn) -> dict:
    """Calibration over labeled rows: per-dimension rates + disagreements.
    A None verdict counts WRONG on a control - a control must be gradeable."""
    get = plan_text.get if hasattr(plan_text, "get") else plan_text
    dims: dict[str, list[int]] = {}
    disagreements: list[dict] = []
    controls_wrong = 0
    for row in rows:
        text, node = get(row["plan"]), row.get("node_text") or ""
        for dimension, label in (row.get("labels") or {}).items():
            verdict, reason = judge_plan(text, node, dimension, spawn=spawn)
            s = dims.setdefault(dimension, [0, 0, 0])
            s[0] += 1
            s[1] += verdict == "fail" and label == "fail"
            s[2] += verdict == "pass" and label == "pass"
            if verdict != label:
                disagreements.append(
                    {"plan": row["plan"], "dimension": dimension, "label": label, "judge": verdict, "reason": reason}
                )
                controls_wrong += bool(row.get("control"))
    return {
        "dimensions": {
            d: {"n": n, "tp_rate": round(tp / n, 3) if n else None, "tn_rate": round(tn / n, 3) if n else None}
            for d, (n, tp, tn) in dims.items()
        },
        "disagreements": disagreements,
        "controls_wrong": controls_wrong,
    }
