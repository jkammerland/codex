# Maintained MCP lifecycle fork

This branch carries user-local Codex MCP lifecycle behavior that is intentionally
maintained separately from upstream releases.

## Identity

- Branch: `release/jkammerland.mcp.14`
- Upstream base: `60fc6995608e8188c0c9f8407d6cd98676efa247` (`origin/main`)
- User-visible suffix: `+jkammerland.mcp.14`
- Git remote for the existing personal fork: `fork`

Both the startup header and `codex --version` include the suffix. Internal
protocol and compatibility reporting retains the upstream Cargo package version.
Built-in TUI update checks treat the branded binary as a source build so they do
not prompt for an npm update that would overwrite the fork.

## Patch stack

1. An explicit MCP `tool_timeout_sec = 0` maps to no client tool-call deadline.
2. Superseded MCP transports shut down when their last owner disappears.
3. A failed call that proves its transport closed schedules a fresh transport
   for the next call without replaying the ambiguous failed request.
4. CLI and TUI version output identify this maintained build.
5. Multi-agent v2 waits sleep until mailbox or user-input activity instead of
   returning on a polling timeout.
6. Code-mode waits preserve yield reasons and absorb empty deadline yields
   without returning control to the model.
7. Unified exec provides a no-timeout `wait_process` that wakes on completion,
   queued input, or TTY output while retaining explicit bounded polling.
8. Completed unified-exec waits use a bounded post-exit output grace period, so
   a descendant-held pipe cannot orphan the wait. The output reader is released
   after that grace period while immediately available and slightly late final
   output is still retained.
9. Code mode surfaces recoverable metadata for live unified-exec sessions even
   when JavaScript emits only a projection of the nested tool result.
10. Multi-agent v2 mail is persisted before delivery, preserves FIFO ordering,
    and rejects same-ID retries whose payload or turn lineage changed.
11. Spawn admission and residency remain reserved until failed unpublished
    children have shut down and their durable state has been removed.
12. Child terminal events and exact terminal status are persisted before parent
    completion delivery; a deterministic outbox acknowledgement makes reload
    replay idempotent.
13. MCP event-stream subscription IDs remain leased until terminal forwarding
    finishes, so a same-ID replacement cannot receive a stale terminal event.
14. Resumed code-mode cells retain their originating turn environment without
    leaking hook context into a newer active turn.
15. Completion envelopes are bounded even when agent paths are unusually long.
16. User and fleet installers verify the selected checkout, active target,
    fork identity, and `codex agents` support before reporting activation.
17. Durable mail admission is pending-only. The exact bounded and stamped model
    item is committed atomically at its delivery claim. Once persistence starts,
    a cancellation shield completes the claim and state transition before a new
    turn can read history. Interrupted turns requeue uncommitted reservations in
    FIFO order, an uncertain persistence result is quarantined without replay,
    and resume reconstructs the item at that claim rather than at admission.
    Preexisting mail from `.mcp.13` remains recoverable without applying
    current-format truncation, with a hard 10K-token item cap.
18. Durable-state resume scans have explicit logical-state, physical-record, and
    record-byte limits across the complete rollout lineage. Claims preserve
    global FIFO order. A legacy `.mcp.13` claim becomes model-visible only when a
    later matching turn start proves the historical claim-before-turn sequence;
    an earlier turn start cannot complete a later claim. Compatibility shadows
    are suppressed and duplicate communication IDs are projected once. MCP event
    streams stop concurrently within one shared timeout window, and a late
    interrupted event cannot erase an already persisted terminal failure.
19. macOS fleet activation fails closed when its canonical source is dirty or
    binary-pair, staged-binary, or rollback-pair verification fails.

The terminal-status marker is intentionally model-invisible. Current builds can
recover both the dedicated marker and the overloaded `.mcp.11` metadata form.
Older binaries can skip the dedicated marker and continue reading the adjacent
raw terminal event; after a downgrade, only the exact `Error` followed by
`Interrupted` status reconstruction is lost.

