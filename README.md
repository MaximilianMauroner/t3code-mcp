# t3-code-mcp

Use GPTVoice tool calls to work with T3 Code hands-free while on the go, especially from a phone. The goal is to ask what an agent is doing, give it a task, hear the result, and respond when it needs input without opening the T3 interface.

This repository provides the MCP gateway for that workflow. GPTVoice handles listening, conversation, tool calls, and spoken responses. The gateway connects those tool calls to one configured T3 Code environment, where projects, threads, agent sessions, workspaces, and execution live.

```mermaid
flowchart LR
    User[You on your phone] <-->|Voice| Voice[GPTVoice]
    Voice <-->|MCP tool calls through a reachable endpoint or tunnel| Gateway[t3-code-mcp]
    Gateway <-->|Authenticated HTTP| T3[T3 Code and its coding agents]
```

## The intended voice workflow

| What you say | Tools the voice client uses |
| --- | --- |
| “Find my website project.” | `t3_projects_list` with `query` |
| “What threads are open in that project?” | `t3_threads_list` with `projectId` and `status: "open"` |
| “Show snoozed / settled threads.” | `t3_threads_list` with `status: "snoozed"` or `"settled"` |
| “How is the login fix going?” | `t3_threads_list`, `t3_thread_get`, and `t3_thread_messages` as needed |
| “Start a thread in that project and investigate the failing tests.” | `t3_thread_create`, then `t3_thread_send` |
| “Any update?” | `t3_run_get` or `t3_run_wait` for the returned run handle |
| “What does it need from me?” | `t3_pending_actions_list` |
| “Approve that request.” | `t3_pending_action_respond` with the request ID and decision |
| “Stop that thread.” | `t3_thread_get`, then `t3_thread_interrupt` with the observed `latestTurn.turnId` |
| “Follow up and ask it to run the tests.” | `t3_thread_send` on the existing idle thread |
| “Archive that thread.” | `t3_thread_archive` |

The client should resolve project and thread names to IDs, keep those IDs and returned run handles in conversation context, and speak short summaries. It should ask for clarification when a name or action is ambiguous. These are client responsibilities; the gateway returns structured results and does not generate speech or manage conversation context.

A task can keep running in T3 after the phone disconnects. To check it later, the client calls the same gateway with the saved `runId`. If that handle is unavailable, it can still find the thread and read its latest state and messages. The gateway has no run-list tool.

## Find and check on existing work

`t3_projects_list` accepts an optional `query` matching a case-insensitive substring of the project title, workspace path, or ID. `t3_threads_list` accepts `projectId`, a `query` matching title, branch, or ID, and a `status`. Filters combine and apply before cursor pagination.

| Thread status | Meaning in this gateway |
| --- | --- |
| `open` | Unarchived work that is neither effectively snoozed nor explicitly settled. Includes pinned threads, running work, and idle threads. |
| `snoozed` | A future snooze time, with no pending approval/input, new failure, or completion since snoozing that would wake it early. Snoozing does not stop execution. |
| `settled` | T3's explicit settled override, subject to activity blockers and pin/snooze precedence. A completed turn alone does not settle a thread. |
| `archived` | Threads with an archive timestamp. This explicit filter includes archives even when `includeArchived` is false. |
| `all` or omitted | No lifecycle filter; archives remain hidden unless `includeArchived=true`. |

The gateway uses the lifecycle fields exposed by T3. The T3 UI also derives settlement from client preferences, inactivity, and linked PR state; those inputs are not available to these tools, so the settled/open lists can differ from the UI's automatic classification. Older servers with no lifecycle fields show unarchived threads as open. This behavior follows the server-backed portion of T3's `threadSettled.ts` and sidebar partitioning, inspected at source revision `4b8388773`.

Thread summaries include `status`, the raw settlement override, snooze time, pin timestamp, session status, latest turn, pending flags, actionable-plan flag, and background liveness (`working` or `monitoring` when T3 provides it). `t3_thread_get` combines the full thread with these shell fields and its latest response. Use `t3_thread_messages` for more history.

