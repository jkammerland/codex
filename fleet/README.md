# Fork fleet releases

`release.json` is the expected state for every critical native-offload host. It
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
