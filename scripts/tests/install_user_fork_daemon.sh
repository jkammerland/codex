#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$SCRIPT_DIR/../lib/install-user-fork-daemon.sh"

TEST_DIR=$(mktemp -d)
trap 'rm -rf -- "$TEST_DIR"' EXIT HUP INT TERM

FAKE_CODEX="$TEST_DIR/codex"
cat >"$FAKE_CODEX" <<'EOF'
#!/bin/sh

case "${FAKE_DAEMON_MODE:-not-running}" in
  not-running)
    printf '%s\n' '{"status":"notRunning","appServerVersion":null}'
    ;;
  matching)
    printf '%s\n' '{"status":"running","appServerVersion":"0.0.0+jkammerland.mcp.17"}'
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

EXPECTED_VERSION="0.0.0+jkammerland.mcp.17"
STATE_DIR="$TEST_DIR/state"
ERROR_LOG="$TEST_DIR/error.log"

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

mkdir -p "$STATE_DIR/app-server-control"
touch "$STATE_DIR/app-server-control/app-server-control.sock"
if FAKE_DAEMON_MODE=unavailable \
  ensure_daemon_runtime_compatible "$FAKE_CODEX" "$EXPECTED_VERSION" "$STATE_DIR" 2>"$ERROR_LOG"; then
  echo "unverifiable daemon socket was accepted" >&2
  exit 1
fi
grep -F "Could not verify the app-server daemon" "$ERROR_LOG" >/dev/null
