#!/bin/sh

# Activates the staged host before the CLI. The caller must arm its EXIT rollback before calling
# this function because a failure after the first move leaves a mixed executable pair.
activate_staged_pair() {
  if ! mv -f -- "$TEMP_CODE_MODE_HOST" "$INSTALLED_CODE_MODE_HOST"; then
    return 1
  fi
  TEMP_CODE_MODE_HOST=""
  if ! mv -f -- "$TEMP_CODEX" "$INSTALLED_CODEX"; then
    return 1
  fi
  TEMP_CODEX=""
}

# Creates the persistent pre-release rollback pair without leaving a partial pair behind.
create_release_rollback_pair() {
  if ! cp -p -- "$INSTALLED_CODEX" "$PREVIOUS_CODEX" ||
    ! cp -p -- "$INSTALLED_CODE_MODE_HOST" "$PREVIOUS_CODE_MODE_HOST"; then
    rm -f -- "$PREVIOUS_CODEX" "$PREVIOUS_CODE_MODE_HOST"
    return 1
  fi
}

# Restores both active executables from the transaction copies prepared by the
# installer. Callers retain the rollback files until their EXIT cleanup verifies
# that both active hashes match.
restore_activated_pair() {
  [ -f "$ROLLBACK_CODEX" ] && [ -f "$ROLLBACK_CODE_MODE_HOST" ] || return 1
  RESTORE_CODEX=$(mktemp "$INSTALLED_CODEX.restore.XXXXXX")
  RESTORE_CODE_MODE_HOST=$(mktemp "$INSTALLED_CODE_MODE_HOST.restore.XXXXXX")
  if ! cp -p -- "$ROLLBACK_CODEX" "$RESTORE_CODEX" ||
    ! cp -p -- "$ROLLBACK_CODE_MODE_HOST" "$RESTORE_CODE_MODE_HOST"; then
    rm -f -- "$RESTORE_CODEX" "$RESTORE_CODE_MODE_HOST"
    RESTORE_CODEX=""
    RESTORE_CODE_MODE_HOST=""
    return 1
  fi
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
