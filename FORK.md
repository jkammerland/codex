# Maintained MCP lifecycle fork

## MCP.23 pinned-main source candidate (2026-10-05)

This candidate transplants the maintained net delta from MCP.22 source
`7f4baa2acbb635d86fd31980b6104802e05d9d36`, relative to upstream stable
`ff6aec96948b70d94983af2641a6b67c94faeff5`, onto pinned upstream main
`7f892275e31002f0422477c6219189284560e689`.

Pinned main declares workspace version `0.0.0`. The candidate CLI identity is
`0.0.0+jkammerland.mcp.23`; build/package provenance must also retain the exact
candidate Git commit. This source identity does not imply an upstream tagged
release. The Rust toolchain remains 1.95.0 and Rusty V8 remains 150.4.0.

## Maintained behavior

Preserve unbounded configured MCP waits, quiet-wait coalescing, event-driven
process waits, original recoverable exec handles, originating code-mode
step/runtime/hook/notification ownership, transport leases, explicit
interruption/shutdown, genuine transport-loss reporting, and fork-compatible
daemon identity checks. Preserve upstream output consolidation, permissions,
package integrity validation, and unrelated behavior. No new deadlines,
context caps, admission quotas, replay, or daemon redesign belongs here.

The MCP.22 history migration repair is retained: only the recognized retired
migration checksum is relocated, transactionally with upstream migration,
without replaying destructive SQL or bypassing checksum validation. Linux
source SQL is LF. Future Windows staging must reproduce and verify the
historically qualified migration byte format; clean Git status alone is not
format evidence. This candidate adds no live database mutation or new
checksum exceptions.

## Qualification and operational scope

This phase owns source and focused Linux regression qualification. Its durable
report, exact source commit, checks, build environment, and lifecycle manifest
are recorded under:

`/home/ai-dev1/.codex/codex-mcp-server/job-artifacts/47ac63b93746fc23/tasks/task-0`

The report is authoritative for remaining blockers. This file does not assert
native Mac/Windows qualification, a release build, package readiness, staging,
or activation. The previous worktree and installed runtimes are preserved.
No branches are pushed automatically.

The retained `fleet/release.json`, fleet controller, and Linux installer are
historical tooling and are **not an MCP.23 staging or activation procedure**.
Do not retarget their obsolete raw-pair activation to this release: a complete
immutable package and separate daemon-package handoff need their own reviewed
procedure. No activator is run or redesigned in this source-only phase.

The reviewed Windows missing rendezvous-directory classification remains
parked pending focused native red/green coverage. Windows shared-daemon
qualification remains separate from this Linux source handoff.
