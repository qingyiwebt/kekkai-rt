# @agent-cell/sdk

Modern Node.js SDK for the AgentCell sandbox service. It requires Node.js 20 or
newer and uses native `fetch`, Web Streams, and ESM.

## Install

```sh
pnpm add @agent-cell/sdk
```

## Execute a command

```ts
import { AgentCellClient } from "@agent-cell/sdk";

const client = new AgentCellClient({
  baseUrl: "http://127.0.0.1:8080",
  token: process.env.AGENTCELL_TOKEN!,
});

const result = await client.execute(
  {
    argv: ["/bin/sh", "-c", "printf 'hello\\n'"],
    cwd: "/workspace",
    timeoutSeconds: 30,
  },
  {
    onEvent(event) {
      if (event.type === "stdout") process.stdout.write(event.data);
    },
  },
);

console.log({ status: result.status, exitCode: result.exitCode });
```

For long-running commands, use the task handle directly:

```ts
const task = await client.createExec({ argv: ["/bin/echo", "hello"] });

for await (const event of task.events()) {
  console.log(event);
}

const snapshot = await task.snapshot();
```

`execute()` and `wait()` return terminal task states, including `failed` and
`timedOut`. Transport errors, malformed responses, and cancelled requests are
thrown as typed errors.

## Workspace files

```ts
const listing = await client.workspace.list("src");
const bytes = await client.workspace.readFile("src/index.ts");

await client.workspace.writeFile("output.bin", new Uint8Array([1, 2, 3]));
await client.workspace.writeFile("hello.txt", "hello");
await client.workspace.delete("output.bin");
```

File reads return `Uint8Array` so binary data is preserved. Workspace paths are
relative and are validated before the request is sent.

## Development

```sh
pnpm run check
pnpm test
pnpm run build
```
