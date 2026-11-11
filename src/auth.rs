use crate::db::{query, query_as};
use crate::db::Db;
use crate::AppState;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, request::Parts, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const SESSION_COOKIE: &str = "autokan_session";

/// Platform-level permissions that can be bundled into roles and mapped to OIDC groups.
pub const PERMISSIONS: &[(&str, &str)] = &[
    ("platform.admin", "Full platform administrator (implies everything below)"),
    ("admin.oidc", "Admin panel: configure single sign-on (OIDC, LDAP, SAML)"),
    ("admin.rbac", "Admin panel: manage roles and group mappings"),
    ("admin.users", "Admin panel: view, enable and disable users"),
    ("admin.agent", "Admin panel: configure agent LLM services, API keys and defaults"),
    ("admin.jira", "Admin panel: configure the organization's Jira connection"),
    ("teams.create", "Create teams"),
    ("teams.manage", "Manage every team (acts as team admin everywhere)"),
];

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub kind: String,
    pub username: String,
    pub display_name: String,
    pub perms: HashSet<String>,
}

impl User {
    pub fn is_breakglass(&self) -> bool {
        self.kind == "breakglass"
    }
    pub fn has(&self, perm: &str) -> bool {
        self.is_breakglass() || self.perms.contains("platform.admin") || self.perms.contains(perm)
    }
    pub fn sees_admin(&self) -> bool {
        PERMISSIONS.iter().any(|(p, _)| p.starts_with("admin.") && self.has(p))
    }
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

/// Create the break glass account on first start. The password comes from
/// AUTOKAN_BREAKGLASS_PASSWORD or is generated and printed exactly once.
pub async fn ensure_breakglass(db: &Db) -> anyhow::Result<()> {
    let exists: Option<(i64,)> = query_as("SELECT id FROM users WHERE kind = 'breakglass'")
        .fetch_optional(db)
        .await?;
    if exists.is_some() {
        return Ok(());
    }
    let username = std::env::var("AUTOKAN_BREAKGLASS_USER").unwrap_or_else(|_| "breakglass".into());
    let (password, generated) = match std::env::var("AUTOKAN_BREAKGLASS_PASSWORD") {
        Ok(p) if !p.is_empty() => (p, false),
        _ => (random_token(12), true),
    };
    query("INSERT INTO users (kind, subject, username, display_name, password_hash, created_at) VALUES ('breakglass', 'local|breakglass', ?, 'Break Glass Admin', ?, ?)")
        .bind(&username)
        .bind(hash_password(&password)?)
        .bind(crate::db::now())
        .execute(db)
        .await?;
    if generated {
        eprintln!("\n================ BREAK GLASS ACCOUNT CREATED ================");
        eprintln!("  username: {username}");
        eprintln!("  password: {password}");
        eprintln!("  This password is shown once. Store it somewhere safe.");
        eprintln!("  Reset it later with: autokan reset-breakglass");
        eprintln!("=============================================================\n");
    } else {
        tracing::info!("break glass account '{username}' created from AUTOKAN_BREAKGLASS_PASSWORD");
    }
    Ok(())
}

/// CLI recovery path: set a fresh random break glass password and revoke its sessions.
pub async fn reset_breakglass(db: &Db) -> anyhow::Result<(String, String)> {
    ensure_breakglass(db).await?;
    let (id, username): (i64, String) = query_as("SELECT id, username FROM users WHERE kind = 'breakglass'")
        .fetch_one(db)
        .await?;
    let password = random_token(12);
    set_password(db, id, &password).await?;
    Ok((username, password))
}

pub async fn set_password(db: &Db, user_id: i64, password: &str) -> anyhow::Result<()> {
    query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash_password(password)?)
        .bind(user_id)
        .execute(db)
        .await?;
    query("DELETE FROM sessions WHERE user_id = ?").bind(user_id).execute(db).await?;
    Ok(())
}

