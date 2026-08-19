export interface RawSseEvent {
  event: string;
  data: string;
}

/** Parse an SSE response body without buffering the stream. */
export async function* parseSse(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<RawSseEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let event = "message";
  let data: string[] = [];

  const dispatch = (): RawSseEvent | undefined => {
    if (data.length === 0) {
      event = "message";
      return undefined;
    }
    const result = { event, data: data.join("\n") };
    event = "message";
    data = [];
    return result;
  };

  const processLine = (line: string): RawSseEvent | undefined => {
    if (line === "") {
      return dispatch();
    }
    if (line.startsWith(":")) {
      return undefined;
    }

    const separator = line.indexOf(":");
    const field = separator === -1 ? line : line.slice(0, separator);
    let value = separator === -1 ? "" : line.slice(separator + 1);
    if (value.startsWith(" ")) {
      value = value.slice(1);
    }

    switch (field) {
      case "event":
        event = value;
        break;
      case "data":
        data.push(value);
        break;
      default:
        break;
    }
    return undefined;
  };

  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split(/\r\n|\r|\n/);
      buffer = lines.pop() ?? "";
      for (const line of lines) {
        const result = processLine(line);
        if (result) {
          yield result;
        }
      }
    }

    buffer += decoder.decode();
    if (buffer.length > 0) {
      const result = processLine(buffer);
      if (result) {
        yield result;
      }
    }
    const result = dispatch();
    if (result) {
      yield result;
    }
  } finally {
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}
