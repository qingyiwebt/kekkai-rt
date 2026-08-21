# AgentCell

Rust/Axum service for executing commands inside a shared rootfs-backed container.

Copy `config.example.toml` to `config.toml`, set a non-empty Bearer token, and provide an existing Linux rootfs. AgentCell generates and owns the OCI bundle under `bundle/` next to `config.toml`; no external `config.json` is required. The selected `runsc` or `runc` executable must already be installed; startup fails with an explicit error when it is missing.

Example configuration:

```toml
[api]
listen_addr = "0.0.0.0:8080"
secret = "replace-me"

[sandbox]
rootfs_dir = "./alpine"
workspace_dir = "./workspace" # optional shared host directory
backend = "runc"
max_timeout_seconds = 300
network_mode = "nat" # nat, host, or none
network_bridge = "agentcell0"
network_subnet = "10.200.0.0/24"
network_gateway = "10.200.0.1"
network_ip = "10.200.0.2"
network_dns = ["1.1.1.1", "8.8.8.8"]
```

The sandbox network is configured in the `[sandbox]` section. `network_mode = "nat"` is the default and creates an isolated bridge/veth network with outbound NAT. `network_mode = "host"` shares the host network namespace, while `network_mode = "none"` keeps the container isolated from external networks. NAT mode requires root (or `CAP_NET_ADMIN`), `ip`, `nsenter`, and `iptables` on the host.

The container belongs to the AgentCell process. AgentCell starts `runc` or `runsc` in foreground mode, waits for accepted execution tasks during graceful shutdown, then kills and deletes the container and removes its session veth. The bridge and iptables rules are retained as shared, idempotently managed host resources. A stale container left by an unclean termination is removed at the next startup; Linux parent-death protection also makes the foreground runtime follow AgentCell when the process is forcefully terminated.

For NAT mode, `network_bridge`, `network_subnet`, `network_gateway`, `network_ip`, and `network_dns` control the managed bridge, container address, default route, and resolver configuration. AgentCell generates the complete OCI `config.json`, including the rootfs, standard system mounts, namespaces, and optional `/workspace` bind mount.

Changes made under the container root filesystem are persistent. For the `runsc` backend, AgentCell disables gVisor's default temporary rootfs overlay so writes go directly to `rootfs_dir` and remain available after the container is stopped and recreated. The `/proc`, `/sys`, `/dev`, `/dev/shm`, and `/sys/fs/cgroup` mounts remain runtime-managed filesystems; use `workspace_dir` for data that should be kept separately from the rootfs.

All network fields are validated at startup. The bridge name must be a valid Linux interface name, the gateway and container address must belong to the configured subnet, and DNS entries must be IPv4 addresses. Relative paths are resolved from the directory containing `config.toml`.

## Host-side tool proxy

Tools can be kept outside the sandbox while still being callable from a program inside it:

```toml
[tools.'something-cli']
path = "./something-cli"
env = "./for-something-cli.env"
```

The executable and env file paths are resolved relative to `config.toml`. The env file uses dotenv-style `KEY=VALUE` lines and is reread for every request, so changes take effect on the next invocation. The host process clears the child environment before adding values from the env file and request-specific overrides.

When at least one tool is configured, AgentCell maps these sockets into the container:

```text
/run/agentcell-tools.socket
/run/agentcell-tools-stdout.socket
/run/agentcell-tools-stderr.socket
/run/agentcell-tools-status.socket
```

The request socket accepts a length-prefixed binary header followed by streaming stdin. The submit connection is the tool's lifetime guard: if it closes before the tool exits, AgentCell terminates the tool process group. The stdout and stderr sockets return raw bytes for a request id, and the status socket returns the decimal exit code. A shell and Unix-capable `nc` are sufficient for a client; see [TOOLS-PROTOCOL.md](TOOLS-PROTOCOL.md) for the framing and an example.

The repository also includes a ready-to-use wrapper, [agentcell-tool-proxy](agentcell-tool-proxy). Copy or symlink it under the configured tool name and make it executable:

```sh
cp agentcell-tool-proxy something-cli
chmod +x something-cli
```

Running `something-cli arg1` then selects the `something-cli` entry, forwards the arguments and stdin, mirrors stdout/stderr, and exits with the host tool's status code.

Run with:

```sh
# Start the service using ./config.toml
agent-cell

# Enable detailed lifecycle and network diagnostics
RUST_LOG=agent_cell=debug agent-cell

# Check configuration, rootfs mountpoints, runtime, and NAT dependencies
agent-cell check

# Create missing rootfs mountpoints and workspace directories, then check again
agent-cell fix

# Use a different configuration file
agent-cell --config /etc/agent-cell/config.toml check

# Download and prepare an Alpine sysroot in ./sysroot
agent-cell init alpine

# Pin a specific Alpine stable release
agent-cell init alpine --version 3.24.1
```

The sysroot maintenance commands check or repair `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`, `/dev/mqueue`, `/run`, and `/sys/fs/cgroup`. When `workspace_dir` is configured, they also check or repair the host workspace and the rootfs `/workspace` mountpoint. `fix` only creates missing directories and prepares the host workspace with mode `0777`; it does not install runtimes or modify bridge and iptables state. A normal startup refuses to continue when required sysroot paths are missing and suggests running `agent-cell fix`.

`agent-cell init alpine` detects the current architecture, downloads the Alpine Mini root filesystem from the official `latest-stable` release directory, verifies its SHA256 checksum, and installs it atomically under `./sysroot`. It also creates `./workspace` and generates `config.toml` when that file does not exist. Existing `sysroot` directories and configuration files are never overwritten.

Create an execution:

```sh
curl -H 'Authorization: Bearer replace-me' -H 'content-type: application/json' \
  -d '{"argv":["/bin/echo","hello"]}' http://127.0.0.1:8080/v1/exec
```

Then consume `/v1/exec/{task_id}/events` as an SSE stream.

When `workspace_dir` is configured, workspace operations use the same Bearer token:

```sh
# List the workspace root
curl -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/workspace

# Create or replace a file (raw bytes are accepted)
curl -X PUT -H 'Authorization: Bearer replace-me' \
  --data-binary 'hello' \
  http://127.0.0.1:8080/v1/workspace/hello.txt

# Read and recursively delete a path
curl -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/workspace/hello.txt
curl -X DELETE -H 'Authorization: Bearer replace-me' \
  http://127.0.0.1:8080/v1/workspace/hello.txt
```
