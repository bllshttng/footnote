"""Suite-runner bounds (``config.test.*``) consumed by ``fno doctor test``."""

from pydantic import BaseModel, ConfigDict


class TestBlock(BaseModel):
    """Bounds for one suite run under ``fno doctor test``."""

    model_config = ConfigDict(extra="ignore")

    # Wall-clock bound for one suite run; on expiry the run's whole process
    # GROUP is killed, so the deps/ test binary cargo exec'd dies with it.
    # ``orphan_min_elapsed_seconds`` is read natively by fno-agents.
    timeout_seconds: int = 1800
