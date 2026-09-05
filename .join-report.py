import json

from fno.plan.fidelity import compute_plan_fidelity

d = compute_plan_fidelity(
    plan_path="/Users/bb16/c3po/internal/fno/plans/20260903-project-state-moves-to-fno-spaces-x-b1ee.md"
)
print(
    json.dumps(
        {
            "refused": d["refused"],
            "planned": d["planned"],
            "delivered": d["delivered"],
            "shortfall": d["shortfall"],
            "reason": d["reason"],
        }
    )
)
