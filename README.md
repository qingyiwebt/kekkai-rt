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

All network fields are validated at startup. The bridge name must be a valid Linux interface name, the gateway and container address must belong to the configured subnet, and DNS entries must be IPv4 addresses. Relative paths are resolved from the directory containing `config.toml`.

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

The sysroot maintenance commands check or repair `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`, `/dev/mqueue`, and `/sys/fs/cgroup`. When `workspace_dir` is configured, they also check or repair the host workspace and the rootfs `/workspace` mountpoint. `fix` only creates missing directories and prepares the host workspace with mode `0777`; it does not install runtimes or modify bridge and iptables state. A normal startup refuses to continue when required sysroot paths are missing and suggests running `agent-cell fix`.

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
