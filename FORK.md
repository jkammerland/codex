# Maintained MCP lifecycle fork

This branch carries user-local Codex lifecycle behavior maintained separately
from upstream releases.

## Identity

- Release: `jkammerland.mcp.16`
- Upstream base: `2f0a5d5516c566e40b7abefea5f3c1f81fcd64bd` (`origin/main`)
- User-visible suffix: `+jkammerland.mcp.16`
- Personal Git remote: `fork`

The startup header, `codex --version`, app-server handshake, daemon lifecycle,
and app-server client metadata share the suffix. Wire schemas remain compatible
with the upstream Cargo workspace version.

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
2. `fleet/jkammerland.mcp.16-source` points exactly at that source commit.
3. A direct descendant pins that source commit and ref in
   `fleet/release.json`; `release/jkammerland.mcp.16` points at the descendant.

The fleet controller verifies the immutable source ref, native build receipts,
binary hashes, CLI identity, executable pair, and compatible MCP-server commit
before reporting a host healthy. Nothing is pushed automatically.

## Validation

The release candidate was reviewed by iterative Codex and validated on Linux,
native Apple Silicon macOS, and the repository's Windows Wine executor. Release
identity and fleet-controller tests are rerun before source and manifest commits
are cut.

## Local installation

From the manifest commit in a clean checkout:

```sh
./scripts/install-user-fork.sh
codex --version
```

The installer verifies the manifest and reviewed source ancestry, builds both
native executables, verifies the fork suffix and `codex agents` support, and
atomically activates the pair with rollback protection.
