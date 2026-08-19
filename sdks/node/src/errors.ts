export interface AgentCellErrorOptions {
  operation: string;
  status?: number;
  details?: unknown;
  cause?: unknown;
}

export class AgentCellError extends Error {
  readonly operation: string;
  readonly status: number | undefined;
  readonly details: unknown;

  constructor(message: string, options: AgentCellErrorOptions) {
    super(message, { cause: options.cause });
    this.name = new.target.name;
    this.operation = options.operation;
    this.status = options.status;
    this.details = options.details;
  }
}

export class AgentCellHttpError extends AgentCellError {
  readonly status: number;
  readonly responseBody: unknown;

  constructor(
    operation: string,
    status: number,
    responseBody: unknown,
  ) {
    super(messageFromResponse(status, responseBody), {
      operation,
      status,
      details: responseBody,
    });
    this.name = new.target.name;
    this.status = status;
    this.responseBody = responseBody;
  }
}

export class AgentCellValidationError extends AgentCellError {
  constructor(message: string, operation = "validation", details?: unknown) {
    super(message, { operation, details });
    this.name = new.target.name;
  }
}

export class AgentCellProtocolError extends AgentCellError {
  constructor(message: string, operation: string, details?: unknown) {
    super(message, { operation, details });
    this.name = new.target.name;
  }
}

function messageFromResponse(status: number, body: unknown): string {
  if (isRecord(body) && typeof body.error === "string") {
    return body.error;
  }
  if (typeof body === "string" && body.length > 0) {
    return body;
  }
  return `AgentCell request failed with HTTP ${status}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
