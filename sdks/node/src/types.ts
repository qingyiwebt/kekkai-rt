export interface AgentCellClientOptions {
  /** The HTTP(S) origin, optionally including a path prefix. */
  baseUrl: string | URL;
  /** Bearer token configured in AgentCell's api.secret setting. */
  token: string;
  /** Injectable fetch implementation, primarily useful for tests. */
  fetch?: typeof globalThis.fetch;
}

export interface RequestOptions {
  signal?: AbortSignal;
}

export interface StreamOptions extends RequestOptions {}

export interface WaitOptions extends StreamOptions {
  onEvent?: (event: ExecEvent) => void | Promise<void>;
}

export interface ExecRequest {
  argv: readonly string[];
  cwd?: string;
  env?: Readonly<Record<string, string>>;
  stdin?: string;
  timeoutSeconds?: number;
}

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
  events(options?: StreamOptions): AsyncIterable<ExecEvent>;
  snapshot(options?: RequestOptions): Promise<ExecSnapshot>;
  wait(options?: WaitOptions): Promise<ExecResult>;
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

export interface WorkspaceClient {
  list(path?: string, options?: RequestOptions): Promise<WorkspaceDirectory>;
  readFile(path: string, options?: RequestOptions): Promise<Uint8Array>;
  writeFile(
    path: string,
    data: string | Uint8Array,
    options?: RequestOptions,
  ): Promise<void>;
  delete(path: string, options?: RequestOptions): Promise<void>;
}
