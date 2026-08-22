# @kekkai-rt/sdk

面向 Node.js 20+ 的现代 Kekkai Runtime SDK。SDK 使用原生 `fetch`、Web Streams 和 ESM，隐藏 HTTP 路径、snake_case 字段以及 SSE 协议细节。

## 安装

```sh
pnpm add @kekkai-rt/sdk
```

## 创建客户端

```ts
import { KekkaiRuntimeClient } from "@kekkai-rt/sdk";

const client = new KekkaiRuntimeClient({
  baseUrl: "http://127.0.0.1:8080",
  token: process.env.TOKEN!,
});

await client.health();
```

## 执行命令

`command` 和 `args` 会作为独立参数传递给容器进程，不会自动经过 shell。

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

对于长时间运行的任务，可以先获取任务对象，再使用异步迭代器消费事件：

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
```

事件类型包括 `started`、`stdout`、`stderr`、`finished`、`timedOut` 和 `failed`。`wait()` 和 `run()` 返回终止状态，包括超时和失败。

## 工作区文件

工作区路径必须是相对于 Kekkai Runtime 工作区根目录的普通路径。SDK 会在发送请求前拒绝 `..`、空路径组件以及不允许的根目录操作。

```ts
const listing = await client.workspace.list("src");
const bytes = await client.workspace.read("src/index.ts");
const text = await client.workspace.readText("src/index.ts");

await client.workspace.write("output.bin", new Uint8Array([1, 2, 3]));
await client.workspace.write("hello.txt", "hello");
await client.workspace.remove("output.bin");
```

`read()` 始终返回 `Uint8Array` 以保留二进制数据；`readText()` 使用 UTF-8 解码，字符串写入也使用 UTF-8 编码。

## 错误处理

SDK 对外只暴露 `KekkaiRuntimeError`：

```ts
import { KekkaiRuntimeError } from "@kekkai-rt/sdk";

try {
  await client.exec.run({ command: "/bin/echo", args: ["hello"] });
} catch (error) {
  if (error instanceof KekkaiRuntimeError) {
    console.error(error.kind, error.operation, error.status, error.message);
  }
}
```

`error.kind` 可能是：

- `validation`：本地参数校验失败；
- `http`：服务端返回 HTTP 错误或网络请求失败；
- `protocol`：服务端响应格式不符合 Kekkai Runtime 协议；
- `aborted`：请求被 `AbortSignal` 取消。

取消客户端的 SSE 或 HTTP 请求不会自动终止已经提交到服务端的执行任务。

## 从旧 API 迁移

| 旧 API | 新 API |
| --- | --- |
| `client.createExec({ argv, stdin, timeoutSeconds })` | `client.exec.start({ command, args, input, timeoutMs })` |
| `client.execute(request, options)` | `client.exec.run(request, options)` |
| `client.getExec(taskId)` | `task.snapshot()` |
| `client.events(taskId)` | `task.events()` |
| `client.workspace.readFile(path)` | `client.workspace.read(path)` |
| `client.workspace.writeFile(path, data)` | `client.workspace.write(path, data)` |
| `client.workspace.delete(path)` | `client.workspace.remove(path)` |
| `KekkaiRuntimeHttpError`、`KekkaiRuntimeProtocolError`、`KekkaiRuntimeValidationError` | `KekkaiRuntimeError` |

旧协议导向方法和错误子类不再从包入口导出。

## 开发

```sh
CI=true pnpm check
CI=true pnpm test
CI=true pnpm build
```
