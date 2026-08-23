# Kekkai Runtime HTTP API

本文档描述 Kekkai Runtime Rust/Axum 后端当前提供的 HTTP API。

默认服务地址为 `http://127.0.0.1:8080`，实际地址由配置文件中的
`[api].listen_addr` 决定。

## 通用约定

### 鉴权

除 `GET /healthz` 外，所有 `/v1/*` 接口都需要使用配置文件中的
`[api].token` 进行 Bearer Token 鉴权：

```http
Authorization: Bearer <api.token>
```

缺少或不匹配的 `Authorization` 请求头返回 `401 Unauthorized`。鉴权失败时响应体为空。

### 错误响应

需要返回错误详情的接口通常使用以下 JSON 格式：

```json
{
  "error": "错误信息"
}
```

### 路径参数

任务 ID 必须是 UUID。工作区路径必须是相对于工作区根目录的路径：

- 允许使用多个普通路径组件，例如 `src/main.rs`；
- 不允许空组件，例如 `a//b`；
- 不允许绝对路径、`.`、`..` 或其他非普通相对路径组件；
- URL 中的空格及其他保留字符必须进行 URL 编码。

## API 总览

| 方法 | 路径 | 说明 | 鉴权 |
| --- | --- | --- | --- |
| `GET` | `/healthz` | 健康检查 | 否 |
| `POST` | `/v1/exec` | 创建异步执行任务 | 是 |
| `GET` | `/v1/exec/{task_id}` | 获取任务快照 | 是 |
| `DELETE` | `/v1/exec/{task_id}` | 请求终止执行任务 | 是 |
| `GET` | `/v1/exec/{task_id}/events` | 订阅任务 SSE 事件 | 是 |
| `GET` | `/v1/workspace` | 获取工作区根目录 | 是 |
| `GET` | `/v1/workspace/{path}` | 获取目录或文件 | 是 |
| `PUT` | `/v1/workspace/{path}` | 创建或替换文件 | 是 |
| `DELETE` | `/v1/workspace` | 删除工作区根目录（始终拒绝） | 是 |
| `DELETE` | `/v1/workspace/{path}` | 删除文件或目录 | 是 |

工作区接口只有在配置了 `[sandbox].workspace_dir` 后才可用；未配置时返回 `404`。

## 健康检查

### `GET /healthz`

无需鉴权。服务正常运行时返回：

```http
HTTP/1.1 200 OK
Content-Type: application/json
```

```json
{
  "status": "ok"
}
```

示例：

```sh
curl http://127.0.0.1:8080/healthz
```

## 执行任务

### `POST /v1/exec`

提交一个在沙箱容器中运行的命令。请求会立即返回任务 ID，命令的实际执行在后台进行。

请求头：

```http
Authorization: Bearer <api.token>
Content-Type: application/json
```

请求体：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `argv` | `string[]` | 是 | 要执行的程序及参数。不能为空；不会自动经过 shell。 |
| `cwd` | `string` | 否 | 容器内的工作目录。 |
| `env` | `object<string, string>` | 否 | 要传递给进程的环境变量，默认为空对象。 |
| `stdin` | `string` | 否 | 写入进程标准输入的文本。 |
| `timeout_seconds` | `integer` | 否 | 本次任务超时时间，单位为秒。缺省为 `300`，并且不会超过配置中的 `sandbox.max_timeout_seconds`。 |

示例请求：

```json
{
  "argv": ["/bin/sh", "-c", "printf '%s\\n' \"$NAME\""],
  "cwd": "/workspace",
  "env": {
    "NAME": "Kekkai Runtime"
  },
  "stdin": "",
  "timeout_seconds": 30
}
```

成功返回 `202 Accepted`：

```json
{
  "task_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

当 `argv` 为空时返回 `400 Bad Request`：

```json
{
  "error": "argv must not be empty"
}
```

请求 JSON 无法解析时由 Axum 返回 `400 Bad Request`。任务启动阶段发生的错误不会导致创建接口返回错误；任务仍会返回 `202`，随后通过任务快照或 SSE 的 `failed` 事件报告错误。

示例：

```sh
curl \
  -H 'Authorization: Bearer replace-me' \
  -H 'Content-Type: application/json' \
  -d '{"argv":["/bin/echo","hello"]}' \
  http://127.0.0.1:8080/v1/exec
```

### `GET /v1/exec/{task_id}`

获取任务当前快照。任务快照在任务创建后最多保留约 5 分钟，任务完成后也会保留约 5 分钟；过期后返回 `404`。

成功返回 `200 OK`：

```json
{
  "task_id": "550e8400-e29b-41d4-a716-446655440000",
  "status": "finished",
  "exit_code": 0,
  "stdout": "hello\n",
  "stderr": "",
  "error": null
}
```

字段说明：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `task_id` | `string` | UUID 格式的任务 ID。 |
| `status` | `string` | `running`、`finished`、`timed_out` 或 `failed`。 |
| `exit_code` | `integer \| null` | 进程退出码。任务仍运行、超时或未能启动时通常为 `null`。 |
| `stdout` | `string` | 当前已收集的标准输出。 |
| `stderr` | `string` | 当前已收集的标准错误。 |
| `error` | `string \| null` | 任务失败时的错误信息，否则为 `null`。 |

任务不存在、已过期或任务 ID 不存在时返回 `404 Not Found`。无法解析的 UUID 会返回 `400 Bad Request`。

示例：

```sh
curl \
  -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/exec/550e8400-e29b-41d4-a716-446655440000