To start work, create a thread if needed and send a message. To continue existing work, send another message to that thread once it is idle. Creating a thread does not itself start a turn. Runtime mode follows the existing tool defaults; request `approval-required` explicitly when creating a thread if that is the intended T3 permission mode.

`t3_thread_interrupt` works on threads started in T3's UI or by another client, without requiring a gateway `runId`. Read the thread and supply `threadId`, `expectedTurnId` (the observed `latestTurn.turnId`), and an `idempotencyKey`. The gateway rejects a changed or finished turn before dispatch and never automatically replays an uncertain interruption. T3 currently interrupts by provider session, so a turn change after the gateway's check remains a race; the expected turn ID is not an atomic upstream condition. Poll `t3_thread_get` to verify the outcome. Interruption does not archive or delete the conversation.

Thread deletion, workspace deletion, checkpoint rollback, and arbitrary terminal commands are not exposed. Filtering snoozed/settled work is read-only; changing snooze or settlement is not implemented by this gateway.

## What is implemented

The gateway uses T3’s authenticated HTTP orchestration API. The recorded integration target is T3 `v0.0.41-nightly.20260910.1507`; pin and test the version used by your deployment.

- Project search and registration, thread search and lifecycle filters, thread creation, compact check-ins, and paginated messages with bounded text.
- Starting agent turns, inspecting runs, polling for changes, requesting interruption by gateway run handle or observed thread turn, responding to approval or user-input requests, and archiving threads.
- Connection status with environment identity, scopes, capabilities, and freshness information. Run results also report connection and freshness; other tool results include the environment ID.
- A local operation journal for idempotency and reconciliation after uncertain dispatches.
- Stateless Streamable HTTP at `/mcp`, plus stdio for clients that launch a local process.
- User systemd units and a launcher for the configured OpenAI Secure MCP Tunnel deployment.

The automated tests exercise the gateway and MCP boundary. They do not establish that the complete GPTVoice phone experience works. That acceptance check still needs to be performed with the intended voice client and its tool connection.

## Connect GPTVoice

Run the gateway on an always-available machine that can reach T3. Configure the voice client's MCP connection, or its tool backend, to reach the gateway. A localhost URL on the gateway machine is not reachable from a phone or a hosted tool runner; use the tunnel deployment below or another authenticated, reachable route.

For a direct Streamable HTTP connection, use `/mcp` and the header `Authorization: Bearer <MCP_BEARER_TOKEN>`. The included tunnel launcher supplies this header on the local gateway connection. Client-side connection setup depends on GPTVoice's supported tool integration; this repository does not contain a GPTVoice app, client adapter, or setup UI. Tool-call support alone does not establish compatibility with this MCP transport and authentication scheme.

For the first end-to-end check:

1. Start the gateway with `MCP_READ_ONLY=true` and connect the voice client.
2. Ask it to report connection status, list projects, and summarize an existing thread. Confirm that it uses tool results from the intended environment.
3. Set `MCP_READ_ONLY=false` and restart when ready to exercise control tools. The T3 token also needs `orchestration:operate`.
4. Ask it to create a disposable thread, start a small task, report progress, and read back the result.
5. Disconnect and reconnect the voice client, then inspect the same thread or saved run handle. Check interruption and pending-action responses when applicable.

Success means completing that loop by voice on the phone, with clear spoken feedback about accepted work, completion, requests for input, and connection failures.

## Official OpenAI documentation

