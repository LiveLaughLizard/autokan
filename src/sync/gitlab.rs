//! GitLab issue boards (REST API v4).
//!
//! A board's lists are backed by labels: an issue sits in the first list whose label it carries,
//! in "Open" if it carries none, and in "Closed" once closed. So moving a card is a label swap
//! (plus close / reopen), creating a column is creating a label and a list for it, and renaming a
//! column renames the label.
use super::{Link, Remote, RemoteBoard, RemoteCard, RemoteColumn};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

const OPEN: &str = "open";
const CLOSED: &str = "closed";
const MAX_PAGES: usize = 30;

struct List {
    id: i64,
    label_id: i64,
    label: String,
}

pub struct GitLab {
    http: reqwest::Client,
    api: String,
    token: String,
    /// URL-encoded project path or numeric id.
    project: String,
    board_id: i64,
    lists: Vec<List>,
    /// iid -> (global id, closed), from the last fetch.
    issues: HashMap<String, (i64, bool)>,
}

fn api_base(base_url: &str) -> String {
    let base = if base_url.trim().is_empty() { "https://gitlab.com" } else { base_url.trim() };
    format!("{}/api/v4", base.trim_end_matches('/'))
}

fn encode(path: &str) -> String {
    url::form_urlencoded::byte_serialize(path.trim().trim_matches('/').as_bytes()).collect()
}

async fn call(http: &reqwest::Client, api: &str, token: &str, method: Method, path: &str, body: Option<Value>) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Value), String> {
    let mut req = http
        .request(method.clone(), format!("{api}{path}"))
        .header("PRIVATE-TOKEN", token)
        .timeout(Duration::from_secs(30));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(|e| format!("Could not reach GitLab: {e}"))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let text = resp.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, headers, value))
}

fn failure(method: &Method, path: &str, status: reqwest::StatusCode, body: &Value) -> String {
    let detail = match (&body["message"], &body["error"]) {
        (Value::Null, Value::Null) => String::new(),
        (Value::Null, e) => format!(": {e}"),
        (m, _) => format!(": {m}"),
    };
    format!("GitLab {method} {} returned {status}{detail}", path.split('?').next().unwrap_or(path))
}

