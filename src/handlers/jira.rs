//! Jira: the project's connection settings, and the per-card panel (create issue, status, moves).
use super::redirect;
use crate::auth::User;
use crate::db::{query, query_as};
use crate::error::{bad, AppError, Res};
use crate::jira::{self, Client, Settings};
use crate::ops;
use crate::perm::{board_access, project_access, require};
use crate::views::panel;
use crate::AppState;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Form;
use maud::{html, Markup};
use serde::Deserialize;

// ---------------------------------------------------------------- connection settings

/// The connection fields shared by the organization-wide form and a project's own connection.
fn connection_fields(base_url: &str, auth: &str, username: &str, has_token: bool, ca_cert: &str) -> Markup {
    html! {
        label class="flex flex-col gap-1" {
            span class="text-sm font-medium" { "Jira address" }
            input class="input w-full" name="base_url" value=(base_url) placeholder="https://your-site.atlassian.net";
            span class="text-xs opacity-60" { "Cloud: https://your-site.atlassian.net. Self-hosted: the address you open in the browser, including any path such as /jira." }
        }
        label class="flex flex-col gap-1" {
            span class="text-sm font-medium" { "Sign-in method" }
            select class="select w-full" name="auth" {
                option value="basic" selected[auth != "bearer"] { "Email + API token (Jira Cloud), or username + password" }
                option value="bearer" selected[auth == "bearer"] { "Personal access token (self-hosted Jira)" }
            }
        }
        label class="flex flex-col gap-1" {
            span class="text-sm font-medium" { "Email or username" }
            input class="input w-full" name="username" value=(username) placeholder="not needed with a personal access token" autocomplete="off";
        }
        label class="flex flex-col gap-1" {
            span class="text-sm font-medium" { "API token, personal access token or password" }
            input class="input w-full" type="password" name="token" autocomplete="off"
                placeholder=(if has_token { "•••••••• (leave blank to keep)" } else { "not set" });
        }
        label class="flex flex-col gap-1 md:col-span-2" {
            span class="text-sm font-medium" { "Custom CA certificate (optional)" }
            textarea class="textarea w-full font-mono text-xs" name="ca_cert" rows="3" spellcheck="false"
                placeholder="-----BEGIN CERTIFICATE-----  (for a self-hosted Jira with a private certificate authority)" { (ca_cert) }
        }
    }
}

/// Validated connection fields from either form. A stored secret is only ever sent to the address
/// it was entered for, so changing the address clears it unless a new one is given.
struct Connection {
    base_url: String,
    auth: String,
    username: String,
    token: String,
    ca_cert: String,
}

fn connection(f: &SettingsForm, old_url: &str, old_token: &str) -> Res<(Connection, Option<String>)> {
    let base_url = f.base_url.trim().trim_end_matches('/').to_string();
    super::board::check_base_url(&base_url)?;
    let moved = base_url != old_url;
    let token = if !f.token.is_empty() {
        f.token.clone()
    } else if moved {
        String::new()
    } else {
        old_token.to_string()
    };
    let auth = if f.auth == "bearer" { "bearer" } else { "basic" }.to_string();
    let username = f.username.trim().to_string();
    let ca_cert = f.ca_cert.trim().to_string();
    let problem = if let Err(e) = crate::tls::parse_ca(&ca_cert) {
        Some(e)
    } else if base_url.is_empty() {
        Some("The Jira address is required".to_string())
    } else if token.is_empty() {
        Some(if moved && !old_token.is_empty() { "The address changed, so the token must be entered again".to_string() } else { "A token or password is required".to_string() })
    } else if auth == "basic" && username.is_empty() {
        Some("An email or username is required for this sign-in method".to_string())
    } else {
        None
    };
    Ok((Connection { base_url, auth, username, token, ca_cert }, problem))
}

// ---- organization-wide connection (admin panel)