- [Realtime with tools](https://developers.openai.com/api/docs/guides/realtime-mcp): function tools, remote MCP configuration, approvals, and the event flow needed to continue a voice response after tools finish.
- [Getting started with the Realtime API](https://developers.openai.com/api/docs/guides/realtime): building speech-to-speech clients and choosing a connection transport.
- [MCP and Connectors](https://developers.openai.com/api/docs/guides/tools-connectors-mcp): connecting remote MCP servers through the Responses API, including tool filtering and authorization.
- [Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels): tunnel setup, runtime credentials, workspace associations, and connecting supported OpenAI products to a private MCP server.

These describe the OpenAI integration options. The gateway itself does not call the Realtime or Responses API. GPTVoice's use of those options and the complete phone workflow must be verified in the target client.

## Run the gateway

Install Node.js 20 or newer and pnpm, then build:

```sh
pnpm install
pnpm build
cp .env.example .env
chmod 600 .env
```

Edit `.env` and set `T3_ACCESS_TOKEN` and a separate random `MCP_BEARER_TOKEN`. For the remote voice workflow, set `MCP_TRANSPORT=http`; use `MCP_READ_ONLY=true` for the initial connection check. Start with:

```sh
node --env-file=.env dist/cli.js serve
```

The CLI reads process environment variables; it does not load `.env` automatically. The command above uses Node's `--env-file` support, available in Node 20.6 and later. On earlier Node 20 releases, export the variables before starting the CLI.

The default HTTP address is `http://127.0.0.1:8787/mcp`. `/healthz` reports that the gateway HTTP server is alive; use `t3_connection_status` to check T3 connectivity. For a local MCP host that launches a process, leave `MCP_TRANSPORT` unset or set it to `stdio`.

| Variable | Purpose and default |
| --- | --- |
| `T3_HTTP_BASE_URL` | T3 HTTP address; `http://127.0.0.1:3773` |
| `T3_ACCESS_TOKEN` | Required T3 credential, kept on the gateway server |
| `MCP_TRANSPORT` | `stdio` by default; use `http` for a remote MCP connection |
| `MCP_BEARER_TOKEN` | Required to serve authenticated `/mcp` requests over HTTP |
| `MCP_READ_ONLY` | Reject mutations when `true`; code default is `false` |
| `MCP_HOST`, `MCP_PORT` | HTTP listener; `127.0.0.1`, `8787` |
| `T3_ENVIRONMENT_ID` | Optional expected environment ID for identity checks |
| `T3_ENVIRONMENT_LABEL` | Optional fallback label when discovery is unavailable |
| `T3_MCP_DATA_DIR` | Operation journal directory; `./data` |
| `T3_STALE_AFTER_MS` | Freshness threshold; `30000` |

## Run and retry behavior

Mutation tools require an `idempotencyKey`. Generate a new key for each intended action, and reuse the same key and input when retrying that action. Reusing a key with different input fails. The journal stores the key, payload hash, command ID, operation handle, and reconciliation fields. It does not store the original prompt or credential fields.

A mutation can return `accepted`, `rejected`, or `uncertain`. Accepted means T3 accepted command intent, not that the task finished. An uncertain result must not trigger a new turn with a fresh key: the original may already be running. The gateway reconciles available T3 state and does not automatically replay mutations.

Preserve `T3_MCP_DATA_DIR` across gateway restarts. Run one gateway process per journal directory; the JSON journal is not a shared database for multiple active gateway processes. Reconnecting MCP clients use that same gateway and journal.

`t3_run_wait` polls for a relevant change, with a tool timeout parameter of 1–30 seconds. It can return before completion, and a T3 request can extend the elapsed wait. Timing out or losing the client connection does not cancel the T3 run. The client must call again for further updates; there are no push notifications or background spoken alerts.

## Current limits

Busy threads reject new turns; queueing and steering are not implemented. Pending-action details are inferred from T3 activity records and may include historical entries; the pending flags and available request IDs need to be considered together. The gateway does not provide a separate human-confirmation mechanism for approval responses.

Run inspection relies on the journaled message/turn IDs and T3's latest-turn projection. Status for older runs or runs whose turn ID is not yet known can be incomplete. A provider-owned turn ID is not currently exposed.

There are no terminal tools, managed command jobs, direct file/Git inspection tools, MCP OAuth server, or multi-environment routing. The HTTP endpoint uses a static bearer token; use a trusted network, tunnel, or HTTPS reverse proxy for remote access. The gateway does not read T3's database, manipulate project files locally, or create provider sessions outside T3.

## Auto-start on Linux

The included user systemd units start the gateway and [OpenAI Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels), restart after unexpected exits, and bind local listeners to loopback. These are deployment-specific files: review the absolute paths in both services and the launcher, the T3 environment ID in the environment example, and the tunnel ID in `deploy/systemd/t3-code-mcp-tunnel.sh` before using them on another host. The tunnel client must already be installed at the launcher's configured path.

The launcher derives the `Bearer` header from `MCP_BEARER_TOKEN` and waits for gateway health before starting tunnel discovery. Credentials are read from `~/.config/t3-code-mcp.env`.

After building, install the units:

```sh
install -d -m 700 "$HOME/.config/systemd/user"
test -e "$HOME/.config/t3-code-mcp.env" || install -m 600 deploy/systemd/t3-code-mcp.env.example "$HOME/.config/t3-code-mcp.env"
install -m 644 deploy/systemd/t3-code-mcp.service "$HOME/.config/systemd/user/t3-code-mcp.service"
install -m 644 deploy/systemd/t3-code-mcp-tunnel.service "$HOME/.config/systemd/user/t3-code-mcp-tunnel.service"
systemctl --user daemon-reload
systemctl --user enable t3-code-mcp.service t3-code-mcp-tunnel.service
```

Set `T3_ACCESS_TOKEN`, `MCP_BEARER_TOKEN`, and the tunnel runtime `CONTROL_PLANE_API_KEY` in the environment file, then start and verify:

```sh
systemctl --user start t3-code-mcp.service t3-code-mcp-tunnel.service
systemctl --user status t3-code-mcp.service t3-code-mcp-tunnel.service
curl -fsS http://127.0.0.1:8787/healthz
curl -fsS http://127.0.0.1:8080/readyz
```

The deployment example starts read-only. Restart the gateway after changing that setting. For user services to remain available after logout and start at boot without a login, the host also needs user lingering enabled.

## Development and verification

```sh
pnpm build
pnpm test
pnpm typecheck:test
```

The default suite uses disposable local T3 fakes and no real credentials. It covers the HTTP boundary, gateway operations, journal recovery and idempotency, MCP schemas and errors, HTTP authentication, stateless clients, body limits, message truncation, stale state, and uncertain dispatch reconciliation.

For a read-only connection diagnostic:

```sh
node --env-file=.env dist/cli.js status
node --env-file=.env dist/cli.js spike
```

`spike` also lists up to five projects. Its optional mutation path requires `T3_SPIKE_ENABLE_MUTATIONS=true`, `T3_SPIKE_CONFIRM=I_UNDERSTAND`, `T3_SPIKE_PROJECT_ID`, and `T3_SPIKE_PROMPT`. Optional `T3_SPIKE_THREAD_TITLE` and `T3_SPIKE_IDEMPOTENCY_KEY` customize the thread title and retry-key prefix. This path creates a thread and submits a prompt; it does not archive the thread afterward.

The opt-in live test creates a disposable thread, asks the configured provider to run a small check without editing files, reads the journal through a second gateway instance, observes the run, and attempts to archive the thread. It currently selects `instanceId=codex_openai` and `model=gpt-5.3-codex-spark`, which must exist in the target T3 environment:

```sh
T3_LIVE_ACCESS_TOKEN='server-side-t3-token' \
T3_LIVE_PROJECT_ID='remote-project-id' \
pnpm test:live
```

Set `T3_LIVE_HTTP_BASE_URL` and `T3_LIVE_ENVIRONMENT_ID` for another environment. If no project ID is supplied, the test uses the first listed project. `test:live` sets `T3_LIVE_TESTS=1`; the test still skips without a token. This verifies the T3 integration, while the phone acceptance workflow above verifies the product goal.
