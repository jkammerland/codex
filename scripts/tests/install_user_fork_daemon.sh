#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$SCRIPT_DIR/../lib/install-user-fork-daemon.sh"

TEST_DIR=$(mktemp -d)
trap 'rm -rf -- "$TEST_DIR"' EXIT HUP INT TERM

FAKE_BIN_DIR="$TEST_DIR/fake bin"
mkdir -p "$FAKE_BIN_DIR"
FAKE_CODEX="$FAKE_BIN_DIR/codex"
cat >"$FAKE_CODEX" <<'EOF'
#!/bin/sh

if [ "${1:-} ${2:-} ${3:-}" = "app-server daemon disable-auto-update" ]; then
  printf '%s\n' "${CODEX_HOME:-missing}" >>"${FAKE_UPDATER_LOG:?}"
  [ "${FAKE_UPDATER_MODE:-success}" != failure ]
  exit $?
fi

case "${FAKE_DAEMON_MODE:-not-running}" in
  not-running)
    printf '%s\n' '{"status":"notRunning","appServerVersion":null}'
    ;;
  matching)
    printf '%s\n' '{"status":"running","appServerVersion":"0.0.0+jkammerland.mcp.18"}'
    ;;
  stale)
    printf '%s\n' '{"status":"running","appServerVersion":"0.151.0"}'
    ;;
  missing-version)
    printf '%s\n' '{"status":"running","appServerVersion":null}'
    ;;
  malformed)
    printf '%s\n' '{'
    ;;
  unavailable)
    exit 1
    ;;
esac
EOF
chmod +x "$FAKE_CODEX"

EXPECTED_VERSION="0.0.0+jkammerland.mcp.18"
STATE_DIR="$TEST_DIR/state"
ERROR_LOG="$TEST_DIR/error.log"
FAKE_UPDATER_LOG="$TEST_DIR/updater.log"
export FAKE_UPDATER_LOG

disable_daemon_auto_update "$FAKE_CODEX" "$STATE_DIR"
FAKE_UPDATER_MODE=success disable_daemon_auto_update "$FAKE_CODEX" "$STATE_DIR"
if FAKE_UPDATER_MODE=failure \
  disable_daemon_auto_update "$FAKE_CODEX" "$STATE_DIR" 2>"$ERROR_LOG"; then
  echo "updater shutdown failure was ignored" >&2
  exit 1
fi
grep -F "Could not disable the official Codex daemon updater" "$ERROR_LOG" >/dev/null
if [ "$(grep -Fxc "$STATE_DIR" "$FAKE_UPDATER_LOG")" -ne 3 ]; then
  echo "updater shutdown did not receive the isolated Codex state directory" >&2
  exit 1
fi

FAKE_DAEMON_MODE=not-running \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR"
FAKE_DAEMON_MODE=matching \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR"
FAKE_DAEMON_MODE=unavailable \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR"

if FAKE_DAEMON_MODE=stale \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR" 2>"$ERROR_LOG"; then
  echo "stale daemon was accepted" >&2
  exit 1
fi
grep -F "daemon version 0.151.0 is running" "$ERROR_LOG" >/dev/null
grep -F "app-server daemon stop" "$ERROR_LOG" >/dev/null

if FAKE_DAEMON_MODE=missing-version \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR" 2>"$ERROR_LOG"; then
  echo "running daemon without a version was accepted" >&2
  exit 1
fi
grep -F "daemon version unknown is running" "$ERROR_LOG" >/dev/null

if FAKE_DAEMON_MODE=malformed \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR" 2>"$ERROR_LOG"; then
  echo "malformed daemon response was accepted" >&2
  exit 1
fi
grep -F "Could not parse app-server daemon status" "$ERROR_LOG" >/dev/null

SOCKET_DIR="$STATE_DIR/app-server-control"
SOCKET_PATH="$SOCKET_DIR/app-server-control.sock"
mkdir -p "$SOCKET_DIR"
python3 - "$SOCKET_PATH" <<'PY'
import socket
import sys

listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(sys.argv[1])
listener.close()
PY
FAKE_DAEMON_MODE=unavailable \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR"

rm -f -- "$SOCKET_PATH"
touch "$SOCKET_PATH"
if FAKE_DAEMON_MODE=unavailable \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR" 2>"$ERROR_LOG"; then
  echo "unverifiable daemon socket was accepted" >&2
  exit 1
fi
grep -F "Could not verify the app-server daemon" "$ERROR_LOG" >/dev/null

rm -f -- "$SOCKET_PATH"
READY_FILE="$TEST_DIR/listener.ready"
python3 - "$SOCKET_PATH" "$READY_FILE" <<'PY' &
import socket
import sys

listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(sys.argv[1])
listener.listen(1)
open(sys.argv[2], "w").close()
connection, _ = listener.accept()
connection.close()
listener.close()
PY
LISTENER_PID=$!
while [ ! -e "$READY_FILE" ]; do
  sleep 0.01
done
if FAKE_DAEMON_MODE=unavailable \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR" 2>"$ERROR_LOG"; then
  echo "reachable but unverifiable daemon socket was accepted" >&2
  exit 1
fi
wait "$LISTENER_PID"
grep -F "Could not verify the app-server daemon" "$ERROR_LOG" >/dev/null
