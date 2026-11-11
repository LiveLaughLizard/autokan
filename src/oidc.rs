//! OpenID Connect sign-in (authorization code flow with PKCE). Configuration lives in the
//! database and is edited from the admin panel. Group membership is read from the access
//! token (falling back to the ID token, then the userinfo endpoint) and drives RBAC.
use crate::db::{query, query_as};
use crate::auth;
use crate::db::{get_setting, Db};
use crate::AppState;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const STATE_COOKIE: &str = "autokan_oidc_state";
const SESSION_HOURS: i64 = 12;

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct OidcConfig {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: String,
    pub groups_claim: String,
    pub button_label: String,
    /// PEM bundle trusted (in addition to the system roots) when talking to the provider.
    pub ca_cert: String,
}

impl OidcConfig {
    pub fn usable(&self) -> bool {
        self.enabled && !self.issuer.is_empty() && !self.client_id.is_empty()
    }
    pub fn scopes(&self) -> &str {
        if self.scopes.trim().is_empty() { "openid profile email" } else { self.scopes.trim() }
    }
    pub fn groups_claim(&self) -> &str {
        if self.groups_claim.trim().is_empty() { "groups" } else { self.groups_claim.trim() }
    }
}

pub async fn load_config(db: &Db) -> OidcConfig {
    get_setting(db, "oidc").await
}

pub fn redirect_uri(state: &AppState) -> String {
    format!("{}/auth/oidc/callback", state.cfg.public_url)
}

#[derive(Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub userinfo_endpoint: Option<String>,
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

pub async fn discover(http: &reqwest::Client, issuer: &str) -> Result<Discovery, String> {
    let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Could not reach {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }
    let disc: Discovery = resp.json().await.map_err(|e| format!("Invalid discovery document: {e}"))?;
    if disc.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
        return Err(format!("Issuer mismatch: configured '{issuer}' but the provider reports '{}'", disc.issuer));
    }
    Ok(disc)
}

fn fail(msg: &str) -> Response {
    tracing::warn!("OIDC sign-in failed: {msg}");
    let q: String = url::form_urlencoded::byte_serialize(msg.as_bytes()).collect();
    Redirect::to(&format!("/login?error={q}")).into_response()
}

pub async fn start(State(state): State<AppState>, jar: CookieJar) -> Response {
    let cfg = load_config(&state.db).await;
    if !cfg.usable() {
        return fail("Single sign-on is not enabled");
    }
    let http = match crate::tls::http_client(&state.http, &cfg.ca_cert) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let disc = match discover(&http, &cfg.issuer).await {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    let state_token = auth::random_token(16);
    let nonce = auth::random_token(16);
    let verifier = auth::random_token(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if let Err(e) = query("INSERT INTO oidc_states (state, nonce, verifier, expires_at) VALUES (?, ?, ?, ?)")
        .bind(&state_token)
        .bind(&nonce)
        .bind(&verifier)
        .bind(crate::db::epoch() + 600)
        .execute(&state.db)
        .await
    {
        return fail(&format!("database error: {e}"));
    }
    let Ok(mut url) = url::Url::parse(&disc.authorization_endpoint) else {
        return fail("The provider's authorization endpoint is not a valid URL");
    };
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &redirect_uri(&state))
        .append_pair("scope", cfg.scopes())
        .append_pair("state", &state_token)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    (jar.add(state.cookie(STATE_COOKIE, state_token, 600)), Redirect::to(url.as_str())).into_response()
}

/// Verify a JWT against the provider's keys. `audience = None` skips the audience check
/// (access tokens are addressed to resource servers, not to us).
fn verify_jwt(token: &str, jwks: &JwkSet, issuer: &str, audience: Option<&str>) -> Result<Value, String> {
    let header = decode_header(token).map_err(|e| format!("not a JWT: {e}"))?;
    if matches!(header.alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512) {
        return Err("symmetric signing algorithms are not accepted".into());
    }
    let jwk = match &header.kid {
        Some(kid) => jwks.find(kid),
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
    .ok_or("no matching signing key in the provider's JWKS")?;
    let key = DecodingKey::from_jwk(jwk).map_err(|e| format!("unusable signing key: {e}"))?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[issuer]);
    match audience {
        Some(aud) => validation.set_audience(&[aud]),
        None => validation.validate_aud = false,
    }
    decode::<Value>(token, &key, &validation)
        .map(|d| d.claims)
        .map_err(|e| format!("token validation failed: {e}"))
}

/// Look a claim up by exact name first (so namespaced claims like `https://example.com/groups`
/// work), then as a dotted path (`realm_access.roles`).
fn claim<'a>(claims: &'a Value, path: &str) -> Option<&'a Value> {
    claims.get(path).or_else(|| path.split('.').try_fold(claims, |v, k| v.get(k)))
}

