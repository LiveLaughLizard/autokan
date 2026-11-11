//! Board page, columns, labels and board settings.
use crate::db::{query, query_as};
use super::{redirect, REFRESH};
use crate::auth::User;
use crate::error::{bad, AppError, Res};
use crate::ops::{self, clean_name, BoardData, Card};
use crate::perm::{board_access, project_access, require, Access};
use crate::llm;
use crate::views::{avatar, crumbs, label_badge, label_class, page, panel, target_select, LABEL_COLORS};
use crate::AppState;
use crate::sync;
use axum::extract::{Path, Query, State};
use std::collections::HashMap;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Form;
use maud::{html, Markup};
use serde::Deserialize;

pub const REFRESH_CLOSE: [(&str, &str); 1] = [("HX-Trigger", r#"{"board-refresh":true,"close-modal":true}"#)];

#[derive(Deserialize)]
pub struct NameForm {
    name: String,
}

pub async fn create_board(State(state): State<AppState>, user: User, Path(pid): Path<i64>, Form(f): Form<NameForm>) -> Res<Response> {
    require(project_access(&state.db, &user, pid).await?.manage)?;
    let id = ops::create_board(&state.db, pid, &f.name).await?;
    Ok(redirect(format!("/boards/{id}")))
}

fn card_tile(data: &BoardData, card: &Card, access: &Access, user: &User) -> Markup {
    let labels = data.card_labels.get(&card.id);
    let assignees = data.card_assignees.get(&card.id);
    let locked = !access.can_edit_card(card.created_by, user.id);
    html! {
        div class={ "card-item lift card bg-base-100 shadow-sm cursor-pointer border border-base-300 " (if locked { "locked" } else { "" }) }
            data-card-id=(card.id) hx-get={ "/cards/" (card.id) } hx-target="#modal-body" {
            div class="p-3 flex flex-col gap-2" {
                @if let Some(labels) = labels {
                    div class="flex flex-wrap gap-1" { @for l in labels { (label_badge(l)) } }
                }
                div class="text-sm break-words" { (card.title) }
                @if let Some(key) = data.jira.get(&card.id) {
                    div { span class="badge badge-sm badge-soft badge-info font-mono" title="Linked Jira issue" { (key) } }
                }
                @if !card.description.is_empty() || card.comment_count > 0 || assignees.is_some() {
                    div class="flex items-center gap-2 text-xs opacity-70" {
                        @if !card.description.is_empty() { span title="Has a description" { "≡" } }
                        @if card.comment_count > 0 { span title="Comments" { "💬 " (card.comment_count) } }
                        div class="ml-auto flex -space-x-1" {
                            @if let Some(people) = assignees { @for p in people { (avatar(p)) } }
                        }
                    }
                }
            }
        }
    }
}

fn columns_view(data: &BoardData, services: &[llm::Service], access: &Access, user: &User) -> Markup {
    let board_id = data.board.id;
    html! {
        div id="columns" class="flex gap-4 items-start h-full" {
            @for col in &data.columns {
                @let cards: Vec<&Card> = data.cards.iter().filter(|c| c.column_id == col.id).collect();
                div class="column stripe w-72 shrink-0 bg-base-100/60 backdrop-blur border border-base-300 shadow-sm rounded-box flex flex-col max-h-full" data-column-id=(col.id) {
                    div class={ "column-handle flex items-center gap-1 px-3 pt-3 pb-1 " (if access.manage { "cursor-grab" } else { "" }) } {
                        h3 class="font-bold text-sm uppercase tracking-wide flex-1 truncate" title=(col.name) { (col.name) }
                        span class="badge badge-sm badge-soft badge-primary" { (cards.len()) }
                        @if access.manage {
                            button class="btn btn-ghost btn-xs btn-square" title="Edit column"
                                hx-get={ "/columns/" (col.id) "/edit" } hx-target="#modal-body" { "⋯" }
                        }
                    }
                    @if access.participant() {
                        div class="px-3 pb-1" {
                            @if !col.agent_prompt.trim().is_empty() {
                                button class="btn btn-xs btn-outline btn-primary" data-send-column=(col.id) title=(col.agent_prompt) { "✦ Send To Agent" }
                                @if let Some(svc) = services.iter().find(|s| Some(s.id) == col.agent_service_id) {
                                    div class="text-[10px] opacity-60 truncate mt-1" title="This column overrides the project's default model" { "via " (svc.name) " · " (col.agent_model) }
                                }
                            } @else if access.manage {
                                // No prompt yet: the button opens the column editor where one can be written.
                                button class="btn btn-xs btn-ghost border border-dashed border-base-content/30 opacity-70 hover:opacity-100"
                                    title="This column has no agent prompt yet. Click to write one."
                                    hx-get={ "/columns/" (col.id) "/edit" } hx-target="#modal-body" { "✦ Set agent prompt" }
                            }
                        }
                    }
                    div class="card-list flex flex-col gap-2 p-2 overflow-y-auto min-h-10 flex-1" data-column-id=(col.id) {
                        @for card in cards { (card_tile(data, card, access, user)) }
                    }
                    @if access.create_cards {
                        form class="p-2 pt-0" hx-post={ "/columns/" (col.id) "/cards" } hx-swap="none" {
                            input class="input input-sm input-ghost w-full" name="title" placeholder="+ Add a card" autocomplete="off" required;
                        }
                    }
                }
            }
            @if access.manage {
                form class="w-72 shrink-0" hx-post={ "/boards/" (board_id) "/columns" } hx-swap="none" {
                    input class="input input-sm w-full" name="name" placeholder="+ Add a column" autocomplete="off" required;
                }
            }
        }
    }
}

pub async fn board_page(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Markup> {
    let access = board_access(&state.db, &user, id).await?;
    let data = ops::load_board(&state.db, id).await?;
    let services = llm::services(&state.db).await?;
    let chat_target = llm::resolve(&state.db, access.project_id, None).await;
    let synced = sync::link(&state.db, id).await?.is_some_and(|l| l.enabled);
    let (project_name, team_name): (String, String) =
        query_as("SELECT p.name, t.name FROM projects p JOIN teams t ON t.id = p.team_id WHERE p.id = ?")
            .bind(access.project_id)
            .fetch_one(&state.db)
            .await?;
    Ok(page(
        Some(&user),
        &data.board.name,
        true,
        html! {
            div class="flex flex-col h-full" {
                div class="flex items-center gap-2 px-4 py-2 border-b border-base-300 bg-base-100/70 backdrop-blur" {
                    div class="flex-1 min-w-0" {
                        (crumbs(&[
                            (team_name.as_str(), Some(format!("/teams/{}", access.team_id))),
                            (project_name.as_str(), Some(format!("/projects/{}", access.project_id))),
                            (data.board.name.as_str(), None),
                        ]))
                    }
                    @if !access.participant() {
                        span class="badge badge-ghost" title="Join the project to create cards and comment" { "Read only" }
                    }
                    @if access.manage {
                        a class="btn btn-sm btn-ghost" href={ "/boards/" (id) "/settings" } { "Settings" }
                    }
                    @if access.participant() {
                        button class="btn btn-sm btn-glow" type="button" data-agent-toggle { "✦ Agent" }
                    }
                }
                div class="flex flex-1 min-h-0" {
                    div id="board" class="flex-1 overflow-x-auto p-4" data-board-id=(id) data-manage=(access.manage) data-synced=[synced.then_some("1")]
                        hx-get={ "/boards/" (id) "/columns" } hx-trigger="board-refresh from:body" hx-swap="innerHTML" {
                        (columns_view(&data, &services, &access, &user))
                    }
                    @if access.participant() {
                        aside id="agent-panel" class="hidden w-full max-w-md shrink-0 border-l border-base-300 bg-base-100/90 backdrop-blur shadow-2xl flex-col" data-board-id=(id) {
                            div class="flex items-center gap-2 px-3 py-2 border-b border-base-300" {
                                (crate::views::logo("w-5 h-5"))
                                div class="flex-1 min-w-0" {
                                    div class="font-semibold leading-tight" { "Agent console" }
                                    div class="text-xs opacity-60 truncate" title="Default service and model for this project" {
                                        @match &chat_target { Ok(t) => { (t.label()) }, Err(_) => { "No agent service configured" } }
                                    }
                                }
                                button class="btn btn-xs btn-ghost" type="button" data-agent-reset title="Clear this conversation" { "New chat" }
                                button class="btn btn-xs btn-ghost btn-square" type="button" data-agent-toggle { "✕" }
                            }
                            div id="agent-messages" class="flex-1 overflow-y-auto p-3 flex flex-col gap-1"
                                hx-get={ "/boards/" (id) "/agent/history" } hx-trigger="load, agent-history from:body" {}
                            form id="agent-form" class="p-3 border-t border-base-300 flex flex-col gap-2" {
                                textarea class="textarea w-full text-sm" name="message" rows="3"
                                    placeholder="Tell the agent what to do with cards or columns…  (Enter to send, Shift+Enter for a new line)" {}
                                div class="flex items-center gap-2" {
                                    span class="text-xs opacity-60 flex-1" { "The agent acts with your permissions on this board." }
                                    button class="btn btn-sm btn-primary" { "Send" }
                                }
                            }
                        }
                    }
                }
                @if access.participant() {
                    // One tab per "Send To Agent" run, added by app.js; a tab opens that run's console.
                    div id="agent-dock" class="hidden shrink-0 border-t border-base-300 bg-base-100/90 backdrop-blur" {
                        div id="agent-dock-console" class="hidden h-64 overflow-y-auto p-3 font-mono text-xs border-b border-base-300" {}
                        div id="agent-dock-tabs" class="flex items-center gap-1 px-2 py-1 overflow-x-auto" {}
                    }
                }
            }
            dialog id="modal" class="modal" {
                div class="modal-box max-w-3xl" { div id="modal-body" {} }
                form method="dialog" class="modal-backdrop" { button { "close" } }
            }
        },
    ))
}

pub async fn columns_fragment(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Markup> {
    let access = board_access(&state.db, &user, id).await?;
    let data = ops::load_board(&state.db, id).await?;
    let services = llm::services(&state.db).await?;
    Ok(columns_view(&data, &services, &access, &user))
}

pub async fn create_column(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<NameForm>) -> Res<Response> {
    require(board_access(&state.db, &user, id).await?.manage)?;
    ops::create_column(&state.db, id, &f.name).await?;
    state.sync.poke(id);
    Ok(REFRESH.into_response())
}

#[derive(Deserialize)]
pub struct OrderForm {
    /// Comma separated column ids in their new order.
    ids: String,
}

pub async fn order_columns(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<OrderForm>) -> Res<Response> {
    require(board_access(&state.db, &user, id).await?.manage)?;
    let ids: Vec<i64> = f.ids.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    ops::set_column_order(&state.db, id, &ids).await?;
    state.sync.poke(id);
    Ok(REFRESH.into_response())
}

pub async fn column_modal(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Markup> {
    let col = ops::get_column(&state.db, id).await?;
    let access = board_access(&state.db, &user, col.board_id).await?;
    require(access.manage)?;
    let services = llm::services(&state.db).await?;
    let inherit = match llm::resolve(&state.db, access.project_id, None).await {
        Ok(t) => format!("Project default ({})", t.label()),
        Err(_) => "Project default (none configured yet)".to_string(),
    };
    Ok(html! {
        h3 class="text-lg font-bold mb-3" { "Edit column" }
        form class="flex flex-col gap-3" hx-post={ "/columns/" (id) } hx-swap="none" {
            label class="flex flex-col gap-1" {
                span class="text-sm font-medium" { "Name" }
                input class="input w-full" name="name" value=(col.name) required;
            }
            label class="flex flex-col gap-1" {
                span class="text-sm font-medium" { "Agent prompt" }
                textarea class="textarea w-full text-sm" name="agent_prompt" rows="5"
                    placeholder="What should the agent do with the cards in this column? e.g. “Break each card down into smaller tasks and move the originals to Human Review.”" { (col.agent_prompt) }
                span class="text-xs opacity-60" { "Used by this column's “Send To Agent” button, which sends every card in the column together with this prompt." }
            }
            label class="flex flex-col gap-1" {
                span class="text-sm font-medium" { "Service and model for this prompt" }
                (target_select("agent_target", &services, &llm::target_value(col.agent_service_id, &col.agent_model), &inherit))
                span class="text-xs opacity-60" { "Only affects this column's “Send To Agent”. The agent console keeps using the project default." }
            }
            div class="flex gap-2" {
                button class="btn btn-primary btn-sm" { "Save" }
                button class="btn btn-sm" type="button" data-close-modal { "Cancel" }
                div class="flex-1" {}
                button class="btn btn-error btn-outline btn-sm" type="button" hx-post={ "/columns/" (id) "/delete" } hx-swap="none"
                    hx-confirm="Delete this column and every card in it?" { "Delete column" }
            }
        }
    })
}

#[derive(Deserialize)]
pub struct ColumnForm {
    name: String,
    #[serde(default)]
    agent_prompt: String,
    #[serde(default)]
    agent_target: String,
}

pub async fn update_column(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<ColumnForm>) -> Res<Response> {
    let col = ops::get_column(&state.db, id).await?;
    require(board_access(&state.db, &user, col.board_id).await?.manage)?;
    let (service_id, model) = llm::parse_target(&state.db, &f.agent_target).await?;
    query("UPDATE columns SET name = ?, agent_prompt = ?, agent_service_id = ?, agent_model = ? WHERE id = ?")
        .bind(clean_name(&f.name, "Column name")?)
        .bind(f.agent_prompt.trim())
        .bind(service_id)
        .bind(model)
        .bind(id)
        .execute(&state.db)
        .await?;
    state.sync.poke(col.board_id);
    Ok(REFRESH_CLOSE.into_response())
}

pub async fn delete_column(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    let col = ops::get_column(&state.db, id).await?;
    require(board_access(&state.db, &user, col.board_id).await?.manage)?;
    ops::delete_column(&state.db, id).await?;
    state.sync.poke(col.board_id);
    Ok(REFRESH_CLOSE.into_response())
}

pub async fn settings_page(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
) -> Res<Markup> {
    let access = board_access(&state.db, &user, id).await?;
    require(access.manage)?;
    let board = ops::get_board(&state.db, id).await?;
    let labels = ops::labels(&state.db, id).await?;
    let effective = llm::resolve(&state.db, access.project_id, None).await;
    let link = sync::link(&state.db, id).await?;
    let base = format!("/boards/{id}");
    Ok(page(
        Some(&user),
        "Board settings",
        false,
        html! {
            (crumbs(&[(board.name.as_str(), Some(base.clone())), ("Settings", None)]))
            (panel("Labels", html! {
                div class="flex flex-wrap gap-2" {
                    @for l in &labels {
                        form method="post" action={ "/labels/" (l.id) "/delete" } class="inline-flex items-center" {
                            span class={ "badge border-0 gap-1 " (label_class(&l.color)) } {
                                (l.name)
                                button class="cursor-pointer opacity-80 hover:opacity-100" title="Delete label" { "✕" }
                            }
                        }
                    }
                    @if labels.is_empty() { span class="text-sm opacity-60" { "No labels yet." } }
                }
                form method="post" action={ (base) "/labels" } class="join" {
                    input class="input input-sm join-item" name="name" placeholder="Label name" required;
                    select class="select select-sm join-item w-28" name="color" {
                        @for (c, _) in LABEL_COLORS { option value=(c) { (c) } }
                    }
                    button class="btn btn-sm btn-primary join-item" { "Add label" }
                }
            }))
            form method="post" action={ (base) "/settings" } class="contents" {
                (panel("Board", html! {
                    label class="flex flex-col gap-1 max-w-xl" {
                        span class="text-sm font-medium" { "Name" }
                        input class="input w-full" name="name" value=(board.name) required;
                    }
                }))
                (panel("Agent", html! {
                    p class="text-sm opacity-70" {
                        "This board's agent console uses "
                        @match &effective {
                            Ok(t) => { b { (t.label()) } " (set by the " (t.source) ")." },
                            Err(_) => { b { "no service yet" } "." },
                        }
                        " Change the default on the "
                        a class="link" href={ "/projects/" (access.project_id) } { "project page" }
                        ", or override it per column in the column's ⋯ menu."
                    }
                    label class="flex flex-col gap-1 max-w-3xl" {
                        span class="text-sm font-medium" { "Extra instructions for the agent on this board" }
                        textarea class="textarea w-full text-sm" name="agent_system_prompt" rows="4" { (board.agent_system_prompt) }
                    }
                    div { button class="btn btn-primary btn-sm" { "Save settings" } }
                }))
            }
            (panel("Synchronization", html! {
                @if let Some(m) = q.get("msg") { div role="alert" class="alert alert-success text-sm" { (m) } }
                @if let Some(m) = q.get("error") { div role="alert" class="alert alert-error text-sm" { (m) } }
                @if let Some(link) = &link {
                    p class="text-sm" {
                        "Linked to " b { (sync::describe(link)) } ". "
                        @match &link.last_sync {
                            Some(at) => { "Last synced " (crate::views::timestamp(at)) "." },
                            None => { "Not synced yet." },
                        }
                    }
                    @if !link.last_error.is_empty() {
                        div role="alert" class="alert alert-warning text-sm" { "The last sync failed: " (link.last_error) }
                    }
                    p class="text-sm opacity-70" {
                        "Columns, card titles and descriptions, which column a card is in, and the order of cards are kept the same on both sides. Changes made here are sent within a few seconds; changes made there are picked up about once a minute."
                    }
                    div class="flex gap-2" {
                        form method="post" action={ (base) "/sync/now" } { button class="btn btn-sm btn-primary" { "Sync now" } }
                        @if access.team_admin {
                            form method="post" action={ (base) "/sync/unlink" }
                                data-confirm="Stop synchronizing this board? Cards and columns stay as they are on both sides." {
                                button class="btn btn-sm btn-outline" { "Unlink" }
                            }
                        }
                    }
                } @else if access.team_admin {
                    p class="text-sm opacity-70" {
                        "Keep this board in step with a GitHub project or a GitLab issue board. When you link, columns with the same name are paired, every other column and card is copied across in both directions, and from then on a change on one side is repeated on the other."
                    }
                    form method="post" action={ (base) "/sync/link" } class="grid md:grid-cols-2 gap-4 max-w-3xl" data-toggle-scope {
                        label class="flex flex-col gap-1" {
                            span class="text-sm font-medium" { "Service" }
                            select class="select w-full" name="provider" data-show-for {
                                @for (key, name) in sync::PROVIDERS { option value=(key) { (name) } }
                            }
                        }
                        label class="flex flex-col gap-1" {
                            span class="text-sm font-medium" { "Access token" }
                            input class="input w-full" type="password" name="token" autocomplete="off" required;
                            span class="text-xs opacity-60" { "GitHub: a token with the “project” scope (plus “repo” to edit issues). GitLab: a token with the “api” scope." }
                        }
                        label class="flex flex-col gap-1" data-provider="github" {
                            span class="text-sm font-medium" { "Organization or user" }
                            input class="input w-full" name="github_owner" placeholder="my-org";
                        }
                        label class="flex flex-col gap-1" data-provider="github" {
                            span class="text-sm font-medium" { "Project number" }
                            input class="input w-full" name="github_number" placeholder="the number in the project's URL";
                            span class="text-xs opacity-60" { "The project needs a single-select “Status” field; its options are the columns." }
                        }
                        label class="flex flex-col gap-1 hidden" data-provider="gitlab" {
                            span class="text-sm font-medium" { "Project path" }
                            input class="input w-full" name="gitlab_project" placeholder="my-group/my-project";
                        }
                        label class="flex flex-col gap-1 hidden" data-provider="gitlab" {
                            span class="text-sm font-medium" { "Board name (optional)" }
                            input class="input w-full" name="gitlab_board" placeholder="the project's first board";
                        }
                        label class="flex flex-col gap-1 md:col-span-2" {
                            span class="text-sm font-medium" { "Server URL (optional)" }
                            input class="input w-full" name="base_url" placeholder="leave blank for github.com / gitlab.com";
                            span class="text-xs opacity-60" { "For GitHub Enterprise Server or a self-managed GitLab, the address you open in the browser, e.g. https://gitlab.example.com" }
                        }
                        div class="md:col-span-2" { button class="btn btn-primary btn-sm" { "Link and sync" } }
                    }
                } @else {
                    p class="text-sm opacity-60" { "This board is not linked to GitHub or GitLab. A team admin can link it." }
                }
            }))
            (panel("Danger zone", html! {
                form method="post" action={ (base) "/delete" } data-confirm="Delete this board and all of its cards?" {
                    button class="btn btn-sm btn-error btn-outline" { "Delete board" }
                }
            }))
        },
    ))
}

#[derive(Deserialize)]
pub struct SettingsForm {
    name: String,
    #[serde(default)]
    agent_system_prompt: String,
}

pub fn check_base_url(url: &str) -> Res<()> {
    if url.is_empty() || url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(bad("Base URL must start with http:// or https://"))
    }
}

pub async fn save_settings(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<SettingsForm>) -> Res<Response> {
    let access = board_access(&state.db, &user, id).await?;
    require(access.manage)?;
    query("UPDATE boards SET name = ?, agent_system_prompt = ? WHERE id = ?")
        .bind(ops::unique_name(&state.db, ops::Named::Board { project_id: access.project_id }, &f.name, Some(id)).await?)
        .bind(f.agent_system_prompt.trim())
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/boards/{id}/settings")))
}

pub async fn delete_board(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    let access = board_access(&state.db, &user, id).await?;
    require(access.manage)?;
    ops::delete_board(&state.db, id).await?;
    Ok(redirect(format!("/projects/{}", access.project_id)))
}

#[derive(Deserialize)]
pub struct LabelForm {
    name: String,
    color: String,
}

pub async fn create_label(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<LabelForm>) -> Res<Response> {
    require(board_access(&state.db, &user, id).await?.manage)?;
    if !LABEL_COLORS.iter().any(|(c, _)| *c == f.color) {
        return Err(bad("Unknown label colour"));
    }
    query("INSERT INTO labels (board_id, name, color) VALUES (?, ?, ?)")
        .bind(id)
        .bind(clean_name(&f.name, "Label name")?)
        .bind(&f.color)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/boards/{id}/settings")))
}

pub async fn delete_label(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    let (board_id,): (i64,) = query_as("SELECT board_id FROM labels WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    require(board_access(&state.db, &user, board_id).await?.manage)?;
    ops::delete_label(&state.db, id).await?;
    Ok(redirect(format!("/boards/{board_id}/settings")))
}

// ---------------------------------------------------------------- GitHub / GitLab synchronization

fn sync_back(board_id: i64, key: &str, message: &str) -> Response {
    let q: String = url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
    redirect(format!("/boards/{board_id}/settings?{key}={q}"))
}

pub async fn link_sync(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Form(f): Form<HashMap<String, String>>,
) -> Res<Response> {
    // Linking stores a token and makes the server call an address of the admin's choosing, so it
    // is reserved for team admins rather than everyone who can manage the board.
    require(board_access(&state.db, &user, id).await?.team_admin)?;
    let field = |k: &str| f.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
    let (provider, base_url, token) = (field("provider"), field("base_url").trim_end_matches('/').to_string(), field("token"));
    if !sync::PROVIDERS.iter().any(|p| p.0 == provider) {
        return Err(bad("Unknown service"));
    }
    check_base_url(&base_url)?;
    if token.is_empty() {
        return Ok(sync_back(id, "error", "An access token is required"));
    }
    let target = match sync::resolve_target(&state, &provider, &base_url, &token, &f).await {
        Ok(t) => t,
        Err(e) => return Ok(sync_back(id, "error", &e)),
    };
    for sql in ["DELETE FROM sync_cards WHERE board_id = ?", "DELETE FROM sync_columns WHERE board_id = ?", "DELETE FROM board_sync WHERE board_id = ?"] {
        query(sql).bind(id).execute(&state.db).await?;
    }
    query("INSERT INTO board_sync (board_id, provider, base_url, token, target, user_id, enabled) VALUES (?, ?, ?, ?, ?, ?, 1)")
        .bind(id)
        .bind(&provider)
        .bind(&base_url)
        .bind(&token)
        .bind(&target)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    tracing::warn!("board {id} linked to {provider} by {}", user.username);
    Ok(match sync::sync_board(&state, id).await {
        Ok(s) => sync_back(id, "msg", &format!("Linked. {}", s.describe())),
        Err(e) => sync_back(id, "error", &format!("Linked, but the first sync failed: {e}")),
    })
}

pub async fn sync_now(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    require(board_access(&state.db, &user, id).await?.manage)?;
    Ok(match sync::sync_board(&state, id).await {
        Ok(s) => sync_back(id, "msg", &s.describe()),
        Err(e) => sync_back(id, "error", &format!("Sync failed: {e}")),
    })
}

pub async fn unlink_sync(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    require(board_access(&state.db, &user, id).await?.team_admin)?;
    for sql in ["DELETE FROM sync_cards WHERE board_id = ?", "DELETE FROM sync_columns WHERE board_id = ?", "DELETE FROM board_sync WHERE board_id = ?"] {
        query(sql).bind(id).execute(&state.db).await?;
    }
    Ok(sync_back(id, "msg", "Unlinked. Nothing was deleted on either side."))
}
