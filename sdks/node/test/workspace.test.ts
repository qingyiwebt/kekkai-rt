import { strict as assert } from "node:assert";
import { test } from "node:test";
import { AgentCellClient, AgentCellValidationError } from "../src/index.js";

test("lists workspace entries and preserves binary file data", async () => {
  const calls: { url: string; init: RequestInit | undefined }[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = input.toString();
    calls.push({ url, init });
    if (url.endsWith("/v1/workspace/src")) {
      return new Response(
        JSON.stringify({
          path: "src",
          type: "directory",
          entries: [{ name: "index.ts", type: "file", size: 3 }],
        }),
        { headers: { "content-type": "application/json" } },
      );
    }
    if (url.endsWith("/v1/workspace/space%20file.bin")) {
      return new Response(new Uint8Array([0, 255, 2]), {
        headers: { "content-type": "application/octet-stream" },
      });
    }
    return new Response(null, { status: 204 });
  };
  const client = new AgentCellClient({ baseUrl: "http://localhost:8080", token: "secret", fetch });

  const listing = await client.workspace.list("src");
  const bytes = await client.workspace.readFile("space file.bin");
  await client.workspace.writeFile("output.bin", bytes);
  await client.workspace.delete("output.bin");

  assert.deepEqual(listing.entries, [{ name: "index.ts", type: "file", size: 3 }]);
  assert.deepEqual([...bytes], [0, 255, 2]);
  assert.equal(calls[1]?.url, "http://localhost:8080/v1/workspace/space%20file.bin");
  assert.equal(calls[2]?.init?.method, "PUT");
  assert.equal(calls[3]?.init?.method, "DELETE");
});

test("rejects traversal and workspace-root mutations locally", async () => {
  const fetch: typeof globalThis.fetch = async () => new Response(null, { status: 204 });
  const client = new AgentCellClient({ baseUrl: "http://localhost:8080", token: "secret", fetch });

  await assert.rejects(
    client.workspace.list("../outside"),
    (error: unknown) => error instanceof AgentCellValidationError,
  );
  await assert.rejects(
    client.workspace.delete(""),
    (error: unknown) => error instanceof AgentCellValidationError,
  );
});
