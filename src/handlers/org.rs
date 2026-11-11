//! Home, teams and projects.
use crate::db::{query, query_as};
use super::{redirect, user_options};
use crate::auth::User;
use crate::error::{bad, AppError, Res};
use crate::ops::{unique_name, Named};
use crate::perm::{project_access, require, team_access};
use crate::llm;
use crate::views::{crumbs, icon, kind_tag, page, panel, target_select};
use crate::AppState;
use axum::extract::{Path, State};
use axum::response::Response;
use axum_extra::extract::Form;
use maud::{html, Markup};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct NameForm {
    name: String,
    #[serde(default)]
    description: String,
}

pub async fn home(State(state): State<AppState>, user: User) -> Res<Markup> {
    let teams: Vec<(i64, String)> = if user.has("teams.manage") {
        query_as("SELECT id, name FROM teams ORDER BY name").fetch_all(&state.db).await?
    } else {
        query_as("SELECT t.id, t.name FROM teams t JOIN team_members m ON m.team_id = t.id WHERE m.user_id = ? ORDER BY t.name")
            .bind(user.id)
            .fetch_all(&state.db)
            .await?
    };
    let project_counts: Vec<(i64, i64)> = query_as("SELECT team_id, COUNT(*) FROM projects GROUP BY team_id")
        .fetch_all(&state.db)
        .await?;
    let board_counts: Vec<(i64, i64)> =
        query_as("SELECT p.team_id, COUNT(*) FROM boards b JOIN projects p ON p.id = b.project_id GROUP BY p.team_id")
            .fetch_all(&state.db)
            .await?;
    let count = |rows: &[(i64, i64)], team: i64| rows.iter().find(|r| r.0 == team).map(|r| r.1).unwrap_or(0);
    Ok(page(
        Some(&user),
        "Teams",
        false,
        html! {
            div class="flex flex-wrap items-end gap-3" {
                div class="flex-1" {
                    h1 class="text-3xl font-extrabold tracking-tight" { "Welcome back, " span class="gradient-text" { (user.display_name) } }
                    p class="opacity-70" { "Pick a team to see its projects and boards." }
                }
                @if user.has("teams.create") {
                    form method="post" action="/teams" class="join" {
                        input class="input input-sm join-item" name="name" placeholder="New team name" required;
                        button class="btn btn-primary btn-sm join-item" { "Create team" }
                    }
                }
            }
            @if teams.is_empty() {
                div class="card bg-base-100 border border-base-300" {
                    div class="card-body items-center text-center opacity-70" {
                        @if user.has("teams.create") {
                            p { "No teams yet. Create the first one above, then give someone rights to manage it." }
                        } @else {
                            p { "You are not a member of any team yet. Ask a team admin to add you." }
                        }
                    }
                }
            }
            // Teams only: projects and boards live on each team's page.
            div class="grid sm:grid-cols-2 lg:grid-cols-3 gap-4" {
                @for (tid, tname) in &teams {
                    @let projects = count(&project_counts, *tid);
                    @let boards = count(&board_counts, *tid);
                    a class="stripe lift card bg-base-100 border border-base-300 shadow-sm group" href={ "/teams/" (tid) } {
                        div class="card-body p-5 gap-4" {
                            div class="flex items-center gap-3" {
                                span class="inline-flex items-center justify-center w-11 h-11 rounded-xl bg-primary/15 text-primary" {
                                    (icon("team", "w-6 h-6"))
                                }
                                span class="flex flex-col min-w-0" {
                                    (kind_tag("Team"))
                                    span class="text-xl font-bold leading-tight truncate group-hover:text-primary transition-colors" { (tname) }
                                }
                            }
                            div class="flex items-center gap-4 text-sm" {
                                span class="inline-flex items-center gap-1.5 opacity-70" {
                                    (icon("project", "w-4 h-4 text-secondary")) (projects) (if projects == 1 { " project" } else { " projects" })
                                }
                                span class="inline-flex items-center gap-1.5 opacity-70" {
                                    (icon("board", "w-4 h-4 text-accent")) (boards) (if boards == 1 { " board" } else { " boards" })
                                }
                                span class="ml-auto font-medium text-primary opacity-0 group-hover:opacity-100 transition-opacity" { "Open →" }
                            }
                        }
                    }
                }
            }
        },
    ))
}

