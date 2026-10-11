import { createHash, randomBytes } from "node:crypto";
import { createInterface } from "node:readline";
import { Writable } from "node:stream";
import { z } from "zod";
import { NATIVE_ACCESS_LEVELS, type NativeAccessLevel, type NativeCredential } from "./t3/native-credential.js";

// A loopback redirect marks this client as local. T3 returns the authorization
// code in the decision response, so nothing listens on this address.
const REDIRECT_URI = "http://127.0.0.1/callback";

const ResourceMetadata = z.object({ resource: z.string(), authorization_servers: z.array(z.string()).min(1) });
const AuthorizationServerMetadata = z.object({
  issuer: z.string(),
  registration_endpoint: z.string(),
  token_endpoint: z.string(),
});
const RegisteredClient = z.object({ client_id: z.string().min(1) });
const DecisionRedirect = z.object({ redirectTo: z.string() });
const TokenResult = z.object({
  access_token: z.string().min(1),
  token_type: z.literal("Bearer"),
  expires_in: z.number().positive(),
  scope: z.string(),
});

export interface NativeLoginInput {
  /** The native MCP endpoint, for example http://127.0.0.1:3773/mcp. */
  readonly resource: string;
  readonly access: NativeAccessLevel;
  /** A one-time code from `t3 auth pairing create`. Its scopes must cover the access level. */
  readonly pairingCode: string;
  readonly now?: () => number;
}

/**
 * Obtains a T3 MCP OAuth credential without a browser: dynamic registration,
 * PKCE, and an approval decision backed by a pairing code. T3 issues no refresh
 * token, so renewal repeats this flow with a new pairing code.
 */
export async function loginNative(input: NativeLoginInput): Promise<NativeCredential> {
  const resourceUrl = new URL(input.resource);
  const resourceMetadata = ResourceMetadata.parse(
    await getJson(new URL(`/.well-known/oauth-protected-resource${resourceUrl.pathname}`, resourceUrl.origin)),
  );
  const issuer = resourceMetadata.authorization_servers[0];
  if (issuer === undefined) throw new Error("Native T3 advertised no authorization server.");
  const server = AuthorizationServerMetadata.parse(await getJson(new URL("/.well-known/oauth-authorization-server", issuer)));

  const client = RegisteredClient.parse(await postJson(server.registration_endpoint, {
    client_name: "t3-code-mcp gateway",
    redirect_uris: [REDIRECT_URI],
    token_endpoint_auth_method: "none",
  }));
  const verifier = randomBytes(32).toString("base64url");
  const state = randomBytes(16).toString("base64url");
  const authorization = {
    response_type: "code",
    client_id: client.client_id,
    redirect_uri: REDIRECT_URI,
    code_challenge: createHash("sha256").update(verifier).digest("base64url"),
    code_challenge_method: "S256",
    state,
    resource: resourceMetadata.resource,
  };
  // T3-specific approval endpoint; standard OAuth leaves consent to the server.
  const decision = DecisionRedirect.parse(await postJson(new URL("/oauth/mcp/decision", server.issuer).toString(), {
    authorization,
    decision: { _tag: "pairing-code", access: input.access, code: input.pairingCode },
  }));
  const redirect = new URL(decision.redirectTo);
  if (redirect.searchParams.get("state") !== state) throw new Error("Native T3 returned a mismatched OAuth state.");
  const code = redirect.searchParams.get("code");
  if (code === null) throw new Error(`Native T3 denied the login: ${redirect.searchParams.get("error") ?? "no authorization code"}.`);

  const obtainedAt = (input.now ?? Date.now)();
  const response = await fetch(server.token_endpoint, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      code,
      redirect_uri: REDIRECT_URI,
      client_id: client.client_id,
      code_verifier: verifier,
      resource: resourceMetadata.resource,
    }),
  });
  const token = TokenResult.parse(await readJson(response, "token exchange"));
  return {
    resource: resourceMetadata.resource,
    clientId: client.client_id,
    accessToken: token.access_token,
    access: input.access,
    scopes: token.scope.split(" ").filter((scope) => scope !== ""),
    obtainedAt: new Date(obtainedAt).toISOString(),
    expiresAt: new Date(obtainedAt + token.expires_in * 1000).toISOString(),
  };
}

export function parseNativeLoginArgs(args: ReadonlyArray<string>): { readonly access: NativeAccessLevel } {
  let access: NativeAccessLevel = "full-access";
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === "--access") {
      const value = NATIVE_ACCESS_LEVELS.find((level) => level === args[index + 1]);
      if (value === undefined) throw new Error(`--access must be one of ${NATIVE_ACCESS_LEVELS.join(", ")}.`);
      access = value;
      index += 1;
      continue;
    }
    if (arg?.startsWith("--code")) {
      throw new Error("Do not pass the pairing code as an argument; process lists expose it. Pipe it on stdin or type it at the prompt.");
    }
    throw new Error("Use native-login [--access <level>] and supply the pairing code on stdin.");
  }
  return { access };
}

/** Reads the pairing code from a hidden prompt, or the first line of piped stdin. */
export async function readPairingCode(): Promise<string> {
  const muted = new Writable({ write: (_chunk, _encoding, callback) => callback() });
  if (process.stdin.isTTY) process.stderr.write("T3 pairing code (from `t3 auth pairing create`): ");
  const reader = createInterface({ input: process.stdin, output: muted, terminal: process.stdin.isTTY === true });
  try {
    for await (const line of reader) {
      const code = line.trim();
      if (code !== "") return code;
    }
  } finally {
    reader.close();
    if (process.stdin.isTTY) process.stderr.write("\n");
  }
  throw new Error("No pairing code was supplied on stdin.");
}

async function getJson(url: URL): Promise<unknown> {
  return readJson(await fetch(url, { headers: { accept: "application/json" } }), url.pathname);
}

async function postJson(url: string, body: unknown): Promise<unknown> {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  return readJson(response, new URL(url).pathname);
}

async function readJson(response: Response, step: string): Promise<unknown> {
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`Native T3 ${step} failed with HTTP ${response.status}: ${text.slice(0, 300)}`);
  }
  return JSON.parse(text);
}
