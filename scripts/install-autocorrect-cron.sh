#!/usr/bin/env bash
# install-autocorrect-cron.sh - schedule the monthly review and the 15-minute S0
# watcher on the host platform.
#
# Detects macOS (launchd) vs Linux (cron). Idempotent: re-running updates the
# installed entries in place without duplicating them.
#
# Flags:
#   --uninstall   remove the schedule (no-op if not installed)
#   --status      print whether the schedule is currently installed
#   --dry-run     render the substituted plist/cron entry without writing it

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TEMPLATES_DIR="$REPO_ROOT/templates"

PACK_SCRIPT="$SCRIPT_DIR/autocorrect-pack.sh"
REVIEW_SCRIPT="$SCRIPT_DIR/autocorrect-review.sh"
WATCHER_SCRIPT="$SCRIPT_DIR/autocorrect-watcher.sh"

LAUNCH_AGENTS_DIR="$HOME/Library/LaunchAgents"
LOG_DIR="${AUTOCORRECT_LOG_DIR:-$HOME/.claude/logs}"

ACTION="install"
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --uninstall) ACTION="uninstall" ;;
    --status)    ACTION="status" ;;
    --dry-run)   DRY_RUN=1 ;;
    -h|--help)
      sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
  esac
done

PLATFORM="$(uname)"

render_plist() {
  local template="$1"
  sed \
    -e "s|{{PACK_SCRIPT}}|$PACK_SCRIPT|g" \
    -e "s|{{REVIEW_SCRIPT}}|$REVIEW_SCRIPT|g" \
    -e "s|{{WATCHER_SCRIPT}}|$WATCHER_SCRIPT|g" \
    -e "s|{{LOG_DIR}}|$LOG_DIR|g" \
    -e "s|{{HOME}}|$HOME|g" \
    "$template"
}

install_macos() {
  mkdir -p "$LAUNCH_AGENTS_DIR" "$LOG_DIR"

  local jobs=("com.user.autocorrect" "com.user.autocorrect-watcher")
  for job in "${jobs[@]}"; do
    local template="$TEMPLATES_DIR/$job.plist"
    local target="$LAUNCH_AGENTS_DIR/$job.plist"
    if [[ ! -f "$template" ]]; then
      echo "install-autocorrect-cron: template missing: $template" >&2
      exit 1
    fi
    local rendered
    rendered=$(render_plist "$template")
    if [[ "$DRY_RUN" == "1" ]]; then
      echo "----- would write $target -----"
      printf '%s\n' "$rendered"
      continue
    fi
    # Compare against existing - only rewrite if changed.
    if [[ -f "$target" ]] && diff -q <(printf '%s\n' "$rendered") "$target" >/dev/null 2>&1; then
      echo "install-autocorrect-cron: $job already up to date" >&2
    else
      printf '%s\n' "$rendered" > "$target"
      chmod 600 "$target"
      # Unload first to ensure changes take effect.
      launchctl unload "$target" 2>/dev/null || true
      launchctl load "$target"
      echo "install-autocorrect-cron: installed $job at $target" >&2
    fi
  done
}

uninstall_macos() {
  local jobs=("com.user.autocorrect" "com.user.autocorrect-watcher")
  for job in "${jobs[@]}"; do
    local target="$LAUNCH_AGENTS_DIR/$job.plist"
    if [[ -f "$target" ]]; then
      launchctl unload "$target" 2>/dev/null || true
      rm "$target"
      echo "install-autocorrect-cron: removed $job" >&2
    fi
  done
}

status_macos() {
  local jobs=("com.user.autocorrect" "com.user.autocorrect-watcher")
  for job in "${jobs[@]}"; do
    if launchctl list | grep -q "$job"; then
      echo "  $job: registered"
    else
      echo "  $job: NOT registered"
    fi
  done
}

_BEGIN_MARK="# BEGIN autocorrect-managed"
_END_MARK="# END autocorrect-managed"

