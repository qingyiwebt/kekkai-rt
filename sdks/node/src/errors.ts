import type { KekkaiRuntimeErrorKind } from "./types.js";

export class KekkaiRuntimeError extends Error {
  readonly kind: KekkaiRuntimeErrorKind;
  readonly operation: string;
  readonly status: number | undefined;
  readonly details: unknown;

  constructor(
    message: string,
    options: {
      kind: KekkaiRuntimeErrorKind;
      operation: string;
      status?: number;
      details?: unknown;
      cause?: unknown;
    },
  ) {
    super(message, { cause: options.cause });
    this.name = "KekkaiRuntimeError";
    this.kind = options.kind;
    this.operation = options.operation;
    this.status = options.status;
    this.details = options.details;
  }
}
