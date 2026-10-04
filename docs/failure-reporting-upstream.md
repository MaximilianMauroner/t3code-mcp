# Failure reporting: upstream evidence and remaining work

The gateway uses the V1 and V2 transports already on main. This change adds
failure evidence and keeps the existing MCP fields and journal records valid.

## Evidence checked on 2026-10-02

- Gateway base: `f08322b`, including failure retention from PR #2 and V2 support.
- Upstream issue [#10545](https://github.com/pingdotgg/t3code/issues/10545) closed
  as completed at 19:22:34 UTC. Its proposed V1 `lastErrorClass` change did not
  land through [#10550](https://github.com/pingdotgg/t3code/pull/10550), which closed
  without merging.
- Upstream [#2829](https://github.com/pingdotgg/t3code/pull/2829) merged at 19:22:22
  UTC, commit `de343914273eceb852a1d1d739cd1d38df7796ee`.
- Read-only live HTTP descriptor reported protocol 1 and version
  `0.0.45-nightly.20261002.2595`, rather than the older version in the task brief.
  No service or tunnel was restarted. No database query or provider turn was run.
- Sanitized live V1 fixtures and V2 adapter-test fixtures are documented in
  [the fixture source notes](failure-fixture-sources.md).

At the merged upstream commit, `packages/contracts/src/orchestrationV2.ts`
defines the failure classes and fields. `packages/shared/src/orchestrationV2ThreadError.ts`
binds failures to a failed run and its root node, excludes recovered retry items,
and orders errors by update time, ordinal, then ID. It clears the shell class
when a distinct session error replaces the root error.

`ClaudeAdapterV2.ts` carries reset times from rejected `rate_limit_event` data.
`CodexAdapterV2.ts` carries reset times from the rate-limit snapshot, including
late updates to the failed error item. There is no `failure.retryAfter` field.
The error item's `retry` records attempt counts and the reported `retryDelayMs`.
The gateway exposes that object as progress rather than inventing a deadline.

## Minimal upstream patch ideas

1. Keep provider error subtype separate from the broad V2 `usage_limit` class.
   Codex currently maps both `usageLimitExceeded` and `rateLimitExceeded` to
   `usage_limit`; its code allows the gateway to distinguish them. Claude also
   maps some HTTP 429 results to that class, with code `api_error_429`. Preserve
   the SDK/provider error type in an additive `failure.providerErrorType` field,
   and use it before HTTP status when classifying a credit or auth refusal.
2. Bind session/start failures to their admission or run. Carry the existing
   request/message identity onto `provider.turn.start.failed` and include an
   authoritative `lastErrorRunId` or `lastErrorTurnId` in status snapshots.
   A session timestamp after the previous run cannot identify the new attempt.
3. If V1 remains supported upstream, carry the known runtime class, provider
   subtype, and reported reset time into its turn-bound activity payload.
   Account-wide usage data alone cannot supply a particular failed run's reset.

## Draft upstream issue text

Title: Preserve provider failure subtype and run identity in orchestration status

V2 improves failure reporting with root error items and reset metadata. Two
remaining gaps prevent a status client from reporting a precise refusal:

- `usage_limit` includes Codex `rateLimitExceeded` and Claude `api_error_429`.
  HTTP 429 can also mean credits are required. Please retain the explicit
  provider error type beside the broad class, without inferring quota from status.
- A distinct session/start failure can replace the shell error without a run
  identity. In V1, live `provider.turn.start.failed` activities have null turn IDs
  and occur after the previous failed turn. They cannot safely explain that turn.

Suggested small change: add a nullable provider subtype to the persisted failure
and an explicit run/admission identity to session/start errors. Preserve reset
and retry data only when the provider sends it. Test a credits-required 429, a
transient rate limit, a real quota stop, and a start failure after an older failed
run. If V1 is maintained, expose the same available metadata on its activities.

This issue text is a draft for Max. No upstream issue, comment, or PR was opened.
