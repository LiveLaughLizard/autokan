//! Agent console endpoints: history fragment, streaming chat, and per-column "Send To Agent".
use crate::db::query;
use crate::agent;
use crate::auth::User;
use crate::error::{bad, Res};
use crate::markdown;
use crate::ops;
use crate::perm::{board_access, require};
use crate::AppState;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Form;
use futures_util::{Stream, StreamExt};
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::Value;
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;

pub async fn history(State(state): State<AppState>, user: User, Path(board_id): Path<i64>) -> Res<Markup> {
    require(board_access(&state.db, &user, board_id).await?.participant())?;
    let messages = agent::history(&state.db, board_id, user.id).await?;
    Ok(html! {
        @if messages.is_empty() {
            div class="text-sm opacity-60 p-2 flex flex-col gap-2" {
                p { "Ask the agent to work on this board, for example:" }
                ul class="list-disc pl-5" {
                    li { "“Create cards for the launch checklist in Todo.”" }
                    li { "“Move everything in Human Review that I approved in the comments to Done.”" }
                    li { "“Add a Blocked column after In Progress.”" }
                }
                p { "Each column's “Send To Agent” button sends that column's cards with its saved prompt." }
            }
        }
        @for m in &messages {
            @match m["role"].as_str().unwrap_or("") {
                "user" => {
                    div class="chat chat-end" {
                        div class="chat-bubble chat-bubble-primary text-sm" {
                            @if let Some(d) = m["display"].as_str() {
                                div class="whitespace-pre-wrap break-words" { (d.replace("**", "")) }
                            } @else {
                                div class="whitespace-pre-wrap break-words" { (m["content"].as_str().unwrap_or("")) }
                            }
                        }
                    }
                }
                "assistant" => {
                    @if let Some(text) = m["content"].as_str().filter(|t| !t.trim().is_empty()) {
                        div class="chat chat-start" {
                            div class="chat-bubble bg-base-200 text-base-content text-sm" { (markdown::render(text)) }
                            @if let Some(model) = m["model"].as_str() { div class="chat-footer text-[10px] opacity-50 mt-0.5" { (model) } }
                        }
                    }
                    @for tc in m["tool_calls"].as_array().into_iter().flatten() {
                        (tool_line(tc["function"]["name"].as_str().unwrap_or("?"), tc["function"]["arguments"].as_str().unwrap_or("")))
                    }
                }
                "tool" => {
                    @let content = m["content"].as_str().unwrap_or("");
                    @if content.contains("\"ok\":false") {
                        div class="text-xs text-error font-mono pl-4 break-words" { "↳ " (content) }
                    }
                }
                _ => {}
            }
        }
    })
}

fn tool_line(name: &str, arguments: &str) -> Markup {
    html! {
        div class="text-xs font-mono opacity-70 pl-2 break-words" { "🔧 " (name) " " span class="opacity-70" { (arguments) } }
    }
}

fn stream(state: AppState, user: User, board_id: i64, content: String, display: Option<String>, column: Option<ops::Column>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Value>(64);
    tokio::spawn(async move {
        let sync = state.sync.clone();
        let jira_state = state.clone();
        agent::run(state, user, board_id, content, display, column, tx).await;
        // The agent may have changed the board; pass that on to a linked GitHub / GitLab board.
        sync.poke(board_id);
        crate::jira::push_board_soon(&jira_state, board_id);
    });
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(ReceiverStream::new(rx).map(|v| Ok(Event::default().data(v.to_string()))));
    Sse::new(events).keep_alive(KeepAlive::default()).into_response()
}

#[derive(Deserialize)]
pub struct ChatForm {
    message: String,
}

pub async fn chat(State(state): State<AppState>, user: User, Path(board_id): Path<i64>, Form(f): Form<ChatForm>) -> Res<Response> {
    require(board_access(&state.db, &user, board_id).await?.participant())?;
    let message = f.message.trim().to_string();
    if message.is_empty() {
        return Err(bad("Message cannot be empty"));
    }
    Ok(stream(state, user, board_id, message, None, None))
}

pub async fn send_column(State(state): State<AppState>, user: User, Path(column_id): Path<i64>) -> Res<Response> {
    let column = ops::get_column(&state.db, column_id).await?;
    require(board_access(&state.db, &user, column.board_id).await?.participant())?;
    if column.agent_prompt.trim().is_empty() {
        return Err(bad("This column has no agent prompt yet"));
    }
    let (content, display) = agent::column_message(&state.db, &column).await?;
    let board_id = column.board_id;
    Ok(stream(state, user, board_id, content, Some(display), Some(column)))
}

pub async fn reset(State(state): State<AppState>, user: User, Path(board_id): Path<i64>) -> Res<Response> {
    require(board_access(&state.db, &user, board_id).await?.participant())?;
    query("DELETE FROM agent_messages WHERE board_id = ? AND user_id = ?")
        .bind(board_id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok([("HX-Trigger", "agent-history")].into_response())
}