pub async fn platform_panel(state: &AppState) -> Markup {
    let p = jira::platform(&state.db).await;
    let (using,): (i64,) = query_as("SELECT COUNT(*) FROM jira_settings WHERE source = 'platform' AND enabled = 1")
        .fetch_one(&state.db)
        .await
        .unwrap_or((0,));
    panel(
        "Organization Jira",
        html! {
            p class="text-sm opacity-70" {
                "One Jira connection for the whole organization. Team admins can then switch Jira on for a project by choosing this connection and naming the Jira project their issues go to, without handling credentials. A project can still configure its own connection instead."
            }
            p class="text-sm" {
                @if using == 1 { "1 project uses this connection." } @else { (using) " projects use this connection." }
            }
            form method="post" action="/admin/jira" class="grid md:grid-cols-2 gap-4 max-w-3xl" {
                (connection_fields(&p.base_url, &p.auth, &p.username, !p.token.is_empty(), &p.ca_cert))
                label class="flex items-center gap-3 md:col-span-2" {
                    input type="checkbox" class="toggle toggle-primary" name="enabled" checked[p.enabled];
                    span { "Make this connection available to projects" }
                }
                div class="md:col-span-2" { button class="btn btn-primary btn-sm" { "Save and test" } }
            }
            p class="text-xs opacity-60" {
                "The account needs permission to create and edit issues, comment, attach files and move issues in every Jira project that teams will use. Turning the connection off pauses Jira for the projects that use it."
            }
        },
    )
}

pub async fn save_platform(State(state): State<AppState>, user: User, Form(f): Form<SettingsForm>) -> Res<Response> {
    require(user.has("admin.jira"))?;
    let old = jira::platform(&state.db).await;
    let (c, problem) = connection(&f, &old.base_url, &old.token)?;
    let mut p = jira::Platform { enabled: f.enabled.is_some(), base_url: c.base_url, auth: c.auth, username: c.username, token: c.token, ca_cert: c.ca_cert };
    let mut outcome: Result<String, String> = Ok("Saved. The organization's Jira connection is switched off.".into());
    if p.enabled {
        outcome = match problem {
            Some(e) => Err(e),
            None => match Client::new(&state, &Settings::default().with_platform(&p)) {
                Ok(client) => client.check_account().await.map(|who| format!("Saved. {who} Team admins can now use it for their projects.")),
                Err(e) => Err(e),
            },
        };
    }
    if outcome.is_err() {
        p.enabled = false;
    }
    crate::db::set_setting(&state.db, "jira", &p).await?;
    tracing::warn!("organization Jira connection changed by {} (enabled: {})", user.username, p.enabled);
    let (key, message) = match outcome {
        Ok(m) => ("msg", m),
        Err(e) => ("error", format!("Saved, but the connection was left switched off: {e}")),
    };
    let q: String = url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
    Ok(redirect(format!("/admin?tab=jira&{key}={q}")))
}

// ---- per-project settings

