import { randomUUID } from "node:crypto";
import WebSocket from "ws";
import { z } from "zod";

const Frame = z.object({
  _tag: z.string(),
  requestId: z.union([z.string(), z.number()]).optional(),
  values: z.array(z.unknown()).optional(),
  exit: z.discriminatedUnion("_tag", [
    z.object({ _tag: z.literal("Success"), value: z.unknown() }),
    z.object({ _tag: z.literal("Failure"), cause: z.array(z.object({
      _tag: z.string(), error: z.unknown().optional(),
    })) }),
  ]).optional(),
});

export class T3RpcError extends Error {
  override readonly name = "T3RpcError";
}

// T3 uses Effect's JSON RPC envelopes over /ws. One request per connection
// keeps lifetime bounded and avoids replaying a mutation after a disconnect.
export async function requestT3Rpc<T>(
  baseUrl: string,
  accessToken: string,
  timeoutMs: number,
  method: string,
  payload: unknown,
  schema: z.ZodType<T>,
  signal?: AbortSignal,
): Promise<T> {
  return collectT3Rpc(baseUrl, accessToken, timeoutMs, method, payload, {
    success: (value) => schema.parse(value),
    interrupted: (reason) => { throw new Error(`T3 RPC ${method} ${reason === "disconnected" ? "disconnected before acknowledgement" : "ended before acknowledgement"}.`); },
  }, signal);
}

export interface RpcCollector<T> {
  chunk?: (values: unknown[]) => T | undefined;
  success?: (value: unknown) => T;
  interrupted: (reason: "timeout" | "streamEnded" | "disconnected") => T;
}

export async function collectT3Rpc<T>(
  baseUrl: string,
  accessToken: string,
  timeoutMs: number,
  method: string,
  payload: unknown,
  collector: RpcCollector<T>,
  signal?: AbortSignal,
): Promise<T> {
  const url = new URL(`${baseUrl.replace(/\/$/, "")}/ws`);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("orchestrationProtocol", "2");
  const requestId = randomUUID();
  const requestSignal = signal ? AbortSignal.any([signal, AbortSignal.timeout(timeoutMs)]) : AbortSignal.timeout(timeoutMs);
  requestSignal.throwIfAborted();
  return new Promise<T>((resolve, reject) => {
    const socket = new WebSocket(url, { headers: { authorization: `Bearer ${accessToken}` }, maxPayload: 64 * 1024 * 1024 });
    let settled = false;
    let bytes = 0;
    const finish = (result: { value: T } | { error: unknown }) => {
      if (settled) return;
      settled = true;
      requestSignal.removeEventListener("abort", abort);
      socket.terminate();
      if ("error" in result) reject(result.error);
      else resolve(result.value);
    };
    const interrupted = (reason: "timeout" | "streamEnded" | "disconnected") => {
      try { finish({ value: collector.interrupted(reason) }); }
      catch (error) { finish({ error }); }
    };
    const abort = () => {
      if (signal?.aborted || !collector.chunk) finish({ error: requestSignal.reason });
      else interrupted("timeout");
    };
    requestSignal.addEventListener("abort", abort, { once: true });
    socket.on("error", (error) => {
      if ("code" in error && typeof error.code === "string" && error.code.startsWith("WS_ERR_")) finish({ error: new Error(`T3 RPC ${method} returned an invalid response.`) });
      else interrupted("disconnected");
    });
    socket.on("close", () => interrupted("disconnected"));
    socket.on("unexpected-response", (_request, response) => {
      response.resume();
      finish({ error: new Error(`T3 RPC upgrade failed with HTTP ${response.statusCode ?? "unknown"}.`) });
    });
    socket.on("open", () => {
      socket.send(JSON.stringify({ _tag: "Request", id: requestId, tag: method, payload, headers: [] }));
    });
    socket.on("message", (data) => {
      try {
        bytes += Buffer.byteLength(data.toString());
        if (bytes > 64 * 1024 * 1024) throw new Error("Response limit exceeded.");
        const raw: unknown = JSON.parse(data.toString());
        for (const entry of Array.isArray(raw) ? raw : [raw]) {
          const frame = Frame.parse(entry);
          if (frame._tag === "Defect" || frame._tag === "ClientProtocolError") {
            finish({ error: new Error(`T3 RPC ${method} returned a protocol error.`) });
            return;
          }
          if (frame.requestId !== requestId) continue;
          if (frame._tag === "Chunk" && collector.chunk) {
            if (frame.values === undefined) throw new Error("Missing chunk values.");
            const value = collector.chunk(frame.values);
            socket.send(JSON.stringify({ _tag: "Ack", requestId }));
            if (value !== undefined) finish({ value });
            if (settled) return;
            continue;
          }
          if (frame._tag !== "Exit") continue;
          if (!frame.exit) throw new Error("Missing exit.");
          if (frame.exit._tag === "Success") {
            if (collector.success) finish({ value: collector.success(frame.exit.value) });
            else interrupted("streamEnded");
          } else {
            const failure = frame.exit.cause.find((cause) => cause._tag === "Fail");
            const detail = z.object({ message: z.string() }).safeParse(failure?.error);
            finish({ error: new T3RpcError(detail.success ? detail.data.message : `T3 RPC ${method} failed.`) });
          }
        }
      } catch {
        finish({ error: new Error(`T3 RPC ${method} returned an invalid response.`) });
      }
    });
  });
}
