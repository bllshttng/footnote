#!/usr/bin/env python3
"""Build the Run 1 FNO_CONFIG: the machine's fno config merged with the study lanes.

  merge_config.py <base.toml> <lanes.toml> <out.toml>

A text concatenation breaks when the base sets a key inline that the lanes file
opens as a table (`provider_limits = { zai = ... }` then `[agents.provider_limits.zai]`):
TOML refuses the duplicate. This merges structurally instead. Tables merge key by
key, arrays of tables (`routing.models`, `accounts.records`) append, and on a
scalar clash the study's value wins. A missing base reads as empty. The z.ai account
record is added when the base has none. Run through `uv run --with tomli-w`.
"""
import sys
import tomllib
from pathlib import Path

import tomli_w

ZAI_ACCOUNT = {"id": "zai", "name": "zai", "harness": "claude", "auth": "api_key", "priority": 100,
               "route": "zai/glm-5.3-flash[1m]", "account_id": "zai"}


def merge(base: dict, extra: dict) -> dict:
    out = dict(base)
    for key, value in extra.items():
        if isinstance(value, dict) and isinstance(out.get(key), dict):
            out[key] = merge(out[key], value)
        elif isinstance(value, list) and isinstance(out.get(key), list):
            out[key] = out[key] + value
        else:
            out[key] = value
    return out


def main(base_path: str, lanes_path: str, out_path: str) -> int:
    base_file = Path(base_path)
    base = tomllib.loads(base_file.read_text()) if base_file.is_file() else {}
    config = merge(base, tomllib.loads(Path(lanes_path).read_text()))
    records = config.setdefault("accounts", {}).setdefault("records", [])
    if not any(r.get("id") == "zai" for r in records):
        records.append(ZAI_ACCOUNT)
    Path(out_path).write_text(tomli_w.dumps(config))
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 4:
        print(__doc__)
        raise SystemExit(2)
    raise SystemExit(main(*sys.argv[1:]))
