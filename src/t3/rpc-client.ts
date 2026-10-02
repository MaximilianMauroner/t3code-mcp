import { randomUUID } from "node:crypto";
import WebSocket from "ws";
import { z } from "zod";

const Frame = z.object({
  _tag: z.string(),
  requestId: z.union([z.string(), z.number()]).optional(),
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
  const url = new URL(`${baseUrl.replace(/\/$/, "")}/ws`);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("orchestrationProtocol", "2");
  const requestId = randomUUID();
  const requestSignal = signal ? AbortSignal.any([signal, AbortSignal.timeout(timeoutMs)]) : AbortSignal.timeout(timeoutMs);
  requestSignal.throwIfAborted();
  return new Promise<T>((resolve, reject) => {
    const socket = new WebSocket(url, { headers: { authorization: `Bearer ${accessToken}` }, maxPayload: 64 * 1024 * 1024 });
    let settled = false;
    const finish = (result: { value: T } | { error: unknown }) => {
      if (settled) return;
      settled = true;
      requestSignal.removeEventListener("abort", abort);
      socket.terminate();
      if ("error" in result) reject(result.error);
      else resolve(result.value);
    };
    const abort = () => finish({ error: requestSignal.reason });
    requestSignal.addEventListener("abort", abort, { once: true });
    socket.on("error", () => finish({ error: new Error(`T3 RPC ${method} connection failed.`) }));
    socket.on("close", () => finish({ error: new Error(`T3 RPC ${method} disconnected before acknowledgement.`) }));
    socket.on("unexpected-response", (_request, response) => {
      response.resume();
      finish({ error: new Error(`T3 RPC upgrade failed with HTTP ${response.statusCode ?? "unknown"}.`) });
    });
    socket.on("open", () => {
      socket.send(JSON.stringify({ _tag: "Request", id: requestId, tag: method, payload, headers: [] }));
    });
    socket.on("message", (data) => {
      try {
        const raw: unknown = JSON.parse(data.toString());
        for (const entry of Array.isArray(raw) ? raw : [raw]) {
          const frame = Frame.parse(entry);
          if (frame._tag === "Defect" || frame._tag === "ClientProtocolError") {
            finish({ error: new Error(`T3 RPC ${method} returned a protocol error.`) });
            return;
          }
          if (frame.requestId !== requestId || frame._tag !== "Exit" || !frame.exit) continue;
          if (frame.exit._tag === "Success") {
            finish({ value: schema.parse(frame.exit.value) });
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
