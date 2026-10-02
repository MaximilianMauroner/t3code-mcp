# Failure payload sources

V1 fixtures are selected fields from read-only T3 HTTP thread snapshots, fetched
on 2026-10-02. The server descriptor reported protocol 1 and version
`0.0.45-nightly.20261002.2595`. Thread, turn, activity, and message IDs are replaced.
Stack traces and private host addresses are removed. Error sentences and turn
and session times are retained. No database query was used.

- `v1-claude-rate-limit.json`: matching `runtime.error` and assistant provider
  `rate_limit_error`. The session was later stopped.
- `v1-codex-usage-limit.json`: matching `runtime.error`, no API-error assistant.
- `v1-credits-required.json`: matching generic usage-limit activity and the more
  specific assistant credit refusal. Both carry the same turn ID.
- `v1-auth-unavailable.json`: a provider assistant message from an older failed
  turn. Tests put it on a matching failed turn and separately test stale IDs.
- `v1-unbound-start-error.json`: start-failure activities have null turn IDs and
  occur after the previous failed turn completed. They cannot explain that turn.

V2 has not been read from a live server. `v2-provider-failures.json` contains
contract fixtures based on adapter replay tests in merged upstream PR #2829 at
`de343914273eceb852a1d1d739cd1d38df7796ee`:

- `apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.test.ts`, the
  usage-limit/known-reset replay cases.
- `apps/server/src/orchestration-v2/testkit/fixtures/claude_result_is_error/output.ts`,
  the authentication failure assertions.

The failure shape is defined in `packages/contracts/src/orchestrationV2.ts`.
Tests add run, root-node, status, and ordinal fields to check correlation.
