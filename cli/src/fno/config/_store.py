"""Remote store keys (``store.*``) - the opt-in shared primary.

Only Rust reads them (crates/fno-agents store_remote.rs), and only from the
global config. This model carries the values so ``fno config set`` can write
them and the unknown-key walker stays quiet.
"""

from typing import Optional

from pydantic import BaseModel, ConfigDict


class StoreBlock(BaseModel):
    """The libSQL (sqld) primary that every machine dials out to."""

    model_config = ConfigDict(extra="ignore")

    remote_url: Optional[str] = None
    remote_token: Optional[str] = None
    share_backlog: bool = False
