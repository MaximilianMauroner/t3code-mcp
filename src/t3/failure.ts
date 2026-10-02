import { z } from "zod";
import type { FailureCategory, FailureInfo } from "../gateway.js";
import { ProviderRetrySchema, type Thread, type ThreadShell } from "./types.js";

const ErrorPayload = z.object({
  message: z.string().optional(), detail: z.string().optional(),
  class: z.string().nullable().optional(), code: z.string().nullable().optional(),
  retry: ProviderRetrySchema.optional(),
  type: z.string().optional(), retryable: z.boolean().nullable().optional(),
  resetAt: z.string().nullable().optional(),
  retryAfter: z.union([z.string(), z.number()]).nullable().optional(),
});
const ErrorActivity = z.object({
  kind: z.enum(["runtime.error", "provider.turn.start.failed"]),
  turnId: z.string(), payload: ErrorPayload,
});
const ProviderError = z.object({ error: z.object({ type: z.string().optional(), code: z.string().optional() }) });

export function categoryForFailure(errorClass?: string | null, code?: string | null, message?: string): FailureCategory {
  if (code === "credits_required" || code === "auth_unavailable" || code === "authentication_error" ||
      message === "API Error: Request rejected (429) · Usage credits are required for this model.") return "auth_billing";
  if (code === "api_error_429") return "unknown";
  if (code === "rate_limit_error" || code === "rateLimitExceeded") return "rate_limit";
  if (code === "usageLimitExceeded" || code === "usage_limit" || errorClass === "usage_limit") return "quota";
  if (message === "Claude usage limit reached. Send the message again once the limit resets." ||
      message === "Codex usage limit reached. Send the message again once the limit resets.") return "quota";
  // provider_error includes refusals and configuration failures, not just crashes.
  if (errorClass === "provider_error") return "provider_error";
  return "unknown";
}

