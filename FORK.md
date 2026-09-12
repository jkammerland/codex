# Maintained MCP lifecycle fork

## Unreleased main rebase candidate (2026-09-12)

Branch: `rebase/minimal-main-20260912`.
Pinned upstream: `c4017a87aacc7558002b7cb510025e967c1d765e`.
Development identity: `0.0.0+jkammerland.mcp.20-dev` (upstream main uses `0.0.0`).
This is not a release or an approved installation. The fleet manifest and installer below
still describe deployed `.mcp.19`; do not use them to install this candidate.

The rebase adopts upstream per-execution delegates and originating `StepContext` instead of
the former fork dispatcher implementation. A smaller adaptation keeps dispatch alive between
turns and retains each cell's original execution runtime/diff tracker until cell closure.
Unbounded MCP timeout mapping, quiet wait coalescing, and recoverable exec handles remain.
Notifications retain upstream originating output metadata but still reject delivery into an
unrelated newer turn, preserving the fork's existing isolation behavior.

Focused Linux qualification passed: affected crates and their test code compile; 549 supporting
crate tests, 16 selected core integration tests, and 12 selected core unit tests pass after
rebase-specific fixture corrections. This includes paused-time direct/nested MCP waits beyond
one day, quiet wait coalescing, idle dispatch, original-step ownership, process-handle recovery,
stdin/wait races, and rejection of an incompatible daemon replacement before stopping it.

An initial apparent integration pass was excluded: the offload environment supplied a zero-valued
network-disabled marker, but the existing test helper skips based on its presence. Verified
loopback-capable test subprocesses were rerun without that erroneous marker; sandbox source and
global settings were not changed. Notification coverage also accounts for logged delivery errors
and multi-item wait outputs, rather than expecting rejected notifications to fail the cell.

Scoped `just fix`, `just bazel-lock-update` (no lockfile delta), and `just fmt` passed. Tests were
not rerun after these final mechanical checks. One non-fatal Clippy warning remains for the
eight-argument code-mode-host `drive_cell` function. Evidence is retained in the controller's
`20260912-main-rebase-validation.json` fleet ledger and its sibling artifact directories.

No full-workspace suite, new 1,200-second live wait, release build, or native Mac/Windows
qualification was run for this candidate. Linux/Mac activation still requires operator approval.
Windows is unavailable and has not been contacted. Existing installations are unchanged.
The historical release details below describe `.mcp.19`, not this candidate.

This branch carries user-local Codex lifecycle behavior maintained separately
from upstream releases.

## Identity

- Release: `jkammerland.mcp.19`, rebuilt from the deployed `.mcp.18` baseline.
- Upstream base: `rust-v0.154.0-alpha.4`.
- User-visible suffix: `+jkammerland.mcp.19`
- Personal Git remote: `fork`

The startup header, `codex --version`, app-server handshake, daemon lifecycle,
and app-server client metadata share the suffix. Wire schemas remain compatible
with the upstream Cargo workspace version.

The maintainer approved native release builds and fleet deployment on 2026-09-05 with the
documented shell-snapshot test timeout still unresolved. Build and activation evidence are
recorded per host; this approval does not expand the production patch scope.

## Maintenance scope

Only demonstrated waiting/recovery fixes belong in this candidate. New blanket context caps,
history projections, admission quotas, and the abandoned fleet/lifecycle hardening rewrite are
not carried forward. Upstream limits and explicit user-selected output budgets are unchanged.
The abandoned source is preserved on `archive/mcp19-hardening-20260905`; the original worktree
also remains untouched.

## Maintained behavior

1. `tool_timeout_sec = 0` creates no MCP tool-call deadline while omitted and
   positive values retain their finite behavior.
2. Active MCP calls lease their transport; superseding a connection cannot
   terminate or replay an ambiguous active request.
3. Unified exec waits are event-driven and preserve bounded terminal output.
4. Code mode retains recoverable live exec-session metadata independently of
   JavaScript result projection.
5. Code-mode deadline yields do not wake the model for quiet nested work.
6. Yielded cells remain bound to their originating dispatch host, turn
   environment, notifications, and hook context.
7. Explicit interruption, shutdown, and real transport failure retain distinct
   lifecycle behavior.
8. Shared app-server daemons are reused only when their runtime identity matches
   the invoking fork; incompatible daemons are left running for explicit,
   operator-controlled restart after active work finishes.

## Release layout

Each fleet release uses two commits because a commit cannot contain its own Git
object ID:

1. The reviewed source commit contains the versioned implementation and
   installer marker.
2. `fleet/jkammerland.mcp.19-source` points exactly at that source commit.
3. A direct descendant pins that source commit and ref in
   `fleet/release.json`; `release/jkammerland.mcp.19` points at the descendant.

The fleet controller verifies the immutable source ref, native build receipts,
binary hashes, CLI identity, executable pair, and compatible MCP-server commit
before reporting a host healthy. Nothing is pushed automatically.

## Validation

Maintenance candidate, 2026-09-05 (Linux only):

- Before carrying the stdin fix, the focused baseline run passed 17 tests and reproduced
  the active-wait/stdin deadlock. After that fix, the stdin test passed; the two added
  drained-output/reused-handle tests reproduced their failures before their fixes.
- The final focused run covered MCP, code-mode protocol/client/runtime, and core
  unified-exec/code-mode integration: 730 of 731 tests passed. The shell-environment
  snapshot test timed out, including on an isolated rerun; it had passed in the preceding
  run. This remains unresolved, not classified as an upstream defect or waived for release.
- A stale code-mode caller and upstream/fork test-contract mismatches were adapted:
  explicit yield reason, originating-turn ticket ownership, and recovery of completed but
  unconsumed process output. Companion test binaries were rebuilt from this worktree.
- The Bazel dependency-lock refresh passed without lockfile changes. Full-workspace and
  native Mac/Windows release qualification have not been run for this candidate.
- Scoped Clippy fixes and repository formatting passed. The final mechanical changes were
  removal of a redundant test-result clone and formatting; tests were not rerun afterward,
  following the repository workflow.

Release identity and fleet-controller tests are rerun before source and manifest
commits are cut. The pinned candidate is then built and validated independently
on Linux, native Apple Silicon macOS, and Windows before activation.

## Local installation

From the manifest commit in a clean checkout:

```sh
./scripts/install-user-fork.sh
codex --version
```

The installer verifies the manifest and reviewed source ancestry, builds both
native executables, verifies the fork suffix and `codex agents` support, and
atomically activates the pair with rollback protection.
