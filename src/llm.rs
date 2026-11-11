//! LLM services: Claude (Anthropic Messages API), ChatGPT (OpenAI), Gemini (Google's
//! OpenAI-compatible endpoint) and any self-hosted OpenAI-compatible server.
//!
//! Conversations are stored in one provider-neutral shape (OpenAI-style chat messages) so that a
//! board's history can be continued by a different service. `Session` converts that history into
//! the provider's native format once, then keeps the native transcript append-only for the rest
//! of the turn.
use crate::db::{query, query_as};
use crate::db::{get_setting, Db};
use crate::error::{bad, Res};
use crate::ops::Column;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc::Sender;

/// (key, label, default base URL)
pub const KINDS: &[(&str, &str, &str)] = &[
    ("anthropic", "Claude (Anthropic)", "https://api.anthropic.com"),
    ("openai", "ChatGPT (OpenAI)", "https://api.openai.com/v1"),
    ("gemini", "Gemini (Google)", "https://generativelanguage.googleapis.com/v1beta/openai"),
    ("compatible", "OpenAI-compatible (self-hosted or proxy)", ""),
];

const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Streaming output ceiling for Claude. Generous, because thinking counts against it.
const ANTHROPIC_MAX_TOKENS: u32 = 64000;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct Service {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    pub models: String,
    pub enabled: bool,
}

impl Service {
    pub fn model_list(&self) -> Vec<String> {
        serde_json::from_str(&self.models).unwrap_or_default()
    }
    pub fn kind_label(&self) -> &'static str {
        KINDS.iter().find(|k| k.0 == self.kind).map(|k| k.1).unwrap_or("Unknown")
    }
    /// Configured base URL, or the provider's public endpoint.
    pub fn url(&self) -> String {
        let default = KINDS.iter().find(|k| k.0 == self.kind).map(|k| k.2).unwrap_or("");
        let base = if self.base_url.trim().is_empty() { default } else { self.base_url.trim() };
        base.trim_end_matches('/').to_string()
    }
}

const SERVICE_COLUMNS: &str = "id, name, kind, base_url, api_key, models, enabled";

pub async fn services(db: &Db) -> Res<Vec<Service>> {
    Ok(query_as(&format!("SELECT {SERVICE_COLUMNS} FROM llm_services ORDER BY name"))
        .fetch_all(db)
        .await?)
}

