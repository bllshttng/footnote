#!/usr/bin/env python3
"""Capture driver for the `review_invocation` goldens.

The Python leg (`cli/src/fno/review/invocation.py` plus the `invocations`
leaf in `cli/src/fno/review/cli.py`) was the parity oracle until the review
port deleted it. This driver ran that leg under env and fixture pins both
legs reproduce and froze the goldens under `tests/golden/review_invocation/`:

- `parse/<key>.json`  one JSON document per parse case, `null` for a
  non-review payload;
- `sidecar/<key>.json`  the id a join answers with, exact for a preset
  sidecar and shape-only (`ri-`) for a mint;
- `settle/<key>.rows.json`  the settle sweep's rows, with the volatile head
  replaced by `<head9>`;
- `settle/<key>.appended.json`  the `review_attestation` data the sweep
  appended per lost row, head likewise normalized;
- `settle/<key>.receipt.txt`  the bare leaf's receipt lines, byte for byte
  except the same head placeholder;
- `cases.json`  the case table itself, which `review_invocation_parity.rs`
  consumes in lockstep.

Volatile fields are normalized instead of frozen: the settle fixture rows
carry ages in minutes, so the Rust side rebuilds the journal relative to its
own clock, and a minted id is pinned by its `ri-` shape. Add a case to BOTH
this driver and the parity test's expectations, or to neither. After the
deletion the driver is kept as provenance: re-running a capture needs the
deleted Python leg, and a bare run says so.

The receipt renderer below is the deleted leaf's exact output shape
(`fno do review invocations` with no action), copied verbatim before the
package went away.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

GOLDEN_DIR = Path(__file__).resolve().parent / "golden" / "review_invocation"
CLI_SRC = Path(__file__).resolve().parents[3] / "cli" / "src"
sys.path.insert(0, str(CLI_SRC))

TTL = 15
HEAD_PLACEHOLDER = "<head9>"

# The corpus the plan froze: the four verbs, the plugin-qualified spelling,
# every level word, ultra, the flags in all three spellings, targets, the
# quote fallback, and the null rows.
PARSE_CASES = [
    ("review bare", "/review"),
    ("code-review bare", "/code-review"),
    ("review-changes bare", "/review-changes"),
    ("sigma-review bare", "/sigma-review"),
    ("plugin qualified", "/fno:review high --comment"),
    ("level low", "/review low"),
    ("level medium", "/review medium"),
    ("level high", "/review high"),
    ("level xhigh", "/review xhigh"),
    ("level max", "/review max"),
    ("ultra forced", "/review ultra --fix 42"),
    ("comment flag", "/review high --comment 123"),
    ("bare fix word", "/review fix"),
    ("em dash fix", "/review —fix"),
    ("en dash is not a flag", "/review high –comment"),
    ("unknown novel flag", "/review high --novel 55"),
    ("pr number target", "/review 312"),
    ("pr url trailing slash", "/review https://github.com/o/r/pull/77/"),
    ("branch target", "/review feature/x"),
    ("quoted target", '/review high --comment "two words"'),
    ("unbalanced quote falls back", "/review high 'unclosed"),
    ("non review verb", "/compact"),
    ("empty payload", ""),
    ("leading slashes", "///review high"),
    ("double dash comment", "/review high -- --comment"),
]

# A join under the pins both legs reproduce: adopt, or mint plus write.
# `preset` seeds the sidecar before the join; `home` decides whether
# FNO_HOME points into the runtime or is scrubbed (the HOME/.fno default).
SIDECAR_CASES = [
    {"label": "valid sidecar adopted", "home": "set", "session": "s-valid", "preset": "valid"},
    {"label": "corrupt sidecar consumed", "home": "set", "session": "s-corrupt", "preset": "corrupt"},
    {"label": "missing sidecar mints", "home": "set", "session": "s-missing", "preset": None},
    {"label": "empty session mints unwritten", "home": "set", "session": "", "preset": None},
    {"label": "default home join", "home": "unset", "session": "s-default", "preset": "valid"},
]

# The settle fixture: fixed ids, ages relative to the sweep's own clock.
SETTLE_CASES = [
    {
        "label": "lost settles once",
        "rows": [{"id": "ri-golden-lost", "kind": "sent", "age_minutes": 120}],
        "detached": False,
    },
    {
        "label": "answered by attestation never settles",
        "rows": [
            {"id": "ri-golden-att", "kind": "sent", "age_minutes": 120},
            {"id": "ri-golden-att", "kind": "attested", "age_minutes": 60},
        ],
        "detached": False,
    },
    {
        "label": "refused stage answers",
        "rows": [
            {"id": "ri-golden-ref", "kind": "sent", "age_minutes": 120},
            {"id": "ri-golden-ref", "kind": "refused", "age_minutes": 60},
        ],
        "detached": False,
    },
    {
        "label": "unjoined sentinel skipped",
        "rows": [{"id": "UNJOINED", "kind": "sent", "age_minutes": 120}],
        "detached": False,
    },
    {
        "label": "fresh inside ttl stays",
        "rows": [{"id": "ri-golden-fresh", "kind": "sent", "age_minutes": 1}],
        "detached": False,
    },
    {
        "label": "detached head refuses",
        "rows": [{"id": "ri-golden-det", "kind": "sent", "age_minutes": 120}],
        "detached": True,
    },
    {
        "label": "mixed table",
        "rows": [
            {"id": "ri-golden-a", "kind": "sent", "age_minutes": 200},
            {"id": "ri-golden-b", "kind": "sent", "age_minutes": 120, "verb": "/review-changes"},
            {"id": "ri-golden-c", "kind": "sent", "age_minutes": 2},
            {"id": "ri-golden-d", "kind": "sent", "age_minutes": 300},
            {"id": "ri-golden-d", "kind": "attested", "age_minutes": 30},
        ],
        "detached": False,
    },
]


def slug(label: str) -> str:
    out: list[str] = []
    prev_us = False
    for ch in label:
        if ch.isascii() and ch.isalnum():
            out.append(ch.lower())
            prev_us = False
        elif not prev_us:
            out.append("_")
            prev_us = True
    return "".join(out).strip("_")


def scrub_env() -> dict[str, str]:
    keep = {}
    for key, value in os.environ.items():
        if key.startswith(("FNO_", "TARGET_", "CLAUDE_", "CODEX_", "GEMINI_",
                           "OPENCODE_", "ANTHROPIC_", "OPENAI_", "ZAI_", "GLM_")):
            continue
        keep[key] = value
    return keep


def rfc3339(ts: float) -> str:
    import datetime

    return datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


def write_preset_sidecar(root: Path, session: str, preset: str | None) -> None:
    if preset is None:
        return
    directory = root / "review-invocations"
    directory.mkdir(parents=True, exist_ok=True)
    sidecar = directory / f"{session}.json"
    if preset == "valid":
        sidecar.write_text(
            json.dumps({"invocation_id": "ri-golden-valid", "target_session_id": session}),
            encoding="utf-8",
        )
    else:
        sidecar.write_text("{not json", encoding="utf-8")


def capture_parse() -> None:
    from fno.review.invocation import parse_review_invocation

    outdir = GOLDEN_DIR / "parse"
    outdir.mkdir(parents=True, exist_ok=True)
    for label, raw in PARSE_CASES:
        parsed = parse_review_invocation(raw)
        (outdir / f"{slug(label)}.json").write_text(
            json.dumps(parsed, sort_keys=True, indent=1) + "\n", encoding="utf-8"
        )


def capture_sidecar() -> None:
    from fno.review.invocation import (
        adopt_pending_invocation,
        mint_invocation_id,
        write_pending_invocation,
    )

    outdir = GOLDEN_DIR / "sidecar"
    outdir.mkdir(parents=True, exist_ok=True)
    for case in SIDECAR_CASES:
        with tempfile.TemporaryDirectory(prefix="ri-golden-") as tmp:
            root = Path(tmp)
            env = scrub_env()
            env["HOME"] = str(root)
            if case["home"] == "set":
                env["FNO_HOME"] = str(root / "home")
            else:
                env.pop("FNO_HOME", None)
            os.environ.clear()
            os.environ.update(env)
            write_preset_sidecar(
                root if case["home"] == "unset" else root / "home",
                case["session"],
                case["preset"],
            )
            # The hook's join, verbatim: adopt, else mint, write only when a
            # session exists.
            session = case["session"]
            found = adopt_pending_invocation(session) if session else None
            if found is None:
                found = mint_invocation_id()
                if session:
                    write_pending_invocation(target_session_id=session, invocation_id=found)
            if found.startswith("ri-golden"):
                golden = {"id": found}
            else:
                golden = {"id_shape": "ri-"}
            (outdir / f"{slug(case['label'])}.json").write_text(
                json.dumps(golden, indent=1) + "\n", encoding="utf-8"
            )


def git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        capture_output=True,
        env={**scrub_env(), "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null"},
    )


def build_runtime(base: Path, detached: bool) -> tuple[Path, Path, str]:
    repo = base / "repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "feature/x")
    git(repo, "config", "user.email", "t@example.test")
    git(repo, "config", "user.name", "t")
    (repo / "f.txt").write_text("x\n", encoding="utf-8")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", "base")
    head9 = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True, text=True
    ).stdout.strip()[:9]
    if detached:
        git(repo, "checkout", "-q", "--detach")
    events_dir = base / ".fno"
    events_dir.mkdir()
    return repo, events_dir / "events.jsonl", head9


def fixture_line(row: dict, now_ts: float) -> str:
    ts = rfc3339(now_ts - row["age_minutes"] * 60)
    kind = row["kind"]
    if kind == "sent":
        return json.dumps(
            {
                "ts": ts,
                "type": "review_invocation",
                "source": "daemon",
                "data": {
                    "invocation_id": row["id"],
                    "stage": "sent",
                    "verb": row.get("verb", "/code-review"),
                    "transport": "mail_send_raw",
                    "receipt": "delivered (hosted)",
                },
            }
        )
    if kind == "refused":
        return json.dumps(
            {
                "ts": ts,
                "type": "review_invocation",
                "source": "daemon",
                "data": {
                    "invocation_id": row["id"],
                    "stage": "refused",
                    "verb": row.get("verb", "/code-review"),
                    "reason": "empty_diff",
                },
            }
        )
    if kind == "attested":
        return json.dumps(
            {
                "ts": ts,
                "type": "review_attestation",
                "source": "hook",
                "data": {
                    "reviewer": "code-review",
                    "head_sha": "f" * 40,
                    "verdict": "pass",
                    "session_id": "fixture",
                    "invocation_id": row["id"],
                },
            }
        )
    raise ValueError(f"unknown fixture kind: {kind}")


def receipt_lines(rows: list[dict]) -> list[str]:
    # The deleted leaf's exact output, copied verbatim from
    # cli/src/fno/review/cli.py before the package went away.
    if not rows:
        return [f"invocations: none lost beyond the {TTL}m TTL"]
    settled = sum(1 for row in rows if row.get("settled"))
    out = [f"invocations: {len(rows)} lost, {settled} settled now"]
    for row in rows:
        if row.get("settled"):
            out.append(f"  settled {row['invocation_id']} at {str(row.get('head'))[:9]}")
        else:
            out.append(f"  refused {row['invocation_id']}: {row.get('reason')}")
    return out


def normalize_head9(text: str, head9: str) -> str:
    return text.replace(head9, HEAD_PLACEHOLDER)


def capture_settle() -> None:
    from fno.review.invocation import settle_lost_invocations
    from fno.events.store_client import native_rows

    outdir = GOLDEN_DIR / "settle"
    outdir.mkdir(parents=True, exist_ok=True)
    import time

    now_ts = time.time()
    for case in SETTLE_CASES:
        key = slug(case["label"])
        with tempfile.TemporaryDirectory(prefix="ri-golden-") as tmp:
            base = Path(tmp)
            repo, events, head9 = build_runtime(base, case["detached"])
            events.write_text(
                "\n".join(fixture_line(row, now_ts) for row in case["rows"]) + "\n",
                encoding="utf-8",
            )
            import datetime

            now = datetime.datetime.fromtimestamp(now_ts, datetime.timezone.utc)
            rows = settle_lost_invocations(ttl_minutes=TTL, cwd=repo, events_path=events, now=now)
            for row in rows:
                if isinstance(row.get("head"), str):
                    row["head"] = HEAD_PLACEHOLDER
            (outdir / f"{key}.rows.json").write_text(
                json.dumps(rows, sort_keys=True, indent=1) + "\n", encoding="utf-8"
            )
            (outdir / f"{key}.receipt.txt").write_text(
                "\n".join(receipt_lines(rows)) + "\n", encoding="utf-8"
            )
            appended = []
            for line in native_rows(events) or []:
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                data = event.get("data")
                if (
                    event.get("type") == "review_attestation"
                    and isinstance(data, dict)
                    and data.get("session_id") == "settle"
                ):
                    data = dict(data)
                    if isinstance(data.get("head_sha"), str):
                        data["head_sha"] = data["head_sha"][:9]
                        if data["head_sha"] == head9:
                            data["head_sha"] = "<head>"
                    appended.append(data)
            appended.sort(key=lambda data: data.get("invocation_id", ""))
            (outdir / f"{key}.appended.json").write_text(
                json.dumps(appended, sort_keys=True, indent=1) + "\n", encoding="utf-8"
            )


def main() -> int:
    if os.environ.get("FNO_CAPTURE_GOLDEN") != "1":
        print(
            "capture-only driver: the Python oracle leg is deleted; set "
            "FNO_CAPTURE_GOLDEN=1 against a tree that still has "
            "cli/src/fno/review/ to re-run the capture"
        )
        return 0
    GOLDEN_DIR.mkdir(parents=True, exist_ok=True)
    cases = (
        [{"family": "parse", "label": label, "raw": raw} for label, raw in PARSE_CASES]
        + [{"family": "sidecar", **case} for case in SIDECAR_CASES]
        + [{"family": "settle", **case} for case in SETTLE_CASES]
    )
    (GOLDEN_DIR / "cases.json").write_text(
        json.dumps(cases, indent=1) + "\n", encoding="utf-8"
    )
    capture_parse()
    capture_sidecar()
    capture_settle()
    print(f"goldens written under {GOLDEN_DIR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
