#!/usr/bin/env python3
"""Draft curated release notes for a candidate tag, in the approved shape.

Reads merged-PR titles since the base tag, buckets them by
conventional-commit prefix, and writes internal/fno/releases/<version>.md
for the user to trim. Runs where the vault checkout exists (the internal/
symlink); refuses to invent a home for the draft.

Every bullet traces to a real PR title. Titles naming a retired command
(scripts/ci/retired-commands.txt) bucket into Before you upgrade, so a
rename never teaches itself as a feature.

usage:
  draft-release-notes.py <tag>                       # v0.1 draft
  draft-release-notes.py <tag> --since <tag>         # explicit base
  draft-release-notes.py <tag> --pr-file prs.json --out /tmp/x  # offline
  --self-test runs the bucket and title-cleaning asserts.
"""

import argparse
import json
import re
import subprocess
from datetime import datetime
from pathlib import Path

# Bucket per conventional-commit prefix. The curated v0.4.0 notes grouped
# PRs by theme; that judgment stays human. A prefix pass is the honest
# mechanical layer under it.
TYPE_BUCKETS = {
    "feat": "Features",
    "feature": "Features",
    "fix": "Fixes",
    "docs": "Documentation",
    "doc": "Documentation",
    "ci": "Build and CI",
    "workflows": "Build and CI",
    "build": "Build and CI",
    "release": "Build and CI",
    "chore": "Internal",
    "refactor": "Internal",
    "test": "Internal",
    "perf": "Internal",
    "style": "Internal",
}

SECTION_ORDER = [
    "Features",
    "Fixes",
    "Documentation",
    "Changes",
    "Build and CI",
    "Before you upgrade",
]

INTERNAL = "Internal"


def git_out(*args: str) -> str:
    out = subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    )
    return out.stdout.strip()


def newest_plain_tag() -> str:
    tags = git_out("tag", "-l", "v[0-9]*", "--sort=-v:refname").split()
    for tag in tags:
        if "rc" not in tag:
            return tag
    raise SystemExit("draft-release-notes: no plain release tag exists; pass --since")


def load_retired() -> list:
    path = Path("scripts/ci/retired-commands.txt")
    if not path.exists():
        return []
    retired = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        retired.append(line.split("|")[0].strip())
    return retired


def repo_slug() -> str:
    url = git_out("config", "--get", "remote.origin.url")
    tail = url.rsplit(":", 1)[-1] if url.startswith("git@") else url.rsplit("github.com/", 1)[-1]
    return tail.removesuffix(".git")


def categorize(title: str, retired: list) -> str:
    # The retired-command registry wins over any prefix: a rename must not
    # present itself as a feature.
    for cmd in retired:
        if cmd.lower() in title.lower():
            return "Before you upgrade"
    head = title.split(":")[0].strip()
    head = head.rstrip("!")
    if head.endswith(")") and "(" in head:
        head = head.split("(")[0].strip()
    return TYPE_BUCKETS.get(head.lower(), "Changes")


def clean_title(title: str) -> str:
    # Strip a conventional-commit prefix; keep the text otherwise verbatim
    # so every bullet still traces to the PR.
    match = re.match(r"^[a-z]+(\([^)]*\))?:\s+(.*)$", title)
    if match:
        return match.group(2)[:1].upper() + match.group(2)[1:]
    return title[:1].upper() + title[1:]


def prs_from_gh(since_date: str) -> list:
    raw = subprocess.run(
        ["gh", "pr", "list", "--state", "merged", "--limit", "3000",
         "--json", "number,title,mergedAt"],
        check=True, capture_output=True, text=True,
    ).stdout
    prs = json.loads(raw)
    return [pr for pr in prs if pr["mergedAt"] >= since_date]


