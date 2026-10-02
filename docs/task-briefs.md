# Task briefs and continuation updates

[Back to usage](usage.md)

The gateway transports task text; the MCP caller owns its content. These examples are a reusable recipe, not a parser, mandatory form, new product specification or authorization policy. Link accepted issue decisions rather than recreating an evolving roadmap in every message.

For a new thread, provide enough information to act without reconstructing another conversation:

```text
Goal: repair the two recovery defects in the owned PR.
Current context: last reviewed SHA, verdict and evidence link. Verify live head,
attached worktree and current review before editing; the checkpoint is historical.
Owned scope: account/authority recovery isolation and uncertain metadata writes.
Behavior examples:
- Account B must not resume account A's recovery for the same event.
- A committed metadata write followed by failed verification preserves recovery.
Execution: preserve existing edits; reproduce each bug; repair it; run affected
checks and the required checks defined in the current repository guidance.
Review: use the configured independent reviewer with a fresh exact-SHA brief,
diff, requirements and verification evidence; return PASS/HOLD with findings.
Supersession: this definition replaces the previous external-only reviewer rule.
Completion: final SHA, changed paths, checks labelled passed/failed/blocked/unrun,
review verdict and precise remaining owner actions.
Authority: follow the user's current repair/merge/deployment authorization.
```

Replace generic labels with actual scope and evidence. Name the real shared lock path or coordinator reference when heavy jobs must serialize. An agent name or GitHub mention does not by itself invoke a reviewer: specify the configured capability and independence requirements. Keep model/effort/runtime/branch/idempotency values in tool arguments. Avoid inferring access or installed capabilities from a prior session.

An existing thread usually needs only the change:

```text
Continue the existing assignment and preserve prior work/evidence.
Changed: the new review identifies an account-switch recovery regression at SHA X.
Owned next action: reproduce and repair that finding; no expansion to other PRs.
Still required: affected checks, current required CI and independent final-SHA review.
Superseded: the old external-only reviewer instruction. Use the configured native
independent reviewer with a self-contained brief when the environment permits it.
Return final-SHA evidence and exact remaining blockers; retain current authorization.
```

Prefer bounded discovery. Refresh state needed for ownership, dependency or safety decisions; expand to every issue/PR only when the assignment is genuinely project-wide triage. Reuse still-valid evidence and label its revision and limits. A blocked check remains blocked; a reviewer request is not a verdict, and an agent report is not independent verification.

## Offline evaluation

[Evals](../evals/task-briefs.json) contains sanitized launch, continuation, reviewer and blocked-CI scenarios with weak versus improved prompts and observable scoring criteria. It is designed for a caller/harness that supplies tools, not for live PR redispatch. The in-memory MCP journey in `tests/mcp-server.test.ts` verifies handle reuse, pending-input/terminal observation, busy rejection and no duplicate dispatch.

For a behavioral model comparison, run both prompt variants on the same disposable fixture and same pinned model, settings and tool inventory, with multiple trials. Capture tool traces and artifact evidence. Score scope correctness, configuration arguments, reviewer invocation, verification labels and redundant discovery; separately report clarification count, upstream requests, elapsed time and any measured tokens/cost. Include failures and coverage denominators. Do not treat keyword presence as behavioral success.

The repository does not own the external caller's model/harness or automatically launch paid model evaluations. The fixtures and deterministic MCP journey are available locally; comparative model quality remains unmeasured until that caller runs the fixture suite. No production replay is needed or authorized by this recipe.

This recipe follows [OpenAI's prompting guidance](https://developers.openai.com/api/docs/guides/prompt-engineering) on clear goals, relevant context, examples and evaluation, and [Anthropic's guidance](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices) on direct action, motivation and clear context boundaries. Use concrete examples where the contract is ambiguous; longer prompts alone do not establish improvement.
