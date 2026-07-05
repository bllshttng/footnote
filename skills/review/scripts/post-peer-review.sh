#!/usr/bin/env bash
# post-peer-review.sh - Post a harness peer's review to a PR under a DISTINCT
# machine identity so it satisfies the login-based loop-check gate (x-4baa).
#
# The trust invariant this preserves: a peer verdict is (1) a different model
# than the author, (2) an immutable posted PR review, (3) able to BLOCK. This
# script posts the provider's output VERBATIM as the review body and each P1 as
# an inline comment carrying the exact blocking-badge markup loop-check's
# `blocking_severity` recognizes - so the loop can never silently flip a
# request-changes into an approve.
#
# It FAILS LOUD (non-zero + gh stderr) on any posting failure: the gate then
# stays UNMET with a stated reason rather than silently wedging (AC6-FR). It is
# idempotent per PR-head-per-identity: a marker comment guards against a second
# loop-check fire double-posting the same review (Concurrency invariant).
#
# Usage:
#   post-peer-review.sh --pr N --provider codex --token-env GH_PEER_TOKEN \
#       --body-file /path/to/review.txt [--p1 "path:line:message"]...
#   post-peer-review.sh --selfcheck   # offline invariants, no gh calls
#
# The identity is whichever account $token-env's PAT authenticates as; its
# login must be in config.review.peer_identity (the gate's expected login).
set -euo pipefail

# The exact P1 blocking-badge markup loop-check matches (loopcheck.rs
# blocking_severity: `body.contains("![P1 Badge]") || body.contains("badge/P1-")`).
# Keep this byte-identical to that matcher or a posted P1 is advisory, not
# blocking.
readonly P1_BADGE='![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat)'

die() { echo "post-peer-review: $*" >&2; exit 1; }

selfcheck() {
  # Offline invariants (the runnable check): the badge string must contain the
  # substrings loop-check keys on, and the marker format must be stable.
  [[ "$P1_BADGE" == *"![P1 Badge]"* ]] || die "selfcheck: badge missing '![P1 Badge]' marker"
  [[ "$P1_BADGE" == *"badge/P1-"* ]]   || die "selfcheck: badge missing 'badge/P1-' url form"
  local m; m="$(head_marker codex deadbeef)"
  [[ "$m" == "<!-- fno-peer:codex:deadbeef -->" ]] || die "selfcheck: marker format drift ($m)"
  echo "selfcheck ok"
}

# A hidden marker embedded in the review body so a re-fire can detect an
# already-posted review for this exact (provider, head_sha) and skip it.
head_marker() { printf '<!-- fno-peer:%s:%s -->' "$1" "$2"; }

main() {
  local pr="" provider="" token_env="" body_file="" ; local -a p1s=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --selfcheck) selfcheck; exit 0 ;;
      --pr) pr="$2"; shift 2 ;;
      --provider) provider="$2"; shift 2 ;;
      --token-env) token_env="$2"; shift 2 ;;
      --body-file) body_file="$2"; shift 2 ;;
      --p1) p1s+=("$2"); shift 2 ;;
      *) die "unknown arg: $1" ;;
    esac
  done

  [[ -n "$pr" ]]        || die "missing --pr"
  [[ -n "$provider" ]]  || die "missing --provider"
  [[ -n "$token_env" ]] || die "missing --token-env"
  [[ -n "$body_file" && -s "$body_file" ]] || die "missing/empty --body-file (peer produced no review - gate stays unmet)"

  local tok="${!token_env:-}"
  [[ -n "$tok" ]] || die "\$$token_env is empty - no PAT for the peer identity; set it and re-run (gate stays unmet)"

  # Resolve the current head sha so the marker + inline commit_id pin to HEAD.
  local head_sha
  head_sha="$(GH_TOKEN="$tok" gh pr view "$pr" --json headRefOid -q .headRefOid)" \
    || die "gh pr view failed for PR #$pr (gate stays unmet)"
  local marker; marker="$(head_marker "$provider" "$head_sha")"

  # Anti-gaming (codex P1 on #205): the peer MUST post under an account distinct
  # from the PR author, or a misconfigured peer_identity == author would self-
  # certify the gate. Verify the token's own login is not the PR author. Fail
  # CLOSED if either read fails (never post under an unverifiable identity).
  local my_login pr_author
  my_login="$(GH_TOKEN="$tok" gh api user -q .login 2>/dev/null)" \
    || die "cannot resolve the peer identity from \$$token_env (gh api user failed); a peer must post under a verifiable distinct account"
  pr_author="$(GH_TOKEN="$tok" gh pr view "$pr" --json author -q .author.login 2>/dev/null)" \
    || die "gh pr view (author) failed for PR #$pr (gate stays unmet)"
  [[ -n "$my_login" && "$my_login" != "$pr_author" ]] \
    || die "peer identity '$my_login' IS the PR author - a peer must post under a DISTINCT machine account (a self-review cannot gate)"

  # Idempotency: skip if a review with this exact marker already exists. The
  # marker is written LAST (after every required post below), so its presence
  # means the whole post succeeded - a partial failure leaves no marker and the
  # next run re-posts everything (codex P1 on #205).
  if GH_TOKEN="$tok" gh api "repos/{owner}/{repo}/pulls/$pr/reviews" --paginate \
        -q '.[].body' 2>/dev/null | grep -qF "$marker"; then
    echo "post-peer-review: $provider review already posted for $head_sha (skip)"
    return 0
  fi

  # Post each P1 as an inline blocking comment carrying the badge markup FIRST,
  # BEFORE the marker-bearing body, so a failed P1 post can never be masked by a
  # marker (which would clear the gate with no blocking finding). Guard the
  # empty case explicitly: `"${p1s[@]:-}"` trips set -u on an empty array in
  # Bash 3.2 (macOS ships it) - gemini MEDIUM on #205.
  local f
  if [[ ${#p1s[@]} -gt 0 ]]; then
   for f in "${p1s[@]}"; do
    [[ -n "$f" ]] || continue
    local path line msg
    path="${f%%:*}"; local rest="${f#*:}"; line="${rest%%:*}"; msg="${rest#*:}"
    [[ -n "$path" && "$line" =~ ^[0-9]+$ ]] || die "malformed --p1 '$f' (want path:line:message)"
    GH_TOKEN="$tok" gh api "repos/{owner}/{repo}/pulls/$pr/comments" \
      -f body="$P1_BADGE"$'\n\n'"$msg" \
      -f commit_id="$head_sha" \
      -f path="$path" \
      -F line="$line" \
      -f side=RIGHT >/dev/null \
      || die "gh api inline comment failed ($path:$line) for PR #$pr (gate stays unmet)"
   done
  fi

  # Post the verbatim body as a COMMENTED review under the peer identity LAST,
  # carrying the idempotency marker - only reached once all P1s above succeeded.
  local body_tmp; body_tmp="$(mktemp)"; trap 'rm -f "$body_tmp"' EXIT
  { cat "$body_file"; printf '\n\n%s\n' "$marker"; } > "$body_tmp"
  GH_TOKEN="$tok" gh pr review "$pr" --comment --body-file "$body_tmp" \
    || die "gh pr review (body) failed for PR #$pr (gate stays unmet)"

  echo "post-peer-review: posted $provider review as gate identity ($my_login, ${#p1s[@]} P1 inline)"
}

main "$@"
