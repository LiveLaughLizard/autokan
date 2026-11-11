// Tiny scripted LLM server for testing the agent console without a real model. NOT a real model.
//
//   node dev/mock-llm.mjs             # http://localhost:9100
//
// It speaks both wire formats Autokan uses:
//   OpenAI-compatible   base URL http://localhost:9100/v1   (POST /v1/chat/completions, GET /v1/models)
//   Anthropic Messages  base URL http://localhost:9100      (POST /v1/messages, GET /v1/models)
//
// Behaviour: for a user message it calls a tool derived from the text, then summarises:
//   "create <title>"  -> create_card in the first column
//   "move all"        -> move_card for every card of the first column into the last column
//   "column <name>"   -> create_column
//   "refuse"          -> (Anthropic only) a classifier refusal
//   anything else     -> plain streamed text
// The Anthropic side also emits a signed thinking block before tool calls and returns a 400 if the
// follow-up request does not pass that block back unchanged, the way the real API does.
import http from "node:http";

const PORT = Number(process.env.PORT || 9100);
const MODELS = ["mock-small", "mock-large", "claude-opus-5-5", "claude-haiku-5-5"];
const SIGNATURE = "mock-signature-0123456789";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function plan(text, board, model) {
  const call = (name, args) => ({ name, arguments: JSON.stringify(args) });
  let m;
  if ((m = text.match(/^create (.+)/i))) return { calls: [call("create_card", { column_id: board.columns[0].id, title: m[1], description: `Created by **mock agent** (${model})` })] };
  if ((m = text.match(/^column (.+)/i))) return { calls: [call("create_column", { name: m[1] })] };
  if (/^refuse/i.test(text)) return { refuse: true };
  if (/move all|Instructions:/i.test(text)) {
    const from = /column id (\d+)/.test(text) ? board.columns.find((c) => c.id === Number(text.match(/column id (\d+)/)[1])) : board.columns[0];
    const to = board.columns[board.columns.length - 1];
    const calls = from.cards.map((c) => call("move_card", { card_id: c.id, column_id: to.id }));
    return calls.length ? { calls } : { text: "No cards to move." };
  }
  return { text: `You said: ${text}. The board has ${board.columns.length} columns. I am ${model}.` };
}
const boardFrom = (system) => JSON.parse(system.split("(JSON):\n")[1]);

function openai(body, res) {
  const last = body.messages[body.messages.length - 1];
  const p =
    last.role === "tool"
      ? { text: `Done (${body.model}). Last tool result: ${last.content}` }
      : plan(last.content, boardFrom(body.messages[0].content), body.model);
  res.writeHead(200, { "content-type": "text/event-stream" });
  const send = (delta, finish = null) =>
    res.write(`data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`);
  return (async () => {
    if (p.calls) {
      p.calls.forEach((c, index) => {
        send({ tool_calls: [{ index, id: `call_${index}_${Date.now()}`, type: "function", function: { name: c.name, arguments: "" } }] });
        const mid = Math.floor(c.arguments.length / 2); // split to exercise incremental assembly
        send({ tool_calls: [{ index, function: { arguments: c.arguments.slice(0, mid) } }] });
        send({ tool_calls: [{ index, function: { arguments: c.arguments.slice(mid) } }] });
      });
      send({}, "tool_calls");
    } else {
      for (const word of (p.text || "I cannot do that.").split(/(?<= )/)) {
        send({ content: word });
        await sleep(15);
      }
      send({}, "stop");
    }
    res.end("data: [DONE]\n\n");
  })();
}

function anthropicError(res, message) {
  res.writeHead(400, { "content-type": "application/json" });
  res.end(JSON.stringify({ type: "error", error: { type: "invalid_request_error", message } }));
}

