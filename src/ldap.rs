//! LDAP / Active Directory sign-in.
//!
//! Flow: bind with the service account, find exactly one user entry under the configured search
//! bases, verify the password by binding as that entry, then collect the user's groups (optionally
//! following nested membership). Group names feed the same RBAC mappings as OIDC groups.
use crate::db::{get_setting, Db};
use ldap3::{ldap_escape, Ldap, LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);
/// Bounds on nested-group expansion, so a huge or cyclic directory cannot stall a sign-in.
const MAX_NESTING_DEPTH: usize = 20;
const MAX_GROUPS: usize = 2000;
/// Active Directory's LDAP_MATCHING_RULE_IN_CHAIN: the server walks nested membership itself.
const AD_IN_CHAIN: &str = "1.2.840.113556.1.4.1941";

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct LdapConfig {
    pub enabled: bool,
    /// `ldap://host:389` or `ldaps://host:636`.
    pub url: String,
    /// Upgrade an `ldap://` connection with StartTLS before binding.
    pub starttls: bool,
    /// PEM bundle trusted in addition to the system roots.
    pub ca_cert: String,
    pub bind_dn: String,
    pub bind_password: String,
    /// One search base per line (the whole tree, or specific OUs).
    pub user_base: String,
    /// `sub` (base and everything below), `one` (direct children only) or `base` (the entry itself).
    pub user_scope: String,
    /// `{username}` is replaced with the escaped login name.
    pub user_filter: String,
    pub attr_username: String,
    pub attr_display: String,
    pub attr_email: String,
    pub group_base: String,
    pub group_scope: String,
    /// `{dn}` is the user's DN, `{username}` their login name.
    pub group_filter: String,
    pub attr_group: String,
    /// `off`, `recursive` (walk parent groups, any server) or `ad` (Active Directory in-chain match).
    pub nested: String,
    pub button_label: String,
}

impl Default for LdapConfig {
    fn default() -> Self {
        LdapConfig {
            enabled: false,
            url: String::new(),
            starttls: false,
            ca_cert: String::new(),
            bind_dn: String::new(),
            bind_password: String::new(),
            user_base: String::new(),
            user_scope: "sub".into(),
            user_filter: "(&(objectClass=person)(|(uid={username})(sAMAccountName={username})(mail={username})))".into(),
            attr_username: "uid".into(),
            attr_display: "cn".into(),
            attr_email: "mail".into(),
            group_base: String::new(),
            group_scope: "sub".into(),
            group_filter: "(|(member={dn})(uniqueMember={dn})(memberUid={username}))".into(),
            attr_group: "cn".into(),
            nested: "recursive".into(),
            button_label: String::new(),
        }
    }
}

impl LdapConfig {
    pub fn usable(&self) -> bool {
        self.enabled && !self.url.trim().is_empty() && !bases(&self.user_base).is_empty()
    }
}

pub async fn load_config(db: &Db) -> LdapConfig {
    get_setting(db, "ldap").await
}

pub struct LdapUser {
    pub dn: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

pub enum LdapError {
    /// Unknown user or wrong password. Deliberately indistinguishable to the person signing in.
    Invalid,
    /// The directory or the configuration is at fault; the detail is for administrators.
    Unavailable(String),
}

fn bases(s: &str) -> Vec<&str> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect()
}

fn scope(s: &str) -> Scope {
    match s {
        "one" => Scope::OneLevel,
        "base" => Scope::Base,
        _ => Scope::Subtree,
    }
}

fn unavailable(what: &str) -> impl Fn(ldap3::LdapError) -> LdapError + '_ {
    move |e| LdapError::Unavailable(format!("{what}: {e}"))
}

async fn connect(cfg: &LdapConfig) -> Result<Ldap, LdapError> {
    let mut settings = LdapConnSettings::new().set_conn_timeout(TIMEOUT).set_starttls(cfg.starttls);
    if let Some(tls) = crate::tls::rustls_config(&cfg.ca_cert).map_err(LdapError::Unavailable)? {
        settings = settings.set_config(tls);
    }
    let (conn, mut ldap) = LdapConnAsync::with_settings(settings, cfg.url.trim())
        .await
        .map_err(unavailable("Could not connect to the LDAP server"))?;
    ldap3::drive!(conn);
    ldap.with_timeout(TIMEOUT);
    Ok(ldap)
}

fn first(entry: &SearchEntry, attr: &str) -> Option<String> {
    // Attribute names are case-insensitive in LDAP.
    entry
        .attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(attr))
        .and_then(|(_, v)| v.first())
        .filter(|v| !v.trim().is_empty())
        .cloned()
}