```

### `GET /v1/exec/{task_id}/events`

以 Server-Sent Events（SSE）形式订阅任务事件。连接建立后会先发送已经产生的历史事件，再发送后续实时事件；发送终止事件后服务端结束该 SSE 流。

响应类型为 `text/event-stream`。SSE keep-alive 注释不是任务事件，客户端应忽略以冒号开头的注释行。

每个任务事件的格式如下：

```text
event: <event-name>
data: <JSON object>

```

事件类型：

| 事件名 | `data` | 说明 |
| --- | --- | --- |
| `started` | `{}` | 任务开始执行。 |
| `stdout` | `{ "data": "..." }` | 收到一段标准输出。 |
| `stderr` | `{ "data": "..." }` | 收到一段标准错误。 |
| `finished` | `{ "exit_code": 0 }` | 进程正常结束；`exit_code` 可以为 `null`。 |
| `timed_out` | `{}` | 任务超过超时时间，进程已被终止。 |
| `failed` | `{ "error": "..." }` | 任务启动或等待过程中发生错误。 |

`finished`、`timed_out` 和 `failed` 是终止事件。任务执行期间，`stdout` 与 `stderr` 事件中的内容也会累积到任务快照中。

任务不存在或已过期时返回 `404 Not Found`。

示例：

```sh
curl -N \
  -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/exec/550e8400-e29b-41d4-a716-446655440000/events
```

可能收到的事件流：

```text
event: started
data: {}

event: stdout
data: {"data":"hello\\n"}

event: finished
data: {"exit_code":0}

```

## 工作区文件

工作区 API 操作的是配置项 `[sandbox].workspace_dir` 指定的宿主机目录。该目录同时会以 `/workspace` 的形式挂载到沙箱容器中（如果配置了工作区）。

所有工作区接口都需要 Bearer Token。路径中的每个组件应单独进行 URL 编码，例如：

```text
space file.bin -> /v1/workspace/space%20file.bin
```

### `GET /v1/workspace` 或 `GET /v1/workspace/{path}`

获取工作区根目录或指定路径。

当目标是目录时返回 `200 OK` 和 JSON：

```json
{
  "path": "src",
  "type": "directory",
  "entries": [
    {
      "name": "main.rs",
      "type": "file",
      "size": 128
    },
    {
      "name": "lib",
      "type": "directory",
      "size": 4096
    }
  ]
}
```

目录项按 `name` 升序排列。`entries[].type` 可能为 `file`、`directory`、`symlink` 或 `other`。

当目标是普通文件时返回 `200 OK`，响应体为原始文件字节，`Content-Type` 为 `application/octet-stream`。

目标不存在时返回 `404 Not Found`；目标既不是普通文件也不是目录时返回 `409 Conflict`；路径非法时返回 `400 Bad Request`。

示例：

```sh
# 列出根目录
curl \
  -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/workspace

# 读取二进制文件
curl \
  -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/workspace/src/main.rs
```

### `PUT /v1/workspace/{path}`

创建或替换一个文件。请求体按原始字节处理，不要求特定的 `Content-Type`。目标文件的父目录会自动创建；如果目标路径已经是目录，则返回 `409 Conflict`。

成功返回 `204 No Content`，响应体为空。

示例：

```sh
curl -X PUT \
  -H 'Authorization: Bearer replace-me' \
  --data-binary 'hello\n' \
  http://127.0.0.1:8080/v1/workspace/hello.txt
```

常见错误：

| 状态码 | 条件 |
| --- | --- |
| `400` | 路径非法或路径逃逸工作区根目录。 |
| `404` | 工作区未配置，或无法找到用于创建文件的父路径。 |
| `409` | 目标路径是目录，不能作为文件写入。 |
| `500` | 文件系统操作失败。 |

### `DELETE /v1/workspace/{path}`

删除指定文件或目录。删除目录时会递归删除其内容。

成功返回 `204 No Content`。目标不存在时返回 `404 Not Found`，路径非法时返回 `400 Bad Request`。

示例：

```sh
curl -X DELETE \
  -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/workspace/hello.txt
```

### `DELETE /v1/workspace`

工作区根目录不可删除。该请求始终返回 `400 Bad Request`：

```json
{
  "error": "workspace root cannot be deleted"
}
```

## 完整调用流程示例

```sh
BASE_URL=http://127.0.0.1:8080
TOKEN=replace-me

TASK_ID=$(curl -s \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"argv":["/bin/echo","hello"]}' \
  "$BASE_URL/v1/exec" | jq -r .task_id)

# 推荐通过 SSE 等待任务结束并实时读取输出
curl -N \
  -H "Authorization: Bearer $TOKEN" \
  "$BASE_URL/v1/exec/$TASK_ID/events"

# 任务结束后获取最终快照
curl \
  -H "Authorization: Bearer $TOKEN" \
  "$BASE_URL/v1/exec/$TASK_ID"
```
