#!/bin/sh

set -eu

FORK_MARKER="+jkammerland.mcp.16"
RUSTY_V8_VERSION="150.4.0"
RUSTY_V8_PROFILE="ptrcomp_sandbox_release"
RUST_TARGET="x86_64-unknown-linux-gnu"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
RELEASE_MANIFEST="$REPO_ROOT/fleet/release.json"
. "$SCRIPT_DIR/lib/install-user-fork-paths.sh"
. "$SCRIPT_DIR/lib/install-user-fork-daemon.sh"
. "$SCRIPT_DIR/lib/install-user-fork-transaction.sh"
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
RESTORE_CODEX=""
RESTORE_CODE_MODE_HOST=""
DOWNLOAD_TEMP=""
ACTIVATION_PENDING=0
KEEP_ROLLBACK=0
ROLLBACK_CODEX_HASH=""
ROLLBACK_CODE_MODE_HOST_HASH=""

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
  if [ "$ACTIVATION_PENDING" -eq 1 ] && ! restore_activated_pair; then
    KEEP_ROLLBACK=1
    saved_status=1
    echo "Activation rollback incomplete; recovery copies: $ROLLBACK_CODEX, $ROLLBACK_CODE_MODE_HOST" >&2
  fi
  for path in "$TEMP_CODEX" "$TEMP_CODE_MODE_HOST" "$ROLLBACK_CODEX" \
    "$ROLLBACK_CODE_MODE_HOST" "$RESTORE_CODEX" "$RESTORE_CODE_MODE_HOST" \
    "$DOWNLOAD_TEMP"; do
    if [ "$KEEP_ROLLBACK" -eq 1 ] && \
      { [ "$path" = "$ROLLBACK_CODEX" ] || [ "$path" = "$ROLLBACK_CODE_MODE_HOST" ]; }; then
      continue
    fi
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
if [ ! -f "$RELEASE_MANIFEST" ] || ! command -v python3 >/dev/null 2>&1; then
  echo "The checked-in fleet release manifest and python3 are required for source verification." >&2
  exit 1
fi
EXPECTED_FORK_COMMIT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["release"]["forkCommit"])' "$RELEASE_MANIFEST")
EXPECTED_CLI_VERSION=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["release"]["expectedCliVersion"])' "$RELEASE_MANIFEST")
EXPECTED_RELEASE_NAME=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["release"]["name"])' "$RELEASE_MANIFEST")
if [ "$FORK_MARKER" != "+$EXPECTED_RELEASE_NAME" ]; then
  echo "Installer marker does not match the fleet release manifest." >&2
  exit 1
fi
if ! git -C "$REPO_ROOT" cat-file -e "$EXPECTED_FORK_COMMIT^{commit}" || \
  ! git -C "$REPO_ROOT" merge-base --is-ancestor "$EXPECTED_FORK_COMMIT" HEAD || \
  ! git -C "$REPO_ROOT" diff --quiet "$EXPECTED_FORK_COMMIT" -- \
    codex-rs scripts/install-user-fork.sh scripts/lib/install-user-fork-paths.sh \
    scripts/lib/install-user-fork-daemon.sh scripts/lib/install-user-fork-transaction.sh; then
  echo "Fork source does not match the reviewed fleet release commit: $EXPECTED_FORK_COMMIT" >&2
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

EXPECTED_RUNTIME_VERSION=${EXPECTED_CLI_VERSION#codex-cli }
ensure_daemon_runtime_compatible "$INSTALLED_CODEX" "$EXPECTED_RUNTIME_VERSION" "$CODEX_STATE_DIR"

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
if [ "$BUILT_VERSION" != "$EXPECTED_CLI_VERSION" ]; then
  echo "Release build identity mismatch: $BUILT_VERSION" >&2
  exit 1
fi

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

RELEASE_NAME=${FORK_MARKER#+}
PREVIOUS_CODEX="$INSTALLED_CODEX.$RELEASE_NAME-previous"
PREVIOUS_CODE_MODE_HOST="$INSTALLED_CODE_MODE_HOST.$RELEASE_NAME-previous"
CURRENT_VERSION=$($INSTALLED_CODEX --version 2>/dev/null || true)

case "$CURRENT_VERSION" in
  *"$FORK_MARKER"*) ;;
  "codex-cli "*+*) ;;
  "codex-cli "*)
    UPSTREAM_VERSION=$(printf '%s\n' "$CURRENT_VERSION" | sed -E 's/^codex-cli ([^+]+).*/\1/')
    UPSTREAM_BACKUP="$INSTALLED_CODEX.$UPSTREAM_VERSION-upstream"
    UPSTREAM_CODE_MODE_HOST_BACKUP="$INSTALLED_CODE_MODE_HOST.$UPSTREAM_VERSION-upstream"
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
      if ! create_release_rollback_pair; then
        echo "Failed to create the release rollback pair." >&2
        exit 1
      fi
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
if [ "$STAGED_VERSION" != "$EXPECTED_CLI_VERSION" ]; then
  echo "Staged executable identity mismatch: $STAGED_VERSION" >&2
  exit 1
fi
if ! "$TEMP_CODEX" agents --help >/dev/null 2>&1; then
  echo "Staged executable does not support codex agents." >&2
  exit 1
fi

ROLLBACK_CODEX=$(mktemp "$INSTALLED_CODEX.rollback.XXXXXX")
ROLLBACK_CODE_MODE_HOST=$(mktemp "$INSTALLED_CODE_MODE_HOST.rollback.XXXXXX")
cp -p -- "$INSTALLED_CODEX" "$ROLLBACK_CODEX"
cp -p -- "$INSTALLED_CODE_MODE_HOST" "$ROLLBACK_CODE_MODE_HOST"
ROLLBACK_CODEX_HASH=$(sha256sum "$ROLLBACK_CODEX" | awk '{print $1}')
ROLLBACK_CODE_MODE_HOST_HASH=$(sha256sum "$ROLLBACK_CODE_MODE_HOST" | awk '{print $1}')
ACTIVATION_PENDING=1

if ! activate_staged_pair; then
  echo "Could not activate the executable pair; the exit transaction will restore both binaries." >&2
  exit 1
fi

ACTIVE_CODEX_AFTER=$(command -v codex || true)
if [ "$ACTIVE_CODEX_AFTER" != "$COMMAND_CODEX" ]; then
  echo "The active Codex command changed during installation; the exit transaction will restore the selected target." >&2
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
  echo "The active Codex installation target changed during installation; the exit transaction will restore the selected target." >&2
  exit 1
fi
ACTIVE_VERSION=$(codex --version 2>/dev/null || true)
if [ "$ACTIVE_VERSION" != "$EXPECTED_CLI_VERSION" ]; then
  echo "The active Codex command did not select the exact installed fork: $ACTIVE_VERSION" >&2
  exit 1
fi
if ! codex agents --help >/dev/null 2>&1; then
  echo "The active fork does not support codex agents; the exit transaction will restore the selected target." >&2
  exit 1
fi

ensure_daemon_runtime_compatible "$INSTALLED_CODEX" "$EXPECTED_RUNTIME_VERSION" "$CODEX_STATE_DIR"

ACTIVATION_PENDING=0
printf 'Installed %s\n' "$STAGED_VERSION"
sha256sum "$INSTALLED_CODEX" "$INSTALLED_CODE_MODE_HOST"
