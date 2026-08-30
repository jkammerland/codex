#!/bin/sh

# Restores both active executables from the transaction copies prepared by the
# installer. Callers retain the rollback files until their EXIT cleanup verifies
# that both active hashes match.
restore_activated_pair() {
  [ -f "$ROLLBACK_CODEX" ] && [ -f "$ROLLBACK_CODE_MODE_HOST" ] || return 1
  RESTORE_CODEX=$(mktemp "$INSTALLED_CODEX.restore.XXXXXX")
  RESTORE_CODE_MODE_HOST=$(mktemp "$INSTALLED_CODE_MODE_HOST.restore.XXXXXX")
  cp -p -- "$ROLLBACK_CODEX" "$RESTORE_CODEX"
  cp -p -- "$ROLLBACK_CODE_MODE_HOST" "$RESTORE_CODE_MODE_HOST"
  rollback_failed=0
  if ! mv -f -- "$RESTORE_CODE_MODE_HOST" "$INSTALLED_CODE_MODE_HOST"; then
    rollback_failed=1
  fi
  RESTORE_CODE_MODE_HOST=""
  if ! mv -f -- "$RESTORE_CODEX" "$INSTALLED_CODEX"; then
    rollback_failed=1
  fi
  RESTORE_CODEX=""
  [ "$(sha256sum "$INSTALLED_CODEX" | awk '{print $1}')" = "$ROLLBACK_CODEX_HASH" ] || rollback_failed=1
  [ "$(sha256sum "$INSTALLED_CODE_MODE_HOST" | awk '{print $1}')" = "$ROLLBACK_CODE_MODE_HOST_HASH" ] || rollback_failed=1
  if [ "$rollback_failed" -eq 0 ]; then
    ACTIVATION_PENDING=0
    return 0
  fi
  return 1
}
