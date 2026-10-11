/** A failure with a stable code that MCP clients can branch on. */
export class GatewayError extends Error {
  override readonly name = "GatewayError";

  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}
