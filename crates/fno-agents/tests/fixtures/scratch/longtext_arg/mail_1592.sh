#!/usr/bin/env bash
read -r -d '' M <<'EOF'
Not ready. Two blockers, one command each.

ci_red: the PR body carries no closure trailer. check-pr-node-closure says the branch names NODEID and the trailer claims none. Run fno do pr closure-trailer NODEID, then append the printed line yourself.

review_in_flight: your OWN hold on feature/NODEID, pinned to c565f559, an older head than 1dda231a. You pushed past your own hold. Release it with fno do pr review-hold release --branch feature/NODEID.
EOF
fno agents mail send t-8975-glm-spawndefaults "$M" --from-name 647b3a9c
