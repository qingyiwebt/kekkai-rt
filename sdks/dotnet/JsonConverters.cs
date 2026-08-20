using System;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AgentCell;

internal sealed class ExecStatusJsonConverter : JsonConverter<ExecStatus>
{
    public override ExecStatus Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var value = reader.GetString();
        return value switch
        {
            "running" => ExecStatus.Running,
            "finished" => ExecStatus.Finished,
            "timed_out" => ExecStatus.TimedOut,
            "failed" => ExecStatus.Failed,
            _ => throw new JsonException($"Unknown execution status '{value}'."),
        };
    }

    public override void Write(Utf8JsonWriter writer, ExecStatus value, JsonSerializerOptions options)
    {
        writer.WriteStringValue(value switch
        {
            ExecStatus.Running => "running",
            ExecStatus.Finished => "finished",
            ExecStatus.TimedOut => "timed_out",
            ExecStatus.Failed => "failed",
            _ => throw new JsonException($"Unknown execution status '{value}'."),
        });
    }
}

internal sealed class WorkspaceEntryTypeJsonConverter : JsonConverter<WorkspaceEntryType>
{
    public override WorkspaceEntryType Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var value = reader.GetString();
        return value switch
        {
            "file" => WorkspaceEntryType.File,
            "directory" => WorkspaceEntryType.Directory,
            "symlink" => WorkspaceEntryType.Symlink,
            "other" => WorkspaceEntryType.Other,
            _ => throw new JsonException($"Unknown workspace entry type '{value}'."),
        };
    }

    public override void Write(Utf8JsonWriter writer, WorkspaceEntryType value, JsonSerializerOptions options)
    {
        writer.WriteStringValue(value switch
        {
            WorkspaceEntryType.File => "file",
            WorkspaceEntryType.Directory => "directory",
            WorkspaceEntryType.Symlink => "symlink",
            WorkspaceEntryType.Other => "other",
            _ => throw new JsonException($"Unknown workspace entry type '{value}'."),
        });
    }
}
