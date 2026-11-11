//! Jira integration, per card rather than per board: a card can have a Jira issue created from
//! it, and from then on Autokan keeps that issue supplied with the card's title, description
//! (under a link back to the card), comments and attached files, and can move it through its
//! workflow. Nothing is read back from Jira except the issue's status and available transitions.
//!
//! Works with Jira Cloud and with self-hosted Jira (Data Center / Server): both serve REST API
//! version 2, which takes text as Jira wiki markup, so that is the only API used.
use crate::db::{now, query, query_as, Db};
use crate::markdown::{IMAGE_EXTS, VIDEO_EXTS};
use crate::AppState;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

/// The organization-wide Jira connection (admin panel). Projects can opt into it instead of
/// configuring their own; they then only choose which Jira project their issues go to.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Platform {
    pub enabled: bool,
    pub base_url: String,
    pub auth: String,
    pub username: String,
    pub token: String,
    pub ca_cert: String,
}

impl Platform {
    pub fn usable(&self) -> bool {
        self.enabled && !self.base_url.is_empty() && !self.token.is_empty()
    }
}

pub async fn platform(db: &Db) -> Platform {
    crate::db::get_setting(db, "jira").await
}

/// A project's Jira settings as stored. With `source == "platform"` the connection fields are
/// unused and come from the organization's connection instead (see `settings`).
#[derive(Clone, Default)]
pub struct Settings {
    #[allow(dead_code)] // present so the struct maps one-to-one onto its table row
    pub project_id: i64,
    pub base_url: String,
    pub auth: String,
    pub username: String,
    pub token: String,
    pub project_key: String,
    pub issue_type: String,
    pub ca_cert: String,
    pub enabled: bool,
    /// "own" (this project's own connection) or "platform" (the organization's).
    pub source: String,
}
crate::row_struct!(Settings { project_id, base_url, auth, username, token, project_key, issue_type, ca_cert, enabled, source });

impl Settings {
    pub fn uses_platform(&self) -> bool {
        self.source == "platform"
    }
    /// Replace the connection fields with the organization's. The project keeps its own Jira
    /// project key and issue type, and is only enabled while the organization's connection is.
    pub fn with_platform(mut self, p: &Platform) -> Settings {
        self.base_url = p.base_url.clone();
        self.auth = p.auth.clone();
        self.username = p.username.clone();
        self.token = p.token.clone();
        self.ca_cert = p.ca_cert.clone();
        self.enabled = self.enabled && p.usable();
        self
    }
}

impl Settings {
    pub fn usable(&self) -> bool {
        self.enabled && !self.base_url.is_empty() && !self.project_key.is_empty() && !self.token.is_empty()
    }
    pub fn browse_url(&self, issue_key: &str) -> String {
        format!("{}/browse/{issue_key}", self.base_url.trim_end_matches('/'))
    }
}

/// The project's settings exactly as stored (for the settings form).
pub async fn stored_settings(db: &Db, project_id: i64) -> anyhow::Result<Option<Settings>> {
    query_as("SELECT project_id, base_url, auth, username, token, project_key, issue_type, ca_cert, enabled, source FROM jira_settings WHERE project_id = ?")
        .bind(project_id)
        .fetch_optional(db)
        .await
}

/// The connection a project actually uses: its own, or the organization's if it opted into that.
pub async fn settings(db: &Db, project_id: i64) -> anyhow::Result<Option<Settings>> {
    let Some(stored) = stored_settings(db, project_id).await? else { return Ok(None) };
    if stored.uses_platform() {
        return Ok(Some(stored.with_platform(&platform(db).await)));
    }
    Ok(Some(stored))
}

#[derive(Clone)]
pub struct Link {
    pub card_id: i64,
    pub issue_key: String,
    pub pushed_summary: String,
    pub pushed_description: String,
    pub last_error: String,
    pub synced_at: Option<String>,
}
crate::row_struct!(Link { card_id, issue_key, pushed_summary, pushed_description, last_error, synced_at });

pub async fn link(db: &Db, card_id: i64) -> anyhow::Result<Option<Link>> {
    query_as("SELECT card_id, issue_key, pushed_summary, pushed_description, last_error, synced_at FROM jira_links WHERE card_id = ?")
        .bind(card_id)
        .fetch_optional(db)
        .await
}

/// A Jira REST client for one project's connection.
pub struct Client {
    http: reqwest::Client,
    cfg: Settings,
}

pub struct Transition {
    pub id: String,
    pub name: String,
    pub to: String,
}

