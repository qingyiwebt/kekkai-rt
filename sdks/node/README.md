# @klrohias/kekkai-rt-sdk

[English](README.md) | [简体中文](README.zh-cn.md)

A modern Kekkai Runtime SDK for Node.js 20+. It uses native `fetch`, Web Streams, and ESM while hiding HTTP paths, `snake_case` fields, and SSE protocol details.

## Installation

```sh
pnpm add @klrohias/kekkai-rt-sdk
```

## Create a client

```ts
import { KekkaiRuntimeClient } from "@klrohias/kekkai-rt-sdk";

const client = new KekkaiRuntimeClient({
  baseUrl: "http://127.0.0.1:8080",
  token: process.env.TOKEN!,
});

await client.health();
```

## Execute a command

`command` and `args` are passed to the container process as separate arguments; they are not automatically run through a shell.

```ts
const result = await client.exec.run(
  {
    command: "/bin/sh",
    args: ["-c", "printf '%s\\n' \"$NAME\""],
    cwd: "/workspace",
    env: { NAME: "Kekkai Runtime" },
    input: "",
    timeoutMs: 30_000,
  },
  {
    onEvent(event) {
      if (event.type === "stdout") process.stdout.write(event.data);
      if (event.type === "stderr") process.stderr.write(event.data);
    },
  },
);

console.log({ status: result.status, exitCode: result.exitCode });
```

For long-running tasks, you can first obtain a task object and then consume events with an async iterator:

```ts
const task = await client.exec.start({
  command: "/bin/echo",
  args: ["hello"],
});

for await (const event of task.events()) {
  console.log(event);
}

const snapshot = await task.snapshot();
const result = await task.wait();

// Stop a running task on the server and wait for its terminal event.
await task.cancel();
```

Event types include `started`, `stdout`, `stderr`, `finished`, `timedOut`, and `failed`. `wait()` and `run()` return the terminal status, including timeouts and failures.

## Workspace files

Workspace paths must be ordinary paths relative to the Kekkai Runtime workspace root. Before sending a request, the SDK rejects `..`, empty path components, and disallowed root-directory operations.

```ts
const listing = await client.workspace.list("src");
const bytes = await client.workspace.read("src/index.ts");
const text = await client.workspace.readText("src/index.ts");

await client.workspace.write("output.bin", new Uint8Array([1, 2, 3]));
await client.workspace.write("hello.txt", "hello");
await client.workspace.remove("output.bin");
```

`read()` always returns a `Uint8Array` to preserve binary data. `readText()` decodes using UTF-8, and string writes are also encoded as UTF-8.

## Error handling

The SDK exposes only `KekkaiRuntimeError`:

```ts
import { KekkaiRuntimeError } from "@klrohias/kekkai-rt-sdk";

try {
  await client.exec.run({ command: "/bin/echo", args: ["hello"] });
} catch (error) {
  if (error instanceof KekkaiRuntimeError) {
    console.error(error.kind, error.operation, error.status, error.message);
  }
}
```

`error.kind` can be:

- `validation`: local argument validation failed;
- `http`: the server returned an HTTP error or the network request failed;
- `protocol`: the server response did not conform to the Kekkai Runtime protocol;
- `aborted`: the request was cancelled by an `AbortSignal`.

Cancelling a client-side SSE or HTTP request does not automatically terminate an execution task that has already been submitted to the server.

## Migrate from the legacy API

| Legacy API | New API |
| --- | --- |
| `client.createExec({ argv, stdin, timeoutSeconds })` | `client.exec.start({ command, args, input, timeoutMs })` |
| `client.execute(request, options)` | `client.exec.run(request, options)` |
| `client.getExec(taskId)` | `task.snapshot()` |
| `client.events(taskId)` | `task.events()` |
| — | `task.cancel()` / `client.exec.cancel(taskId)` |
| `client.workspace.readFile(path)` | `client.workspace.read(path)` |
| `client.workspace.writeFile(path, data)` | `client.workspace.write(path, data)` |
| `client.workspace.delete(path)` | `client.workspace.remove(path)` |
| `KekkaiRuntimeHttpError`、`KekkaiRuntimeProtocolError`、`KekkaiRuntimeValidationError` | `KekkaiRuntimeError` |

Protocol-oriented legacy methods and error subclasses are no longer exported from the package entry point.

## Development

```sh
CI=true pnpm check
CI=true pnpm test
CI=true pnpm build
```
