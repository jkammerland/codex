#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$SCRIPT_DIR/../lib/install-user-fork-paths.sh"
. "$SCRIPT_DIR/../lib/install-user-fork-transaction.sh"

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

SECOND_RELEASE_ROOT="$STATE_DIR/packages/standalone/releases/0.152.0"
SECOND_RELEASE_BIN="$SECOND_RELEASE_ROOT/bin"
mkdir -p "$SECOND_RELEASE_BIN"
touch "$SECOND_RELEASE_BIN/codex" "$SECOND_RELEASE_BIN/codex-code-mode-host"
chmod +x "$SECOND_RELEASE_BIN/codex" "$SECOND_RELEASE_BIN/codex-code-mode-host"
ln -sfn "$SECOND_RELEASE_ROOT" "$(dirname -- "$CURRENT_BIN")"
resolved_after_retarget=$(resolve_standalone_codex_bin_dir "$CURRENT_BIN/codex" "$STATE_DIR")
[ "$resolved_after_retarget" = "$SECOND_RELEASE_BIN" ]
[ "$resolved_after_retarget" != "$RELEASE_BIN" ]

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

TRANSACTION_BIN="$TEST_DIR/transaction/bin"
mkdir -p "$TRANSACTION_BIN"
INSTALLED_CODEX="$TRANSACTION_BIN/codex"
INSTALLED_CODE_MODE_HOST="$TRANSACTION_BIN/codex-code-mode-host"
ROLLBACK_CODEX="$TRANSACTION_BIN/codex.rollback"
ROLLBACK_CODE_MODE_HOST="$TRANSACTION_BIN/codex-code-mode-host.rollback"
RESTORE_CODEX=""
RESTORE_CODE_MODE_HOST=""
printf 'old-cli\n' >"$ROLLBACK_CODEX"
printf 'old-host\n' >"$ROLLBACK_CODE_MODE_HOST"
printf 'new-cli\n' >"$INSTALLED_CODEX"
printf 'new-host\n' >"$INSTALLED_CODE_MODE_HOST"
ROLLBACK_CODEX_HASH=$(sha256sum "$ROLLBACK_CODEX" | awk '{print $1}')
ROLLBACK_CODE_MODE_HOST_HASH=$(sha256sum "$ROLLBACK_CODE_MODE_HOST" | awk '{print $1}')
ACTIVATION_PENDING=1

restore_activated_pair

[ "$ACTIVATION_PENDING" -eq 0 ]
cmp -s "$INSTALLED_CODEX" "$ROLLBACK_CODEX"
cmp -s "$INSTALLED_CODE_MODE_HOST" "$ROLLBACK_CODE_MODE_HOST"

rm -f "$ROLLBACK_CODE_MODE_HOST"
ACTIVATION_PENDING=1
if restore_activated_pair; then
  echo "incomplete rollback pair was accepted" >&2
  exit 1
fi
[ "$ACTIVATION_PENDING" -eq 1 ]
