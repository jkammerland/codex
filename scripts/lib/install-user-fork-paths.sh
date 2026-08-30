#!/bin/sh

resolve_standalone_codex_bin_dir() {
  command_codex=$1
  codex_state_dir=$2
  current_codex="$codex_state_dir/packages/standalone/current/bin/codex"

  [ -n "$command_codex" ] || return 1
  [ -x "$current_codex" ] || return 1
  resolved_command_codex=$(readlink -f -- "$command_codex") || return 1
  resolved_current_codex=$(readlink -f -- "$current_codex") || return 1
  [ "$resolved_command_codex" = "$resolved_current_codex" ] || return 1

  standalone_bin_dir=$(dirname -- "$resolved_current_codex")
  [ -x "$standalone_bin_dir/codex-code-mode-host" ] || return 1
  printf '%s\n' "$standalone_bin_dir"
}
