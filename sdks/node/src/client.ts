import {
  AgentCellHttpError,
  AgentCellProtocolError,
  AgentCellValidationError,
} from "./errors.js";
import { parseSse, type RawSseEvent } from "./sse.js";
import type {
  AgentCellClientOptions,
  ExecEvent,
  ExecRequest,
  ExecResult,
  ExecSnapshot,
  ExecStatus,
  ExecTask,
  HealthResponse,
  RequestOptions,
  StreamOptions,
  WaitOptions,
  WorkspaceClient,
  WorkspaceDirectory,
  WorkspaceEntry,
  WorkspaceEntryType,
} from "./types.js";

const TERMINAL_STATUSES = new Set<ExecStatus>([
  "finished",
  "timedOut",
  "failed",
]);

export class AgentCellClient {
  private readonly baseUrl: URL;
  private readonly token: string;
  private readonly fetcher: typeof globalThis.fetch;
  readonly workspace: WorkspaceClient;

  constructor(options: AgentCellClientOptions) {
    this.baseUrl = normalizeBaseUrl(options.baseUrl);
    if (typeof options.token !== "string") {
      throw new AgentCellValidationError("token must be a string", "constructor");
    }
    this.token = options.token;
    this.fetcher = options.fetch ?? globalThis.fetch.bind(globalThis);
    this.workspace = new WorkspaceClientImpl(this);
  }

  async health(): Promise<HealthResponse> {
    const body = await this.requestJson("health", "healthz", {
      method: "GET",
    });
    if (!isRecord(body) || body.status !== "ok") {
      throw new AgentCellProtocolError(
        "health response has an unexpected shape",
        "health",
        body,
      );
    }
    return { status: "ok" };
  }

  async createExec(
    request: ExecRequest,
    options: RequestOptions = {},
  ): Promise<ExecTask> {
    validateExecRequest(request);
    const body = await this.requestJson("createExec", "v1/exec", withSignal({
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(toWireExecRequest(request)),
    }, options.signal));

    if (!isRecord(body) || typeof body.task_id !== "string" || body.task_id === "") {
      throw new AgentCellProtocolError(
        "execution response is missing task_id",
        "createExec",
        body,
      );
    }
    return new ExecTaskHandle(this, body.task_id);
  }

  async getExec(
    taskId: string,
    options: RequestOptions = {},
  ): Promise<ExecSnapshot> {
    validateTaskId(taskId, "getExec");
    const body = await this.requestJson(
      "getExec",
      `v1/exec/${encodeURIComponent(taskId)}`,
      withSignal({ method: "GET" }, options.signal),
    );
    return decodeSnapshot(body, "getExec");
  }

  events(taskId: string, options: StreamOptions = {}): AsyncIterable<ExecEvent> {
    validateTaskId(taskId, "events");
    return this.readEvents(taskId, options.signal);
  }

  async execute(
    request: ExecRequest,
    options: WaitOptions = {},
  ): Promise<ExecResult> {
    const task = await this.createExec(request, options);
    return task.wait(options);
  }

  private async *readEvents(
    taskId: string,
    signal: AbortSignal | undefined,
  ): AsyncGenerator<ExecEvent> {
    const response = await this.fetcher(this.url(`v1/exec/${encodeURIComponent(taskId)}/events`), withSignal({
      method: "GET",
      headers: this.headers(),
    }, signal));
    if (!response.ok) {
      throw new AgentCellHttpError(
        "events",
        response.status,
        await readResponseBody(response),
      );
    }
    if (!response.body) {
      throw new AgentCellProtocolError(
        "SSE response has no body",
        "events",
      );
    }

    for await (const rawEvent of parseSse(response.body)) {
      yield decodeEvent(rawEvent, "events");
    }
  }

  private async requestJson(
    operation: string,
    path: string,
    init: RequestInit,
  ): Promise<unknown> {
    const response = await this.fetcher(this.url(path), {
      ...init,
      headers: { ...this.headers(), ...init.headers },
    });
    const body = await readResponseBody(response);
    if (!response.ok) {
      throw new AgentCellHttpError(operation, response.status, body);
    }
    return body;
  }

