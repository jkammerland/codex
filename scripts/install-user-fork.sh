#!/bin/sh

set -eu

FORK_MARKER="+jkammerland.mcp.11"
RUSTY_V8_VERSION="150.4.0"
RUSTY_V8_PROFILE="ptrcomp_sandbox_release"
RUST_TARGET="x86_64-unknown-linux-gnu"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
. "$SCRIPT_DIR/lib/install-user-fork-paths.sh"
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

restore_activated_pair() {
  restore_codex=$(mktemp "$INSTALLED_CODEX.restore.XXXXXX")
  restore_host=$(mktemp "$INSTALLED_CODE_MODE_HOST.restore.XXXXXX")
  cp -p -- "$ROLLBACK_CODEX" "$restore_codex"
  cp -p -- "$ROLLBACK_CODE_MODE_HOST" "$restore_host"
  mv -f -- "$restore_host" "$INSTALLED_CODE_MODE_HOST"
  mv -f -- "$restore_codex" "$INSTALLED_CODEX"
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

COMMAND_CODEX=$(command -v codex || true)
CODEX_STATE_DIR=${CODEX_HOME:-"$HOME/.codex"}
if INSTALLED_BIN_DIR=$(resolve_standalone_codex_bin_dir "$COMMAND_CODEX" "$CODEX_STATE_DIR"); then
  INSTALL_KIND=standalone
elif is_active_managed_standalone_codex "$COMMAND_CODEX" "$CODEX_STATE_DIR"; then
  echo "The active standalone Codex install is incomplete; refusing to update a different installation." >&2
  exit 1
else
  NPM_ROOT=$(npm root --global 2>/dev/null || true)
  if ! INSTALLED_BIN_DIR=$(resolve_npm_codex_bin_dir "$COMMAND_CODEX" "$NPM_ROOT"); then
    echo "The active Codex executable is not the managed standalone or npm installation; refusing to update an inactive install." >&2
    exit 1
  fi
  INSTALL_KIND=npm
fi
INSTALLED_CODEX="$INSTALLED_BIN_DIR/codex"
INSTALLED_CODE_MODE_HOST="$INSTALLED_BIN_DIR/codex-code-mode-host"

if [ ! -x "$INSTALLED_CODEX" ]; then
  echo "Could not find the active standalone or npm-managed native Codex executable at: $INSTALLED_CODEX" >&2
  exit 1
fi

if [ ! -x "$INSTALLED_CODE_MODE_HOST" ]; then
  echo "Could not find the active standalone or npm-managed code-mode host at: $INSTALLED_CODE_MODE_HOST" >&2
  exit 1
fi

SELECTED_CODEX_HASH=$(sha256sum "$INSTALLED_CODEX" | awk '{print $1}')
SELECTED_CODE_MODE_HOST_HASH=$(sha256sum "$INSTALLED_CODE_MODE_HOST" | awk '{print $1}')

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

ACTIVE_CODEX_BEFORE=$(command -v codex || true)
if [ "$ACTIVE_CODEX_BEFORE" != "$COMMAND_CODEX" ]; then
  echo "The active Codex command changed during the build; refusing activation." >&2
  exit 1
fi
case "$INSTALL_KIND" in
  standalone)
    ACTIVE_BIN_DIR_BEFORE=$(resolve_standalone_codex_bin_dir "$ACTIVE_CODEX_BEFORE" "$CODEX_STATE_DIR" || true)
    ;;
  npm)
    ACTIVE_BIN_DIR_BEFORE=$(resolve_npm_codex_bin_dir "$ACTIVE_CODEX_BEFORE" "$NPM_ROOT" || true)
    ;;
esac
if [ "$ACTIVE_BIN_DIR_BEFORE" != "$INSTALLED_BIN_DIR" ]; then
  echo "The active Codex installation target changed during the build; refusing activation." >&2
  exit 1
fi
if [ "$(sha256sum "$INSTALLED_CODEX" | awk '{print $1}')" != "$SELECTED_CODEX_HASH" ] || \
  [ "$(sha256sum "$INSTALLED_CODE_MODE_HOST" | awk '{print $1}')" != "$SELECTED_CODE_MODE_HOST_HASH" ]; then
  echo "The selected Codex binaries changed during the build; refusing activation." >&2
  exit 1
fi

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
if ! "$TEMP_CODEX" agents --help >/dev/null 2>&1; then
  echo "Staged executable does not support codex agents." >&2
  exit 1
fi

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

ACTIVE_CODEX_AFTER=$(command -v codex || true)
if [ "$ACTIVE_CODEX_AFTER" != "$COMMAND_CODEX" ]; then
  restore_activated_pair
  echo "The active Codex command changed during installation; refusing to report successful activation." >&2
  exit 1
fi
case "$INSTALL_KIND" in
  standalone)
    ACTIVE_BIN_DIR_AFTER=$(resolve_standalone_codex_bin_dir "$ACTIVE_CODEX_AFTER" "$CODEX_STATE_DIR" || true)
    ;;
  npm)
    ACTIVE_BIN_DIR_AFTER=$(resolve_npm_codex_bin_dir "$ACTIVE_CODEX_AFTER" "$NPM_ROOT" || true)
    ;;
esac
if [ "$ACTIVE_BIN_DIR_AFTER" != "$INSTALLED_BIN_DIR" ]; then
  restore_activated_pair
  echo "The active Codex installation target changed during installation; restored the selected target." >&2
  exit 1
fi
ACTIVE_VERSION=$(codex --version 2>/dev/null || true)
case "$ACTIVE_VERSION" in
  *"$FORK_MARKER"*) ;;
  *)
    restore_activated_pair
    echo "The active Codex command did not select the installed fork: $ACTIVE_VERSION" >&2
    exit 1
    ;;
esac
if ! codex agents --help >/dev/null 2>&1; then
  restore_activated_pair
  echo "The active fork does not support codex agents; restored the selected target." >&2
  exit 1
fi

printf 'Installed %s\n' "$STAGED_VERSION"
sha256sum "$INSTALLED_CODEX" "$INSTALLED_CODE_MODE_HOST"