/// Find the project and board named in the link form. Creates a board if the project has none.
pub async fn resolve(http: &reqwest::Client, base_url: &str, token: &str, form: &HashMap<String, String>) -> Result<String, String> {
    let project = form.get("gitlab_project").map(|s| s.trim().trim_matches('/')).unwrap_or("");
    if project.is_empty() {
        return Err("Enter the GitLab project path, for example my-group/my-project".into());
    }
    let wanted = form.get("gitlab_board").map(|s| s.trim()).unwrap_or("");
    let api = api_base(base_url);
    let path = format!("/projects/{}/boards?per_page=100", encode(project));
    let (status, _, boards) = call(http, &api, token, Method::GET, &path, None).await?;
    if !status.is_success() {
        return Err(failure(&Method::GET, &path, status, &boards));
    }
    let boards = boards.as_array().cloned().unwrap_or_default();
    let chosen = if wanted.is_empty() {
        boards.first().cloned()
    } else {
        Some(
            boards
                .iter()
                .find(|b| b["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(wanted)))
                .cloned()
                .ok_or_else(|| format!("The project has no board named “{wanted}”"))?,
        )
    };
    let board = match chosen {
        Some(b) => b,
        None => {
            let path = format!("/projects/{}/boards", encode(project));
            let (status, _, created) = call(http, &api, token, Method::POST, &path, Some(json!({ "name": "Autokan" }))).await?;
            if !status.is_success() {
                return Err(failure(&Method::POST, &path, status, &created));
            }
            created
        }
    };
    let board_id = board["id"].as_i64().ok_or("GitLab did not return a board id")?;
    Ok(json!({ "project": project, "board_id": board_id, "board_name": board["name"].as_str().unwrap_or("Board") }).to_string())
}

impl GitLab {
    pub fn new(http: reqwest::Client, link: &Link) -> Result<Self, String> {
        let target: Value = serde_json::from_str(&link.target).map_err(|_| "The link is missing its GitLab target".to_string())?;
        Ok(GitLab {
            http,
            api: api_base(&link.base_url),
            token: link.token.clone(),
            project: encode(target["project"].as_str().ok_or("The link is missing its GitLab project")?),
            board_id: target["board_id"].as_i64().ok_or("The link is missing its GitLab board")?,
            lists: Vec::new(),
            issues: HashMap::new(),
        })
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let (status, _, value) = call(&self.http, &self.api, &self.token, method.clone(), path, body).await?;
        if status.is_success() {
            Ok(value)
        } else {
            Err(failure(&method, path, status, &value))
        }
    }

    fn list(&self, column: &str) -> Option<&List> {
        column.parse::<i64>().ok().and_then(|id| self.lists.iter().find(|l| l.id == id))
    }

    fn global_id(&self, iid: &str) -> Result<i64, String> {
        self.issues.get(iid).map(|i| i.0).ok_or_else(|| format!("GitLab issue #{iid} is not known"))
    }

    fn card(&self, issue: &Value) -> Option<RemoteCard> {
        let iid = issue["iid"].as_i64()?.to_string();
        let closed = issue["state"] == "closed";
        let labels: Vec<&str> = issue["labels"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let column = if closed {
            CLOSED.to_string()
        } else {
            self.lists.iter().find(|l| labels.contains(&l.label.as_str())).map(|l| l.id.to_string()).unwrap_or_else(|| OPEN.to_string())
        };
        Some(RemoteCard {
            id: iid,
            title: issue["title"].as_str().unwrap_or("").to_string(),
            body: issue["description"].as_str().unwrap_or("").to_string(),
            column,
        })
    }

    fn remember(&mut self, issue: &Value) {
        if let (Some(iid), Some(id)) = (issue["iid"].as_i64(), issue["id"].as_i64()) {
            self.issues.insert(iid.to_string(), (id, issue["state"] == "closed"));
        }
    }
}

#[async_trait]
impl Remote for GitLab {
    async fn fetch(&mut self, known: &[String]) -> Result<RemoteBoard, String> {
        let p = self.project.clone();
        let lists = self.send(Method::GET, &format!("/projects/{p}/boards/{}/lists?per_page=100", self.board_id), None).await?;
        let mut lists: Vec<(i64, List)> = lists
            .as_array()
            .into_iter()
            .flatten()
            // Only label lists are columns; assignee / milestone lists (paid tiers) are left alone.
            .filter_map(|l| {
                Some((
                    l["position"].as_i64().unwrap_or(0),
                    List { id: l["id"].as_i64()?, label_id: l["label"]["id"].as_i64()?, label: l["label"]["name"].as_str()?.to_string() },
                ))
            })
            .collect();
        lists.sort_by_key(|l| l.0);
        self.lists = lists.into_iter().map(|l| l.1).collect();
        self.issues.clear();

        // Open issues, in board order.
        let mut cards = Vec::new();
        let mut page = 1;
        loop {
            let path = format!("/projects/{p}/issues?state=opened&order_by=relative_position&sort=asc&per_page=100&page={page}");
            let (status, headers, issues) = call(&self.http, &self.api, &self.token, Method::GET, &path, None).await?;
            if !status.is_success() {
                return Err(failure(&Method::GET, &path, status, &issues));
            }
            for issue in issues.as_array().into_iter().flatten() {
                self.remember(issue);
                cards.extend(self.card(issue));
            }
            let next = headers.get("x-next-page").and_then(|v| v.to_str().ok()).unwrap_or("");
            if next.is_empty() {
                break;
            }
            page += 1;
            if page > MAX_PAGES {
                return Err(format!("The project has more than {} open issues, which is more than this sync handles", MAX_PAGES * 100));
            }
        }
        // Paired issues that are no longer open: closed (they show in the Closed column) or deleted.
        for iid in known.iter().filter(|iid| !self.issues.contains_key(iid.as_str())).cloned().collect::<Vec<_>>() {
            let path = format!("/projects/{p}/issues/{iid}");
            let (status, _, issue) = call(&self.http, &self.api, &self.token, Method::GET, &path, None).await?;
            if status == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            if !status.is_success() {
                return Err(failure(&Method::GET, &path, status, &issue));
            }
            self.remember(&issue);
            cards.extend(self.card(&issue));
        }

        let mut columns = vec![RemoteColumn { id: OPEN.into(), name: "Open".into(), fixed: true }];
        columns.extend(self.lists.iter().map(|l| RemoteColumn { id: l.id.to_string(), name: l.label.clone(), fixed: false }));
        columns.push(RemoteColumn { id: CLOSED.into(), name: "Closed".into(), fixed: true });
        Ok(RemoteBoard { columns, cards, fallback: OPEN.into() })
    }

    async fn create_column(&mut self, name: &str) -> Result<RemoteColumn, String> {
        let p = self.project.clone();
        // Reuse a label of that name if the project already has one.
        let search: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
        let existing = self.send(Method::GET, &format!("/projects/{p}/labels?per_page=100&search={search}"), None).await?;
        let label = match existing.as_array().into_iter().flatten().find(|l| l["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name))) {
            Some(l) => l.clone(),
            None => self.send(Method::POST, &format!("/projects/{p}/labels"), Some(json!({ "name": name, "color": "#6699cc" }))).await?,
        };
        let label_id = label["id"].as_i64().ok_or("GitLab did not return a label id")?;
        let list = self.send(Method::POST, &format!("/projects/{p}/boards/{}/lists", self.board_id), Some(json!({ "label_id": label_id }))).await?;
        let id = list["id"].as_i64().ok_or("GitLab did not return a list id")?;
        let label_name = label["name"].as_str().unwrap_or(name).to_string();
        self.lists.push(List { id, label_id, label: label_name.clone() });
        Ok(RemoteColumn { id: id.to_string(), name: label_name, fixed: false })
    }

    async fn rename_column(&mut self, id: &str, name: &str) -> Result<(), String> {
        let label_id = self.list(id).map(|l| l.label_id).ok_or("That GitLab list no longer exists")?;
        self.send(Method::PUT, &format!("/projects/{}/labels/{label_id}", self.project), Some(json!({ "new_name": name }))).await?;
        if let Some(l) = self.lists.iter_mut().find(|l| l.label_id == label_id) {
            l.label = name.to_string();
        }
        Ok(())
    }

    async fn delete_column(&mut self, id: &str) -> Result<(), String> {
        // Removes the list from the board; the label and the issues carrying it are untouched.
        self.send(Method::DELETE, &format!("/projects/{}/boards/{}/lists/{id}", self.project, self.board_id), None).await?;
        self.lists.retain(|l| l.id.to_string() != id);
        Ok(())
    }

    async fn order_columns(&mut self, ids: &[String]) -> Result<(), String> {
        for (position, id) in ids.iter().enumerate() {
            self.send(Method::PUT, &format!("/projects/{}/boards/{}/lists/{id}", self.project, self.board_id), Some(json!({ "position": position }))).await?;
        }
        Ok(())
    }

    async fn create_card(&mut self, title: &str, body: &str, column: &str) -> Result<String, String> {
        let mut issue = json!({ "title": title, "description": body });
        if let Some(l) = self.list(column) {
            issue["labels"] = json!(l.label);
        }
        let created = self.send(Method::POST, &format!("/projects/{}/issues", self.project), Some(issue)).await?;
        let iid = created["iid"].as_i64().ok_or("GitLab did not return the new issue's number")?.to_string();
        self.remember(&created);
        if column == CLOSED {
            self.move_card(&iid, CLOSED).await?;
        }
        Ok(iid)
    }

    async fn update_card(&mut self, id: &str, title: &str, body: &str) -> Result<(), String> {
        self.send(Method::PUT, &format!("/projects/{}/issues/{id}", self.project), Some(json!({ "title": title, "description": body }))).await?;
        Ok(())
    }

    async fn move_card(&mut self, id: &str, column: &str) -> Result<(), String> {
        let target = self.list(column).map(|l| l.label.clone());
        let others: Vec<&str> = self.lists.iter().map(|l| l.label.as_str()).filter(|l| Some(*l) != target.as_deref()).collect();
        let mut change = json!({});
        if column != CLOSED {
            // Closing keeps the issue's list label, as it does in GitLab's own board.
            change["remove_labels"] = json!(others.join(","));
            if let Some(label) = &target {
                change["add_labels"] = json!(label);
            }
        }
        let closed = self.issues.get(id).is_some_and(|i| i.1);
        if column == CLOSED && !closed {
            change["state_event"] = json!("close");
        } else if column != CLOSED && closed {
            change["state_event"] = json!("reopen");
        }
        let updated = self.send(Method::PUT, &format!("/projects/{}/issues/{id}", self.project), Some(change)).await?;
        self.remember(&updated);
        Ok(())
    }

    async fn place_card(&mut self, id: &str, previous: Option<&str>, next: Option<&str>) -> Result<(), String> {
        // GitLab names these from the other issue's point of view: `move_before_id` is the issue
        // that ends up before this one, `move_after_id` the one that ends up after it.
        let mut body = json!({});
        if let Some(p) = previous {
            body["move_before_id"] = json!(self.global_id(p)?);
        }
        if let Some(n) = next {
            body["move_after_id"] = json!(self.global_id(n)?);
        }
        if body.as_object().is_some_and(|o| o.is_empty()) {
            return Ok(());
        }
        self.send(Method::PUT, &format!("/projects/{}/issues/{id}/reorder", self.project), Some(body)).await?;
        Ok(())
    }

    async fn delete_card(&mut self, id: &str) -> Result<bool, String> {
        let path = format!("/projects/{}/issues/{id}", self.project);
        let (status, _, body) = call(&self.http, &self.api, &self.token, Method::DELETE, &path, None).await?;
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(true);
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            // Deleting issues needs the Owner role; closing is the next best thing.
            self.send(Method::PUT, &path, Some(json!({ "state_event": "close" }))).await?;
            return Ok(false);
        }
        Err(failure(&Method::DELETE, &path, status, &body))
    }
}