# Read the user's crontab. Distinguish "no crontab" (clean empty) from
# "crontab -l failed for another reason" (refuse to clobber).
_read_crontab() {
  local out err rc
  err=$(mktemp)
  out=$(crontab -l 2>"$err"); rc=$?
  if [[ $rc -eq 0 ]]; then
    rm -f "$err"
    printf '%s' "$out"
    return 0
  fi
  # "no crontab for $USER" is the only acceptable failure: returns empty.
  if grep -q "no crontab" "$err" 2>/dev/null; then
    rm -f "$err"
    printf ''
    return 0
  fi
  echo "install-autocorrect-cron: crontab -l failed: $(cat "$err")" >&2
  rm -f "$err"
  return 1
}

# Strip our managed block from a crontab body. Uses BEGIN/END sentinels so
# unrelated lines containing autocorrect paths or the literal "# autocorrect"
# are not touched.
_strip_managed_block() {
  awk -v begin="$_BEGIN_MARK" -v end="$_END_MARK" '
    $0 == begin {skip=1; next}
    $0 == end   {skip=0; next}
    !skip {print}
  '
}

install_linux() {
  mkdir -p "$LOG_DIR"
  local monthly_entry="0 9 1 * * $PACK_SCRIPT --severity S1,S2 | $REVIEW_SCRIPT --severity S1,S2 --packet-source monthly-cron >> $LOG_DIR/autocorrect-monthly.log 2>&1"
  local watcher_entry="*/15 * * * * $WATCHER_SCRIPT >> $LOG_DIR/autocorrect-watcher.log 2>&1"

  if [[ "$DRY_RUN" == "1" ]]; then
    echo "----- would add to crontab -----"
    printf '%s\n%s\n%s\n%s\n' "$_BEGIN_MARK" "$monthly_entry" "$watcher_entry" "$_END_MARK"
    return
  fi

  local existing
  if ! existing=$(_read_crontab); then
    echo "install-autocorrect-cron: refusing to install over an unreadable crontab" >&2
    return 1
  fi

  local stripped
  stripped=$(printf '%s\n' "$existing" | _strip_managed_block)

  {
    if [[ -n "$stripped" ]]; then
      printf '%s\n' "$stripped"
    fi
    printf '%s\n%s\n%s\n%s\n' "$_BEGIN_MARK" "$monthly_entry" "$watcher_entry" "$_END_MARK"
  } | crontab -
  echo "install-autocorrect-cron: installed Linux cron entries" >&2
}

uninstall_linux() {
  local existing
  if ! existing=$(_read_crontab); then
    echo "install-autocorrect-cron: refusing to mutate an unreadable crontab" >&2
    return 1
  fi
  local stripped
  stripped=$(printf '%s\n' "$existing" | _strip_managed_block)
  if [[ -z "$stripped" ]]; then
    crontab -r 2>/dev/null || true
  else
    printf '%s\n' "$stripped" | crontab -
  fi
  echo "install-autocorrect-cron: removed Linux cron entries" >&2
}

status_linux() {
  local existing
  existing=$(_read_crontab 2>/dev/null || printf '')
  local managed
  managed=$(printf '%s\n' "$existing" | awk -v begin="$_BEGIN_MARK" -v end="$_END_MARK" '
    $0 == begin {inblk=1; next}
    $0 == end   {inblk=0; next}
    inblk {print}
  ')
  if printf '%s\n' "$managed" | grep -qF -- "$PACK_SCRIPT"; then
    echo "  monthly cron: registered"
  else
    echo "  monthly cron: NOT registered"
  fi
  if printf '%s\n' "$managed" | grep -qF -- "$WATCHER_SCRIPT"; then
    echo "  watcher cron: registered"
  else
    echo "  watcher cron: NOT registered"
  fi
}

case "$PLATFORM:$ACTION" in
  Darwin:install)   install_macos ;;
  Darwin:uninstall) uninstall_macos ;;
  Darwin:status)    status_macos ;;
  Linux:install)    install_linux ;;
  Linux:uninstall)  uninstall_linux ;;
  Linux:status)     status_linux ;;
  *)
    echo "install-autocorrect-cron: unsupported platform: $PLATFORM" >&2
    exit 1
    ;;
esac
