---
name: verify-t3code-mcp
description: Verify gateway transport and read-only T3 connectivity. Use when checking changed history, Git read or transport paths with isolated journal state.
---

# Verify t3code-mcp

Read README.md, docs/development.md, docs/configuration.md, compatibility and
[features.md](features.md). Use isolated worktree/journal and local test servers.
Check active gateway owners and host resources first. Never restart or reconfigure
a shared deployed gateway. Select journeys affected by work in the requested
window. Record gateway revision and target T3 protocol/build.

`pnpm install --frozen-lockfile`, then `pnpm build`, `pnpm test` and
`pnpm typecheck:test` run disposable transport fixtures without real credentials.
They are local boundary proof, not current live T3 proof. Use existing authenticated
configuration only for status/doctor/smoke with process-local `MCP_READ_ONLY=true`
and an owned `T3_MCP_DATA_DIR`. Do not print credentials or copy them into files.
CLI diagnostics require the built `dist/cli.js`; no browser is needed for MCP.

Require status to identify the intended environment, fresh state, protocol and
required read operations. Honor the current run's lifecycle restrictions; never
read excluded archived/deleted threads. Mark those ranges excluded from coverage.
Read every allowed page for inventory/history journeys. Check
range/truncation and completed-turn output; successful JSON alone cannot establish
complete history. Compare a supported independent read source when output is absent.
Do not treat overview highlights as full coverage. Recheck connection/readiness
before one targeted retry. Retain bounded redacted captures outside journal state.

Live write-enabled tasks, input responses, lifecycle changes and opt-in live tests
need a disposable target and separate model/spend authority. The existing live test
hardcodes provider/model selection; confirm it exists before enabling it. Do not
let it pick an arbitrary first project. Leave those journeys blocked in read-only work.

Stop owned local fixture/gateway processes, close owned ports and remove only the
owned journal/test worktree state after evidence retention. Do not delete the
shared task journal or alter T3 settings. Report diagnostics, local tests, live
reads, live mutations and history gaps separately.
