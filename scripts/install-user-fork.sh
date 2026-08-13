#!/bin/sh

set -eu

FORK_MARKER="+jkammerland.mcp.5"
RUSTY_V8_VERSION="150.4.0"
RUSTY_V8_PROFILE="ptrcomp_sandbox_release"
RUST_TARGET="x86_64-unknown-linux-gnu"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
BUILT_CODEX="$REPO_ROOT/codex-rs/target/release/codex"
BUILT_CODE_MODE_HOST="$REPO_ROOT/codex-rs/target/release/codex-code-mode-host"
RUSTY_V8_DIR="$REPO_ROOT/codex-rs/target/fork-rusty-v8/$RUSTY_V8_VERSION/$RUST_TARGET"
RUSTY_V8_BASE_URL="https://github.com/openai/codex/releases/download/rusty-v8-v$RUSTY_V8_VERSION"
RUSTY_V8_ARCHIVE_NAME="librusty_v8_${RUSTY_V8_PROFILE}_${RUST_TARGET}.a.gz"
RUSTY_V8_BINDING_NAME="src_binding_${RUSTY_V8_PROFILE}_${RUST_TARGET}.rs"
RUSTY_V8_CHECKSUMS_NAME="rusty_v8_${RUSTY_V8_PROFILE}_${RUST_TARGET}.sha256"

TEMP_CODEX=""
TEMP_CODE_MODE_HOST=""
ROLLBACK_CODEX=""
ROLLBACK_CODE_MODE_HOST=""
DOWNLOAD_TEMP=""

normalize_generated_release_lock() {
  status=$(git -C "$REPO_ROOT" status --short --untracked-files=all)
  [ "$status" = ' M codex-rs/Cargo.lock' ] || return 0

  changed_lines=$(git -C "$REPO_ROOT" diff --unified=0 -- codex-rs/Cargo.lock |
    sed -n -e '/^---/d' -e '/^+++/d' -e '/^[+-]/p')
  if [ -z "$changed_lines" ]; then
    echo "Generated Cargo.lock has no release-version changes; refusing to restore it." >&2
    return 1
  fi
  invalid_lines=$(printf '%s\n' "$changed_lines" |
    grep -Ev '^[+-]version = "(0\.0\.0|0\.147\.0)"$' || true)
  if [ -n "$invalid_lines" ]; then
    echo "Cargo.lock contains changes other than the generated release versions; refusing to restore it." >&2
    return 1
  fi
  git -C "$REPO_ROOT" restore --worktree -- codex-rs/Cargo.lock
}

cleanup() {
  saved_status=$?
  trap - EXIT HUP INT TERM
  for path in "$TEMP_CODEX" "$TEMP_CODE_MODE_HOST" "$ROLLBACK_CODEX" \
    "$ROLLBACK_CODE_MODE_HOST" "$DOWNLOAD_TEMP"; do
    if [ -n "$path" ] && [ -e "$path" ]; then
      rm -f -- "$path"
    fi
  done
  if ! normalize_generated_release_lock; then
    saved_status=1
  fi
  exit "$saved_status"
}

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$(id -u)" -eq 0 ]; then
  echo "Refusing to install a user-local Codex fork as root." >&2
  exit 1
fi

normalize_generated_release_lock
if [ -n "$(git -C "$REPO_ROOT" status --porcelain=v1)" ]; then
  echo "Refusing to install from a dirty fork worktree: $REPO_ROOT" >&2
  exit 1
fi

if [ "$(uname -s)" != "Linux" ] || [ "$(uname -m)" != "x86_64" ]; then
  echo "This installer currently supports only the maintained Linux x86_64 user build." >&2
  exit 1
fi

RUST_HOST=$(rustc -vV | sed -n 's/^host: //p')
if [ "$RUST_HOST" != "$RUST_TARGET" ]; then
  echo "Expected Rust host $RUST_TARGET, found $RUST_HOST." >&2
  exit 1
fi

NPM_ROOT=$(npm root --global)
INSTALLED_BIN_DIR="$NPM_ROOT/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin"
INSTALLED_CODEX="$INSTALLED_BIN_DIR/codex"
INSTALLED_CODE_MODE_HOST="$INSTALLED_BIN_DIR/codex-code-mode-host"

if [ ! -x "$INSTALLED_CODEX" ]; then
  echo "Could not find the npm-managed native Codex executable at: $INSTALLED_CODEX" >&2
  exit 1
fi

if [ ! -x "$INSTALLED_CODE_MODE_HOST" ]; then
  echo "Could not find the npm-managed code-mode host at: $INSTALLED_CODE_MODE_HOST" >&2
  exit 1
fi

mkdir -p "$RUSTY_V8_DIR"
download_v8_artifact() {
  name=$1
  destination="$RUSTY_V8_DIR/$name"
  DOWNLOAD_TEMP=$(mktemp "$destination.partial.XXXXXX")
  curl --proto '=https' --tlsv1.2 -fsSL "$RUSTY_V8_BASE_URL/$name" -o "$DOWNLOAD_TEMP"
  mv -f -- "$DOWNLOAD_TEMP" "$destination"
  DOWNLOAD_TEMP=""
}

download_v8_artifact "$RUSTY_V8_ARCHIVE_NAME"
download_v8_artifact "$RUSTY_V8_BINDING_NAME"
download_v8_artifact "$RUSTY_V8_CHECKSUMS_NAME"
if [ "$(grep -cve '^[[:space:]]*$' "$RUSTY_V8_DIR/$RUSTY_V8_CHECKSUMS_NAME")" -ne 2 ]; then
  echo "Expected exactly two rusty_v8 checksums for $RUST_TARGET." >&2
  exit 1
