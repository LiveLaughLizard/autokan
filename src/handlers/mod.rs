mod admin;
mod agent;
mod board;
mod card;
mod org;
mod upload;

use crate::db::query_as;
use crate::auth;
use crate::error::Res;
use crate::views::page;
use crate::db::query;
use crate::{ldap, oidc, saml, AppState};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use axum_extra::extract::cookie::{Cookie, CookieJar};
use axum_extra::extract::Form;
use maud::{html, Markup};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router(state: AppState) -> Router {
    let uploads = Router::new()
        .route("/boards/{id}/uploads", post(upload::upload))
        .layer(DefaultBodyLimit::max(state.cfg.max_upload_bytes));

    Router::new()
        .route("/static/{name}", get(crate::views::serve_asset))
        // auth
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/auth/ldap", post(ldap_login))
        .route("/auth/saml/start", get(saml_start))
        .route("/auth/saml/acs", post(saml_acs))
        .route("/auth/saml/metadata", get(saml_metadata))
        .route("/auth/oidc/start", get(oidc::start))
        .route("/auth/oidc/callback", get(oidc::callback))
        // teams & projects
        .route("/", get(org::home))
        .route("/teams", post(org::create_team))
        .route("/teams/{id}", get(org::team_page).post(org::rename_team))
        .route("/teams/{id}/delete", post(org::delete_team))
        .route("/teams/{id}/members", post(org::set_team_member))
        .route("/teams/{id}/members/{uid}/delete", post(org::remove_team_member))
        .route("/teams/{id}/projects", post(org::create_project))
        .route("/projects/{id}", get(org::project_page).post(org::update_project))
        .route("/projects/{id}/delete", post(org::delete_project))
        .route("/projects/{id}/join", post(org::join_project))
        .route("/projects/{id}/leave", post(org::leave_project))
        .route("/projects/{id}/members", post(org::set_project_member))
        .route("/projects/{id}/members/{uid}/delete", post(org::remove_project_member))
        .route("/projects/{id}/boards", post(board::create_board))
        // boards & columns
        .route("/boards/{id}", get(board::board_page))
        .route("/boards/{id}/columns", get(board::columns_fragment).post(board::create_column))
        .route("/boards/{id}/columns/order", post(board::order_columns))
        .route("/boards/{id}/settings", get(board::settings_page).post(board::save_settings))
        .route("/boards/{id}/delete", post(board::delete_board))
        .route("/boards/{id}/sync/link", post(board::link_sync))
        .route("/boards/{id}/sync/now", post(board::sync_now))
        .route("/boards/{id}/sync/unlink", post(board::unlink_sync))
        .route("/boards/{id}/labels", post(board::create_label))
        .route("/labels/{id}/delete", post(board::delete_label))
        .route("/columns/{id}/edit", get(board::column_modal))
        .route("/columns/{id}", post(board::update_column))
        .route("/columns/{id}/delete", post(board::delete_column))
        // cards & comments
        .route("/columns/{id}/cards", post(card::create))
        .route("/cards/{id}", get(card::modal).post(card::update))
        .route("/cards/{id}/move", post(card::move_card))
        .route("/cards/{id}/delete", post(card::delete))
        .route("/cards/{id}/labels/{lid}", post(card::toggle_label))
        .route("/cards/{id}/assignees/{uid}", post(card::toggle_assignee))
        .route("/cards/{id}/comments", post(card::add_comment))
        .route("/comments/{id}", post(card::edit_comment))
        .route("/comments/{id}/delete", post(card::delete_comment))
        // media
        .merge(uploads)
        .route("/uploads/{id}", get(upload::serve))
        // agent
        .route("/boards/{id}/agent/history", get(agent::history))
        .route("/boards/{id}/agent/chat", post(agent::chat))
        .route("/boards/{id}/agent/reset", post(agent::reset))
        .route("/columns/{id}/agent", post(agent::send_column))
        // admin
        .route("/admin", get(admin::page_handler))
        .route("/admin/oidc", post(admin::save_oidc))
        .route("/admin/ldap", post(admin::save_ldap))
        .route("/admin/ldap/test", post(admin::test_ldap))
        .route("/admin/saml", post(admin::save_saml))
        .route("/admin/roles", post(admin::create_role))
        .route("/admin/roles/{id}", post(admin::update_role))
        .route("/admin/roles/{id}/delete", post(admin::delete_role))
        .route("/admin/mappings", post(admin::create_mapping))
        .route("/admin/mappings/{id}/delete", post(admin::delete_mapping))
        .route("/admin/users/{id}/disabled", post(admin::set_user_disabled))
        .route("/admin/agent", post(admin::save_agent))
        .route("/admin/services", post(admin::create_service))
        .route("/admin/services/{id}", post(admin::update_service))
        .route("/admin/services/{id}/delete", post(admin::delete_service))
        .route("/admin/services/{id}/test", post(admin::test_service))
        .route("/projects/{id}/agent", post(org::set_project_agent))
        .route("/admin/password", post(admin::change_password))
        .with_state(state)
}

