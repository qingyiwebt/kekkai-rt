using System;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Net.Http;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using System.Linq;
using KekkaiRuntime;

await ProtocolTests.RunAsync();

static class ProtocolTests
{
    private static readonly Guid TaskId = Guid.Parse("550e8400-e29b-41d4-a716-446655440000");
    private const string Token = "test-token";
    private const string BaseUrl = "http://kekkai-rt.test/";

    public static async Task RunAsync()
    {
        await HighLevelExecutionUsesAsyncStreamAsync();
        await ExecuteAndWaitReturnsFinalSnapshotAsync();
        await WorkspaceUsesEncodedPathAndAuthAsync();
        await ProtocolErrorsAreReportedAsync();
        await CancellationStopsSseEnumerationAsync();
        Console.WriteLine("Kekkai Runtime protocol tests passed.");
    }

    private static async Task HighLevelExecutionUsesAsyncStreamAsync()
    {
        var requests = new List<HttpRequestMessage>();
        using var httpClient = new HttpClient(new ScriptedHandler(request =>
        {
            requests.Add(request);
            if (request.Method == HttpMethod.Post && request.RequestUri!.AbsolutePath == "/v1/exec")
            {
                return Json($"{{\"task_id\":\"{TaskId:D}\"}}");
            }
            if (request.Method == HttpMethod.Get && request.RequestUri!.AbsolutePath.EndsWith("/events", StringComparison.Ordinal))
            {
                return Sse(": keep-alive\n\n" +
                          "event: started\ndata: {}\n\n" +
                          "event: stdout\ndata: {\n" +
                          "data: \"data\":\"hello\\n\"}\n\n" +
                          "event: finished\ndata: {\"exit_code\":0}\n\n");
            }
            throw new InvalidOperationException($"Unexpected request: {request.Method} {request.RequestUri}");
        }));
        using var client = new KekkaiRuntimeClient(httpClient, new Uri(BaseUrl), Token);

        var events = new List<ExecEvent>();
        await foreach (var @event in client.ExecuteAsync(new ExecRequest
        {
            Argv = new[] { "/bin/echo", "hello" },
        }))
        {
            events.Add(@event);
        }

        Assert(events.Count == 3, "high-level stream should yield three events");
        Assert(events[0] is ExecStartedEvent, "first event should be started");
        Assert(events[1] is ExecStdoutEvent stdout && stdout.Data == "hello\n", "stdout event should decode split data fields");
        Assert(events[2] is ExecFinishedEvent finished && finished.ExitCode == 0, "finished event should decode exit code");
        Assert(requests[0].Headers.Authorization?.Scheme == "Bearer", "execution request should use bearer auth");
        Assert(requests[0].Headers.Authorization?.Parameter == Token, "execution request should use configured token");
    }

    private static async Task ExecuteAndWaitReturnsFinalSnapshotAsync()
    {
        var getSnapshot = false;
        using var httpClient = new HttpClient(new ScriptedHandler(request =>
        {
            if (request.Method == HttpMethod.Post) return Json($"{{\"task_id\":\"{TaskId:D}\"}}");
            if (request.RequestUri!.AbsolutePath.EndsWith("/events", StringComparison.Ordinal))
            {
                return Sse("event: finished\ndata: {\"exit_code\":0}\n\n");
            }
            if (request.Method == HttpMethod.Get && request.RequestUri!.AbsolutePath == $"/v1/exec/{TaskId:D}")
            {
                getSnapshot = true;
                return Json($"{{\"task_id\":\"{TaskId:D}\",\"status\":\"finished\",\"exit_code\":0,\"stdout\":\"hello\\n\",\"stderr\":\"\",\"error\":null}}");
            }
            throw new InvalidOperationException("Unexpected request in ExecuteAndWait test.");
        }));
        using var client = new KekkaiRuntimeClient(httpClient, new Uri(BaseUrl), Token);

        var result = await client.ExecuteAndWaitAsync(new ExecRequest { Argv = new[] { "/bin/echo", "hello" } });

        Assert(getSnapshot, "ExecuteAndWait should fetch the final snapshot");
        Assert(result.Status == ExecStatus.Finished, "result status should be finished");
        Assert(result.Stdout == "hello\n", "result should contain final stdout");
    }

