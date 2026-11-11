//! Admin panel: single sign-on, roles & group mappings, users, agent services, break glass password.
//! Only the break glass account sees it until a role granting `admin.*` is mapped to a group
//! delivered by one of the sign-in providers.
use crate::db::{query, query_as};
use super::board::check_base_url;
use super::redirect;
use crate::llm::{self, AgentDefaults, Service};
use crate::auth::{self, User, PERMISSIONS};
use crate::db::{get_setting, set_setting};
use crate::error::{bad, AppError, Res};
use crate::ops::clean_name;
use crate::perm::require;
use crate::views::{page, panel, target_select};
use crate::{ldap, oidc, saml, AppState};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum_extra::extract::Form;
use maud::{html, Markup};
use serde::Deserialize;
use std::collections::HashMap;

fn tabs(user: &User) -> Vec<(&'static str, &'static str)> {
    let mut t = Vec::new();
    if user.has("admin.oidc") {
        t.push(("oidc", "Single sign-on"));
    }
    if user.has("admin.rbac") {
        t.push(("rbac", "Roles & groups"));
    }
    if user.has("admin.users") {
        t.push(("users", "Users"));
    }
    if user.has("admin.agent") {
        t.push(("agent", "Agent services"));
    }
    if user.is_breakglass() {
        t.push(("account", "Break glass account"));
    }
    t
}

fn back(tab: &str, key: &str, msg: &str) -> Response {
    let q: String = url::form_urlencoded::byte_serialize(msg.as_bytes()).collect();
    redirect(format!("/admin?tab={tab}&{key}={q}"))
}

fn field(label: &str, help: &str, input: Markup) -> Markup {
    html! {
        label class="flex flex-col gap-1" {
            span class="text-sm font-medium" { (label) }
            (input)
            @if !help.is_empty() { span class="text-xs opacity-60" { (help) } }
        }
    }
}

pub async fn page_handler(State(state): State<AppState>, user: User, Query(q): Query<HashMap<String, String>>) -> Res<Markup> {
    let tabs = tabs(&user);
    require(!tabs.is_empty())?;
    let wanted = q.get("tab").map(String::as_str).unwrap_or("");
    let tab = tabs.iter().find(|(k, _)| *k == wanted).unwrap_or(&tabs[0]).0;
    let body = match tab {
        "oidc" => sso_tab(&state, q.get("provider").map(String::as_str).unwrap_or("")).await,
        "rbac" => rbac_tab(&state).await?,
        "users" => users_tab(&state).await?,
        "agent" => agent_tab(&state).await?,
        _ => account_tab(),
    };
    Ok(page(
        Some(&user),
        "Admin",
        false,
        html! {
            h1 class="text-2xl font-bold" { "Admin" }
            div role="tablist" class="tabs tabs-border" {
                @for (key, label) in &tabs {
                    a role="tab" class={ "tab " (if *key == tab { "tab-active" } else { "" }) } href={ "/admin?tab=" (key) } { (label) }
                }
            }
            @if let Some(m) = q.get("msg") { div role="alert" class="alert alert-success text-sm" { (m) } }
            @if let Some(m) = q.get("error") { div role="alert" class="alert alert-error text-sm" { (m) } }
            (body)
        },
    ))
}

// ---------------------------------------------------------------- Single sign-on

/// One provider at a time: pick it, configure it, enable it. Several may be enabled together.
async fn sso_tab(state: &AppState, selected: &str) -> Markup {
    let oidc_cfg = oidc::load_config(&state.db).await;
    let ldap_cfg = ldap::load_config(&state.db).await;
    let saml_cfg = saml::load_config(&state.db).await;
    // (key, name, summary, enabled, has any settings)
    let providers = [
        ("oidc", "OpenID Connect", "Redirect to an identity provider such as Keycloak, Entra ID, Okta or Google.", oidc_cfg.usable(), !oidc_cfg.issuer.is_empty()),
        ("ldap", "LDAP / Active Directory", "Username and password checked against your directory.", ldap_cfg.usable(), !ldap_cfg.url.is_empty()),
        ("saml", "SAML 2.0", "Redirect to a SAML identity provider such as ADFS, Shibboleth or Okta.", saml_cfg.usable(), !saml_cfg.idp_metadata.is_empty()),
    ];
    html! {
        section class="flex flex-col gap-3" {
            p class="text-sm opacity-70" {
                "Choose a sign-in method to configure. Whichever methods are enabled appear on the sign-in page; groups from any of them feed the same role mappings under Roles & groups."
            }
            div class="grid md:grid-cols-3 gap-3" {
                @for (key, name, summary, enabled, configured) in providers {
                    a href={ "/admin?tab=oidc&provider=" (key) }
                        class={ "lift card bg-base-100 border " (if key == selected { "border-primary ring-2 ring-primary/30" } else { "border-base-300" }) } {
                        div class="card-body p-4 gap-2" {
                            div class="flex items-center gap-2" {
                                span class="font-semibold flex-1" { (name) }
                                @if enabled {
                                    span class="badge badge-sm badge-success" { "Enabled" }
                                } @else if configured {
                                    span class="badge badge-sm badge-warning" { "Disabled" }
                                } @else {
                                    span class="badge badge-sm badge-ghost" { "Not set up" }
                                }
                            }
                            p class="text-sm opacity-70" { (summary) }
                            span class="text-sm text-primary font-medium" { (if key == selected { "Configuring ↓" } else { "Configure →" }) }
                        }
                    }
                }
            }
        }
        @match selected {
            "oidc" => (oidc_tab(state).await),
            "ldap" => (ldap_panel(state).await),
            "saml" => (saml_panel(state).await),
            _ => {},
        }
    }
}

// ---------------------------------------------------------------- OIDC

