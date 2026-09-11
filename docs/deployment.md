# Linux deployment

[Back to README](../README.md)

Run the commands below from the repository root.

First [build and configure the gateway](configuration.md#run-the-gateway), and review the [security and access risks](security.md).

Deployment options include the provided [OpenAI Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels) option, and any MCP host that can launch a local process (stdio) or reach authenticated Streamable HTTP at `/mcp`. The tunnel is one deployment choice; general MCP-host compatibility is separate and depends on the client's supported tool integration.

Render host-specific units with `setup` instead of editing committed paths. `deploy/systemd/*` are templates with placeholder IDs and paths:

```sh
node dist/cli.js setup --workspace=/opt/t3code-mcp --node=/usr/bin/node \
  --tunnel-bin=/usr/local/bin/tunnel-client --tunnel-id=tunnel_YOURS \
  --environment-id=YOUR_ENV_ID --out="$HOME/.config/systemd/user"
```

`setup` writes service files and a launcher template without secrets, creates the env file only when missing (mode 600), and never overwrites existing credentials. The launcher derives the `Bearer` header from `MCP_BEARER_TOKEN` and waits for gateway health before starting tunnel discovery. Credentials are read from `~/.config/t3-code-mcp.env`. The tunnel client must already be installed at the launcher's configured path.

After building, install the rendered units (or copy the templates and edit placeholders):

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
