import { describe, expect, it } from "vitest";
import { loadConfig } from "../src/config.js";

const baseEnv = {
  T3_HTTP_BASE_URL: "https://t3.example.test/",
  T3_ACCESS_TOKEN: "server-token",
  MCP_BEARER_TOKEN: "gateway-token",
};

describe("loadConfig", () => {
  it("normalizes URLs and applies safe defaults", () => {
    const config = loadConfig(baseEnv);

    expect(config.t3HttpBaseUrl).toBe("https://t3.example.test");
    expect(config.mcpBearerToken).toBe("gateway-token");
    expect(config.readOnly).toBe(false);
    expect(config.host).toBe("127.0.0.1");
    expect(config.port).toBe(8787);
    expect(config.staleAfterMs).toBe(30_000);
    expect(config.dataDir).toContain("data");
  });

  it("accepts explicit booleans, ports, environment identity, and data directory", () => {
    const config = loadConfig({
      ...baseEnv,
      MCP_READ_ONLY: "TRUE",
      MCP_HOST: "0.0.0.0",
      MCP_PORT: "9000",
      T3_ENVIRONMENT_ID: "env-1",
      T3_ENVIRONMENT_LABEL: "remote",
      T3_MCP_DATA_DIR: "/tmp/t3-code-mcp-test-data",
      T3_STALE_AFTER_MS: "2500",
    });

    expect(config).toMatchObject({
      readOnly: true,
      host: "0.0.0.0",
      port: 9000,
      environmentId: "env-1",
      environmentLabel: "remote",
      dataDir: "/tmp/t3-code-mcp-test-data",
      staleAfterMs: 2500,
    });
  });

  it.each([
    ["missing token", { ...baseEnv, T3_ACCESS_TOKEN: "" }, "T3_ACCESS_TOKEN must be set."],
    ["bad URL", { ...baseEnv, T3_HTTP_BASE_URL: "not a URL" }, "T3_HTTP_BASE_URL must be a valid URL."],
    ["unsupported protocol", { ...baseEnv, T3_HTTP_BASE_URL: "file:///tmp/t3" }, "T3_HTTP_BASE_URL must use http or https."],
    ["bad boolean", { ...baseEnv, MCP_READ_ONLY: "yes" }, "MCP_READ_ONLY must be true or false."],
    ["bad port", { ...baseEnv, MCP_PORT: "0" }, "MCP_PORT must be a positive integer."],
    ["fractional timeout", { ...baseEnv, T3_STALE_AFTER_MS: "1.5" }, "T3_STALE_AFTER_MS must be a positive integer."],
  ] as const)("rejects %s", (_label, env, message) => {
    expect(() => loadConfig(env)).toThrow(message);
  });

  it("treats whitespace-only optional values as unset", () => {
    const config = loadConfig({
      ...baseEnv,
      MCP_BEARER_TOKEN: "  ",
      T3_ENVIRONMENT_ID: "  ",
      T3_ENVIRONMENT_LABEL: "  ",
    });

    expect(config.mcpBearerToken).toBeNull();
    expect(config.environmentId).toBeNull();
    expect(config.environmentLabel).toBeNull();
  });
});
