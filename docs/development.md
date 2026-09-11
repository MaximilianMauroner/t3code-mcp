# Development and verification

[Back to README](../README.md)

Run the commands below from the repository root.

```sh
pnpm build
pnpm test
pnpm typecheck:test
```

The default suite uses disposable local T3 fakes, disposable Git repositories/worktrees, and no real credentials. It covers the HTTP boundary, gateway operations, journal recovery and idempotency, MCP schemas and errors, HTTP authentication, stateless clients, body limits, message truncation, stale state, uncertain dispatch reconciliation, T3-grounded Git workspace selection, staged/unstaged status and diffs, literal path filtering, explicit truncation, renames, binary changes, conflicts, and missing/non-Git paths.

For a read-only connection diagnostic:

```sh
node --env-file=.env dist/cli.js status
node --env-file=.env dist/cli.js doctor
node --env-file=.env dist/cli.js smoke
node --env-file=.env dist/cli.js spike
```

`doctor` checks config, journal writability, T3 identity/scopes/expiry, effective access, freshness, build fingerprint, MCP discovery, a harmless overview call, and tunnel readiness without mutation. `smoke` calls status plus the bounded overview path MCP clients use and validates highlight/excerpt bounds. After every interface deployment, run `smoke`, restart gateway then tunnel, force fresh client discovery, compare the discovered `toolSchemaFingerprint`, and invoke status plus overview from your MCP client ("What's running and does anything need me?").

`spike` also lists up to five projects. Its optional mutation path requires `T3_SPIKE_ENABLE_MUTATIONS=true`, `T3_SPIKE_CONFIRM=I_UNDERSTAND`, `T3_SPIKE_PROJECT_ID`, and `T3_SPIKE_PROMPT`. Optional `T3_SPIKE_THREAD_TITLE` and `T3_SPIKE_IDEMPOTENCY_KEY` customize the thread title and retry-key prefix. This path creates a thread and submits a prompt; it does not archive the thread afterward.

The opt-in live test creates a disposable thread, asks the configured provider to run a small check without editing files, reads the journal through a second gateway instance, observes the run, and attempts to archive the thread. It currently selects `instanceId=codex_openai` and `model=gpt-5.3-codex-spark`, which must exist in the target T3 environment:

```sh
T3_LIVE_ACCESS_TOKEN='server-side-t3-token' \
T3_LIVE_PROJECT_ID='remote-project-id' \
pnpm test:live
```

Set `T3_LIVE_HTTP_BASE_URL` and `T3_LIVE_ENVIRONMENT_ID` for another environment. If no project ID is supplied, the test uses the first listed project. `test:live` sets `T3_LIVE_TESTS=1`; the test still skips without a token. This verifies the T3 integration, while the [client connection check](configuration.md#connect-an-mcp-client) verifies the complete workflow.
