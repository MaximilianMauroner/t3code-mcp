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
| “What actually changed in that project?” | `t3_git_status` plus `t3_git_diff` for uncommitted work, or `t3_git_compare` for committed base-to-head work |
| “Take this on in that project.” | `t3_task_start` with an explicit `runtimeMode` |
| “Find the task from yesterday.” | `t3_tasks_list`, then `t3_task_get` |
| “Show me what that task changed.” | `t3_result_get` for a task-bound evidence package |
| “Create a thread and start its first message.” | `t3_thread_create` |
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
| “Review how this gateway has been used.” | `t3_audit_log` with filters and pagination |

The client should resolve project and thread names to IDs, keep those IDs and returned run handles in conversation context, and present concise summaries. It should ask for clarification when a name or action is ambiguous. These are client responsibilities; the gateway returns structured results and does not manage the client interface or conversation context.

A task can keep running in T3 after the client disconnects. Composite tasks started with `t3_task_start` have a durable `taskRef` and can be recovered with `t3_tasks_list` / `t3_task_get` even when the conversation did not retain the run ID. Low-level turns can still be checked with their saved `runId`; there is no general list of turns that were started outside a composite task.

## Inspect Git changes directly

`t3_git_status` and `t3_git_diff` are read-only checks performed by the gateway process against a workspace selected from a fresh T3 shell snapshot. Both require an exact `projectId`. Supplying `threadId` also verifies that the thread belongs to that project and selects its T3-recorded `worktreePath`; a thread with no separate worktree falls back to the project's `workspaceRoot`. The result identifies the environment, project, optional thread, T3-selected path, resolved path, repository root, Git directory, common Git directory, and the T3-recorded thread branch. The observed Git branch is reported separately. Relative workspace paths are rejected rather than resolved against the gateway's own working directory.

`t3_git_status` returns branch name/detached/unborn state, HEAD commit, upstream and ahead/behind counts when available, plus structured `staged`, `unstaged`, `untracked`, and `conflicts` categories. Each category contains an exact total `count`, a bounded `items` list, and `truncated`. `maxEntries` is the per-category item limit (default 100, maximum 500). Top-level `truncation` repeats the limit and exact omitted count per category, so a short response is never mistaken for a complete status. Rename/copy rows include both paths and Git's score; conflict rows preserve the two-letter index/worktree state.

`t3_git_diff` accepts `mode: "unstaged"` (working tree versus index, the default) or `mode: "staged"` (index versus HEAD). `t3_git_compare` resolves two explicit revision names to commit IDs and returns their committed patch, so a clean worktree does not hide already committed work. Revision ranges, whitespace, and option-like revisions are rejected. It reports whether the working tree was dirty at observation time; a dirty tree does not alter the committed patch but makes automatic task attribution incomplete. Both tools accept up to 100 literal relative paths and the same bounded `maxBytes` output. Absolute paths, parent traversal, NULs, and Git pathspec magic are rejected. Truncation reports captured, total, and omitted bytes; binary payloads are not emitted.

These tools are available even when `MCP_READ_ONLY=true`; that setting prevents mutations, not source disclosure. They do not expose arbitrary Git arguments, commits, pushes, filesystem reads, or a shell. Git is invoked directly with fixed read-only arguments, optional index locking disabled, external diff/textconv disabled, and inherited `GIT_*` overrides removed. T3 remains the workspace authority, but the gateway service must share the same filesystem namespace and have read access to the T3-recorded worktree. Missing paths, non-Git directories, bare repositories, ambiguous identity, project/thread mismatches, and unavailable Git are returned as explicit tool errors.

## Find and check on existing work

