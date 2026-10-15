# Fork fleet releases

## Active host inventory

`hosts.json` tracks the current update targets, installed versions, and staging
status. It includes the additional Linux target `dnarb`, reached through
`dnarb-offload` (SSH alias `dnarb`). Include it in future fleet staging and
activation work. Consult each inventory entry for its installed and staged
state; registration alone does not imply qualification or installation.

The inventory records the exact source commit embedded in the staged packages,
which may precede documentation-only tracking commits. It is tracking data;
the historical controller does not consume it or automatically update hosts.

`release.json` and the controller instructions below describe the historical
MCP.20 raw-binary workflow, not the current complete-package deployment process.
Do not use them to activate the staged MCP.25 packages. The controller only
accepts Mac/Windows entries, so adding Linux targets there would require a
separate controller change.

## Windows operating mode (2026-09-20)

Maintainer decision: **park Windows shared-daemon / `codex agents` support**.
Use Batch Codex through `windows-offload` for Windows builds, tests, and reviews.
This is an operational decision, not removal of daemon code or Windows support.
Linux and Mac shared-daemon support remains enabled.

- The current Windows host is `WIN11`; `WIN11-offload` selects the separate
  standard-user `codex-worker` account. `WIN11` selects administrator `ai-dev1`.
  The older `windows-11` alias is a different machine; historical manifest
  entries must not be treated as the current host inventory.
- Both Windows accounts were upgraded to `0.156.0-alpha.8+jkammerland.mcp.21`.
  Their launchers use complete packages under each account's
  `.codex/releases/jkammerland.mcp.21/bin`, without a `current` junction.
- Worker authentication was restored and a real Batch Codex request completed
  successfully under `.mcp.21`. The MCP server launches `codex exec` directly;
  its durable jobs, waits, and recovery do not require the shared daemon.
- Shared-daemon startup remains blocked: the worker's SSH process reports
  `EnforceRedirectionTrust=true`, and Windows rejects traversal of the
  worker-created daemon-package junction as an **untrusted mount point**.
  The physical package exists; matching worker ownership and permissions did
  not resolve the rejection. A native PowerShell-created junction also failed.

Microsoft documents that [RedirectionGuard blocks junctions created by
non-administrators](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-process-mitigation-redirection-trust-policy).
Upstream [issue #40828](https://github.com/openai/codex/issues/40828) reports
the analogous standalone-installer junction failure in Windows OpenSSH sessions.
This establishes upstream overlap, not proof that every daemon issue is identical.

Do not disable RedirectionGuard, broaden ACLs, run the worker as administrator,
or add another fork workaround to enable this optional workflow. Revisit only
if the maintainer explicitly requests Windows shared-session support. Keep the
CLI/offload qualification separate from daemon qualification: package hashes and
CLI smoke checks passed, but the selected Windows regression tests aborted
during test discovery in the temporary-alias fixture; no native regression-suite
pass is claimed.

Activation and diagnostic evidence is retained in the controller's
`~/.codex/fleet-ledger/`, particularly `mcp21-all-hosts-activation.md` and
`mcp21-worker-auth-restored.md`. Existing MCP connections may retain the old
executable environment; reconnect after upgrading before submitting work.

## Release controller

`release.json` records the historical MCP.20 expected deployment state. It
tracks the maintained-fork commit and version independently from the Codex MCP
server deployment commit.

Use the controller from this checkout:

```sh
python3 scripts/fleet_release.py status
python3 scripts/fleet_release.py plan --host mac
python3 scripts/fleet_release.py stage --host mac --bundle /path/to/fork.bundle --apply
python3 scripts/fleet_release.py activate --host mac --apply
```

`status` and `plan` are read-only. `rollout --apply` gates on the MCP server
commit, stages source and a fresh build worktree, builds both native binaries,
atomically activates them with rollback copies, and checks final status. For a
critical-host rollout, use `stage --apply` first; it performs the server gate,
source staging, and native build without replacing installed binaries. Follow
with `activate --apply` after the host is quiescent. Builds use a Git bundle
until the fork ref is published; after publishing, omit `--bundle`.

Add a host by copying an existing entry, using absolute host-native paths and a
dedicated source/build-root pair. Allocate a new build root for each release;
the command refuses to reuse one. Keep the same release values for every host.