/// The Jira panel on the project page. Only team admins can change it: it either stores
/// credentials or spends the organization's.
pub async fn settings_panel(state: &AppState, project_id: i64, team_admin: bool) -> Res<Markup> {
    let stored = jira::stored_settings(&state.db, project_id).await?.unwrap_or_default();
    let platform = jira::platform(&state.db).await;
    let effective = jira::settings(&state.db, project_id).await?.unwrap_or_default();
    // Offer the organization's connection when it exists; keep showing it for a project already on it.
    let offer_platform = platform.usable() || stored.uses_platform();
    let on_platform = stored.uses_platform() || (offer_platform && stored.base_url.is_empty() && !stored.enabled);
    let base = format!("/projects/{project_id}/jira");
    Ok(panel(
        "Jira",
        html! {
            @if !team_admin {
                p class="text-sm opacity-70" {
                    @if effective.usable() {
                        "Cards in this project can create issues in the Jira project " b { (effective.project_key) } " at " (effective.base_url) "."
                    } @else {
                        "Jira is not connected. A team admin can connect it."
                    }
                }
            } @else {
                p class="text-sm opacity-70" {
                    "Connect this project to Jira so that a card can create a Jira issue. The issue links back to the card and receives the card's title, description, comments and attached files; its status can be changed from the card."
                }
                @if stored.uses_platform() && !platform.usable() {
                    div role="alert" class="alert alert-warning text-sm" {
                        "This project uses the organization's Jira connection, which is currently switched off, so Jira is paused here."
                    }
                }
                form method="post" action=(base) class="grid md:grid-cols-2 gap-4 max-w-3xl" {
                    @if offer_platform {
                        label class="flex flex-col gap-1 md:col-span-2" {
                            span class="text-sm font-medium" { "Connection" }
                            select class="select w-full" name="source" data-show-for {
                                option value="platform" selected[on_platform] {
                                    "The organization's Jira"
                                    @if !platform.base_url.is_empty() { " (" (platform.base_url) ")" }
                                }
                                option value="own" selected[!on_platform] { "A connection just for this project" }
                            }
                            span class="text-xs opacity-60" { "The organization's connection is managed by the platform admins; you only choose the Jira project below." }
                        }
                    } @else {
                        input type="hidden" name="source" value="own";
                    }
                    label class="flex flex-col gap-1" {
                        span class="text-sm font-medium" { "Jira project key" }
                        input class="input w-full font-mono" name="project_key" value=(stored.project_key) placeholder="ABC";
                        span class="text-xs opacity-60" { "Issues created from this project's cards go here." }
                    }
                    label class="flex flex-col gap-1" {
                        span class="text-sm font-medium" { "Issue type" }
                        input class="input w-full" name="issue_type" value=(if stored.issue_type.is_empty() { "Task" } else { stored.issue_type.as_str() });
                    }
                    // Only for the project's own connection (hidden while the organization's is chosen).
                    div class={ "md:col-span-2 " (if offer_platform && on_platform { "hidden" } else { "" }) } data-provider="own" {
                        div class="grid md:grid-cols-2 gap-4" {
                            (connection_fields(&stored.base_url, &stored.auth, &stored.username, !stored.token.is_empty(), &stored.ca_cert))
                        }
                    }
                    label class="flex items-center gap-3 md:col-span-2" {
                        input type="checkbox" class="toggle toggle-primary" name="enabled" checked[stored.enabled];
                        span { "Enable Jira for this project" }
                    }
                    div class="md:col-span-2" { button class="btn btn-primary btn-sm" { "Save and test" } }
                }
            }
        },
    ))
}

#[derive(Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    source: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    project_key: String,
    #[serde(default)]
    issue_type: String,
    #[serde(default)]
    ca_cert: String,
    enabled: Option<String>,
}

fn project_back(project_id: i64, key: &str, message: &str) -> Response {
    let q: String = url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
    redirect(format!("/projects/{project_id}?jira_{key}={q}#jira"))
}

