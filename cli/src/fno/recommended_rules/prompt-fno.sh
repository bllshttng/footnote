# footnote pane provenance - portable bash/zsh prompt segment (no prompt engine).
#
# fno-spawned panes export FNO_NODE (backlog node), FNO_SLUG, FNO_PLAN, and
# FNO_PR (once a PR is linked). Source this from ~/.bashrc or ~/.zshrc and your
# prompt gains a "⚑ <node>" prefix while inside an fno pane; a plain shell shows
# nothing (gated on $FNO_NODE). This is the starship-free path - it edits only
# $PS1 and needs no external tool.
#
# Install: `fno setup wizard` offers to add the `source` line, or add it yourself:
#   echo 'source "$(fno path prompt-snippet 2>/dev/null || true)"' >> ~/.zshrc
# (starship users: see starship-fno.toml for the equivalent custom module.)

if [ -n "${FNO_NODE:-}" ]; then
  # Plain text (no color): PS1 color escapes differ between bash (\[ \]) and zsh
  # (%{ %}); the starship module owns styling. The idempotent guard means a
  # re-source (or a nested login shell) never double-prefixes.
  _fno_prov="⚑ ${FNO_NODE}${FNO_PR:+ PR#${FNO_PR}} "
  case "${PS1:-}" in
    *"$_fno_prov"*) : ;;
    *) PS1="${_fno_prov}${PS1:-}" ;;
  esac
  unset _fno_prov
fi