pub async fn service(db: &Db, id: i64) -> Res<Option<Service>> {
    Ok(query_as(&format!("SELECT {SERVICE_COLUMNS} FROM llm_services WHERE id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// Platform-wide agent settings (admin panel).
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct AgentDefaults {
    pub service_id: Option<i64>,
    pub model: String,
    pub system_prompt: String,
}

/// The service and model one agent run will use, and which level chose it.
pub struct Target {
    pub service: Service,
    pub model: String,
    pub source: &'static str,
}

impl Target {
    pub fn label(&self) -> String {
        format!("{} · {}", self.service.name, self.model)
    }
}

/// Column override -> project default -> platform default.
pub async fn resolve(db: &Db, project_id: i64, column: Option<&Column>) -> Result<Target, String> {
    let err = |e: anyhow::Error| format!("database error: {e}");
    let project: Option<(Option<i64>, String)> = query_as("SELECT agent_service_id, agent_model FROM projects WHERE id = ?")
        .bind(project_id)
        .fetch_optional(db)
        .await
        .map_err(err)?;
    let defaults: AgentDefaults = get_setting(db, "agent").await;
    let mut levels: Vec<(Option<i64>, String, &'static str)> = Vec::new();
    if let Some(c) = column {
        levels.push((c.agent_service_id, c.agent_model.clone(), "column"));
    }
    if let Some((id, model)) = project {
        levels.push((id, model, "project"));
    }
    levels.push((defaults.service_id, defaults.model, "platform default"));

    for (service_id, model, source) in levels {
        let Some(id) = service_id else { continue };
        let Some(service) = service(db, id).await.map_err(|e| e.message())? else { continue };
        if !service.enabled {
            return Err(format!("The agent service “{}” (chosen by the {source}) is disabled.", service.name));
        }
        let models = service.model_list();
        // A model that was later removed from the service falls back to the service's first model.
        let model = match models.iter().find(|m| **m == model) {
            Some(m) => m.clone(),
            None => models.first().cloned().ok_or(format!("The agent service “{}” has no models configured.", service.name))?,
        };
        return Ok(Target { service, model, source });
    }
    Err("No agent service is configured. An administrator can add one under Admin → Agent services, then pick a default there or on the project page.".into())
}

/// Form value for a service / model choice: `"<service id>|<model>"`, or empty for "inherit".
pub fn target_value(service_id: Option<i64>, model: &str) -> String {
    service_id.map(|id| format!("{id}|{model}")).unwrap_or_default()
}

pub async fn parse_target(db: &Db, value: &str) -> Res<(Option<i64>, String)> {
    let value = value.trim();
    if value.is_empty() {
        return Ok((None, String::new()));
    }
    let (id, model) = value.split_once('|').ok_or_else(|| bad("Invalid service / model choice"))?;
    let id: i64 = id.parse().map_err(|_| bad("Invalid service / model choice"))?;
    let svc = service(db, id).await?.ok_or_else(|| bad("That agent service no longer exists"))?;
    if !svc.model_list().iter().any(|m| m == model) {
        return Err(bad("That model is not offered by the chosen service"));
    }
    Ok((Some(id), model.to_string()))
}

// ------------------------------------------------------------------------------------------
// Wire helpers
// ------------------------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON text exactly as the model produced it (may be invalid; the caller validates).
    pub arguments: String,
    /// Provider data that must be echoed back with the call (Gemini thought signatures).
    pub extra: Option<Value>,
}

#[derive(Default)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

async fn emit(tx: &Sender<Value>, v: Value) -> Result<(), String> {
    tx.send(v).await.map_err(|_| "The browser disconnected".to_string())
}

/// Reads the `data:` payloads of a server-sent event stream.
struct SseReader {
    resp: reqwest::Response,
    buf: Vec<u8>,
}

impl SseReader {
    async fn next(&mut self) -> Result<Option<String>, String> {
        loop {
            while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                if let Some(data) = line.trim().strip_prefix("data:") {
                    return Ok(Some(data.trim().to_string()));
                }
            }
            let chunk = tokio::time::timeout(STREAM_IDLE_TIMEOUT, self.resp.chunk())
                .await
                .map_err(|_| "The model stopped responding".to_string())?
                .map_err(|e| format!("The response stream failed: {e}"))?;
            match chunk {
                Some(c) => self.buf.extend_from_slice(&c),
                None => return Ok(None),
            }
        }
    }
}

async fn check_status(resp: reqwest::Response, who: &str) -> Result<reqwest::Response, String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(600).collect());
    Err(format!("{who} returned {status}: {detail}"))
}

/// OpenAI-format tool definitions -> Anthropic tool definitions.
fn anthropic_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t["function"]["name"],
                "description": t["function"]["description"],
                "input_schema": t["function"]["parameters"],
                // Stream tool input as it is generated; we parse and validate it ourselves.
                "eager_input_streaming": true,
            })
        })
        .collect()
}

/// Anthropic only accepts `[a-zA-Z0-9_-]` in tool-use ids; ids minted by other providers may not comply.
fn anthropic_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Stored (OpenAI-style) history -> Anthropic `messages`. Tool results are grouped into one user
/// message directly after the assistant turn that made the calls.
fn anthropic_messages(history: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in history {
        match m["role"].as_str().unwrap_or("") {
            "user" => {
                if let Some(text) = m["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    out.push(json!({ "role": "user", "content": [{ "type": "text", "text": text }] }));
                }
            }
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(text) = m["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for tc in m["tool_calls"].as_array().into_iter().flatten() {
                    let input = tc["function"]["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": anthropic_id(tc["id"].as_str().unwrap_or("")),
                        "name": tc["function"]["name"],
                        "input": input,
                    }));
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": anthropic_id(m["tool_call_id"].as_str().unwrap_or("")),
                    "content": m["content"].as_str().unwrap_or(""),
                });
                let merged = out.last_mut().is_some_and(|last| {
                    let is_results = last["role"] == "user" && last["content"][0]["type"] == "tool_result";
                    if is_results {
                        if let Some(content) = last["content"].as_array_mut() {
                            content.push(block.clone());
                        }
                    }
                    is_results
                });
                if !merged {
                    out.push(json!({ "role": "user", "content": [block] }));
                }
            }
            _ => {}
        }
    }
    out
}

