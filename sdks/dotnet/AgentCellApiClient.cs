using System;
using System.Collections.Generic;
using System.IO;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace AgentCell;

/// <summary>Low-level client that maps directly to AgentCell HTTP API endpoints.</summary>
public sealed class AgentCellApiClient : IDisposable
{
    private static readonly JsonSerializerOptions JsonOptions = CreateJsonOptions();
    private readonly HttpClient _httpClient;
    private readonly Uri _baseUri;
    private readonly string _token;
    private readonly bool _disposeHttpClient;

    /// <summary>Creates an API client using an injected HttpClient. The HttpClient is not disposed.</summary>
    public AgentCellApiClient(HttpClient httpClient, Uri baseUri, string token)
    {
        _httpClient = httpClient ?? throw new ArgumentNullException(nameof(httpClient));
        _baseUri = NormalizeBaseUri(baseUri);
        _token = ValidateToken(token);
        _disposeHttpClient = false;
    }

    /// <summary>Creates an API client with an internally owned HttpClient.</summary>
    public AgentCellApiClient(Uri baseUri, string token)
        : this(new HttpClient(), baseUri, token)
    {
        _disposeHttpClient = true;
    }

    public async Task<HealthResponse> HealthAsync(CancellationToken cancellationToken = default)
    {
        var body = await SendJsonAsync(HttpMethod.Get, "healthz", null, "health", false, cancellationToken)
            .ConfigureAwait(false);
        try
        {
            var response = JsonSerializer.Deserialize<HealthResponse>(body, JsonOptions);
            if (response == null || response.Status != "ok")
            {
                throw new AgentCellProtocolException("Health response has an unexpected shape.", "health", body);
            }
            return response;
        }
        catch (JsonException)
        {
            throw new AgentCellProtocolException("Health response is not valid JSON.", "health", body);
        }
    }

    public async Task<Guid> CreateExecAsync(
        ExecRequest request,
        CancellationToken cancellationToken = default)
    {
        ValidateExecRequest(request);
        var json = JsonSerializer.Serialize(request, JsonOptions);
        using var content = new StringContent(json, Encoding.UTF8, "application/json");
        var body = await SendJsonAsync(
                HttpMethod.Post,
                "v1/exec",
                content,
                "createExec",
                true,
                cancellationToken)
            .ConfigureAwait(false);

        try
        {
            using var document = JsonDocument.Parse(body);
            if (!document.RootElement.TryGetProperty("task_id", out var idElement) ||
                idElement.ValueKind != JsonValueKind.String ||
                !Guid.TryParse(idElement.GetString(), out var taskId))
            {
                throw new AgentCellProtocolException("Execution response is missing task_id.", "createExec", body);
            }
            return taskId;
        }
        catch (JsonException)
        {
            throw new AgentCellProtocolException("Execution response is not valid JSON.", "createExec", body);
        }
    }

    public async Task<ExecSnapshot> GetExecAsync(Guid taskId, CancellationToken cancellationToken = default)
    {
        ValidateTaskId(taskId, "getExec");
        var body = await SendJsonAsync(
                HttpMethod.Get,
                $"v1/exec/{taskId:D}",
                null,
                "getExec",
                true,
                cancellationToken)
            .ConfigureAwait(false);
        try
        {
            var snapshot = JsonSerializer.Deserialize<ExecSnapshot>(body, JsonOptions);
            if (snapshot == null || snapshot.TaskId == Guid.Empty)
            {
                throw new AgentCellProtocolException("Execution snapshot has an unexpected shape.", "getExec", body);
            }
            return snapshot;
        }
        catch (JsonException)
        {
            throw new AgentCellProtocolException("Execution snapshot is not valid JSON.", "getExec", body);
        }
    }