pub async fn create_team(State(state): State<AppState>, user: User, Form(f): Form<NameForm>) -> Res<Response> {
    require(user.has("teams.create"))?;
    let id = query("INSERT INTO teams (name, created_at) VALUES (?, ?)")
        .bind(unique_name(&state.db, Named::Team, &f.name, None).await?)
        .bind(crate::db::now())
        .insert(&state.db)
        .await?;
    // A creator who cannot already manage every team becomes this team's first admin.
    if !user.has("teams.manage") {
        query("INSERT INTO team_members (team_id, user_id, role) VALUES (?, ?, 'admin')")
            .bind(id)
            .bind(user.id)
            .execute(&state.db)
            .await?;
    }
    Ok(redirect(format!("/teams/{id}")))
}

pub async fn team_page(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Markup> {
    let access = team_access(&state.db, &user, id).await?;
    let (name,): (String,) = query_as("SELECT name FROM teams WHERE id = ?").bind(id).fetch_one(&state.db).await?;
    let projects: Vec<(i64, String, String)> = query_as("SELECT id, name, description FROM projects WHERE team_id = ? ORDER BY name")
        .bind(id)
        .fetch_all(&state.db)
        .await?;
    let boards: Vec<(i64, i64, String, i64)> = query_as(
        "SELECT b.id, b.project_id, b.name, (SELECT COUNT(*) FROM cards c WHERE c.board_id = b.id) FROM boards b \
         JOIN projects p ON p.id = b.project_id WHERE p.team_id = ? ORDER BY b.name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let members: Vec<(i64, String, String, String)> = query_as(
        "SELECT u.id, u.username, u.display_name, m.role FROM team_members m JOIN users u ON u.id = m.user_id WHERE m.team_id = ? ORDER BY u.display_name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let candidates: Vec<(i64, String, String)> = query_as(
        "SELECT id, username, display_name FROM users WHERE kind != 'breakglass' AND disabled = 0 \
         AND id NOT IN (SELECT user_id FROM team_members WHERE team_id = ?) ORDER BY display_name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let base = format!("/teams/{id}");
    Ok(page(
        Some(&user),
        &name,
        false,
        html! {
            (crumbs(&[("Teams", Some("/".into())), (&name, None)]))
            (panel("Projects", html! {
                div class="flex flex-wrap gap-x-6 gap-y-1 text-sm" {
                    span class="inline-flex items-center gap-2" {
                        (icon("project", "w-4 h-4 text-secondary")) span { b { "Project" } span class="opacity-70" { " — a body of work with its own members and permissions" } }
                    }
                    span class="inline-flex items-center gap-2" {
                        (icon("board", "w-4 h-4 text-accent")) span { b { "Board" } span class="opacity-70" { " — a kanban of columns and cards inside a project" } }
                    }
                }
                @if projects.is_empty() { p class="text-sm opacity-60" { "No projects yet." } }
                @for (pid, pname, desc) in &projects {
                    @let project_boards: Vec<_> = boards.iter().filter(|b| b.1 == *pid).collect();
                    div class="rounded-box border border-base-300 border-l-4 border-l-secondary bg-base-200/60 p-4 flex flex-col gap-3" {
                        a class="flex items-center gap-3 group" href={ "/projects/" (pid) } {
                            (icon("project", "w-6 h-6 text-secondary"))
                            span class="flex flex-col min-w-0" {
                                (kind_tag("Project"))
                                span class="font-semibold leading-tight group-hover:text-secondary transition-colors" { (pname) }
                                @if !desc.is_empty() { span class="text-sm opacity-70 line-clamp-2" { (desc) } }
                            }
                            span class="ml-auto shrink-0 text-xs opacity-60" {
                                (project_boards.len()) (if project_boards.len() == 1 { " board" } else { " boards" })
                            }
                        }
                        div class="flex flex-col gap-1.5 pl-9" {
                            (kind_tag("Boards in this project"))
                            div class="flex flex-wrap gap-2" {
                                @for (bid, _, bname, cards) in &project_boards {
                                    a class="lift inline-flex items-center gap-2 rounded-lg border border-base-300 bg-base-100 px-3 py-2 text-sm" href={ "/boards/" (bid) } {
                                        (icon("board", "w-4 h-4 text-accent"))
                                        span class="font-medium" { (bname) }
                                        span class="text-xs opacity-60" { (cards) (if *cards == 1 { " card" } else { " cards" }) }
                                    }
                                }
                                @if project_boards.is_empty() {
                                    a class="text-sm opacity-60 link link-hover" href={ "/projects/" (pid) } { "No boards yet — open the project to add one." }
                                }
                            }
                        }
                    }
                }
                @if access.admin {
                    form method="post" action={ (base) "/projects" } class="join mt-2" {
                        input class="input input-sm join-item" name="name" placeholder="New project name" required;
                        button class="btn btn-primary btn-sm join-item" { "Create project" }
                    }
                }
            }))
            (panel("Members", html! {
                table class="table table-sm" {
                    thead { tr { th { "User" } th { "Team role" } th {} } }
                    tbody {
                        @for (uid, username, display, role) in &members {
                            tr {
                                td { (display) " " span class="opacity-60" { "(" (username) ")" } }
                                td {
                                    @if access.admin {
                                        form method="post" action={ (base) "/members" } {
                                            input type="hidden" name="user_id" value=(uid);
                                            select class="select select-sm w-40" name="role" onchange="this.form.submit()" {
                                                option value="member" selected[role == "member"] { "Member" }
                                                option value="admin" selected[role == "admin"] { "Team admin" }
                                            }
                                        }
                                    } @else {
                                        (if role == "admin" { "Team admin" } else { "Member" })
                                    }
                                }
                                td class="text-right" {
                                    @if access.admin {
                                        form method="post" action={ (base) "/members/" (uid) "/delete" } {
                                            button class="btn btn-ghost btn-xs text-error" { "Remove" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                @if members.is_empty() { p class="text-sm opacity-60" { "Nobody has been added to this team yet." } }
                @if access.admin {
                    form method="post" action={ (base) "/members" } class="flex flex-wrap gap-2 items-center" {
                        select class="select select-sm w-64" name="user_id" required {
                            option value="" disabled selected { "Add a user…" }
                            (user_options(&candidates))
                        }
                        select class="select select-sm w-40" name="role" {
                            option value="member" { "Member" }
                            option value="admin" { "Team admin" }
                        }
                        button class="btn btn-sm btn-primary" { "Add" }
                    }
                    p class="text-xs opacity-60" {
                        "Users appear here after their first single sign-on. Team admins can create projects, manage every board in the team and grant project permissions."
                    }
                }
            }))
            @if access.admin {
                (panel("Team settings", html! {
                    form method="post" action=(base) class="join" {
                        input class="input input-sm join-item" name="name" value=(name) required;
                        button class="btn btn-sm join-item" { "Rename" }
                    }
                    form method="post" action={ (base) "/delete" } data-confirm="Delete this team and all of its projects, boards and cards?" {
                        button class="btn btn-sm btn-error btn-outline" { "Delete team" }
                    }
                }))
            }
        },
    ))
}

pub async fn rename_team(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<NameForm>) -> Res<Response> {
    require(team_access(&state.db, &user, id).await?.admin)?;
    query("UPDATE teams SET name = ? WHERE id = ?")
        .bind(unique_name(&state.db, Named::Team, &f.name, Some(id)).await?)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/teams/{id}")))
}

pub async fn delete_team(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    require(team_access(&state.db, &user, id).await?.admin)?;
    crate::ops::delete_team(&state.db, id).await?;
    Ok(redirect("/"))
}

#[derive(Deserialize)]
pub struct TeamMemberForm {
    user_id: i64,
    role: String,
}

pub async fn set_team_member(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Form(f): Form<TeamMemberForm>,
) -> Res<Response> {
    require(team_access(&state.db, &user, id).await?.admin)?;
    if f.role != "admin" && f.role != "member" {
        return Err(bad("Unknown team role"));
    }
    // Update-or-insert without vendor upsert syntax.
    let existing: Option<(i64,)> = query_as("SELECT user_id FROM team_members WHERE team_id = ? AND user_id = ?")
        .bind(id)
        .bind(f.user_id)
        .fetch_optional(&state.db)
        .await?;
    if existing.is_some() {
        query("UPDATE team_members SET role = ? WHERE team_id = ? AND user_id = ?")
            .bind(&f.role)
            .bind(id)
            .bind(f.user_id)
            .execute(&state.db)
            .await?;
    } else {
        query("INSERT INTO team_members (team_id, user_id, role) VALUES (?, ?, ?)")
            .bind(id)
            .bind(f.user_id)
            .bind(&f.role)
            .execute(&state.db)
            .await?;
    }
    Ok(redirect(format!("/teams/{id}")))
}

pub async fn remove_team_member(State(state): State<AppState>, user: User, Path((id, uid)): Path<(i64, i64)>) -> Res<Response> {
    require(team_access(&state.db, &user, id).await?.admin)?;
    query("DELETE FROM project_members WHERE user_id = ? AND project_id IN (SELECT id FROM projects WHERE team_id = ?)")
        .bind(uid)
        .bind(id)
        .execute(&state.db)
        .await?;
    query("DELETE FROM team_members WHERE team_id = ? AND user_id = ?")
        .bind(id)
        .bind(uid)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/teams/{id}")))
}

pub async fn create_project(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<NameForm>) -> Res<Response> {
    require(team_access(&state.db, &user, id).await?.admin)?;
    let pid = query("INSERT INTO projects (team_id, name, created_at) VALUES (?, ?, ?)")
        .bind(id)
        .bind(unique_name(&state.db, Named::Project { team_id: id }, &f.name, None).await?)
        .bind(crate::db::now())
        .insert(&state.db)
        .await?;
    Ok(redirect(format!("/projects/{pid}")))
}

const PROJECT_PERMS: &[(&str, &str, &str)] = &[
    ("manage", "Manage", "Create, edit, delete and re-order boards and columns; manage labels and agent settings"),
    ("create_cards", "Create cards", "Add new cards"),
    ("comment", "Comment", "Comment on any card"),
    ("edit_own", "Edit own cards", "Edit, move and delete cards they created"),
    ("edit_all", "Edit all cards", "Edit, move and delete any card"),
];

pub async fn project_page(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Res<Markup> {
    let access = project_access(&state.db, &user, id).await?;
    let jira_panel = super::jira::settings_panel(&state, id, access.team_admin).await?;
    let (agent_service_id, agent_model): (Option<i64>, String) = query_as("SELECT agent_service_id, agent_model FROM projects WHERE id = ?")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    let services = llm::services(&state.db).await?;
    let effective = llm::resolve(&state.db, id, None).await;
    let platform: llm::AgentDefaults = crate::db::get_setting(&state.db, "agent").await;
    let inherit = match services.iter().find(|s| Some(s.id) == platform.service_id) {
        Some(s) => format!("Platform default ({} · {})", s.name, platform.model),
        None => "Platform default (none set)".to_string(),
    };
    let (name, description, team_name): (String, String, String) =
        query_as("SELECT p.name, p.description, t.name FROM projects p JOIN teams t ON t.id = p.team_id WHERE p.id = ?")
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    let boards: Vec<(i64, String, i64)> = query_as(
        "SELECT b.id, b.name, (SELECT COUNT(*) FROM cards c WHERE c.board_id = b.id) FROM boards b WHERE b.project_id = ? ORDER BY b.name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let members: Vec<(i64, String, String, bool, bool, bool, bool, bool)> = query_as(
        "SELECT u.id, u.username, u.display_name, pm.can_manage, pm.can_create_cards, pm.can_comment, pm.can_edit_own, pm.can_edit_all \
         FROM project_members pm JOIN users u ON u.id = pm.user_id WHERE pm.project_id = ? ORDER BY u.display_name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let candidates: Vec<(i64, String, String)> = query_as(
        "SELECT u.id, u.username, u.display_name FROM team_members tm JOIN users u ON u.id = tm.user_id \
         WHERE tm.team_id = ? AND u.id NOT IN (SELECT user_id FROM project_members WHERE project_id = ?) ORDER BY u.display_name",
    )
    .bind(access.team_id)
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let base = format!("/projects/{id}");
    Ok(page(
        Some(&user),
        &name,
        false,
        html! {
            div class="flex items-center gap-3" {
                div class="flex-1" {
                    (crumbs(&[("Teams", Some("/".into())), (&team_name, Some(format!("/teams/{}", access.team_id))), (&name, None)]))
                }
                @if access.member {
                    form method="post" action={ (base) "/leave" } { button class="btn btn-sm btn-ghost" { "Leave project" } }
                } @else {
                    form method="post" action={ (base) "/join" } { button class="btn btn-sm btn-primary" { "Join project" } }
                }
            }
            @if !description.is_empty() { p class="opacity-80" { (description) } }
            @if !access.member && !access.manage {
                div role="alert" class="alert text-sm" { "You can view this project. Join it to create cards and comment." }
            }
            (panel("Boards", html! {
                @if boards.is_empty() { p class="text-sm opacity-60" { "No boards yet." } }
                div class="grid sm:grid-cols-2 lg:grid-cols-3 gap-3" {
                    @for (bid, bname, count) in &boards {
                        a class="lift card bg-base-200 border border-base-300" href={ "/boards/" (bid) } {
                            div class="card-body p-4 gap-1" {
                                div class="flex items-center gap-2" { (icon("board", "w-5 h-5 text-accent")) (kind_tag("Board")) }
                                div class="font-semibold" { (bname) }
                                div class="text-sm opacity-60" { (count) " cards" }
                            }
                        }
                    }
                }
                @if access.manage {
                    form method="post" action={ (base) "/boards" } class="join mt-2" {
                        input class="input input-sm join-item" name="name" placeholder="New board name" required;
                        button class="btn btn-primary btn-sm join-item" { "Create board" }
                    }
                }
            }))
            (panel("Agent", html! {
                p class="text-sm opacity-70" {
                    "The LLM service and model the agent uses on every board in this project. Currently: "
                    @match &effective {
                        Ok(t) => { b { (t.label()) } " (from the " (t.source) ")." },
                        Err(_) => { b { "none" } " — the agent console will not work until a service is chosen." },
                    }
                    " Individual columns can override it for their “Send To Agent” prompt."
                }
                @if access.manage {
                    @if services.is_empty() {
                        p class="text-sm opacity-60" { "No agent services exist yet. A platform administrator adds them under Admin → Agent services." }
                    } @else {
                        form method="post" action={ (base) "/agent" } class="flex flex-wrap items-center gap-2 max-w-2xl" {
                            div class="flex-1 min-w-64" {
                                (target_select("agent_target", &services, &llm::target_value(agent_service_id, &agent_model), &inherit))
                            }
                            button class="btn btn-sm btn-primary" { "Save" }
                        }
                    }
                }
            }))
            div id="jira" class="scroll-mt-20 flex flex-col gap-2" {
                @if let Some(m) = q.get("jira_msg") { div role="alert" class="alert alert-success text-sm" { (m) } }
                @if let Some(m) = q.get("jira_error") { div role="alert" class="alert alert-error text-sm" { (m) } }
                (jira_panel)
            }
            (panel("Members & permissions", html! {
                div class="overflow-x-auto" {
                    table class="table table-sm" {
                        thead {
                            tr {
                                th { "User" }
                                @for (_, label, help) in PROJECT_PERMS { th title=(help) { (label) } }
                                th {}
                            }
                        }
                        tbody {
                            @for (uid, username, display, manage, create, comment, own, all) in &members {
                                @let form_id = format!("pm-{uid}");
                                tr {
                                    td {
                                        (display) " " span class="opacity-60" { "(" (username) ")" }
                                        @if access.team_admin {
                                            form id=(form_id) method="post" action={ (base) "/members" } {
                                                input type="hidden" name="user_id" value=(uid);
                                            }
                                        }
                                    }
                                    @for ((key, _, _), on) in PROJECT_PERMS.iter().zip([manage, create, comment, own, all]) {
                                        td {
                                            input type="checkbox" class="checkbox checkbox-sm" name=(key) form=(form_id)
                                                checked[*on] disabled[!access.team_admin] onchange="this.form.submit()";
                                        }
                                    }
                                    td class="text-right" {
                                        @if access.team_admin {
                                            form method="post" action={ (base) "/members/" (uid) "/delete" } {
                                                button class="btn btn-ghost btn-xs text-error" { "Remove" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                @if members.is_empty() { p class="text-sm opacity-60" { "Nobody has joined this project yet." } }
                @if access.team_admin {
                    form method="post" action={ (base) "/members" } class="flex flex-wrap gap-2 items-center" {
                        select class="select select-sm w-64" name="user_id" required {
                            option value="" disabled selected { "Add a team member…" }
                            (user_options(&candidates))
                        }
                        input type="hidden" name="create_cards" value="on";
                        input type="hidden" name="comment" value="on";
                        input type="hidden" name="edit_own" value="on";
                        button class="btn btn-sm btn-primary" { "Add" }
                    }
                    p class="text-xs opacity-60" {
                        "Team members can join on their own with the default permissions (create cards, comment, edit own cards). Team admins grant or revoke the rest here; changes save immediately."
                    }
                }
            }))
            @if access.team_admin {
                (panel("Project settings", html! {
                    form method="post" action=(base) class="flex flex-col gap-2 max-w-xl" {
                        input class="input input-sm w-full" name="name" value=(name) required;
                        textarea class="textarea textarea-sm w-full" name="description" rows="2" placeholder="Description" { (description) }
                        div { button class="btn btn-sm" { "Save" } }
                    }
                    form method="post" action={ (base) "/delete" } data-confirm="Delete this project and all of its boards and cards?" {
                        button class="btn btn-sm btn-error btn-outline" { "Delete project" }
                    }
                }))
            }
        },
    ))
}

pub async fn update_project(State(state): State<AppState>, user: User, Path(id): Path<i64>, Form(f): Form<NameForm>) -> Res<Response> {
    let access = project_access(&state.db, &user, id).await?;
    require(access.team_admin)?;
    query("UPDATE projects SET name = ?, description = ? WHERE id = ?")
        .bind(unique_name(&state.db, Named::Project { team_id: access.team_id }, &f.name, Some(id)).await?)
        .bind(f.description.trim())
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/projects/{id}")))
}

pub async fn delete_project(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    let access = project_access(&state.db, &user, id).await?;
    require(access.team_admin)?;
    crate::ops::delete_project(&state.db, id).await?;
    Ok(redirect(format!("/teams/{}", access.team_id)))
}

pub async fn join_project(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    // project_access already guarantees the user is on the team (or administers it).
    project_access(&state.db, &user, id).await?;
    let joined: Option<(i64,)> = query_as("SELECT user_id FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await?;
    if joined.is_none() {
        query("INSERT INTO project_members (project_id, user_id) VALUES (?, ?)")
            .bind(id)
            .bind(user.id)
            .execute(&state.db)
            .await?;
    }
    Ok(redirect(format!("/projects/{id}")))
}

pub async fn leave_project(State(state): State<AppState>, user: User, Path(id): Path<i64>) -> Res<Response> {
    project_access(&state.db, &user, id).await?;
    query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/projects/{id}")))
}

#[derive(Deserialize)]
pub struct ProjectMemberForm {
    user_id: i64,
    manage: Option<String>,
    create_cards: Option<String>,
    comment: Option<String>,
    edit_own: Option<String>,
    edit_all: Option<String>,
}

pub async fn set_project_member(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Form(f): Form<ProjectMemberForm>,
) -> Res<Response> {
    let access = project_access(&state.db, &user, id).await?;
    require(access.team_admin)?;
    let on_team: Option<(i64,)> = query_as("SELECT 1 FROM team_members WHERE team_id = ? AND user_id = ?")
        .bind(access.team_id)
        .bind(f.user_id)
        .fetch_optional(&state.db)
        .await?;
    if on_team.is_none() {
        return Err(AppError::Bad("That user is not a member of this team".into()));
    }
    let flags = [f.manage.is_some(), f.create_cards.is_some(), f.comment.is_some(), f.edit_own.is_some(), f.edit_all.is_some()];
    let existing: Option<(i64,)> = query_as("SELECT user_id FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(id)
        .bind(f.user_id)
        .fetch_optional(&state.db)
        .await?;
    let sql = if existing.is_some() {
        "UPDATE project_members SET can_manage = ?, can_create_cards = ?, can_comment = ?, can_edit_own = ?, can_edit_all = ? \
         WHERE project_id = ? AND user_id = ?"
    } else {
        "INSERT INTO project_members (can_manage, can_create_cards, can_comment, can_edit_own, can_edit_all, project_id, user_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    };
    let mut statement = query(sql);
    for flag in flags {
        statement = statement.bind(flag);
    }
    statement.bind(id).bind(f.user_id).execute(&state.db).await?;
    Ok(redirect(format!("/projects/{id}")))
}

pub async fn remove_project_member(State(state): State<AppState>, user: User, Path((id, uid)): Path<(i64, i64)>) -> Res<Response> {
    require(project_access(&state.db, &user, id).await?.team_admin)?;
    query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(id)
        .bind(uid)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/projects/{id}")))
}

#[derive(Deserialize)]
pub struct ProjectAgentForm {
    #[serde(default)]
    agent_target: String,
}

pub async fn set_project_agent(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Form(f): Form<ProjectAgentForm>,
) -> Res<Response> {
    require(project_access(&state.db, &user, id).await?.manage)?;
    let (service_id, model) = llm::parse_target(&state.db, &f.agent_target).await?;
    query("UPDATE projects SET agent_service_id = ?, agent_model = ? WHERE id = ?")
        .bind(service_id)
        .bind(model)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(redirect(format!("/projects/{id}")))
}
