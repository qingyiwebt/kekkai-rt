# AgentCell tool proxy protocol

The proxy is enabled when the top-level `tools` table contains at least one entry. The configured key is the only command name that a container client may select:

```toml
[tools.'something-cli']
path = "./something-cli"
env = "./for-something-cli.env"
```

The env file is reread immediately before each tool process starts. It is never copied into the container.

## Socket paths

The sockets are available inside the container at:

```text
/run/agentcell-tools.socket
/run/agentcell-tools-stdout.socket
/run/agentcell-tools-stderr.socket
/run/agentcell-tools-status.socket
```

Each integer is an unsigned 32-bit big-endian value. Each field is encoded as its length followed by that many bytes. Request ids and command names are UTF-8. Arguments and environment values are byte strings on Unix.

## Submit and stdin

The client connects to `agentcell-tools.socket` and sends:

```text
field(request id)
u32(arg count)
u32(env count)
field(command/config key)
field(arg 1) ... field(arg N)
field(env key 1) field(env value 1) ... field(env key M) field(env value M)
stdin bytes ...
```

The daemon starts the configured executable after the header is parsed. All remaining bytes are copied to its stdin. The client half-closes the socket write side, or closes it after sending all input, to send stdin EOF. The submit socket does not send an acknowledgement.

The executable runs with an empty inherited environment. Values from the configured env file are added first; request env values are added afterward and therefore override matching keys.

## Output and status

For stdout or stderr, connect to the corresponding socket and send `field(request id)`. The daemon then returns raw bytes for that stream and closes the connection after the process exits and the stream is drained. Only one active reader is supported for each stream.

For status, connect to `agentcell-tools-status.socket` and send `field(request id)`. The daemon waits for completion and returns the decimal exit code followed by `\n`. A process terminated by signal uses `128 + signal`. Unknown request ids and duplicate stream readers return an `ERR ...` line.

## Shell + nc example

For normal command-line use, copy the repository's [`agentcell-tool-proxy`](agentcell-tool-proxy) under the configured command name:

```sh
cp agentcell-tool-proxy something-cli
chmod +x something-cli
./something-cli arg1 arg2 < input.dat
```

The wrapper derives `command` from its basename, so the renamed file must match a `[tools.'name']` key. It forwards the process arguments and stdin, mirrors both output streams, and exits with the status returned by the daemon.

The following illustrates the framing helpers. `nc` must support Unix sockets with `-U`; the example uses text arguments and no request environment values.

```sh
#!/bin/sh

id=${1:-request-1}
command=${2:-something-cli}

be32() {
    n=$1
    printf '%b' "\\$(printf '%03o' $(( (n >> 24) & 255 )))"
    printf '%b' "\\$(printf '%03o' $(( (n >> 16) & 255 )))"
    printf '%b' "\\$(printf '%03o' $(( (n >> 8) & 255 )))"
    printf '%b' "\\$(printf '%03o' $(( n & 255 )))"
}

field() {
    value=$1
    be32 "${#value}"
    printf '%s' "$value"
}

send_request() {
    {
        field "$id"
        be32 0                 # argument count
        be32 0                 # environment count
        field "$command"
        cat                    # stdin; EOF closes the write side
    } | nc -U /run/agentcell-tools.socket &
    submit_pid=$!
}

send_request <<'EOF'
hello from the sandbox
EOF

field "$id" | nc -U /run/agentcell-tools-stdout.socket &
stdout_pid=$!
field "$id" | nc -U /run/agentcell-tools-stderr.socket &
stderr_pid=$!

wait "$submit_pid"
wait "$stdout_pid"
wait "$stderr_pid"
exit_code=$(field "$id" | nc -U /run/agentcell-tools-status.socket)
printf 'tool exit code: %s' "$exit_code"
```

For a production proxy, start the stdout/stderr readers before or immediately after submitting the request so that output can be consumed while the tool is running.