    /// <summary>Streams task events until the server closes the SSE connection.</summary>
    public async IAsyncEnumerable<ExecEvent> EventsAsync(
        Guid taskId,
        [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        ValidateTaskId(taskId, "events");
        using var request = CreateRequest(HttpMethod.Get, $"v1/exec/{taskId:D}/events");
        using var response = await _httpClient.SendAsync(
                request,
                HttpCompletionOption.ResponseHeadersRead,
                cancellationToken)
            .ConfigureAwait(false);
        if (!response.IsSuccessStatusCode)
        {
            throw await CreateHttpExceptionAsync("events", response).ConfigureAwait(false);
        }

        using var stream = await response.Content.ReadAsStreamAsync().ConfigureAwait(false);
        await foreach (var rawEvent in ParseSseAsync(stream, cancellationToken).ConfigureAwait(false))
        {
            yield return DecodeEvent(rawEvent.Event, rawEvent.Data);
        }
    }

    public async Task<WorkspaceDirectory> ListWorkspaceAsync(
        string path = "",
        CancellationToken cancellationToken = default)
    {
        var body = await SendJsonAsync(
                HttpMethod.Get,
                WorkspacePath(path, true),
                null,
                "workspace.list",
                true,
                cancellationToken)
            .ConfigureAwait(false);
        try
        {
            var directory = JsonSerializer.Deserialize<WorkspaceDirectory>(body, JsonOptions);
            if (directory == null || directory.Type != WorkspaceEntryType.Directory)
            {
                throw new AgentCellProtocolException("Workspace response is not a directory.", "workspace.list", body);
            }
            return directory;
        }
        catch (JsonException)
        {
            throw new AgentCellProtocolException("Workspace response is not valid JSON.", "workspace.list", body);
        }
    }

    public async Task<byte[]> ReadWorkspaceFileAsync(
        string path,
        CancellationToken cancellationToken = default)
    {
        using var response = await SendAsync(
                HttpMethod.Get,
                WorkspacePath(path, false),
                null,
                "workspace.readFile",
                cancellationToken)
            .ConfigureAwait(false);
        return await response.Content.ReadAsByteArrayAsync().ConfigureAwait(false);
    }

    public async Task WriteWorkspaceFileAsync(
        string path,
        byte[] data,
        CancellationToken cancellationToken = default)
    {
        if (data == null) throw new ArgumentNullException(nameof(data));
        using var content = new ByteArrayContent(data);
        content.Headers.ContentType = new MediaTypeHeaderValue("application/octet-stream");
        using var response = await SendAsync(
                HttpMethod.Put,
                WorkspacePath(path, false),
                content,
                "workspace.writeFile",
                cancellationToken)
            .ConfigureAwait(false);
    }

    public async Task DeleteWorkspaceAsync(
        string path,
        CancellationToken cancellationToken = default)
    {
        using var response = await SendAsync(
                HttpMethod.Delete,
                WorkspacePath(path, false),
                null,
                "workspace.delete",
                cancellationToken)
            .ConfigureAwait(false);
    }

    private async Task<string> SendJsonAsync(
        HttpMethod method,
        string path,
        HttpContent? content,
        string operation,
        bool authenticated,
        CancellationToken cancellationToken)
    {
        using var response = await SendAsync(method, path, content, operation, cancellationToken, authenticated)
            .ConfigureAwait(false);
        return await response.Content.ReadAsStringAsync().ConfigureAwait(false);
    }

    private async Task<HttpResponseMessage> SendAsync(
        HttpMethod method,
        string path,
        HttpContent? content,
        string operation,
        CancellationToken cancellationToken,
        bool authenticated = true)
    {
        using var request = CreateRequest(method, path, authenticated);
        request.Content = content;
        var response = await _httpClient.SendAsync(request, cancellationToken).ConfigureAwait(false);
        if (!response.IsSuccessStatusCode)
        {
            var exception = await CreateHttpExceptionAsync(operation, response).ConfigureAwait(false);
            response.Dispose();
            throw exception;
        }
        return response;
    }

    private HttpRequestMessage CreateRequest(HttpMethod method, string path, bool authenticated = true)
    {
        var request = new HttpRequestMessage(method, new Uri(_baseUri, path));
        request.Headers.Accept.Add(new MediaTypeWithQualityHeaderValue("application/json"));
        if (authenticated)
        {
            request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", _token);
        }
        return request;
    }

    private static async Task<AgentCellHttpException> CreateHttpExceptionAsync(
        string operation,
        HttpResponseMessage response)
    {
        var body = response.Content == null ? null : await response.Content.ReadAsStringAsync().ConfigureAwait(false);
        return new AgentCellHttpException(operation, response.StatusCode, body);
    }

    private string WorkspacePath(string path, bool allowRoot)
    {
        var parts = ValidateWorkspacePath(path, allowRoot);
        return parts.Count == 0
            ? "v1/workspace"
            : "v1/workspace/" + string.Join("/", parts.ConvertAll(Uri.EscapeDataString));
    }

    private static List<string> ValidateWorkspacePath(string path, bool allowRoot)
    {
        if (path == null) throw new ArgumentNullException(nameof(path));
        if (path.Length == 0)
        {
            if (allowRoot) return new List<string>();
            throw new AgentCellValidationException("Workspace file path must not be empty.", "workspace");
        }
        var parts = path.Split('/');
        var result = new List<string>(parts.Length);
        foreach (var part in parts)
        {
            if (part.Length == 0 || part == "." || part == ".." || part.IndexOf('\\') >= 0)
            {
                throw new AgentCellValidationException(
                    "Workspace path must contain only normal relative components.",
                    "workspace");
            }
            result.Add(part);
        }
        return result;
    }

    private static void ValidateExecRequest(ExecRequest request)
    {
        if (request == null) throw new ArgumentNullException(nameof(request));
        if (request.Argv == null || request.Argv.Count == 0)
        {
            throw new AgentCellValidationException("argv must not be empty.", "createExec");
        }
        foreach (var argument in request.Argv)
        {
            if (argument == null) throw new AgentCellValidationException("argv must not contain null values.", "createExec");
        }
        if (request.TimeoutSeconds.HasValue && request.TimeoutSeconds.Value < 0)
        {
            throw new AgentCellValidationException("timeoutSeconds must be non-negative.", "createExec");
        }
    }

    private static void ValidateTaskId(Guid taskId, string operation)
    {
        if (taskId == Guid.Empty) throw new AgentCellValidationException("taskId must not be empty.", operation);
    }

    private static Uri NormalizeBaseUri(Uri baseUri)
    {
        if (baseUri == null) throw new ArgumentNullException(nameof(baseUri));
        if (!baseUri.IsAbsoluteUri || (baseUri.Scheme != Uri.UriSchemeHttp && baseUri.Scheme != Uri.UriSchemeHttps))
        {
            throw new ArgumentException("baseUri must be an absolute HTTP or HTTPS URI.", nameof(baseUri));
        }
        var value = baseUri.ToString();
        return new Uri(value.EndsWith("/", StringComparison.Ordinal) ? value : value + "/", UriKind.Absolute);
    }

    private static string ValidateToken(string token)
    {
        if (string.IsNullOrWhiteSpace(token)) throw new ArgumentException("token must not be empty.", nameof(token));
        return token;
    }

    private static JsonSerializerOptions CreateJsonOptions()
    {
        var options = new JsonSerializerOptions
        {
            PropertyNameCaseInsensitive = true,
        };
        options.Converters.Add(new ExecStatusJsonConverter());
        options.Converters.Add(new WorkspaceEntryTypeJsonConverter());
        return options;
    }

    private static ExecEvent DecodeEvent(string eventName, string data)
    {
        try
        {
            switch (eventName)
            {
                case "started": return new ExecStartedEvent();
                case "stdout": return new ExecStdoutEvent(ReadStringPayload(data, "stdout"));
                case "stderr": return new ExecStderrEvent(ReadStringPayload(data, "stderr"));
                case "finished": return new ExecFinishedEvent(ReadExitCode(data));
                case "timed_out": return new ExecTimedOutEvent();
                case "failed": return new ExecFailedEvent(ReadStringPayload(data, "failed", "error"));
                default: throw new AgentCellProtocolException($"Unknown SSE event '{eventName}'.", "events", data);
            }
        }
        catch (JsonException)
        {
            throw new AgentCellProtocolException($"SSE event '{eventName}' contains invalid JSON.", "events", data);
        }
    }

    private static string ReadStringPayload(string data, string eventName, string property = "data")
    {
        using var document = JsonDocument.Parse(data);
        if (!document.RootElement.TryGetProperty(property, out var value) || value.ValueKind != JsonValueKind.String)
        {
            throw new AgentCellProtocolException($"SSE event '{eventName}' is missing '{property}'.", "events", data);
        }
        return value.GetString() ?? string.Empty;
    }

    private static int? ReadExitCode(string data)
    {
        using var document = JsonDocument.Parse(data);
        if (!document.RootElement.TryGetProperty("exit_code", out var value) || value.ValueKind == JsonValueKind.Null)
        {
            return null;
        }
        return value.GetInt32();
    }

    private static async IAsyncEnumerable<RawSseEvent> ParseSseAsync(
        Stream stream,
        [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken cancellationToken)
    {
        using var reader = new StreamReader(stream, Encoding.UTF8, true, 4096, leaveOpen: true);
        var eventName = "message";
        var data = new List<string>();
        while (true)
        {
            var line = await ReadLineAsync(reader, cancellationToken).ConfigureAwait(false);
            if (line == null)
            {
                if (data.Count > 0) yield return new RawSseEvent(eventName, string.Join("\n", data));
                yield break;
            }
            if (line.Length == 0)
            {
                if (data.Count > 0)
                {
                    yield return new RawSseEvent(eventName, string.Join("\n", data));
                    eventName = "message";
                    data.Clear();
                }
                continue;
            }
            if (line[0] == ':') continue;
            var separator = line.IndexOf(':');
            var field = separator < 0 ? line : line.Substring(0, separator);
            var value = separator < 0 ? string.Empty : line.Substring(separator + 1).TrimStart(' ');
            if (field == "event") eventName = value;
            else if (field == "data") data.Add(value);
        }
    }

    private static async Task<string?> ReadLineAsync(StreamReader reader, CancellationToken cancellationToken)
    {
        var readTask = reader.ReadLineAsync();
        if (!cancellationToken.CanBeCanceled) return await readTask.ConfigureAwait(false);

        var cancellation = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
        using var registration = cancellationToken.Register(() => cancellation.TrySetResult(null));
        var completed = await Task.WhenAny(readTask, cancellation.Task).ConfigureAwait(false);
        if (completed != readTask) throw new OperationCanceledException(cancellationToken);
        return await readTask.ConfigureAwait(false);
    }

    public void Dispose()
    {
        if (_disposeHttpClient) _httpClient.Dispose();
    }

    private readonly struct RawSseEvent
    {
        public RawSseEvent(string @event, string data) { Event = @event; Data = data; }
        public string Event { get; }
        public string Data { get; }
    }
}
