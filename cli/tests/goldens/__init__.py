"""Door-level golden receipts for the workflow verbs (lifecycle, session,
advance, triage, reconcile).

Every test drives the native door exactly the way a user does and pins the
exit code plus the exact receipt lines it answers. The receipts were captured
from the Python surface before the x-fcb4 port moved each verb into Rust, so
a native arm may only change a receipt when this file changes with it.
"""
