# Usage and tool behavior

[Back to README](../README.md)

## Example workflow

| Request | Tools the client uses |
| --- | --- |
| “Find my website project.” | `t3_projects_list` with `query` |
| “What about the threads?” | `t3_threads_overview` for counts plus running threads |
| “What threads are open in that project?” | `t3_threads_list` with `projectId` and `status: "open"` |
| “What is running right now?” | `t3_threads_list` with `onlyRunning: true`, or `sessionStatus: "running"` |
| “Show snoozed / settled threads.” | `t3_threads_list` with `status: "snoozed"` or `"settled"` |
| “How is the login fix going?” | `t3_threads_list`, `t3_thread_get`, and `t3_thread_messages` as needed |
| “Start a thread in that project and investigate the failing tests.” | `t3_thread_create`, then `t3_thread_send` |
| “Any update?” | `t3_run_get` or `t3_run_wait` for the returned run handle |
| “What does it need from me?” | `t3_pending_actions_list` |
| “Approve that request.” | `t3_pending_action_respond` with the request ID and decision |
| “Stop that thread.” | `t3_thread_get`, then `t3_thread_interrupt` with the observed `latestTurn.turnId` |
| “Follow up and ask it to run the tests.” | `t3_thread_send` on the existing idle thread |
| “Snooze this thread.” | `t3_thread_snooze` with no preset (this evening, else tomorrow morning) |
| “Snooze it for an hour / 3 hours / tomorrow / next week.” | `t3_thread_snooze` with `preset: "hour"`, `"three-hours"`, `"tomorrow"`, or `"next-week"` |
| “Wake this thread up.” | `t3_thread_unsnooze` |
| “Settle this thread.” / “Mark it done.” | `t3_thread_settle` |
| “Reopen that thread.” | `t3_thread_unsettle` |
| “Archive that thread.” | `t3_thread_archive` |

The client should resolve project and thread names to IDs, keep those IDs and returned run handles in conversation context, and present concise summaries. It should ask for clarification when a name or action is ambiguous. These are client responsibilities; the gateway returns structured results and does not manage the client interface or conversation context.

A task can keep running in T3 after the client disconnects. To check it later, the client calls the same gateway with the saved `runId`. If that handle is unavailable, it can still find the thread and read its latest state and messages. The gateway has no run-list tool.

## Find and check on existing work

`t3_projects_list` accepts an optional `query` matching a case-insensitive substring of the project title, workspace path, or ID. `t3_threads_list` accepts `projectId`, a `query` matching title, branch, or ID, a lifecycle `status`, plus execution filters (`activity`, `onlyRunning`, `sessionStatus`) kept separate from lifecycle. `needsAttention` filters pending approval/input, inconsistent/stale observations, and failures. `sort` is deterministic (`recent`/`title`/`status`); `detail` is `summary` or `full` (full adds latest-response enrichment on the page only). Filters combine and apply before cursor pagination. Zero results return a `resolutionHint` retry note; one exact candidate may be selected; multiple candidates must be presented with project/title/branch/activity for clarification, never an invented newest choice. `t3_threads_overview` accepts the same `projectId`/`query`/`includeArchived` scope and returns `total`, lifecycle `counts`, `executionCounts`, `needsAttentionCount`, `runningCount`, running summaries capped by `runningLimit`, and up to five deterministic `highlights` (pending approval/input, inconsistent/stale, failed, running, recent) with project title and a 200-char response excerpt. All rows share one `observedAt`.

| Thread status | Meaning in this gateway |
| --- | --- |
| `open` | Unarchived work that is neither effectively snoozed nor explicitly settled. Includes pinned threads, running work, and idle threads. |
| `snoozed` | A future snooze time, with no pending approval/input, new failure, or completion since snoozing that would wake it early. Snoozing does not stop execution. |
| `settled` | T3's explicit settled override, subject to activity blockers and pin/snooze precedence. A completed turn alone does not settle a thread. |
| `archived` | Threads with an archive timestamp. This explicit filter includes archives even when `includeArchived` is false. |
| `all` or omitted | No lifecycle filter; archives remain hidden unless `includeArchived=true`. |

The gateway uses the lifecycle fields exposed by T3. The T3 UI also derives settlement from client preferences, inactivity, and linked PR state; those inputs are not available to these tools, so the settled/open lists can differ from the UI's automatic classification. Older servers with no lifecycle fields show unarchived threads as open. This behavior follows the server-backed portion of T3's `threadSettled.ts` and sidebar partitioning, inspected at source revision `4b8388773`.

Thread summaries include `status` (lifecycle), `statusReason`, `activity` (execution), `isRunning`, `quality` (`fresh`/`stale`/`incomplete`/`inconsistent`), `warning` when T3 signals disagree or are incomplete, `observedTurnId`/`observedAt`/`observedTarget` (`environmentId`/`threadId`/`turnId`/`observedAt`), project title, the raw settlement override, snooze time and snooze start, pin timestamp, session status and session update time, latest user-message time, latest turn, pending flags, actionable-plan flag, and background liveness (`working` or `monitoring` when T3 provides it). `hasConflictingSignals` is kept for compatibility and mirrors `quality=inconsistent`. Overview, detail, and run reads share this normalizer and never silently resolve contradictory fields. `t3_thread_get` combines the full thread with these shell fields and its latest response. Use `t3_thread_messages` for more history. `t3_providers_list` aggregates observed `instanceId`/`provider`/`model` labels, per-project defaults, and thread usage for thread creation.

