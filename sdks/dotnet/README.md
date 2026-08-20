# AgentCell .NET SDK

This directory contains a small, dependency-light C# client for the AgentCell HTTP API.
It targets `netstandard2.1`, which provides native async-stream support and works with
.NET Core 3.0+, .NET 5+, and modern .NET applications. `System.Text.Json` is included as
a NuGet dependency.

## Usage

```csharp
using AgentCell;

using var client = new AgentCellClient(
    new Uri("http://127.0.0.1:8080/"),
    "replace-me");

await foreach (var @event in client.ExecuteAsync(new ExecRequest
{
    Argv = new[] { "/bin/echo", "hello" },
}))
{
    if (@event is ExecStdoutEvent stdout) Console.Write(stdout.Data);
}

var result = await client.ExecuteAndWaitAsync(new ExecRequest
{
    Argv = new[] { "/bin/echo", "hello" },
});
Console.WriteLine(result.Status);
```

For applications that manage `HttpClient` themselves, use the overload accepting an
`HttpClient`. That overload does not dispose the supplied client.

## API shape

- `AgentCellClient.ExecuteAsync` submits a task and returns an `IAsyncEnumerable<ExecEvent>` for `await foreach` consumption.
- `AgentCellClient.ExecuteAndWaitAsync` submits a task, consumes its SSE stream, and returns its final snapshot.
- `Workspace.ListAsync`, `ReadFileAsync`, `WriteFileAsync`, and `DeleteAsync` expose workspace CRUD.
- Cancellation tokens are supported by every network operation and by SSE enumeration.

For endpoint-level control, use `AgentCellApiClient` directly:

```csharp
using var api = new AgentCellApiClient(
    new Uri("http://127.0.0.1:8080/"),
    "replace-me");

var taskId = await api.CreateExecAsync(new ExecRequest
{
    Argv = new[] { "/bin/echo", "hello" },
});

await foreach (var @event in api.EventsAsync(taskId))
{
    // Access the raw task-oriented HTTP API when needed.
}

var snapshot = await api.GetExecAsync(taskId);
```

Build the package from this directory with:

```sh
dotnet build AgentCell.slnx -c Release
dotnet pack AgentCell.csproj -c Release
```

Run the protocol tests with:

```sh
dotnet run --project tests/AgentCell.ProtocolTests/AgentCell.ProtocolTests.csproj -c Release
```
