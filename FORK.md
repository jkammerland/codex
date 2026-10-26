# Maintained MCP lifecycle fork

This integration candidate rebases the MCP.25 multi-machine team trial onto
upstream main `970111f6cb62cc02fe36a82ea7d71b37a2889c97`. It uses
`0.164.0+jkammerland.mcp.25.team.10` for CLI, app-server, and package manifests,
and stamps `0.164.0` into the Cargo workspace version used by model discovery.
This is a source candidate, not an activated fleet release; the existing host
inventory continues to describe the retained installations and staged packages.
Build the client and server
from this branch together; changing package metadata alone does not update the
model catalog request.

MCP.25 transplants the maintained net delta from
`4d98246bb637350a78925f7a479ee4810218a495` (base
`7f892275e31002f0422477c6219189284560e689`) onto pinned upstream main
`bb64b894fd67158be0f874872e6d7bfe717e7368`.

The original MCP.25 workspace version is `0.163.0`; CLI and package identity is
`0.163.0+jkammerland.mcp.25`. Builds must stamp the exact source tip through
`STABLE_GIT_COMMIT`. This is a pinned-main fork, not an exact stable release.

Preserve unbounded configured MCP waits, quiet-wait coalescing, event-driven
process waits, original recoverable exec handles, originating code-mode
step/runtime/hook ownership, transport leases, explicit interruption/shutdown,
genuine transport-loss reporting, and fork-compatible daemon identity.
Upstream remote output retention, cleanup, and local post-exit draining remain
in effect. Packages follow the current upstream layout without bundled zsh.

The retired history migration repair remains narrowly checksum-matched and
transactional, without replaying destructive SQL or bypassing validation.
Embedded migration SQL is pinned to LF. SQLite startup accepts exact historical
LF/CRLF checksum variants outside quoted values and identifiers without rewriting
applied records or replaying SQL, preserving compatibility with retained binaries.

Outgoing Responses requests, including Responses Lite, omit unsupported
`content_item_metadata` to avoid backend rejection. Internal history provenance
and supported `content_item_kinds` remain intact.

Builds use Rust 1.99.1 and target-matched V8 152.2.0 archive/binding pairs.
Upstream message lifecycle callbacks share one live delivery handle across
queued and reconciled mail. Durable wake receipts remain separate and are
cleared only when the model accepts the selected request. Restarted mail
recovers its persisted payload without recreating live extension callbacks.

Release qualification requires the default Linux suite, focused native checks,
real terminal trials, and complete staged packages. Focused rebase checks do not
substitute for that later qualification.
Staging does not authorize installation, selection, daemon changes, or pushes.
The retained fleet controller and installer are historical tooling; their raw
pair activation flow is not the package staging procedure for this candidate.