/// Header that makes the open board re-fetch its columns.
pub const REFRESH: [(&str, &str); 1] = [("HX-Trigger", "board-refresh")];

pub fn redirect(to: impl AsRef<str>) -> Response {
    Redirect::to(to.as_ref()).into_response()
}

async fn login_page(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Markup {
    let oidc_cfg = oidc::load_config(&state.db).await;
    let oidc_on = oidc_cfg.usable();
    let ldap_cfg = ldap::load_config(&state.db).await;
    let ldap_on = ldap_cfg.usable();
    let saml_cfg = saml::load_config(&state.db).await;
    let saml_on = saml_cfg.usable();
    let any_sso = oidc_on || ldap_on || saml_on;
    page(
        None,
        "Sign in",
        false,
        html! {
            div class="flex-1 flex items-center justify-center" {
                div class="stripe card bg-base-100/90 backdrop-blur border border-base-300 shadow-2xl w-full max-w-sm" {
                    div class="card-body gap-4" {
                        div class="flex flex-col items-center text-center gap-2 pt-2" {
                            (crate::views::logo("w-20 h-20"))
                            h1 class="gradient-text text-4xl font-extrabold tracking-tight" { "Autokan" }
                            p class="text-sm opacity-70" { "Kanban boards with an agent on call." }
                        }
                        @if let Some(err) = q.get("error") {
                            div role="alert" class="alert alert-error text-sm" { (err) }
                        }
                        @if oidc_on {
                            a class="btn btn-glow" href="/auth/oidc/start" {
                                (if oidc_cfg.button_label.is_empty() { "Sign in with OIDC" } else { oidc_cfg.button_label.as_str() })
                            }
                        }
                        @if saml_on {
                            a class=(if oidc_on { "btn btn-primary" } else { "btn btn-glow" }) href="/auth/saml/start" {
                                (if saml_cfg.button_label.is_empty() { "Sign in with SAML" } else { saml_cfg.button_label.as_str() })
                            }
                        }
                        @if ldap_on {
                            @if oidc_on || saml_on { div class="divider my-0 text-xs opacity-60" { "or" } }
                            form method="post" action="/auth/ldap" class="flex flex-col gap-3" {
                                input class="input w-full" name="username" placeholder="Directory username" autocomplete="username" required;
                                input class="input w-full" type="password" name="password" placeholder="Password" autocomplete="current-password" required;
                                button class=(if oidc_on || saml_on { "btn btn-primary" } else { "btn btn-glow" }) {
                                    (if ldap_cfg.button_label.is_empty() { "Sign in with LDAP" } else { ldap_cfg.button_label.as_str() })
                                }
                            }
                        }
                        @if !any_sso {
                            p class="text-sm opacity-70" {
                                "Single sign-on is not configured yet. Sign in with the break glass account to set it up."
                            }
                        }
                        details class="collapse collapse-arrow border border-base-300" open[!any_sso] {
                            summary class="collapse-title text-sm font-medium" { "Break glass account" }
                            div class="collapse-content" {
                                form method="post" action="/login" class="flex flex-col gap-3" {
                                    input class="input w-full" name="username" placeholder="Username" autocomplete="username" required;
                                    input class="input w-full" type="password" name="password" placeholder="Password" autocomplete="current-password" required;
                                    button class="btn btn-neutral" { "Sign in" }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login(State(state): State<AppState>, jar: CookieJar, Form(f): Form<LoginForm>) -> Res<Response> {
    if state.throttle.lock().unwrap().locked() {
        return Ok(redirect("/login?error=Too+many+failed+attempts.+Try+again+in+a+few+minutes."));
    }
    let row: Option<(i64, String, Option<String>)> =
        query_as("SELECT id, username, password_hash FROM users WHERE kind = 'breakglass' AND disabled = 0")
            .fetch_optional(&state.db)
            .await?;
    let ok = match &row {
        Some((_, username, Some(hash))) => {
            // Always verify the password so a wrong username costs the same time.
            let pw_ok = auth::verify_password(&f.password, hash);
            pw_ok && *username == f.username.trim()
        }
        _ => false,
    };
    if !ok {
        state.throttle.lock().unwrap().fail();
        tracing::warn!("failed break glass sign-in attempt");
        return Ok(redirect("/login?error=Invalid+username+or+password"));
    }
    state.throttle.lock().unwrap().reset();
    tracing::warn!("break glass account signed in");
    let cookie = auth::create_session(&state, row.unwrap().0, 12).await?;
    Ok((jar.add(cookie), redirect("/")).into_response())
}

/// Directory sign-in: username and password checked against LDAP.
async fn ldap_login(State(state): State<AppState>, jar: CookieJar, Form(f): Form<LoginForm>) -> Res<Response> {
    let cfg = ldap::load_config(&state.db).await;
    if !cfg.usable() {
        return Ok(redirect("/login?error=LDAP+sign-in+is+not+enabled"));
    }
    let key = f.username.trim().to_lowercase();
    if state.ldap_throttle.lock().unwrap().locked(&key) {
        return Ok(redirect("/login?error=Too+many+failed+attempts+for+this+account.+Try+again+in+a+few+minutes."));
    }
    let found = match ldap::authenticate(&cfg, &f.username, Some(&f.password)).await {
        Ok(u) => u,
        Err(ldap::LdapError::Invalid) => {
            state.ldap_throttle.lock().unwrap().fail(&key);
            return Ok(redirect("/login?error=Invalid+username+or+password"));
        }
        Err(ldap::LdapError::Unavailable(detail)) => {
            tracing::error!("LDAP sign-in failed: {detail}");
            return Ok(redirect("/login?error=Directory+sign-in+is+unavailable+right+now.+Please+contact+an+administrator."));
        }
    };
    state.ldap_throttle.lock().unwrap().reset(&key);
    let subject = format!("ldap|{}", found.dn.to_lowercase());
    let user_id = match auth::upsert_external_user(
        &state.db,
        "ldap",
        &subject,
        &found.username,
        &found.display_name,
        found.email.as_deref(),
        &found.groups,
    )
    .await
    {
        Ok(id) => id,
        Err(message) => {
            let q: String = url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
            return Ok(redirect(format!("/login?error={q}")));
        }
    };
    tracing::info!("LDAP sign-in: {} (groups: {:?})", found.username, found.groups);
    let cookie = auth::create_session(&state, user_id, 12).await?;
    Ok((jar.add(cookie), redirect("/")).into_response())
}

// ---- SAML ----------------------------------------------------------------------------------

const SAML_COOKIE: &str = "autokan_saml";

fn login_error(message: &str) -> Response {
    let q: String = url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
    redirect(format!("/login?error={q}"))
}

/// Send the browser to the identity provider with a fresh AuthnRequest.
async fn saml_start(State(state): State<AppState>, jar: CookieJar) -> Res<Response> {
    let cfg = saml::load_config(&state.db).await;
    if !cfg.usable() {
        return Ok(login_error("SAML sign-in is not enabled"));
    }
    let relay = auth::random_token(16);
    let (request_id, url) = match saml::authn_request(&state, &cfg, &relay) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("SAML sign-in could not start: {e}");
            return Ok(login_error("SAML sign-in is unavailable right now. Please contact an administrator."));
        }
    };
    // Pending requests share the short-lived state table with OIDC: state = RelayState, nonce = request id.
    query("INSERT INTO oidc_states (state, nonce, verifier, expires_at) VALUES (?, ?, 'saml', ?)")
        .bind(&relay)
        .bind(&request_id)
        .bind(crate::db::epoch() + 600)
        .execute(&state.db)
        .await?;
    // The response comes back as a cross-site POST, which only a SameSite=None cookie survives, and
    // browsers require HTTPS for those. Over plain HTTP (development) the flow is not tied to the browser.
    let jar = if state.cfg.public_url.starts_with("https://") {
        jar.add(
            Cookie::build((SAML_COOKIE, relay))
                .path("/auth/saml")
                .http_only(true)
                .secure(true)
                .same_site(axum_extra::extract::cookie::SameSite::None)
                .max_age(time::Duration::seconds(600))
                .build(),
        )
    } else {
        jar
    };
    Ok((jar, redirect(url)).into_response())
}

#[derive(Deserialize)]
struct SamlAcsForm {
    #[serde(rename = "SAMLResponse")]
    saml_response: String,
    #[serde(rename = "RelayState", default)]
    relay_state: String,
}

/// Assertion consumer service: the identity provider posts the signed response here.
async fn saml_acs(State(state): State<AppState>, jar: CookieJar, Form(f): Form<SamlAcsForm>) -> Res<Response> {
    let cfg = saml::load_config(&state.db).await;
    if !cfg.usable() {
        return Ok(login_error("SAML sign-in is not enabled"));
    }
    if state.cfg.public_url.starts_with("https://") && jar.get(SAML_COOKIE).map(|c| c.value()) != Some(f.relay_state.as_str()) {
        return Ok(login_error("This sign-in was not started from this browser. Please try again."));
    }
    let pending: Option<(String,)> = query_as("SELECT nonce FROM oidc_states WHERE state = ? AND verifier = 'saml' AND expires_at > ?")
        .bind(&f.relay_state)
        .bind(crate::db::epoch())
        .fetch_optional(&state.db)
        .await?;
    // Single use: a response can only be redeemed once, for the request it answers.
    let consumed = query("DELETE FROM oidc_states WHERE state = ? AND verifier = 'saml'")
        .bind(&f.relay_state)
        .execute(&state.db)
        .await?;
    let Some((request_id,)) = pending.filter(|_| consumed > 0) else {
        return Ok(login_error("Sign-in attempt expired or was already used. Please try again."));
    };
    let found = match saml::consume(&state, &cfg, &f.saml_response, &request_id) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("SAML sign-in failed: {e}");
            return Ok(login_error("The identity provider's response could not be verified."));
        }
    };
    let subject = format!("saml|{}|{}", found.idp, found.name_id);
    let user_id = match auth::upsert_external_user(
        &state.db,
        "saml",
        &subject,
        &found.username,
        &found.display_name,
        found.email.as_deref(),
        &found.groups,
    )
    .await
    {
        Ok(id) => id,
        Err(message) => return Ok(login_error(&message)),
    };
    tracing::info!("SAML sign-in: {} (groups: {:?})", found.username, found.groups);
    let cookie = auth::create_session(&state, user_id, 12).await?;
    let jar = jar.remove(Cookie::build(SAML_COOKIE).path("/auth/saml")).add(cookie);
    Ok((jar, redirect("/")).into_response())
}

/// This service provider's metadata, for registering Autokan with the identity provider.
async fn saml_metadata(State(state): State<AppState>) -> Response {
    let cfg = saml::load_config(&state.db).await;
    ([(axum::http::header::CONTENT_TYPE, "application/samlmetadata+xml")], saml::sp_metadata(&state, &cfg)).into_response()
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    auth::destroy_session(&state.db, &jar).await;
    (jar.remove(Cookie::build(auth::SESSION_COOKIE).path("/")), redirect("/login")).into_response()
}

/// Small helper used by pages that list users in a `<select>`.
pub fn user_options(users: &[(i64, String, String)]) -> Markup {
    html! {
        @for (id, username, display) in users {
            option value=(id) { (display) " (" (username) ")" }
        }
    }
}
