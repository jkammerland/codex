# Codex teams

The opt-in team coordinator connects existing agent sessions across app servers.
Each session keeps its history, files, authentication and approvals on its owning
host. The coordinator stores the directory, messages, receipts and control history.
Idle agents wait for events; messaging does not spend model turns on polling.

This is an experimental protocol. Use matching builds of the coordinator and
workers. Machines and team actions extend the existing `codex agents` overview.
Ordinary sessions are visible without joining the team; enrollment is explicit.

## Start a team

Copy [example.toml](example.toml) to a private configuration directory and edit the
machine/server entries. Paths are relative to the manifest. Runtime state and
credentials belong outside source repositories; this manifest is separate from
the fork's deployment inventory.

```sh
codex agents --manifest team.toml
# Optionally enroll an existing root session:
codex agents --manifest team.toml enroll local/primary/THREAD_UUID builder
```

The overview starts or reuses a local coordinator asynchronously. To load the
manifest automatically, set the top-level `agents_manifest = "/path/to/team.toml"`
in `config.toml`. Relative paths resolve against the configuration file. Use
`codex agents --local` to open only the local overview, or `--remote URL` for one
explicit server. `--manifest` overrides configuration; it cannot be combined with
`--local` or `--remote`. The old `--team` flag and `codex team` commands remain
compatibility aliases.

Enrollment accepts an existing durable root session, including one already loaded
by the daemon. Descendants join its family. Repeating enrollment renames the alias
without replacing history or credentials. Addresses contain machine ID, server ID
and thread ID; aliases are display names. The coordinator pins each installation
identity and refuses a changed installation until its enrollment is reviewed.

For service supervision, `codex agents --manifest team.toml serve` runs the
coordinator in the foreground. Only one process can own its state directory.
Its operator token is created on first start. Keep that token private: it grants
control and interactive attachment to all enrolled hosts.

## Connect workers and attach

Supported manifest transports are a local daemon socket, authenticated WebSocket,
and SSH forwarding to a running loopback WebSocket listener. Remote connections
use WSS unless protected by SSH. Certificate configuration uses the existing Codex
custom-CA support. Plaintext endpoints are restricted to loopback.

For a daemon that needs both local socket access and SSH forwarding, start the
same app-server process with both listeners:

```sh
codex app-server --listen unix:///ABSOLUTE/PATH/daemon.sock \
  --listen-loopback 127.0.0.1:4501 \
  --ws-auth capability-token --ws-token-file /ABSOLUTE/PATH/worker-token
```

Create the worker token first and protect it with the account's file permissions.
Pass these arguments through your service's launch configuration. Do not launch a
second daemon against the same socket/account. On Windows, use a native absolute
socket path accepted by the daemon and a running console daemon; SSH only forwards
the connection and does not launch the worker. macOS and Linux use the same wire
protocol.

Enter attaches to the selected session within the existing TUI. Machines form
the outer groups; status, project and model grouping remain available within each
machine. Search, pins and normal session actions remain in the same overview.
SSH keys and worker transport credentials remain on the coordinator. Each host
retains its connection, request IDs, pending approvals and tool replies when you
switch machines. Leaving the panel does not cancel remote work.

## Controls and settings

| Action | Effect |
| --- | --- |
| Pause (F6) | Hold new turns, model requests, tools, delegation, delivery and publication for enrolled families. Already admitted work may finish. |
| Stop (F8) | Interrupt enrolled families and request cancellation of their owned tracked jobs. |
| Resume (F7) | Release the global hold; continue a stopped original turn before later mail. Preserve manual holds and approvals. |

F6/F7/F8 also work inside the model picker and message composer. Pause is cooperative;
it does not suspend OS processes or undo completed side effects. The panel shows
each host's applied revision, admitted operations and unconfirmed cancellations.
Archived sessions stay held across reconnect until explicitly unarchived.
An offline host cannot acknowledge a new control. Its last applied control remains
in force, and all missing revisions are replayed on reconnect, including Stop
followed by Resume. Startup reconciles persisted enrollment before admitting work.

Use F9 to open the selected task's team actions, including its model and effort
picker. The picker uses that host's catalog. Changes use
`thread/settings/update` and apply to subsequent turns; current-turn settings are
shown separately. No substitute model is selected. Ultra is an explicit choice
when the host offers it. Enrollment preserves existing settings. Agent messages
cannot choose models or effort; explicit model arguments in an enrolled agent's
spawn call are rejected. User-configured role/default overrides remain supported.
Children that follow their parent receive its next-turn selection; an explicit
child selection is retained, including a changed selection from an attached TUI.
A selection missing from the host's catalog holds new work and queued mail until
you select a supported model or effort. Custom providers that do not publish their
model names retain their existing provider policy; the team never invents a replacement.

```sh
codex agents --manifest team.toml pause
codex agents --manifest team.toml stop
codex agents --manifest team.toml resume
codex agents --manifest team.toml models local primary
codex agents --manifest team.toml select-model local/primary/THREAD_UUID --model value:MODEL --effort value:high
codex agents --manifest team.toml select-model local/primary/THREAD_UUID --effort default
codex agents --manifest team.toml status
```

Model and effort each support `keep`, `default`, `inherit` and `value:NAME`.
Inheritance is available for descendants. Equal values do not imply inheritance.
Requested, applied-next and active-turn settings are distinct. Models with no
effort choices use their default without a fabricated effort level.

