//! Cards, comments, labels and assignees. Everything renders into the card modal.
use crate::db::{query, query_as};
use super::board::REFRESH_CLOSE;
use super::REFRESH;
use crate::auth::User;
use crate::error::{AppError, Res};
use crate::markdown;
use crate::ops::{self, Card};
use crate::perm::{board_access, require, Access};
use crate::views::{editor, label_badge, label_class, timestamp};
use crate::AppState;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Form;
use maud::{html, Markup};
use serde::Deserialize;

async fn ctx(state: &AppState, user: &User, card_id: i64) -> Res<(Card, Access)> {
    let card = ops::get_card(&state.db, card_id).await?;
    let access = board_access(&state.db, user, card.board_id).await?;
    Ok((card, access))
}

#[derive(Deserialize)]
pub struct CreateForm {
    title: String,
}

pub async fn create(State(state): State<AppState>, user: User, Path(column_id): Path<i64>, Form(f): Form<CreateForm>) -> Res<Response> {
    let column = ops::get_column(&state.db, column_id).await?;
    require(board_access(&state.db, &user, column.board_id).await?.create_cards)?;
    ops::create_card(&state.db, &column, user.id, &f.title, "").await?;
    state.sync.poke(column.board_id);
    Ok(REFRESH.into_response())
}

