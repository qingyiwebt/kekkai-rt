import { strict as assert } from "node:assert";
import { test } from "node:test";
import {
  AgentCellClient,
  AgentCellHttpError,
  AgentCellProtocolError,
  AgentCellValidationError,
  type ExecEvent,
} from "../src/index.js";

interface FetchCall {
  url: string;
  init: RequestInit | undefined;
}

function jsonResponse(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function sseResponse(chunks: string[]): Response {
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(encoder.encode(chunk));
      }
      controller.close();
    },
  });
  return new Response(body, {
    headers: { "content-type": "text/event-stream" },
  });
}

function fakeFetch(
  handler: (url: string, init: RequestInit | undefined) => Promise<Response> | Response,
): { fetch: typeof fetch; calls: FetchCall[] } {
  const calls: FetchCall[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = typeof input === "string" ? input : input.toString();
    calls.push({ url, init });
    return handler(url, init);
  };
  return { fetch, calls };
}

test("creates an execution with auth and snake_case request fields", async () => {
  const { fetch, calls } = fakeFetch(() => jsonResponse({ task_id: "task-1" }, 202));
  const client = new AgentCellClient({
    baseUrl: "http://localhost:8080",
    token: "secret",
    fetch,
  });

  const task = await client.createExec({
    argv: ["/bin/echo", "hello"],
    cwd: "/workspace",
    env: { MODE: "test" },
    stdin: "input",
    timeoutSeconds: 15,
  });

  assert.equal(task.id, "task-1");
  assert.equal(calls[0]?.url, "http://localhost:8080/v1/exec");
  assert.equal(calls[0]?.init?.headers && new Headers(calls[0].init.headers).get("authorization"), "Bearer secret");
  assert.deepEqual(JSON.parse(String(calls[0]?.init?.body)), {
    argv: ["/bin/echo", "hello"],
    cwd: "/workspace",
    env: { MODE: "test" },
    stdin: "input",
    timeout_seconds: 15,
  });
});

test("waits for terminal SSE event and reads the authoritative snapshot", async () => {
  const events: ExecEvent[] = [];
  const { fetch } = fakeFetch((url) => {
    if (url.endsWith("/v1/exec")) {
      return jsonResponse({ task_id: "task-2" }, 202);
    }
    if (url.endsWith("/events")) {
      return sseResponse([
        ": keep-alive\n\n",
        "event: started\ndata: {}\n\n",
        "event: stdout\ndata: {\"data\":\"hello\\n\"}\n\n",
        "event: finished\ndata: {\"exit_code\":0}\n\n",
      ]);
    }
    return jsonResponse({
      task_id: "task-2",
      status: "finished",
      exit_code: 0,
      stdout: "hello\n",
      stderr: "",
      error: null,
    });
  });
  const client = new AgentCellClient({ baseUrl: "http://localhost:8080/", token: "secret", fetch });

  const result = await client.execute(
    { argv: ["/bin/echo", "hello"] },
    {
      onEvent: (event) => {
        events.push(event);
      },
    },
  );

  assert.deepEqual(events, [
    { type: "started" },
    { type: "stdout", data: "hello\n" },
    { type: "finished", exitCode: 0 },
  ]);
  assert.equal(result.taskId, "task-2");
  assert.equal(result.status, "finished");
  assert.equal(result.stdout, "hello\n");
});

test("reports HTTP and validation errors with typed errors", async () => {
  const { fetch } = fakeFetch(() => jsonResponse({ error: "not authorized" }, 401));
  const client = new AgentCellClient({ baseUrl: "http://localhost:8080", token: "bad", fetch });

  await assert.rejects(
    client.createExec({ argv: ["/bin/echo"] }),
    (error: unknown) =>
      error instanceof AgentCellHttpError &&
      error.status === 401 &&
      error.message === "not authorized",
  );
  await assert.rejects(
    client.createExec({ argv: [] }),
    (error: unknown) => error instanceof AgentCellValidationError,
  );
});

test("rejects malformed task snapshots", async () => {
  const { fetch } = fakeFetch(() => jsonResponse({ task_id: "task-3", status: "unknown" }));
  const client = new AgentCellClient({ baseUrl: "http://localhost:8080", token: "secret", fetch });

  await assert.rejects(
    client.getExec("task-3"),
    (error: unknown) => error instanceof AgentCellProtocolError,
  );
});

test("passes AbortSignal through to streaming requests", async () => {
  let receivedSignal: AbortSignal | null | undefined;
  const { fetch } = fakeFetch((_url, init) => {
    receivedSignal = init?.signal;
    return sseResponse([]);
  });
  const client = new AgentCellClient({ baseUrl: "http://localhost:8080", token: "secret", fetch });
  const controller = new AbortController();

  const events = client.events("task-4", { signal: controller.signal });
  for await (const _event of events) {
    // The test only verifies the signal passed to fetch.
  }

  assert.equal(receivedSignal, controller.signal);
});
