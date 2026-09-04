#!/bin/sh

# Release tags set the workspace package version while retaining the source
# lockfile's 0.0.0 package entries. Cargo normalizes those entries during a
# native build; restore only that exact generated drift.
normalize_generated_release_lock() {
  status=$(git -C "$REPO_ROOT" status --short --untracked-files=all)
  [ "$status" = ' M codex-rs/Cargo.lock' ] || return 0

  workspace_version=$(sed -n '/^\[workspace\.package\]$/,/^\[/ {
    /^version = "/ {
      s/^version = "\([^"]*\)"$/\1/
      p
      q
    }
  }' "$REPO_ROOT/codex-rs/Cargo.toml")
  if [ -z "$workspace_version" ]; then
    echo "Could not determine the Cargo workspace version; refusing to restore Cargo.lock." >&2
    return 1
  fi

  changed_lines=$(git -C "$REPO_ROOT" diff --unified=0 -- codex-rs/Cargo.lock |
    sed -n -e '/^---/d' -e '/^+++/d' -e '/^[+-]/p')
  if [ -z "$changed_lines" ]; then
    echo "Generated Cargo.lock has no release-version changes; refusing to restore it." >&2
    return 1
  fi
  invalid_lines=$(printf '%s\n' "$changed_lines" | awk -v workspace_version="$workspace_version" '
    $0 == "-version = \"0.0.0\"" { next }
    $0 == "+version = \"" workspace_version "\"" { next }
    { print }
  ')
  if [ -n "$invalid_lines" ]; then
    echo "Cargo.lock contains changes other than the generated release versions; refusing to restore it." >&2
    return 1
  fi
  git -C "$REPO_ROOT" restore --worktree -- codex-rs/Cargo.lock
}
