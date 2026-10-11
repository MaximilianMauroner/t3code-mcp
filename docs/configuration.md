# Configuration and client connections

[Back to README](../README.md)

Run the commands below from the repository root.

For T3 orchestration protocol 2, the upstream URL must route HTTP API requests and WebSocket upgrades at `/ws`. The same `T3_ACCESS_TOKEN` authenticates both. See [orchestrator compatibility](orchestrator-compatibility.md) for version support and rollout checks.

## Run the gateway

Install Node.js 20 or newer and pnpm, then build:

```sh
pnpm install
pnpm build
cp .env.example .env
chmod 600 .env
```

Edit `.env` and set `T3_ACCESS_TOKEN` and a separate random `MCP_BEARER_TOKEN`. For a remote MCP connection, set `MCP_TRANSPORT=http`; use `MCP_READ_ONLY=true` for the initial connection check. Start with:

```sh
node --env-file=.env dist/cli.js serve
```

The CLI reads process environment variables; it does not load `.env` automatically. The command above uses Node's `--env-file` support, available in Node 20.6 and later. On earlier Node 20 releases, export the variables before starting the CLI.

The default HTTP address is `http://127.0.0.1:8787/mcp`. `/healthz` reports that the gateway HTTP server is alive; use `t3_connection_status` to check T3 connectivity. For a local MCP host that launches a process, leave `MCP_TRANSPORT` unset or set it to `stdio`.

| Variable | Purpose and default |
| --- | --- |
| `T3_HTTP_BASE_URL` | T3 HTTP address; `http://127.0.0.1:3773` |
| `T3_ACCESS_TOKEN` | Required T3 credential, kept on the gateway server |
| `T3_NATIVE_MCP_URL` | T3's native MCP endpoint; `${T3_HTTP_BASE_URL}/mcp`. The credential for it comes from [`native-login`](#native-t3-login), not from the environment |
| `MCP_TRANSPORT` | `stdio` by default; use `http` for a remote MCP connection |
| `MCP_BEARER_TOKEN` | Required to serve authenticated `/mcp` requests over HTTP |
| `MCP_READ_ONLY` | Reject mutations when `true`; code default is `false` |
| `MCP_HOST`, `MCP_PORT` | HTTP listener; `127.0.0.1`, `8787` |
| `MCP_MAX_BODY_BYTES` | Largest accepted HTTP request body; `2097152` (2 MiB). Raise it to accept larger `t3_attachment_upload` requests; base64 adds a third to the file size |
| `MCP_HOST_TOOL_NAMES_JSON` | Optional `doctor`-only JSON array captured from the actual MCP host for end-to-end tool discovery comparison |
| `T3_ENVIRONMENT_ID` | Optional expected environment ID for identity checks |
| `T3_ENVIRONMENT_LABEL` | Optional fallback label when discovery is unavailable |
| `T3_MCP_DATA_DIR` | Durable operation journal, redacted usage audit, and native credential directory; `./data` (`operations.json`, `audit.jsonl`, and `native-mcp-credential.json`) |
| `T3_MCP_WORKTREE_ROOT` | Optional absolute root for gateway-created isolated worktrees; by default a hidden sibling directory beside each project workspace |
| `T3_STALE_AFTER_MS` | Freshness threshold; `30000` |

## Native T3 login

