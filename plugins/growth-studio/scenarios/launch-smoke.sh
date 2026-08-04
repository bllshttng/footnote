#!/usr/bin/env bash
# Declared-test evidence for the growth-studio pack: a real, portable structural
# check with no fno dependency (so it runs wherever the verifier runs). It
# validates the packaged manifest declares the four expected roles, each at the
# founder approval floor and an internal authority ceiling, exiting nonzero on
# any shortfall. It dispatches nothing externally and grants no effect.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
manifest="$here/../plugin.yaml"

python3 - "$manifest" <<'PY'
import sys
import yaml

with open(sys.argv[1], encoding="utf-8") as handle:
    pack = yaml.safe_load(handle)

roles = {role["role"]["id"]: role for role in pack.get("roles", ())}
expected = {"marketing", "communications", "design", "social"}
missing = expected - roles.keys()
assert not missing, f"missing expected roles: {sorted(missing)}"

for role_id, role in roles.items():
    assert role["approval_floor"] == "founder", f"{role_id} approval_floor is not founder"
    assert role["authority_ceiling"] == "internal", f"{role_id} authority_ceiling is not internal"
    assert role["default_topology"] in {"direct", "loop", "squad", "pipeline"}, (
        f"{role_id} default_topology is not one of the four closed literals"
    )

print("growth-studio launch smoke: four roles present, founder floor, internal ceiling, closed topology")
PY
