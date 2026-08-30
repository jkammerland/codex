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
NPM_ROOT="$TEST_DIR/npm/lib/node_modules"
NPM_PACKAGE_ROOT="$NPM_ROOT/@openai/codex"
NPM_LAUNCHER="$NPM_PACKAGE_ROOT/bin/codex.js"
NPM_BIN="$NPM_PACKAGE_ROOT/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin"
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

rm -f "$RELEASE_BIN/codex-code-mode-host"
if resolve_standalone_codex_bin_dir "$CURRENT_BIN/codex" "$STATE_DIR" >/dev/null; then
  echo "incomplete standalone pair was accepted" >&2
  exit 1
fi
is_active_managed_standalone_codex "$CURRENT_BIN/codex" "$STATE_DIR"

mkdir -p "$(dirname -- "$NPM_LAUNCHER")" "$NPM_BIN" "$TEST_DIR/npm/bin"
touch "$NPM_LAUNCHER" "$NPM_BIN/codex" "$NPM_BIN/codex-code-mode-host"
chmod +x "$NPM_LAUNCHER" "$NPM_BIN/codex" "$NPM_BIN/codex-code-mode-host"
ln -s "$NPM_LAUNCHER" "$TEST_DIR/npm/bin/codex"

resolved=$(resolve_npm_codex_bin_dir "$TEST_DIR/npm/bin/codex" "$NPM_ROOT")
[ "$resolved" = "$NPM_BIN" ]

if resolve_npm_codex_bin_dir "$UNRELATED_BIN/codex" "$NPM_ROOT" >/dev/null; then
  echo "unrelated PATH executable was accepted as the npm-managed install" >&2
  exit 1
fi