async fn oidc_tab(state: &AppState) -> Markup {
    let cfg = oidc::load_config(&state.db).await;
    panel(
        "OpenID Connect",
        html! {
            p class="text-sm opacity-70" {
                "Register Autokan with your identity provider as a confidential web client using this redirect URI:"
            }
            code class="bg-base-200 rounded px-2 py-1 text-sm w-fit select-all" { (oidc::redirect_uri(state)) }
            form method="post" action="/admin/oidc" class="grid md:grid-cols-2 gap-4 max-w-3xl" {
                (field("Issuer URL", "Discovery is read from <issuer>/.well-known/openid-configuration.", html! {
                    input class="input w-full" name="issuer" value=(cfg.issuer) placeholder="https://idp.example.com/realms/main";
                }))
                (field("Sign-in button label", "", html! {
                    input class="input w-full" name="button_label" value=(cfg.button_label) placeholder="Sign in with OIDC";
                }))
                (field("Client ID", "", html! { input class="input w-full" name="client_id" value=(cfg.client_id); }))
                (field("Client secret", "Leave blank to keep the stored secret.", html! {
                    input class="input w-full" type="password" name="client_secret" autocomplete="off"
                        placeholder=(if cfg.client_secret.is_empty() { "not set" } else { "••••••••" });
                }))
                (field("Scopes", "Space separated. Add your provider's groups scope if it needs one.", html! {
                    input class="input w-full" name="scopes" value=(cfg.scopes()) ;
                }))
                (field("Groups claim", "Read from the access token (then the ID token, then userinfo). Dotted paths such as realm_access.roles work.", html! {
                    input class="input w-full" name="groups_claim" value=(cfg.groups_claim());
                }))
                (ca_field(&cfg.ca_cert))
                label class="flex items-center gap-3 md:col-span-2" {
                    input type="checkbox" class="toggle toggle-primary" name="enabled" checked[cfg.enabled];
                    span { "Enable OpenID Connect sign-in" }
                }
                div class="md:col-span-2" { button class="btn btn-primary btn-sm" { "Save" } }
            }
            p class="text-xs opacity-60" {
                "Signing in with OIDC only creates an ordinary user. Platform permissions come from the Roles & groups tab; the break glass account keeps working regardless."
            }
        },
    )
}

#[derive(Deserialize)]
pub struct OidcForm {
    issuer: String,
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    scopes: String,
    #[serde(default)]
    groups_claim: String,
    #[serde(default)]
    button_label: String,
    #[serde(default)]
    ca_cert: String,
    enabled: Option<String>,
}

pub async fn save_oidc(State(state): State<AppState>, user: User, Form(f): Form<OidcForm>) -> Res<Response> {
    require(user.has("admin.oidc"))?;
    let old = oidc::load_config(&state.db).await;
    let mut cfg = oidc::OidcConfig {
        enabled: f.enabled.is_some(),
        issuer: f.issuer.trim().trim_end_matches('/').to_string(),
        client_id: f.client_id.trim().to_string(),
        client_secret: if f.client_secret.is_empty() { old.client_secret } else { f.client_secret },
        scopes: f.scopes.trim().to_string(),
        groups_claim: f.groups_claim.trim().to_string(),
        button_label: f.button_label.trim().to_string(),
        ca_cert: f.ca_cert.trim().to_string(),
    };
    let mut problem = crate::tls::parse_ca(&cfg.ca_cert).err();
    if problem.is_none() && cfg.enabled {
        if cfg.issuer.is_empty() || cfg.client_id.is_empty() {
            problem = Some("Issuer URL and client ID are required".to_string());
        } else if !cfg.scopes().split_whitespace().any(|s| s == "openid") {
            problem = Some("Scopes must include 'openid'".to_string());
        } else {
            let http = crate::tls::http_client(&state.http, &cfg.ca_cert);
            let found = match &http {
                Ok(http) => oidc::discover(http, &cfg.issuer).await.map(|_| ()),
                Err(e) => Err(e.clone()),
            };
            problem = found.err();
        }
    }
    if problem.is_some() {
        cfg.enabled = false;
    }
    set_setting(&state.db, "oidc", &cfg).await?;
    tracing::warn!("OIDC configuration changed by {} (enabled: {})", user.username, cfg.enabled);
    Ok(match problem {
        Some(p) => back("oidc&provider=oidc", "error", &format!("OpenID Connect settings saved, but it was left disabled: {p}")),
        None if cfg.enabled => back("oidc&provider=oidc", "msg", "OpenID Connect settings saved. Discovery succeeded and it is enabled."),
        None => back("oidc&provider=oidc", "msg", "OpenID Connect settings saved. OpenID Connect sign-in is disabled."),
    })
}

/// "Paste your CA" box shared by the identity provider forms.
fn ca_field(value: &str) -> Markup {
    html! {
        div class="md:col-span-2" {
            (field("Custom CA certificate (optional)", "Paste the full PEM certificate (or chain) of the certificate authority that signed the server's TLS certificate. It is trusted in addition to the system's CAs, for this provider only.", html! {
                textarea class="textarea w-full font-mono text-xs" name="ca_cert" rows="4" spellcheck="false"
                    placeholder="-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----" { (value) }
            }))
        }
    }
}

// ---------------------------------------------------------------- LDAP

fn scope_select(name: &str, current: &str) -> Markup {
    html! {
        select class="select w-full" name=(name) {
            option value="sub" selected[current == "sub"] { "Whole subtree — the base and everything beneath it" }
            option value="one" selected[current == "one"] { "One level — only entries directly inside the base" }
            option value="base" selected[current == "base"] { "Base only — the base entry itself" }
        }
    }
}

