"""Suite-runner bounds (``config.test.*``) consumed by ``fno doctor test``."""

from typing import Optional

from pydantic import BaseModel, ConfigDict


class TestBlock(BaseModel):
    """Bounds for one suite run under ``fno doctor test``."""

    model_config = ConfigDict(extra="ignore")

    # Wall-clock bound for one suite run; on expiry the run's whole process
    # GROUP is killed, so the deps/ test binary cargo exec'd dies with it.
    # ``orphan_min_elapsed_seconds`` and ``max_net_new`` are read natively by
    # fno-agents.
    timeout_seconds: int = 1800
    # The PR test cap on net-new test declarations. Unset means no cap: a
    # repo that never opted in adds tests freely. Read by the pr push gate
    # and `fno-agents test-delta`.
    max_net_new: Optional[int] = None
