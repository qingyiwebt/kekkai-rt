import { strict as assert } from "node:assert";
import { test } from "node:test";
import {
  KekkaiRuntimeClient,
  KekkaiRuntimeError,
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

test("starts an execution with high-level fields and wire conversion", async () => {
  const { fetch, calls } = fakeFetch(() => jsonResponse({ task_id: "task-1" }, 202));
  const client = new KekkaiRuntimeClient({
    baseUrl: "http://localhost:8080",
    token: "secret",
    fetch,
  });

  const task = await client.exec.start({
    command: "/bin/echo",
    args: ["hello"],
    cwd: "/workspace",
    env: { MODE: "test" },
    input: "input",
    timeoutMs: 1_501,
  });

  assert.equal(task.id, "task-1");
  assert.equal(calls[0]?.url, "http://localhost:8080/v1/exec");
  assert.equal(
    calls[0]?.init?.headers && new Headers(calls[0].init.headers).get("authorization"),
    "Bearer secret",
  );
  assert.deepEqual(JSON.parse(String(calls[0]?.init?.body)), {
    argv: ["/bin/echo", "hello"],
    cwd: "/workspace",
    env: { MODE: "test" },
    stdin: "input",
    timeout_seconds: 2,
  });
});

test("runs a task from SSE to the authoritative terminal snapshot", async () => {
  const events: ExecEvent[] = [];
  const { fetch } = fakeFetch((url) => {
    if (url.endsWith("/v1/exec")) {
      return jsonResponse({ task_id: "task-2" }, 202);
    }
    if (url.endsWith("/events")) {
      return sseResponse([
        ": keep-alive\n\n",
        "event: started\ndata: {}\n\n",
        "event: stdout\ndata: {\"data\":\"hello",
        "\\n\"}\n\n",
        "event: stderr\ndata: {\"data\":\"warning\\n\"}\n\n",
        "event: finished\ndata: {\"exit_code\":0}\n\n",
      ]);
    }
    return jsonResponse({
      task_id: "task-2",
      status: "finished",
      exit_code: 0,
      stdout: "hello\n",
      stderr: "warning\n",
      error: null,
    });
  });
  const client = new KekkaiRuntimeClient({
    baseUrl: "http://localhost:8080/",
    token: "secret",
    fetch,
  });

  const result = await client.exec.run(
    { command: "/bin/echo", args: ["hello"] },
    {
      onEvent: (event) => {
        events.push(event);
      },
    },
  );

  assert.deepEqual(events, [
    { type: "started" },
    { type: "stdout", data: "hello\n" },
    { type: "stderr", data: "warning\n" },
    { type: "finished", exitCode: 0 },
  ]);
  assert.equal(result.taskId, "task-2");
  assert.equal(result.status, "finished");
  assert.equal(result.stdout, "hello\n");
  assert.equal(result.stderr, "warning\n");
});

test("maps timeout and failure events to terminal results", async () => {
  for (const [event, snapshot] of [
    [
      "event: timed_out\ndata: {}\n\n",
      {
        task_id: "timed-out",
        status: "timed_out",
        exit_code: null,
        stdout: "",
        stderr: "",
        error: null,
      },
    ],
    [
      "event: failed\ndata: {\"error\":\"could not start\"}\n\n",
      {
        task_id: "failed",
        status: "failed",
        exit_code: null,
        stdout: "",
        stderr: "",
        error: "could not start",
      },
    ],
  ] as const) {
    const { fetch } = fakeFetch((url) => {
      if (url.endsWith("/v1/exec")) {
        return jsonResponse({ task_id: snapshot.task_id }, 202);
      }
      if (url.endsWith("/events")) {
        return sseResponse([event]);
      }
      return jsonResponse(snapshot);
    });
    const client = new KekkaiRuntimeClient({ baseUrl: "http://localhost:8080", token: "secret", fetch });
    const result = await client.exec.run({ command: "/bin/false" });

    assert.equal(result.taskId, snapshot.task_id);
    assert.equal(result.status, snapshot.status === "timed_out" ? "timedOut" : "failed");
  }
});

test("exposes one typed error for validation, HTTP, protocol, and abort failures", async () => {
  const http = fakeFetch(() => jsonResponse({ error: "not authorized" }, 401));
  const httpClient = new KekkaiRuntimeClient({ baseUrl: "http://localhost:8080", token: "bad", fetch: http.fetch });

  await assert.rejects(
    httpClient.exec.start({ command: "/bin/echo" }),
    (error: unknown) =>
      error instanceof KekkaiRuntimeError &&
      error.kind === "http" &&
      error.status === 401 &&
      error.message === "not authorized",
  );

  await assert.rejects(
    httpClient.exec.start({ command: "" }),
    (error: unknown) => error instanceof KekkaiRuntimeError && error.kind === "validation",
  );

  const protocol = fakeFetch(() => jsonResponse({ task_id: "task-3", status: "unknown" }));
  const protocolClient = new KekkaiRuntimeClient({ baseUrl: "http://localhost:8080", token: "secret", fetch: protocol.fetch });
  const task = await protocolClient.exec.start({ command: "/bin/echo" });
  await assert.rejects(
    task.snapshot(),
    (error: unknown) => error instanceof KekkaiRuntimeError && error.kind === "protocol",
  );

  const controller = new AbortController();
  controller.abort();
  const aborted = fakeFetch(async (_url, init) => {
    assert.equal(init?.signal, controller.signal);
    throw Object.assign(new Error("aborted"), { name: "AbortError" });
  });
  const abortedClient = new KekkaiRuntimeClient({ baseUrl: "http://localhost:8080", token: "secret", fetch: aborted.fetch });
  await assert.rejects(
    abortedClient.exec.start({ command: "/bin/echo" }, { signal: controller.signal }),
    (error: unknown) => error instanceof KekkaiRuntimeError && error.kind === "aborted",
  );
});

test("passes AbortSignal through to task event streams", async () => {
  let receivedSignal: AbortSignal | null | undefined;
  const { fetch } = fakeFetch((url, init) => {
    receivedSignal = init?.signal;
    if (url.endsWith("/v1/exec")) {
      return jsonResponse({ task_id: "task-4" }, 202);
    }
    return sseResponse([]);
  });
  const client = new KekkaiRuntimeClient({ baseUrl: "http://localhost:8080", token: "secret", fetch });
  const controller = new AbortController();
  const task = await client.exec.start({ command: "/bin/echo" });

  for await (const _event of task.events({ signal: controller.signal })) {
    // The test only verifies the signal passed to fetch.
  }

  assert.equal(receivedSignal, controller.signal);
});

test("cancels an execution through the task endpoint", async () => {
  const { fetch, calls } = fakeFetch((url, init) => {
    if (url.endsWith("/v1/exec")) return jsonResponse({ task_id: "task-cancel" }, 202);
    assert.equal(init?.method, "DELETE");
    assert.ok(url.endsWith("/v1/exec/task-cancel"));
    return new Response(null, { status: 204 });
  });
  const client = new KekkaiRuntimeClient({ baseUrl: "http://localhost:8080", token: "secret", fetch });
  const task = await client.exec.start({ command: "/bin/sh" });
  await task.cancel();
  assert.equal(calls.length, 2);
});