impl Client {
    pub fn new(state: &AppState, cfg: &Settings) -> Result<Self, String> {
        Ok(Client { http: crate::tls::http_client(&state.http, &cfg.ca_cert)?, cfg: cfg.clone() })
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/rest/api/2{path}", self.cfg.base_url.trim_end_matches('/'));
        let req = self.http.request(method, url).timeout(Duration::from_secs(60)).header("Accept", "application/json");
        // Jira Cloud: email + API token. Self-hosted: a personal access token (bearer), or username + password.
        if self.cfg.auth == "bearer" {
            req.bearer_auth(&self.cfg.token)
        } else {
            req.basic_auth(&self.cfg.username, Some(&self.cfg.token))
        }
    }

    async fn finish(what: &str, req: reqwest::RequestBuilder) -> Result<Value, String> {
        let resp = req.send().await.map_err(|e| format!("Could not reach Jira: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        // Jira reports problems as errorMessages: [..] and / or errors: {field: message}.
        let mut details: Vec<String> = body["errorMessages"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(str::to_string)).collect();
        if let Some(fields) = body["errors"].as_object() {
            details.extend(fields.iter().map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or("invalid"))));
        }
        let detail = if details.is_empty() { String::new() } else { format!(": {}", details.join("; ")) };
        let hint = match status.as_u16() {
            401 => " (check the username and token)",
            403 => " (the account lacks permission)",
            404 => " (not found, or the account cannot see it)",
            _ => "",
        };
        Err(format!("Jira could not {what}: {status}{hint}{detail}"))
    }

    async fn call(&self, what: &str, method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let mut req = self.request(method, path);
        if let Some(b) = body {
            req = req.json(&b);
        }
        Self::finish(what, req).await
    }

    /// Verify the credentials alone (the organization's connection has no Jira project of its own).
    pub async fn check_account(&self) -> Result<String, String> {
        let me = self.call("identify the account", Method::GET, "/myself", None).await?;
        Ok(format!("Connected as {}.", me["displayName"].as_str().or(me["name"].as_str()).unwrap_or("the configured account")))
    }

    /// Verify the credentials and that the project exists. Returns a short description.
    pub async fn check(&self) -> Result<String, String> {
        let me = self.call("identify the account", Method::GET, "/myself", None).await?;
        let project = self.call("open the project", Method::GET, &format!("/project/{}", self.cfg.project_key), None).await?;
        let types: Vec<&str> = project["issueTypes"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
        if !types.is_empty() && !types.iter().any(|t| t.eq_ignore_ascii_case(&self.cfg.issue_type)) {
            return Err(format!("The project has no issue type “{}”. It offers: {}", self.cfg.issue_type, types.join(", ")));
        }
        Ok(format!(
            "Connected as {} to project “{}”.",
            me["displayName"].as_str().or(me["name"].as_str()).unwrap_or("the configured account"),
            project["name"].as_str().unwrap_or(&self.cfg.project_key)
        ))
    }

    async fn create_issue(&self, summary: &str, description: &str) -> Result<String, String> {
        let body = json!({ "fields": {
            "project": { "key": self.cfg.project_key },
            "issuetype": { "name": self.cfg.issue_type },
            "summary": summary,
            "description": description,
        }});
        let created = self.call("create the issue", Method::POST, "/issue", Some(body)).await?;
        created["key"].as_str().map(str::to_string).ok_or_else(|| "Jira did not return the new issue's key".to_string())
    }

    async fn update_issue(&self, key: &str, summary: &str, description: &str) -> Result<(), String> {
        let body = json!({ "fields": { "summary": summary, "description": description } });
        self.call("update the issue", Method::PUT, &format!("/issue/{key}"), Some(body)).await.map(|_| ())
    }

    async fn attach(&self, key: &str, filename: &str, bytes: Vec<u8>) -> Result<(), String> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new().part("file", part);
        let req = self
            .request(Method::POST, &format!("/issue/{key}/attachments"))
            // Required by Jira for attachment uploads (it is their CSRF check for form posts).
            .header("X-Atlassian-Token", "no-check")
            .timeout(Duration::from_secs(600))
            .multipart(form);
        Self::finish("attach the file", req).await.map(|_| ())
    }

    pub async fn status(&self, key: &str) -> Result<String, String> {
        let issue = self.call("read the issue", Method::GET, &format!("/issue/{key}?fields=status"), None).await?;
        Ok(issue["fields"]["status"]["name"].as_str().unwrap_or("Unknown").to_string())
    }