async fn render(state: &AppState, user: &User, card_id: i64) -> Res<Markup> {
    let (card, access) = ctx(state, user, card_id).await?;
    let db = &state.db;
    let can_edit = access.can_edit_card(card.created_by, user.id);
    let column = ops::get_column(db, card.column_id).await?;
    let all_labels = ops::labels(db, card.board_id).await?;
    let labels = ops::card_labels(db, card.id).await?;
    let members = ops::project_members(db, access.project_id).await?;
    let assignees = ops::card_assignees(db, card.id).await?;
    let comments: Vec<(i64, Option<i64>, String, String, Option<String>, Option<String>)> = query_as(
        "SELECT m.id, m.user_id, m.body, m.created_at, m.updated_at, u.display_name FROM comments m \
         LEFT JOIN users u ON u.id = m.user_id WHERE m.card_id = ? ORDER BY m.id",
    )
    .bind(card.id)
    .fetch_all(db)
    .await?;
    let base = format!("/cards/{}", card.id);
    let heading = "text-xs font-semibold uppercase tracking-wide opacity-60";

    Ok(html! {
        div class="flex flex-col gap-5" hx-target="#modal-body" {
            div class="flex flex-col gap-1" {
                div class="flex items-start gap-2" {
                    @if can_edit {
                        form class="join flex-1" hx-post=(base) {
                            input class="input join-item flex-1 font-semibold" name="title" value=(card.title) required;
                            button class="btn join-item" { "Save" }
                        }
                    } @else {
                        h3 class="text-lg font-bold flex-1 break-words" { (card.title) }
                    }
                    button class="btn btn-ghost btn-square" type="button" data-close-modal title="Close" { "✕" }
                }
                div class="text-xs opacity-60" {
                    "Column: " span class="font-medium" { (column.name) }
                    " · created by " (card.author.as_deref().unwrap_or("a former user")) " · " (timestamp(&card.created_at))
                }
            }

            @if can_edit || !labels.is_empty() {
                section class="flex flex-col gap-2" {
                    div class=(heading) { "Labels" }
                    div class="flex flex-wrap gap-1" {
                        @if can_edit {
                            @for l in &all_labels {
                                @let on = labels.iter().any(|x| x.id == l.id);
                                button type="button" hx-post={ (base) "/labels/" (l.id) } title=(if on { "Remove label" } else { "Apply label" })
                                    class={ "badge border-0 cursor-pointer " (label_class(&l.color)) (if on { "" } else { " opacity-30 hover:opacity-60" }) } {
                                    (l.name)
                                }
                            }
                            @if all_labels.is_empty() {
                                span class="text-sm opacity-60" { "This board has no labels yet (board settings → Labels)." }
                            }
                        } @else {
                            @for l in &labels { (label_badge(l)) }
                        }
                    }
                }
            }

            @if can_edit || !assignees.is_empty() {
                section class="flex flex-col gap-2" {
                    div class=(heading) { "Assigned to" }
                    div class="flex flex-wrap gap-1" {
                        @if can_edit {
                            @for m in &members {
                                @let on = assignees.iter().any(|a| a.id == m.id);
                                button type="button" hx-post={ (base) "/assignees/" (m.id) }
                                    class={ "btn btn-xs " (if on { "btn-primary" } else { "btn-outline opacity-60" }) } {
                                    (if on { "✓ " } else { "" }) (m.display_name)
                                }
                            }
                            @if members.is_empty() { span class="text-sm opacity-60" { "Nobody has joined this project yet." } }
                        } @else {
                            @for a in &assignees { span class="badge badge-neutral" { (a.display_name) } }
                        }
                    }
                }
            }

            section class="flex flex-col gap-2" data-toggle-scope {
                div class="flex items-center gap-2" {
                    div class=(heading) { "Description" }
                    @if can_edit {
                        button class="btn btn-ghost btn-xs" type="button" data-toggle=".desc-toggle" { "Edit" }
                    }
                }
                div class="desc-toggle" {
                    @if card.description.trim().is_empty() {
                        p class="text-sm opacity-60" { "No description." }
                    } @else {
                        (markdown::render(&card.description))
                    }
                }
                @if can_edit {
                    div class="desc-toggle hidden" {
                        form class="flex flex-col gap-2" hx-post=(base) {
                            (editor(card.board_id, "description", &card.description, "Describe this card…", 8))
                            div class="flex gap-2" {
                                button class="btn btn-primary btn-sm" { "Save description" }
                                button class="btn btn-sm" type="button" data-toggle=".desc-toggle" { "Cancel" }
                            }
                        }
                    }
                }
            }

            section class="flex flex-col gap-3" {
                div class=(heading) { "Comments" }
                @for (cid, author_id, body, created, updated, author) in &comments {
                    @let mine = *author_id == Some(user.id);
                    div class="bg-base-200 rounded-box p-3 flex flex-col gap-2" data-toggle-scope {
                        div class="flex items-center gap-2 text-xs" {
                            span class="font-semibold" { (author.as_deref().unwrap_or("Former user")) }
                            span class="opacity-60" { (timestamp(created)) @if updated.is_some() { " · edited" } }
                            div class="flex-1" {}
                            @if mine && access.comment {
                                button class="btn btn-ghost btn-xs" type="button" data-toggle=".comment-toggle" { "Edit" }
                            }
                            @if mine || access.manage {
                                button class="btn btn-ghost btn-xs text-error" type="button" hx-post={ "/comments/" (cid) "/delete" }
                                    hx-confirm="Delete this comment?" { "Delete" }
                            }
                        }
                        div class="comment-toggle" { (markdown::render(body)) }
                        @if mine && access.comment {
                            div class="comment-toggle hidden" {
                                form class="flex flex-col gap-2" hx-post={ "/comments/" (cid) } {
                                    (editor(card.board_id, "body", body, "", 4))
                                    div class="flex gap-2" {
                                        button class="btn btn-primary btn-sm" { "Save" }
                                        button class="btn btn-sm" type="button" data-toggle=".comment-toggle" { "Cancel" }
                                    }
                                }
                            }
                        }
                    }
                }
                @if comments.is_empty() { p class="text-sm opacity-60" { "No comments yet." } }
                @if access.comment {
                    form class="flex flex-col gap-2" hx-post={ (base) "/comments" } {
                        (editor(card.board_id, "body", "", "Write a comment…", 3))
                        div { button class="btn btn-primary btn-sm" { "Comment" } }
                    }
                }
            }

            @if can_edit {
                div class="flex justify-end border-t border-base-300 pt-3" {
                    button class="btn btn-error btn-outline btn-sm" type="button" hx-post={ (base) "/delete" } hx-swap="none"
                        hx-confirm="Delete this card?" { "Delete card" }
                }
            }
        }
    })
}

