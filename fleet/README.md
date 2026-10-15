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

## Windows daemon opt-out

Windows shared-daemon support remains parked. Both account configurations
explicitly disable `daemon_auto_start` using
`codex features disable daemon_auto_start`. Upstream enables it by default;
without this override, ordinary CLI startup attempts the retained old daemon
package and fails the maintained-fork version check. Preserve this account
setting during upgrades while daemon support remains parked. `--no-daemon`
also provides an explicit per-invocation bypass.

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
