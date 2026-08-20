import { AgentCellError } from "./errors.js";
import { ExecutionClient } from "./execution.js";
import { decodeHealth } from "./protocol.js";
import { Transport } from "./transport.js";
import { WorkspaceClient } from "./workspace.js";
import type {
  AgentCellClientOptions,
  ExecutionApi,
  HealthResponse,
  WorkspaceApi,
} from "./types.js";

export class AgentCellClient {
  readonly exec: ExecutionApi;
  readonly workspace: WorkspaceApi;
  private readonly transport: Transport;

  constructor(options: AgentCellClientOptions) {
    if (!options || typeof options !== "object") {
      throw new AgentCellError("options must be an object", {
        kind: "validation",
        operation: "constructor",
      });
    }
    this.transport = new Transport({
      baseUrl: options.baseUrl,
      token: options.token,
      ...(options.fetch === undefined ? {} : { fetcher: options.fetch }),
    });
    this.exec = new ExecutionClient(this.transport);
    this.workspace = new WorkspaceClient(this.transport);
  }

  async health(): Promise<HealthResponse> {
    const body = await this.transport.json("health", "healthz", {
      method: "GET",
    });
    return decodeHealth(body);
  }
}
