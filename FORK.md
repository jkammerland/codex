# Maintained MCP lifecycle fork

This branch carries user-local Codex MCP lifecycle behavior that is intentionally
maintained separately from upstream releases.

## Identity

- Branch: `release/jkammerland.mcp.6`
- Upstream base: `dbe9dac1ae0050dc14097d7e30d23109a2744121` (`origin/main`)
- User-visible suffix: `+jkammerland.mcp.6`
- Git remote for the existing personal fork: `fork`

Both the startup header and `codex --version` include the suffix. Internal
protocol and compatibility reporting retains the upstream Cargo package version.
Built-in TUI update checks treat the branded binary as a source build so they do
not prompt for an npm update that would overwrite the fork.

## Patch stack

1. An explicit MCP `tool_timeout_sec = 0` maps to no client tool-call deadline.
2. Superseded MCP transports shut down when their last owner disappears.
3. A failed call that proves its transport closed schedules a fresh transport
   for the next call without replaying the ambiguous failed request.
4. CLI and TUI version output identify this maintained build.
5. Multi-agent v2 waits sleep until mailbox or user-input activity instead of
   returning on a polling timeout.
6. Code-mode waits preserve yield reasons and absorb empty deadline yields
   without returning control to the model.
7. Unified exec provides a no-timeout `wait_process` that wakes on completion,
   queued input, or TTY output while retaining explicit bounded polling.
8. Completed unified-exec waits drain through the terminal output-close event,
   preserving final output that arrives after process exit.

## Validate

From `codex-rs`:

```bash
just fmt
just test -p codex-mcp
just test -p codex-cli
just test -p codex-tui
just fix -p codex-mcp -p codex-cli -p codex-tui
```

The focused `codex-core` MCP tests should also pass after transport changes:

```bash
just test -p codex-core --lib mcp
```

## Install for this user

After committing a clean, tested tree:

```bash
./scripts/install-user-fork.sh
codex --version
```

The installer builds `codex` and `codex-code-mode-host` in release mode, verifies
the fork suffix, preserves the first upstream executables for that upstream
version, stages and optionally strips the new executables, and atomically
replaces each npm platform-native binary. The npm JavaScript launcher and other
helper binaries remain untouched.

## Rebase onto a later upstream release

1. Fetch upstream tags with `git fetch origin --tags`.
2. Rebase this branch onto the selected, reviewed `rust-vX.Y.Z` tag.
3. Resolve the patch stack rather than accepting either side wholesale.
4. Increment the suffix in `codex-rs/tui/src/version.rs`, the CLI test, this
   file, and `scripts/install-user-fork.sh`.
5. Run the validation commands above and review all snapshot changes.
6. Install the clean committed result and verify both the banner and
   `codex --version` before using long MCP waits.

Publishing or force-updating the personal remote branch is a separate explicit
operation; local maintenance does not push automatically.