The [native T3 tools](usage.md#native-t3-tools) need a separate credential. T3's `/mcp` endpoint accepts only its own MCP OAuth sessions, not `T3_ACCESS_TOKEN`. `native-login` gets that session with a one-time T3 pairing code, so no browser is needed.

1. On the T3 host, create a pairing code that expires soon:

   ```sh
   t3 auth pairing create --scope orchestration:read --scope orchestration:operate \
     --scope settings:write --scope source-control:write --ttl 10m --label t3-code-mcp --json
   ```

2. Run the login with the same environment as the gateway, and supply the code from the `credential` field at the hidden prompt or on stdin:

   ```sh
   node --env-file=.env dist/cli.js native-login --access full-access
   ```

The command refuses a pairing code given as an argument, because process lists show arguments. It checks the new session with a native `tools/list`, then writes `native-mcp-credential.json` with mode `600` to `T3_MCP_DATA_DIR`. It prints the access level, scopes, expiry, and tool counts, never the token.

`--access` sets the T3 runtime ceiling for native launches: `read-only`, `approval-required`, `auto-accept-edits`, `auto`, or `full-access` (the default). A `read-only` login blocks every native tool that changes state.

T3 issues the session for 30 days without a refresh token. Run `native-login` again with a new pairing code before it expires. The running gateway uses the new file on the next call, so no restart is needed. `t3_connection_status` and `doctor` report the expiry. The `doctor` check `native_mcp` says "expires soon" when fewer than 7 days remain, and fails when the credential is missing or expired.

## Connect an MCP client

Run the gateway on a machine that can reach T3, then choose the connection your client supports:

| Setup | How to connect |
| --- | --- |
| Local MCP client | Launch `node dist/cli.js serve` with `MCP_TRANSPORT=stdio` and the required T3 environment variables. |
| Remote MCP client | Run with `MCP_TRANSPORT=http` and connect to a reachable `/mcp` endpoint using `Authorization: Bearer <MCP_BEARER_TOKEN>`. |

The gateway can run alongside T3 or on another machine that can reach its HTTP API. For remote access, use the included [tunnel deployment](deployment.md) or an authenticated route through your own network or HTTPS reverse proxy. A localhost URL on the gateway machine is not reachable from a remote client or hosted tool runner. The included tunnel launcher supplies the bearer header on the local gateway connection.

Configure the connection in your MCP client's settings. The client must support the selected transport and, for direct HTTP connections, bearer authentication; custom tool backends can provide an adapter where needed.

For the first end-to-end check:

1. Start the gateway with `MCP_READ_ONLY=true` and connect the MCP client.
2. Request connection status, list projects, and summarize an existing thread. Confirm that the results come from the intended environment and that `native.credential` is `valid`.
3. Set `MCP_READ_ONLY=false` and restart when ready to exercise control tools. The T3 token also needs `orchestration:operate`.
4. Start a small task with `t3_task_start`, wait with `t3_thread_wait`, and retrieve the result with `t3_result_get`.
5. Disconnect and reconnect the client, then find the same task with `t3_tasks_list`. Check `t3_task_cancel` and pending-action responses when applicable.

Verify that the client clearly reports accepted work, completion, requests for input, and connection failures.

## Integration references

- [MCP and Connectors](https://developers.openai.com/api/docs/guides/tools-connectors-mcp): connecting remote MCP servers through the Responses API, including tool filtering and authorization.
- [Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels): the optional [tunnel deployment](deployment.md).

These references describe OpenAI-specific integration options. Other MCP clients can connect using the transports above; consult your client's documentation for its configuration format.

## Review usage

The gateway automatically appends structured usage events to `audit.jsonl` inside `T3_MCP_DATA_DIR`. The file is created with mode `600` and records MCP tool calls, HTTP/stdio transport activity, upstream T3 requests, Git commands, journal transitions, outcomes, and durations. Prompts, message text, patches, answers, and credentials are represented by redacted metadata rather than stored verbatim. The read-only `t3_audit_log` tool provides bounded filtering by time, source, event, operation, and outcome so a later agent can review the gateway's behavior through MCP, including when T3 is disconnected.

The audit trail is best-effort: an unavailable audit filesystem does not block T3 work. Protect the data directory and apply a retention policy appropriate for the host; the file grows over time.

For a compact tool-usage review, call `t3_audit_log` with `{ "source": "mcp", "event": "tool.call", "limit": 100 }`, then request later pages with the returned `nextCursor`. Use `source: "t3"` or `source: "git"` to inspect downstream work.