function providerCode(message: string): string | null {
  try {
    const parsed = ProviderError.safeParse(JSON.parse(message));
    if (parsed.success) return parsed.data.error.type ?? parsed.data.error.code ?? null;
  } catch { /* Non-JSON provider messages are normal. */ }
  if (!message.startsWith("API Error:")) return null;
  return /^API Error:\s*(?:\d{3}\s+)?(rate_limit_error|auth_unavailable|authentication_error|credits_required)\s*:/u.exec(message)?.[1] ??
    /^API Error: Request rejected \(\d{3}\) · All credentials for model \S+ are cooling down \(last error: (rate_limit_error):/u.exec(message)?.[1] ?? null;
}

export function failureInfo(thread: ThreadShell, expectedTurnId?: string | null): FailureInfo | null {
  const turnId = expectedTurnId ?? thread.latestTurn?.turnId ?? thread.session?.activeTurnId ?? null;
  if (turnId === null) return null;
  const full = "messages" in thread ? thread as Thread : null;
  const persisted = full?.turnFailures?.find((entry) => entry.turnId === turnId);
  if (persisted) {
    return buildFailure({ ...persisted.failure, retry: persisted.retry }, turnId, persisted.provider, persisted.modelSelection.model, "t3_v2_turn_item");
  }
  const turn = thread.latestTurn;
  if (turn && (turn.turnId !== turnId || turn.state !== "error")) return null;
  if (!turn && thread.session?.status !== "error") return null;

  const activities = (full?.activities ?? []).flatMap((value) => {
    const parsed = ErrorActivity.safeParse(value);
    return parsed.success && parsed.data.turnId === turnId ? [parsed.data] : [];
  });
  const activity = activities.at(-1)?.payload;
  const assistant = full?.messages.filter((message) => message.role === "assistant" &&
    message.turnId === turnId && message.text.startsWith("API Error:")).at(-1);
  const assistantCode = assistant ? providerCode(assistant.text) : null;
  const session = thread.session;
  const sessionTime = Date.parse(session?.updatedAt ?? "");
  const sessionMatches = session?.activeTurnId === turnId ||
    (session?.activeTurnId == null && session?.status === "error" && turn?.turnId === turnId &&
      sessionTime >= Date.parse(turn.requestedAt) &&
      (turn.completedAt == null || sessionTime <= Date.parse(turn.completedAt)));
  const provider = sessionMatches ? session?.providerName : null;
  const identity = provider ?? thread.modelSelection.provider ?? thread.modelSelection.instanceId ?? null;
  // A provider type is more specific than V1's generic usage-limit sentence.
  if (assistant && categoryForFailure(null, assistantCode, assistant.text) !== "unknown") {
    return buildFailure({ message: assistant.text, code: assistantCode }, turnId, identity, thread.modelSelection.model, "t3_message");
  }
  if (activity && (activity.message || activity.detail)) {
    return buildFailure(activity, turnId, identity, thread.modelSelection.model, "t3_activity");
  }
  if (sessionMatches && session?.lastError?.trim()) {
    const failure = buildFailure({
      message: session.lastError, class: session.lastErrorClass,
      code: session.failureCode ?? providerCode(session.lastError),
      resetAt: session.resetAt, retryAfter: session.retryAfter,
    }, turnId, identity, thread.modelSelection.model, "t3_session");
    const category = session.failureCategory;
    if (category === "quota" || category === "rate_limit" || category === "auth_billing" ||
        category === "provider_internal" || category === "provider_error" || category === "unknown") return { ...failure, category };
    return failure;
  }
  if (assistant) return buildFailure({ message: assistant.text, code: assistantCode }, turnId, identity, thread.modelSelection.model, "t3_message");
  return buildFailure({}, turnId, identity, thread.modelSelection.model, "t3_turn");
}

function buildFailure(
  payload: z.infer<typeof ErrorPayload>, turnId: string, provider: string | null,
  model: string, source: FailureInfo["source"],
): FailureInfo {
  const message = payload.message ?? payload.detail ?? "T3 reported that the provider turn failed without an error message.";
  const code = payload.code ?? payload.type ?? providerCode(message);
  return {
    category: categoryForFailure(payload.class, code, message),
    class: payload.class ? sanitizeFailureText(payload.class, 200) : null,
    code: code ? sanitizeFailureText(code, 200) : null,
    message: sanitizeFailureText(message, 2_000),
    provider: provider === null ? null : sanitizeFailureText(provider, 200),
    model: sanitizeFailureText(model, 200), turnId,
    resetAt: payload.resetAt && Number.isFinite(Date.parse(payload.resetAt))
      ? new Date(Date.parse(payload.resetAt)).toISOString() : null,
    retryAfter: payload.retryAfter == null ? null : sanitizeFailureText(String(payload.retryAfter), 200),
    retryable: payload.retryable ?? null,
    retry: payload.retry ?? null,
    source,
  };
}

export function sanitizeFailureText(value: string, maxLength: number): string {
  return value
    .replace(/\n\s+at\s[^\n]*/g, "")
    .replace(/(?:https?|file):\/\/[^\s)]+/gi, "[REDACTED URL]")
    .replace(/(["'])(?:\/|[A-Za-z]:[\\/]|\\\\)[^\r\n]*?\1/g, "$1[REDACTED PATH]$1")
    .replace(/(^|[^\p{L}\p{N}_\\/])(?:\/[^\s"'<>\])}]+|[A-Za-z]:[\\/][^\s"'<>\])}]+|\\\\[^\s"'<>\])}]+)/gu, "$1[REDACTED PATH]")
    .replace(/\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b/g, "[REDACTED HOST]")
    .replace(/(["'])(api[_-]?key|(?:access|refresh|id)[_-]?token|authorization|credential|password|secret|token)\1\s*:\s*(["'])(?:\\.|(?!\3)[^\\])*\3/gi,
      "$1$2$1:$3[REDACTED]$3")
    .replace(/\b(Bearer|Basic)\s+[^\s,;]+/gi, "$1 [REDACTED]")
    .replace(/\bsk-[A-Za-z0-9_-]{12,}\b/g, "[REDACTED]")
    .replace(/\b(api[_-]?key|(?:access|refresh|id)[_-]?token|authorization|credential|password|secret|token)\s*[:=]\s*[^\s,;]+/gi, "$1=[REDACTED]")
    .replace(/[\u0000-\u001f\u007f]/g, " ")
    .slice(0, maxLength);
}
