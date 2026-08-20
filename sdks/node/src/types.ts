export interface AgentCellClientOptions {
  /** The HTTP(S) origin, optionally including a path prefix. */
  baseUrl: string | URL;
  /** Bearer token configured in AgentCell's api.secret setting. */
  token: string;
  /** Injectable fetch implementation, primarily useful for tests. */
  fetch?: typeof globalThis.fetch;
}

export interface ExecRequest {
  /** Absolute or container-relative executable path. */
  command: string;
  /** Arguments passed to the executable without shell interpolation. */
  args?: readonly string[];
  /** Working directory inside the sandbox container. */
  cwd?: string;
  /** Environment variables added to the process. */
  env?: Readonly<Record<string, string>>;
  /** UTF-8 text written to the process stdin. */
  input?: string;
  /** Process timeout in milliseconds. */
  timeoutMs?: number;
}

export interface ExecStartOptions {
  signal?: AbortSignal;
}

export interface ExecWaitOptions extends ExecStartOptions {
  onEvent?: (event: ExecEvent) => void | Promise<void>;
}

export interface ExecRunOptions extends ExecWaitOptions {}

export interface ExecTaskOptions extends ExecStartOptions {}

export type ExecStatus = "running" | "finished" | "timedOut" | "failed";

export interface ExecSnapshot {
  taskId: string;
  status: ExecStatus;
  exitCode: number | null;
  stdout: string;
  stderr: string;
  error: string | null;
}

export type ExecResult = ExecSnapshot & {
  status: Exclude<ExecStatus, "running">;
};

export type ExecEvent =
  | { type: "started" }
  | { type: "stdout"; data: string }
  | { type: "stderr"; data: string }
  | { type: "finished"; exitCode: number | null }
  | { type: "timedOut" }
  | { type: "failed"; error: string };

export interface ExecTask {
  readonly id: string;
  events(options?: ExecTaskOptions): AsyncIterable<ExecEvent>;
  snapshot(options?: ExecTaskOptions): Promise<ExecSnapshot>;
  wait(options?: ExecWaitOptions): Promise<ExecResult>;
}

export interface ExecutionApi {
  start(request: ExecRequest, options?: ExecStartOptions): Promise<ExecTask>;
  run(request: ExecRequest, options?: ExecRunOptions): Promise<ExecResult>;
}

export interface HealthResponse {
  status: "ok";
}

export type WorkspaceEntryType =
  | "file"
  | "directory"
  | "symlink"
  | "other";

export interface WorkspaceEntry {
  name: string;
  type: WorkspaceEntryType;
  size: number;
}

export interface WorkspaceDirectory {
  path: string;
  type: "directory";
  entries: WorkspaceEntry[];
}

export interface WorkspaceOptions {
  signal?: AbortSignal;
}

export interface WorkspaceApi {
  list(path?: string, options?: WorkspaceOptions): Promise<WorkspaceDirectory>;
  read(path: string, options?: WorkspaceOptions): Promise<Uint8Array>;
  readText(path: string, options?: WorkspaceOptions): Promise<string>;
  write(
    path: string,
    data: string | Uint8Array,
    options?: WorkspaceOptions,
  ): Promise<void>;
  remove(path: string, options?: WorkspaceOptions): Promise<void>;
}

export type AgentCellErrorKind =
  | "validation"
  | "http"
  | "protocol"
  | "aborted";
