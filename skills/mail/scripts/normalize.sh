#!/usr/bin/env bash
# normalize.sh - deterministic input normalizer for the /mail skill write verbs.
#
# The /mail skill is a runner-less front door over `fno mail`: an operator on a
# phone types `/mail send target "..."` and the model (the runner) must extract a
# clean recipient + body, refuse an empty one, and run the GENUINE `fno mail`
# command. This helper is the deterministic backstop for that parse so the
# refusal (AC4-ERR) and the smart-quote stripping are not left to model judgment.
#
# Contract (read line-by-line; never `eval`):
#   status=ok | status=error
#   error=<message>            (only when status=error)
#   verb=send | reply
#   recipient=<name>           (send, name mode; empty for broadcast)
#   to_project=<project>       (send, broadcast mode; empty otherwise)
#   msg_id=<id>                (reply)
#   body=<body text>
#
# Only the WRITE verbs need normalization. Reads (unread/list/view/status/ack/
# drain) are thin pass-throughs the skill runs directly, so they are out of scope
# here. Hermetic: no fno, no network. Exit is always 0 (the skill branches on the
# status field, mirroring skills/agent/scripts/normalize.sh).

set -uo pipefail

VERB=""
INPUT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    # Guard value flags: a bare `--verb` / `--input` with no argument would make
    # `shift 2` fail (no set -e) and spin the loop forever, so refuse it cleanly.
    # `--input ""` (an explicit empty value) is $#>=2 and still accepted.
    --verb)
      [[ $# -ge 2 ]] || { printf 'status=error\nerror=--verb needs a value\n'; exit 0; }
      VERB="$2";  shift 2 ;;
    --input)
      [[ $# -ge 2 ]] || { printf 'status=error\nerror=--input needs a value\n'; exit 0; }
      INPUT="$2"; shift 2 ;;
    -h|--help)
      echo "usage: normalize.sh --verb send|reply --input '<text after the verb>'"
      exit 0 ;;
    *)
      printf 'status=error\nerror=unknown argument: %s\n' "$1"
      exit 0 ;;
  esac
done

emit_error() { printf 'status=error\nerror=%s\n' "$1"; exit 0; }

# Convert curly/smart quotes to straight (byte-literal subs: locale-independent).
strip_curly() {
  printf '%s' "$1" | sed 's/“/"/g; s/”/"/g; s/‘/'\''/g; s/’/'\''/g'
}

# Trim leading/trailing whitespace.
trim() {
  local s="$1"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  printf '%s' "$s"
}

# Strip one matching outer pair of straight quotes (symmetric, byte-safe).
strip_wrap() {
  local s="$1"
  if [[ ${#s} -ge 2 ]]; then
    local a="${s:0:1}" b="${s: -1}"
    if { [[ "$a" == '"' && "$b" == '"' ]] || [[ "$a" == "'" && "$b" == "'" ]]; }; then
      s="${s:1:${#s}-2}"
    fi
  fi
  printf '%s' "$s"
}

# Strip any leading/trailing straight quote chars from a single bare token.
strip_token_quotes() {
  local s="$1"
  s="${s#[\"\']}"
  s="${s%[\"\']}"
  printf '%s' "$s"
}

# First whitespace-delimited token of a (pre-trimmed) string. Used instead of
# `read -r first rest <<<...`, which stops at the first NEWLINE and would
# silently truncate a multiline body to its first line (the exact defect the
# sibling skills/agent/scripts/normalize.sh was already fixed for).
first_token() { printf '%s' "${1%%[[:space:]]*}"; }

# Everything after the first token, trimmed. Preserves interior newlines so a
# pasted multiline body survives intact (the body is emitted as the LAST key=
# field, so a multiline value is "everything after body=" - mirroring how
# /agent emits its multiline `message`).
rest_after() { local s="$1" t; t="$(first_token "$s")"; trim "${s#"$t"}"; }

# True when a string is empty OR only whitespace (a blank body is still empty).
is_blank() { [[ -z "${1//[[:space:]]/}" ]]; }

# Lowercase the verb defensively (the model normally passes it lowercase, but a
# phone keyboard may auto-capitalize a hand-typed 'Send').
VERB="$(printf '%s' "$VERB" | tr '[:upper:]' '[:lower:]')"
case "$VERB" in
  send|reply) ;;
  "") emit_error "missing --verb (expected send or reply)" ;;
  *)  emit_error "unsupported verb '$VERB'; the /mail write verbs are send and reply (reads are pass-through)" ;;
esac

raw="$(strip_curly "$INPUT")"
raw="$(trim "$raw")"
[[ -z "$raw" ]] && emit_error "empty input: a $VERB needs a target and a body"

if [[ "$VERB" == "send" ]]; then
  # Split the first token from the rest (rest preserves interior + trailing
  # newlines so a multiline body is not truncated).
  first="$(first_token "$raw")"
  rest="$(rest_after "$raw")"

  # Broadcast detection: a leading `project` / `to-project` / `--to-project`
  # keyword (dashless form is phone-safe; the dash form is back-compat).
  lc_first="$(printf '%s' "$first" | tr '[:upper:]' '[:lower:]')"
  case "$lc_first" in
    project|to-project|--to-project)
      to_project="$(strip_token_quotes "$(first_token "$rest")")"
      [[ -z "$to_project" ]] && emit_error "broadcast send needs a project: 'project <name> \"<body>\"'"
      body="$(strip_wrap "$(rest_after "$rest")")"
      is_blank "$body" && emit_error "empty body: nothing to send to project '$to_project'"
      printf 'status=ok\nverb=send\nrecipient=\nto_project=%s\nmsg_id=\nbody=%s\n' "$to_project" "$body"
      exit 0
      ;;
  esac

  recipient="$(strip_token_quotes "$first")"
  [[ -z "$recipient" ]] && emit_error "empty recipient: who is this for?"
  body="$(strip_wrap "$rest")"
  is_blank "$body" && emit_error "empty body: nothing to send to '$recipient'"
  printf 'status=ok\nverb=send\nrecipient=%s\nto_project=\nmsg_id=\nbody=%s\n' "$recipient" "$body"
  exit 0
fi

# VERB == reply
msg_id="$(strip_token_quotes "$(first_token "$raw")")"
[[ -z "$msg_id" ]] && emit_error "empty msg-id: which message are you replying to?"
body="$(strip_wrap "$(rest_after "$raw")")"
is_blank "$body" && emit_error "empty body: nothing to reply with to '$msg_id'"
printf 'status=ok\nverb=reply\nrecipient=\nto_project=\nmsg_id=%s\nbody=%s\n' "$msg_id" "$body"
exit 0
