# Configuration and client connections

[Back to README](../README.md)

Run the commands below from the repository root.

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
| `MCP_TRANSPORT` | `stdio` by default; use `http` for a remote MCP connection |
| `MCP_BEARER_TOKEN` | Required to serve authenticated `/mcp` requests over HTTP |
| `MCP_READ_ONLY` | Reject mutations when `true`; code default is `false` |
| `MCP_HOST`, `MCP_PORT` | HTTP listener; `127.0.0.1`, `8787` |
| `T3_ENVIRONMENT_ID` | Optional expected environment ID for identity checks |
| `T3_ENVIRONMENT_LABEL` | Optional fallback label when discovery is unavailable |
| `T3_MCP_DATA_DIR` | Operation journal directory; `./data` |
| `T3_STALE_AFTER_MS` | Freshness threshold; `30000` |

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
2. Request connection status, list projects, and summarize an existing thread. Confirm that the results come from the intended environment.
3. Set `MCP_READ_ONLY=false` and restart when ready to exercise control tools. The T3 token also needs `orchestration:operate`.
4. Create a disposable thread, start a small task, check progress, and retrieve the result.
5. Disconnect and reconnect the client, then inspect the same thread or saved run handle. Check interruption and pending-action responses when applicable.

Verify that the client clearly reports accepted work, completion, requests for input, and connection failures.

## Integration references

- [MCP and Connectors](https://developers.openai.com/api/docs/guides/tools-connectors-mcp): connecting remote MCP servers through the Responses API, including tool filtering and authorization.
- [Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels): the optional [tunnel deployment](deployment.md).

These references describe OpenAI-specific integration options. Other MCP clients can connect using the transports above; consult your client's documentation for its configuration format.