`t3_projects_list` accepts an optional `query` matching a case-insensitive substring of the project title, workspace path, or ID. `t3_threads_list` accepts `projectId`, a `query` matching title, branch, or ID, a lifecycle `status`, plus execution filters (`activity`, `onlyRunning`, `sessionStatus`) kept separate from lifecycle. `needsAttention` filters pending approval/input, inconsistent/stale observations, and failures. `sort` is deterministic (`recent`/`title`/`status`); `detail` is `summary` or `full` (full adds latest-response enrichment on the page only). Filters combine and apply before cursor pagination. Zero results return a `resolutionHint` retry note; one exact candidate may be selected; multiple candidates must be presented with project/title/branch/activity for clarification, never an invented newest choice. `t3_threads_overview` accepts the same `projectId`/`query`/`includeArchived` scope and returns `total`, lifecycle `counts`, `executionCounts`, `needsAttentionCount`, `runningCount`, running summaries capped by `runningLimit`, and up to five deterministic `highlights` (pending approval/input, inconsistent/stale, failed, running, recent) with project title and a 200-char response excerpt. All rows share one `observedAt`. List selection, sorting, state, and overview counts use one shell snapshot. Full response enrichment does not replace that state. If a newer full observation reports another turn or state, the row keeps its shell state and omits response evidence. A V2 snapshot can still enrich the shell-selected turn from its matching historical failure or recovery record. Without that explicit V2 record, conflicting failure evidence is omitted. A V1 successor has no historical recovery record, so the shell-selected turn keeps its bound retained failure. Current thread reads use evidence order for state conflicts and V2 distinct turns, so an omitted shell request time cannot make a newer turn look older. Enrichment compares conflicting full evidence with the originating shell order before retention, including running rows with no failure record. An older full observation cannot hide the newer shell failure. Read the run or thread for current detail.

| Thread status | Meaning in this gateway |
| --- | --- |
| `open` | Unarchived work that is neither effectively snoozed nor explicitly settled. Includes pinned threads, running work, and idle threads. |
| `snoozed` | A future snooze time, with no pending approval/input, new failure, or completion since snoozing that would wake it early. Snoozing does not stop execution. |
| `settled` | T3's explicit settled override, subject to activity blockers and pin/snooze precedence. A completed turn alone does not settle a thread. |
| `archived` | Threads with an archive timestamp. This explicit filter includes archives even when `includeArchived` is false. |
| `all` or omitted | No lifecycle filter; archives remain hidden unless `includeArchived=true`. |

The gateway uses the lifecycle fields exposed by T3. On V1, the T3 UI also derives settlement from client preferences, inactivity, and linked PR state; those inputs are not available to these tools, so the settled/open lists can differ from the UI's automatic classification. V2 persists settlement on the server; the gateway reads those lifecycle fields. Older servers with no lifecycle fields show unarchived threads as open. This behavior follows the server-backed portion of T3's `threadSettled.ts` and sidebar partitioning, inspected at source revision `4b8388773`.

Thread summaries include `status` (lifecycle), `statusReason`, `activity` (execution), `isRunning`, `quality` (`fresh`/`stale`/`incomplete`/`inconsistent`), `warning` when T3 signals disagree or are incomplete, `observedTurnId`/`observedAt`/`observedTarget` (`environmentId`/`threadId`/`turnId`/`observedAt`), project title, the raw settlement override, snooze time and snooze start, pin timestamp, session status and session update time, latest user-message time, latest turn, pending flags, actionable-plan flag, and background liveness (`working` or `monitoring` when T3 provides it). `hasConflictingSignals` is kept for compatibility and mirrors `quality=inconsistent`. Overview, detail, and run reads share this normalizer and never silently resolve contradictory fields. `t3_thread_get` combines the full thread with these shell fields and its latest response. Use `t3_thread_messages` for more history. `t3_providers_list` aggregates observed `instanceId`/`provider`/`model` labels, per-project defaults, and thread usage for thread creation.

For a new assignment, prefer `t3_task_start`. It performs a recoverable `thread.create` followed by the required first instruction, and requires an explicit `runtimeMode`. By default it uses the project's current checkout (or an explicitly supplied existing `worktreePath`). Set `workspaceMode: "worktree"` and supply `branch` as the base branch to have the gateway create a deterministic isolated Git worktree before starting the turn. `startFromOrigin: true` fetches and resolves that branch from `origin` when the remote exists; `false` or omission starts from the local branch. The gateway derives the task branch and path from the durable operation, then passes both through ordinary HTTP `thread.create`; the subsequent `thread.turn.start` carries no `bootstrap` field. This works around T3's HTTP bootstrap gap while keeping retries on one thread. `worktreePath` remains invalid in this mode. The workaround does not launch T3's configured setup script; the agent must run required project setup in its attached worktree.