    private static async Task WorkspaceUsesEncodedPathAndAuthAsync()
    {
        var sawEncodedPath = false;
        using var httpClient = new HttpClient(new ScriptedHandler(request =>
        {
            Assert(request.Headers.Authorization?.Parameter == Token, "workspace request should use bearer auth");
            if (request.Method == HttpMethod.Get && request.RequestUri!.OriginalString.Contains("space%20file.bin", StringComparison.Ordinal))
            {
                sawEncodedPath = true;
                return new HttpResponseMessage(HttpStatusCode.OK)
                {
                    Content = new ByteArrayContent(new byte[] { 0, 255, 2 }),
                };
            }
            if (request.Method == HttpMethod.Get && request.RequestUri!.AbsolutePath == "/v1/workspace")
            {
                return Json("{\"path\":\"\",\"type\":\"directory\",\"entries\":[{\"name\":\"a.txt\",\"type\":\"file\",\"size\":5}]}");
            }
            throw new InvalidOperationException("Unexpected workspace request.");
        }));
        using var api = new KekkaiRuntimeApiClient(httpClient, new Uri(BaseUrl), Token);

        var directory = await api.ListWorkspaceAsync();
        var bytes = await api.ReadWorkspaceFileAsync("space file.bin");

        Assert(directory.Entries.Count == 1 && directory.Entries[0].Name == "a.txt", "directory JSON should decode");
        Assert(bytes.SequenceEqual(new byte[] { 0, 255, 2 }), "workspace file bytes should remain unchanged");
        Assert(sawEncodedPath, "workspace path components should be URL encoded");
    }

    private static async Task ProtocolErrorsAreReportedAsync()
    {
        using var httpClient = new HttpClient(new ScriptedHandler(request =>
        {
            if (request.RequestUri!.AbsolutePath.EndsWith("/events", StringComparison.Ordinal))
            {
                return Sse("event: unknown\ndata: {}\n\n");
            }
            return new HttpResponseMessage(HttpStatusCode.BadRequest)
            {
                Content = JsonContent("{\"error\":\"bad request\"}"),
            };
        }));
        using var api = new KekkaiRuntimeApiClient(httpClient, new Uri(BaseUrl), Token);

        await AssertThrowsAsync<KekkaiRuntimeProtocolException>(async () =>
        {
            await foreach (var _ in api.EventsAsync(TaskId)) { }
        }, "unknown SSE event should be a protocol error");

        await AssertThrowsAsync<KekkaiRuntimeHttpException>(async () =>
        {
            await api.GetExecAsync(TaskId);
        }, "HTTP errors should be mapped to KekkaiRuntimeHttpException");
    }

    private static async Task CancellationStopsSseEnumerationAsync()
    {
        using var httpClient = new HttpClient(new ScriptedHandler(_ => new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new BlockingContent(),
        }));
        using var api = new KekkaiRuntimeApiClient(httpClient, new Uri(BaseUrl), Token);
        using var cancellation = new CancellationTokenSource(TimeSpan.FromMilliseconds(100));

        await AssertThrowsAsync<OperationCanceledException>(async () =>
        {
            await foreach (var _ in api.EventsAsync(TaskId, cancellation.Token)) { }
        }, "cancellation should interrupt an open SSE stream");
    }

    private static HttpResponseMessage Json(string content) => new(HttpStatusCode.OK)
    {
        Content = JsonContent(content),
    };

    private static HttpContent JsonContent(string content) => new StringContent(content, Encoding.UTF8, "application/json");

    private static HttpResponseMessage Sse(string content) => new(HttpStatusCode.OK)
    {
        Content = new StringContent(content, Encoding.UTF8, "text/event-stream"),
    };

    private static void Assert(bool condition, string message)
    {
        if (!condition) throw new InvalidOperationException(message);
    }

    private static async Task AssertThrowsAsync<T>(Func<Task> action, string message)
        where T : Exception
    {
        try
        {
            await action();
        }
        catch (T)
        {
            return;
        }
        throw new InvalidOperationException(message);
    }

    private sealed class ScriptedHandler : HttpMessageHandler
    {
        private readonly Func<HttpRequestMessage, HttpResponseMessage> _handler;

        public ScriptedHandler(Func<HttpRequestMessage, HttpResponseMessage> handler) => _handler = handler;

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
            => Task.FromResult(_handler(request));
    }

    private sealed class BlockingContent : HttpContent
    {
        protected override Task SerializeToStreamAsync(Stream stream, TransportContext? context)
            => throw new NotSupportedException();

        protected override bool TryComputeLength(out long length)
        {
            length = 0;
            return false;
        }

        protected override Task<Stream> CreateContentReadStreamAsync()
            => Task.FromResult<Stream>(new NeverEndingStream());
    }

    private sealed class NeverEndingStream : MemoryStream
    {
        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken)
            => new TaskCompletionSource<int>().Task;

        public override ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default)
            => new(new TaskCompletionSource<int>().Task);
    }
}
