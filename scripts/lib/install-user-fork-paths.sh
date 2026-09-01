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

is_active_managed_standalone_codex() {
  command_codex=$1
  codex_state_dir=$2
  current_codex="$codex_state_dir/packages/standalone/current/bin/codex"

  [ -n "$command_codex" ] || return 1
  [ -e "$current_codex" ] || return 1
  resolved_command_codex=$(readlink -f -- "$command_codex") || return 1
  resolved_current_codex=$(readlink -f -- "$current_codex") || return 1
  [ "$resolved_command_codex" = "$resolved_current_codex" ]
}

resolve_npm_codex_bin_dir() {
  command_codex=$1
  npm_root=$2
  npm_package_root="$npm_root/@openai/codex"
  npm_launcher="$npm_package_root/bin/codex.js"
  npm_bin_dir="$npm_package_root/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin"
  npm_codex="$npm_bin_dir/codex"

  [ -n "$command_codex" ] || return 1
  [ -e "$npm_launcher" ] || return 1
  [ -x "$npm_codex" ] || return 1
  [ -x "$npm_bin_dir/codex-code-mode-host" ] || return 1
  resolved_command_codex=$(readlink -f -- "$command_codex") || return 1
  resolved_npm_launcher=$(readlink -f -- "$npm_launcher") || return 1
  resolved_npm_codex=$(readlink -f -- "$npm_codex") || return 1
  if [ "$resolved_command_codex" != "$resolved_npm_launcher" ] && \
    [ "$resolved_command_codex" != "$resolved_npm_codex" ]; then
    return 1
  fi

  printf '%s\n' "$npm_bin_dir"
}
