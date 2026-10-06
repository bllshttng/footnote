#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
python3 - <<'PY'
import re, subprocess
from pathlib import Path
owner = Path('crates/fno-agents/src/role_migration.rs')
source = owner.read_text()
table = source.split('const WORDS:', 1)[1].split('];', 1)[0]
banned = {old.lower() for old in re.findall(r'\("([A-Za-z]+)",\s*"[a-z_]+"\)', table)}
if len(banned) < 20:
    raise SystemExit('role vocabulary: migration vocabulary is missing or truncated')
allowed = {str(owner), 'crates/fno/src/role_migration.rs'}
files = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z']).decode().split('\0')
failures = []
suspect = re.compile("|".join(re.escape(word) for word in sorted(banned)), re.I)
def found(text):
    if not suspect.search(text):
        return False
    for word in re.findall('[A-Za-z]+', text):
        parts = re.findall('[A-Z]+(?=[A-Z][a-z]|$)|[A-Z]?[a-z]+|[A-Z]+', word)
        if any(part.lower() in banned for part in parts):
            return True
    return False
for filename in sorted(set(files)):
    if not filename or filename in allowed:
        continue
    path = Path(filename)
    if not path.exists() or path.is_symlink():
        continue
    if found(filename):
        failures.append(f'{filename}: forbidden role vocabulary in path')
    try:
        content = path.read_text()
    except UnicodeError:
        continue
    for number, line in enumerate(content.splitlines(), 1):
        if found(line):
            failures.append(f'{filename}:{number}: forbidden role vocabulary')
if failures:
    print('\n'.join(failures))
    raise SystemExit(1)
print('role vocabulary: paths and text are clean; compatibility is confined to the migration boundary')
PY