async fn ldap_panel(state: &AppState) -> Markup {
    let cfg = ldap::load_config(&state.db).await;
    panel(
        "LDAP / Active Directory",
        html! {
            p class="text-sm opacity-70" {
                "People sign in with their directory username and password. Autokan binds with the service account, finds the user, checks the password by binding as that user, then reads their groups for the Roles & groups mappings."
            }
            form method="post" action="/admin/ldap" class="grid md:grid-cols-2 gap-4 max-w-3xl" {
                (field("Server URL", "ldaps://host:636 for TLS, or ldap://host:389 (ideally with StartTLS).", html! {
                    input class="input w-full" name="url" value=(cfg.url) placeholder="ldaps://ldap.example.com:636";
                }))
                (field("Sign-in button label", "", html! {
                    input class="input w-full" name="button_label" value=(cfg.button_label) placeholder="Sign in with LDAP";
                }))
                (field("Service account DN", "Leave blank to search anonymously.", html! {
                    input class="input w-full" name="bind_dn" value=(cfg.bind_dn) placeholder="cn=autokan,ou=services,dc=example,dc=com";
                }))
                (field("Service account password", "Leave blank to keep the stored password.", html! {
                    input class="input w-full" type="password" name="bind_password" autocomplete="off"
                        placeholder=(if cfg.bind_password.is_empty() { "not set" } else { "••••••••" });
                }))
                label class="flex items-center gap-3 md:col-span-2" {
                    input type="checkbox" class="checkbox checkbox-sm" name="starttls" checked[cfg.starttls];
                    span class="text-sm" { "Use StartTLS (upgrade an ldap:// connection to TLS before sending any password)" }
                }
                (ca_field(&cfg.ca_cert))

                div class="md:col-span-2 font-semibold border-t border-base-300 pt-3" { "Finding users" }
                (field("User search base(s)", "One DN per line. Use the directory root for the full tree, or one or more OUs to restrict who can sign in.", html! {
                    textarea class="textarea w-full font-mono text-sm" name="user_base" rows="2" placeholder="ou=people,dc=example,dc=com" { (cfg.user_base) }
                }))
                (field("User search depth", "", scope_select("user_scope", &cfg.user_scope)))
                div class="md:col-span-2" {
                    (field("User filter", "{username} is what the person typed. It must match exactly one entry.", html! {
                        input class="input w-full font-mono text-sm" name="user_filter" value=(cfg.user_filter);
                    }))
                }
                div class="md:col-span-2 grid md:grid-cols-3 gap-4" {
                    (field("Username attribute", "", html! { input class="input w-full font-mono text-sm" name="attr_username" value=(cfg.attr_username); }))
                    (field("Display name attribute", "", html! { input class="input w-full font-mono text-sm" name="attr_display" value=(cfg.attr_display); }))
                    (field("Email attribute", "", html! { input class="input w-full font-mono text-sm" name="attr_email" value=(cfg.attr_email); }))
                }

                div class="md:col-span-2 font-semibold border-t border-base-300 pt-3" { "Finding groups" }
                (field("Group search base(s)", "One DN per line. Leave blank to skip groups.", html! {
                    textarea class="textarea w-full font-mono text-sm" name="group_base" rows="2" placeholder="ou=groups,dc=example,dc=com" { (cfg.group_base) }
                }))
                (field("Group search depth", "", scope_select("group_scope", &cfg.group_scope)))
                div class="md:col-span-2" {
                    (field("Group membership filter", "{dn} is the user's DN and {username} their login name.", html! {
                        input class="input w-full font-mono text-sm" name="group_filter" value=(cfg.group_filter);
                    }))
                }
                (field("Group name attribute", "This value is what you map to roles.", html! {
                    input class="input w-full font-mono text-sm" name="attr_group" value=(cfg.attr_group);
                }))
                (field("Nested groups", "Whether membership of a group inside another group counts.", html! {
                    select class="select w-full" name="nested" {
                        option value="off" selected[cfg.nested == "off"] { "Off — direct memberships only" }
                        option value="recursive" selected[cfg.nested == "recursive"] { "Follow parent groups (any LDAP server)" }
                        option value="ad" selected[cfg.nested == "ad"] { "Active Directory in-chain match (one query)" }
                    }
                }))
                label class="flex items-center gap-3 md:col-span-2" {
                    input type="checkbox" class="toggle toggle-primary" name="enabled" checked[cfg.enabled];
                    span { "Enable LDAP sign-in" }
                }
                div class="md:col-span-2" { button class="btn btn-primary btn-sm" { "Save" } }
            }
            form method="post" action="/admin/ldap/test" class="flex flex-wrap items-end gap-2 border-t border-base-300 pt-3" {
                (field("Test the saved settings", "", html! {
                    input class="input input-sm w-56" name="username" placeholder="a directory username" required;
                }))
                input class="input input-sm w-56" type="password" name="password" autocomplete="off" placeholder="password (optional)";
                button class="btn btn-sm" { "Look up" }
                span class="text-xs opacity-60 basis-full" { "Shows the entry and groups the directory returns. With a password it also checks the password. Nobody is signed in." }
            }
        },
    )
}

#[derive(Deserialize)]
pub struct LdapForm {
    #[serde(default)]
    url: String,
    #[serde(default)]
    button_label: String,
    #[serde(default)]
    bind_dn: String,
    #[serde(default)]
    bind_password: String,
    starttls: Option<String>,
    #[serde(default)]
    ca_cert: String,
    #[serde(default)]
    user_base: String,
    #[serde(default)]
    user_scope: String,
    #[serde(default)]
    user_filter: String,
    #[serde(default)]
    attr_username: String,
    #[serde(default)]
    attr_display: String,
    #[serde(default)]
    attr_email: String,
    #[serde(default)]
    group_base: String,
    #[serde(default)]
    group_scope: String,
    #[serde(default)]
    group_filter: String,
    #[serde(default)]
    attr_group: String,
    #[serde(default)]
    nested: String,
    enabled: Option<String>,
}

pub async fn save_ldap(State(state): State<AppState>, user: User, Form(f): Form<LdapForm>) -> Res<Response> {
    require(user.has("admin.oidc"))?;
    let old = ldap::load_config(&state.db).await;
    let defaults = ldap::LdapConfig::default();
    let or = |value: &str, fallback: &str| if value.trim().is_empty() { fallback.to_string() } else { value.trim().to_string() };
    let one_of = |value: &str, allowed: &[&str], fallback: &str| if allowed.contains(&value) { value.to_string() } else { fallback.to_string() };
    let url = f.url.trim().to_string();
    // The stored password only ever goes to the server it was entered for.
    let moved = url != old.url;
    let mut cfg = ldap::LdapConfig {
        enabled: f.enabled.is_some(),
        starttls: f.starttls.is_some(),
        ca_cert: f.ca_cert.trim().to_string(),
        bind_dn: f.bind_dn.trim().to_string(),
        bind_password: if !f.bind_password.is_empty() { f.bind_password.clone() } else if moved { String::new() } else { old.bind_password.clone() },
        user_base: f.user_base.trim().to_string(),
        user_scope: one_of(&f.user_scope, &["sub", "one", "base"], "sub"),
        user_filter: or(&f.user_filter, &defaults.user_filter),
        attr_username: or(&f.attr_username, &defaults.attr_username),
        attr_display: or(&f.attr_display, &defaults.attr_display),
        attr_email: or(&f.attr_email, &defaults.attr_email),
        group_base: f.group_base.trim().to_string(),
        group_scope: one_of(&f.group_scope, &["sub", "one", "base"], "sub"),
        group_filter: or(&f.group_filter, &defaults.group_filter),
        attr_group: or(&f.attr_group, &defaults.attr_group),
        nested: one_of(&f.nested, &["off", "recursive", "ad"], "recursive"),
        button_label: f.button_label.trim().to_string(),
        url,
    };
    let mut problem = crate::tls::parse_ca(&cfg.ca_cert).err();
    if problem.is_none() && cfg.enabled {
        if !(cfg.url.starts_with("ldap://") || cfg.url.starts_with("ldaps://")) {
            problem = Some("The server URL must start with ldap:// or ldaps://".into());
        } else if cfg.user_base.is_empty() {
            problem = Some("At least one user search base is required".into());
        } else if !cfg.user_filter.contains("{username}") {
            problem = Some("The user filter must contain {username}".into());
        } else if moved && f.bind_password.is_empty() && !old.bind_password.is_empty() {
            problem = Some("The server URL changed, so the service account password must be entered again".into());
        }
    }
    if problem.is_some() {
        cfg.enabled = false;
    }
    set_setting(&state.db, "ldap", &cfg).await?;
    tracing::warn!("LDAP configuration changed by {} (enabled: {})", user.username, cfg.enabled);
    Ok(match problem {
        Some(p) => back("oidc&provider=ldap", "error", &format!("LDAP settings saved, but LDAP sign-in was left disabled: {p}")),
        None if cfg.enabled => back("oidc&provider=ldap", "msg", "LDAP settings saved and enabled. Use “Look up” below the form to check them."),
        None => back("oidc&provider=ldap", "msg", "LDAP settings saved. LDAP sign-in is disabled."),
    })
}

