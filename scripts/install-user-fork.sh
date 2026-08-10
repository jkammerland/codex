#!/bin/sh

set -eu

FORK_MARKER="+jkammerland.mcp.1"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
BUILT_CODEX="$REPO_ROOT/codex-rs/target/release/codex"

if [ "$(id -u)" -eq 0 ]; then
  echo "Refusing to install a user-local Codex fork as root." >&2
  exit 1
fi

if [ -n "$(git -C "$REPO_ROOT" status --porcelain=v1)" ]; then
  echo "Refusing to install from a dirty fork worktree: $REPO_ROOT" >&2
  exit 1
fi

if [ "$(uname -s)" != "Linux" ] || [ "$(uname -m)" != "x86_64" ]; then
  echo "This installer currently supports only the maintained Linux x86_64 user build." >&2
  exit 1
fi

NPM_ROOT=$(npm root --global)
INSTALLED_CODEX="$NPM_ROOT/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex"

if [ ! -x "$INSTALLED_CODEX" ]; then
  echo "Could not find the npm-managed native Codex executable at: $INSTALLED_CODEX" >&2
  exit 1
fi

(
  cd "$REPO_ROOT/codex-rs"
  cargo build --release --bin codex
)

if [ ! -x "$BUILT_CODEX" ]; then
  echo "Release build did not produce: $BUILT_CODEX" >&2
  exit 1
fi

BUILT_VERSION=$($BUILT_CODEX --version)
case "$BUILT_VERSION" in
  *"$FORK_MARKER"*) ;;
  *)
    echo "Release build is missing fork identity: $BUILT_VERSION" >&2
    exit 1
    ;;
esac

UPSTREAM_VERSION=$(printf '%s\n' "$BUILT_VERSION" | sed -E 's/^codex-cli ([^+]+).*/\1/')
UPSTREAM_BACKUP="$INSTALLED_CODEX.$UPSTREAM_VERSION-upstream"
CURRENT_VERSION=$($INSTALLED_CODEX --version 2>/dev/null || true)

case "$CURRENT_VERSION" in
  *"$FORK_MARKER"*) ;;
  *)
    if [ ! -e "$UPSTREAM_BACKUP" ]; then
      cp -p -- "$INSTALLED_CODEX" "$UPSTREAM_BACKUP"
    fi
    ;;
esac

TEMP_CODEX=$(mktemp "$INSTALLED_CODEX.jkammerland.XXXXXX")
cleanup() {
  if [ -n "${TEMP_CODEX:-}" ] && [ -e "$TEMP_CODEX" ]; then
    rm -f -- "$TEMP_CODEX"
  fi
}
trap cleanup EXIT HUP INT TERM

install -m 0755 "$BUILT_CODEX" "$TEMP_CODEX"
if command -v strip >/dev/null 2>&1; then
  strip "$TEMP_CODEX"
fi

STAGED_VERSION=$($TEMP_CODEX --version)
case "$STAGED_VERSION" in
  *"$FORK_MARKER"*) ;;
  *)
    echo "Staged executable lost fork identity: $STAGED_VERSION" >&2
    exit 1
    ;;
esac

mv -f -- "$TEMP_CODEX" "$INSTALLED_CODEX"
TEMP_CODEX=""
trap - EXIT HUP INT TERM

printf 'Installed %s\n' "$STAGED_VERSION"
sha256sum "$INSTALLED_CODEX"
