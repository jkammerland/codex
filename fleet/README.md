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

The team.7 activation completed the local, Mac and Dnarb normal daemons and
private workers, plus the Windows private worker. The protocol 6 coordinator
has all five servers connected and its prior Running mode restored. Board,
enrollments, reads, subscriptions and message receipts were preserved; no
terminated work was replayed.

Both Windows normal components still require local-console activation. The
worker account passed a suspended breakaway-process probe, but its supported
updater then failed to traverse the managed `current` junction under SSH
(RedirectionGuard, OS error 448). It published the complete candidate before
that failure, without selecting it. The normal daemon is stopped; its managed
selection and normal launchers remain team.6. Its existing foreground Scheduled
Task successfully runs the separate team.7 private worker. The administrator's
SSH token is elevated, so the updater rejects it; its healthy team.6 normal
daemon and launchers remain matched. Retained, per-account console handoffs
cover both cases without changing security policy.

Qualify both junction traversal and detached process creation before stopping
a healthy Windows normal daemon. InteractiveToken scheduled tasks previously
installed packages but rejected persistent normal-daemon startup; a process
launch probe alone does not qualify managed-package access. Complete the normal
activation from a local, non-elevated PowerShell console. Stop with the old
executable before running a verified, complete private copy of the new package's
`app-server daemon update --from-cli --yes`, then start the new daemon. The
private copy needs writable package metadata; preserve the staged release.
Retain complete previous packages and SQLite-consistent pre-upgrade ledgers
outside installer pruning. Never automatically downgrade a migrated database.

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
