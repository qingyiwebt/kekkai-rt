import { KekkaiRuntimeError } from "./errors.js";
import { decodeWorkspaceDirectory } from "./protocol.js";
import { Transport, withSignal } from "./transport.js";
import type {
  WorkspaceApi,
  WorkspaceDirectory,
  WorkspaceOptions,
} from "./types.js";

export class WorkspaceClient implements WorkspaceApi {
  constructor(private readonly transport: Transport) {}

  async list(
    path = "",
    options: WorkspaceOptions = {},
  ): Promise<WorkspaceDirectory> {
    const body = await this.transport.json(
      "workspace.list",
      workspacePath(path, true),
      withSignal({ method: "GET" }, options.signal),
    );
    return decodeWorkspaceDirectory(body);
  }

  async read(path: string, options: WorkspaceOptions = {}): Promise<Uint8Array> {
    return this.transport.bytes(
      "workspace.read",
      workspacePath(path, false),
      withSignal({ method: "GET" }, options.signal),
    );
  }

  async readText(path: string, options: WorkspaceOptions = {}): Promise<string> {
    const bytes = await this.read(path, options);
    return new TextDecoder().decode(bytes);
  }

  async write(
    path: string,
    data: string | Uint8Array,
    options: WorkspaceOptions = {},
  ): Promise<void> {
    if (typeof data !== "string" && !(data instanceof Uint8Array)) {
      throw new KekkaiRuntimeError("workspace data must be a string or Uint8Array", {
        kind: "validation",
        operation: "workspace.write",
      });
    }
    const body = typeof data === "string" ? new TextEncoder().encode(data) : data;
    await this.transport.noContent(
      "workspace.write",
      workspacePath(path, false),
      withSignal({
        method: "PUT",
        headers: { "content-type": "application/octet-stream" },
        body: body as unknown as BodyInit,
      }, options.signal),
    );
  }

  async remove(path: string, options: WorkspaceOptions = {}): Promise<void> {
    await this.transport.noContent(
      "workspace.remove",
      workspacePath(path, false),
      withSignal({ method: "DELETE" }, options.signal),
    );
  }
}

function workspacePath(path: string, allowRoot: boolean): string {
  const segments = validateWorkspacePath(path, allowRoot);
  return segments.length === 0
    ? "v1/workspace"
    : `v1/workspace/${segments.map(encodeURIComponent).join("/")}`;
}

function validateWorkspacePath(path: string, allowRoot: boolean): string[] {
  if (typeof path !== "string") {
    throw new KekkaiRuntimeError("workspace path must be a string", {
      kind: "validation",
      operation: "workspace",
    });
  }
  if (path === "") {
    if (allowRoot) {
      return [];
    }
    throw new KekkaiRuntimeError("workspace root cannot be used for this operation", {
      kind: "validation",
      operation: "workspace",
    });
  }

  const segments = path.split("/");
  if (
    segments.some(
      (segment) => segment.length === 0 || segment === "." || segment === "..",
    )
  ) {
    throw new KekkaiRuntimeError(
      "workspace path must contain only normal relative components",
      {
        kind: "validation",
        operation: "workspace",
      },
    );
  }
  return segments;
}
