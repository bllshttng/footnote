"""fno shims must never dangle into a cleaned mktemp dir (x-c911).

Scans the tool bin for fno* symlinks whose target dangles or resolves under
a temp root, and repoints them to the durable uv tools copy.
Run: `python -m fno.setup.shim_check [--repair] [--bin-dir D]`.
"""

from __future__ import annotations

import os
import sys
import tempfile
from pathlib import Path

UV_TOOL_FNO_BIN = Path.home() / ".local" / "share" / "uv" / "tools" / "fno" / "bin"


def _temp_root() -> str:
    return str(Path(tempfile.gettempdir()).resolve())


def scan(bin_dir: Path | None = None) -> dict:
    """Report every fno* symlink defect (dangling or temp-resolving).

    Scope is the fno prefix, so a broken third-party link is not our finding.
    """
    directory = Path(bin_dir) if bin_dir else Path.home() / ".local" / "bin"
    temp_root = _temp_root()
    defects: list[dict] = []
    for entry in sorted(Path(directory).glob("fno*")):
        if not entry.is_symlink():
            continue
        target = Path(os.readlink(entry))
        if not target.is_absolute():
            target = entry.parent / target
        resolved = target.resolve()
        if not resolved.exists():
            defects.append(_defect(entry, resolved, "dangling"))
        elif str(resolved).startswith(temp_root):
            defects.append(_defect(entry, resolved, "temp-resolving"))
    return {"bin_dir": str(directory), "defects": defects, "healthy": not defects}


def _defect(link: Path, resolved: Path, problem: str) -> dict:
    durable = UV_TOOL_FNO_BIN / link.name
    repairable = durable.is_file() and os.access(durable, os.X_OK)
    return {
        "name": link.name,
        "link": str(link),
        "target": str(resolved),
        "problem": problem,
        "repair": str(durable) if repairable else None,
    }


def repair(defects: list[dict]) -> list[str]:
    """Repoint repairable defects to the durable copy; return what still fails."""
    remaining: list[str] = []
    for defect in defects:
        durable = defect.get("repair")
        if not durable:
            remaining.append(f"{defect['name']}: no durable copy at {durable}")
            continue
        link = Path(defect["link"])
        tmp = link.with_name(f".{link.name}.relink.{os.getpid()}")
        try:
            tmp.symlink_to(durable)
            os.replace(tmp, link)  # atomic: a reader never sees the link absent
        except OSError as exc:
            remaining.append(f"{defect['name']}: relink failed: {exc}")
        finally:
            Path(tmp).unlink(missing_ok=True)
    return remaining


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    do_repair = "--repair" in args
    bin_dir = None
    if "--bin-dir" in args:
        bin_dir = Path(args[args.index("--bin-dir") + 1])
    report = scan(bin_dir)
    for defect in report["defects"]:
        print(f"shim defect: {defect['name']} -> {defect['target']} ({defect['problem']})")
    if do_repair and report["defects"]:
        for line in repair(report["defects"]):
            print(f"shim unrepairable: {line}")
        report = scan(bin_dir)
    if not report["healthy"]:
        print(
            f"shim scan: {len(report['defects'])} defect(s) in {report['bin_dir']}; "
            f"re-run with --repair, or relink to {UV_TOOL_FNO_BIN}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

