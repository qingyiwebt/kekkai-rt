import { KekkaiRuntimeError } from "./errors.js";
import { decodeEvent, decodeSnapshot, decodeTaskId, encodeExecRequest } from "./protocol.js";
import { parseSse } from "./sse.js";
import { Transport, withSignal } from "./transport.js";
import type {
  ExecutionApi,
  ExecEvent,
  ExecRequest,
  ExecResult,
  ExecRunOptions,
  ExecSnapshot,
  ExecStartOptions,
  ExecStatus,
  ExecTask,
  ExecTaskOptions,
  ExecWaitOptions,
} from "./types.js";

const TERMINAL_STATUSES = new Set<ExecStatus>([
  "finished",
  "timedOut",
  "failed",
]);

export class ExecutionClient implements ExecutionApi {
  constructor(private readonly transport: Transport) {}

  async start(
    request: ExecRequest,
    options: ExecStartOptions = {},
  ): Promise<ExecTask> {
    validateExecRequest(request);
    const body = await this.transport.json("exec.start", "v1/exec", withSignal({
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(encodeExecRequest(request)),
    }, options.signal));
    return new TaskHandle(this, decodeTaskId(body, "exec.start"));
  }

  async run(
    request: ExecRequest,
    options: ExecRunOptions = {},
  ): Promise<ExecResult> {
    const task = await this.start(request, options);
    return task.wait(options);
  }

  async snapshot(taskId: string, options: ExecTaskOptions = {}): Promise<ExecSnapshot> {
    validateTaskId(taskId, "exec.snapshot");
    const body = await this.transport.json(
      "exec.snapshot",
      `v1/exec/${encodeURIComponent(taskId)}`,
      withSignal({ method: "GET" }, options.signal),
    );
    return decodeSnapshot(body, "exec.snapshot");
  }

  async *events(taskId: string, options: ExecTaskOptions = {}): AsyncIterable<ExecEvent> {
    validateTaskId(taskId, "exec.events");
    const body = await this.transport.stream(
      "exec.events",
      `v1/exec/${encodeURIComponent(taskId)}/events`,
      options.signal,
    );
    for await (const rawEvent of parseSse(body)) {
      yield decodeEvent(rawEvent, "exec.events");
    }
  }

  async wait(taskId: string, options: ExecWaitOptions = {}): Promise<ExecResult> {
    let terminal = false;
    for await (const event of this.events(taskId, options)) {
      await options.onEvent?.(event);
      if (isTerminalEvent(event)) {
        terminal = true;
        break;
      }
    }
    if (!terminal) {
      throw new KekkaiRuntimeError("SSE stream ended before a terminal event", {
        kind: "protocol",
        operation: "exec.wait",
      });
    }

    const snapshot = await this.snapshot(taskId, options);
    if (!TERMINAL_STATUSES.has(snapshot.status)) {
      throw new KekkaiRuntimeError("task snapshot is not terminal after a terminal event", {
        kind: "protocol",
        operation: "exec.wait",
        details: snapshot,
      });
    }
    return snapshot as ExecResult;
  }
}

class TaskHandle implements ExecTask {
  constructor(
    private readonly execution: ExecutionClient,
    readonly id: string,
  ) {}

  events(options: ExecTaskOptions = {}): AsyncIterable<ExecEvent> {
    return this.execution.events(this.id, options);
  }

  snapshot(options: ExecTaskOptions = {}): Promise<ExecSnapshot> {
    return this.execution.snapshot(this.id, options);
  }

  wait(options: ExecWaitOptions = {}): Promise<ExecResult> {
    return this.execution.wait(this.id, options);
  }
}

function validateExecRequest(request: ExecRequest): void {
  if (!isRecord(request) || typeof request.command !== "string" || request.command.length === 0) {
    throw validationError(
      "command must be a non-empty string",
      "exec.start",
    );
  }
  if (
    request.args !== undefined &&
    (!Array.isArray(request.args) || request.args.some((argument) => typeof argument !== "string"))
  ) {
    throw validationError("args must be an array of strings", "exec.start");
  }
  if (request.cwd !== undefined && typeof request.cwd !== "string") {
    throw validationError("cwd must be a string", "exec.start");
  }
  if (request.input !== undefined && typeof request.input !== "string") {
    throw validationError("input must be a string", "exec.start");
  }
  if (
    request.timeoutMs !== undefined &&
    (!Number.isSafeInteger(request.timeoutMs) || request.timeoutMs < 0)
  ) {
    throw validationError(
      "timeoutMs must be a non-negative safe integer",
      "exec.start",
    );
  }
  if (request.env !== undefined) {
    if (!isRecord(request.env)) {
      throw validationError("env must be an object", "exec.start");
    }
    for (const [key, value] of Object.entries(request.env)) {
      if (typeof value !== "string") {
        throw validationError(
          `environment value for ${key} must be a string`,
          "exec.start",
        );
      }
    }
  }
}

function validateTaskId(taskId: string, operation: string): void {
  if (typeof taskId !== "string" || taskId.length === 0) {
    throw validationError("task id must be a non-empty string", operation);
  }
}

function isTerminalEvent(event: ExecEvent): boolean {
  return (
    event.type === "finished" ||
    event.type === "timedOut" ||
    event.type === "failed"
  );
}

function validationError(message: string, operation: string): KekkaiRuntimeError {
  return new KekkaiRuntimeError(message, {
    kind: "validation",
    operation,
  });
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
