import { describe, expect, it } from "vitest";
import { T3HttpError, T3HttpClient } from "../src/t3/http-client.js";
import { FakeT3 } from "./support/fake-t3.js";

describe("T3HttpClient", () => {
  it("uses public descriptor discovery without auth and authenticates private RPCs", async () => {
    const fake = new FakeT3();
    await fake.start();
    try {
      const client = new T3HttpClient(fake.baseUrl, fake.accessToken);

      const descriptor = await client.getDescriptor();
      await client.getSession();
      const shell = await client.getShell();

      expect(descriptor.environmentId).toBe(fake.environmentId);
      expect(fake.requests[0]).toMatchObject({ path: "/.well-known/t3/environment", authorization: undefined });
      expect(fake.requests.slice(1).every((request) => request.authorization === `Bearer ${fake.accessToken}`)).toBe(true);
      expect(client.getCachedDescriptor()?.environmentId).toBe(fake.environmentId);
      expect(shell.snapshotSequence).toBe(1);
      expect(client.telemetry().lastSnapshotAt).not.toBeNull();
      expect(client.telemetry().lastSuccessfulAt).not.toBeNull();
    } finally {
      await fake.close();
    }
  });

  it("URL-encodes thread IDs and sends typed dispatch bodies", async () => {
    const fake = new FakeT3();
    fake.addProject({ id: "project/one" });
    fake.addThread({ id: "thread/with/slash", projectId: "project/one" });
    await fake.start();
    try {
      const client = new T3HttpClient(fake.baseUrl, fake.accessToken);
      await client.getThread("thread/with/slash");
      await client.dispatch({
        type: "thread.archive",
        commandId: "command-1",
        threadId: "thread/with/slash",
      });

      expect(fake.requests.some((request) => request.path === "/api/orchestration/threads/thread%2Fwith%2Fslash")).toBe(true);
      const dispatch = fake.dispatches[0];
      expect(dispatch?.command).toMatchObject({ type: "thread.archive", commandId: "command-1" });
      expect(dispatch?.request.authorization).toBe(`Bearer ${fake.accessToken}`);
    } finally {
      await fake.close();
    }
  });

  it("preserves structured HTTP errors and telemetry", async () => {
    const fake = new FakeT3({ dispatchStatus: 409, dispatchErrorCode: "conflict", dispatchErrorMessage: "already accepted" });
    await fake.start();
    try {
      const client = new T3HttpClient(fake.baseUrl, fake.accessToken);
      await expect(
        client.dispatch({ type: "thread.archive", commandId: "command-1", threadId: "thread-1" }),
      ).rejects.toMatchObject<Partial<T3HttpError>>({
        status: 409,
        method: "POST",
        path: "/api/orchestration/dispatch",
        code: "conflict",
        message: "already accepted",
      });
      expect(client.telemetry().lastError).toBe("already accepted");
      expect(client.telemetry().lastSuccessfulAt).toBeNull();
    } finally {
      await fake.close();
    }
  });

  it("rejects malformed successful responses instead of returning untyped data", async () => {
    const fake = new FakeT3({ invalidResponsePaths: ["/.well-known/t3/environment"] });
    await fake.start();
    try {
      const client = new T3HttpClient(fake.baseUrl, fake.accessToken);
      await expect(client.getDescriptor()).rejects.toThrow("returned an invalid response");
      expect(client.telemetry().lastError).toContain("returned an invalid response");
    } finally {
      await fake.close();
    }
  });

  it("supports cancellation without converting it into a successful response", async () => {
    const fake = new FakeT3({ dispatchDelayMs: 250 });
    await fake.start();
    try {
      const client = new T3HttpClient(fake.baseUrl, fake.accessToken, 2_000);
      const controller = new AbortController();
      const pending = client.dispatch(
        { type: "thread.archive", commandId: "command-1", threadId: "thread-1" },
        controller.signal,
      );
      controller.abort();
      await expect(pending).rejects.toBeInstanceOf(Error);
      expect(client.telemetry().lastError).toBeTruthy();
    } finally {
      await fake.close();
    }
  });
});
