# Protocol refresh acceptance handoff

This plan checks that a gateway stays usable when T3 changes orchestration
protocol. It is a plan for later execution. No live acceptance run, deployment,
service restart, provider write, or live setting change was performed here.

Scope: [PR #4](https://github.com/MaximilianMauroner/t3code-mcp/pull/4).
The [review record](https://github.com/MaximilianMauroner/t3code-mcp/pull/4#issuecomment-5973976840)
holds the current head, base, checks, findings, and review limits.

## Evidence already available

| Requirement | Local or synthetic evidence | Remaining live evidence |
| --- | --- | --- |
| Recover the first read after an upgrade | `tests/v2-compatibility.test.ts`: first shell/thread read after V1 startup discovery; same client and upstream URL | Same gateway process before and after a controlled upstream transition |
| Select the current mutation transport | First snooze after an upgrade; one command, V2 RPC, no V1 dispatch | One snooze on an approved disposable thread, without invoking a provider |
| Support rollback | Same-client V1/V2 transitions, including response-schema recovery | Controlled rollback in an approved isolated environment, if required by rollout policy |
| Bound retries and concurrency | `tests/http-client-protocol.test.ts`: shared discovery, separate cancellation, at most one read retry after a confirmed version change | Request counts for simultaneous callers during the transition |
| Preserve errors and mutation safety | Unchanged HTTP/schema errors, rejected preflight receipts, safe fresh-key retry, V1 timeout and V2 disconnect uncertainty | Sanitized results and command counts from an isolated fault test, if separately approved |
| Preserve provider failure evidence | Merged PR #3 tests and shell-row source-sequence checks run with the protocol repair | Correct thread/run association on the selected live V2 build |

Healthy reads keep cached negotiation. A failed read refreshes on HTTP 400 or an
invalid response schema, then retries once only if the version changed. Dispatch
refreshes before sending. A preflight failure rejects an unsent command and gives
safe new-key retry guidance. A failure after sending remains subject to the
existing uncertainty and reconciliation rules. No mutation replay was added.

## Owner and prerequisites

Max selects the acceptance operator and explicitly authorizes the named test
environment, candidate gateway setup, controlled T3 upgrade or rollback, and
disposable-thread snooze/unsnooze. These actions are not authorized by this plan.
Use an isolated environment with no production data or provider work in progress.

The operator records the candidate PR head, T3 versions and advertised protocols,
environment ID, gateway PID/start time, and sanitized request results. Keep
credentials in the existing approved credential mechanism. Do not print tokens,
request authorization headers, user messages, or private thread content.

Verify that the named upstream route supports HTTP snapshots and authenticated
WebSocket upgrades at `/ws`. Use a disposable, idle, unsnoozed thread with no
pending approval, user input, or queued turn. Record its initial state and ID.
Do not silently select a different environment or create provider work.

## Controlled live protocol transition

1. The operator prepares the approved candidate gateways before the transition.
   Use two isolated gateway processes if testing both first-operation paths:
   one for reads and one for snooze, with separate `T3_MCP_DATA_DIR` values
   and separate listener ports if using HTTP. Record both PIDs and start times. Discover
   V1 and make a successful baseline read through each gateway's existing MCP
   connection. Do not restart either gateway during the test.
2. The operator performs the separately authorized upstream transition to the
   selected V2 build. Record the upstream version and protocol from an independent
   descriptor read. Do not call gateway connection status, refresh its descriptor,
   or run other gateway tools between the transition and the first operation.
3. On the read gateway, call `t3_thread_get` with the recorded thread ID. Two
   simultaneous first reads can also check concurrency during the transition.
   Then call `t3_threads_overview` or `t3_threads_list` with bounded output. Require successful
   responses for the same environment and thread. Capture sanitized upstream
   request counts. The initial stale read may receive HTTP 400 internally; it
   must recover without surfacing that error when discovery advertises V2.
4. On the mutation gateway, make its first post-transition call
   `t3_thread_snooze` with the recorded ID, preset `hour`, and a unique recorded
   idempotency key. Require `accepted`, one snooze command through V2 RPC, no V1
   dispatch attempt, and a read showing the new wake time. Repeat the identical
   call with the same key; require no second snooze command. Use separately
   authorized `t3_thread_unsnooze` to restore the initial state and verify it.
   Gateway snooze reads the thread before dispatch, so this checks the first-tool
   recovery and mutation routing. The direct-client synthetic snooze case is
   the separate evidence for dispatch refreshing stale negotiation itself.
5. Run concurrent bounded reads through the read gateway. Require valid V2
   responses and no schema/header mismatch. Use the synthetic tests for precise
   refresh-count and cancellation limits unless the live route exposes enough
   sanitized request evidence to measure them.
6. Compare both gateway PIDs and start times with the baseline. They must be
   unchanged. A CLI command that creates a new client does not prove this claim.
   If only one gateway was approved, record that the independent first-operation
   mutation case was not live verified.

After the first-operation checks, optional read-only diagnostics are
`node dist/cli.js status`, `node dist/cli.js doctor`, and
`node dist/cli.js smoke`, using the approved environment already supplied to the
process. They do not prove same-client protocol refresh. The existing restart
guidance in other documents must not be used between the baseline and assertions.

If the operator also approves rollback testing, repeat the read procedure across
V2 to V1 without restarting the gateway. Inject discovery outages or lost
mutation responses only in an isolated environment with separate authorization.
Do not resubmit an uncertain operation with a fresh key. Confirmed preflight
rejections may use a new key after discovery recovers; same-key rejected receipts
remain rejected.

## Broader live integration test

`pnpm test:live` runs `tests/live.integration.test.ts`. It creates a thread,
starts a provider task, reads it through a second gateway, and attempts to archive
it. It does not switch the upstream protocol while one client remains alive.
Its success cannot replace the controlled transition check above.

Do not run this test under the current restrictions. Before a future authorized
run, the operator must verify the test's configured `codex_openai` provider and
`gpt-5.3-codex-spark` model exist in the named environment. Supply all of
`T3_LIVE_HTTP_BASE_URL`, `T3_LIVE_ENVIRONMENT_ID`, `T3_LIVE_PROJECT_ID`, and the
credential through the approved process environment; do not rely on the localhost
or first-project defaults. A missing provider/model is a blocker, not a reason
to silently substitute one. The test's archive attempt catches errors, so inspect
the recorded thread afterward and confirm cleanup explicitly.

## Acceptance receipt and remaining gates

The operator records each check as passed, failed, or not run, with the head,
target environment/build, process continuity, safe request counts, receipt IDs,
and cleanup result. Link sanitized evidence to the PR review record. Do not infer
live acceptance from local checks, CI, code review, or an accepted mutation.

The agent owns current-head CI and code-review tracking. Max owns the decision
to authorize live acceptance; the selected operator owns execution and cleanup.
The 2026-10-04 base-interaction check found one inherited Sunday snooze failure:
`tests/thread-lifecycle.test.ts` requests `next-week` even when it collapses into
`tomorrow`. The same test fails on current `main` without this PR. Max or the
snooze owner must resolve that separate issue before all project checks can pass.
The PR remains unmerged under the explicit task restriction. No temporary
compatibility layer was added. Existing V1 support in `src/t3/http-client.ts`
and its tests remains until V1 deployment and test support explicitly end.