pub async fn save_settings(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<SettingsForm>) -> Res<Response> {
    require(project_access(&state.db, &user, id).await?.team_admin)?;
    let old = jira::stored_settings(&state.db, id).await?.unwrap_or_default();
    let platform = jira::platform(&state.db).await;
    let use_platform = f.source == "platform";
    let project_key = f.project_key.trim().to_uppercase();
    let issue_type = if f.issue_type.trim().is_empty() { "Task".to_string() } else { f.issue_type.trim().to_string() };
    let enabled = f.enabled.is_some();

    // What gets stored, and what (if anything) stops it from being enabled.
    let (mut cfg, mut problem) = if use_platform {
        // The project's own connection fields are kept as they were, in case it switches back.
        let cfg = Settings { project_id: id, project_key, issue_type, enabled, source: "platform".into(), ..old.clone() };
        (cfg, (!platform.usable()).then(|| "The organization's Jira connection is not available".to_string()))
    } else {
        let (c, problem) = connection(&f, &old.base_url, &old.token)?;
        let cfg = Settings {
            project_id: id,
            base_url: c.base_url,
            auth: c.auth,
            username: c.username,
            token: c.token,
            ca_cert: c.ca_cert,
            project_key,
            issue_type,
            enabled,
            source: "own".into(),
        };
        (cfg, problem)
    };
    if problem.is_none() && cfg.project_key.is_empty() {
        problem = Some("The Jira project key is required".into());
    }
    let mut outcome: Result<String, String> = Ok("Jira settings saved. Jira is disabled for this project.".into());
    if cfg.enabled {
        outcome = match problem {
            Some(e) => Err(e),
            None => {
                let effective = if use_platform { cfg.clone().with_platform(&platform) } else { cfg.clone() };
                match Client::new(&state, &effective) {
                    Ok(client) => client.check().await.map(|who| format!("Jira settings saved. {who}")),
                    Err(e) => Err(e),
                }
            }
        };
    }
    if outcome.is_err() {
        cfg.enabled = false;
    }
    query("DELETE FROM jira_settings WHERE project_id = ?").bind(id).execute(&state.db).await?;
    query("INSERT INTO jira_settings (project_id, base_url, auth, username, token, project_key, issue_type, ca_cert, enabled, source) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id)
        .bind(&cfg.base_url)
        .bind(if cfg.auth.is_empty() { "basic" } else { cfg.auth.as_str() })
        .bind(&cfg.username)
        .bind(&cfg.token)
        .bind(&cfg.project_key)
        .bind(&cfg.issue_type)
        .bind(&cfg.ca_cert)
        .bind(cfg.enabled)
        .bind(&cfg.source)
        .execute(&state.db)
        .await?;
    tracing::warn!("Jira settings of project {id} changed by {} (source: {}, enabled: {})", user.username, cfg.source, cfg.enabled);
    Ok(match outcome {
        Ok(m) => project_back(id, "msg", &m),
        Err(e) => project_back(id, "error", &format!("Jira settings saved, but Jira was left disabled: {e}")),
    })
}

// ---------------------------------------------------------------- per-card panel

struct Ctx {
    card: ops::Card,
    can_edit: bool,
    cfg: Option<Settings>,
}

async fn ctx(state: &AppState, user: &User, card_id: i64) -> Res<Ctx> {
    let card = ops::get_card(&state.db, card_id).await?;
    let access = board_access(&state.db, user, card.board_id).await?;
    let cfg = jira::settings(&state.db, access.project_id).await?.filter(Settings::usable);
    Ok(Ctx { can_edit: access.can_edit_card(card.created_by, user.id), card, cfg })
}

/// Loaded into the card modal after it opens, so a slow Jira never delays the card itself.
async fn render(state: &AppState, user: &User, card_id: i64, notice: Option<Result<String, String>>) -> Res<Markup> {
    let c = ctx(state, user, card_id).await?;
    let Some(cfg) = c.cfg else { return Ok(html! {}) };
    let link = jira::link(&state.db, card_id).await?;
    let base = format!("/cards/{card_id}/jira");
    let heading = "text-xs font-semibold uppercase tracking-wide opacity-60";
    let live = match &link {
        Some(l) => match Client::new(state, &cfg) {
            Ok(client) => {
                let status = client.status(&l.issue_key).await;
                let moves = if c.can_edit { client.transitions(&l.issue_key).await.unwrap_or_default() } else { Vec::new() };
                Some((status, moves))
            }
            Err(e) => Some((Err(e), Vec::new())),
        },
        None => None,
    };
    Ok(html! {
        section class="flex flex-col gap-2" {
            div class=(heading) { "Jira" }
            @match &notice {
                Some(Ok(m)) => div role="alert" class="alert alert-success text-sm py-2" { (m) },
                Some(Err(m)) => div role="alert" class="alert alert-error text-sm py-2" { (m) },
                None => {},
            }
            @match (&link, &live) {
                (Some(l), Some((status, moves))) => {
                    div class="flex flex-wrap items-center gap-2" {
                        a class="link link-primary font-mono font-semibold" href=(cfg.browse_url(&l.issue_key)) target="_blank" rel="noopener" { (l.issue_key) " ↗" }
                        @match status {
                            Ok(s) => span class="badge badge-soft badge-primary" { (s) },
                            Err(e) => span class="text-xs text-error" { (e) },
                        }
                        @if c.can_edit {
                            div class="flex-1" {}
                            button class="btn btn-ghost btn-xs" type="button" hx-post={ (base) "/resync" } hx-target="#jira-panel" title="Send the card's current content to Jira again" { "Resend" }
                            button class="btn btn-ghost btn-xs" type="button" hx-post={ (base) "/unlink" } hx-target="#jira-panel"
                                hx-confirm="Stop updating this Jira issue from the card? The issue itself is kept." { "Unlink" }
                        }
                    }
                    @if !moves.is_empty() {
                        div class="flex flex-wrap items-center gap-1" {
                            span class="text-xs opacity-60 mr-1" { "Move to:" }
                            @for t in moves {
                                form hx-post={ (base) "/transition" } hx-target="#jira-panel" {
                                    input type="hidden" name="transition" value=(t.id);
                                    button class="btn btn-xs btn-outline" title=(if t.to.is_empty() { t.name.clone() } else { format!("{} → {}", t.name, t.to) }) {
                                        (if t.to.is_empty() { t.name.as_str() } else { t.to.as_str() })
                                    }
                                }
                            }
                        }
                    }
                    @if !l.last_error.is_empty() {
                        div role="alert" class="alert alert-warning text-sm py-2" { "The last update to Jira failed: " (l.last_error) }
                    }
                    p class="text-xs opacity-60" {
                        "The title, description, comments and attached files of this card are sent to the issue as they change."
                        @if let Some(at) = &l.synced_at { " Last sent " (crate::views::timestamp(at)) "." }
                    }
                }
                _ => {
                    @if c.can_edit {
                        div class="flex items-center gap-3" {
                            button class="btn btn-sm btn-outline btn-primary" type="button" hx-post={ (base) "/create" } hx-target="#jira-panel" hx-disabled-elt="this" {
                                "Create Jira issue"
                            }
                            span class="text-xs opacity-60" { "In project " span class="font-mono" { (cfg.project_key) } " as a " (cfg.issue_type) ", linked back to this card." }
                        }
                    } @else {
                        p class="text-sm opacity-60" { "No Jira issue has been created from this card." }
                    }
                }
            }
        }
    })
}

pub async fn panel_fragment(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Markup> {
    render(&state, &user, id, None).await
}

async fn editable(state: &AppState, user: &User, card_id: i64) -> Res<(ops::Card, Settings)> {
    let c = ctx(state, user, card_id).await?;
    require(c.can_edit)?;
    let cfg = c.cfg.ok_or_else(|| bad("Jira is not set up for this project"))?;
    Ok((c.card, cfg))
}

pub async fn create(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    editable(&state, &user, id).await?;
    let notice = jira::create(&state, id).await.map(|key| format!("Created {key}."));
    // The board refresh puts the issue key on the card's tile.
    Ok((super::REFRESH, render(&state, &user, id, Some(notice)).await?).into_response())
}

#[derive(Deserialize)]
pub struct TransitionForm {
    transition: String,
}

pub async fn transition(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<TransitionForm>) -> Res<Markup> {
    let (_, cfg) = editable(&state, &user, id).await?;
    let link = jira::link(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let outcome = match Client::new(&state, &cfg) {
        Ok(client) => client.transition(&link.issue_key, &f.transition).await.map(|_| "Moved.".to_string()),
        Err(e) => Err(e),
    };
    render(&state, &user, id, Some(outcome)).await
}

pub async fn resync(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Markup> {
    editable(&state, &user, id).await?;
    // Forget what was sent so everything goes out again.
    query("UPDATE jira_links SET pushed_summary = '', pushed_description = '' WHERE card_id = ?").bind(id).execute(&state.db).await?;
    query("UPDATE jira_comments SET pushed_body = '' WHERE card_id = ?").bind(id).execute(&state.db).await?;
    jira::push(&state, id).await;
    let failed = jira::link(&state.db, id).await?.is_some_and(|l| !l.last_error.is_empty());
    render(&state, &user, id, (!failed).then(|| Ok("Sent to Jira.".to_string()))).await
}

pub async fn unlink(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    let c = ctx(&state, &user, id).await?;
    require(c.can_edit)?;
    let _ = c.card;
    for sql in ["DELETE FROM jira_links WHERE card_id = ?", "DELETE FROM jira_comments WHERE card_id = ?", "DELETE FROM jira_files WHERE card_id = ?"] {
        query(sql).bind(id).execute(&state.db).await?;
    }
    Ok((super::REFRESH, render(&state, &user, id, Some(Ok("Unlinked. The Jira issue was left as it is.".to_string()))).await?).into_response())
}

