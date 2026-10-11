import { createHash } from "node:crypto";
import { chmod, mkdir, readFile, rename, stat, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { z } from "zod";

export const NATIVE_CREDENTIAL_FILE = "native-mcp-credential.json";

/** T3's MCP OAuth access levels. The level is also the runtime-mode ceiling for native launches. */
export const NATIVE_ACCESS_LEVELS = ["read-only", "approval-required", "auto-accept-edits", "auto", "full-access"] as const;

export type NativeAccessLevel = (typeof NATIVE_ACCESS_LEVELS)[number];

const NativeCredentialSchema = z.object({
  resource: z.string().min(1),
  clientId: z.string().min(1),
  accessToken: z.string().min(1),
  access: z.enum(NATIVE_ACCESS_LEVELS),
  scopes: z.array(z.string()),
  obtainedAt: z.string(),
  expiresAt: z.string(),
});

export type NativeCredential = z.infer<typeof NativeCredentialSchema>;

export type NativeCredentialState =
  | { readonly state: "missing"; readonly reason: string }
  | { readonly state: "expired"; readonly credential: NativeCredential }
  | { readonly state: "valid"; readonly credential: NativeCredential };

/**
 * The native MCP token lives in the gateway data directory, not the
 * environment, so `native-login` can renew it without a service restart.
 * Reads are cached by file identity and always reflect the latest login.
 */
export class NativeCredentialStore {
  private cached: { readonly key: string; readonly credential: NativeCredential } | null = null;

  constructor(readonly filePath: string) {}

  static inDataDir(dataDir: string): NativeCredentialStore {
    return new NativeCredentialStore(join(dataDir, NATIVE_CREDENTIAL_FILE));
  }

  async read(now = Date.now()): Promise<NativeCredentialState> {
    let key: string;
    try {
      const info = await stat(this.filePath);
      key = `${info.mtimeMs}:${info.size}:${info.ino}`;
    } catch (error) {
      this.cached = null;
      return {
        state: "missing",
        reason: errorCode(error) === "ENOENT"
          ? "No native MCP credential. Run `t3-code-mcp native-login` with a T3 pairing code."
          : unreadableReason(error),
      };
    }
    if (this.cached?.key !== key) {
      let text: string;
      try {
        text = await readFile(this.filePath, "utf8");
      } catch (error) {
        this.cached = null;
        return { state: "missing", reason: unreadableReason(error) };
      }
      const parsed = NativeCredentialSchema.safeParse(parseJson(text));
      if (!parsed.success) {
        this.cached = null;
        return { state: "missing", reason: "The native MCP credential file is invalid. Run `t3-code-mcp native-login` again." };
      }
      this.cached = { key, credential: parsed.data };
    }
    const credential = this.cached.credential;
    return Date.parse(credential.expiresAt) <= now ? { state: "expired", credential } : { state: "valid", credential };
  }

  async write(credential: NativeCredential): Promise<void> {
    await mkdir(dirname(this.filePath), { recursive: true, mode: 0o700 });
    const temporary = `${this.filePath}.${process.pid}.tmp`;
    await writeFile(temporary, `${JSON.stringify(credential, null, 2)}\n`, { mode: 0o600 });
    await chmod(temporary, 0o600);
    await rename(temporary, this.filePath);
  }
}

/** The reason to report when the credential path fails with an error other than ENOENT, such as EACCES or EISDIR. */
function unreadableReason(error: unknown): string {
  return `The native MCP credential file cannot be read (${errorCode(error) ?? "unknown error"}). ` +
    "Check the owner and mode of the file and its directory, or run `t3-code-mcp native-login` as the gateway user.";
}

function errorCode(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error && typeof error.code === "string" ? error.code : undefined;
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

/** Stable, non-secret identity for a token, used to detect renewal. */
export function tokenIdentity(token: string): string {
  return createHash("sha256").update(token).digest("hex").slice(0, 16);
}