  private async requestNoContent(
    operation: string,
    path: string,
    init: RequestInit,
  ): Promise<void> {
    const response = await this.fetcher(this.url(path), {
      ...init,
      headers: { ...this.headers(), ...init.headers },
    });
    if (!response.ok) {
      throw new AgentCellHttpError(
        operation,
        response.status,
        await readResponseBody(response),
      );
    }
    await response.body?.cancel();
  }

  private async requestBytes(
    operation: string,
    path: string,
    init: RequestInit,
  ): Promise<Uint8Array> {
    const response = await this.fetcher(this.url(path), {
      ...init,
      headers: { ...this.headers(), ...init.headers },
    });
    if (!response.ok) {
      throw new AgentCellHttpError(
        operation,
        response.status,
        await readResponseBody(response),
      );
    }
    return new Uint8Array(await response.arrayBuffer());
  }

  private headers(): HeadersInit {
    return {
      accept: "application/json",
      authorization: `Bearer ${this.token}`,
    };
  }

  private url(path: string): string {
    return new URL(path, this.baseUrl).toString();
  }

  /** @internal */
  async waitForTask(
    taskId: string,
    options: WaitOptions,
  ): Promise<ExecResult> {
    let terminalEvent = false;
    for await (const event of this.events(taskId, options)) {
      await options.onEvent?.(event);
      if (isTerminalEvent(event)) {
        terminalEvent = true;
        break;
      }
    }
    if (!terminalEvent) {
      throw new AgentCellProtocolError(
        "SSE stream ended before a terminal event",
        "wait",
      );
    }

    const snapshot = await this.getExec(taskId, options);
    if (!TERMINAL_STATUSES.has(snapshot.status)) {
      throw new AgentCellProtocolError(
        "task snapshot is not terminal after a terminal event",
        "wait",
        snapshot,
      );
    }
    return { ...snapshot, status: snapshot.status } as ExecResult;
  }

  /** @internal */
  pathForWorkspace(path: string, allowRoot: boolean): string {
    const segments = validateWorkspacePath(path, allowRoot);
    return segments.length === 0
      ? "v1/workspace"
      : `v1/workspace/${segments.map(encodeURIComponent).join("/")}`;
  }

  /** @internal */
  async listWorkspace(
    path: string,
    options: RequestOptions,
  ): Promise<WorkspaceDirectory> {
    const body = await this.requestJson(
      "workspace.list",
      this.pathForWorkspace(path, true),
      withSignal({ method: "GET" }, options.signal),
    );
    return decodeWorkspaceDirectory(body);
  }

  /** @internal */
  async readWorkspaceFile(
    path: string,
    options: RequestOptions,
  ): Promise<Uint8Array> {
    return this.requestBytes(
      "workspace.readFile",
      this.pathForWorkspace(path, false),
      withSignal({ method: "GET" }, options.signal),
    );
  }

  /** @internal */
  async writeWorkspaceFile(
    path: string,
    data: string | Uint8Array,
    options: RequestOptions,
  ): Promise<void> {
    if (!(typeof data === "string" || data instanceof Uint8Array)) {
      throw new AgentCellValidationError(
        "workspace file data must be a string or Uint8Array",
        "workspace.writeFile",
      );
    }
    const body = typeof data === "string" ? new TextEncoder().encode(data) : data;
    await this.requestNoContent(
      "workspace.writeFile",
      this.pathForWorkspace(path, false),
      withSignal({
        method: "PUT",
        headers: { "content-type": "application/octet-stream" },
        body: body as unknown as BodyInit,
      }, options.signal),
    );
  }

  /** @internal */
  async deleteWorkspace(
    path: string,
    options: RequestOptions,
  ): Promise<void> {
    await this.requestNoContent(
      "workspace.delete",
      this.pathForWorkspace(path, false),
      withSignal({ method: "DELETE" }, options.signal),
    );
  }
}

class ExecTaskHandle implements ExecTask {
  constructor(
    private readonly client: AgentCellClient,
    readonly id: string,
  ) {}

