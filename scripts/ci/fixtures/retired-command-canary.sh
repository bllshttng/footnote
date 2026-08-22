#!/usr/bin/env bash
# Not executed. This file exists so check-retired-command-strings.sh can prove
# its marker mechanism works without depending on production text.
#
# The target control used to require at least one `retired-ok:` marker
# somewhere in the tree. That made a successful cleanup indistinguishable from
# a drifted marker spelling: rewrite the last descriptive site and the gate
# goes red, with the only exits being to re-add a string the ruling forbids or
# to delete the gate. The control now asserts on the line below instead, so
# production text is free to reach zero marked sites.
#
# Keep both lines. The first must be seen and cleared; the second must be seen
# and reported when its marker is removed.

# retired-ok: the canary the gate's target control asserts on.
CANARY_MARKED="claude rm <short_id> exited 1"
