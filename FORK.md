# Maintained MCP lifecycle fork

## MCP.21 alpha.8 candidate (2026-09-20)

This candidate is a minimal rebase of the maintained fork onto the annotated
`rust-v0.156.0-alpha.8` tag object
`0783e3270fece607ff0f2c4024025a558282581d`, peeled commit
`da41ff285c7e3f627dfcfef0588a30b3b7d91617`. The candidate CLI identity is
`codex-cli 0.156.0-alpha.8+jkammerland.mcp.21`; the compatible MCP server pin is
`f75bfd7cd174ee9cbcf732c966d5e129cc7605f0`, with Rusty V8 `150.4.0`.

This is a source and staging handoff only. No release build, installation,
activation, daemon/session stop, remote write, or native Mac/Windows validation
was performed here. Activation remains separately unapproved.

## Maintained behavior

The rebase preserves unbounded configured MCP waits, quiet-wait coalescing,
event-driven process waits, recoverable original exec handles, originating
code-mode step/runtime/hook/notification ownership, active transport leases,
explicit interruption/shutdown, genuine transport-loss reporting, and the
fork-compatible daemon runtime guard. Upstream's separate app-server-daemon
package layout and complete-package validation remain authoritative.

Raw two-binary runtime package materialization is intentionally not part of this
candidate. A fresh bare binary without a complete `codex-package.json` package
must retain upstream's explicit missing-package error. Future staging must
produce and select a complete package outside product runtime, with the actual
`x86_64-unknown-linux-gnu` GNU target; the official Linux package metadata may
describe musl and cannot be globally relaxed. Future activation must reconcile
the complete staged daemon package and is not approved by this handoff.

## Focused evidence and review disposition

The final fixture correction makes the complete daemon package test copy Linux
`bwrap` only on Linux, and makes reconnect fixture server versions explicit:
remote tests retain `reconnect-test/2.0.0`, while the local-daemon fixture uses
the candidate CLI identity. The previously reported active-reconnect failure
was therefore an owned shared-fixture mismatch, not an upstream/environment
failure. Its exact test now passes without changing remote assertions or
snapshots.

Focused evidence from this worktree, with the verified Rusty V8 cache supplied
through `RUSTY_V8_ARCHIVE` and `RUSTY_V8_SRC_BINDING_PATH`:

- TUI fixture correction library selectors: 7 passed, 4,938 skipped;
  `tui-fixture-correction-lib.log`.
- Complete daemon package compatibility selector: 1 passed, 20 skipped;
  `tui-fixture-correction-daemon.log`.
- Earlier focused matrix evidence retained from this candidate includes 7
  live-session/dispatch-liveness tests passed, 4,567 skipped, and 23 MCP
  refresh/reconciliation/interrupt/shutdown tests passed, 223 skipped.
- Fleet Python fixture suite: exactly 11 passed, 1 skipped, 12 total.
- The V8 archive checksum is
  `a35c75d1f26e6a983885a45b33490a4ebe54f05050568b32b89cfb421b30b583`; the
  source binding checksum is
  `7727826ae479bdb645e807239fb12d1f8e2e23de7a6cf16f5ee592690d1d8506`.

Historical broad rerun evidence is not release qualification: the prior core
rerun recorded 4,548 tests and 18 failures in
`test-codex-core-rerun2.log`; `test-codex-core.log` was a compile failure, not
test evidence. An earlier TUI run recorded 47 failures. The active-reconnect
fixture failure is resolved above; other historical failures remain explicitly
unclassified and were not fixed or waived here.

The focused checks did not run the full workspace suite. Scoped formatting and
lint checks are required at the final checkpoint; dependency files were not
changed, so no additional Bazel lock refresh is required.

## Fleet metadata and deferred hosts

The manifest describes desired candidate staging, not installed state. Fresh
Mac source/build roots are:

```text
/Users/ai-dev/repos/codex-mcp-fork-release-source-mcp21-alpha8
/Users/ai-dev/repos/codex-mcp-fork-build-mcp21-alpha8-20260920
```

The existing Mac installed CLI and paired-host paths remain unchanged. The
verified Windows worker is `WIN11-offload`, with source/build roots
`C:\codex-worker\repos\codex-mcp-fork-release-source-mcp21-alpha8` and
`C:\codex-worker\repos\codex-mcp-fork-build-mcp21-alpha8-20260920`, and server
root `C:\Users\codex-worker\repos\codex-mcp-server`. Its observed installed
pair is under
`C:\Users\codex-worker\.codex\releases\jkammerland.mcp.20\`; Windows is
deferred because OAuth is revoked and Rust was not found. No host was contacted
and these observations are not a native validation or installed-state claim.

The recommended future Linux staged package root is:

```text
/home/ai-dev1/.codex/fleet-staging/jkammerland.mcp.21/linux-x86_64-gnu/package
```

That root must contain a verified complete package before any future activation
review. This candidate does not provision it.

## Source and manifest refs

The reviewed source ref is `fleet/jkammerland.mcp.21-source`. A direct
descendant manifest ref, `release/jkammerland.mcp.21`, pins the exact source
commit and the compatible MCP server commit. The source and manifest commits
are local only; nothing is pushed automatically.

## Scope

Only demonstrated waiting, recovery, daemon compatibility, packaging-fixture,
and release-identity adaptations belong in this candidate. No blanket context
caps, polling, elapsed-time cancellation, admission quotas, replay, lifecycle
rewrite, controller architecture, or production raw materializer is included.

Installed binaries remain untouched. Any future activation requires a separate
reviewed complete-package staging receipt and explicit activation approval.
