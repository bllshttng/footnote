#!/usr/bin/env bash
# Retired: producers run through hooks/context-run.sh over the groups in
# hooks/context-hooks.json. This no-op stub stays for one release so a session
# initialized before the retirement cannot fail its hook-config snapshot on a
# missing file. Delete it in the next release.
exec cat >/dev/null
exit 0