#[derive(Deserialize)]
pub struct LdapTestForm {
    username: String,
    #[serde(default)]
    password: String,
}

pub async fn test_ldap(State(state): State<AppState>, user: User, Form(f): Form<LdapTestForm>) -> Res<Response> {
    require(user.has("admin.oidc"))?;
    let cfg = ldap::load_config(&state.db).await;
    let password = (!f.password.is_empty()).then_some(f.password.as_str());
    Ok(match ldap::authenticate(&cfg, &f.username, password).await {
        Ok(u) => back(
            "oidc&provider=ldap",
            "msg",
            &format!(
                "Found {} — username “{}”, name “{}”, email {}. {}Groups ({}): {}",
                u.dn,
                u.username,
                u.display_name,
                u.email.as_deref().unwrap_or("none"),
                if password.is_some() { "Password accepted. " } else { "" },
                u.groups.len(),
                if u.groups.is_empty() { "none".to_string() } else { u.groups.join(", ") }
            ),
        ),
        Err(ldap::LdapError::Invalid) if password.is_some() => back("oidc&provider=ldap", "error", "No matching user, or the password was rejected."),
        Err(ldap::LdapError::Invalid) => back("oidc&provider=ldap", "error", "No entry matched that username under the configured search bases."),
        Err(ldap::LdapError::Unavailable(e)) => back("oidc&provider=ldap", "error", &format!("LDAP test failed: {e}")),
    })
}

// ---------------------------------------------------------------- SAML

async fn saml_panel(state: &AppState) -> Markup {
    let cfg = saml::load_config(&state.db).await;
    panel(
        "SAML 2.0",
        html! {
            @if !saml::AVAILABLE {
                div role="alert" class="alert alert-warning text-sm" {
                    "This build of Autokan was compiled without SAML support. Rebuild with the default features (needs libxml2 and xmlsec1) to use it."
                }
            }
            p class="text-sm opacity-70" { "Register Autokan with your identity provider as a service provider using these values:" }
            div class="grid sm:grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm items-center" {
                span class="opacity-70" { "Entity ID" } code class="bg-base-200 rounded px-2 py-1 w-fit select-all" { (saml::entity_id(state, &cfg)) }
                span class="opacity-70" { "ACS URL (HTTP-POST)" } code class="bg-base-200 rounded px-2 py-1 w-fit select-all" { (saml::acs_url(state)) }
                span class="opacity-70" { "SP metadata" } a class="link w-fit" href="/auth/saml/metadata" target="_blank" { (saml::metadata_url(state)) }
            }
            form method="post" action="/admin/saml" class="grid md:grid-cols-2 gap-4 max-w-3xl" {
                (field("IdP metadata URL (optional)", "If set, the metadata is downloaded from here each time you save.", html! {
                    input class="input w-full" name="idp_metadata_url" value=(cfg.idp_metadata_url) placeholder="https://idp.example.com/saml/metadata";
                }))
                (field("Sign-in button label", "", html! {
                    input class="input w-full" name="button_label" value=(cfg.button_label) placeholder="Sign in with SAML";
                }))
                div class="md:col-span-2" {
                    (field("IdP metadata XML", "Paste the identity provider's metadata. Its signing certificate is what responses are verified against, so only paste metadata you obtained from the provider itself.", html! {
                        textarea class="textarea w-full font-mono text-xs" name="idp_metadata" rows="6" spellcheck="false"
                            placeholder="<EntityDescriptor entityID=\"…\" …>" { (cfg.idp_metadata) }
                    }))
                }
                (ca_field(&cfg.ca_cert))
                (field("SP entity ID (optional)", "Defaults to the SP metadata URL above.", html! {
                    input class="input w-full" name="entity_id" value=(cfg.entity_id);
                }))
                (field("Groups attribute", "Assertion attribute holding group names (one value per group).", html! {
                    input class="input w-full font-mono text-sm" name="attr_groups" value=(cfg.groups_attribute());
                }))
                div class="md:col-span-2 grid md:grid-cols-3 gap-4" {
                    (field("Username attribute", "Blank = the NameID.", html! { input class="input w-full font-mono text-sm" name="attr_username" value=(cfg.attr_username); }))
                    (field("Display name attribute", "", html! { input class="input w-full font-mono text-sm" name="attr_display" value=(cfg.attr_display); }))
                    (field("Email attribute", "", html! { input class="input w-full font-mono text-sm" name="attr_email" value=(cfg.attr_email); }))
                }
                label class="flex items-center gap-3 md:col-span-2" {
                    input type="checkbox" class="toggle toggle-primary" name="enabled" checked[cfg.enabled];
                    span { "Enable SAML sign-in" }
                }
                div class="md:col-span-2" { button class="btn btn-primary btn-sm" { "Save" } }
            }
            p class="text-xs opacity-60" {
                "Sign-in is always started from Autokan (IdP-initiated sign-in is refused). Responses or assertions must be signed with RSA or ECDSA and SHA-256 or stronger."
            }
        },
    )
}

#[derive(Deserialize)]
pub struct SamlForm {
    #[serde(default)]
    idp_metadata: String,
    #[serde(default)]
    idp_metadata_url: String,
    #[serde(default)]
    ca_cert: String,
    #[serde(default)]
    entity_id: String,
    #[serde(default)]
    attr_username: String,
    #[serde(default)]
    attr_display: String,
    #[serde(default)]
    attr_email: String,
    #[serde(default)]
    attr_groups: String,
    #[serde(default)]
    button_label: String,
    enabled: Option<String>,
}