    pub async fn transitions(&self, key: &str) -> Result<Vec<Transition>, String> {
        let data = self.call("list the issue's transitions", Method::GET, &format!("/issue/{key}/transitions"), None).await?;
        Ok(data["transitions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                Some(Transition {
                    id: t["id"].as_str()?.to_string(),
                    name: t["name"].as_str()?.to_string(),
                    to: t["to"]["name"].as_str().unwrap_or("").to_string(),
                })
            })
            .collect())
    }

    pub async fn transition(&self, key: &str, transition_id: &str) -> Result<(), String> {
        let body = json!({ "transition": { "id": transition_id } });
        self.call("move the issue", Method::POST, &format!("/issue/{key}/transitions"), Some(body)).await.map(|_| ())
    }
}

// ------------------------------------------------------------------------------------------
// Markdown -> Jira wiki markup
// ------------------------------------------------------------------------------------------

/// Ids of the uploads a piece of Markdown refers to (`/uploads/<id>`), in order of appearance.
fn upload_ids(markdown: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for part in markdown.split("/uploads/").skip(1) {
        let id: String = part.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.').collect();
        let id = id.trim_end_matches('.').to_string();
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

fn upload_id(url: &str) -> Option<&str> {
    url.strip_prefix("/uploads/").map(|rest| rest.split(['?', '#']).next().unwrap_or(rest))
}

/// How to refer to a link or image target from inside Jira: an attachment on the issue if the
/// file was uploaded there, otherwise an absolute URL.
enum Target {
    Attachment(String),
    Url(String),
}

fn resolve(url: &str, files: &HashMap<String, String>, public_url: &str) -> Target {
    if let Some(name) = upload_id(url).and_then(|id| files.get(id)) {
        return Target::Attachment(name.clone());
    }
    if url.starts_with('/') {
        // Site-relative links only mean something inside Autokan.
        return Target::Url(format!("{public_url}{url}"));
    }
    Target::Url(url.to_string())
}

/// Convert Markdown (as written in cards and comments) to Jira wiki markup. `files` maps upload
/// ids to the names of the attachments already on the issue.
pub fn to_wiki(markdown: &str, files: &HashMap<String, String>, public_url: &str) -> String {
    let mut out = String::new();
    let mut lists: Vec<char> = Vec::new();
    let mut in_code = false;
    let mut skip_text = 0usize; // inside an image or attachment link: its text is not wanted
    let mut link_tails: Vec<String> = Vec::new();
    let mut in_head = false;
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => out.push_str(&format!("h{}. ", level as usize)),
                Tag::BlockQuote(_) => out.push_str("{quote}\n"),
                Tag::CodeBlock(kind) => {
                    in_code = true;
                    match kind {
                        CodeBlockKind::Fenced(lang) if !lang.trim().is_empty() => {
                            // Jira only accepts languages it knows; an unknown one breaks the block.
                            let lang = lang.trim().to_lowercase();
                            let known = ["java", "javascript", "js", "python", "sql", "xml", "json", "bash", "sh", "ruby", "go", "c", "cpp", "yaml", "html", "css"];
                            if known.contains(&lang.as_str()) {
                                out.push_str(&format!("{{code:{lang}}}\n"));
                            } else {
                                out.push_str("{code}\n");
                            }
                        }
                        _ => out.push_str("{code}\n"),
                    }
                }
                Tag::List(first) => {
                    if lists.is_empty() && !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    lists.push(if first.is_some() { '#' } else { '*' });
                }
                Tag::Item => {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&lists.iter().collect::<String>());
                    out.push(' ');
                }
                Tag::Emphasis => out.push('_'),
                Tag::Strong => out.push('*'),
                Tag::Strikethrough => out.push('-'),
                Tag::Link { dest_url, .. } => match resolve(&dest_url, files, public_url) {
                    Target::Attachment(name) => {
                        out.push_str(&format!("[^{name}]"));
                        skip_text += 1;
                        link_tails.push(String::new());
                    }
                    Target::Url(url) => {
                        out.push('[');
                        link_tails.push(format!("|{url}]"));
                    }
                },
                Tag::Image { dest_url, .. } => {
                    skip_text += 1;
                    match resolve(&dest_url, files, public_url) {
                        Target::Attachment(name) => {
                            let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
                            if IMAGE_EXTS.contains(&ext.as_str()) {
                                out.push_str(&format!("!{name}|thumbnail!"));
                            } else {
                                // Videos and other files show as attachment links.
                                out.push_str(&format!("[^{name}]"));
                            }
                        }
                        Target::Url(url) => {
                            let ext = url.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
                            if VIDEO_EXTS.contains(&ext.as_str()) || url.contains("/uploads/") {
                                out.push_str(&format!("[{url}]"));
                            } else {
                                out.push_str(&format!("!{url}!"));
                            }
                        }
                    }
                }
                Tag::TableHead => in_head = true,
                Tag::TableRow => {}
                Tag::TableCell => out.push_str(if in_head { "||" } else { "|" }),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if lists.is_empty() {
                        out.push_str("\n\n");
                    }
                }
                TagEnd::Heading(_) => out.push_str("\n\n"),
                TagEnd::BlockQuote(_) => {
                    while out.ends_with('\n') {
                        out.pop();
                    }
                    out.push_str("\n{quote}\n\n");
                }
                TagEnd::CodeBlock => {
                    in_code = false;
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("{code}\n\n");
                }
                TagEnd::List(_) => {
                    lists.pop();
                    if lists.is_empty() {
                        out.push_str("\n\n");
                    }
                }
                TagEnd::Item => {}
                TagEnd::Emphasis => out.push('_'),
                TagEnd::Strong => out.push('*'),
                TagEnd::Strikethrough => out.push('-'),
                TagEnd::Link => {
                    if let Some(tail) = link_tails.pop() {
                        if tail.is_empty() {
                            skip_text = skip_text.saturating_sub(1);
                        }
                        out.push_str(&tail);
                    }
                }
                TagEnd::Image => skip_text = skip_text.saturating_sub(1),
                TagEnd::TableHead => {
                    in_head = false;
                    out.push_str("||\n");
                }
                TagEnd::TableRow => out.push_str("|\n"),
                TagEnd::Table => out.push('\n'),
                _ => {}
            },
            Event::Text(text) => {
                if skip_text > 0 {
                    continue;
                }
                if in_code {
                    out.push_str(&text);
                } else {
                    // Braces and brackets start macros and links in wiki markup.
                    out.push_str(&text.replace('{', "\\{").replace('[', "\\["));
                }
            }
            Event::Code(code) => {
                if skip_text == 0 {
                    out.push_str(&format!("{{{{{code}}}}}"));
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if skip_text == 0 {
                    out.push_str(&html.replace('{', "\\{"));
                }
            }
            Event::SoftBreak => out.push(if in_code { '\n' } else { ' ' }),
            Event::HardBreak => out.push('\n'),
            Event::Rule => out.push_str("----\n\n"),
            _ => {}
        }
    }
    out.trim_end().to_string()
}