pub async fn create_session(state: &AppState, user_id: i64, hours: i64) -> anyhow::Result<Cookie<'static>> {
    let token = random_token(32);
    query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(hash_token(&token))
        .bind(user_id)
        .bind(crate::db::epoch() + hours * 3600)
        .execute(&state.db)
        .await?;
    query("UPDATE users SET last_login = ? WHERE id = ?")
        .bind(crate::db::now())
        .bind(user_id)
        .execute(&state.db)
        .await?;
    Ok(state.cookie(SESSION_COOKIE, token, hours * 3600))
}

pub async fn destroy_session(db: &Db, jar: &CookieJar) {
    if let Some(c) = jar.get(SESSION_COOKIE) {
        let _ = query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(hash_token(c.value()))
            .execute(db)
            .await;
    }
}

impl AppState {
    pub fn cookie(&self, name: &'static str, value: String, max_age_secs: i64) -> Cookie<'static> {
        Cookie::build((name, value))
            .path("/")
            .http_only(true)
            .same_site(SameSite::Lax)
            .secure(self.cfg.public_url.starts_with("https://"))
            .max_age(time_duration(max_age_secs))
            .build()
    }
}

fn time_duration(secs: i64) -> time::Duration {
    time::Duration::seconds(secs)
}

async fn load_user(db: &Db, token: &str) -> anyhow::Result<Option<User>> {
    let row: Option<(i64, String, String, String)> = query_as(
        "SELECT u.id, u.kind, u.username, u.display_name FROM sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.token_hash = ? AND s.expires_at > ? AND u.disabled = 0",
    )
    .bind(hash_token(token))
    .bind(crate::db::epoch())
    .fetch_optional(db)
    .await?;
    let Some((id, kind, username, display_name)) = row else { return Ok(None) };
    let rows: Vec<(String,)> = query_as(
        "SELECT r.permissions FROM user_groups g JOIN group_mappings m ON m.group_name = g.group_name \
         JOIN roles r ON r.id = m.role_id WHERE g.user_id = ?",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    let mut perms = HashSet::new();
    for (json,) in rows {
        perms.extend(serde_json::from_str::<Vec<String>>(&json).unwrap_or_default());
    }
    Ok(Some(User { id, kind, username, display_name, perms }))
}

impl FromRequestParts<AppState> for User {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let jar = CookieJar::from_headers(&parts.headers);
        if let Some(c) = jar.get(SESSION_COOKIE) {
            match load_user(&state.db, c.value()).await {
                Ok(Some(u)) => return Ok(u),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("session lookup failed: {e:#}");
                    return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
            }
        }
        if parts.headers.contains_key("hx-request") || parts.method != Method::GET {
            Err((StatusCode::UNAUTHORIZED, [("HX-Redirect", "/login")], "Signed out").into_response())
        } else {
            Err(Redirect::to("/login").into_response())
        }
    }
}

/// Rejects cross-origin state-changing requests (defence in depth on top of SameSite=Lax cookies).
pub async fn origin_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // The SAML assertion consumer service is, by design, a cross-site POST from the identity
    // provider; it authenticates the request itself (signed response answering a request we issued).
    let exempt = req.uri().path() == "/auth/saml/acs";
    if !exempt && !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
            let origin_host = origin.split("://").nth(1).unwrap_or("");
            let public = state.cfg.public_url.trim_end_matches('/');
            if origin_host != host && origin != public {
                return (StatusCode::FORBIDDEN, "Cross-origin request rejected").into_response();
            }
        }
    }
    next.run(req).await
}

/// Very small global brake on break glass password guessing.
#[derive(Default)]
pub struct Throttle {
    failures: u32,
    window_start: Option<std::time::Instant>,
}

impl Throttle {
    const MAX_FAILURES: u32 = 10;
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