fn groups_from(claims: &Value, path: &str) -> Option<Vec<String>> {
    match claim(claims, path)? {
        Value::Array(items) => Some(items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()),
        Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

pub async fn callback(State(state): State<AppState>, jar: CookieJar, Query(q): Query<HashMap<String, String>>) -> Response {
    let result = finish(&state, &jar, &q).await;
    let jar = jar.remove(Cookie::build(STATE_COOKIE).path("/"));
    match result {
        Ok(cookie) => (jar.add(cookie), Redirect::to("/")).into_response(),
        Err(e) => (jar, fail(&e)).into_response(),
    }
}

async fn finish(state: &AppState, jar: &CookieJar, q: &HashMap<String, String>) -> Result<Cookie<'static>, String> {
    if let Some(err) = q.get("error") {
        return Err(format!("The identity provider returned an error: {} {}", err, q.get("error_description").map(String::as_str).unwrap_or("")));
    }
    let code = q.get("code").ok_or("Missing authorization code")?;
    let state_param = q.get("state").ok_or("Missing state")?;
    // The state must match the cookie set on this browser when the flow started (login CSRF).
    if jar.get(STATE_COOKIE).map(|c| c.value()) != Some(state_param.as_str()) {
        return Err("Sign-in state did not match. Please try again.".into());
    }
    let row: Option<(String, String)> = query_as("SELECT nonce, verifier FROM oidc_states WHERE state = ? AND expires_at > ?")
        .bind(state_param)
        .bind(crate::db::epoch())
        .fetch_optional(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    // Single use: the delete must remove a row, so a replayed callback finds nothing to consume.
    let consumed = query("DELETE FROM oidc_states WHERE state = ?")
        .bind(state_param)
        .execute(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let row = row.filter(|_| consumed > 0);
    let (nonce, verifier) = row.ok_or("Sign-in attempt expired. Please try again.")?;

    let cfg = load_config(&state.db).await;
    if !cfg.usable() {
        return Err("Single sign-on is not enabled".into());
    }
    let http = crate::tls::http_client(&state.http, &cfg.ca_cert)?;
    let disc = discover(&http, &cfg.issuer).await?;

    // Exchange the code.
    let redirect = redirect_uri(state);
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    let methods = disc.token_endpoint_auth_methods_supported.clone().unwrap_or_default();
    let use_post = !methods.is_empty() && !methods.iter().any(|m| m == "client_secret_basic");
    let mut req = http.post(&disc.token_endpoint).timeout(std::time::Duration::from_secs(20));
    if cfg.client_secret.is_empty() || use_post {
        form.push(("client_id", cfg.client_id.as_str()));
        if !cfg.client_secret.is_empty() {
            form.push(("client_secret", cfg.client_secret.as_str()));
        }
    } else {
        req = req.basic_auth(&cfg.client_id, Some(&cfg.client_secret));
    }
    let resp = req.form(&form).send().await.map_err(|e| format!("Token request failed: {e}"))?;
    let status = resp.status();
    let tokens: Value = resp.json().await.map_err(|e| format!("Token endpoint returned an unreadable response ({status}): {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "Token endpoint returned {status}: {}",
            tokens["error_description"].as_str().or(tokens["error"].as_str()).unwrap_or("unknown error")
        ));
    }
    let id_token = tokens["id_token"].as_str().ok_or("The provider did not return an ID token (is the 'openid' scope set?)")?;
    let access_token = tokens["access_token"].as_str().unwrap_or("");

    let jwks: JwkSet = http
        .get(&disc.jwks_uri)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Could not fetch the provider's signing keys: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Invalid JWKS: {e}"))?;

    let id_claims = verify_jwt(id_token, &jwks, &disc.issuer, Some(&cfg.client_id)).map_err(|e| format!("ID token rejected: {e}"))?;
    if id_claims["nonce"].as_str() != Some(nonce.as_str()) {
        return Err("ID token rejected: nonce mismatch".into());
    }
    let sub = id_claims["sub"].as_str().filter(|s| !s.is_empty()).ok_or("ID token has no subject")?;

    // Groups: access token first (when it is a JWT signed by the provider), then ID token, then userinfo.
    let claim_name = cfg.groups_claim();
    let access_claims = verify_jwt(access_token, &jwks, &disc.issuer, None).ok();
    let mut groups = access_claims.as_ref().and_then(|c| groups_from(c, claim_name));
    if groups.is_none() {
        groups = groups_from(&id_claims, claim_name);
    }
    let mut profile = id_claims.clone();
    let needs_userinfo = groups.is_none() || (profile["name"].is_null() && profile["preferred_username"].is_null());
    if let (true, Some(endpoint), false) = (needs_userinfo, &disc.userinfo_endpoint, access_token.is_empty()) {
        let info = http.get(endpoint).bearer_auth(access_token).timeout(std::time::Duration::from_secs(15)).send().await;
        if let Ok(info) = info {
            if let Ok(info) = info.json::<Value>().await {
                // Userinfo must describe the same subject as the verified ID token.
                if info["sub"].as_str() == Some(sub) {
                    if groups.is_none() {
                        groups = groups_from(&info, claim_name);
                    }
                    if let (Some(p), Some(i)) = (profile.as_object_mut(), info.as_object()) {
                        for (k, v) in i {
                            p.entry(k.clone()).or_insert(v.clone());
                        }
                    }
                }
            }
        }
    }
    let groups = groups.unwrap_or_default();

    let username = profile["preferred_username"].as_str().or(profile["email"].as_str()).unwrap_or(sub);
    let display = profile["name"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or(username);
    let subject = format!("{}|{}", disc.issuer, sub);

    let user_id =
        auth::upsert_external_user(&state.db, "oidc", &subject, username, display, profile["email"].as_str(), &groups).await?;
    tracing::info!("OIDC sign-in: {username} (groups: {groups:?})");
    auth::create_session(state, user_id, SESSION_HOURS).await.map_err(|e| e.to_string())
}
