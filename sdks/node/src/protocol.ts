import { KekkaiRuntimeError } from "./errors.js";
import type {
  ExecEvent,
  ExecRequest,
  ExecSnapshot,
  ExecStatus,
  HealthResponse,
  WorkspaceDirectory,
  WorkspaceEntry,
  WorkspaceEntryType,
} from "./types.js";
import type { RawSseEvent } from "./sse.js";

export interface WireExecRequest {
  argv: string[];
  cwd?: string;
  env?: Readonly<Record<string, string>>;
  stdin?: string;
  timeout_seconds?: number;
}

export function encodeExecRequest(request: ExecRequest): WireExecRequest {
  return {
    argv: [request.command, ...(request.args ?? [])],
    ...(request.cwd === undefined ? {} : { cwd: request.cwd }),
    ...(request.env === undefined ? {} : { env: request.env }),
    ...(request.input === undefined ? {} : { stdin: request.input }),
    ...(request.timeoutMs === undefined
      ? {}
      : { timeout_seconds: Math.ceil(request.timeoutMs / 1000) }),
  };
}

export function decodeHealth(value: unknown): HealthResponse {
  if (!isRecord(value) || value.status !== "ok") {
    throw protocolError("health response has an unexpected shape", "health", value);
  }
  return { status: "ok" };
}

export function decodeTaskId(value: unknown, operation: string): string {
  if (!isRecord(value) || typeof value.task_id !== "string" || value.task_id === "") {
    throw protocolError("execution response is missing task_id", operation, value);
  }
  return value.task_id;
}

export function decodeSnapshot(value: unknown, operation: string): ExecSnapshot {
  if (!isRecord(value)) {
    throw protocolError("task snapshot must be an object", operation, value);
  }
  const status = decodeStatus(value.status, operation, value);
  if (
    typeof value.task_id !== "string" ||
    !isNullableNumber(value.exit_code) ||
    typeof value.stdout !== "string" ||
    typeof value.stderr !== "string" ||
    !isNullableString(value.error)
  ) {
    throw protocolError("task snapshot has an unexpected shape", operation, value);
  }
  return {
    taskId: value.task_id,
    status,
    exitCode: value.exit_code,
    stdout: value.stdout,
    stderr: value.stderr,
    error: value.error,
  };
}

export function decodeEvent(raw: RawSseEvent, operation: string): ExecEvent {
  let value: unknown;
  try {
    value = JSON.parse(raw.data);
  } catch (error) {
    throw protocolError(`SSE ${raw.event} event contains invalid JSON`, operation, error);
  }
  if (!isRecord(value)) {
    throw protocolError(`SSE ${raw.event} event must contain a JSON object`, operation, value);
  }

  switch (raw.event) {
    case "started":
      return { type: "started" };
    case "stdout":
      return { type: "stdout", data: requireString(value.data, raw.event, operation) };
    case "stderr":
      return { type: "stderr", data: requireString(value.data, raw.event, operation) };
    case "finished":
      if (!isNullableNumber(value.exit_code)) {
        throw protocolError("finished event has an invalid exit_code", operation, value);
      }
      return { type: "finished", exitCode: value.exit_code };
    case "timed_out":
      return { type: "timedOut" };
    case "failed":
      return { type: "failed", error: requireString(value.error, raw.event, operation) };
    default:
      throw protocolError(`unknown SSE event: ${raw.event}`, operation, raw);
  }
}

export function decodeWorkspaceDirectory(value: unknown): WorkspaceDirectory {
  if (!isRecord(value) || value.type !== "directory" || typeof value.path !== "string") {
    throw protocolError("workspace listing has an unexpected shape", "workspace.list", value);
  }
  if (!Array.isArray(value.entries)) {
    throw protocolError("workspace listing entries must be an array", "workspace.list", value);
  }
  return {
    path: value.path,
    type: "directory",
    entries: value.entries.map((entry) => decodeWorkspaceEntry(entry)),
  };
}

function decodeWorkspaceEntry(value: unknown): WorkspaceEntry {
  if (
    !isRecord(value) ||
    typeof value.name !== "string" ||
    !isWorkspaceEntryType(value.type) ||
    typeof value.size !== "number" ||
    !Number.isSafeInteger(value.size) ||
    value.size < 0
  ) {
    throw protocolError("workspace entry has an unexpected shape", "workspace.list", value);
  }
  return { name: value.name, type: value.type, size: value.size };
}

function decodeStatus(value: unknown, operation: string, details: unknown): ExecStatus {
  if (value === "running" || value === "finished" || value === "failed") {
    return value;
  }
  if (value === "timed_out") {
    return "timedOut";
  }
  throw protocolError("task snapshot has an invalid status", operation, details);
}

function requireString(value: unknown, event: string, operation: string): string {
  if (typeof value !== "string") {
    throw protocolError(`${event} event is missing a string value`, operation, value);
  }
  return value;
}

function protocolError(message: string, operation: string, details: unknown): KekkaiRuntimeError {
  return new KekkaiRuntimeError(message, {
    kind: "protocol",
    operation,
    details,
  });
}

function isWorkspaceEntryType(value: unknown): value is WorkspaceEntryType {
  return (
    value === "file" ||
    value === "directory" ||
    value === "symlink" ||
    value === "other"
  );
}

function isNullableNumber(value: unknown): value is number | null {
  return value === null || typeof value === "number";
}

function isNullableString(value: unknown): value is string | null {
  return value === null || typeof value === "string";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
