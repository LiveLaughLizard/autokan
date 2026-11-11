//! GitHub Projects (the current "Projects v2", GraphQL API).
//!
//! A project's board columns are the options of its single-select "Status" field; a card's column
//! is that field's value on the project item. Items can be draft issues, issues or pull requests;
//! cards created from Autokan become draft issues.
use super::{Link, Remote, RemoteBoard, RemoteCard, RemoteColumn};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

const NONE: &str = "none";
const STATUS_FIELD: &str = "Status";
const MAX_PAGES: usize = 30;

#[derive(Clone)]
struct StatusOption {
    id: String,
    name: String,
    color: String,
    description: String,
}

pub struct GitHub {
    http: reqwest::Client,
    endpoint: String,
    token: String,
    project_id: String,
    field_id: String,
    options: Vec<StatusOption>,
    /// item id -> (content type, content node id), from the last fetch.
    content: HashMap<String, (String, String)>,
    remap: HashMap<String, String>,
}

/// `https://api.github.com/graphql`, or `<base>/api/graphql` for GitHub Enterprise Server.
fn endpoint(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() || base == "https://github.com" || base == "https://api.github.com" {
        "https://api.github.com/graphql".to_string()
    } else if base.ends_with("/graphql") {
        base.to_string()
    } else {
        format!("{base}/api/graphql")
    }
}

/// Run a GraphQL operation. With `partial`, a response that carries both data and errors is
/// returned (used when probing whether a login is an organization or a user).
async fn graphql(http: &reqwest::Client, endpoint: &str, token: &str, query: &str, variables: Value, partial: bool) -> Result<Value, String> {
    let resp = http
        .post(endpoint)
        .bearer_auth(token)
        .header("User-Agent", "autokan")
        .timeout(Duration::from_secs(30))
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await
        .map_err(|e| format!("Could not reach GitHub: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("GitHub returned an unreadable response ({status}): {e}"))?;
    if !status.is_success() {
        return Err(format!("GitHub returned {status}: {}", body["message"].as_str().unwrap_or("request failed")));
    }
    if let Some(errors) = body["errors"].as_array().filter(|e| !e.is_empty()) {
        if !(partial && body["data"].is_object()) {
            let messages: Vec<&str> = errors.iter().filter_map(|e| e["message"].as_str()).collect();
            return Err(format!("GitHub rejected the request: {}", messages.join("; ")));
        }
    }
    Ok(body["data"].clone())
}

/// Find the project named in the link form: an owner (organization or user) and a project number.
pub async fn resolve(http: &reqwest::Client, base_url: &str, token: &str, form: &HashMap<String, String>) -> Result<String, String> {
    let owner = form.get("github_owner").map(|s| s.trim().trim_start_matches('@')).unwrap_or("");
    let number: i64 = form
        .get("github_number")
        .and_then(|s| s.trim().trim_start_matches('#').parse().ok())
        .ok_or("Enter the project number (the number in the project's URL)")?;
    if owner.is_empty() {
        return Err("Enter the organization or user that owns the project".into());
    }
    let query = "query($login: String!, $number: Int!) {
        organization(login: $login) { projectV2(number: $number) { id title } }
        user(login: $login) { projectV2(number: $number) { id title } }
    }";
    let data = graphql(http, &endpoint(base_url), token, query, json!({ "login": owner, "number": number }), true).await?;
    let project = [&data["organization"]["projectV2"], &data["user"]["projectV2"]]
        .into_iter()
        .find(|p| p["id"].is_string())
        .ok_or_else(|| format!("No project #{number} was found for “{owner}” (or the token cannot see it)"))?;
    Ok(json!({ "owner": owner, "number": number, "project_id": project["id"], "title": project["title"] }).to_string())
}

impl GitHub {
    pub fn new(http: reqwest::Client, link: &Link) -> Result<Self, String> {
        let target: Value = serde_json::from_str(&link.target).map_err(|_| "The link is missing its GitHub target".to_string())?;
        Ok(GitHub {
            http,
            endpoint: endpoint(&link.base_url),
            token: link.token.clone(),
            project_id: target["project_id"].as_str().ok_or("The link is missing its GitHub project")?.to_string(),
            field_id: String::new(),
            options: Vec::new(),
            content: HashMap::new(),
            remap: HashMap::new(),
        })
    }

    async fn run(&self, query: &str, variables: Value) -> Result<Value, String> {
        graphql(&self.http, &self.endpoint, &self.token, query, variables, false).await
    }

    /// Replace the Status field's option list. GitHub may hand back new ids for options, so the
    /// result is matched up by name and any id that changed is recorded for the caller.
    async fn set_options(&mut self, wanted: Vec<(Option<String>, StatusOption)>) -> Result<(), String> {
        let input: Vec<Value> = wanted
            .iter()
            .map(|(_, o)| json!({ "name": o.name, "color": o.color, "description": o.description }))
            .collect();
        let query = "mutation($field: ID!, $options: [ProjectV2SingleSelectFieldOptionInput!]) {
            updateProjectV2Field(input: { fieldId: $field, singleSelectOptions: $options }) {
                projectV2Field { ... on ProjectV2SingleSelectField { options { id name color description } } }
            }
        }";
        let data = self.run(query, json!({ "field": self.field_id, "options": input })).await?;
        let options = parse_options(&data["updateProjectV2Field"]["projectV2Field"]["options"]);
        for (old_id, option) in &wanted {
            let new_id = options.iter().find(|o| o.name == option.name).map(|o| o.id.clone());
            if let (Some(old), Some(new)) = (old_id, new_id) {
                if *old != new {
                    // Keep earlier entries pointing at the newest id (a -> b, then b -> c, gives a -> c).
                    for v in self.remap.values_mut() {
                        if v == old {
                            *v = new.clone();
                        }
                    }
                    self.remap.insert(old.clone(), new);
                }
            }
        }
        self.options = options;
        Ok(())
    }

    /// The id an option has now, given one handed out earlier in this cycle.
    fn live(&self, id: &str) -> String {
        self.remap.get(id).cloned().unwrap_or_else(|| id.to_string())
    }

    fn current(&self) -> Vec<(Option<String>, StatusOption)> {
        self.options.iter().map(|o| (Some(o.id.clone()), o.clone())).collect()
    }
}

