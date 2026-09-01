#!/bin/sh

# Refuse binary replacement when a daemon with a different runtime identity is
# reachable. The installer never stops a daemon because it cannot know whether
# the daemon still owns active work.
ensure_daemon_runtime_compatible() {
  daemon_codex=$1
  expected_runtime_version=$2
  codex_state_dir=$3
  daemon_socket="$codex_state_dir/app-server-control/app-server-control.sock"

  if ! daemon_output=$($daemon_codex app-server daemon version 2>/dev/null); then
    if [ -e "$daemon_socket" ]; then
      echo "Could not verify the app-server daemon at $daemon_socket; refusing to replace Codex." >&2
      echo "The daemon was left untouched. Finish active daemon-backed jobs, stop it, then rerun the installer." >&2
      return 1
    fi
    return 0
  fi

  if ! daemon_fields=$(printf '%s\n' "$daemon_output" | python3 -c '
import json
import sys

value = json.load(sys.stdin)
status = value.get("status")
version = value.get("appServerVersion")
if not isinstance(status, str):
    raise ValueError("daemon status is missing")
if version is not None and not isinstance(version, str):
    raise ValueError("daemon version is invalid")
print(status)
print(version or "")
' 2>/dev/null); then
    echo "Could not parse app-server daemon status; refusing to replace Codex." >&2
    return 1
  fi

  daemon_status=$(printf '%s\n' "$daemon_fields" | sed -n '1p')
  daemon_version=$(printf '%s\n' "$daemon_fields" | sed -n '2p')
  case "$daemon_status" in
    notRunning)
      return 0
      ;;
    running)
      if [ "$daemon_version" = "$expected_runtime_version" ]; then
        return 0
      fi
      echo "App-server daemon version ${daemon_version:-unknown} is running; the new Codex runtime is $expected_runtime_version." >&2
      echo "The daemon was left untouched. Finish active daemon-backed jobs, then run:" >&2
      echo "  $daemon_codex app-server daemon stop" >&2
      echo "Rerun the installer after the daemon stops." >&2
      return 1
      ;;
    *)
      echo "Unexpected app-server daemon status '$daemon_status'; refusing to replace Codex." >&2
      return 1
      ;;
  esac
}
