using System;
using System.Net;

namespace KekkaiRuntime;

public class KekkaiRuntimeException : Exception
{
    internal KekkaiRuntimeException(string message, string operation, Exception? innerException = null)
        : base(message, innerException) => Operation = operation;

    public string Operation { get; }
}

public sealed class KekkaiRuntimeValidationException : KekkaiRuntimeException
{
    internal KekkaiRuntimeValidationException(string message, string operation)
        : base(message, operation) { }
}

public sealed class KekkaiRuntimeProtocolException : KekkaiRuntimeException
{
    internal KekkaiRuntimeProtocolException(string message, string operation, object? details = null)
        : base(message, operation) => Details = details;

    public object? Details { get; }
}

public sealed class KekkaiRuntimeHttpException : KekkaiRuntimeException
{
    internal KekkaiRuntimeHttpException(
        string operation,
        HttpStatusCode statusCode,
        string? responseBody)
        : base(CreateMessage(statusCode, responseBody), operation)
    {
        StatusCode = statusCode;
        ResponseBody = responseBody;
    }

    public HttpStatusCode StatusCode { get; }
    public string? ResponseBody { get; }

    private static string CreateMessage(HttpStatusCode statusCode, string? responseBody)
    {
        if (!string.IsNullOrWhiteSpace(responseBody))
        {
            const string marker = "\"error\":\"";
            var body = responseBody!;
            var start = body.IndexOf(marker, StringComparison.Ordinal);
            if (start >= 0)
            {
                start += marker.Length;
                var end = body.IndexOf('"', start);
                if (end > start)
                {
                    return body.Substring(start, end - start);
                }
            }
        }

        return $"Kekkai Runtime request failed with HTTP {(int)statusCode} ({statusCode}).";
    }
}