// ------------------------------------------------------------------------------------------
// Keeping a linked issue up to date
// ------------------------------------------------------------------------------------------

struct CardInfo {
    board_id: i64,
    project_id: i64,
    title: String,
    description: String,
}

async fn card_info(db: &Db, card_id: i64) -> Result<Option<CardInfo>, String> {
    let row: Option<(i64, i64, String, String)> = query_as(
        "SELECT c.board_id, b.project_id, c.title, c.description FROM cards c JOIN boards b ON b.id = c.board_id WHERE c.id = ?",
    )
    .bind(card_id)
    .fetch_optional(db)
    .await
    .map_err(|e| format!("database error: {e:#}"))?;
    Ok(row.map(|(board_id, project_id, title, description)| CardInfo { board_id, project_id, title, description }))
}

pub fn card_url(state: &AppState, board_id: i64, card_id: i64) -> String {
    format!("{}/boards/{board_id}?card={card_id}", state.cfg.public_url)
}

/// The issue description: a link back to the card, then (a couple of lines down) the card's own description.
fn issue_description(state: &AppState, card_id: i64, card: &CardInfo, files: &HashMap<String, String>) -> String {
    let title = card.title.replace(['|', ']', '['], " ");
    let mut out = format!("Autokan card: [{title}|{}]", card_url(state, card.board_id, card_id));
    let body = to_wiki(&card.description, files, &state.cfg.public_url);
    if !body.is_empty() {
        out.push_str("\n\n\n");
        out.push_str(&body);
    }
    out
}

/// Jira attachment names must be unique per issue for `!name!` references to be unambiguous.
fn attachment_name(upload_id: &str, original: &str) -> String {
    let ext = upload_id.rsplit_once('.').map(|(_, e)| e).unwrap_or("bin");
    let stem = original.rsplit_once('.').map(|(s, _)| s).unwrap_or(original);
    let stem: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(60).collect();
    let short: String = upload_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    format!("{}-{short}.{ext}", if stem.is_empty() { "file" } else { &stem })
}

