#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$SCRIPT_DIR/../lib/install-user-fork-paths.sh"

TEST_DIR=$(mktemp -d)
trap 'rm -rf -- "$TEST_DIR"' EXIT HUP INT TERM

STATE_DIR="$TEST_DIR/state"
RELEASE_ROOT="$STATE_DIR/packages/standalone/releases/0.151.0"
RELEASE_BIN="$RELEASE_ROOT/bin"
CURRENT_BIN="$STATE_DIR/packages/standalone/current/bin"
UNRELATED_BIN="$TEST_DIR/unrelated/bin"
mkdir -p "$RELEASE_BIN" "$UNRELATED_BIN"
touch "$RELEASE_BIN/codex" "$RELEASE_BIN/codex-code-mode-host"
touch "$UNRELATED_BIN/codex" "$UNRELATED_BIN/codex-code-mode-host"
chmod +x "$RELEASE_BIN/codex" "$RELEASE_BIN/codex-code-mode-host"
chmod +x "$UNRELATED_BIN/codex" "$UNRELATED_BIN/codex-code-mode-host"
mkdir -p "$(dirname -- "$(dirname -- "$CURRENT_BIN")")"
ln -s "$RELEASE_ROOT" "$(dirname -- "$CURRENT_BIN")"

resolved=$(resolve_standalone_codex_bin_dir "$CURRENT_BIN/codex" "$STATE_DIR")
[ "$resolved" = "$RELEASE_BIN" ]

if resolve_standalone_codex_bin_dir "$UNRELATED_BIN/codex" "$STATE_DIR" >/dev/null; then
  echo "unrelated PATH executable was accepted as the managed standalone install" >&2
  exit 1
fi
