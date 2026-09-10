import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { GatewayConfig } from "../../src/config.js";
import { makeGateway } from "../../src/gateway.js";
import type { T3Gateway } from "../../src/gateway.js";
import type { OperationJournal } from "../../src/operations/journal.js";
import { T3HttpClient } from "../../src/t3/http-client.js";
import { FakeT3 } from "./fake-t3.js";

export interface GatewayFixture {
  readonly directory: string;
  readonly config: GatewayConfig;
  readonly gateway: T3Gateway;
  readonly client: T3HttpClient;
  readonly journal: OperationJournal;
  readonly cleanup: () => Promise<void>;
}

export async function gatewayFixture(fake: FakeT3, overrides: Partial<GatewayConfig> = {}): Promise<GatewayFixture> {
  const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-gateway-test-"));
  const config: GatewayConfig = {
    t3HttpBaseUrl: overrides.t3HttpBaseUrl ?? fake.baseUrl,
    t3AccessToken: fake.accessToken,
    mcpBearerToken: "gateway-test-token",
    readOnly: false,
    host: "127.0.0.1",
    port: 0,
    environmentId: null,
    environmentLabel: null,
    dataDir: directory,
    staleAfterMs: 30_000,
    ...overrides,
  };
  const { gateway, client, journal } = makeGateway(config);
  return {
    directory,
    config,
    gateway,
    client,
    journal,
    cleanup: () => rm(directory, { recursive: true, force: true }),
  };
}
