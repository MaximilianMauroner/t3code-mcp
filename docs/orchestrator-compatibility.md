# Orchestrator compatibility

[Back to README](../README.md)

T3 [PR #2829](https://github.com/pingdotgg/t3code/pull/2829), merged on 2026-10-02, introduces orchestration protocol 2. It merged after `v0.0.45` was released. A review of that release alone does not establish compatibility with V2.

The source target is merge commit [`de343914273eceb852a1d1d739cd1d38df7796ee`](https://github.com/pingdotgg/t3code/tree/de343914273eceb852a1d1d739cd1d38df7796ee). The review covered `environmentHttp.ts`, `environment.ts`, `orchestrationV2.ts`, `project.ts`, server `orchestration-v2/http.ts`, `ws.ts`, and the Effect JSON RPC envelope and serializer.

## Gateway behavior

The public MCP tools and journal format stay the same. The T3 boundary selects its transport from the public descriptor's `orchestrationProtocolVersion`. An absent version means V1. Unknown versions fail before dispatch.

| Operation | V1 | V2 |
| --- | --- | --- |
| Shell read | HTTP shell snapshot | HTTP shell with `x-t3-orchestration-protocol: 2`, plus archived-shell RPC |
| Thread read | HTTP thread snapshot | HTTP thread projection with the protocol header |
| Project creation | HTTP orchestration dispatch | HTTP `/api/projects/mutate` |
| Thread control | HTTP orchestration dispatch | Effect JSON RPC at `/ws?orchestrationProtocol=2` |
| Start a turn | `thread.turn.start` | Persist thread runtime/interaction modes, then `message.dispatch` |
| Interrupt | `thread.turn.interrupt` | `run.interrupt` with the observed run ID |
| Respond to input or approval | Separate response commands | `runtime-request.respond` |

Both transports use the configured bearer credential. Each RPC connection handles one request, has a timeout, and closes after success, failure, cancellation, or disconnection. Mutations are never automatically replayed after a disconnect. A send can persist thread settings before message admission fails; an uncertain result must be reconciled with the same gateway receipt.

`src/t3/v2.ts` maps V2 runs and messages into the gateway's existing turn fields. T3 run IDs populate `t3TurnId`, `observedTarget.turnId`, and message `turnId`; these are not provider-native IDs. Queue entries do not replace the run that is executing, and child-node responses do not become the root run's answer.

Pending-action detail comes from pending runtime requests with a `live` or `message` response capability. Resolved, expired, cancelled, and `not_resumable` requests are excluded. Structured failure code and reset time come from the matching run's error item. `usage_limit` maps to `quota`, and `provider_error` maps to `provider_internal`. Other failure classes remain `unknown` in the existing MCP failure contract.

The gateway still rejects sends to busy threads and polls for updates. This change does not expose V2 queue, steering, fork, or delegation features. Composite tasks retain the gateway's existing explicit worktree preparation. V2 rejects the internal V1 bootstrap payload; MCP task creation uses separate thread creation and message admission.

## Compatibility lifetime

Both V1 and V2 remain supported for deployments and comparison testing, including upgrades and rollbacks. The V1 path in `src/t3/http-client.ts` and its wire tests remain until an explicit decision ends V1 deployment and test support. No version override or speculative fallback is added.

The same client can switch protocols after `getDescriptor()` refreshes the advertised version. Restarting the gateway also clears its cached descriptor. A regression test switches V2 to V1 and back on the same upstream URL and verifies snapshot reads, protocol headers, and HTTP versus WebSocket dispatch.

The V2 projection adapter is required while the public MCP contract and stored receipts use turn fields. It can be replaced only with a planned MCP contract and receipt migration. T3's V1 importer preserves message IDs but imports messages with no run ID. Old V1 run receipts therefore cannot recover execution status from imported history. They remain readable; no V2 run identity is invented for them.

## Verification and rollout

Local fixtures cover both transports. V2 coverage includes composite task creation, duplicate sends, archived threads, run association, pending requests, settings, interruption, structured failures, unknown versions, RPC errors, disconnection, and timeout/cancellation.

Live V2 verification remains required. After selecting a V2 build, restart the gateway, run `status`, `doctor`, and `smoke`, and run the opt-in live integration test with a valid configured provider/model. Confirm that the upstream proxy supports `/ws` upgrades. No deployment or production data migration is performed by this gateway change.