/// Make sure every upload referenced by `texts` is attached to the issue. Returns upload id ->
/// attachment name for all files on the issue. A file that cannot be attached is left out, and
/// its reference then falls back to a link into Autokan.
async fn ensure_files(state: &AppState, client: &Client, card_id: i64, board_id: i64, key: &str, texts: &[&str], problems: &mut Vec<String>) -> Result<HashMap<String, String>, String> {
    let db = &state.db;
    let dbe = |e: anyhow::Error| format!("database error: {e:#}");
    let mut files: HashMap<String, String> = query_as::<(String, String)>("SELECT upload_id, filename FROM jira_files WHERE card_id = ?")
        .bind(card_id)
        .fetch_all(db)
        .await
        .map_err(dbe)?
        .into_iter()
        .collect();
    for text in texts {
        for id in upload_ids(text) {
            if files.contains_key(&id) {
                continue;
            }
            // Only files that belong to this card's board are ever sent out.
            let found: Option<(String,)> = query_as("SELECT filename FROM uploads WHERE id = ? AND board_id = ?")
                .bind(&id)
                .bind(board_id)
                .fetch_optional(db)
                .await
                .map_err(dbe)?;
            let Some((original,)) = found else { continue };
            let name = attachment_name(&id, &original);
            let bytes = match tokio::fs::read(state.uploads_dir().join(&id)).await {
                Ok(b) => b,
                Err(e) => {
                    problems.push(format!("{original} could not be read ({e})"));
                    continue;
                }
            };
            match client.attach(key, &name, bytes).await {
                Ok(()) => {
                    query("INSERT INTO jira_files (card_id, upload_id, filename) VALUES (?, ?, ?)")
                        .bind(card_id)
                        .bind(&id)
                        .bind(&name)
                        .execute(db)
                        .await
                        .map_err(dbe)?;
                    files.insert(id, name);
                }
                Err(e) => problems.push(format!("{original}: {e}")),
            }
        }
    }
    Ok(files)
}

/// Create the Jira issue for a card and send everything the card has so far.
pub async fn create(state: &AppState, card_id: i64) -> Result<String, String> {
    let _one_at_a_time = state.jira_lock.lock().await;
    let db = &state.db;
    let dbe = |e: anyhow::Error| format!("database error: {e:#}");
    let card = card_info(db, card_id).await?.ok_or("The card no longer exists")?;
    if link(db, card_id).await.map_err(dbe)?.is_some() {
        return Err("This card already has a Jira issue".into());
    }
    let cfg = settings(db, card.project_id).await.map_err(dbe)?.filter(Settings::usable).ok_or("Jira is not set up for this project")?;
    let client = Client::new(state, &cfg)?;
    // Created with the link only; the description follows once any files it shows are attached.
    let first = issue_description(state, card_id, &CardInfo { description: String::new(), title: card.title.clone(), ..card }, &HashMap::new());
    let key = client.create_issue(&card.title, &first).await?;
    query("INSERT INTO jira_links (card_id, issue_key, pushed_summary, pushed_description) VALUES (?, ?, ?, ?)")
        .bind(card_id)
        .bind(&key)
        .bind(&card.title)
        .bind(&first)
        .execute(db)
        .await
        .map_err(dbe)?;
    drop(_one_at_a_time);
    push(state, card_id).await;
    Ok(key)
}

/// Bring a linked issue up to date with its card: files, summary, description and comments.
/// Does nothing for a card without an issue. Failures are recorded on the link, not raised.
pub async fn push(state: &AppState, card_id: i64) {
    let _one_at_a_time = state.jira_lock.lock().await;
    let Ok(Some(existing)) = link(&state.db, card_id).await else { return };
    let outcome = push_inner(state, &existing).await;
    let error = match &outcome {
        Ok(problems) if problems.is_empty() => String::new(),
        Ok(problems) => format!("Some files could not be attached: {}", problems.join("; ")),
        Err(e) => e.clone(),
    };
    if !error.is_empty() {
        tracing::warn!("Jira update for card {card_id} ({}) failed: {error}", existing.issue_key);
    }
    let _ = query("UPDATE jira_links SET last_error = ?, synced_at = ? WHERE card_id = ?")
        .bind(error.chars().take(2000).collect::<String>())
        .bind(now())
        .bind(card_id)
        .execute(&state.db)
        .await;
}

