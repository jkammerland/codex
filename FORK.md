# Maintained MCP lifecycle fork

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
