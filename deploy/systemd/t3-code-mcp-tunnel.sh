#!/bin/sh
# Template: render host-specific values with `node dist/cli.js setup`.
# Required env (from the operator-owned env file, mode 600):
#   MCP_BEARER_TOKEN, CONTROL_PLANE_API_KEY
# Non-secret values below are defaults for `setup`; edit via setup flags,
# not by committing host IDs here.

set -eu

: "${MCP_BEARER_TOKEN:?MCP_BEARER_TOKEN must be set}"
: "${CONTROL_PLANE_API_KEY:?CONTROL_PLANE_API_KEY must be set}"

# tunnel-client resolves an entire header value from an env: reference. Build
# the scheme-qualified value at launch so it stays synchronized with the
# gateway token without putting the token in ExecStart or a second secret file.
MCP_AUTH_HEADER="Bearer ${MCP_BEARER_TOKEN}"
export MCP_AUTH_HEADER

attempt=0
while [ "$attempt" -lt 60 ]; do
  if /usr/bin/curl --silent --fail --max-time 2 http://127.0.0.1:8787/healthz >/dev/null 2>&1; then
    break
  fi
  attempt=$((attempt + 1))
  /usr/bin/sleep 1
done
if [ "$attempt" -ge 60 ]; then
  echo "gateway health check did not become ready" >&2
  exit 1
fi

exec /usr/local/bin/tunnel-client run \
  --control-plane.tunnel-id=tunnel_YOURS \
  --control-plane.api-key=env:CONTROL_PLANE_API_KEY \
  --mcp.server-url=url=http://127.0.0.1:8787/mcp,channel=main \
  '--mcp.extra-headers=Authorization: env:MCP_AUTH_HEADER' \
  '--mcp.discovery-extra-headers=Authorization: env:MCP_AUTH_HEADER' \
  --health.listen-addr=127.0.0.1:8080