async fn push_inner(state: &AppState, existing: &Link) -> Result<Vec<String>, String> {
    let db = &state.db;
    let dbe = |e: anyhow::Error| format!("database error: {e:#}");
    let card_id = existing.card_id;
    let key = existing.issue_key.as_str();
    let Some(card) = card_info(db, card_id).await? else { return Ok(Vec::new()) };
    let cfg = settings(db, card.project_id).await.map_err(dbe)?.filter(Settings::usable).ok_or("Jira is no longer set up for this project")?;
    let client = Client::new(state, &cfg)?;
    let mut problems = Vec::new();

    let comments: Vec<(i64, String, Option<String>)> = query_as(
        "SELECT m.id, m.body, u.display_name FROM comments m LEFT JOIN users u ON u.id = m.user_id WHERE m.card_id = ? ORDER BY m.id",
    )
    .bind(card_id)
    .fetch_all(db)
    .await
    .map_err(dbe)?;
    let mut texts: Vec<&str> = vec![card.description.as_str()];
    texts.extend(comments.iter().map(|c| c.1.as_str()));
    let files = ensure_files(state, &client, card_id, card.board_id, key, &texts, &mut problems).await?;

    // Summary and description.
    let description = issue_description(state, card_id, &card, &files);
    if card.title != existing.pushed_summary || description != existing.pushed_description {
        client.update_issue(key, &card.title, &description).await?;
        query("UPDATE jira_links SET pushed_summary = ?, pushed_description = ? WHERE card_id = ?")
            .bind(&card.title)
            .bind(&description)
            .bind(card_id)
            .execute(db)
            .await
            .map_err(dbe)?;
    }

    // Comments: new ones are added, edited ones updated, deleted ones removed.
    let sent: Vec<(i64, String, String)> = query_as("SELECT comment_id, jira_comment_id, pushed_body FROM jira_comments WHERE card_id = ?")
        .bind(card_id)
        .fetch_all(db)
        .await
        .map_err(dbe)?;
    for (comment_id, body, author) in &comments {
        // The Jira comment is posted by the integration's account, so say who actually wrote it.
        let text = format!("*{}* commented in Autokan:\n\n{}", author.as_deref().unwrap_or("Someone").replace('*', ""), to_wiki(body, &files, &state.cfg.public_url));
        match sent.iter().find(|s| s.0 == *comment_id) {
            None => {
                let created = client.call("add the comment", Method::POST, &format!("/issue/{key}/comment"), Some(json!({ "body": text }))).await?;
                let jira_id = created["id"].as_str().ok_or("Jira did not return the new comment's id")?;
                query("INSERT INTO jira_comments (comment_id, card_id, jira_comment_id, pushed_body) VALUES (?, ?, ?, ?)")
                    .bind(comment_id)
                    .bind(card_id)
                    .bind(jira_id)
                    .bind(&text)
                    .execute(db)
                    .await
                    .map_err(dbe)?;
            }
            Some((_, jira_id, pushed)) if *pushed != text => {
                client.call("update the comment", Method::PUT, &format!("/issue/{key}/comment/{jira_id}"), Some(json!({ "body": text }))).await?;
                query("UPDATE jira_comments SET pushed_body = ? WHERE comment_id = ?").bind(&text).bind(comment_id).execute(db).await.map_err(dbe)?;
            }
            Some(_) => {}
        }
    }
    for (comment_id, jira_id, _) in sent.iter().filter(|s| !comments.iter().any(|c| c.0 == s.0)) {
        // Already gone on the Jira side is fine.
        if let Err(e) = client.call("remove the comment", Method::DELETE, &format!("/issue/{key}/comment/{jira_id}"), None).await {
            if !e.contains("404") {
                return Err(e);
            }
        }
        query("DELETE FROM jira_comments WHERE comment_id = ?").bind(comment_id).execute(db).await.map_err(dbe)?;
    }
    Ok(problems)
}

/// Update the issue of one card in the background (after an edit or a comment).
pub fn push_soon(state: &AppState, card_id: i64) {
    let state = state.clone();
    tokio::spawn(async move { push(&state, card_id).await });
}

/// Update every linked issue on a board in the background (after the agent or a sync changed cards).
pub fn push_board_soon(state: &AppState, board_id: i64) {
    let state = state.clone();
    tokio::spawn(async move {
        let linked: Vec<(i64,)> = query_as("SELECT l.card_id FROM jira_links l JOIN cards c ON c.id = l.card_id WHERE c.board_id = ?")
            .bind(board_id)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
        for (card_id,) in linked {
            push(&state, card_id).await;
        }
    });
}
