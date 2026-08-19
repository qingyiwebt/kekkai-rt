import { strict as assert } from "node:assert";
import { test } from "node:test";
import { parseSse } from "../src/sse.js";

test("parses split SSE frames, comments, and multiline data", async () => {
  const encoder = new TextEncoder();
  const chunks = [
    ": ping\n\n",
    "event: stdout\ndata: first\n",
    "data: second\n\n",
    "event: finished\ndata: {}\n\n",
  ];
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });

  const result = [];
  for await (const event of parseSse(body)) result.push(event);

  assert.deepEqual(result, [
    { event: "stdout", data: "first\nsecond" },
    { event: "finished", data: "{}" },
  ]);
});
