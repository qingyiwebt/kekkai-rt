# Kekkai Runtime tool proxy protocol

The proxy is enabled when the top-level `tools` table contains at least one entry. The configured key is selected by the basename of the executable inside the container:

```toml
[tools.'something-cli']
path = "./something-cli"
env = "./for-something-cli.env"
```

The `env` field is optional. When present, the env file is reread immediately before each tool process starts and is never copied into the container. When omitted, no environment variables are injected into the tool process.

## Socket

When tools are configured, Kekkai Runtime exposes one Unix socket inside the container:

```text
/run/kekkai-rt-tools.socket
```

The socket uses a binary full-duplex protocol. Every frame is:

```text
u8     frame type
u32    payload length, big-endian
bytes  payload
```

The maximum frame and field size is 1 MiB. A connection contains exactly one tool invocation and does not use a request id.

## Client frames

The first frame must be `OPEN` (`0x01`). Its payload is:

```text
field(command/config key)
u32(argument count)
field(argument 1) ... field(argument N)
```

Each `field` is a big-endian u32 length followed by that many bytes. Command names are UTF-8; arguments are byte strings.

The client then sends any number of:

```text
STDIN     0x02     raw stdin bytes
STDIN_EOF 0x03     empty payload; closes the tool's stdin normally
```

The client must keep the connection open after `STDIN_EOF` while it receives the result. If the connection closes before the tool finishes, Kekkai Runtime kills the entire tool process group.

## Server frames

The server sends output as soon as it is available:

```text
STDOUT 0x10     raw stdout bytes
STDERR 0x11     raw stderr bytes
EXIT   0x12     signed i32 exit code, big-endian
ERROR  0x13     UTF-8 diagnostic message
```

An `ERROR` frame is followed by an `EXIT` frame with code `127` when the request cannot be started or is invalid. A normal invocation ends with one `EXIT` frame and then the server closes the connection. A process terminated by signal uses `128 + signal`.

## Go proxy

The repository's `sdks/proxy/` directory contains a libc-independent Go client. Build or download the Linux amd64 binary, then copy or symlink it under every configured tool name:

```sh
cp kekkai-rt-tool-proxy-x86_64-unknown-linux-gnu something-cli
chmod +x something-cli
./something-cli arg1 arg2 < input.dat
```

The Go proxy has no dependency on shell utilities, `nc`, or the container's libc. It forwards stdin, stdout, stderr, and the host tool's exit code over the single socket. A local connection or protocol failure exits with code `125`.