One parent idempotency key owns deterministic child receipts for thread creation and the initial turn; retries with identical input reconcile the same thread, message, and run. The gateway never advances to the turn while thread creation is uncertain. Task stages include `thread_create_uncertain`, `thread_created`, `dispatch_rejected`, `dispatch_uncertain`, `run_accepted`, and `rejected`.

The task journal stores the title, payload hash, stage, IDs, optional Git baseline, and timestamps, but not the instruction text. `t3_task_get` reconciles uncertain bootstrap operations without replaying them. Older partial two-command assignments can still be continued by retrying `t3_task_start` with the same key and identical original input. `t3_tasks_list` is bounded and deterministic; use task detail for fresh thread/run state.

`t3_result_get` is a read-only composition over those sources, not another result database. It returns fresh task/thread/run state plus committed base-to-HEAD comparison, staged and unstaged patches, structured status, evidence provenance, and explicit limitations. The baseline is captured only after T3 accepts the selected workspace. A missing baseline or inaccessible workspace produces a limitation instead of an invented result; assistant text remains agent-reported rather than independent command evidence.

The low-level thread tools remain available. Call `t3_providers_list` when a project has no default model. `t3_thread_create` requires its initial `message` and is a local MCP wrapper over ordinary T3 `thread.create` and `thread.turn.start` commands; empty thread creation is not exposed to MCP clients. In worktree mode, `branch` selects the base and `startFromOrigin` selects the local branch or its `origin` tracking commit. A local-mode `worktreePath` must already exist as a directory on the shared gateway/T3 filesystem. Use `t3_thread_send` only to continue an existing idle thread. Busy threads return `thread_busy`; uncertain low-level dispatches return a durable operation handle.

Failed run, thread, and overview records include a sanitized, turn-bound `failure`. `t3_run_get` and `t3_run_wait` retain observed terminal failures in the local journal across reconnects and later turns. `t3_thread_get` and overview highlights report the observed thread turn. A previous turn's assistant message is never used as a failed run's response. Error-response excerpts receive the same credential and private-path redaction as the failure text.

