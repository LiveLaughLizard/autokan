//! Working with boards from outside the app: personal API keys, a small JSON API and an MCP
//! server. Both offer the board agent's tools (`agent::tools`), run as the key's owner.
use super::redirect;
use crate::agent;
use crate::auth::{self, ApiUser, User};
use crate::db::{query, query_as, Db};
use crate::error::{bad, AppError, Res};
use crate::ops::clean_name;
use crate::perm::{project_access, require};
use crate::views::{page, panel, timestamp};
use crate::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use axum_extra::extract::Form;
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

// ---- managing keys (signed-in users, in the browser) ----------------------------------------

const MAX_KEYS: usize = 50;

fn epoch_text(epoch: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(epoch).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

async fn keys_view(state: &AppState, user: &User, created: Option<&str>) -> Res<Markup> {
    let keys: Vec<(i64, String, String, String, Option<String>, Option<i64>)> =
        query_as("SELECT id, name, prefix, created_at, last_used, expires_at FROM api_keys WHERE user_id = ? ORDER BY id DESC")
            .bind(user.id)
            .fetch_all(&state.db)
            .await?;
    let allowed = user.has("api.keys");
    let url = &state.cfg.public_url;
    let now = crate::db::epoch();
    Ok(page(
        Some(user),
        "API keys",
        false,
        html! {
            div {
                h1 class="text-3xl font-extrabold tracking-tight" { "API keys" }
                p class="opacity-70" { "A key lets a script, an MCP client or a coding agent work on your boards as you, with exactly your permissions." }
            }
            @if !allowed {
                div role="alert" class="alert alert-warning text-sm" {
                    "Your account does not hold the " span class="font-mono" { "api.keys" } " permission, so keys cannot be created and existing ones do not work. An administrator grants it under Admin → Roles & groups."
                }
            }
            @if let Some(token) = created {
                div role="alert" class="alert alert-success flex-col items-start gap-2" {
                    div class="text-sm font-medium" { "Key created. Copy it now: it is not shown again." }
                    input class="input input-sm w-full font-mono" readonly value=(token) onfocus="this.select()";
                }
            }
            @if allowed {
                (panel("New key", html! {
                    form method="post" action="/account/keys" class="flex flex-wrap items-end gap-3" {
                        label class="flex flex-col gap-1 flex-1 min-w-48" {
                            span class="text-sm font-medium" { "Name" }
                            input class="input input-sm w-full" name="name" placeholder="What will use this key? e.g. “laptop opencode”" maxlength="100" required;
                        }
                        label class="flex flex-col gap-1" {
                            span class="text-sm font-medium" { "Expires" }
                            select class="select select-sm" name="days" {
                                option value="30" { "In 30 days" }
                                option value="90" selected { "In 90 days" }
                                option value="365" { "In a year" }
                                option value="0" { "Never" }
                            }
                        }
                        button class="btn btn-primary btn-sm" { "Create key" }
                    }
                }))
            }
            (panel("Your keys", html! {
                @if keys.is_empty() {
                    p class="text-sm opacity-60" { "No keys yet." }
                } @else {
                    div class="overflow-x-auto" {
                        table class="table table-sm" {
                            thead { tr { th { "Name" } th { "Key" } th { "Created" } th { "Last used" } th { "Expires" } th {} } }
                            tbody {
                                @for (id, name, prefix, created_at, last_used, expires_at) in &keys {
                                    tr {
                                        td { (name) }
                                        td class="font-mono text-xs" { (prefix) "…" }
                                        td { (timestamp(created_at)) }
                                        td { @match last_used { Some(t) => { (timestamp(t)) }, None => { span class="opacity-60" { "Never" } } } }
                                        td {
                                            @match expires_at {
                                                Some(e) if *e <= now => { span class="badge badge-sm badge-error" { "Expired" } },
                                                Some(e) => { (timestamp(&epoch_text(*e))) },
                                                None => { span class="opacity-60" { "Never" } },
                                            }
                                        }
                                        td class="text-right" {
                                            form method="post" action={ "/account/keys/" (id) "/delete" } data-confirm="Revoke this key? Anything using it stops working immediately." {
                                                button class="btn btn-xs btn-ghost text-error" { "Revoke" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }))
            (panel("Using a key", html! {
                div class="text-sm flex flex-col gap-3" {
                    p { "Send the key as " span class="font-mono" { "Authorization: Bearer <key>" } ". The tools are the ones the board agent has; " span class="font-mono" { "GET " (url) "/api/v1/tools" } " lists them with their arguments." }
                    div {
                        div class="font-medium" { "JSON API" }
                        pre class="bg-base-200 rounded-box p-3 text-xs overflow-x-auto" {
                            "curl -H \"Authorization: Bearer $AUTOKAN_API_KEY\" " (url) "/api/v1/boards\n"
                            "curl -H \"Authorization: Bearer $AUTOKAN_API_KEY\" " (url) "/api/v1/boards/1\n"
                            "curl -H \"Authorization: Bearer $AUTOKAN_API_KEY\" -d '{\"card_id\": 7, \"column_id\": 3}' \\\n     " (url) "/api/v1/boards/1/tools/move_card"
                        }
                    }
                    div {
                        div class="font-medium" { "MCP server (Streamable HTTP)" }
                        pre class="bg-base-200 rounded-box p-3 text-xs overflow-x-auto" {
                            (url) "/mcp\n"
                            "claude mcp add --transport http autokan " (url) "/mcp --header \"Authorization: Bearer $AUTOKAN_API_KEY\""
                        }
                    }
                }
            }))
        },
    ))
}

pub async fn keys_page(State(state): State<AppState>, user: User) -> Res<Markup> {
    keys_view(&state, &user, None).await
}

#[derive(Deserialize)]
pub struct KeyForm {
    name: String,
    #[serde(default)]
    days: i64,
}

pub async fn create_key(State(state): State<AppState>, user: User, Form(f): Form<KeyForm>) -> Res<Markup> {
    require(user.has("api.keys"))?;
    let name = clean_name(&f.name, "Key name")?;
    let (count,): (i64,) = query_as("SELECT COUNT(*) FROM api_keys WHERE user_id = ?").bind(user.id).fetch_one(&state.db).await?;
    if count as usize >= MAX_KEYS {
        return Err(bad(format!("You already have {MAX_KEYS} keys. Revoke one first.")));
    }
    let expires = (f.days > 0).then(|| crate::db::epoch() + f.days.min(3650) * 86400);
    let token = auth::create_api_key(&state.db, user.id, &name, expires).await?;
    tracing::info!("API key '{name}' created by {}", user.username);
    keys_view(&state, &user, Some(&token)).await
}

/// Revoking needs no permission beyond owning the key, so it still works after `api.keys` is taken away.
pub async fn delete_key(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    query("DELETE FROM api_keys WHERE id = ? AND user_id = ?").bind(id).bind(user.id).execute(&state.db).await?;
    Ok(redirect("/account/keys"))
}

// ---- shared by the JSON API and the MCP server ----------------------------------------------

/// Boards the user can work on (the tools need a participant, as the agent console does).
async fn boards_for(state: &AppState, user: &User) -> Res<Vec<Value>> {
    let db: &Db = &state.db;
    let rows: Vec<(i64, String, i64, String, String)> = query_as(
        "SELECT b.id, b.name, p.id, p.name, t.name FROM boards b JOIN projects p ON p.id = b.project_id \
         JOIN teams t ON t.id = p.team_id ORDER BY t.name, p.name, b.name",
    )
    .fetch_all(db)
    .await?;
    let mut allowed: HashMap<i64, bool> = HashMap::new();
    let mut out = Vec::new();
    for (id, name, project_id, project, team) in rows {
        let ok = match allowed.get(&project_id) {
            Some(ok) => *ok,
            None => {
                let ok = match project_access(db, user, project_id).await {
                    Ok(access) => access.participant(),
                    Err(AppError::Forbidden | AppError::NotFound) => false,
                    Err(e) => return Err(e),
                };
                allowed.insert(project_id, ok);
                ok
            }
        };
        if ok {
            out.push(json!({ "id": id, "name": name, "project": project, "team": team, "url": format!("{}/boards/{id}", state.cfg.public_url) }));
        }
    }
    Ok(out)
}

/// Run one board tool as `user` and pass any change on to a linked GitHub / GitLab board and Jira.
async fn run_tool(state: &AppState, user: &User, board_id: i64, name: &str, args: &Value) -> Res<Value> {
    let result = agent::exec_tool(&state.db, user, board_id, name, args).await?;
    if !agent::read_only(name) {
        state.sync.poke(board_id);
        crate::jira::push_board_soon(state, board_id);
    }
    Ok(result)
}

fn known_tool(name: &str) -> bool {
    agent::tools().iter().any(|t| t["function"]["name"] == name)
}

// ---- JSON API -------------------------------------------------------------------------------

fn fail(e: AppError) -> Response {
    let status = match &e {
        AppError::NotFound => StatusCode::NOT_FOUND,
        AppError::Forbidden => StatusCode::FORBIDDEN,
        AppError::Bad(_) => StatusCode::BAD_REQUEST,
        AppError::Internal(err) => {
            tracing::error!("API request failed: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (status, Json(json!({ "error": e.message() }))).into_response()
}

type Api = Result<Json<Value>, Response>;

pub async fn boards(State(state): State<AppState>, ApiUser(user): ApiUser) -> Api {
    Ok(Json(json!({ "boards": boards_for(&state, &user).await.map_err(fail)? })))
}

pub async fn board(State(state): State<AppState>, ApiUser(user): ApiUser, Path(id): Path<i64>) -> Api {
    run_tool(&state, &user, id, "get_board", &json!({})).await.map(Json).map_err(fail)
}

pub async fn tools(_: ApiUser) -> Json<Value> {
    let tools: Vec<Value> = agent::tools()
        .iter()
        .map(|t| json!({ "name": t["function"]["name"], "description": t["function"]["description"], "arguments": t["function"]["parameters"] }))
        .collect();
    Json(json!({ "tools": tools }))
}

/// `POST /api/v1/boards/{id}/tools/{name}` with the tool's arguments as a JSON object.
pub async fn call(State(state): State<AppState>, ApiUser(user): ApiUser, Path((id, name)): Path<(i64, String)>, body: Bytes) -> Api {
    let args = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| fail(bad("The request body must be a JSON object of tool arguments")))?
    };
    if !known_tool(&name) {
        return Err(fail(AppError::NotFound));
    }
    run_tool(&state, &user, id, &name, &args).await.map(Json).map_err(fail)
}

// ---- MCP server (Streamable HTTP, stateless, JSON responses) --------------------------------

const MCP_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The agent's tools with a `board_id` argument added, since an MCP session is not tied to a board.
fn mcp_tools() -> Vec<Value> {
    let mut out = vec![json!({
        "name": "list_boards",
        "description": "List the Autokan boards you can work on, with their ids. Call this first to find a board_id.",
        "inputSchema": { "type": "object", "properties": {} },
    })];
    for t in agent::tools() {
        let f = &t["function"];
        let mut schema = f["parameters"].clone();
        schema["properties"]["board_id"] = json!({ "type": "integer", "description": "Board id, from list_boards" });
        let mut required = vec![json!("board_id")];
        required.extend(schema["required"].as_array().cloned().unwrap_or_default());
        schema["required"] = json!(required);
        out.push(json!({ "name": f["name"], "description": f["description"], "inputSchema": schema }));
    }
    out
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })).into_response()
}

pub async fn mcp(State(state): State<AppState>, ApiUser(user): ApiUser, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        return rpc_error(Value::Null, -32700, "Parse error");
    };
    if !req.is_object() {
        return rpc_error(Value::Null, -32600, "Send one JSON-RPC message per request");
    }
    // No id: a notification (or a response to us, which we never solicit). Nothing to answer.
    let Some(id) = req.get("id").filter(|v| !v.is_null()).cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = &req["params"];
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => {
            let wanted = params["protocolVersion"].as_str().unwrap_or("");
            json!({
                "protocolVersion": MCP_VERSIONS.iter().find(|v| **v == wanted).unwrap_or(&MCP_VERSIONS[0]),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "autokan", "version": env!("CARGO_PKG_VERSION") },
                "instructions": format!(
                    "Autokan kanban boards at {}. You act as {} and are limited to what they may do. Call list_boards, then get_board for the columns and cards; refer to cards and columns by id.",
                    state.cfg.public_url, user.display_name
                ),
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": mcp_tools() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let empty = json!({});
            let args = if params["arguments"].is_object() { &params["arguments"] } else { &empty };
            let outcome = if name == "list_boards" {
                boards_for(&state, &user).await.map(|b| json!({ "boards": b }))
            } else if !known_tool(name) {
                return rpc_error(id, -32602, "Unknown tool");
            } else {
                match args["board_id"].as_i64().or_else(|| args["board_id"].as_str().and_then(|s| s.trim().parse().ok())) {
                    Some(board_id) => run_tool(&state, &user, board_id, name, args).await,
                    None => Err(bad("Missing integer argument 'board_id'")),
                }
            };
            match outcome {
                Ok(v) => json!({ "content": [{ "type": "text", "text": v.to_string() }], "isError": false }),
                Err(e) => {
                    if let AppError::Internal(err) = &e {
                        tracing::error!("MCP tool {name} failed: {err:#}");
                    }
                    json!({ "content": [{ "type": "text", "text": e.message() }], "isError": true })
                }
            }
        }
        _ => return rpc_error(id, -32601, "Method not found"),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}
