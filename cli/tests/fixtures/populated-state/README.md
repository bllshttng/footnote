# Populated state lane

This profile is created inside the smoke runner's temporary sandbox before the
populated lane starts. It carries the state channels that a developer machine
normally has: one graph node named `STATE_LEAK_CANARY`, a config file pointing
at the sandbox state directory, an agent registry with a crown, a session
identity, and one live claim.

The clean lane creates none of these markers. `test_state_canary.py` reads the
graph through `fno.paths.graph_json()` and must pass in clean and fail in
populated. The failure is the positive control that makes an empty verdict
diff readable.