The existing fields remain: `category`, `code`, `message`, `provider`, `model`, `turnId`, `resetAt`, `retryAfter`, and `source`. New additive fields are `class` (the T3 class), `retryable` (the provider's boolean), and `retry` (`attempt`, `maxAttempts`, `retryDelayMs`). Missing values are `null` on new observations. Older retained records can omit these three fields. `retry` describes the provider's retry attempt; it is not a promise that a new attempt will succeed.

| API | Authoritative failure sources | Times and limitations |
| --- | --- | --- |
| V1 | `latestTurn.state`; matching `activities[].turnId` on `runtime.error` or `provider.turn.start.failed`; matching assistant `messages[].turnId`; session `lastError` with matching `activeTurnId`, or an error-session timestamp inside the failed turn's request/completion interval. Activity text is `payload.message` or `payload.detail`. | For gateway runs, admitted model/provider settings in the run journal replace mutable thread settings. A thread-only observation before run binding can report only current thread configuration, not historical provider/model identity. The live HTTP API checked on 2026-10-02 (protocol 1, `0.0.45-nightly.20261002.2595`) supplied no structured reset time, retry delay, or failure class. Unbound start errors and later session errors cannot explain an earlier failed turn. A missed V1 failure cannot be recovered after T3 moves to another turn. |
| V2 full thread | `projection.runs[].status`, run ID, `rootNodeId`, provider and model; the same run/root node's failed `projection.turnItems[]` error item, with `failure.class`, `message`, `code`, `retryable`, optional `resetAt`, and optional item `retry`. The latest error is selected by update time, then ordinal and ID, as in T3. | `failure.resetAt` is passed through only when supplied and valid. Claude and Codex adapters carry provider reset data; Codex can update it after the stop. Item `retry.retryDelayMs` is retained as retry progress. V2 has no direct `retryAfter` field, so it remains `null`. Historical failed runs remain readable while their root error items are present. A limit-blocked run remains visible after an unstarted queued successor is cancelled. Child errors and recovered retry items do not explain a root run failure. |
| V2 shell/overview | `latestRunId` or the activity-owning `activeRunId`, status, `lastError`, `lastErrorClass`, and `usageLimitResetAt`. T3 clears `lastErrorClass` when a distinct session error replaces the root error; that unbound text is excluded. Full reads enrich the selected overview highlights. | Shell rows lack the full error code, retryability, retry progress, and run model selection. Read the run or thread for those fields. |

V2 run provider/model identity stays separate from error ordering in the private journal metadata. An older full read can supply that immutable identity without replacing a newer bound shell reason. Later shell settings cannot change the retained run identity. This also applies when the first observation is a full list or overview, and after restart. Existing journals can omit this metadata.

Recovery retains a private ordered record with no failure, so delayed older failed reads cannot restore the error after restart. V2 full snapshots retain completed/interrupted state for historical runs, including cancelled and rolled-back runs mapped to interrupted, allowing their matching gateway runs to recover after a successor starts. Same-sequence V2 evidence uses the supplied update timestamp before full/shell scope. V1 activities with structured error metadata but no text use a generic sanitized message and preserve the supplied code and retry data.

List and overview recovery updates are batched, including full enrichment, to avoid one journal rewrite per terminal row. A distinct V1 reason replaces its code/reset/retry metadata as a whole. Host redaction includes bare DNS names and IPv6 endpoints. Tool schema version 9 changes the discovery fingerprint for the additive failure contract.

Shell recovery is recorded before list/overview response filters. After an ordered recovery clear, thread/list/overview responses reread retained evidence. If a newer failure rejects recovery or full enrichment first discovers a same-turn failure, the response keeps its observed state, with list/overview selection and counts on the shell snapshot, but reports that same-turn failure; stale success responses are omitted. Across protocol transitions, private read start ordering prevents an older in-flight observation from overwriting a later failure or recovery when the protocols' snapshot sequences differ. This chronology stays inside the journal and does not add MCP fields.

Structured provider/model/class/code values retain identifier punctuation, with controls removed and length bounded. Class/code fields also redact explicit credential assignments and credential tokens. Error messages still receive credential, host and path redaction. Categories require explicit evidence. A generic outer code does not mask a recognized provider subtype in JSON error text. A V1 provider message can retain structured activity metadata when both the turn and raw error reason match. Its parsed provider subtype takes priority; when parsing supplies no code, the matching activity code remains available. Known provider types/codes distinguish `rate_limit_error` and Codex `rateLimitExceeded` (`rate_limit`), `usageLimitExceeded` (`quota`), and `auth_unavailable`/`authentication_error`/`credits_required` (`auth_billing`). The exact T3 Claude/Codex usage-limit sentences map to `quota`; the exact credits-required refusal maps to `auth_billing`. A specific turn-bound provider type or credit refusal takes priority over V1's generic usage-limit sentence. HTTP 429 alone never selects a category. Arbitrary text remains `unknown`.

V2 classes are `usage_limit`, `provider_error`, `transport_error`, `permission_error`, `validation_error`, and `unknown`. The gateway preserves the class. A matching V2 shell session supplies its provider instance before mutable model configuration. A bound shell class/reset remains available when error text is absent; the gateway supplies a safe default message and keeps broad shell usage-limit categories unknown. `usage_limit` maps to the existing `quota` category unless a specific provider code or the exact credit refusal supplies a more precise category. T3 uses this broad class for some rate-limit cases, including Claude `api_error_429`; that ambiguous code remains `unknown`, even when the class is `usage_limit`. A shell-only `usage_limit` class stays `unknown` unless an explicit provider type in the error text or an exact known refusal sentence identifies the category. It alone does not prove subscription quota exhaustion. `provider_error` maps to the additive `provider_error` category, which does not claim a crash. Other classes stay `unknown` unless an explicit provider type identifies the category. Existing `provider_internal` journal entries and explicitly supplied session categories remain supported for stored data and current clients.

`source` is `t3_v2_turn_item`, `t3_message`, `t3_activity`, `t3_session`, or `t3_turn` (a generic failed-turn fallback). More precise observed sources can upgrade retained failures. Later bound V2 shell reasons replace earlier full reasons without inheriting their code or reset time. Matching shell reasons keep known full-only code and retry fields while adding a supplied reset time and advancing observation order. Distinct raw reasons do not share metadata, even when redaction or truncation makes their displayed text equal. An unbound shell session error keeps an already known root reason; a newer full snapshot can still clear it. Later same-source V1 activities and provider refusals replace earlier evidence in snapshot order. Full V2 reads retain T3's selected root error, or its authoritative absence, in snapshot/item order, so a delayed older read cannot replace newer evidence. Protocol switches use read-admission order, with a stored protocol boundary that also rejects delayed reads after switching back to the prior version. Thread detail applies that boundary before selecting full or shell state. Equal observations advance the in-memory admission watermark without rewriting the journal. Ordering stays inside the journal; it is not an MCP field. Structured provider/model identifiers keep valid dots, slashes, colons, and credential words; control characters are removed and length is bounded. Free-form error text also receives credential and endpoint redaction. Older records without ordering or a protocol boundary remain readable and gain it on a fresh ordered observation. An equally ordered read can keep a reported matching shell reset that the earlier full read omitted. A completed or interrupted observation for the same turn clears its retained failure in both thread and run records. A different later turn leaves historical failures intact. Reset times and retry delays are never calculated from HTTP status, message text, or account-wide usage snapshots. Missing `resetAt` and `retryAfter` mean unknown. No automatic retry or provider switch is performed.


`t3_thread_interrupt` works on threads started in T3's UI or by another client, without requiring a gateway `runId`. Read the thread and supply `threadId`, `expectedTurnId` (the `observedTarget.turnId`), and an `idempotencyKey`. The gateway rejects a changed or finished turn before dispatch (`turn_changed`/`thread_not_running` with current target details) and never automatically replays an uncertain interruption. Acceptance returns post-dispatch `verification` (`interrupted`/`still_running`/`target_changed`/`not_running`/`inconsistent`/`unknown`); acceptance alone never confirms a stop. V1 interrupts by provider session, so a turn change after the gateway's check remains a race. V2 sends `run.interrupt` with the observed T3 run ID. Poll `t3_thread_get` to verify the outcome. Interruption does not archive or delete the conversation.

Snooze hides a thread from the inbox until its wake time; it never stops a running agent. `t3_thread_snooze` defaults to this evening (18:00 gateway-local) while meaningfully before evening, else tomorrow morning (09:00); presets `hour`, `three-hours`, `evening`, `tomorrow`, and `next-week` (Monday 09:00) match the T3 clients, or supply an explicit future ISO `snoozedUntil`. Threads blocked on you (pending approval/input) or with a queued turn start cannot be snoozed. `t3_thread_unsnooze` wakes immediately. `t3_thread_settle` marks a thread done and clears snooze and pin; it is blocked while the thread runs, has a pending approval, or has a queued turn start. `t3_thread_unsettle` reopens. All four are idempotent mutations with the same journal, read-only, and scope handling as the other control tools.

Thread deletion, workspace deletion, checkpoint rollback, and arbitrary terminal commands are not exposed.

## What is implemented

The gateway supports T3 orchestration protocols 1 and 2. Protocol 2, introduced by [upstream PR #2829](https://github.com/pingdotgg/t3code/pull/2829), uses HTTP snapshot reads and authenticated WebSocket RPC for thread control. Protocol 1 retains HTTP dispatch for older deployments. The recorded live integration target is `v0.0.41-nightly.20260910.1507`; V2 has source-contract and local fixture coverage, with live verification still required. See [orchestrator compatibility](orchestrator-compatibility.md).

- Project search and registration, thread search with lifecycle/execution/attention filters and deterministic sort, one-call bounded overviews with execution counts and highlights, provider discovery, thread creation with T3-accepted workspace, compact check-ins, and paginated messages with bounded text.
- Direct read-only Git status, bounded staged/unstaged patches, and validated committed base-to-head comparison, resolved only from T3 project and thread workspace identity.
- Recoverable one-call task start plus task lookup/listing, backed by composite receipts linked to T3-owned threads and runs rather than a second execution-state model.
- Task-bound result packages that compose T3 observations with Git-observed committed and uncommitted evidence and explicit provenance.
- Starting agent turns with typed busy rejection, inspecting runs with shared observation quality, polling for changes, requesting interruption by gateway run handle or observed thread turn with post-dispatch verification, responding to approval or user-input requests, snoozing/settling threads, and archiving threads.
- Connection status with gateway version/commit/fingerprint (`T3_CODE_MCP_COMMIT` plus package version), observation time, effective access mode with callable/disabled operations and stable reason codes (`gateway_read_only`/`t3_scope_required`), upstream T3 scopes, capabilities, freshness, and credential expiry. Doctor performs a real local MCP `tools/list` exchange and can compare a supplied actual-host capture. Run results also report connection, freshness, thread quality, and turn-bound failures.
- `t3_audit_log` exposes a bounded, filtered view of the local redacted usage trail. It records the MCP/transport request, each upstream T3 request, each read-only Git subprocess, and durable journal transitions with correlation IDs, timing, outcome, and safe input/output summaries. Prompt, message, patch, answer, and credential values are never written verbatim.
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

Busy threads reject new turns; queueing and steering are not implemented. On V1, pending-action details are inferred from T3 activity records and may include historical entries; the pending flags and available request IDs need to be considered together. On V2, detail reads include only pending runtime requests with a live or message response capability. The gateway does not provide a separate human-confirmation mechanism for approval responses. Composite task records cover only work started with `t3_task_start`; they do not reclassify every historical T3 thread as a task.

Run inspection relies on the journaled message/turn IDs and T3's latest-turn projection. Status for older runs or runs whose turn ID is not yet known can be incomplete. A provider-owned turn ID is not currently exposed.

There are no terminal tools, managed command jobs, general file-reading tools, general-purpose Git mutation tools, MCP OAuth server, or multi-environment routing. The one scoped Git mutation is deterministic `git fetch`/`git worktree add` preparation for `workspaceMode=worktree`; worktrees are retained rather than automatically deleted because they may contain task changes. Git inspection and worktree preparation require the gateway and T3 workspaces to share a filesystem namespace; they do not work across an HTTP-only host boundary where T3's workspace paths are absent. Status item lists and diff bytes are bounded, and very large Git operations fail after 30 seconds instead of returning a partial result. The HTTP endpoint uses a static bearer token; use a trusted network, tunnel, or HTTPS reverse proxy for remote access. The gateway does not read T3's database or create provider sessions outside T3.

## Launch settings and preflight

Put a user's model and runtime preferences in `modelSelection` and `runtimeMode`, not in instruction prose. If no model preference exists, omission intentionally uses the project's default on creation or the current thread's model on follow-up. `t3_providers_list` reports observed selections; it is not a list of installed models or favorites. Do not guess provider option keys for reasoning effort. Use the keys supported by the configured T3 provider.

An isolated assignment uses the **base branch** in `branch`; the gateway generates the task branch and path. For example, after resolving the real project ID and configured model:

```json
{
  "projectId": "resolved-project-id",
  "title": "Repair transient Undo",
  "instruction": "Repair the owned Undo defect; reproduce it, verify the repair, and report evidence.",
  "modelSelection": { "instanceId": "configured-instance", "model": "requested-model" },
  "runtimeMode": "full-access",
  "workspaceMode": "worktree",
  "branch": "main",
  "startFromOrigin": true,
  "idempotencyKey": "unique-assignment-key"
}
```

Fill placeholders from actual discovery. Omit `worktreePath` in worktree mode. Use `workspaceMode: "local"` to work in the current checkout; `startFromOrigin` applies only to worktree mode. Save the returned `taskRef`, `runId` and operation IDs. `t3_task_start` is the preferred composite task receipt; `t3_thread_create` provides the lower-level thread/run start without a composite task reference.

New starts, sends and run observations include additive `settings` evidence. `requested` records explicit model/runtime arguments, `resolved` records the selection used to dispatch, and `modelSource`/`runtimeSource` distinguish explicit choices from project defaults and thread inheritance. `effective` is populated only when T3's snapshot attributes the settings to the matching current turn. `state: "unresolved"` does not claim provider acceptance or observed configuration; `matchesResolved: false` surfaces a mismatch. These are observed T3 thread settings, not independent proof of the provider execution model or effective reasoning effort. Historical receipts without settings remain readable. Provider options and reasoning effort are deliberately omitted from the receipt because the gateway cannot establish their effective meaning from the upstream projection.

## Monitoring recipe

1. Start once and retain the durable handles. Accepted intent is not completed work.
2. For a known gateway run, call `t3_run_wait` for a bounded interval. It returns on status, response identity, approval/input, observation quality or connection changes. A response text update on the same assistant message ID is not a change trigger.
3. Stop waiting when input/approval is needed, the run is terminal, or the connection is lost. Inspect pending actions or the changed response as appropriate. Fetch message history only when more context is needed.
4. On unchanged windows, back off between calls, communicate useful progress, and continue independent work. Avoid multiple monitors for the same run. For project-wide triage use bounded `t3_threads_overview`, then fetch detail for changed or attention-requiring threads.
5. On uncertainty reconcile the original handle using `t3_task_get` or `t3_run_get`; keep the original idempotency key and identical payload if a retry is necessary. A busy rejection is a reason to observe existing work, not create a duplicate thread. Do not interrupt work merely because a wait timed out.

The gateway backs off from 500 ms to 1 second and then at most 2 seconds between healthy observations. Environment identity is checked at entry and exit and reused only within the bounded read. Independent reads and mutations retain their normal checks. `monitoring` reports observations, requested timeout and actual elapsed milliseconds. An in-flight request can extend the timeout; strict wall-clock cancellation and push notifications are not provided.

## Usage analysis

Run offline without T3/MCP credentials:

```bash
pnpm usage-summary --file ./data/audit.jsonl --since 2026-09-18T00:00:00Z --until 2026-09-30T21:13:16.337Z
```

The command reads a fixed byte snapshot without altering the log or contacting T3. It reports the window, invalid lines, explicit denominators, calls/errors/latency by tool and endpoint, model omissions, active wait timeouts, measured observation counts, and successive successful same-thread detail comparisons in file order (which can bridge failed reads). Omission may intentionally use defaults; unchanged state is not proof of waste. Tool completion is not independent task success, and request counts do not establish token or financial savings.

New downstream audit events include `parentCorrelationId` and `parentOperation` from an async-local MCP context, keeping concurrent callers isolated. Historical events without parents remain explicitly unattributed. Median and p95 latencies are fixed-histogram upper bounds; means/minima/maxima are exact. Operation groups and tracked thread comparisons have fixed caps; thread-state evictions are reported. The audit tool streams filtering/pagination while preserving append order, filtered offsets, total matches and invalid-line counts. A query still scans the snapshot to compute totals but does not retain the full history. Appends are visible on the next query. Rotation/deletion is not performed.

Run `pnpm build` then `pnpm benchmark:audit` for a disposable large-file benchmark under a 32 MB JavaScript heap. The benchmark never reads live history or dispatches tasks.

See [task briefs and evaluation](task-briefs.md) for launch and continuation prompt examples.