/// Stored history -> OpenAI chat messages. Only standard fields are sent; Gemini additionally
/// gets back the `extra_content` (thought signatures) it attached to its own tool calls.
fn openai_messages(history: &[Value], keep_extra: bool) -> Vec<Value> {
    history
        .iter()
        .filter_map(|m| match m["role"].as_str()? {
            "user" => Some(json!({ "role": "user", "content": m["content"] })),
            "tool" => Some(json!({ "role": "tool", "tool_call_id": m["tool_call_id"], "content": m["content"] })),
            "assistant" => {
                let mut out = json!({ "role": "assistant", "content": m["content"].as_str().unwrap_or("") });
                let calls: Vec<Value> = m["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|tc| {
                        let mut call = json!({ "id": tc["id"], "type": "function", "function": tc["function"] });
                        if keep_extra && tc["extra_content"].is_object() {
                            call["extra_content"] = tc["extra_content"].clone();
                        }
                        call
                    })
                    .collect();
                if !calls.is_empty() {
                    out["tool_calls"] = Value::Array(calls);
                }
                Some(out)
            }
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------------------------------
// Session: one user turn (possibly many model calls) against one service
// ------------------------------------------------------------------------------------------

pub struct Session {
    pub target: Target,
    system: String,
    /// Tool definitions in OpenAI function format.
    tools: Vec<Value>,
    /// Transcript in the provider's native format. Append-only for the life of the session, which
    /// keeps Claude's thinking blocks valid and the prompt cache warm between tool rounds.
    messages: Vec<Value>,
}

impl Session {
    pub fn new(target: Target, system: String, tools: Vec<Value>, history: &[Value]) -> Self {
        let messages = match target.service.kind.as_str() {
            "anthropic" => anthropic_messages(history),
            kind => openai_messages(history, kind == "gemini"),
        };
        Session { target, system, tools, messages }
    }

    /// Ask the model for its next step. Text is streamed to the browser as it arrives.
    pub async fn step(&mut self, http: &reqwest::Client, tx: &Sender<Value>) -> Result<Reply, String> {
        if self.target.service.kind == "anthropic" {
            self.step_anthropic(http, tx).await
        } else {
            self.step_openai(http, tx).await
        }
    }

    /// Hand the results of the last reply's tool calls back, in call order.
    pub fn tool_results(&mut self, results: &[(String, String)]) {
        if self.target.service.kind == "anthropic" {
            let blocks: Vec<Value> = results
                .iter()
                .map(|(id, content)| json!({ "type": "tool_result", "tool_use_id": anthropic_id(id), "content": content }))
                .collect();
            // All results of one assistant turn go back in a single user message.
            self.messages.push(json!({ "role": "user", "content": blocks }));
        } else {
            for (id, content) in results {
                self.messages.push(json!({ "role": "tool", "tool_call_id": id, "content": content }));
            }
        }
    }

    async fn step_openai(&mut self, http: &reqwest::Client, tx: &Sender<Value>) -> Result<Reply, String> {
        let svc = &self.target.service;
        let url = format!("{}/chat/completions", svc.url());
        let mut messages = vec![json!({ "role": "system", "content": self.system })];
        messages.extend(self.messages.iter().cloned());
        let mut req = http
            .post(&url)
            .json(&json!({ "model": self.target.model, "messages": messages, "tools": self.tools, "stream": true }));
        if !svc.api_key.is_empty() {
            req = req.bearer_auth(&svc.api_key);
        }
        let resp = req.send().await.map_err(|e| format!("Could not reach {} at {url}: {e}", svc.name))?;
        let resp = check_status(resp, &svc.name).await?;
        let is_stream = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("event-stream"));
        let mut reply = Reply::default();

        if !is_stream {
            // The server ignored `stream: true`; accept a plain completion.
            let v: Value = resp.json().await.map_err(|e| format!("Unreadable response from {}: {e}", svc.name))?;
            let msg = &v["choices"][0]["message"];
            if let Some(t) = msg["content"].as_str() {
                reply.content = t.to_string();
                emit(tx, json!({ "type": "delta", "text": t })).await?;
            }
            for tc in msg["tool_calls"].as_array().into_iter().flatten() {
                reply.tool_calls.push(ToolCall {
                    id: tc["id"].as_str().unwrap_or_default().to_string(),
                    name: tc["function"]["name"].as_str().unwrap_or_default().to_string(),
                    arguments: tc["function"]["arguments"].as_str().unwrap_or("{}").to_string(),
                    extra: tc.get("extra_content").filter(|e| e.is_object()).cloned(),
                });
            }
        } else {
            let mut sse = SseReader { resp, buf: Vec::new() };
            while let Some(data) = sse.next().await? {
                if data == "[DONE]" {
                    break;
                }
                let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                if let Some(err) = v.get("error") {
                    return Err(format!("{} reported an error: {}", svc.name, err["message"].as_str().unwrap_or(&err.to_string())));
                }
                let delta = &v["choices"][0]["delta"];
                if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                    reply.content.push_str(t);
                    emit(tx, json!({ "type": "delta", "text": t })).await?;
                }
                for tc in delta["tool_calls"].as_array().into_iter().flatten() {
                    let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                    if idx > 64 {
                        continue;
                    }
                    while reply.tool_calls.len() <= idx {
                        reply.tool_calls.push(ToolCall::default());
                    }
                    let call = &mut reply.tool_calls[idx];
                    if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                        call.id = id.to_string();
                    }
                    if let Some(n) = tc["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                        if call.name.is_empty() {
                            call.name = n.to_string();
                        }
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        call.arguments.push_str(a);
                    }
                    if let Some(extra) = tc.get("extra_content").filter(|e| e.is_object()) {
                        call.extra = Some(extra.clone());
                    }
                }
            }
        }

        for (i, call) in reply.tool_calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{i}_{}", crate::auth::random_token(4));
            }
            if call.arguments.trim().is_empty() {
                call.arguments = "{}".into();
            }
        }
        let keep_extra = svc.kind == "gemini";
        let mut assistant = json!({ "role": "assistant", "content": reply.content });
        if !reply.tool_calls.is_empty() {
            assistant["tool_calls"] = reply
                .tool_calls
                .iter()
                .map(|c| {
                    let mut call = json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.arguments } });
                    if let (true, Some(extra)) = (keep_extra, &c.extra) {
                        call["extra_content"] = extra.clone();
                    }
                    call
                })
                .collect();
        }
        self.messages.push(assistant);
        Ok(reply)
    }

    /// Server-side refusal fallbacks exist on the first-party Claude API for these model families;
    /// with them a declined request is retried on the model Anthropic recommends for the category.
    fn anthropic_fallbacks(&self) -> bool {
        let first_party = self.target.service.url() == "https://api.anthropic.com";
        let model = self.target.model.as_str();
        first_party && ["claude-fable-5-1", "claude-opus-5", "claude-sonnet-5-5"].iter().any(|p| model.starts_with(p))
    }

    async fn step_anthropic(&mut self, http: &reqwest::Client, tx: &Sender<Value>) -> Result<Reply, String> {
        let svc = &self.target.service;
        let url = format!("{}/v1/messages", svc.url());
        // `thinking` is left unset on purpose: current Claude models think adaptively by default,
        // and older ones reject the adaptive setting.
        let mut body = json!({
            "model": self.target.model,
            "max_tokens": ANTHROPIC_MAX_TOKENS,
            "stream": true,
            "system": self.system,
            "tools": anthropic_tools(&self.tools),
            "messages": self.messages,
            // Cache the growing prefix (tools, system, earlier messages) between tool rounds.
            "cache_control": { "type": "ephemeral" },
        });
        let mut req = http.post(&url).header("anthropic-version", ANTHROPIC_VERSION);
        if !svc.api_key.is_empty() {
            req = req.header("x-api-key", &svc.api_key);
        }
        if self.anthropic_fallbacks() {
            body["fallbacks"] = json!("default");
            req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
        }
        let resp = req.json(&body).send().await.map_err(|e| format!("Could not reach {} at {url}: {e}", svc.name))?;
        let resp = check_status(resp, &svc.name).await?;

        // Rebuild the response's content blocks exactly as sent, so they can be passed back unchanged.
        let mut blocks: Vec<Value> = Vec::new();
        let mut partial_json: Vec<String> = Vec::new();
        let mut stop_reason = String::new();
        let mut stop_category = String::new();
        let mut sse = SseReader { resp, buf: Vec::new() };
        while let Some(data) = sse.next().await? {
            let Ok(ev) = serde_json::from_str::<Value>(&data) else { continue };
            let index = ev["index"].as_u64().unwrap_or(0) as usize;
            match ev["type"].as_str().unwrap_or("") {
                "content_block_start" => {
                    if index > 512 {
                        continue;
                    }
                    while blocks.len() <= index {
                        blocks.push(Value::Null);
                        partial_json.push(String::new());
                    }
                    blocks[index] = ev["content_block"].clone();
                }
                "content_block_delta" => {
                    let Some(block) = blocks.get_mut(index) else { continue };
                    let delta = &ev["delta"];
                    let append = |block: &mut Value, key: &str, text: &str| {
                        let mut s = block[key].as_str().unwrap_or("").to_string();
                        s.push_str(text);
                        block[key] = json!(s);
                    };
                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let text = delta["text"].as_str().unwrap_or("");
                            append(block, "text", text);
                            if !text.is_empty() {
                                emit(tx, json!({ "type": "delta", "text": text })).await?;
                            }
                        }
                        "input_json_delta" => partial_json[index].push_str(delta["partial_json"].as_str().unwrap_or("")),
                        "thinking_delta" => append(block, "thinking", delta["thinking"].as_str().unwrap_or("")),
                        "signature_delta" => block["signature"] = delta["signature"].clone(),
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(r) = ev["delta"]["stop_reason"].as_str() {
                        stop_reason = r.to_string();
                    }
                    let details = if ev["delta"]["stop_details"].is_object() { &ev["delta"]["stop_details"] } else { &ev["stop_details"] };
                    if let Some(c) = details["category"].as_str() {
                        stop_category = c.to_string();
                    }
                }
                "error" => {
                    return Err(format!("{} reported an error: {}", svc.name, ev["error"]["message"].as_str().unwrap_or("unknown error")));
                }
                _ => {}
            }
        }

        // Always check the stop reason before using the content.
        if stop_reason == "refusal" {
            let category = if stop_category.is_empty() { String::new() } else { format!(" (category: {stop_category})") };
            return Err(format!("{} declined this request{category}. Nothing was changed by this step; try rephrasing.", self.target.model));
        }

        // After a mid-response fallback to another model, blocks before the last `fallback` marker
        // belong to the model that declined: only their text may be passed back.
        let boundary = blocks.iter().rposition(|b| b["type"] == "fallback");
        let mut content: Vec<Value> = Vec::new();
        let mut reply = Reply::default();
        for (i, mut block) in blocks.into_iter().enumerate() {
            let kind = block["type"].as_str().unwrap_or("").to_string();
            let before_boundary = boundary.is_some_and(|b| i < b);
            match kind.as_str() {
                "" | "fallback" => continue,
                "text" => {
                    let text = block["text"].as_str().unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    reply.content.push_str(text);
                }
                _ if before_boundary => continue,
                "tool_use" => {
                    let raw = std::mem::take(&mut partial_json[i]);
                    let raw = if raw.trim().is_empty() { "{}".to_string() } else { raw };
                    // The native block must carry an object; invalid input is reported back to the
                    // model as a failed call by the caller, which parses `arguments` strictly.
                    block["input"] = serde_json::from_str::<Value>(&raw).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
                    reply.tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        arguments: raw,
                        extra: None,
                    });
                }
                _ => {} // thinking, redacted_thinking and anything newer: passed back untouched
            }
            content.push(block);
        }

        if stop_reason == "max_tokens" && !reply.tool_calls.is_empty() {
            // A truncated tool input can still parse as a valid, shorter object; never run it.
            return Err("The model's response was cut off at the output limit in the middle of a tool call, so nothing was run. Try a smaller request.".into());
        }
        if !content.is_empty() {
            self.messages.push(json!({ "role": "assistant", "content": content }));
        }
        Ok(reply)
    }
}