fi
(
  cd "$RUSTY_V8_DIR"
  tr -d '\r' < "$RUSTY_V8_CHECKSUMS_NAME" | sha256sum -c -
)

(
  cd "$REPO_ROOT/codex-rs"
  RUSTY_V8_ARCHIVE="$RUSTY_V8_DIR/$RUSTY_V8_ARCHIVE_NAME" \
    RUSTY_V8_SRC_BINDING_PATH="$RUSTY_V8_DIR/$RUSTY_V8_BINDING_NAME" \
    cargo build --release --bin codex --bin codex-code-mode-host
)
normalize_generated_release_lock

if [ ! -x "$BUILT_CODEX" ]; then
  echo "Release build did not produce: $BUILT_CODEX" >&2
  exit 1
fi

if [ ! -x "$BUILT_CODE_MODE_HOST" ]; then
  echo "Release build did not produce: $BUILT_CODE_MODE_HOST" >&2
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
UPSTREAM_CODE_MODE_HOST_BACKUP="$INSTALLED_CODE_MODE_HOST.$UPSTREAM_VERSION-upstream"
RELEASE_NAME=${FORK_MARKER#+}
PREVIOUS_CODEX="$INSTALLED_CODEX.$RELEASE_NAME-previous"
PREVIOUS_CODE_MODE_HOST="$INSTALLED_CODE_MODE_HOST.$RELEASE_NAME-previous"
CURRENT_VERSION=$($INSTALLED_CODEX --version 2>/dev/null || true)

case "$CURRENT_VERSION" in
  *"$FORK_MARKER"*) ;;
  "codex-cli $UPSTREAM_VERSION")
    if [ ! -e "$UPSTREAM_BACKUP" ]; then
      cp -p -- "$INSTALLED_CODEX" "$UPSTREAM_BACKUP"
    fi
    if [ ! -e "$UPSTREAM_CODE_MODE_HOST_BACKUP" ]; then
      cp -p -- "$INSTALLED_CODE_MODE_HOST" "$UPSTREAM_CODE_MODE_HOST_BACKUP"
    fi
    ;;
esac

case "$CURRENT_VERSION" in
  *"$FORK_MARKER"*) ;;
  *)
    if [ -e "$PREVIOUS_CODEX" ] || [ -e "$PREVIOUS_CODE_MODE_HOST" ]; then
      if [ ! -e "$PREVIOUS_CODEX" ] || [ ! -e "$PREVIOUS_CODE_MODE_HOST" ]; then
        echo "Release rollback pair is incomplete." >&2
        exit 1
      fi
      if ! cmp -s "$INSTALLED_CODEX" "$PREVIOUS_CODEX" || \
        ! cmp -s "$INSTALLED_CODE_MODE_HOST" "$PREVIOUS_CODE_MODE_HOST"; then
        echo "Release rollback pair does not match the currently installed binaries." >&2
        exit 1
      fi
    else
      cp -p -- "$INSTALLED_CODEX" "$PREVIOUS_CODEX"
      cp -p -- "$INSTALLED_CODE_MODE_HOST" "$PREVIOUS_CODE_MODE_HOST"
    fi
    ;;
esac

if [ -e "$PREVIOUS_CODEX" ] && [ -e "$PREVIOUS_CODE_MODE_HOST" ]; then
  :
elif [ -e "$PREVIOUS_CODEX" ] || [ -e "$PREVIOUS_CODE_MODE_HOST" ]; then
  echo "Release rollback pair is incomplete." >&2
  exit 1
fi

TEMP_CODEX=$(mktemp "$INSTALLED_CODEX.jkammerland.XXXXXX")
TEMP_CODE_MODE_HOST=$(mktemp "$INSTALLED_CODE_MODE_HOST.jkammerland.XXXXXX")
install -m 0755 "$BUILT_CODEX" "$TEMP_CODEX"
install -m 0755 "$BUILT_CODE_MODE_HOST" "$TEMP_CODE_MODE_HOST"
if command -v strip >/dev/null 2>&1; then
  strip "$TEMP_CODEX"
  strip "$TEMP_CODE_MODE_HOST"
fi

STAGED_VERSION=$($TEMP_CODEX --version)
case "$STAGED_VERSION" in
  *"$FORK_MARKER"*) ;;
  *)
    echo "Staged executable lost fork identity: $STAGED_VERSION" >&2
    exit 1
    ;;
esac

ROLLBACK_CODEX=$(mktemp "$INSTALLED_CODEX.rollback.XXXXXX")
ROLLBACK_CODE_MODE_HOST=$(mktemp "$INSTALLED_CODE_MODE_HOST.rollback.XXXXXX")
cp -p -- "$INSTALLED_CODEX" "$ROLLBACK_CODEX"
cp -p -- "$INSTALLED_CODE_MODE_HOST" "$ROLLBACK_CODE_MODE_HOST"

if ! mv -f -- "$TEMP_CODE_MODE_HOST" "$INSTALLED_CODE_MODE_HOST"; then
  echo "Could not activate the code-mode host." >&2
  exit 1
fi
TEMP_CODE_MODE_HOST=""
if ! mv -f -- "$TEMP_CODEX" "$INSTALLED_CODEX"; then
  restore_host=$(mktemp "$INSTALLED_CODE_MODE_HOST.restore.XXXXXX")
  cp -p -- "$ROLLBACK_CODE_MODE_HOST" "$restore_host"
  mv -f -- "$restore_host" "$INSTALLED_CODE_MODE_HOST"
  echo "Could not activate Codex; restored the previous code-mode host." >&2
  exit 1
fi
TEMP_CODEX=""

printf 'Installed %s\n' "$STAGED_VERSION"
sha256sum "$INSTALLED_CODEX" "$INSTALLED_CODE_MODE_HOST"
