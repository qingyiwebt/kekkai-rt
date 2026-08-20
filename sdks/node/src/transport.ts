import { AgentCellError } from "./errors.js";

export interface TransportOptions {
  baseUrl: string | URL;
  token: string;
  fetcher?: typeof globalThis.fetch;
}

export class Transport {
  private readonly baseUrl: URL;
  private readonly token: string;
  private readonly fetcher: typeof globalThis.fetch;

  constructor(options: TransportOptions) {
    this.baseUrl = normalizeBaseUrl(options.baseUrl);
    if (typeof options.token !== "string") {
      throw new AgentCellError("token must be a string", {
        kind: "validation",
        operation: "constructor",
      });
    }
    this.token = options.token;
    this.fetcher = options.fetcher ?? globalThis.fetch.bind(globalThis);
  }

  async json(
    operation: string,
    path: string,
    init: RequestInit,
  ): Promise<unknown> {
    const response = await this.request(operation, path, init);
    return readResponseBody(response);
  }

  async bytes(
    operation: string,
    path: string,
    init: RequestInit,
  ): Promise<Uint8Array> {
    const response = await this.request(operation, path, init);
    return new Uint8Array(await response.arrayBuffer());
  }

  async noContent(
    operation: string,
    path: string,
    init: RequestInit,
  ): Promise<void> {
    const response = await this.request(operation, path, init);
    await response.body?.cancel();
  }

  async stream(
    operation: string,
    path: string,
    signal?: AbortSignal,
  ): Promise<ReadableStream<Uint8Array>> {
    const response = await this.request(operation, path, withSignal({
      method: "GET",
      headers: { accept: "text/event-stream" },
    }, signal));
    if (!response.body) {
      throw new AgentCellError("SSE response has no body", {
        kind: "protocol",
        operation,
      });
    }
    return response.body;
  }

  private async request(
    operation: string,
    path: string,
    init: RequestInit,
  ): Promise<Response> {
    let response: Response;
    try {
      response = await this.fetcher(this.url(path), {
        ...init,
        headers: {
          accept: "application/json",
          authorization: `Bearer ${this.token}`,
          ...init.headers,
        },
      });
    } catch (error) {
      if (isAbortError(error, init.signal)) {
        throw new AgentCellError("request was aborted", {
          kind: "aborted",
          operation,
          cause: error,
        });
      }
      throw new AgentCellError("request failed", {
        kind: "http",
        operation,
        cause: error,
      });
    }

    if (!response.ok) {
      const details = await readResponseBody(response);
      throw new AgentCellError(messageFromResponse(response.status, details), {
        kind: "http",
        operation,
        status: response.status,
        details,
      });
    }
    return response;
  }

  private url(path: string): string {
    return new URL(path, this.baseUrl).toString();
  }
}

export function withSignal(init: RequestInit, signal?: AbortSignal): RequestInit {
  return signal === undefined ? init : { ...init, signal };
}

export async function readResponseBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (text.length === 0) {
    return undefined;
  }
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

function normalizeBaseUrl(input: string | URL): URL {
  let url: URL;
  try {
    url = new URL(input);
  } catch (error) {
    throw new AgentCellError("baseUrl must be a valid absolute URL", {
      kind: "validation",
      operation: "constructor",
      cause: error,
    });
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new AgentCellError("baseUrl must use http or https", {
      kind: "validation",
      operation: "constructor",
    });
  }
  if (!url.pathname.endsWith("/")) {
    url.pathname += "/";
  }
  return url;
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

function isAbortError(error: unknown, signal: AbortSignal | null | undefined): boolean {
  return signal?.aborted === true || (isRecord(error) && error.name === "AbortError");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