  events(options: StreamOptions = {}): AsyncIterable<ExecEvent> {
    return this.client.events(this.id, options);
  }

  snapshot(options: RequestOptions = {}): Promise<ExecSnapshot> {
    return this.client.getExec(this.id, options);
  }

  wait(options: WaitOptions = {}): Promise<ExecResult> {
    return this.client.waitForTask(this.id, options);
  }
}

class WorkspaceClientImpl implements WorkspaceClient {
  constructor(private readonly client: AgentCellClient) {}

  list(path = "", options: RequestOptions = {}): Promise<WorkspaceDirectory> {
    return this.client.listWorkspace(path, options);
  }

  readFile(path: string, options: RequestOptions = {}): Promise<Uint8Array> {
    return this.client.readWorkspaceFile(path, options);
  }

  writeFile(
    path: string,
    data: string | Uint8Array,
    options: RequestOptions = {},
  ): Promise<void> {
    return this.client.writeWorkspaceFile(path, data, options);
  }

  delete(path: string, options: RequestOptions = {}): Promise<void> {
    return this.client.deleteWorkspace(path, options);
  }
}

function normalizeBaseUrl(input: string | URL): URL {
  let url: URL;
  try {
    url = new URL(input);
  } catch (error) {
    throw new AgentCellValidationError(
      "baseUrl must be a valid absolute URL",
      "constructor",
      error,
    );
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new AgentCellValidationError(
      "baseUrl must use http or https",
      "constructor",
    );
  }
  if (!url.pathname.endsWith("/")) {
    url.pathname += "/";
  }
  return url;
}

function validateExecRequest(request: ExecRequest): void {
  if (!request || !Array.isArray(request.argv) || request.argv.length === 0) {
    throw new AgentCellValidationError(
      "argv must contain at least one command argument",
      "createExec",
    );
  }
  if (request.argv.some((argument) => typeof argument !== "string")) {
    throw new AgentCellValidationError(
      "argv entries must be strings",
      "createExec",
    );
  }
  if (request.cwd !== undefined && typeof request.cwd !== "string") {
    throw new AgentCellValidationError("cwd must be a string", "createExec");
  }
  if (request.stdin !== undefined && typeof request.stdin !== "string") {
    throw new AgentCellValidationError("stdin must be a string", "createExec");
  }
  if (
    request.timeoutSeconds !== undefined &&
    (!Number.isSafeInteger(request.timeoutSeconds) || request.timeoutSeconds < 0)
  ) {
    throw new AgentCellValidationError(
      "timeoutSeconds must be a non-negative safe integer",
      "createExec",
    );
  }
  if (request.env !== undefined) {
    if (!isRecord(request.env)) {
      throw new AgentCellValidationError("env must be an object", "createExec");
    }
    for (const [key, value] of Object.entries(request.env)) {
      if (typeof value !== "string") {
        throw new AgentCellValidationError(
          `environment value for ${key} must be a string`,
          "createExec",
        );
      }
    }
  }
}

function toWireExecRequest(request: ExecRequest): Record<string, unknown> {
  return {
    argv: [...request.argv],
    ...(request.cwd === undefined ? {} : { cwd: request.cwd }),
    ...(request.env === undefined ? {} : { env: request.env }),
    ...(request.stdin === undefined ? {} : { stdin: request.stdin }),
    ...(request.timeoutSeconds === undefined
      ? {}
      : { timeout_seconds: request.timeoutSeconds }),
  };
}

function validateTaskId(taskId: string, operation: string): void {
  if (typeof taskId !== "string" || taskId.length === 0) {
    throw new AgentCellValidationError("taskId must be a non-empty string", operation);
  }
}

function validateWorkspacePath(path: string, allowRoot: boolean): string[] {
  if (typeof path !== "string") {
    throw new AgentCellValidationError(
      "workspace path must be a string",
      "workspace",
    );
  }
  if (path === "") {
    if (allowRoot) {
      return [];
    }
    throw new AgentCellValidationError(
      "workspace root cannot be used for this operation",
      "workspace",
    );
  }
  const segments = path.split("/");
  if (
    segments.some(
      (segment) => segment.length === 0 || segment === "." || segment === "..",
    )
  ) {
    throw new AgentCellValidationError(
      "workspace path must contain only normal relative components",
      "workspace",
    );
  }
  return segments;
}

