#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TEST_DIR=$(mktemp -d)
trap 'rm -rf -- "$TEST_DIR"' EXIT HUP INT TERM

. "$SCRIPT_DIR/../lib/install-user-fork-release-lock.sh"

init_fixture() {
  REPO_ROOT=$1
  mkdir -p "$REPO_ROOT/codex-rs"
  cat >"$REPO_ROOT/codex-rs/Cargo.toml" <<'EOF'
[workspace.package]
version = "0.154.0-alpha.3"
EOF
  cat >"$REPO_ROOT/codex-rs/Cargo.lock" <<'EOF'
[[package]]
name = "codex-core"
version = "0.0.0"
EOF
  git -C "$REPO_ROOT" init -q
  git -C "$REPO_ROOT" -c user.name='Fleet Test' -c user.email=fleet@example.invalid add .
  git -C "$REPO_ROOT" -c user.name='Fleet Test' -c user.email=fleet@example.invalid commit -qm baseline
}

GENERATED_REPO="$TEST_DIR/generated"
init_fixture "$GENERATED_REPO"
sed -i.bak 's/version = "0.0.0"/version = "0.154.0-alpha.3"/' "$GENERATED_REPO/codex-rs/Cargo.lock"
rm -f -- "$GENERATED_REPO/codex-rs/Cargo.lock.bak"
REPO_ROOT=$GENERATED_REPO normalize_generated_release_lock
git -C "$GENERATED_REPO" diff --quiet

UNRELATED_REPO="$TEST_DIR/unrelated"
init_fixture "$UNRELATED_REPO"
printf 'checksum = "unexpected"\n' >>"$UNRELATED_REPO/codex-rs/Cargo.lock"
if REPO_ROOT=$UNRELATED_REPO normalize_generated_release_lock 2>"$TEST_DIR/error.log"; then
  echo "unrelated Cargo.lock drift was accepted" >&2
  exit 1
fi
grep -F "changes other than the generated release versions" "$TEST_DIR/error.log" >/dev/null
grep -F 'checksum = "unexpected"' "$UNRELATED_REPO/codex-rs/Cargo.lock" >/dev/null
