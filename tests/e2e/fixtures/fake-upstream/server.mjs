import { createServer } from "node:http";
import { createHash, randomUUID } from "node:crypto";

const port = Number(process.env.PORT ?? 4010);
const adminKey = process.env.LITELLM_MASTER_KEY ?? "e2e-master-key";
const pluginKey = process.env.E2E_PLUGIN_KEY ?? "sk-e2e-wasm-plugin";
const users = new Map([["admin@example.com", { user_id: "upstream-admin", user_email: "admin@example.com", teams: [] }]]);
const keys = new Map([[pluginKey, { token: "e2e-wasm-plugin" }]]);
const requests = [];

function json(response, status, body) {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

async function body(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  const bytes = Buffer.concat(chunks);
  return bytes.length ? JSON.parse(bytes.toString("utf8")) : {};
}

createServer(async (request, response) => {
  const url = new URL(request.url ?? "/", `http://${request.headers.host}`);
  if (url.pathname === "/health" || url.pathname === "/health/liveliness") return json(response, 200, { status: "ok" });
  if (url.pathname === "/_e2e/requests") return json(response, 200, requests);
  if (url.pathname === "/_e2e/reset" && request.method === "POST") {
    requests.length = 0;
    keys.clear();
    keys.set(pluginKey, { token: "e2e-wasm-plugin" });
    return json(response, 200, { reset: true });
  }

  if (request.headers.authorization === `Bearer ${adminKey}`) {
    if (url.pathname === "/user/list") {
      const email = url.searchParams.get("user_email")?.toLowerCase();
      return json(response, 200, { users: email && users.has(email) ? [users.get(email)] : [] });
    }
    if (url.pathname === "/key/generate" && request.method === "POST") {
      const input = await body(request);
      const key = `sk-e2e-${randomUUID()}`;
      const token = createHash("sha256").update(key).digest("hex");
      keys.set(key, { ...input, token });
      return json(response, 200, { key, token, expires: null });
    }
    if (url.pathname === "/key/list" && request.method === "GET") {
      const alias = url.searchParams.get("key_alias");
      return json(response, 200, { keys: [...keys.values()].filter((key) => !alias || key.key_alias === alias) });
    }
    if (url.pathname === "/key/info" && request.method === "GET") {
      const requested = url.searchParams.get("key");
      const entry = [...keys.entries()].find(([key, value]) => key === requested || value.token === requested)?.[1];
      if (!entry) return json(response, 404, { error: { type: "token_not_found_in_db", code: "401" } });
      return json(response, 200, { key: requested, info: entry });
    }
    if (url.pathname === "/key/update" && request.method === "POST") {
      const input = await body(request);
      const credential = [...keys.entries()].find(([key, value]) => key === input.key || value.token === input.key)?.[0];
      if (!credential) return json(response, 404, { error: "unknown key" });
      keys.set(credential, { ...keys.get(credential), ...input });
      return json(response, 200, { updated: true });
    }
    if (url.pathname === "/key/delete" && request.method === "POST") {
      const input = await body(request);
      for (const key of input.keys ?? []) {
        keys.delete(key);
        for (const [credential, value] of keys) if (value.token === key) keys.delete(credential);
      }
      return json(response, 200, { deleted_keys: input.keys ?? [] });
    }
  }

  const credential = request.headers.authorization?.replace(/^Bearer\s+/, "");
  if (!credential || !keys.has(credential)) return json(response, 401, { error: { message: "invalid virtual key" } });
  const input = await body(request);
  const outputText = JSON.stringify(input).includes("BLUE_CERT_OK") ? "BLUE_CERT_OK" : "hello";
  const namespacedTool = (input.tools ?? [])
    .filter((tool) => tool?.type === "namespace")
    .flatMap((tool) => (tool.tools ?? []).map((nested) => ({ name: nested?.name, namespace: tool.name })))
    .find((tool) => typeof tool.name === "string" && tool.name.includes("blue_certify"));
  const toolName = namespacedTool?.name ?? (input.tools ?? [])
    .flatMap((tool) => [tool?.name, tool?.function?.name])
    .find((name) => typeof name === "string" && name.includes("blue_certify"));
  const toolNamespace = namespacedTool?.namespace;
  const needsToolCall = Boolean(toolName) && !JSON.stringify(input).includes("BLUE_MCP_OK");
  requests.push({ method: request.method, path: url.pathname, query: url.search, authorization: credential, body: input, headers: request.headers });
  if (url.pathname === "/v1/e2e/error") {
    response.setHeader("retry-after", "7");
    return json(response, 429, { error: { message: "e2e upstream rate limit" } });
  }
  if (url.pathname.endsWith("/responses")) {
    if (needsToolCall) {
      const item = {
        id: "fc_e2e", type: "function_call", status: "completed", name: toolName,
        ...(toolNamespace ? { namespace: toolNamespace } : {}),
        call_id: "call_e2e", arguments: "{}",
      };
      const toolResponse = { id: "resp_tool_e2e", object: "response", status: "completed", model: input.model, output: [item], usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 } };
      if (input.stream) {
        response.writeHead(200, { "content-type": "text/event-stream" });
        response.write(`event: response.output_item.added\ndata: ${JSON.stringify({ type: "response.output_item.added", output_index: 0, item })}\n\n`);
        response.write(`event: response.function_call_arguments.done\ndata: ${JSON.stringify({ type: "response.function_call_arguments.done", item_id: item.id, output_index: 0, name: toolName, arguments: "{}" })}\n\n`);
        response.write(`event: response.output_item.done\ndata: ${JSON.stringify({ type: "response.output_item.done", output_index: 0, item })}\n\n`);
        response.write(`event: response.completed\ndata: ${JSON.stringify({ type: "response.completed", response: toolResponse })}\n\n`);
        return response.end();
      }
      return json(response, 200, toolResponse);
    }
    if (input.stream) {
      const responseBody = {
        id: "resp_e2e",
        object: "response",
        status: "completed",
        model: input.model,
        output: [{
          id: "msg_e2e", type: "message", status: "completed", role: "assistant",
          content: [{ type: "output_text", text: outputText, annotations: [] }],
        }],
        usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
      };
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.write(`event: response.created\ndata: ${JSON.stringify({ type: "response.created", response: { ...responseBody, status: "in_progress", output: [] } })}\n\n`);
      response.write(`event: response.output_item.added\ndata: ${JSON.stringify({ type: "response.output_item.added", output_index: 0, item: responseBody.output[0] })}\n\n`);
      response.write(`event: response.output_text.delta\ndata: ${JSON.stringify({ type: "response.output_text.delta", item_id: "msg_e2e", output_index: 0, content_index: 0, delta: outputText })}\n\n`);
      response.write(`event: response.output_text.done\ndata: ${JSON.stringify({ type: "response.output_text.done", item_id: "msg_e2e", output_index: 0, content_index: 0, text: outputText })}\n\n`);
      response.write(`event: response.completed\ndata: ${JSON.stringify({ type: "response.completed", response: responseBody })}\n\n`);
      return response.end();
    }
    return json(response, 200, { id: "resp_e2e", object: "response", status: "completed", model: input.model, output_text: outputText, output: [] });
  }
  if (url.pathname.endsWith("/messages")) {
    if (needsToolCall) {
      const message = { id: "msg_tool_e2e", type: "message", role: "assistant", model: input.model, content: [{ type: "tool_use", id: "toolu_e2e", name: toolName, input: {} }], stop_reason: "tool_use", stop_sequence: null, usage: { input_tokens: 1, output_tokens: 1 } };
      if (!input.stream) return json(response, 200, message);
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.write(`event: message_start\ndata: ${JSON.stringify({ type: "message_start", message: { ...message, content: [], stop_reason: null } })}\n\n`);
      response.write(`event: content_block_start\ndata: ${JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "toolu_e2e", name: toolName, input: {} } })}\n\n`);
      response.write(`event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: "{}" } })}\n\n`);
      response.write(`event: content_block_stop\ndata: ${JSON.stringify({ type: "content_block_stop", index: 0 })}\n\n`);
      response.write(`event: message_delta\ndata: ${JSON.stringify({ type: "message_delta", delta: { stop_reason: "tool_use", stop_sequence: null }, usage: { output_tokens: 1 } })}\n\n`);
      response.write(`event: message_stop\ndata: ${JSON.stringify({ type: "message_stop" })}\n\n`);
      return response.end();
    }
    const message = { id: "msg_e2e", type: "message", role: "assistant", model: input.model, content: [{ type: "text", text: outputText }], stop_reason: "end_turn", stop_sequence: null, usage: { input_tokens: 1, output_tokens: 1 } };
    if (input.stream) {
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.write(`event: message_start\ndata: ${JSON.stringify({ type: "message_start", message: { ...message, content: [], stop_reason: null, usage: { input_tokens: 1, output_tokens: 0 } } })}\n\n`);
      response.write(`event: content_block_start\ndata: ${JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } })}\n\n`);
      response.write(`event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: outputText } })}\n\n`);
      response.write(`event: content_block_stop\ndata: ${JSON.stringify({ type: "content_block_stop", index: 0 })}\n\n`);
      response.write(`event: message_delta\ndata: ${JSON.stringify({ type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 1 } })}\n\n`);
      response.write(`event: message_stop\ndata: ${JSON.stringify({ type: "message_stop" })}\n\n`);
      return response.end();
    }
    return json(response, 200, message);
  }
  if (input.stream) {
    if (needsToolCall) {
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.write(`data: ${JSON.stringify({ id: "chatcmpl-tool-e2e", object: "chat.completion.chunk", choices: [{ index: 0, delta: { role: "assistant", tool_calls: [{ index: 0, id: "call_e2e", type: "function", function: { name: toolName, arguments: "{}" } }] }, finish_reason: null }], model: input.model })}\n\n`);
      response.write(`data: ${JSON.stringify({ id: "chatcmpl-tool-e2e", object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }], model: input.model })}\n\n`);
      response.end("data: [DONE]\n\n");
      return;
    }
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.write(`data: ${JSON.stringify({ id: "chatcmpl-e2e", object: "chat.completion.chunk", choices: [{ index: 0, delta: { role: "assistant", content: outputText }, finish_reason: null }], model: input.model })}\n\n`);
    response.write(`data: ${JSON.stringify({ id: "chatcmpl-e2e", object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: "stop" }], model: input.model })}\n\n`);
    response.end("data: [DONE]\n\n");
    return;
  }
  if (needsToolCall) {
    return json(response, 200, { id: "chatcmpl-tool-e2e", object: "chat.completion", model: input.model, choices: [{ index: 0, finish_reason: "tool_calls", message: { role: "assistant", content: null, tool_calls: [{ id: "call_e2e", type: "function", function: { name: toolName, arguments: "{}" } }] } }] });
  }
  return json(response, 200, { id: "e2e-completion", object: "chat.completion", model: input.model, choices: [{ index: 0, finish_reason: "stop", message: { role: "assistant", content: outputText } }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } });
}).listen(port, "0.0.0.0", () => console.log(`fake LiteLLM listening on ${port}`));