pub async fn save_saml(State(state): State<AppState>, user: User, Form(f): Form<SamlForm>) -> Res<Response> {
    require(user.has("admin.oidc"))?;
    let mut cfg = saml::SamlConfig {
        enabled: f.enabled.is_some(),
        idp_metadata: f.idp_metadata.trim().to_string(),
        idp_metadata_url: f.idp_metadata_url.trim().to_string(),
        ca_cert: f.ca_cert.trim().to_string(),
        entity_id: f.entity_id.trim().to_string(),
        attr_username: f.attr_username.trim().to_string(),
        attr_display: f.attr_display.trim().to_string(),
        attr_email: f.attr_email.trim().to_string(),
        attr_groups: f.attr_groups.trim().to_string(),
        button_label: f.button_label.trim().to_string(),
    };
    let mut problem = crate::tls::parse_ca(&cfg.ca_cert).err();
    if problem.is_none() && !cfg.idp_metadata_url.is_empty() {
        // Fetch over TLS only: the metadata carries the certificate every response is checked against.
        let fetched = async {
            if !cfg.idp_metadata_url.starts_with("https://") {
                return Err("The IdP metadata URL must use https:// (or paste the XML instead)".to_string());
            }
            let http = crate::tls::http_client(&state.http, &cfg.ca_cert)?;
            let resp = http
                .get(&cfg.idp_metadata_url)
                .timeout(std::time::Duration::from_secs(20))
                .send()
                .await
                .map_err(|e| format!("Could not download the IdP metadata: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("The IdP metadata URL returned {}", resp.status()));
            }
            resp.text().await.map_err(|e| format!("Could not read the IdP metadata: {e}"))
        }
        .await;
        match fetched {
            Ok(xml) => cfg.idp_metadata = xml.trim().to_string(),
            Err(e) => problem = Some(e),
        }
    }
    if problem.is_none() && cfg.enabled {
        if cfg.idp_metadata.is_empty() {
            problem = Some("IdP metadata is required".into());
        } else {
            problem = saml::validate(&state, &cfg).err();
        }
    }
    if problem.is_some() {
        cfg.enabled = false;
    }
    set_setting(&state.db, "saml", &cfg).await?;
    tracing::warn!("SAML configuration changed by {} (enabled: {})", user.username, cfg.enabled);
    Ok(match problem {
        Some(p) => back("oidc&provider=saml", "error", &format!("SAML settings saved, but SAML sign-in was left disabled: {p}")),
        None if cfg.enabled => back("oidc&provider=saml", "msg", "SAML settings saved and enabled."),
        None => back("oidc&provider=saml", "msg", "SAML settings saved. SAML sign-in is disabled."),
    })
}

// ---------------------------------------------------------------- RBAC

async fn rbac_tab(state: &AppState) -> Res<Markup> {
    let roles: Vec<(i64, String, String)> = query_as("SELECT id, name, permissions FROM roles ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let mappings: Vec<(i64, String, String)> = query_as(
        "SELECT m.id, m.group_name, r.name FROM group_mappings m JOIN roles r ON r.id = m.role_id ORDER BY m.group_name, r.name",
    )
    .fetch_all(&state.db)
    .await?;
    // Every group any signed-in user arrived with, and which kinds of provider reported it.
    let seen_rows: Vec<(String, String)> =
        query_as("SELECT DISTINCT g.group_name, u.kind FROM user_groups g JOIN users u ON u.id = g.user_id ORDER BY g.group_name, u.kind")
            .fetch_all(&state.db)
            .await?;
    let mut seen: Vec<(String, Vec<&str>)> = Vec::new();
    for (group, kind) in &seen_rows {
        let source = match kind.as_str() {
            "ldap" => "LDAP",
            "saml" => "SAML",
            _ => "OIDC",
        };
        match seen.last_mut() {
            Some((name, sources)) if name == group => sources.push(source),
            _ => seen.push((group.clone(), vec![source])),
        }
    }
    Ok(html! {
        (panel("Roles", html! {
            p class="text-sm opacity-70" {
                "A role is a bundle of platform permissions. Map roles to groups from your identity provider below (OpenID Connect, LDAP or SAML alike); users receive the union of every role their groups map to. Team and project permissions are granted inside each team."
            }
            @for (id, name, perms) in &roles {
                @let granted: Vec<String> = serde_json::from_str(perms).unwrap_or_default();
                div class="border border-base-300 rounded-box p-4 flex flex-col gap-3" {
                    form method="post" action={ "/admin/roles/" (id) } class="flex flex-col gap-3" {
                        input class="input input-sm w-64 font-semibold" name="name" value=(name) required;
                        (perm_checkboxes(&granted))
                        div { button class="btn btn-sm btn-primary" { "Save role" } }
                    }
                    form method="post" action={ "/admin/roles/" (id) "/delete" } data-confirm="Delete this role and its group mappings?" {
                        button class="btn btn-xs btn-ghost text-error" { "Delete role" }
                    }
                }
            }
            form method="post" action="/admin/roles" class="join" {
                input class="input input-sm join-item" name="name" placeholder="New role name" required;
                button class="btn btn-sm btn-primary join-item" { "Create role" }
            }
        }))
        (panel("Group mappings", html! {
            table class="table table-sm max-w-2xl" {
                thead { tr { th { "Group" } th { "Role" } th {} } }
                tbody {
                    @for (id, group, role) in &mappings {
                        tr {
                            td class="font-mono" { (group) }
                            td { (role) }
                            td class="text-right" {
                                form method="post" action={ "/admin/mappings/" (id) "/delete" } {
                                    button class="btn btn-ghost btn-xs text-error" { "Remove" }
                                }
                            }
                        }
                    }
                }
            }
            @if mappings.is_empty() { p class="text-sm opacity-60" { "No groups are mapped yet, so only the break glass account has platform permissions." } }
            @if roles.is_empty() {
                p class="text-sm opacity-60" { "Create a role first." }
            } @else {
                form method="post" action="/admin/mappings" class="flex flex-wrap gap-2 items-center" {
                    input class="input input-sm w-64 font-mono" name="group_name" placeholder="Group name, exactly as the provider sends it" list="seen-groups" required;
                    datalist id="seen-groups" { @for (g, _) in &seen { option value=(g) {} } }
                    select class="select select-sm w-48" name="role_id" {
                        @for (id, name, _) in &roles { option value=(id) { (name) } }
                    }
                    button class="btn btn-sm btn-primary" { "Map group" }
                }
            }
            @if !seen.is_empty() {
                p class="text-xs opacity-60" {
                    "Groups seen in sign-ins so far: "
                    @for (i, (g, sources)) in seen.iter().enumerate() {
                        @if i > 0 { ", " }
                        span class="font-mono" { (g) } " (" (sources.join(", ")) ")"
                    }
                }
            }
            p class="text-xs opacity-60" { "A mapping applies to that group name from every sign-in method. Group membership is refreshed each time a user signs in; role and mapping changes apply immediately." }
        }))
    })
}

fn perm_checkboxes(granted: &[String]) -> Markup {
    html! {
        div class="grid md:grid-cols-2 gap-x-6 gap-y-1" {
            @for (key, help) in PERMISSIONS {
                label class="flex items-start gap-2 text-sm" {
                    input type="checkbox" class="checkbox checkbox-sm mt-0.5" name="perms" value=(key) checked[granted.iter().any(|g| g == key)];
                    span { span class="font-mono" { (key) } span class="opacity-60" { " — " (help) } }
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub struct RoleForm {
    name: String,
    #[serde(default)]
    perms: Vec<String>,
}

pub async fn create_role(State(state): State<AppState>, user: User, Form(f): Form<RoleForm>) -> Res<Response> {
    require(user.has("admin.rbac"))?;
    let name = clean_name(&f.name, "Role name")?;
    let r = query("INSERT INTO roles (name) VALUES (?)").bind(&name).execute(&state.db).await;
    Ok(match r {
        Ok(_) => back("rbac", "msg", "Role created. Tick its permissions and save."),
        Err(_) => back("rbac", "error", "A role with that name already exists"),
    })
}

pub async fn update_role(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<RoleForm>) -> Res<Response> {
    require(user.has("admin.rbac"))?;
    let perms: Vec<&String> = f.perms.iter().filter(|p| PERMISSIONS.iter().any(|(k, _)| k == p)).collect();
    // Only full platform admins may hand out platform.admin, so admin.rbac alone is not a path to it
    // by ticking the box. (admin.rbac holders can still grant every other permission.)
    if perms.iter().any(|p| *p == "platform.admin") && !user.has("platform.admin") {
        return Err(AppError::Forbidden);
    }
    let r = query("UPDATE roles SET name = ?, permissions = ? WHERE id = ?")
        .bind(clean_name(&f.name, "Role name")?)
        .bind(serde_json::to_string(&perms)?)
        .bind(id)
        .execute(&state.db)
        .await;
    tracing::warn!("role {id} updated by {}: {perms:?}", user.username);
    Ok(match r {
        Ok(_) => back("rbac", "msg", "Role saved."),
        Err(_) => back("rbac", "error", "A role with that name already exists"),
    })
}

pub async fn delete_role(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    require(user.has("admin.rbac"))?;
    query("DELETE FROM group_mappings WHERE role_id = ?").bind(id).execute(&state.db).await?;
    query("DELETE FROM roles WHERE id = ?").bind(id).execute(&state.db).await?;
    Ok(back("rbac", "msg", "Role deleted."))
}

#[derive(Deserialize)]
pub struct MappingForm {
    group_name: String,
    role_id: i64,
}

pub async fn create_mapping(State(state): State<AppState>, user: User, Form(f): Form<MappingForm>) -> Res<Response> {
    require(user.has("admin.rbac"))?;
    let group = f.group_name.trim();
    if group.is_empty() {
        return Err(bad("Group name cannot be empty"));
    }
    let (perms,): (String,) = query_as("SELECT permissions FROM roles WHERE id = ?")
        .bind(f.role_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if perms.contains("\"platform.admin\"") && !user.has("platform.admin") {
        return Err(AppError::Forbidden);
    }
    let mapped: Option<(i64,)> = query_as("SELECT id FROM group_mappings WHERE group_name = ? AND role_id = ?")
        .bind(group)
        .bind(f.role_id)
        .fetch_optional(&state.db)
        .await?;
    if mapped.is_none() {
        query("INSERT INTO group_mappings (group_name, role_id) VALUES (?, ?)")
            .bind(group)
            .bind(f.role_id)
            .execute(&state.db)
            .await?;
    }
    tracing::warn!("group '{group}' mapped to role {} by {}", f.role_id, user.username);
    Ok(back("rbac", "msg", "Group mapped."))
}

pub async fn delete_mapping(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    require(user.has("admin.rbac"))?;
    query("DELETE FROM group_mappings WHERE id = ?").bind(id).execute(&state.db).await?;
    Ok(back("rbac", "msg", "Mapping removed."))
}

// ---------------------------------------------------------------- Users

async fn users_tab(state: &AppState) -> Res<Markup> {
    let rows: Vec<(i64, String, String, String, Option<String>, bool, Option<String>)> = query_as(
        "SELECT id, kind, username, display_name, email, disabled, last_login FROM users ORDER BY kind, display_name",
    )
    .fetch_all(&state.db)
    .await?;
    let memberships: Vec<(i64, String)> = query_as("SELECT user_id, group_name FROM user_groups ORDER BY group_name")
        .fetch_all(&state.db)
        .await?;
    let users: Vec<_> = rows
        .into_iter()
        .map(|(id, kind, username, display, email, disabled, last_login)| {
            let groups: Vec<&str> = memberships.iter().filter(|m| m.0 == id).map(|m| m.1.as_str()).collect();
            let groups = (!groups.is_empty()).then(|| groups.join(", "));
            (id, kind, username, display, email, disabled, last_login, groups)
        })
        .collect();
    Ok(panel(
        "Users",
        html! {
            div class="overflow-x-auto" {
                table class="table table-sm" {
                    thead { tr { th { "Name" } th { "Username" } th { "Type" } th { "Groups" } th { "Last sign-in (UTC)" } th {} } }
                    tbody {
                        @for (id, kind, username, display, email, disabled, last_login, groups) in &users {
                            tr class=(if *disabled { "opacity-50" } else { "" }) {
                                td { (display) @if let Some(e) = email { div class="text-xs opacity-60" { (e) } } }
                                td class="font-mono" { (username) }
                                td { (match kind.as_str() { "breakglass" => "Break glass", "ldap" => "LDAP", "saml" => "SAML", _ => "OIDC" }) }
                                td class="font-mono text-xs" { (groups.as_deref().unwrap_or("")) }
                                td { (last_login.as_deref().unwrap_or("never")) }
                                td class="text-right" {
                                    @if kind != "breakglass" {
                                        form method="post" action={ "/admin/users/" (id) "/disabled" } {
                                            input type="hidden" name="disabled" value=(if *disabled { "0" } else { "1" });
                                            button class="btn btn-ghost btn-xs" { (if *disabled { "Enable" } else { "Disable" }) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            p class="text-xs opacity-60" { "Users are created on their first single sign-on. Disabling a user signs them out and blocks further sign-ins." }
        },
    ))
}

#[derive(Deserialize)]
pub struct DisabledForm {
    disabled: String,
}

pub async fn set_user_disabled(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<DisabledForm>) -> Res<Response> {
    require(user.has("admin.users"))?;
    let disabled = f.disabled == "1";
    query("UPDATE users SET disabled = ? WHERE id = ? AND kind != 'breakglass'")
        .bind(disabled)
        .bind(id)
        .execute(&state.db)
        .await?;
    if disabled {
        query("DELETE FROM sessions WHERE user_id = ? AND user_id IN (SELECT id FROM users WHERE kind != 'breakglass')")
            .bind(id)
            .execute(&state.db)
            .await?;
    }
    Ok(back("users", "msg", if disabled { "User disabled." } else { "User enabled." }))
}

// ---------------------------------------------------------------- Agent services

fn service_form(action: &str, svc: Option<&Service>) -> Markup {
    let kind = svc.map(|s| s.kind.as_str()).unwrap_or("anthropic");
    let models = svc.map(|s| s.model_list().join("\n")).unwrap_or_default();
    html! {
        form method="post" action=(action) class="grid md:grid-cols-2 gap-4" {
            (field("Name", "Shown to project managers when they pick a model.", html! {
                input class="input w-full" name="name" value=[svc.map(|s| s.name.as_str())] placeholder="e.g. Claude" required;
            }))
            (field("Provider", "", html! {
                select class="select w-full" name="kind" {
                    @for (key, label, _) in llm::KINDS { option value=(key) selected[*key == kind] { (label) } }
                }
            }))
            (field("Base URL", "Leave blank for Claude, ChatGPT and Gemini to use the provider's public API. Required for OpenAI-compatible servers (e.g. http://localhost:11434/v1).", html! {
                input class="input w-full" name="base_url" value=[svc.map(|s| s.base_url.as_str())] placeholder="provider default";
            }))
            (field("API key", if svc.is_some() { "Leave blank to keep the stored key." } else { "Optional for self-hosted servers that need none." }, html! {
                input class="input w-full" type="password" name="api_key" autocomplete="off"
                    placeholder=(if svc.is_some_and(|s| !s.api_key.is_empty()) { "••••••••" } else { "not set" });
            }))
            div class="md:col-span-2" {
                (field("Models", "One model id per line, exactly as the provider names it. These are the models projects and columns can choose; the first is the service's default. Save, then use “Test connection” to see the ids your key can use.", html! {
                    textarea class="textarea w-full font-mono text-sm" name="models" rows="4" required
                        placeholder="claude-opus-5-5\nclaude-sonnet-5-5\nclaude-haiku-5-5" { (models) }
                }))
            }
            div class="md:col-span-2 flex flex-wrap items-center gap-4" {
                label class="flex items-center gap-2" {
                    input type="checkbox" class="toggle toggle-primary toggle-sm" name="enabled" checked[svc.is_none_or(|s| s.enabled)];
                    span class="text-sm" { "Enabled" }
                }
                @if svc.is_some() {
                    label class="flex items-center gap-2" {
                        input type="checkbox" class="checkbox checkbox-sm" name="clear_key";
                        span class="text-sm" { "Remove the stored API key" }
                    }
                }
                button class="btn btn-primary btn-sm" { (if svc.is_some() { "Save service" } else { "Add service" }) }
            }
        }
    }
}

async fn agent_tab(state: &AppState) -> Res<Markup> {
    let services = llm::services(&state.db).await?;
    let defaults: AgentDefaults = get_setting(&state.db, "agent").await;
    Ok(html! {
        (panel("Agent services", html! {
            p class="text-sm opacity-70" {
                "The LLM providers the board agents may use. Each project picks its default service and model from this list, and a column can override it for its “Send To Agent” prompt. API keys stay on the server and are never shown again."
            }
            @for s in &services {
                @let models = s.model_list();
                details class="collapse collapse-arrow border border-base-300 bg-base-100" {
                    summary class="collapse-title flex flex-wrap items-center gap-2 font-medium" {
                        (s.name)
                        span class="badge badge-sm badge-soft badge-primary" { (s.kind_label()) }
                        span class="text-xs opacity-60 font-normal" { (models.len()) (if models.len() == 1 { " model" } else { " models" }) }
                        @if !s.enabled { span class="badge badge-sm badge-warning" { "disabled" } }
                    }
                    div class="collapse-content flex flex-col gap-3" {
                        (service_form(&format!("/admin/services/{}", s.id), Some(s)))
                        div class="flex gap-2 border-t border-base-300 pt-3" {
                            form method="post" action={ "/admin/services/" (s.id) "/test" } {
                                button class="btn btn-sm" { "Test connection" }
                            }
                            form method="post" action={ "/admin/services/" (s.id) "/delete" }
                                data-confirm="Delete this service? Projects and columns that use it fall back to their defaults." {
                                button class="btn btn-sm btn-ghost text-error" { "Delete" }
                            }
                        }
                    }
                }
            }
            details class="collapse collapse-arrow border border-dashed border-base-300" open[services.is_empty()] {
                summary class="collapse-title font-medium" { "+ Add a service" }
                div class="collapse-content" { (service_form("/admin/services", None)) }
            }
        }))
        (panel("Platform defaults", html! {
            form method="post" action="/admin/agent" class="flex flex-col gap-4 max-w-3xl" {
                (field("Default service and model", "Used by every project that has not chosen its own.", html! {
                    (target_select("agent_target", &services, &llm::target_value(defaults.service_id, &defaults.model), "— none: projects must choose their own —"))
                }))
                (field("Extra instructions for every board's agent", "", html! {
                    textarea class="textarea w-full text-sm" name="system_prompt" rows="4" { (defaults.system_prompt) }
                }))
                div { button class="btn btn-primary btn-sm" { "Save defaults" } }
            }
        }))
    })
}

#[derive(Deserialize)]
pub struct ServiceForm {
    name: String,
    kind: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    models: String,
    enabled: Option<String>,
    clear_key: Option<String>,
}

/// Validated (name, kind, base_url, models JSON) from a service form.
fn service_fields(f: &ServiceForm) -> Res<(String, String, String, String)> {
    let name = clean_name(&f.name, "Service name")?;
    if !llm::KINDS.iter().any(|k| k.0 == f.kind) {
        return Err(bad("Unknown provider"));
    }
    let base_url = f.base_url.trim().trim_end_matches('/').to_string();
    check_base_url(&base_url)?;
    if f.kind == "compatible" && base_url.is_empty() {
        return Err(bad("An OpenAI-compatible service needs a base URL"));
    }
    let mut models: Vec<String> = Vec::new();
    for m in f.models.lines().map(str::trim).filter(|m| !m.is_empty()) {
        if m.contains('|') {
            return Err(bad("Model ids cannot contain '|'"));
        }
        if !models.iter().any(|x| x == m) {
            models.push(m.to_string());
        }
    }
    if models.is_empty() {
        return Err(bad("List at least one model"));
    }
    Ok((name, f.kind.clone(), base_url, serde_json::to_string(&models)?))
}

pub async fn create_service(State(state): State<AppState>, user: User, Form(f): Form<ServiceForm>) -> Res<Response> {
    require(user.has("admin.agent"))?;
    let (name, kind, base_url, models) = service_fields(&f)?;
    let r = query("INSERT INTO llm_services (name, kind, base_url, api_key, models, enabled) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&name)
        .bind(&kind)
        .bind(&base_url)
        .bind(f.api_key.trim())
        .bind(&models)
        .bind(f.enabled.is_some())
        .execute(&state.db)
        .await;
    tracing::warn!("agent service '{name}' ({kind}) created by {}", user.username);
    Ok(match r {
        Ok(_) => back("agent", "msg", "Service added. Use “Test connection” to check the key."),
        Err(_) => back("agent", "error", "A service with that name already exists"),
    })
}

pub async fn update_service(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<ServiceForm>) -> Res<Response> {
    require(user.has("admin.agent"))?;
    let old = llm::service(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let (name, kind, base_url, models) = service_fields(&f)?;
    // A stored key is only ever sent to the endpoint it was entered for: changing where the
    // service points requires entering the key again.
    let moved = base_url != old.base_url || kind != old.kind;
    let api_key = if !f.api_key.trim().is_empty() {
        f.api_key.trim().to_string()
    } else if f.clear_key.is_some() || moved {
        String::new()
    } else {
        old.api_key.clone()
    };
    let dropped_key = moved && f.api_key.trim().is_empty() && !old.api_key.is_empty();
    let r = query("UPDATE llm_services SET name = ?, kind = ?, base_url = ?, api_key = ?, models = ?, enabled = ? WHERE id = ?")
        .bind(&name)
        .bind(&kind)
        .bind(&base_url)
        .bind(&api_key)
        .bind(&models)
        .bind(f.enabled.is_some())
        .bind(id)
        .execute(&state.db)
        .await;
    tracing::warn!("agent service '{name}' updated by {}", user.username);
    Ok(match r {
        Ok(_) if dropped_key => back("agent", "error", "Saved, but the stored API key was removed because the provider or base URL changed. Enter the key again."),
        Ok(_) => back("agent", "msg", "Service saved."),
        Err(_) => back("agent", "error", "A service with that name already exists"),
    })
}

pub async fn delete_service(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    require(user.has("admin.agent"))?;
    for sql in [
        "UPDATE projects SET agent_service_id = NULL, agent_model = '' WHERE agent_service_id = ?",
        "UPDATE columns SET agent_service_id = NULL, agent_model = '' WHERE agent_service_id = ?",
        "DELETE FROM llm_services WHERE id = ?",
    ] {
        query(sql).bind(id).execute(&state.db).await?;
    }
    let mut defaults: AgentDefaults = get_setting(&state.db, "agent").await;
    if defaults.service_id == Some(id) {
        defaults.service_id = None;
        defaults.model.clear();
        set_setting(&state.db, "agent", &defaults).await?;
    }
    Ok(back("agent", "msg", "Service deleted."))
}

pub async fn test_service(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    require(user.has("admin.agent"))?;
    let svc = llm::service(&state.db, id).await?.ok_or(AppError::NotFound)?;
    Ok(match llm::list_models(&state.http, &svc).await {
        Ok(ids) => {
            let configured = svc.model_list();
            let missing: Vec<&String> = configured.iter().filter(|m| !ids.contains(m)).collect();
            let shown: Vec<&str> = ids.iter().take(60).map(String::as_str).collect();
            let mut msg = format!("{} is reachable. {} models available: {}{}", svc.name, ids.len(), shown.join(", "), if ids.len() > 60 { ", …" } else { "" });
            if missing.is_empty() {
                back("agent", "msg", &msg)
            } else {
                msg.push_str(&format!(" — but these configured models were not in that list: {}", missing.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ")));
                back("agent", "error", &msg)
            }
        }
        Err(e) => back("agent", "error", &format!("Connection test failed: {e}")),
    })
}

#[derive(Deserialize)]
pub struct AgentForm {
    #[serde(default)]
    agent_target: String,
    #[serde(default)]
    system_prompt: String,
}

pub async fn save_agent(State(state): State<AppState>, user: User, Form(f): Form<AgentForm>) -> Res<Response> {
    require(user.has("admin.agent"))?;
    let (service_id, model) = llm::parse_target(&state.db, &f.agent_target).await?;
    let defaults = AgentDefaults { service_id, model, system_prompt: f.system_prompt.trim().to_string() };
    set_setting(&state.db, "agent", &defaults).await?;
    Ok(back("agent", "msg", "Defaults saved."))
}

// ---------------------------------------------------------------- Break glass account

fn account_tab() -> Markup {
    panel(
        "Break glass password",
        html! {
            p class="text-sm opacity-70" {
                "The break glass account is a local full administrator that works even when single sign-on is down. Keep its password in a vault and use it only for setup and emergencies. If it is ever lost, run "
                code { "autokan reset-breakglass" } " on the server."
            }
            form method="post" action="/admin/password" class="flex flex-col gap-3 max-w-sm" {
                input class="input w-full" type="password" name="current" placeholder="Current password" autocomplete="current-password" required;
                input class="input w-full" type="password" name="new" placeholder="New password (at least 12 characters)" autocomplete="new-password" minlength="12" required;
                input class="input w-full" type="password" name="confirm" placeholder="Repeat new password" autocomplete="new-password" required;
                div { button class="btn btn-primary btn-sm" { "Change password" } }
            }
            p class="text-xs opacity-60" { "Changing the password signs the break glass account out everywhere." }
        },
    )
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    new: String,
    confirm: String,
}

pub async fn change_password(State(state): State<AppState>, user: User, Form(f): Form<PasswordForm>) -> Res<Response> {
    require(user.is_breakglass())?;
    let (hash,): (Option<String>,) = query_as("SELECT password_hash FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if !auth::verify_password(&f.current, hash.as_deref().unwrap_or("")) {
        return Ok(back("account", "error", "Current password is incorrect"));
    }
    if f.new.chars().count() < 12 {
        return Ok(back("account", "error", "New password must be at least 12 characters"));
    }
    if f.new != f.confirm {
        return Ok(back("account", "error", "New passwords do not match"));
    }
    auth::set_password(&state.db, user.id, &f.new).await?;
    tracing::warn!("break glass password changed");
    Ok(redirect("/login?error=Password+changed.+Please+sign+in+again."))
}
