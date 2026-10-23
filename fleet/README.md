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

Staging creates a separate package and leaves launchers, managed daemons, team
workers, and the coordinator on their installed versions. Check `stagedVersion`
separately from `installedVersion` and `runningDaemonVersion`. When a candidate
changes the team protocol, activate the coordinator and enrolled workers together
after preserving their ledgers; staging alone does not migrate live state.

`release.json` and the controller instructions below describe the historical
MCP.20 raw-binary workflow, not the current complete-package deployment process.
Do not use them to activate the staged MCP.25 packages. The controller only
accepts Mac/Windows entries, so adding Linux targets there would require a
separate controller change.

## Windows daemon qualification

Both Windows accounts were qualified with matching CLI and managed packages,
verified through live JSON-RPC after starting from local, non-elevated PowerShell
consoles. Consult the inventory for their current versions. Worker auto-start
remains enabled; the administrator account retains its disabled
`daemon_auto_start` setting. An explicit daemon start is still allowed with
that setting disabled. `--no-daemon` is the per-invocation bypass.

SSH junction traversal remains blocked under RedirectionGuard. A remote
version probe can therefore return a null `managedCodexVersion` even when the
live daemon reports its version successfully. Verify the managed package from
a local console or its physical release path; do not loosen its ACLs.

InteractiveToken scheduled tasks can install the package, but their launch
context rejected persistent daemon startup. Complete that step from a local,
non-elevated console. When changing versions, stop with the old executable
before running the new packaged executable's
`app-server daemon update --from-cli --yes`, then start the new daemon.
Preserve the complete previous package and any pre-migration team ledgers.

## Historical controller

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