The `.mcp.14` durable-mail encoding is intentionally fail-closed on downgrade.
After `.mcp.14` has written new-format durable mail to a rollout, do not resume
that rollout with `.mcp.13` or older: those binaries cannot prove whether a
pending-only record was delivered. Roll back the executable pair only before
new-format sessions are used, or upgrade back to `.mcp.14` before resuming them.

## Release layout

Each fleet release uses two commits and one immutable local source ref because a
commit cannot contain its own Git object ID:

1. The reviewed source commit contains the versioned code and installer marker.
2. `fleet/jkammerland.mcp.14-source` points exactly at that source commit and is
   the ref exported in deployment bundles.
3. A direct manifest descendant pins both the source commit and source ref in
   `fleet/release.json`; `release/jkammerland.mcp.14` points at this manifest
   commit.

Run fleet preparation and installation from the manifest commit. The controller
fetches the dedicated source ref, verifies that its tip equals the pinned source
commit, and builds that commit. The source commit is not expected to contain the
descendant manifest pin. Both hashes and the source ref belong in release-review
evidence, and nothing is pushed automatically.

## Validate

From `codex-rs`:

```bash
just fmt
just test -p codex-history
just test -p codex-rollout
just test -p codex-thread-store
just test -p codex-external-agent-migration
just test -p codex-app-server-protocol
just test -p codex-memories-write
just test -p codex-core
just test -p codex-mcp
just test -p codex-cli
just test -p codex-tui
just fix -p codex-core -p codex-history -p codex-rollout -p codex-thread-store
just fix -p codex-external-agent-migration -p codex-mcp -p codex-cli -p codex-tui
```

The focused `codex-core` MCP tests should also pass after transport changes:

```bash
just test -p codex-core --lib mcp
```

For the event-liveness hardening in this release, the focused and crate-wide
validation completed as follows before the final lint and formatting pass:

- `codex-history`: 21 passed.
- `codex-rollout`: 135 passed; one preexisting compressed-rollout search test
  could not create its fixture because of local filesystem permissions. All 11
  scanner tests passed.
- `codex-thread-store`: 244 passed.
- `codex-external-agent-migration`: 169 passed.
- `codex-app-server-protocol`: 300 passed.
- `codex-memories-write`: 44 passed.
- `codex-core`: 3,981 passed, 11 failed, and 9 skipped. Every new durable-mail,
  unbounded-MCP, and unified-exec regression passed. The failures reproduce as
  unrelated local baseline issues: two existing 30-second async timeouts, eight
  CLI stream tests whose fixture binary is unavailable, and one missing plugin
  environment variable.
- Fleet controller tests: 10 passed and the Windows-only test was skipped on
  Linux.

The final scoped `just fix` pass covered every changed Rust crate, followed by
`just fmt`. Per repository policy, tests were not rerun after that final lint and
format pass.

## Install for this user

After committing a clean, tested tree:

```bash
./scripts/install-user-fork.sh
codex --version
```

The installer builds `codex` and `codex-code-mode-host` in release mode, verifies
the fork suffix and `codex agents` support, preserves the first upstream
executables for that upstream version, stages and optionally strips the new
executables, and atomically replaces the selected managed standalone or npm
platform-native binary pair. The npm JavaScript launcher and other helper
binaries remain untouched.

## Rebase onto a later upstream release

1. Fetch upstream tags with `git fetch origin --tags`.
2. Rebase this branch onto the selected, reviewed `rust-vX.Y.Z` tag.
3. Resolve the patch stack rather than accepting either side wholesale.
4. Increment the suffix in `codex-rs/tui/src/version.rs`, the CLI test, this
   file, and `scripts/install-user-fork.sh`.
5. Run the validation commands above and review all snapshot changes.
6. Install the clean committed result and verify both the banner and
   `codex --version` before using long MCP waits.

Publishing or force-updating the personal remote branch is a separate explicit
operation; local maintenance does not push automatically.
