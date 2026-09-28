"""Short bind roots for AF_UNIX test sockets.

macOS TMPDIR alone eats 48 of sun_path's 104 bytes, so a socket path under
pytest's tmp_path (or a plain ``mkdtemp``) sits at or past the limit before
the test does anything. Every test that binds a unix socket takes its root
from here: a per-run directory pinned under /tmp stays inside the limit on
any machine, and ``mkdtemp`` keeps runs unique.
"""

import tempfile
from pathlib import Path


def short_bind_root(prefix: str = "fno-sock-") -> Path:
    """Per-run bind root under /tmp, short enough for any socket beneath it."""
    return Path(tempfile.mkdtemp(prefix=prefix, dir="/tmp"))
