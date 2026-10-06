# Feature map

Reusable procedure map. Reuse docs/development.md and existing tests.
A map does not establish a complete live history read.

| ID | Public caller journey and expected result | Existing proof | Reset |
| --- | --- | --- | --- |
| G1 | tools/list then connection status. Environment, freshness, scopes/expiry and schema fingerprint match actual target; read-only mode disables control. | setup-doctor, config, MCP server tests; CLI doctor | Remove owned journal. |
| G2 | Exhaust project/thread pages including archived/all statuses; get detail and message ranges. Stable IDs deduplicated, bounds explicit; missing completed output remains a gap. | gateway, thread-state, contract/V2 tests | Read-only. |
| G3 | Read selected workspace Git status, staged/unstaged and committed diffs with literal path/revision. Renames/binary/conflicts/truncation are explicit. | git-inspection, git-mcp tests | Remove fixture Git repos only. |
| G4 | Start a disposable task, retry same key, reconnect with another gateway and find/observe result. No duplicate dispatch; interruption/recovery is attributed to exact turn. | task-delegation, journal, usage-improvements/V2 tests | Archive owned test thread only with authority. |
| G5 | Input/approval and snooze/settle/reopen/archive actions on disposable thread. Current pending action/turn identity required; uncertain dispatch is reconciled. | thread-lifecycle, gateway-hardening/V2 tests | Restore owned thread state. |
| G6 | Exercise V1/V2 tagged errors, disconnects, cancellation and rate/quota/auth failures. No unrelated old failure attached to new turn. | http-client, failure-reporting, V2 compatibility tests | Stop owned fake T3. |
| G7 | Authenticate local Streamable HTTP client and reject missing/wrong bearer, oversized body and unsupported methods. Stdio tools match supported catalog. | transport and MCP tests | Close owned listener. |
| G8 | Read filtered audit pages and usage summary. Fixed snapshot cursors, attribution and redaction survive concurrent activity; optional bounded benchmark removes fixtures. | audit-log, usage-summary tests; benchmark:audit | Remove owned journal only. |

Live G4-G5 are blocked without disposable target/provider authority. Current coding
reads support G1-G2 only. Protocol fixtures cannot fill an empty native child history.
