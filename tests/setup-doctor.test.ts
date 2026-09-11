import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { runDoctor } from "../src/doctor.js";
import { parseSetupArgs, renderSetup } from "../src/setup.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { FakeT3 } from "./support/fake-t3.js";

const dirs: string[] = [];
const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((f) => f.cleanup()));
  await Promise.all(fakes.splice(0).map((f) => f.close()));
  await Promise.all(dirs.splice(0).map((d) => rm(d, { recursive: true, force: true })));
});

describe("transferable setup", () => {
  it("renders host files without secrets and never overwrites credentials", async () => {
    const workspace = await mkdtemp(join(tmpdir(), "setup-ws-"));
    const out = await mkdtemp(join(tmpdir(), "setup-out-"));
    const envDir = await mkdtemp(join(tmpdir(), "setup-env-"));
    dirs.push(workspace, out, envDir);
    const envPath = join(envDir, "t3-code-mcp.env");
    const options = parseSetupArgs([
      `--workspace=${workspace}`,
      "--node=/usr/bin/node",
      "--tunnel-bin=/usr/local/bin/tunnel-client",
      "--tunnel-id=tunnel_test123",
      "--environment-id=env-test-1",
      "--out=" + out,
      `--env-path=${envPath}`,
    ]);
    const first = await renderSetup(options);
    expect(first.files).toContain(envPath);
    const env = await readFile(envPath, "utf8");
    expect(env).toContain("T3_ENVIRONMENT_ID=env-test-1");
    expect(env).not.toContain("tunnel_test123");
    expect(env).toMatch(/T3_ACCESS_TOKEN=\n/);
    expect((await stat(envPath)).mode & 0o777).toBe(0o600);
    const gatewayService = await readFile(join(out, "t3-code-mcp.service"), "utf8");
    expect(gatewayService).toContain(workspace);
    expect(gatewayService).not.toContain("tunnel_test123");
    // Second render must not overwrite existing credentials.
    await readFile(envPath, "utf8").then(async (content) => {
      const { writeFile } = await import("node:fs/promises");
      await writeFile(envPath, content.replace("T3_ACCESS_TOKEN=", "T3_ACCESS_TOKEN=secret123"));
    });
    const second = await renderSetup(options);
    expect(second.files).not.toContain(envPath);
    expect(await readFile(envPath, "utf8")).toContain("secret123");
  });

  it("requires explicit tunnel and environment IDs", () => {
    expect(() => parseSetupArgs(["--workspace=/tmp/x"])).toThrow("--tunnel-id");
  });
});

describe("read-only doctor", () => {
  it("diagnoses the gateway chain without mutation", async () => {
    const fake = new FakeT3();
    fakes.push(fake);
    await fake.start();
    const fixture = await gatewayFixture(fake);
    fixtures.push(fixture);
    const result = await runDoctor(fixture.gateway, fixture.config, { tunnelHealthUrl: "http://127.0.0.1:1/readyz" });
    const names = result.checks.map((c) => c.name);
    expect(names).toEqual(expect.arrayContaining(["config", "journal", "t3_identity", "effective_access", "freshness", "build", "mcp_discovery", "overview_read", "tunnel"]));
    expect(result.checks.find((c) => c.name === "t3_identity")?.ok).toBe(true);
    expect(result.checks.find((c) => c.name === "overview_read")?.ok).toBe(true);
    expect(fake.dispatches).toHaveLength(0);
  });

  it("fails closed when T3 is unreachable", async () => {
    const fake = new FakeT3();
    fakes.push(fake);
    await fake.start();
    const fixture = await gatewayFixture(fake);
    fixtures.push(fixture);
    await fake.close();
    const result = await runDoctor(fixture.gateway, fixture.config, { tunnelHealthUrl: "http://127.0.0.1:1/readyz" });
    expect(result.ok).toBe(false);
    expect(result.checks.find((c) => c.name === "t3_identity")?.ok).toBe(false);
  });
});
