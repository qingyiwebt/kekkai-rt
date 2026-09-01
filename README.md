# Kekkai Runtime

Rust/Axum service for executing commands inside a shared rootfs-backed container.

Copy `config.example.toml` to `config.toml`, set a non-empty Bearer token, and provide an existing Linux rootfs. Kekkai Runtime generates and owns the OCI bundle under `bundle/` next to `config.toml`; no external `config.json` is required. The selected `runsc` or `runc` executable must already be installed; startup fails with an explicit error when it is missing.

Example configuration:

```toml
[api]
listen_addr = "0.0.0.0:8080"
token = "replace-me"

[sandbox]
rootfs_dir = "./sysroot"
backend = "runc"
max_timeout_seconds = 300
network_mode = "nat" # nat, host, or none
network_bridge = "kekkai-rt0"
network_subnet = "10.200.0.0/24"
network_gateway = "10.200.0.1"
network_ip = "10.200.0.2"
network_dns = ["1.1.1.1", "8.8.8.8"]

[features]
cgroups = "auto" # auto, required, or disabled

[mounts]
"/workspace" = "./workspace" # optional host bind mount
```

The sandbox network is configured in the `[sandbox]` section. `network_mode = "nat"` is the default and creates an isolated bridge/veth network with outbound NAT. `network_mode = "host"` shares the host network namespace, while `network_mode = "none"` keeps the container isolated from external networks. NAT mode requires root (or `CAP_NET_ADMIN`) and Linux route/netfilter Netlink support. When using `runsc`, host and none modes are translated to the corresponding runtime network flags automatically.

The optional `[features]` section controls host-dependent runtime features. `cgroups = "auto"` is the default and uses cgroups when the host exposes the memory controller, otherwise it starts runsc with `--ignore-cgroups`. Use `required` to fail when the memory controller is unavailable, or `disabled` to always skip cgroup setup. `init` detects the current host capabilities when it creates a new configuration: it chooses NAT when its permissions and dependencies are available, otherwise host networking, and records the matching cgroup setting.

The container belongs to the Kekkai Runtime process. Kekkai Runtime starts `runc` or `runsc` in foreground mode, waits for accepted execution tasks during graceful shutdown, then kills and deletes the instance-specific container and removes its session veth. The bridge and AgentCell nftables rules are retained as shared, idempotently managed host resources. Each configuration directory gets its own container, network namespace, and veth names; multiple instances must still use non-conflicting configured network addresses.

For NAT mode, `network_bridge`, `network_subnet`, `network_gateway`, `network_ip`, and `network_dns` control the managed bridge, container address, default route, and resolver configuration. Kekkai Runtime generates the complete OCI `config.json`, including the rootfs, standard system mounts, namespaces, and all entries from `[mounts]`. Container mount paths must be absolute; host paths may be absolute or relative to `config.toml`.

Changes made under the container root filesystem are persistent. For the `runsc` backend, Kekkai Runtime disables gVisor's default temporary rootfs overlay so writes go directly to `rootfs_dir` and remain available after the container is stopped and recreated. The `/proc`, `/sys`, `/dev`, `/dev/shm`, and `/sys/fs/cgroup` mounts remain runtime-managed filesystems; use `[mounts]` for data that should be kept separately from the rootfs.

All network fields are validated at startup. The bridge name must be a valid Linux interface name, the gateway and container address must belong to the configured subnet, and DNS entries must be IPv4 addresses. Relative paths are resolved from the directory containing `config.toml`.

## Host-side tool proxy

Tools can be kept outside the sandbox while still being callable from a program inside it:

```toml
[tools.'something-cli']
path = "./something-cli"
env = "./for-something-cli.env"
```

The executable and optional env file paths are resolved relative to `config.toml`. The env file uses dotenv-style `KEY=VALUE` lines and is reread for every request, so changes take effect on the next invocation. If `env` is omitted, the tool process receives no injected environment variables. The host process clears the child environment before adding values from the env file.

When at least one tool is configured, Kekkai Runtime maps one Unix socket into the container:

```text
/run/kekkai-rt-tools.socket
```

The single connection carries a binary request, streaming stdin, stdout, stderr, and the final exit code. It is also the tool's lifetime guard: if it closes before the tool exits, Kekkai Runtime terminates the entire tool process group. See [tools_protocol.md](docs/tools_protocol.md) for the frame format.

The repository contains a libc-independent Go proxy under [sdks/proxy/](sdks/proxy/). Copy or symlink its Linux amd64 release binary under the configured tool name and make it executable:

```sh
cp kekkai-rt-tool-proxy-x86_64-unknown-linux-gnu something-cli
chmod +x something-cli
```

Running `something-cli arg1` then selects the `something-cli` entry, forwards all stdio, and exits with the host tool's status code.

Run with:

```sh
# Start the service using ./config.toml
kekkai-rt

# Enable detailed lifecycle and network diagnostics
RUST_LOG=kekkai_rt=debug kekkai-rt

# Check configuration, rootfs mountpoints, runtime, and NAT dependencies
kekkai-rt check

# Create missing rootfs mountpoints and workspace directories, then check again
kekkai-rt fix

# Use a different configuration file
kekkai-rt --config /etc/kekkai-rt/config.toml check

# Prepare ./sysroot from an OCI image archive or OCI image layout
kekkai-rt init ./image.tar

# Connect to the currently running sandbox
kekkai-rt shell
kekkai-rt --config /etc/kekkai-rt/config.toml shell --shell zsh
```

`kekkai-rt shell` connects to the already-running sandbox selected by the configuration. Without `--shell`, it runs `bash` when available and falls back to `sh`; `--shell zsh` selects a specific shell. The command does not start or stop the sandbox.

The sysroot maintenance commands check or repair `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`, `/dev/mqueue`, `/run`, `/sys/fs/cgroup`, and configured mount targets. Mount sources must already exist; `fix` does not create missing host sources. It does not install runtimes or modify bridge or nftables state. A normal startup refuses to continue when required sysroot paths are missing and suggests running `kekkai-rt fix`.

`kekkai-rt check` reports the effective cgroup policy and network capability status. An explicitly configured NAT network remains strict and fails with an actionable error when the host lacks the required permission or dependency; automatic NAT fallback is only used by `init` while generating a new configuration.

`kekkai-rt init <oci-image>` accepts an OCI image archive or OCI image layout, selects the manifest matching the current Linux architecture, verifies OCI blob digests, applies layers including whiteouts, and installs the result atomically under `./sysroot`. It also creates `./workspace` and generates `config.toml` with a `/workspace` mount when that file does not exist. Existing `sysroot` directories and configuration files are never overwritten.

Create an execution:

```sh
curl -H 'Authorization: Bearer replace-me' -H 'content-type: application/json' \
  -d '{"argv":["/bin/echo","hello"]}' http://127.0.0.1:8080/v1/exec
```

Then consume `/v1/exec/{task_id}/events` as an SSE stream.

To terminate a running execution, send an authenticated `DELETE` request to
`/v1/exec/{task_id}`. The task then emits its terminal `finished` event with a
null exit code after the child process has been killed.

When the exact `"/workspace"` mount is configured, workspace operations use the same Bearer token:

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
