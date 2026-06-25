#!/usr/bin/env python3
"""Point a Homebrew tap formula at a published PyPI version of `fno`.

Rewrites the macOS x86_64 + arm64 wheel `url`+`sha256` and the pinned `version`
in a `Formula/fno.rb` to the wheels PyPI is serving for <version>. Driven by the
release workflow's Homebrew leg (x-0afe); also runnable by hand for a one-off.

    update-homebrew-formula.py <version> <formula_path>
    update-homebrew-formula.py --self-check

The live tap is the source of truth for `brew install`; the in-repo
scripts/install/homebrew/fno.rb is only a seed snapshot.
"""
from __future__ import annotations

import json
import re
import sys
import urllib.request

PYPI = "https://pypi.org/pypi/fno/{}/json"
# Arch substrings as they appear in the wheel filenames + the formula url lines.
X64 = "macosx_10_12_x86_64"
ARM = "macosx_14_0_arm64"
_URL_RE = re.compile(r'url "([^"]*?(' + re.escape(X64) + "|" + re.escape(ARM) + r')[^"]*\.whl)"')


def fetch_wheels(version: str) -> dict[str, tuple[str, str]]:
    """{arch_substr: (url, sha256)} for the two macOS wheels of <version>."""
    with urllib.request.urlopen(PYPI.format(version), timeout=30) as r:
        data = json.load(r)
    out: dict[str, tuple[str, str]] = {}
    for u in data["urls"]:
        fn = u["filename"]
        if fn.endswith(".whl"):
            for arch in (X64, ARM):
                if arch in fn:
                    out[arch] = (u["url"], u["digests"]["sha256"])
    return out


def rewrite(text: str, version: str, wheels: dict[str, tuple[str, str]]) -> str:
    """Pin `version` and swap each macOS wheel url + the sha256 that follows it.

    The formula has two `url ".../<arch>.whl"` lines, each followed (a few lines
    later) by its own `sha256 "..."`. Walk line by line, remember the arch of the
    last url seen, and rewrite the next sha256 with that arch's digest - so it is
    order-independent and never pairs a sha with the wrong arch.
    """
    out: list[str] = []
    pending: str | None = None
    for line in text.splitlines(keepends=True):
        m = _URL_RE.search(line)
        if m:
            url, _ = wheels[m.group(2)]
            line = line[: m.start(1)] + url + line[m.end(1) :]
            pending = m.group(2)
        elif pending and 'sha256 "' in line:
            _, sha = wheels[pending]
            line = re.sub(r'sha256 "[^"]*"', f'sha256 "{sha}"', line)
            pending = None
        elif re.search(r'version "[^"]+"', line):
            line = re.sub(r'version "[^"]+"', f'version "{version}"', line)
        out.append(line)
    return "".join(out)


def self_check() -> None:
    sample = (
        '  url "https://old/fno-0.1.0-py3-none-macosx_10_12_x86_64.whl", using: :nounzip\n'
        '  version "0.1.0"\n'
        '  sha256 "' + "deadbeef" * 8 + '"\n'
        "  on_macos do\n"
        "    on_arm do\n"
        '      url "https://old/fno-0.1.0-py3-none-macosx_14_0_arm64.whl", using: :nounzip\n'
        '      sha256 "' + "cafebabe" * 8 + '"\n'
    )
    wheels = {X64: ("https://new/x64.whl", "1" * 64), ARM: ("https://new/arm.whl", "2" * 64)}
    got = rewrite(sample, "0.2.1", wheels)
    assert 'version "0.2.1"' in got, got
    assert 'url "https://new/x64.whl"' in got and f'sha256 "{"1" * 64}"' in got, got
    assert 'url "https://new/arm.whl"' in got and f'sha256 "{"2" * 64}"' in got, got
    assert "0.1.0" not in got and "deadbeef" not in got and "cafebabe" not in got, got
    print("self-check OK")


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--self-check":
        self_check()
        return 0
    if len(sys.argv) != 3:
        sys.stderr.write(__doc__)
        return 2
    version, path = sys.argv[1], sys.argv[2]
    try:
        wheels = fetch_wheels(version)
    except (OSError, ValueError) as e:  # network error, bad status, or non-JSON body
        sys.stderr.write(f"fno {version}: could not fetch wheels from PyPI: {e}\n")
        return 1
    missing = [a for a in (X64, ARM) if a not in wheels]
    if missing:
        sys.stderr.write(f"fno {version}: macOS wheels not on PyPI yet: {missing}\n")
        return 1
    with open(path, encoding="utf-8") as f:
        text = f.read()
    with open(path, "w", encoding="utf-8") as f:
        f.write(rewrite(text, version, wheels))
    print(f"updated {path} -> fno {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