async function anthropic(req, body, res) {
  // Shape checks a real server would enforce.
  if (!req.headers["x-api-key"]) return anthropicError(res, "missing x-api-key");
  if (!req.headers["anthropic-version"]) return anthropicError(res, "missing anthropic-version");
  if (!body.max_tokens) return anthropicError(res, "max_tokens is required");
  if (typeof body.system !== "string") return anthropicError(res, "expected a top-level system prompt");
  if (!body.tools?.every((t) => t.name && t.input_schema && !t.function)) return anthropicError(res, "tools must use name / input_schema");
  if (body.messages[0]?.role !== "user") return anthropicError(res, "first message must be from the user");
  for (const [i, m] of body.messages.entries()) {
    if (!Array.isArray(m.content) || !m.content.length) return anthropicError(res, `messages.${i}: content must be a non-empty array`);
    const uses = m.role === "assistant" ? m.content.filter((b) => b.type === "tool_use") : [];
    if (uses.length) {
      const results = (body.messages[i + 1]?.content || []).filter((b) => b.type === "tool_result").map((b) => b.tool_use_id);
      if (body.messages[i + 1]?.role !== "user" || !uses.every((u) => results.includes(u.id)))
        return anthropicError(res, `messages.${i}: tool_use ids were found without tool_result blocks immediately after`);
    }
  }

  const last = body.messages[body.messages.length - 1];
  const results = last.content.filter((b) => b.type === "tool_result");
  let p;
  if (results.length) {
    // The assistant turn being continued must carry our thinking block back, untouched and first.
    const prev = body.messages[body.messages.length - 2];
    const thinking = prev.content[0];
    if (thinking?.type !== "thinking" || thinking.signature !== SIGNATURE || thinking.thinking !== "Planning the board change.")
      return anthropicError(res, "Invalid `signature` in `thinking` block (it was not passed back unchanged)");
    p = { text: `Done (${body.model}). ${results.length} tool result(s): ${results.map((r) => r.content).join(" ")}` };
  } else {
    p = plan(last.content.find((b) => b.type === "text")?.text || "", boardFrom(body.system), body.model);
  }

  res.writeHead(200, { "content-type": "text/event-stream" });
  const send = (type, data) => res.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
  send("message_start", { message: { id: "msg_mock", type: "message", role: "assistant", model: body.model, content: [] } });
  let index = 0;
  let stop = "end_turn";
  if (p.refuse) {
    send("message_delta", { delta: { stop_reason: "refusal", stop_details: { type: "refusal", category: "cyber" } }, usage: { output_tokens: 0 } });
    send("message_stop", {});
    return res.end();
  }
  if (p.calls) {
    send("content_block_start", { index, content_block: { type: "thinking", thinking: "", signature: "" } });
    send("content_block_delta", { index, delta: { type: "thinking_delta", thinking: "Planning the " } });
    send("content_block_delta", { index, delta: { type: "thinking_delta", thinking: "board change." } });
    send("content_block_delta", { index, delta: { type: "signature_delta", signature: SIGNATURE } });
    send("content_block_stop", { index: index++ });
    for (const c of p.calls) {
      send("content_block_start", { index, content_block: { type: "tool_use", id: `toolu_${index}_${Date.now()}`, name: c.name, input: {} } });
      const mid = Math.floor(c.arguments.length / 2);
      send("content_block_delta", { index, delta: { type: "input_json_delta", partial_json: c.arguments.slice(0, mid) } });
      send("content_block_delta", { index, delta: { type: "input_json_delta", partial_json: c.arguments.slice(mid) } });
      send("content_block_stop", { index: index++ });
    }
    stop = "tool_use";
  } else {
    send("content_block_start", { index, content_block: { type: "text", text: "" } });
    for (const word of p.text.split(/(?<= )/)) {
      send("content_block_delta", { index, delta: { type: "text_delta", text: word } });
      await sleep(15);
    }
    send("content_block_stop", { index });
  }
  send("message_delta", { delta: { stop_reason: stop }, usage: { output_tokens: 10 } });
  send("message_stop", {});
  res.end();
}

http
  .createServer((req, res) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => {
      const path = req.url.split("?")[0];
      if (req.method === "GET" && path.endsWith("/models")) {
        res.writeHead(200, { "content-type": "application/json" });
        return res.end(JSON.stringify({ data: MODELS.map((id) => ({ id })) }));
      }
      if (path.endsWith("/chat/completions")) return openai(JSON.parse(data), res);
      if (path.endsWith("/v1/messages")) return anthropic(req, JSON.parse(data), res);
      res.writeHead(404).end();
    });
  })
  .listen(PORT, () => console.log(`mock LLM on http://localhost:${PORT} (OpenAI-compatible at /v1, Anthropic Messages at /)`));