    pub fn locked(&mut self) -> bool {
        if self.window_start.is_some_and(|t| t.elapsed() > Self::WINDOW) {
            *self = Self::default();
        }
        self.failures >= Self::MAX_FAILURES
    }
    pub fn fail(&mut self) {
        self.window_start.get_or_insert_with(std::time::Instant::now);
        self.failures += 1;
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Create or refresh the local record of someone who signed in through an identity provider
/// (OIDC, LDAP or SAML) and replace their group memberships, which drive RBAC.
/// Returns the user id, or an error message if the account has been disabled here.
pub async fn upsert_external_user(
    db: &Db,
    kind: &str,
    subject: &str,
    username: &str,
    display_name: &str,
    email: Option<&str>,
    groups: &[String],
) -> Result<i64, String> {
    let fail = |e: anyhow::Error| {
        tracing::error!("could not save the signed-in user: {e:#}");
        "A database error prevented sign-in".to_string()
    };
    // Group names are stored in a 255-character key column.
    let clip = |s: &str| s.chars().take(255).collect::<String>();
    let (subject, username, display_name) = (clip(subject), clip(username), clip(display_name));
    let tx = db.begin().await.map_err(fail)?;
    let existing: Option<(i64, bool)> = query_as("SELECT id, disabled FROM users WHERE subject = ?")
        .bind(&subject)
        .fetch_optional(&tx)
        .await
        .map_err(fail)?;
    let (user_id, disabled) = match existing {
        Some((id, disabled)) => {
            query("UPDATE users SET kind = ?, username = ?, display_name = ?, email = ? WHERE id = ?")
                .bind(kind)
                .bind(&username)
                .bind(&display_name)
                .bind(email.map(clip))
                .bind(id)
                .execute(&tx)
                .await
                .map_err(fail)?;
            (id, disabled)
        }
        None => {
            let id = query("INSERT INTO users (kind, subject, username, display_name, email, created_at) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(kind)
                .bind(&subject)
                .bind(&username)
                .bind(&display_name)
                .bind(email.map(clip))
                .bind(crate::db::now())
                .insert(&tx)
                .await
                .map_err(fail)?;
            (id, false)
        }
    };
    query("DELETE FROM user_groups WHERE user_id = ?").bind(user_id).execute(&tx).await.map_err(fail)?;
    let mut seen: Vec<String> = Vec::new();
    for g in groups {
        // Skip duplicates (also ones differing only by case, which some databases treat as equal).
        let g = clip(g.trim());
        if g.is_empty() || seen.iter().any(|s| s.eq_ignore_ascii_case(&g)) {
            continue;
        }
        query("INSERT INTO user_groups (user_id, group_name) VALUES (?, ?)")
            .bind(user_id)
            .bind(&g)
            .execute(&tx)
            .await
            .map_err(fail)?;
        seen.push(g);
    }
    tx.commit().await.map_err(fail)?;
    if disabled {
        return Err("This account has been disabled".into());
    }
    Ok(user_id)
}

/// Per-login brake on password guessing for directory (LDAP) sign-in, where many people share
/// the form and one person's typos must not lock out everyone else.
#[derive(Default)]
pub struct KeyedThrottle {
    attempts: std::collections::HashMap<String, (u32, std::time::Instant)>,
}

impl KeyedThrottle {
    const MAX_FAILURES: u32 = 5;
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(300);
    const MAX_KEYS: usize = 20_000;

    pub fn locked(&mut self, key: &str) -> bool {
        match self.attempts.get(key) {
            Some((_, since)) if since.elapsed() > Self::WINDOW => {
                self.attempts.remove(key);
                false
            }
            Some((failures, _)) => *failures >= Self::MAX_FAILURES,
            None => false,
        }
    }
    pub fn fail(&mut self, key: &str) {
        if self.attempts.len() >= Self::MAX_KEYS {
            // Under a spray of made-up logins, forget expired entries rather than grow without bound.
            self.attempts.retain(|_, (_, since)| since.elapsed() <= Self::WINDOW);
            if self.attempts.len() >= Self::MAX_KEYS {
                return;
            }
        }
        self.attempts.entry(key.to_string()).or_insert((0, std::time::Instant::now())).0 += 1;
    }
    pub fn reset(&mut self, key: &str) {
        self.attempts.remove(key);
    }
}