/// Search every configured base and return (dn, group name) pairs.
async fn search_groups(ldap: &mut Ldap, cfg: &LdapConfig, filter: &str) -> Result<Vec<(String, String)>, LdapError> {
    let mut found = Vec::new();
    for base in bases(&cfg.group_base) {
        let (entries, _) = ldap
            .search(base, scope(&cfg.group_scope), filter, vec![cfg.attr_group.as_str()])
            .await
            .and_then(|r| r.success())
            .map_err(unavailable("The group search failed"))?;
        for entry in entries {
            let entry = SearchEntry::construct(entry);
            if let Some(name) = first(&entry, &cfg.attr_group) {
                found.push((entry.dn, name));
            }
        }
    }
    Ok(found)
}

/// Groups the user belongs to, directly and (depending on `cfg.nested`) through other groups.
async fn groups_of(ldap: &mut Ldap, cfg: &LdapConfig, user_dn: &str, username: &str) -> Result<Vec<String>, LdapError> {
    if bases(&cfg.group_base).is_empty() {
        return Ok(Vec::new());
    }
    let dn = ldap_escape(user_dn);
    if cfg.nested == "ad" {
        // One query: the server follows the membership chain to any depth.
        let filter = format!("(member:{AD_IN_CHAIN}:={dn})");
        return Ok(search_groups(ldap, cfg, &filter).await?.into_iter().map(|g| g.1).collect());
    }
    let filter = cfg.group_filter.replace("{dn}", &dn).replace("{username}", &ldap_escape(username));
    let direct = search_groups(ldap, cfg, &filter).await?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut names: Vec<String> = Vec::new();
    let mut frontier: Vec<String> = Vec::new();
    for (group_dn, name) in direct {
        if seen.insert(group_dn.to_lowercase()) {
            names.push(name);
            frontier.push(group_dn);
        }
    }
    if cfg.nested == "recursive" {
        // Breadth-first walk up the "is a member of" relation. `seen` makes cycles harmless.
        for _ in 0..MAX_NESTING_DEPTH {
            let mut next = Vec::new();
            for group_dn in &frontier {
                let g = ldap_escape(group_dn.as_str());
                let filter = format!("(|(member={g})(uniqueMember={g}))");
                for (parent_dn, name) in search_groups(ldap, cfg, &filter).await? {
                    if seen.len() < MAX_GROUPS && seen.insert(parent_dn.to_lowercase()) {
                        names.push(name);
                        next.push(parent_dn);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
    }
    Ok(names)
}

/// Look a user up and, when `password` is given, verify it. Without a password this is the
/// admin panel's dry run: it shows what the directory returns without signing anyone in.
pub async fn authenticate(cfg: &LdapConfig, username: &str, password: Option<&str>) -> Result<LdapUser, LdapError> {
    let username = username.trim();
    // An empty password would be an "unauthenticated bind", which many servers accept.
    if username.is_empty() || password.is_some_and(|p| p.is_empty()) {
        return Err(LdapError::Invalid);
    }
    let mut ldap = connect(cfg).await?;
    if !cfg.bind_dn.trim().is_empty() {
        ldap.simple_bind(cfg.bind_dn.trim(), &cfg.bind_password)
            .await
            .and_then(|r| r.success())
            .map_err(unavailable("The service account could not bind"))?;
    }

    let filter = cfg.user_filter.replace("{username}", &ldap_escape(username));
    let attrs = vec![cfg.attr_username.as_str(), cfg.attr_display.as_str(), cfg.attr_email.as_str()];
    let mut matches: Vec<SearchEntry> = Vec::new();
    for base in bases(&cfg.user_base) {
        let (entries, _) = ldap
            .search(base, scope(&cfg.user_scope), &filter, attrs.clone())
            .await
            .and_then(|r| r.success())
            .map_err(unavailable("The user search failed"))?;
        matches.extend(entries.into_iter().map(SearchEntry::construct));
    }
    // Overlapping search bases can return the same entry twice.
    let mut seen = HashSet::new();
    matches.retain(|e| seen.insert(e.dn.to_lowercase()));
    let entry = match matches.len() {
        0 => return Err(LdapError::Invalid),
        1 => matches.remove(0),
        n => return Err(LdapError::Unavailable(format!("The user filter matched {n} entries for this login; it must match exactly one"))),
    };

    if let Some(password) = password {
        // Verify the password on its own connection so the service bind stays in place for the group search.
        let mut user_conn = connect(cfg).await?;
        let bound = user_conn.simple_bind(&entry.dn, password).await.and_then(|r| r.success());
        let _ = user_conn.unbind().await;
        if bound.is_err() {
            return Err(LdapError::Invalid);
        }
    }

    let login = first(&entry, &cfg.attr_username).unwrap_or_else(|| username.to_string());
    let groups = groups_of(&mut ldap, cfg, &entry.dn, &login).await?;
    let _ = ldap.unbind().await;
    Ok(LdapUser {
        display_name: first(&entry, &cfg.attr_display).unwrap_or_else(|| login.clone()),
        email: first(&entry, &cfg.attr_email),
        username: login,
        dn: entry.dn,
        groups,
    })
}