def draft(tag: str, since: str, prs: list, slug: str, retired: list, now: str) -> str:
    sections = {name: [] for name in SECTION_ORDER + [INTERNAL]}
    for pr in prs:
        bucket = categorize(pr["title"], retired)
        bullet = f"- {clean_title(pr['title'])} (#{pr['number']})"
        sections[bucket].append(bullet)
    if not prs:
        raise SystemExit(f"draft-release-notes: no merged PRs since {since}; nothing to draft")

    lines = [
        "---",
        f"created: {now}",
        f"updated: {now}",
        "---",
        "<!--",
        f"Draft release notes for {tag}. Trim, then drop into the GitHub Release:",
        f"  awk '/^## {tag}/{{p=1}} p' internal/fno/releases/{tag}.md > /tmp/notes.md",
        f"  gh release edit {tag} --notes-file /tmp/notes.md",
        f"Every bullet traces to a real PR title since {since}. Drafted by script; review before publishing.",
        "-->",
        "",
        f"## {tag}",
        "",
        f"{len(prs)} merged pull requests since {since}.",
        "",
    ]
    for name in SECTION_ORDER:
        bullets = sections[name]
        if not bullets:
            continue
        lines.append(f"### {name} ({len(bullets)} PRs)")
        lines.append("")
        lines.extend(bullets)
        lines.append("")
    internal = sections[INTERNAL]
    if internal:
        lines.append("<details>")
        lines.append(f"<summary>Internal ({len(internal)} PRs)</summary>")
        lines.append("")
        lines.append("\n".join(internal))
        lines.append("")
        lines.append("</details>")
        lines.append("")
    lines.append(f"**Full Changelog**: https://github.com/{slug}/compare/{since}...{tag}")
    return "\n".join(lines) + "\n"


def self_test() -> None:
    retired = ["claude rm", "fno dispatch"]
    assert categorize("feat(mux): add portals", retired) == "Features"
    assert categorize("feat!: flip the default", retired) == "Features"
    assert categorize("fix: stop the crash", retired) == "Fixes"
    assert categorize("fno dispatch is gone, spawn instead", retired) == "Before you upgrade"
    assert categorize("ci: fix the wheel build", retired) == "Build and CI"
    assert categorize("chore: tidy", retired) == "Internal"
    assert categorize("unprefixed work", retired) == "Changes"
    assert clean_title("feat(mux): Add portals") == "Add portals"
    assert clean_title("docs: rewrite the install page") == "Rewrite the install page"
    assert clean_title("plain title") == "Plain title"
    print("draft-release-notes: self-test OK")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("tag", nargs="?", help="candidate tag, e.g. v0.4.1rc1")
    parser.add_argument("--since", help="base tag (default: newest plain tag)")
    parser.add_argument("--pr-file", help="offline PR list (JSON) instead of gh")
    parser.add_argument("--out", help="output dir (default internal/fno/releases)")
    parser.add_argument("--force", action="store_true", help="overwrite an existing draft")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return

    root = Path(git_out("rev-parse", "--show-toplevel"))
    since = args.since or newest_plain_tag()
    since_date = git_out("for-each-ref", f"refs/tags/{since}", "--format=%(creatordate:iso-strict)")
    retired = load_retired()

    if args.pr_file:
        prs = [pr for pr in json.loads(Path(args.pr_file).read_text()) if pr["mergedAt"] >= since_date]
    else:
        prs = prs_from_gh(since_date)

    now = datetime.now().strftime("%Y-%m-%dT%H:%M")
    body = draft(tag=args.tag, since=since, prs=prs, slug=repo_slug(), retired=retired, now=now)

    out_dir = Path(args.out) if args.out else root / "internal/fno/releases"
    if not out_dir.is_dir():
        raise SystemExit(
            f"draft-release-notes: {out_dir} does not exist (run where the vault checkout lives); nothing written"
        )
    out_path = out_dir / f"{args.tag}.md"
    if out_path.exists() and not args.force:
        raise SystemExit(f"draft-release-notes: {out_path} exists; pass --force to overwrite")
    out_path.write_text(body)
    print(f"draft-release-notes: wrote {out_path} ({len(prs)} PRs since {since})")


if __name__ == "__main__":
    main()
