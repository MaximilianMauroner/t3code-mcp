# t3-code-mcp

`t3-code-mcp` is a standalone MCP gateway for one configured remote T3 Code environment. T3 remains the authority for projects, threads, coding-agent sessions, workspaces, and execution. The gateway owns only MCP-facing policy and a small operation journal.

The first slice is live against T3 `v0.0.41-nightly.20260910.1507` and uses T3’s authenticated HTTP orchestration boundary:

- read-only project, thread, message, and connection tools;
- project registration, thread creation, agent turns, bounded waits, interruption, pending-action responses, and archiving;
- environment identity and freshness on results;
- idempotency keys persisted before T3 dispatch;
- no automatic replay after a disconnect; uncertain mutations are reconciled instead;
- Streamable HTTP and stdio MCP transports.

Terminal tools, managed command jobs, file/Git inspection, MCP OAuth, and multi-environment routing are intentionally not part of this first release. The HTTP transport currently uses a configured static bearer token at the gateway boundary; keep it behind a trusted network or reverse proxy until the OAuth authorization server is added.

## Run it

Use a T3 bearer access token as a server-side environment variable. Do not put the T3 token or gateway token in an MCP prompt.

```sh
cp .env.example .env
export T3_HTTP_BASE_URL=http://127.0.0.1:3773
export T3_ACCESS_TOKEN='server-side-t3-token'
export MCP_BEARER_TOKEN='controller-token'
export MCP_TRANSPORT=http
node dist/cli.js serve
```

The MCP endpoint is `http://127.0.0.1:8787/mcp`. For a local host that launches an MCP process, leave `MCP_TRANSPORT` unset and run `node dist/cli.js serve` over stdio.

Build and test without relying on a globally installed toolchain:

```sh
pnpm install
pnpm build
pnpm test
pnpm typecheck:test
```

The default suite is disposable and credential-free. It exercises the typed T3 HTTP boundary, all implemented gateway operations, journal recovery and idempotency, MCP tool schemas and errors, Streamable HTTP authentication, stateless clients, body limits, stale state, and ambiguous dispatch reconciliation.

The real-environment acceptance test is opt-in. It uses the configured Codex Pro provider and Spark model (`instanceId=codex_openai`, `model=gpt-5.3-codex-spark`), creates a disposable thread, starts a no-file-change prompt, reconnects through a second gateway instance, waits for the run, and archives the thread:

```sh
T3_LIVE_ACCESS_TOKEN='server-side-t3-token' \
T3_LIVE_PROJECT_ID='remote-project-id' \
pnpm test:live
```

Set `T3_LIVE_HTTP_BASE_URL` and `T3_LIVE_ENVIRONMENT_ID` when the live environment is not the local default. The live test is skipped unless `T3_LIVE_TESTS=1` is set by the script and a token is supplied. Never commit or pass the token to an MCP prompt.

## Connection spike

The safe spike reads the live environment, identifies the T3 version, and lists projects:

```sh
T3_ACCESS_TOKEN='server-side-t3-token' node dist/cli.js spike
```

To create a thread and submit one prompt, opt into the mutation path explicitly and choose an existing T3 project:

```sh
T3_ACCESS_TOKEN='server-side-t3-token' \
T3_SPIKE_ENABLE_MUTATIONS=true \
T3_SPIKE_CONFIRM=I_UNDERSTAND \
T3_SPIKE_PROJECT_ID='remote-project-id' \
T3_SPIKE_PROMPT='Investigate the failing tests and report your findings.' \
node dist/cli.js spike
```

The command prints the accepted thread and gateway-owned `runId`. Disconnecting the caller does not cancel that T3 run; use `t3_run_get` or `t3_run_wait` from another MCP client to reconcile it.

## Tool boundary

The gateway exposes task-specific tools rather than a generic RPC escape hatch: `t3_connection_status`, project and thread listing/creation, thread detail/history, `t3_thread_send`, run inspection/wait/interruption, pending-action inspection/response, and thread archive.

Mutation tools require an `idempotencyKey`. The journal stores the caller’s key, payload hash, command ID, operation handle, and reconciliation fields. It never stores the original prompt or credentials. A retry with the same key and different input fails; a connection failure leaves the operation uncertain and does not replay raw terminal input or an agent turn.

The configured `T3_ACCESS_TOKEN` is checked for `orchestration:operate` before control tools dispatch. A read-only T3 credential cannot trigger execution through this gateway. `MCP_READ_ONLY=true` is an additional endpoint-level guard.

## Auto-start on Linux

This repository includes hardened user-level systemd units for the gateway and
the OpenAI Secure MCP Tunnel. They use the fixed T3 environment and tunnel ID
for this deployment, restart after unexpected exits, bind local listeners to
loopback, and start the gateway in read-only mode.
The tunnel unit derives the `Bearer` header at launch from `MCP_BEARER_TOKEN`
and waits for the gateway health endpoint before probing MCP. This keeps the
secret out of `ExecStart` and avoids a startup race. `ProtectKernelModules` is
omitted because this host's user systemd rejects that restriction; the other
compatible hardening controls remain enabled.

Install and enable them after building:

```sh
test -e "$HOME/.config/t3-code-mcp.env" || install -m 600 deploy/systemd/t3-code-mcp.env.example "$HOME/.config/t3-code-mcp.env"
install -d -m 700 "$HOME/.config/systemd/user"
install -m 644 deploy/systemd/t3-code-mcp.service "$HOME/.config/systemd/user/t3-code-mcp.service"
install -m 644 deploy/systemd/t3-code-mcp-tunnel.service "$HOME/.config/systemd/user/t3-code-mcp-tunnel.service"
systemctl --user daemon-reload
systemctl --user enable t3-code-mcp.service t3-code-mcp-tunnel.service
```

Edit `~/.config/t3-code-mcp.env` and set `T3_ACCESS_TOKEN`, a separate
random `MCP_BEARER_TOKEN`, and the OpenAI runtime `CONTROL_PLANE_API_KEY`.
Start and verify only after those values are present:

```sh
systemctl --user start t3-code-mcp.service t3-code-mcp-tunnel.service
systemctl --user status t3-code-mcp.service t3-code-mcp-tunnel.service
curl -fsS http://127.0.0.1:8787/healthz
curl -fsS http://127.0.0.1:8080/readyz
```

Keep `MCP_READ_ONLY=true` until the read-only ChatGPT connection is verified;
then change it deliberately and restart the gateway before using control tools.

## Remote ownership and compatibility

All project, thread, workspace, and run operations go to `T3_HTTP_BASE_URL`. The gateway does not read T3’s database, manipulate local project files, automate the T3 UI, or create provider sessions outside T3. Pin and test the T3 server version used by a deployment; the HTTP contract is an integration boundary, not a promise that arbitrary T3 builds are compatible.
