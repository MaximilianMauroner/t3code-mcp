import { afterEach, describe, expect, it, vi } from "vitest";
import { T3HttpClient } from "../src/t3/http-client.js";
import { FakeT3 } from "./support/fake-t3.js";
import { gatewayFixture } from "./support/gateway-fixture.js";

const descriptor = { environmentId: "test-env", label: "Test", serverVersion: "test" };
const shell = { snapshotSequence: 1, projects: [], threads: [], updatedAt: "2026-10-03T08:47:00.000Z" };
const command = { type: "thread.archive", commandId: "archive-1", threadId: "thread-1" } as const;
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });

afterEach(() => vi.restoreAllMocks());

describe("protocol discovery during gateway uptime", () => {
  it("shares concurrent discovery and refreshes again after it completes", async () => {
    const responses: Array<(response: Response) => void> = [];
    const fetchMock = vi.spyOn(globalThis, "fetch").mockImplementation(() => new Promise<Response>((resolve) => responses.push(resolve)));
    const client = new T3HttpClient("http://test.invalid", "token");
    const reads = [client.getShell(), client.getShell()];
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    responses[0]!(json(descriptor));
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(3));
    responses[1]!(json(shell));
    responses[2]!(json(shell));
    await expect(Promise.all(reads)).resolves.toEqual([shell, shell]);

    const next = client.getDescriptor();
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(4));
    responses[3]!(json({ ...descriptor, orchestrationProtocolVersion: 2 }));
    await expect(next).resolves.toMatchObject({ orchestrationProtocolVersion: 2 });
    expect(client.getCachedDescriptor()?.orchestrationProtocolVersion).toBe(2);
  });

  it("cancels one discovery waiter without cancelling another or sending its mutation", async () => {
    const responses: Array<(response: Response) => void> = [];
    const fetchMock = vi.spyOn(globalThis, "fetch").mockImplementation(() => new Promise<Response>((resolve) => responses.push(resolve)));
    const client = new T3HttpClient("http://test.invalid", "token");
    const controller = new AbortController();
    const cancelled = client.dispatch(command, controller.signal);
    const cancelledCheck = expect(cancelled).rejects.toThrow("cancel this caller");
    const survivor = client.getShell();
    controller.abort(new Error("cancel this caller"));
    await cancelledCheck;
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0]?.[1]?.signal?.aborted).toBe(false);
    responses[0]!(json(descriptor));
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    responses[1]!(json(shell));
    await expect(survivor).resolves.toEqual(shell);
    expect(fetchMock.mock.calls.map(([url]) => url)).toEqual([
      "http://test.invalid/.well-known/t3/environment",
      "http://test.invalid/api/orchestration/shell",
    ]);
  });

  it("keeps the header tied to the selected schema while another caller refreshes", async () => {
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json(descriptor))
      .mockResolvedValueOnce(json({ ...descriptor, orchestrationProtocolVersion: 2 }))
      .mockResolvedValueOnce(json(shell));
    const client = new T3HttpClient("http://test.invalid", "token");
    // A concurrent refresh can complete between selecting a schema and fetch.
    const discover = client.getDescriptor.bind(client);
    vi.spyOn(client, "getDescriptor").mockImplementationOnce(async () => {
      const selected = await discover();
      await discover();
      return selected;
    });
    await expect(client.getShell()).resolves.toEqual(shell);
    expect(client.getCachedDescriptor()?.orchestrationProtocolVersion).toBe(2);
    expect(fetchMock.mock.calls[2]?.[1]?.headers).not.toHaveProperty("x-t3-orchestration-protocol");
  });

  it("coalesces refreshes after concurrent protocol failures and retries each read once", async () => {
    const responses: Array<(response: Response) => void> = [];
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json({ ...descriptor, orchestrationProtocolVersion: 2 }))
      .mockResolvedValueOnce(json({ code: "invalid_request", reason: "protocol" }, 400))
      .mockResolvedValueOnce(json({ code: "invalid_request", reason: "protocol" }, 400))
      .mockImplementation(() => new Promise<Response>((resolve) => responses.push(resolve)));
    const client = new T3HttpClient("http://test.invalid", "token");
    await client.getDescriptor();
    const reads = [client.getShell(), client.getShell()];
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(4));
    responses[0]!(json(descriptor));
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(6));
    responses[1]!(json(shell));
    responses[2]!(json(shell));
    await expect(Promise.all(reads)).resolves.toEqual([shell, shell]);
    expect(fetchMock.mock.calls.filter(([url]) => String(url).endsWith("/.well-known/t3/environment"))).toHaveLength(2);
  });

  it("refreshes before a mutation and does not send it if discovery fails", async () => {
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json(descriptor))
      .mockRejectedValueOnce(new Error("discovery unavailable"))
      .mockResolvedValueOnce(json(descriptor))
      .mockResolvedValueOnce(json(shell));
    const client = new T3HttpClient("http://test.invalid", "token");
    await client.getDescriptor();
    await expect(client.dispatch(command)).rejects.toMatchObject({
      status: 400,
      code: "protocol_discovery_failed",
      reason: "command_not_sent",
      message: expect.stringContaining("T3 command not sent: discovery unavailable"),
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(client.telemetry().lastError).toContain("discovery unavailable");
    await client.getDescriptor();
    await expect(client.getShell()).resolves.toEqual(shell);
    expect(fetchMock).toHaveBeenCalledTimes(4);
  });

  it.each(["network", "HTTP", "schema", "unsupported"] as const)("records a %s preflight failure as rejected and permits safe fresh-key retry", async (failure) => {
    const fake = new FakeT3();
    await fake.start();
    const fixture = await gatewayFixture(fake);
    try {
      fake.addThread({ id: "thread-1" });
      await fixture.client.getDescriptor();
      const fetch = globalThis.fetch;
      let failDiscovery = true;
      vi.spyOn(globalThis, "fetch").mockImplementation((url, init) => {
        if (String(url).endsWith("/.well-known/t3/environment") && failDiscovery) {
          failDiscovery = false;
          if (failure === "network") return Promise.reject(new Error("descriptor offline"));
          if (failure === "HTTP") return Promise.resolve(json({ message: "descriptor offline" }, 503));
          if (failure === "schema") return Promise.resolve(json({ invalid: true }));
          return Promise.resolve(json({ ...descriptor, orchestrationProtocolVersion: 3 }));
        }
        return fetch(url, init);
      });
      const input = { threadId: "thread-1", idempotencyKey: "not-sent" };
      const rejected = await fixture.gateway.threadArchive(input);
      expect(rejected).toMatchObject({ status: "rejected", reason: expect.stringContaining("command not sent") });
      expect(rejected).toMatchObject({ reason: expect.stringContaining("new idempotency key") });
      expect(fake.dispatches).toHaveLength(0);
      expect(await fixture.journal.getByIdempotencyKey(input.idempotencyKey)).toMatchObject({ status: "rejected" });
      expect(await fixture.gateway.threadArchive(input)).toMatchObject({ status: "rejected", operationId: rejected.operationId });
      expect(fake.dispatches).toHaveLength(0);
      expect(await fixture.gateway.threadArchive({ ...input, idempotencyKey: "safe-retry" })).toMatchObject({ status: "accepted" });
      expect(fake.dispatches).toHaveLength(1);
    } finally {
      await fixture.cleanup();
      await fake.close();
    }
  });

  it("preserves the original read error when protocol refresh fails", async () => {
    const error = { code: "invalid_request", reason: "protocol", message: "original failure" };
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json(descriptor))
      .mockResolvedValueOnce(json(error, 400))
      .mockRejectedValueOnce(new Error("discovery unavailable"));
    const client = new T3HttpClient("http://test.invalid", "token");
    await expect(client.getShell()).rejects.toMatchObject({ ...error, status: 400 });
    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(client.telemetry().lastError).toBe(error.message);
  });

  it("preserves an invalid response error without replay when the protocol is unchanged", async () => {
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json(descriptor))
      .mockResolvedValueOnce(json({ unexpected: true }))
      .mockResolvedValueOnce(json(descriptor));
    const client = new T3HttpClient("http://test.invalid", "token");
    await expect(client.getShell()).rejects.toThrow("T3 GET /api/orchestration/shell returned an invalid response.");
    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(client.telemetry().lastError).toBe("T3 GET /api/orchestration/shell returned an invalid response.");
  });

  it("honors cancellation during refresh without retrying the read", async () => {
    const responses: Array<(response: Response) => void> = [];
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json(descriptor))
      .mockResolvedValueOnce(json({ code: "invalid_request", reason: "protocol" }, 400))
      .mockImplementation(() => new Promise<Response>((resolve) => responses.push(resolve)));
    const client = new T3HttpClient("http://test.invalid", "token");
    const controller = new AbortController();
    const read = client.getShell(controller.signal);
    const rejection = expect(read).rejects.toThrow("cancel refresh");
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(3));
    controller.abort(new Error("cancel refresh"));
    await rejection;
    responses[0]!(json({ ...descriptor, orchestrationProtocolVersion: 2 }));
    await client.getDescriptor();
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("does not refresh or retry again if the read still fails after a protocol change", async () => {
    const error = { code: "invalid_request", reason: "protocol", message: "still rejected" };
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json({ ...descriptor, orchestrationProtocolVersion: 2 }))
      .mockResolvedValueOnce(json(error, 400))
      .mockResolvedValueOnce(json(descriptor))
      .mockResolvedValueOnce(json(error, 400));
    const client = new T3HttpClient("http://test.invalid", "token");
    await expect(client.getShell()).rejects.toMatchObject({ ...error, status: 400 });
    expect(fetchMock).toHaveBeenCalledTimes(4);
    expect(client.telemetry().lastError).toBe(error.message);
  });

  it.each([
    { operation: "read", status: 400, code: "invalid_request", reason: "protocol" },
    { operation: "read", status: 403, code: "insufficient_scope", reason: "read_forbidden" },
    { operation: "read", status: 404, code: "not_found", reason: "thread_not_found" },
    { operation: "read", status: 503, code: "unavailable", reason: "offline" },
    { operation: "mutation", status: 400, code: "invalid_request", reason: "protocol" },
    { operation: "mutation", status: 409, code: "conflict", reason: "already_accepted" },
    { operation: "mutation", status: 503, code: "unavailable", reason: "offline" },
  ])("preserves $operation errors without retry ($status)", async ({ operation, status, code, reason }) => {
    const error = { code, reason, traceId: "trace-1", message: "upstream error" };
    const fetchMock = vi.spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json(descriptor))
      .mockResolvedValueOnce(json(error, status))
      .mockResolvedValueOnce(json(descriptor));
    const client = new T3HttpClient("http://test.invalid", "token");
    await expect(operation === "read" ? client.getThread("thread-1") : client.dispatch(command)).rejects.toMatchObject({
      ...error, status, method: operation === "read" ? "GET" : "POST",
      path: operation === "read" ? "/api/orchestration/threads/thread-1" : "/api/orchestration/dispatch",
    });
    expect(fetchMock).toHaveBeenCalledTimes(operation === "read" && status === 400 ? 3 : 2);
    expect(client.telemetry().lastError).toBe(error.message);
  });
});