fn parse_options(v: &Value) -> Vec<StatusOption> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            Some(StatusOption {
                id: o["id"].as_str()?.to_string(),
                name: o["name"].as_str()?.to_string(),
                color: o["color"].as_str().unwrap_or("GRAY").to_string(),
                description: o["description"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

#[async_trait]
impl Remote for GitHub {
    async fn fetch(&mut self, _known: &[String]) -> Result<RemoteBoard, String> {
        let query = "query($project: ID!, $field: String!, $cursor: String) {
            node(id: $project) {
                ... on ProjectV2 {
                    field(name: $field) { ... on ProjectV2SingleSelectField { id options { id name color description } } }
                    items(first: 100, after: $cursor) {
                        pageInfo { hasNextPage endCursor }
                        nodes {
                            id
                            isArchived
                            fieldValueByName(name: $field) { ... on ProjectV2ItemFieldSingleSelectValue { optionId } }
                            content {
                                __typename
                                ... on DraftIssue { id title body }
                                ... on Issue { id title body }
                                ... on PullRequest { id title body }
                            }
                        }
                    }
                }
            }
        }";
        let mut cards = Vec::new();
        let mut cursor = Value::Null;
        self.content.clear();
        for page in 0.. {
            if page >= MAX_PAGES {
                return Err(format!("The project has more than {} items, which is more than this sync handles", MAX_PAGES * 100));
            }
            let data = self.run(query, json!({ "project": self.project_id, "field": STATUS_FIELD, "cursor": cursor })).await?;
            let project = &data["node"];
            if project.is_null() {
                return Err("The GitHub project no longer exists, or the token cannot see it".into());
            }
            if page == 0 {
                self.field_id = project["field"]["id"]
                    .as_str()
                    .ok_or("The project has no single-select “Status” field to use as columns")?
                    .to_string();
                self.options = parse_options(&project["field"]["options"]);
            }
            for item in project["items"]["nodes"].as_array().into_iter().flatten() {
                // Archived items are off the board; items whose content the token cannot read have no title.
                let (Some(id), Some(kind), Some(content_id)) = (item["id"].as_str(), item["content"]["__typename"].as_str(), item["content"]["id"].as_str()) else {
                    continue;
                };
                if item["isArchived"] == true {
                    continue;
                }
                self.content.insert(id.to_string(), (kind.to_string(), content_id.to_string()));
                let option = item["fieldValueByName"]["optionId"].as_str();
                cards.push(RemoteCard {
                    id: id.to_string(),
                    title: item["content"]["title"].as_str().unwrap_or("").to_string(),
                    body: item["content"]["body"].as_str().unwrap_or("").to_string(),
                    column: option.filter(|o| self.options.iter().any(|x| x.id == *o)).unwrap_or(NONE).to_string(),
                });
            }
            let info = &project["items"]["pageInfo"];
            if info["hasNextPage"] != true {
                break;
            }
            cursor = info["endCursor"].clone();
        }
        let mut columns = vec![RemoteColumn { id: NONE.into(), name: "No Status".into(), fixed: true }];
        columns.extend(self.options.iter().map(|o| RemoteColumn { id: o.id.clone(), name: o.name.clone(), fixed: false }));
        Ok(RemoteBoard { columns, cards, fallback: NONE.into() })
    }

    async fn create_column(&mut self, name: &str) -> Result<RemoteColumn, String> {
        let mut wanted = self.current();
        wanted.push((None, StatusOption { id: String::new(), name: name.to_string(), color: "GRAY".into(), description: String::new() }));
        self.set_options(wanted).await?;
        let created = self.options.iter().find(|o| o.name == name).ok_or("GitHub did not create the status option")?;
        Ok(RemoteColumn { id: created.id.clone(), name: created.name.clone(), fixed: false })
    }

    async fn rename_column(&mut self, id: &str, name: &str) -> Result<(), String> {
        let id = self.live(id);
        let mut wanted = self.current();
        let option = wanted.iter_mut().find(|(old, _)| old.as_deref() == Some(id.as_str())).ok_or("That status option no longer exists")?;
        option.1.name = name.to_string();
        self.set_options(wanted).await
    }

    async fn delete_column(&mut self, id: &str) -> Result<(), String> {
        let id = self.live(id);
        let wanted: Vec<_> = self.current().into_iter().filter(|(old, _)| old.as_deref() != Some(id.as_str())).collect();
        self.set_options(wanted).await
    }

    async fn order_columns(&mut self, ids: &[String]) -> Result<(), String> {
        let mut current = self.current();
        let mut wanted = Vec::new();
        for id in ids {
            // An id handed out before an earlier edit in this cycle may have been re-issued since.
            let id = self.live(id);
            if let Some(i) = current.iter().position(|(old, _)| old.as_deref() == Some(id.as_str())) {
                wanted.push(current.remove(i));
            }
        }
        wanted.extend(current);
        self.set_options(wanted).await
    }

    async fn create_card(&mut self, title: &str, body: &str, column: &str) -> Result<String, String> {
        let query = "mutation($project: ID!, $title: String!, $body: String) {
            addProjectV2DraftIssue(input: { projectId: $project, title: $title, body: $body }) {
                projectItem { id content { ... on DraftIssue { id } } }
            }
        }";
        let data = self.run(query, json!({ "project": self.project_id, "title": title, "body": body })).await?;
        let item = &data["addProjectV2DraftIssue"]["projectItem"];
        let id = item["id"].as_str().ok_or("GitHub did not return the new item's id")?.to_string();
        if let Some(content_id) = item["content"]["id"].as_str() {
            self.content.insert(id.clone(), ("DraftIssue".to_string(), content_id.to_string()));
        }
        if column != NONE {
            self.move_card(&id, column).await?;
        }
        Ok(id)
    }

    async fn update_card(&mut self, id: &str, title: &str, body: &str) -> Result<(), String> {
        let (kind, content_id) = self.content.get(id).cloned().ok_or("That project item is not known")?;
        let query = match kind.as_str() {
            "DraftIssue" => "mutation($id: ID!, $title: String!, $body: String) { updateProjectV2DraftIssue(input: { draftIssueId: $id, title: $title, body: $body }) { draftIssue { id } } }",
            "Issue" => "mutation($id: ID!, $title: String!, $body: String) { updateIssue(input: { id: $id, title: $title, body: $body }) { issue { id } } }",
            "PullRequest" => "mutation($id: ID!, $title: String!, $body: String) { updatePullRequest(input: { pullRequestId: $id, title: $title, body: $body }) { pullRequest { id } } }",
            other => return Err(format!("GitHub items of type {other} cannot be edited")),
        };
        self.run(query, json!({ "id": content_id, "title": title, "body": body })).await?;
        Ok(())
    }

    async fn move_card(&mut self, id: &str, column: &str) -> Result<(), String> {
        if column == NONE {
            let query = "mutation($project: ID!, $item: ID!, $field: ID!) {
                clearProjectV2ItemFieldValue(input: { projectId: $project, itemId: $item, fieldId: $field }) { projectV2Item { id } }
            }";
            self.run(query, json!({ "project": self.project_id, "item": id, "field": self.field_id })).await?;
        } else {
            let query = "mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) {
                updateProjectV2ItemFieldValue(input: { projectId: $project, itemId: $item, fieldId: $field, value: { singleSelectOptionId: $option } }) { projectV2Item { id } }
            }";
            self.run(query, json!({ "project": self.project_id, "item": id, "field": self.field_id, "option": column })).await?;
        }
        Ok(())
    }

    async fn place_card(&mut self, id: &str, previous: Option<&str>, _next: Option<&str>) -> Result<(), String> {
        // `afterId: null` moves the item to the top.
        let query = "mutation($project: ID!, $item: ID!, $after: ID) {
            updateProjectV2ItemPosition(input: { projectId: $project, itemId: $item, afterId: $after }) { clientMutationId }
        }";
        self.run(query, json!({ "project": self.project_id, "item": id, "after": previous })).await?;
        Ok(())
    }

    async fn delete_card(&mut self, id: &str) -> Result<bool, String> {
        // Removes the item from the project. An issue or pull request behind it is left untouched.
        let query = "mutation($project: ID!, $item: ID!) { deleteProjectV2Item(input: { projectId: $project, itemId: $item }) { deletedItemId } }";
        self.run(query, json!({ "project": self.project_id, "item": id })).await?;
        Ok(true)
    }

    fn take_column_remap(&mut self) -> HashMap<String, String> {
        std::mem::take(&mut self.remap)
    }
}