## Messages, wakeups and MCP

```sh
codex agents --manifest team.toml send local/primary/THREAD_UUID 'Please review the patch'
codex agents --manifest team.toml send local/primary/THREAD_UUID 'Build finished' --kind note
codex agents --manifest team.toml reply MESSAGE_UUID 'Review complete'
codex agents --manifest team.toml reply MESSAGE_UUID 'Join the current turn when safe' --safe-boundary
codex agents --manifest team.toml post builds 'Windows build is ready'
codex agents --manifest team.toml subscribe local/primary/THREAD_UUID builds
codex agents --manifest team.toml subscribe local/primary/THREAD_UUID builds --remove
```

Requests and replies wake eligible idle recipients. Notes remain passive until a
subsequent turn. Next-turn delivery preserves running work; `--safe-boundary`
allows input to join the current turn at its next safe input boundary. It never
interrupts a tool to inject a message. Board posts wake only explicit subscribers;
their audience is fixed when published. Enrolled agents have a native `team` tool
for the directory, inbox, attributed messages, replies, board and subscriptions.
F9 opens enrollment, alias, message, subscription and board actions. Sending
acknowledges durable intake; it does not wait for a model reply. A lost response
can be retried with the same `--operation-id` to recover the original result.

For an external agent client, obtain that agent's bound credential through the
operator CLI and provide it through an environment variable:

```sh
codex agents --manifest team.toml agent-credential local/primary/THREAD_UUID
codex agents mcp --socket /ABSOLUTE/PATH/daemon.sock --thread THREAD_UUID \
  --credential-env CODEX_TEAM_AGENT_CREDENTIAL
```

The agent MCP exposes only the `team` tool, bound to that thread. WebSocket clients
can use `--url` and `--auth-token-env`. The separate privileged
`codex agents --manifest team.toml control-mcp` exposes `team_control` with
`status`, `pause`, `resume`, and `stop`. Configure that surface only for an operator.
Both stdio surfaces negotiate MCP 2025-06-18.

Host outboxes survive coordinator outages. Input markers are durable before model
execution; lost acknowledgements are reconciled against original history. A
delivery whose execution cannot be established is marked `needsReconciliation`.
Inspect that session before continuing it; sending the same delivery ID does not
replay it. Reverted history also requires inspection. No exactly-once guarantee is
made for external side effects. Invalid outgoing envelopes are retained under
`rejectedOutbox` in status and do not block later mail; correct and resend them.

## External job cancellation

Local background terminals are terminated through their owning session. For an
external MCP job service, configure an adapter on its worker entry. The adapter
must name an idempotent cancellation tool and its structured acknowledgement:

```toml
[[machines.servers.cancellation_adapters]]
server = "jobs"
launchTool = "launch"
jobIdPointer = "/job_id"
cancelTool = "cancel"
cancelIdField = "job_id"
cancelArguments = {}
confirmationPointer = "/cancelled"
confirmationValue = true
# pluginId = "trusted-plugin-id" # Omit for a user-configured MCP server.
```

Only handles returned by the configured launch tool's structured result are
owned. IDs in messages or tool prose are never cancellation targets. Launch and
cancel provenance must match the configured server/plugin. Stop intent is retained
if the handle arrives late. Unknown handles, disconnected services and responses
without the configured confirmation remain unconfirmed. A repeated Stop requests
another cancellation attempt; Resume never claims that cancellation succeeded.

## Optional Gitea

Add a `[gitea]` section with `url`, `token_file`, `webhook_secret_file`, selected
`repositories = ["owner/repository"]`, and optional `[gitea.assignees]` mapping
Gitea usernames to team aliases. Configure selected repositories' JSON webhooks to
`/gitea/webhook` on the coordinator's authenticated TLS reverse proxy, using the
same webhook secret. This route verifies the Gitea HMAC signature; it does not use
the operator token. The token needs access to the selected repositories and issue
comments.

Issue/PR mentions of `@alias`, mapped assignments and review requests create
attributed requests. Replies map back to the originating issue/PR. Repeated
webhooks are deduplicated, and mirrored comments are ignored. Publication pauses
with the team. An uncertain comment POST requires inspection and
`codex agents --manifest team.toml retry MESSAGE_UUID`; the adapter checks for its
original comment. If its outcome remains uncertain, it stays held rather than
posting again. Publication remains bound to the original configured integration.

`gitea-inspect owner/repository` queries the configured instance's version and
project API. Project boards are listed when available; project webhook automation
is not assumed. See the primary [webhook documentation](https://docs.gitea.com/usage/repository/webhooks/)
and [project API](https://docs.gitea.com/api/operations/repo-list-projects/).

## Development checks

```sh
just test -p codex-team -p codex-team-store -p codex-team-extension
just test -p codex-app-server --test all v2::team
just test -p codex-tui -E 'test(agents_overview) | test(agents_hosts) | test(model_target)'
```

The coordinator runtime test uses two real app-server processes and a mock model
service, including socket/WebSocket identity, relay authentication, equal RPC IDs,
paused delivery and control replay. Build `codex-app-server` before running that
test alone. The same tests are intended for native Linux, macOS and Windows builds;
SSH account/service setup and terminal detach also require native qualification.