pub async fn modal(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Markup> {
    render(&state, &user, id).await
}

async fn rerender(state: &AppState, user: &User, card_id: i64) -> Res<Response> {
    Ok((REFRESH, render(state, user, card_id).await?).into_response())
}

#[derive(Deserialize)]
pub struct UpdateForm {
    title: Option<String>,
    description: Option<String>,
}

pub async fn update(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<UpdateForm>) -> Res<Response> {
    let (card, access) = ctx(&state, &user, id).await?;
    require(access.can_edit_card(card.created_by, user.id))?;
    ops::update_card(&state.db, id, f.title.as_deref(), f.description.as_deref()).await?;
    state.sync.poke(card.board_id);
    rerender(&state, &user, id).await
}

#[derive(Deserialize)]
pub struct MoveForm {
    column_id: i64,
    position: usize,
}

pub async fn move_card(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<MoveForm>) -> Res<Response> {
    let (card, access) = ctx(&state, &user, id).await?;
    require(access.can_edit_card(card.created_by, user.id))?;
    ops::move_card(&state.db, &card, f.column_id, Some(f.position)).await?;
    state.sync.poke(card.board_id);
    Ok(REFRESH.into_response())
}

pub async fn delete(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    let (card, access) = ctx(&state, &user, id).await?;
    require(access.can_edit_card(card.created_by, user.id))?;
    ops::delete_card(&state.db, id).await?;
    state.sync.poke(card.board_id);
    Ok(REFRESH_CLOSE.into_response())
}

pub async fn toggle_label(State(state): State<AppState>, user: User, Path((id, label_id)): Path<(i64, i64)>) -> Res<Response> {
    let (card, access) = ctx(&state, &user, id).await?;
    require(access.can_edit_card(card.created_by, user.id))?;
    ops::set_card_label(&state.db, &card, label_id, None).await?;
    rerender(&state, &user, id).await
}

pub async fn toggle_assignee(State(state): State<AppState>, user: User, Path((id, uid)): Path<(i64, i64)>) -> Res<Response> {
    let (card, access) = ctx(&state, &user, id).await?;
    require(access.can_edit_card(card.created_by, user.id))?;
    ops::set_card_assignee(&state.db, &card, access.project_id, uid, None).await?;
    rerender(&state, &user, id).await
}

#[derive(Deserialize)]
pub struct CommentForm {
    body: String,
}

pub async fn add_comment(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<CommentForm>) -> Res<Response> {
    let (card, access) = ctx(&state, &user, id).await?;
    require(access.comment)?;
    ops::add_comment(&state.db, card.id, user.id, &f.body).await?;
    rerender(&state, &user, id).await
}

/// Returns (card id, author id) for a comment.
async fn comment_ctx(state: &AppState, comment_id: i64) -> Res<(i64, Option<i64>)> {
    query_as("SELECT card_id, user_id FROM comments WHERE id = ?")
        .bind(comment_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn edit_comment(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<CommentForm>) -> Res<Response> {
    let (card_id, author) = comment_ctx(&state, id).await?;
    let (_, access) = ctx(&state, &user, card_id).await?;
    require(author == Some(user.id) && access.comment)?;
    if f.body.trim().is_empty() {
        return Err(AppError::Bad("Comment cannot be empty".into()));
    }
    query("UPDATE comments SET body = ?, updated_at = ? WHERE id = ?")
        .bind(&f.body)
        .bind(crate::db::now())
        .bind(id)
        .execute(&state.db)
        .await?;
    rerender(&state, &user, card_id).await
}

pub async fn delete_comment(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    let (card_id, author) = comment_ctx(&state, id).await?;
    let (_, access) = ctx(&state, &user, card_id).await?;
    require(access.manage || (author == Some(user.id) && access.participant()))?;
    query("DELETE FROM comments WHERE id = ?").bind(id).execute(&state.db).await?;
    rerender(&state, &user, card_id).await
}
