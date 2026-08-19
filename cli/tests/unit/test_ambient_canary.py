"""The dirty lane's positive control.

A green dirty lane is supposed to mean "the ambient surface is complete". That
reading is only worth anything if the lane can go red at all, and a lane that
has never been observed red has two explanations - no leaks, or the instrument
never ran - which no green result can tell apart.

So this test reads a channel nothing in ``fno/hermetic.py`` scrubs:

    clean lane -> AMBIENT_LEAK_CANARY unset -> PASSES
    dirty lane -> AMBIENT_LEAK_CANARY leaks -> FAILS

``tests/ci/test_hermetic_lanes.sh`` asserts BOTH halves. If someone widens a
scrub rule until it swallows the canary, this test stops failing under poison,
that self-test goes red, and the silent disarming is caught.

Do NOT "fix" a dirty-lane failure here by pinning the canary. Its failure IS
the feature.

This file is `--ignore`d by the smoke pytest step for that reason: as part of
the ordinary suite its dirty-lane half would make `smoke` red on every commit,
and a job that is always red gets ignored exactly as fast as one that is
always green. The self-test runs it by path, in both lanes, and asserts each
outcome.
"""
from __future__ import annotations

import os

from fno.hermetic import AMBIENT_LEAK_CANARY


def test_no_ambient_state_reaches_this_process():
    leaked = os.environ.get(AMBIENT_LEAK_CANARY)
    assert leaked is None, (
        f"{AMBIENT_LEAK_CANARY}={leaked!r} reached a test process.\n"
        "In the dirty lane this is the EXPECTED failure: it proves the lane can "
        "detect a channel the ambient surface does not cover.\n"
        "In the clean lane it means something is poisoning the environment "
        "outside `fno test smoke --ambient dirty`."
    )
