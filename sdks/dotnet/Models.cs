using System;
using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace KekkaiRuntime;

/// <summary>Request submitted to the sandbox execution API.</summary>
public sealed class ExecRequest
{
    /// <summary>The executable and its arguments. The list must not be empty.</summary>
    [JsonPropertyName("argv")]
    public IReadOnlyList<string> Argv { get; set; } = Array.Empty<string>();

    /// <summary>Optional working directory inside the sandbox.</summary>
    [JsonPropertyName("cwd")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Cwd { get; set; }

    /// <summary>Optional environment variables passed to the process.</summary>
    [JsonPropertyName("env")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyDictionary<string, string>? Environment { get; set; }

    /// <summary>Optional text written to standard input.</summary>
    [JsonPropertyName("stdin")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Stdin { get; set; }

    /// <summary>Optional timeout in seconds.</summary>
    [JsonPropertyName("timeout_seconds")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public long? TimeoutSeconds { get; set; }
}

/// <summary>Current state of a Kekkai Runtime execution task.</summary>
[JsonConverter(typeof(ExecStatusJsonConverter))]
public enum ExecStatus
{
    Running,
    Finished,
    TimedOut,
    Failed,
}

/// <summary>Snapshot of an execution task.</summary>
public class ExecSnapshot
{
    [JsonInclude]
    [JsonPropertyName("task_id")]
    public Guid TaskId { get; internal set; }

    [JsonInclude]
    [JsonPropertyName("status")]
    public ExecStatus Status { get; internal set; }

    [JsonInclude]
    [JsonPropertyName("exit_code")]
    public int? ExitCode { get; internal set; }

    [JsonInclude]
    [JsonPropertyName("stdout")]
    public string Stdout { get; internal set; } = string.Empty;

    [JsonInclude]
    [JsonPropertyName("stderr")]
    public string Stderr { get; internal set; } = string.Empty;

    [JsonInclude]
    [JsonPropertyName("error")]
    public string? Error { get; internal set; }

    public bool IsTerminal => Status != ExecStatus.Running;
}

/// <summary>Final snapshot returned after an execution completes.</summary>
public sealed class ExecResult : ExecSnapshot
{
    internal ExecResult(ExecSnapshot snapshot)
    {
        TaskId = snapshot.TaskId;
        Status = snapshot.Status;
        ExitCode = snapshot.ExitCode;
        Stdout = snapshot.Stdout;
        Stderr = snapshot.Stderr;
        Error = snapshot.Error;
    }
}

public enum ExecEventType
{
    Started,
    Stdout,
    Stderr,
    Finished,
    TimedOut,
    Failed,
}

/// <summary>Base type for events received from the execution SSE stream.</summary>
public abstract class ExecEvent
{
    public abstract ExecEventType Type { get; }
    public bool IsTerminal => Type == ExecEventType.Finished ||
                              Type == ExecEventType.TimedOut ||
                              Type == ExecEventType.Failed;
}

public sealed class ExecStartedEvent : ExecEvent
{
    public override ExecEventType Type => ExecEventType.Started;
}

public sealed class ExecStdoutEvent : ExecEvent
{
    public ExecStdoutEvent(string data) => Data = data;
    public override ExecEventType Type => ExecEventType.Stdout;
    public string Data { get; }
}

public sealed class ExecStderrEvent : ExecEvent
{
    public ExecStderrEvent(string data) => Data = data;
    public override ExecEventType Type => ExecEventType.Stderr;
    public string Data { get; }
}

public sealed class ExecFinishedEvent : ExecEvent
{
    public ExecFinishedEvent(int? exitCode) => ExitCode = exitCode;
    public override ExecEventType Type => ExecEventType.Finished;
    public int? ExitCode { get; }
}

public sealed class ExecTimedOutEvent : ExecEvent
{
    public override ExecEventType Type => ExecEventType.TimedOut;
}

public sealed class ExecFailedEvent : ExecEvent
{
    public ExecFailedEvent(string error) => Error = error;
    public override ExecEventType Type => ExecEventType.Failed;
    public string Error { get; }
}

public sealed class HealthResponse
{
    [JsonInclude]
    [JsonPropertyName("status")]
    public string Status { get; internal set; } = string.Empty;
}

[JsonConverter(typeof(WorkspaceEntryTypeJsonConverter))]
public enum WorkspaceEntryType
{
    File,
    Directory,
    Symlink,
    Other,
}

public sealed class WorkspaceEntry
{
    [JsonInclude]
    [JsonPropertyName("name")]
    public string Name { get; internal set; } = string.Empty;

    [JsonInclude]
    [JsonPropertyName("type")]
    public WorkspaceEntryType Type { get; internal set; }

    [JsonInclude]
    [JsonPropertyName("size")]
    public long Size { get; internal set; }
}

public sealed class WorkspaceDirectory
{
    [JsonInclude]
    [JsonPropertyName("path")]
    public string Path { get; internal set; } = string.Empty;

    [JsonInclude]
    [JsonPropertyName("type")]
    public WorkspaceEntryType Type { get; internal set; }

    [JsonInclude]
    [JsonPropertyName("entries")]
    public IReadOnlyList<WorkspaceEntry> Entries { get; internal set; } = Array.Empty<WorkspaceEntry>();
}
