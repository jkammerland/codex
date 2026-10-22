# Maintained MCP lifecycle fork

This integration candidate rebases the MCP.25 multi-machine team trial onto
upstream main `d138398cef7d49e46fe33f1d0ddbd2ca0ed16aa4`. It uses
`0.163.0+jkammerland.mcp.25.team.4` for CLI, app-server, and package manifests,
and retains the `0.163.0`
Cargo workspace version used by model discovery. Build the client and server
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
Windows builders must retain the historically qualified migration byte format.

Outgoing Responses requests, including Responses Lite, omit unsupported
`content_item_metadata` to avoid backend rejection. Internal history provenance
and supported `content_item_kinds` remain intact.

Builds use Rust 1.99.1 and target-matched V8 152.2.0 archive/binding pairs.
Upstream message lifecycle callbacks share one live delivery handle across
queued and reconciled mail. Durable wake receipts remain separate and are
cleared only when the model accepts the selected request. Restarted mail
recovers its persisted payload without recreating live extension callbacks.

Qualification includes the default Linux suite, focused native checks, real
terminal trials, and complete staged packages.
Staging does not authorize installation, selection, daemon changes, or pushes.
The retained fleet controller and installer are historical tooling; their raw
pair activation flow is not the package staging procedure for this candidate.