function decodeSnapshot(value: unknown, operation: string): ExecSnapshot {
  if (!isRecord(value)) {
    throw new AgentCellProtocolError("task snapshot must be an object", operation, value);
  }
  const status = decodeStatus(value.status, operation, value);
  if (
    typeof value.task_id !== "string" ||
    !isNullableNumber(value.exit_code) ||
    typeof value.stdout !== "string" ||
    typeof value.stderr !== "string" ||
    !isNullableString(value.error)
  ) {
    throw new AgentCellProtocolError(
      "task snapshot has an unexpected shape",
      operation,
      value,
    );
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

function decodeEvent(raw: RawSseEvent, operation: string): ExecEvent {
  let value: unknown;
  try {
    value = JSON.parse(raw.data);
  } catch (error) {
    throw new AgentCellProtocolError(
      `SSE ${raw.event} event contains invalid JSON`,
      operation,
      error,
    );
  }
  if (!isRecord(value)) {
    throw new AgentCellProtocolError(
      `SSE ${raw.event} event must contain a JSON object`,
      operation,
      value,
    );
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
        throw new AgentCellProtocolError(
          "finished event has an invalid exit_code",
          operation,
          value,
        );
      }
      return { type: "finished", exitCode: value.exit_code };
    case "timed_out":
      return { type: "timedOut" };
    case "failed":
      return { type: "failed", error: requireString(value.error, raw.event, operation) };
    default:
      throw new AgentCellProtocolError(
        `unknown SSE event: ${raw.event}`,
        operation,
        raw,
      );
  }
}

function decodeStatus(value: unknown, operation: string, details: unknown): ExecStatus {
  if (value === "running" || value === "finished" || value === "failed") {
    return value;
  }
  if (value === "timed_out") {
    return "timedOut";
  }
  throw new AgentCellProtocolError("task snapshot has an invalid status", operation, details);
}

function decodeWorkspaceDirectory(value: unknown): WorkspaceDirectory {
  if (!isRecord(value) || value.type !== "directory" || typeof value.path !== "string") {
    throw new AgentCellProtocolError(
      "workspace listing has an unexpected shape",
      "workspace.list",
      value,
    );
  }
  if (!Array.isArray(value.entries)) {
    throw new AgentCellProtocolError(
      "workspace listing entries must be an array",
      "workspace.list",
      value,
    );
  }
  const entries = value.entries.map((entry) => decodeWorkspaceEntry(entry));
  return { path: value.path, type: "directory", entries };
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
    throw new AgentCellProtocolError(
      "workspace entry has an unexpected shape",
      "workspace.list",
      value,
    );
  }
  return { name: value.name, type: value.type, size: value.size };
}

async function readResponseBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (text.length === 0) {
    return undefined;
  }
  if (response.headers.get("content-type")?.includes("application/json")) {
    try {
      return JSON.parse(text) as unknown;
    } catch {
      return text;
    }
  }
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

function isTerminalEvent(event: ExecEvent): boolean {
  return (
    event.type === "finished" ||
    event.type === "timedOut" ||
    event.type === "failed"
  );
}

function requireString(value: unknown, event: string, operation: string): string {
  if (typeof value !== "string") {
    throw new AgentCellProtocolError(
      `${event} event is missing a string value`,
      operation,
      value,
    );
  }
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isNullableNumber(value: unknown): value is number | null {
  return value === null || typeof value === "number";
}

function isNullableString(value: unknown): value is string | null {
  return value === null || typeof value === "string";
}

function isWorkspaceEntryType(value: unknown): value is WorkspaceEntryType {
  return (
    value === "file" ||
    value === "directory" ||
    value === "symlink" ||
    value === "other"
  );
}

function withSignal(init: RequestInit, signal: AbortSignal | undefined): RequestInit {
  return signal === undefined ? init : { ...init, signal };
}
