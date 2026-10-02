# Development and verification

[Back to README](../README.md)

Run the commands below from the repository root.

```sh
pnpm build
pnpm test
pnpm typecheck:test
```

The default suite uses disposable local T3 fakes, disposable Git repositories/worktrees, and no real credentials. It covers the HTTP boundary, gateway operations, composite task concurrency/recovery, journal migration and idempotency, MCP schemas and errors, HTTP authentication, stateless clients, body limits, message truncation, stale state, turn-bound failure reporting, uncertain dispatch reconciliation, T3-grounded Git workspace selection, staged/unstaged and committed revision diffs, literal path/revision validation, explicit truncation, renames, binary changes, conflicts, missing/non-Git paths, and audit persistence/redaction/filtering.

For a read-only connection diagnostic:

```sh
node --env-file=.env dist/cli.js status
node --env-file=.env dist/cli.js doctor
node --env-file=.env dist/cli.js smoke
node --env-file=.env dist/cli.js spike
```

`doctor` checks config, journal writability, T3 identity/scopes/expiry, effective access, freshness, build fingerprint, a real local MCP `tools/list` exchange, a harmless overview call, and tunnel readiness without mutation. Its output includes a machine-readable capability manifest. To compare an actual host capture, set `MCP_HOST_TOOL_NAMES_JSON` to a JSON array of the tool names that host discovered before running `doctor`; missing and unexpected operations fail the `host_discovery` check. `smoke` calls status plus the bounded overview path MCP clients use and validates highlight/excerpt bounds. After every interface deployment, run `smoke`, restart gateway then tunnel, force fresh client discovery, compare the discovered `toolSchemaFingerprint`, and invoke status plus overview from your MCP client ("What's running and does anything need me?").

`spike` also lists up to five projects. Its optional mutation path requires `T3_SPIKE_ENABLE_MUTATIONS=true`, `T3_SPIKE_CONFIRM=I_UNDERSTAND`, `T3_SPIKE_PROJECT_ID`, and `T3_SPIKE_PROMPT`. Optional `T3_SPIKE_THREAD_TITLE` and `T3_SPIKE_IDEMPOTENCY_KEY` customize the thread title and retry-key prefix. This path creates a thread and submits a prompt; it does not archive the thread afterward.

The opt-in live test starts a recoverable composite task, asks the configured provider to run a small check without editing files, finds that task through a second gateway instance without carrying its run ID, observes the run, and attempts to archive the thread. It currently selects `instanceId=codex_openai` and `model=gpt-5.3-codex-spark`, which must exist in the target T3 environment:

```sh
T3_LIVE_ACCESS_TOKEN='server-side-t3-token' \
T3_LIVE_PROJECT_ID='remote-project-id' \
pnpm test:live
```

Set `T3_LIVE_HTTP_BASE_URL` and `T3_LIVE_ENVIRONMENT_ID` for another environment. If no project ID is supplied, the test uses the first listed project. `test:live` sets `T3_LIVE_TESTS=1`; the test still skips without a token. This verifies the T3 integration, while the [client connection check](configuration.md#connect-an-mcp-client) verifies the complete workflow.

## Usage and monitoring regression checks

`tests/usage-improvements.test.ts` exercises adaptive wait request counts with fake time, pending input/approval, disconnection, environment revalidation, failure isolation, settings inheritance and restart provenance. `tests/mcp-server.test.ts` includes a caller journey with no duplicate dispatch plus concurrent audit attribution. `tests/usage-summary.test.ts` verifies filtered streaming cursors, fixed-snapshot appends, invalid lines, aggregation and context isolation.

For bounded-memory evidence, build and run the disposable audit benchmark:

```sh
pnpm build
pnpm benchmark:audit
```

It scans 131,072 synthetic events (about 75 MB) for both a one-item page and a usage summary under a 32 MB JavaScript heap, reports elapsed time/peak RSS, and removes its fixture. RSS includes native/runtime memory and is not the JavaScript heap limit. This is an offline workload measurement, not a claim of production token/cost savings.

Prompt comparison scenarios live in `evals/task-briefs.json`; [the evaluation recipe](task-briefs.md#offline-evaluation) explains the external caller boundary and evidence required for a model-quality comparison.

## Upstream compatibility review

See [orchestrator compatibility](orchestrator-compatibility.md) for the merged V2 source target, transport changes, and rollout limits. `tests/v2-compatibility.test.ts` uses a disposable HTTP/WebSocket server to cover protocol negotiation, archived listings, composite task creation, send idempotency, run association, settings, input responses, interruption, structured failures, disconnection, tagged RPC errors, and cancellation/timeout. `tests/http-client.test.ts` covers V1 transport and tagged HTTP errors.

This is local boundary verification. Run the read-only diagnostics and opt-in live integration test against the intended deployed V2 build before treating it as live verified. The configured route must support authenticated WebSocket upgrades at `/ws` as well as the HTTP API. Restart the gateway when upgrading T3 so its cached descriptor is refreshed.