To start work, create a thread if needed and send a message. Call `t3_providers_list` first when a project has no default model; thread creation returns the T3-accepted `modelSelection`, branch, and worktree. To continue existing work, send another message to that thread once it is idle. Busy threads return `thread_busy` with the active turn/session and valid next actions. Uncertain dispatches return the durable operation handle with reconcile guidance: use `t3_run_get`, never resubmit with a fresh key. Creating a thread does not itself start a turn. Runtime mode follows the existing tool defaults; request `approval-required` explicitly when creating a thread if that is the intended T3 permission mode.

`t3_thread_interrupt` works on threads started in T3's UI or by another client, without requiring a gateway `runId`. Read the thread and supply `threadId`, `expectedTurnId` (the `observedTarget.turnId`), and an `idempotencyKey`. The gateway rejects a changed or finished turn before dispatch (`turn_changed`/`thread_not_running` with current target details) and never automatically replays an uncertain interruption. Acceptance returns post-dispatch `verification` (`interrupted`/`still_running`/`target_changed`/`not_running`/`inconsistent`/`unknown`); acceptance alone never confirms a stop. T3 currently interrupts by provider session, so a turn change after the gateway's check remains a race; the expected turn ID is not an atomic upstream condition. Poll `t3_thread_get` to verify the outcome. Interruption does not archive or delete the conversation.

Snooze hides a thread from the inbox until its wake time; it never stops a running agent. `t3_thread_snooze` defaults to this evening (18:00 gateway-local) while meaningfully before evening, else tomorrow morning (09:00); presets `hour`, `three-hours`, `evening`, `tomorrow`, and `next-week` (Monday 09:00) match the T3 clients, or supply an explicit future ISO `snoozedUntil`. Threads blocked on you (pending approval/input) or with a queued turn start cannot be snoozed. `t3_thread_unsnooze` wakes immediately. `t3_thread_settle` marks a thread done and clears snooze and pin; it is blocked while the thread runs, has a pending approval, or has a queued turn start. `t3_thread_unsettle` reopens. All four are idempotent mutations with the same journal, read-only, and scope handling as the other control tools.

Thread deletion, workspace deletion, checkpoint rollback, and arbitrary terminal commands are not exposed.

## What is implemented

The gateway uses T3’s authenticated HTTP orchestration API. The recorded integration target is T3 `v0.0.41-nightly.20260910.1507`; pin and test the version used by your deployment.

- Project search and registration, thread search with lifecycle/execution/attention filters and deterministic sort, one-call bounded overviews with execution counts and highlights, provider discovery, thread creation with T3-accepted workspace, compact check-ins, and paginated messages with bounded text.
- Starting agent turns with typed busy rejection, inspecting runs with shared observation quality, polling for changes, requesting interruption by gateway run handle or observed thread turn with post-dispatch verification, responding to approval or user-input requests, snoozing/settling threads, and archiving threads.
- Connection status with gateway version/commit/fingerprint (`T3_CODE_MCP_COMMIT` plus package version), observation time, effective access mode with callable/disabled operations and stable reason codes (`gateway_read_only`/`t3_scope_required`), upstream T3 scopes, capabilities, freshness, and credential expiry. Run results also report connection, freshness, and thread quality; other tool results include the environment ID and observation time.
- A local operation journal for idempotency and reconciliation after uncertain dispatches.
- Stateless Streamable HTTP at `/mcp`, plus stdio for clients that launch a local process.
- A `setup` command that renders host systemd/tunnel files from explicit flags without secrets, and a read-only `doctor` command for the gateway-to-client chain.

The automated tests exercise the gateway and MCP boundary. Verify the complete integration with your chosen MCP client using the [client connection check](configuration.md#connect-an-mcp-client).

## Run and retry behavior

Mutation tools require an `idempotencyKey`. Generate a new key for each intended action, and reuse the same key and input when retrying that action. Reusing a key with different input fails. The journal stores the key, payload hash, command ID, operation handle, and reconciliation fields. It does not store the original prompt or credential fields.

A mutation can return `accepted`, `rejected`, or `uncertain`. Accepted means T3 accepted command intent, not that the task finished. An uncertain result must not trigger a new turn with a fresh key: the original may already be running. The gateway reconciles available T3 state and does not automatically replay mutations.

Preserve `T3_MCP_DATA_DIR` across gateway restarts. Run one gateway process per journal directory; the JSON journal is not a shared database for multiple active gateway processes. Reconnecting MCP clients use that same gateway and journal.

`t3_run_wait` polls for a relevant change, with a tool timeout parameter of 1–30 seconds. It can return before completion, and a T3 request can extend the elapsed wait. Timing out or losing the client connection does not cancel the T3 run. The client must call again for further updates; there are no push notifications.

## Current limits

Busy threads reject new turns; queueing and steering are not implemented. Pending-action details are inferred from T3 activity records and may include historical entries; the pending flags and available request IDs need to be considered together. The gateway does not provide a separate human-confirmation mechanism for approval responses.

Run inspection relies on the journaled message/turn IDs and T3's latest-turn projection. Status for older runs or runs whose turn ID is not yet known can be incomplete. A provider-owned turn ID is not currently exposed.

There are no terminal tools, managed command jobs, direct file/Git inspection tools, MCP OAuth server, or multi-environment routing. The HTTP endpoint uses a static bearer token; use a trusted network, tunnel, or HTTPS reverse proxy for remote access. The gateway does not read T3's database, manipulate project files locally, or create provider sessions outside T3.
