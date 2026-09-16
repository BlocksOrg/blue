import { join } from "node:path";
import { createInterface } from "node:readline";
import { mkdirSync, writeFileSync } from "node:fs";

const markers = process.env.E2E_SLIM_MARKER_DIR || "/tmp/blue-e2e/component-markers";
mkdirSync(markers, { recursive: true });
writeFileSync(join(markers, "mcp-started"), "started\n");

const input = createInterface({ input: process.stdin });
input.on("line", (line) => {
  let message;
  try { message = JSON.parse(line); } catch { return; }
  if (message.id === undefined) return;
  let result = {};
  if (message.method === "initialize") {
    result = { protocolVersion: "2025-03-26", capabilities: { tools: {} }, serverInfo: { name: "blue-e2e", version: "1.0.0" } };
  } else if (message.method === "tools/list") {
    result = { tools: [{ name: "blue_certify", description: "Return the Blue E2E MCP certification marker", inputSchema: { type: "object", properties: {} } }] };
  } else if (message.method === "tools/call") {
    writeFileSync(join(markers, "mcp-called"), "called\n");
    result = { content: [{ type: "text", text: "BLUE_MCP_OK" }] };
  }
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: message.id, result })}\n`);
});