/// Lists the model ids a service's credentials can see. Used by the admin panel's "Test" button.
pub async fn list_models(http: &reqwest::Client, svc: &Service) -> Result<Vec<String>, String> {
    let (url, req) = if svc.kind == "anthropic" {
        let url = format!("{}/v1/models?limit=1000", svc.url());
        let mut req = http.get(&url).header("anthropic-version", ANTHROPIC_VERSION);
        if !svc.api_key.is_empty() {
            req = req.header("x-api-key", &svc.api_key);
        }
        (url, req)
    } else {
        let url = format!("{}/models", svc.url());
        let mut req = http.get(&url);
        if !svc.api_key.is_empty() {
            req = req.bearer_auth(&svc.api_key);
        }
        (url, req)
    };
    let resp = req
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("Could not reach {url}: {e}"))?;
    let v: Value = check_status(resp, &svc.name)
        .await?
        .json()
        .await
        .map_err(|e| format!("Unreadable model list from {url}: {e}"))?;
    let mut ids: Vec<String> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        // Gemini's OpenAI-compatible endpoint lists ids as "models/<id>".
        .map(|id| id.trim_start_matches("models/").to_string())
        .collect();
    ids.sort();
    Ok(ids)
}

/// One-time upgrade from the single "OpenAI-compatible endpoint" settings of the first release:
/// the platform endpoint and any per-board endpoints become services.
pub async fn migrate_legacy(db: &Db) -> anyhow::Result<()> {
    let unique_name = |base: String, taken: &[String]| {
        let mut name = base.clone();
        let mut n = 2;
        while taken.contains(&name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        name
    };
    let mut taken: Vec<String> = query_as::<(String,)>("SELECT name FROM llm_services")
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|r| r.0)
        .collect();

    let old: Value = get_setting(db, "agent").await;
    if let Some(base_url) = old["base_url"].as_str().filter(|u| !u.is_empty()) {
        let model = old["model"].as_str().unwrap_or("").to_string();
        let name = unique_name("Default endpoint".into(), &taken);
        taken.push(name.clone());
        let id = query("INSERT INTO llm_services (name, kind, base_url, api_key, models) VALUES (?, 'compatible', ?, ?, ?)")
            .bind(&name)
            .bind(base_url)
            .bind(old["api_key"].as_str().unwrap_or(""))
            .bind(json!([model]).to_string())
            .insert(db)
        .await?;
        let defaults = AgentDefaults {
            service_id: Some(id),
            model,
            system_prompt: old["system_prompt"].as_str().unwrap_or("").to_string(),
        };
        crate::db::set_setting(db, "agent", &defaults).await?;
        tracing::info!("migrated the platform agent endpoint to the service '{name}'");
    }

    let boards: Vec<(i64, i64, String, String, String, String)> = query_as(
        "SELECT id, project_id, name, agent_base_url, agent_api_key, agent_model FROM boards WHERE agent_base_url != ''",
    )
    .fetch_all(db)
    .await?;
    for (board_id, project_id, board_name, base_url, api_key, model) in boards {
        let name = unique_name(format!("Board: {board_name}"), &taken);
        taken.push(name.clone());
        let id = query("INSERT INTO llm_services (name, kind, base_url, api_key, models) VALUES (?, 'compatible', ?, ?, ?)")
            .bind(&name)
            .bind(&base_url)
            .bind(&api_key)
            .bind(json!([model]).to_string())
            .insert(db)
        .await?;
        // The endpoint becomes its project's default unless the project already has one.
        query("UPDATE projects SET agent_service_id = ?, agent_model = ? WHERE id = ? AND agent_service_id IS NULL")
            .bind(id)
            .bind(&model)
            .bind(project_id)
            .execute(db)
            .await?;
        query("UPDATE boards SET agent_base_url = '', agent_api_key = '', agent_model = '' WHERE id = ?")
            .bind(board_id)
            .execute(db)
            .await?;
        tracing::info!("migrated board {board_id}'s agent endpoint to the service '{name}'");
    }
    Ok(())
}

crate::row_struct!(Service { id, name, kind, base_url, api_key, models, enabled });
