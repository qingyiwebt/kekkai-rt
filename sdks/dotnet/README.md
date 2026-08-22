# Kekkai Runtime .NET SDK

This directory contains a small, dependency-light C# client for the Kekkai Runtime HTTP API.
It targets `netstandard2.1`, which provides native async-stream support and works with
.NET Core 3.0+, .NET 5+, and modern .NET applications. `System.Text.Json` is included as
a NuGet dependency.

## Usage

```csharp
using KekkaiRuntime;

using var client = new KekkaiRuntimeClient(
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

- `KekkaiRuntimeClient.ExecuteAsync` submits a task and returns an `IAsyncEnumerable<ExecEvent>` for `await foreach` consumption.
- `KekkaiRuntimeClient.ExecuteAndWaitAsync` submits a task, consumes its SSE stream, and returns its final snapshot.
- `Workspace.ListAsync`, `ReadFileAsync`, `WriteFileAsync`, and `DeleteAsync` expose workspace CRUD.
- Cancellation tokens are supported by every network operation and by SSE enumeration.

For endpoint-level control, use `KekkaiRuntimeApiClient` directly:

```csharp
using var api = new KekkaiRuntimeApiClient(
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
dotnet build KekkaiRuntime.slnx -c Release
dotnet pack KekkaiRuntime.csproj -c Release
```

Run the protocol tests with:

```sh
dotnet run --project tests/KekkaiRuntime.ProtocolTests/KekkaiRuntime.ProtocolTests.csproj -c Release
```
