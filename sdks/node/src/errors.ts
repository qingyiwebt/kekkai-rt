import type { AgentCellErrorKind } from "./types.js";

export class AgentCellError extends Error {
  readonly kind: AgentCellErrorKind;
  readonly operation: string;
  readonly status: number | undefined;
  readonly details: unknown;

  constructor(
    message: string,
    options: {
      kind: AgentCellErrorKind;
      operation: string;
      status?: number;
      details?: unknown;
      cause?: unknown;
    },
  ) {
    super(message, { cause: options.cause });
    this.name = "AgentCellError";
    this.kind = options.kind;
    this.operation = options.operation;
    this.status = options.status;
    this.details = options.details;
  }
}
