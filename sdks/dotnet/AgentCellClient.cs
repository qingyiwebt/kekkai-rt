using System;
using System.Collections.Generic;
using System.Net.Http;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace AgentCell;

/// <summary>High-level AgentCell client for common execution and workspace workflows.</summary>
public sealed class AgentCellClient : IDisposable
{
    private readonly AgentCellApiClient _api;
    private readonly bool _disposeApi;

    /// <summary>Creates a high-level client using an injected HttpClient.</summary>
    public AgentCellClient(HttpClient httpClient, Uri baseUri, string token)
        : this(new AgentCellApiClient(httpClient, baseUri, token), true)
    {
    }

    /// <summary>Creates a high-level client with an internally owned HttpClient.</summary>
    public AgentCellClient(Uri baseUri, string token)
        : this(new AgentCellApiClient(baseUri, token), true)
    {
    }

    /// <summary>Creates a high-level client over an existing low-level API client.</summary>
    public AgentCellClient(AgentCellApiClient apiClient)
        : this(apiClient, false)
    {
    }

    private AgentCellClient(AgentCellApiClient apiClient, bool disposeApi)
    {
        _api = apiClient ?? throw new ArgumentNullException(nameof(apiClient));
        _disposeApi = disposeApi;
        Workspace = new WorkspaceClient(_api);
    }

    public WorkspaceClient Workspace { get; }

    /// <summary>
    /// Starts a sandbox execution and asynchronously yields events as they arrive.
    /// The stream ends after the server sends a terminal event.
    /// </summary>
    public async IAsyncEnumerable<ExecEvent> ExecuteAsync(
        ExecRequest request,
        [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var taskId = await _api.CreateExecAsync(request, cancellationToken).ConfigureAwait(false);
        await foreach (var @event in _api.EventsAsync(taskId, cancellationToken).ConfigureAwait(false))
        {
            yield return @event;
        }
    }

    /// <summary>Executes a command, waits for its terminal event, and returns the final snapshot.</summary>
    public async Task<ExecResult> ExecuteAndWaitAsync(
        ExecRequest request,
        CancellationToken cancellationToken = default)
    {
        var taskId = await _api.CreateExecAsync(request, cancellationToken).ConfigureAwait(false);
        var terminal = false;
        await foreach (var @event in _api.EventsAsync(taskId, cancellationToken).ConfigureAwait(false))
        {
            if (@event.IsTerminal)
            {
                terminal = true;
                break;
            }
        }

        if (!terminal)
        {
            throw new AgentCellProtocolException("SSE stream ended before a terminal event.", "executeAndWait");
        }

        var snapshot = await _api.GetExecAsync(taskId, cancellationToken).ConfigureAwait(false);
        if (!snapshot.IsTerminal)
        {
            throw new AgentCellProtocolException(
                "Task snapshot is not terminal after a terminal event.",
                "executeAndWait",
                snapshot);
        }
        return new ExecResult(snapshot);
    }

    public void Dispose()
    {
        if (_disposeApi) _api.Dispose();
    }
}

public sealed class WorkspaceClient
{
    private readonly AgentCellApiClient _api;

    internal WorkspaceClient(AgentCellApiClient api) => _api = api;

    public Task<WorkspaceDirectory> ListAsync(
        string path = "",
        CancellationToken cancellationToken = default) => _api.ListWorkspaceAsync(path, cancellationToken);

    public Task<byte[]> ReadFileAsync(
        string path,
        CancellationToken cancellationToken = default) => _api.ReadWorkspaceFileAsync(path, cancellationToken);

    public Task WriteFileAsync(
        string path,
        byte[] data,
        CancellationToken cancellationToken = default) => _api.WriteWorkspaceFileAsync(path, data, cancellationToken);

    public Task WriteFileAsync(
        string path,
        string text,
        CancellationToken cancellationToken = default)
    {
        if (text == null) throw new ArgumentNullException(nameof(text));
        return WriteFileAsync(path, Encoding.UTF8.GetBytes(text), cancellationToken);
    }

    public Task DeleteAsync(
        string path,
        CancellationToken cancellationToken = default) => _api.DeleteWorkspaceAsync(path, cancellationToken);
}
