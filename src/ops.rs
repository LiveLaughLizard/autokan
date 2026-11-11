//! Board data access and mutations shared by the HTTP handlers and the agent tools.
//! Callers are responsible for permission checks (see `perm`).
use crate::db::{now, query, query_as, Conn};
use crate::db::Db;
use crate::error::{bad, AppError, Res};
use std::collections::HashMap;

pub const DEFAULT_COLUMNS: &[&str] = &["Todo", "In Progress", "Human Review", "Done"];

#[derive(Clone)]
pub struct Board {
    pub id: i64,
    pub name: String,
    pub agent_system_prompt: String,
}

#[derive(Clone)]
pub struct Column {
    pub id: i64,
    pub board_id: i64,
    pub name: String,
    pub agent_prompt: String,
    /// Optional service / model override for this column's "Send To Agent".
    pub agent_service_id: Option<i64>,
    pub agent_model: String,
}

#[derive(Clone)]
pub struct Card {
    pub id: i64,
    pub board_id: i64,
    pub column_id: i64,
    pub title: String,
    pub description: String,
    pub created_by: Option<i64>,
    pub created_at: String,
    pub author: Option<String>,
    pub comment_count: i64,
}

#[derive(Clone)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: String,
}

#[derive(Clone)]
pub struct Person {
    pub id: i64,
    pub username: String,
    pub display_name: String,
}

pub struct BoardData {
    pub board: Board,
    pub columns: Vec<Column>,
    pub cards: Vec<Card>,
    pub labels: Vec<Label>,
    pub card_labels: HashMap<i64, Vec<Label>>,
    pub card_assignees: HashMap<i64, Vec<Person>>,
    /// Card id -> key of the Jira issue created from it.
    pub jira: HashMap<i64, String>,
}

const CARD_SELECT: &str = "SELECT c.id, c.board_id, c.column_id, c.title, c.description, c.created_by, c.created_at, \
    u.display_name AS author, (SELECT COUNT(*) FROM comments m WHERE m.card_id = c.id) AS comment_count \
    FROM cards c LEFT JOIN users u ON u.id = c.created_by";

pub fn clean_name(s: &str, what: &str) -> Res<String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(bad(format!("{what} cannot be empty")));
    }
    if s.chars().count() > 200 {
        return Err(bad(format!("{what} is too long (max 200 characters)")));
    }
    Ok(s.to_string())
}

/// The things whose names must be unique among their siblings.
pub enum Named {
    Team,
    Project { team_id: i64 },
    Board { project_id: i64 },
}

/// Cleans a name and rejects it if a sibling already uses it (ignoring case). `except` is the
/// row being renamed, so saving a thing under its current name is fine.
pub async fn unique_name(db: &Db, kind: Named, name: &str, except: Option<i64>) -> Res<String> {
    let (what, table, scope, place) = match kind {
        Named::Team => ("Team name", "teams", None, "There is already a team"),
        Named::Project { team_id } => ("Project name", "projects", Some(("team_id", team_id)), "This team already has a project"),
        Named::Board { project_id } => ("Board name", "boards", Some(("project_id", project_id)), "This project already has a board"),
    };
    let name = clean_name(name, what)?;
    // LOWER() on both sides is the one case-insensitive comparison every backend agrees on.
    let mut sql = format!("SELECT id FROM {table} WHERE LOWER(name) = LOWER(?) AND id != ?");
    if let Some((column, _)) = scope {
        sql.push_str(&format!(" AND {column} = ?"));
    }
    let mut lookup = query_as::<(i64,)>(&sql).bind(&name).bind(except.unwrap_or(-1));
    if let Some((_, id)) = scope {
        lookup = lookup.bind(id);
    }
    let taken = lookup.fetch_optional(db).await?;
    if taken.is_some() {
        return Err(bad(format!("{place} named “{name}”. Choose a different name.")));
    }
    Ok(name)
}

pub async fn get_board(db: &Db, id: i64) -> Res<Board> {
    query_as("SELECT id, name, agent_system_prompt FROM boards WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn get_column(db: &Db, id: i64) -> Res<Column> {
    query_as("SELECT id, board_id, name, agent_prompt, agent_service_id, agent_model FROM columns WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn get_card(db: &Db, id: i64) -> Res<Card> {
    query_as(&format!("{CARD_SELECT} WHERE c.id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn columns(db: &Db, board_id: i64) -> Res<Vec<Column>> {
    Ok(query_as("SELECT id, board_id, name, agent_prompt, agent_service_id, agent_model FROM columns WHERE board_id = ? ORDER BY position, id")
        .bind(board_id)
        .fetch_all(db)
        .await?)
}

pub async fn labels(db: &Db, board_id: i64) -> Res<Vec<Label>> {
    Ok(query_as("SELECT id, name, color FROM labels WHERE board_id = ? ORDER BY name")
        .bind(board_id)
        .fetch_all(db)
        .await?)
}

pub async fn card_labels(db: &Db, card_id: i64) -> Res<Vec<Label>> {
    Ok(query_as(
        "SELECT l.id, l.name, l.color FROM card_labels cl JOIN labels l ON l.id = cl.label_id WHERE cl.card_id = ? ORDER BY l.name",
    )
    .bind(card_id)
    .fetch_all(db)
    .await?)
}

pub async fn card_assignees(db: &Db, card_id: i64) -> Res<Vec<Person>> {
    Ok(query_as(
        "SELECT u.id, u.username, u.display_name FROM card_assignees a JOIN users u ON u.id = a.user_id WHERE a.card_id = ? ORDER BY u.display_name",
    )
    .bind(card_id)
    .fetch_all(db)
    .await?)
}

pub async fn project_members(db: &Db, project_id: i64) -> Res<Vec<Person>> {
    Ok(query_as(
        "SELECT u.id, u.username, u.display_name FROM project_members pm JOIN users u ON u.id = pm.user_id \
         WHERE pm.project_id = ? ORDER BY u.display_name",
    )
    .bind(project_id)
    .fetch_all(db)
    .await?)
}

pub async fn load_board(db: &Db, board_id: i64) -> Res<BoardData> {
    let board = get_board(db, board_id).await?;
    let cards: Vec<Card> = query_as(&format!("{CARD_SELECT} WHERE c.board_id = ? ORDER BY c.position, c.id"))
        .bind(board_id)
        .fetch_all(db)
        .await?;
    let mut card_labels: HashMap<i64, Vec<Label>> = HashMap::new();
    let rows: Vec<(i64, i64, String, String)> = query_as(
        "SELECT cl.card_id, l.id, l.name, l.color FROM card_labels cl JOIN labels l ON l.id = cl.label_id WHERE l.board_id = ? ORDER BY l.name",
    )
    .bind(board_id)
    .fetch_all(db)
    .await?;
    for (card_id, id, name, color) in rows {
        card_labels.entry(card_id).or_default().push(Label { id, name, color });
    }
    let mut card_assignees: HashMap<i64, Vec<Person>> = HashMap::new();
    let rows: Vec<(i64, i64, String, String)> = query_as(
        "SELECT a.card_id, u.id, u.username, u.display_name FROM card_assignees a JOIN cards c ON c.id = a.card_id \
         JOIN users u ON u.id = a.user_id WHERE c.board_id = ? ORDER BY u.display_name",
    )
    .bind(board_id)
    .fetch_all(db)
    .await?;
    for (card_id, id, username, display_name) in rows {
        card_assignees.entry(card_id).or_default().push(Person { id, username, display_name });
    }
    let jira: Vec<(i64, String)> =
        query_as("SELECT l.card_id, l.issue_key FROM jira_links l JOIN cards c ON c.id = l.card_id WHERE c.board_id = ?")
            .bind(board_id)
            .fetch_all(db)
            .await?;
    Ok(BoardData {
        jira: jira.into_iter().collect(),
        columns: columns(db, board_id).await?,
        labels: labels(db, board_id).await?,
        board,
        cards,
        card_labels,
        card_assignees,
    })
}

pub async fn create_board(db: &Db, project_id: i64, name: &str) -> Res<i64> {
    let name = unique_name(db, Named::Board { project_id }, name, None).await?;
    let tx = db.begin().await?;
    let id = query("INSERT INTO boards (project_id, name, created_at) VALUES (?, ?, ?)")
        .bind(project_id)
        .bind(name)
        .bind(now())
        .insert(&tx)
        .await?;
    for (i, col) in DEFAULT_COLUMNS.iter().enumerate() {
        query("INSERT INTO columns (board_id, name, position) VALUES (?, ?, ?)")
            .bind(id)
            .bind(*col)
            .bind(i as i64)
            .execute(&tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn create_column(db: &Db, board_id: i64, name: &str) -> Res<i64> {
    let name = clean_name(name, "Column name")?;
    let (position,): (i64,) = query_as("SELECT COALESCE(MAX(position), -1) + 1 FROM columns WHERE board_id = ?")
        .bind(board_id)
        .fetch_one(db)
        .await?;
    Ok(query("INSERT INTO columns (board_id, name, position) VALUES (?, ?, ?)")
        .bind(board_id)
        .bind(name)
        .bind(position)
        .insert(db)
        .await?)
}

/// Apply a full or partial ordering; ids that are not on the board are rejected.
pub async fn set_column_order(db: &Db, board_id: i64, ids: &[i64]) -> Res<()> {
    let mut order: Vec<i64> = columns(db, board_id).await?.iter().map(|c| c.id).collect();
    if ids.iter().any(|id| !order.contains(id)) {
        return Err(bad("Column does not belong to this board"));
    }
    order.retain(|id| !ids.contains(id));
    let mut all = ids.to_vec();
    all.dedup();
    all.extend(order);
    let tx = db.begin().await?;
    for (i, id) in all.iter().enumerate() {
        query("UPDATE columns SET position = ? WHERE id = ?")
            .bind(i as i64)
            .bind(id)
            .execute(&tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn move_column(db: &Db, board_id: i64, column_id: i64, index: usize) -> Res<()> {
    let mut order: Vec<i64> = columns(db, board_id).await?.iter().map(|c| c.id).collect();
    if !order.contains(&column_id) {
        return Err(bad("Column does not belong to this board"));
    }
    order.retain(|id| *id != column_id);
    order.insert(index.min(order.len()), column_id);
    set_column_order(db, board_id, &order).await
}

pub async fn create_card(db: &Db, column: &Column, user_id: i64, title: &str, description: &str) -> Res<i64> {
    let title = clean_name(title, "Card title")?;
    let (position,): (i64,) = query_as("SELECT COALESCE(MAX(position), -1) + 1 FROM cards WHERE column_id = ?")
        .bind(column.id)
        .fetch_one(db)
        .await?;
    let now = now();
    Ok(query(
        "INSERT INTO cards (board_id, column_id, title, description, created_by, position, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(column.board_id)
    .bind(column.id)
    .bind(title)
    .bind(description)
    .bind(user_id)
    .bind(position)
    .bind(&now)
    .bind(&now)
    .insert(db)
    .await?)
}

/// Move a card to `column_id` at `index` (end of the column when None), renumbering the column.
pub async fn move_card(db: &Db, card: &Card, column_id: i64, index: Option<usize>) -> Res<()> {
    let column = get_column(db, column_id).await?;
    if column.board_id != card.board_id {
        return Err(bad("Column does not belong to this board"));
    }
    let tx = db.begin().await?;
    let rows: Vec<(i64,)> = query_as("SELECT id FROM cards WHERE column_id = ? AND id != ? ORDER BY position, id")
        .bind(column_id)
        .bind(card.id)
        .fetch_all(&tx)
        .await?;
    let mut order: Vec<i64> = rows.into_iter().map(|r| r.0).collect();
    let at = index.unwrap_or(order.len()).min(order.len());
    order.insert(at, card.id);
    query("UPDATE cards SET column_id = ?, updated_at = ? WHERE id = ?")
        .bind(column_id)
        .bind(now())
        .bind(card.id)
        .execute(&tx)
        .await?;
    for (i, id) in order.iter().enumerate() {
        query("UPDATE cards SET position = ? WHERE id = ?")
            .bind(i as i64)
            .bind(id)
            .execute(&tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn update_card(db: &Db, card_id: i64, title: Option<&str>, description: Option<&str>) -> Res<()> {
    if let Some(t) = title {
        query("UPDATE cards SET title = ?, updated_at = ? WHERE id = ?")
            .bind(clean_name(t, "Card title")?)
            .bind(now())
            .bind(card_id)
            .execute(db)
            .await?;
    }
    if let Some(d) = description {
        query("UPDATE cards SET description = ?, updated_at = ? WHERE id = ?")
            .bind(d)
            .bind(now())
            .bind(card_id)
            .execute(db)
            .await?;
    }
    Ok(())
}

/// `on = None` toggles. The label must belong to the card's board.
pub async fn set_card_label(db: &Db, card: &Card, label_id: i64, on: Option<bool>) -> Res<()> {
    let ok: Option<(i64,)> = query_as("SELECT id FROM labels WHERE id = ? AND board_id = ?")
        .bind(label_id)
        .bind(card.board_id)
        .fetch_optional(db)
        .await?;
    if ok.is_none() {
        return Err(bad("Label does not belong to this board"));
    }
    let has: Option<(i64,)> = query_as("SELECT 1 FROM card_labels WHERE card_id = ? AND label_id = ?")
        .bind(card.id)
        .bind(label_id)
        .fetch_optional(db)
        .await?;
    let sql = match (on.unwrap_or(has.is_none()), has.is_some()) {
        (true, false) => "INSERT INTO card_labels (card_id, label_id) VALUES (?, ?)",
        (false, true) => "DELETE FROM card_labels WHERE card_id = ? AND label_id = ?",
        _ => return Ok(()),
    };
    query(sql).bind(card.id).bind(label_id).execute(db).await?;
    Ok(())
}

/// `on = None` toggles. Only project members can be assigned.
pub async fn set_card_assignee(db: &Db, card: &Card, project_id: i64, user_id: i64, on: Option<bool>) -> Res<()> {
    let has: Option<(i64,)> = query_as("SELECT 1 FROM card_assignees WHERE card_id = ? AND user_id = ?")
        .bind(card.id)
        .bind(user_id)
        .fetch_optional(db)
        .await?;
    let sql = match (on.unwrap_or(has.is_none()), has.is_some()) {
        (true, false) => {
            let member: Option<(i64,)> = query_as("SELECT 1 FROM project_members WHERE project_id = ? AND user_id = ?")
                .bind(project_id)
                .bind(user_id)
                .fetch_optional(db)
                .await?;
            if member.is_none() {
                return Err(bad("Only users who have joined the project can be assigned"));
            }
            "INSERT INTO card_assignees (card_id, user_id) VALUES (?, ?)"
        }
        (false, true) => "DELETE FROM card_assignees WHERE card_id = ? AND user_id = ?",
        _ => return Ok(()),
    };
    query(sql).bind(card.id).bind(user_id).execute(db).await?;
    Ok(())
}

pub async fn add_comment(db: &Db, card_id: i64, user_id: i64, body: &str) -> Res<i64> {
    if body.trim().is_empty() {
        return Err(bad("Comment cannot be empty"));
    }
    Ok(query("INSERT INTO comments (card_id, user_id, body, created_at) VALUES (?, ?, ?, ?)")
        .bind(card_id)
        .bind(user_id)
        .bind(body)
        .bind(now())
        .insert(db)
        .await?)
}

// ---- Deletes -----------------------------------------------------------------------------
//
// Children are removed explicitly, parents last, inside one transaction. SQL Server cannot
// cascade through this schema (see `db::schema`), so nothing here relies on ON DELETE CASCADE.

async fn run_all<C: Conn + ?Sized>(conn: &C, statements: &[&str], id: i64) -> Res<()> {
    for sql in statements {
        query(sql).bind(id).execute(conn).await?;
    }
    Ok(())
}

async fn delete_card_in<C: Conn + ?Sized>(conn: &C, id: i64) -> Res<()> {
    run_all(
        conn,
        &[
            "DELETE FROM jira_links WHERE card_id = ?",
            "DELETE FROM jira_comments WHERE card_id = ?",
            "DELETE FROM jira_files WHERE card_id = ?",
            "DELETE FROM card_labels WHERE card_id = ?",
            "DELETE FROM card_assignees WHERE card_id = ?",
            "DELETE FROM comments WHERE card_id = ?",
            "DELETE FROM cards WHERE id = ?",
        ],
        id,
    )
    .await
}

async fn delete_board_in<C: Conn + ?Sized>(conn: &C, id: i64) -> Res<()> {
    run_all(
        conn,
        &[
            "DELETE FROM jira_links WHERE card_id IN (SELECT id FROM cards WHERE board_id = ?)",
            "DELETE FROM jira_comments WHERE card_id IN (SELECT id FROM cards WHERE board_id = ?)",
            "DELETE FROM jira_files WHERE card_id IN (SELECT id FROM cards WHERE board_id = ?)",
            "DELETE FROM card_labels WHERE card_id IN (SELECT id FROM cards WHERE board_id = ?)",
            "DELETE FROM card_assignees WHERE card_id IN (SELECT id FROM cards WHERE board_id = ?)",
            "DELETE FROM comments WHERE card_id IN (SELECT id FROM cards WHERE board_id = ?)",
            "DELETE FROM cards WHERE board_id = ?",
            "DELETE FROM labels WHERE board_id = ?",
            "DELETE FROM columns WHERE board_id = ?",
            "DELETE FROM uploads WHERE board_id = ?",
            "DELETE FROM agent_messages WHERE board_id = ?",
            "DELETE FROM sync_cards WHERE board_id = ?",
            "DELETE FROM sync_columns WHERE board_id = ?",
            "DELETE FROM board_sync WHERE board_id = ?",
            "DELETE FROM boards WHERE id = ?",
        ],
        id,
    )
    .await
}

async fn delete_project_in<C: Conn + ?Sized>(conn: &C, id: i64) -> Res<()> {
    let boards: Vec<(i64,)> = query_as("SELECT id FROM boards WHERE project_id = ?").bind(id).fetch_all(conn).await?;
    for (board_id,) in boards {
        delete_board_in(conn, board_id).await?;
    }
    run_all(conn, &["DELETE FROM jira_settings WHERE project_id = ?", "DELETE FROM project_members WHERE project_id = ?", "DELETE FROM projects WHERE id = ?"], id).await
}

pub async fn delete_card(db: &Db, id: i64) -> Res<()> {
    let tx = db.begin().await?;
    delete_card_in(&tx, id).await?;
    Ok(tx.commit().await?)
}

pub async fn delete_column(db: &Db, id: i64) -> Res<()> {
    let tx = db.begin().await?;
    run_all(
        &tx,
        &[
            "DELETE FROM jira_links WHERE card_id IN (SELECT id FROM cards WHERE column_id = ?)",
            "DELETE FROM jira_comments WHERE card_id IN (SELECT id FROM cards WHERE column_id = ?)",
            "DELETE FROM jira_files WHERE card_id IN (SELECT id FROM cards WHERE column_id = ?)",
            "DELETE FROM card_labels WHERE card_id IN (SELECT id FROM cards WHERE column_id = ?)",
            "DELETE FROM card_assignees WHERE card_id IN (SELECT id FROM cards WHERE column_id = ?)",
            "DELETE FROM comments WHERE card_id IN (SELECT id FROM cards WHERE column_id = ?)",
            "DELETE FROM cards WHERE column_id = ?",
            "DELETE FROM columns WHERE id = ?",
        ],
        id,
    )
    .await?;
    Ok(tx.commit().await?)
}

pub async fn delete_label(db: &Db, id: i64) -> Res<()> {
    let tx = db.begin().await?;
    run_all(&tx, &["DELETE FROM card_labels WHERE label_id = ?", "DELETE FROM labels WHERE id = ?"], id).await?;
    Ok(tx.commit().await?)
}

pub async fn delete_board(db: &Db, id: i64) -> Res<()> {
    let tx = db.begin().await?;
    delete_board_in(&tx, id).await?;
    Ok(tx.commit().await?)
}

pub async fn delete_project(db: &Db, id: i64) -> Res<()> {
    let tx = db.begin().await?;
    delete_project_in(&tx, id).await?;
    Ok(tx.commit().await?)
}

pub async fn delete_team(db: &Db, id: i64) -> Res<()> {
    let tx = db.begin().await?;
    let projects: Vec<(i64,)> = query_as("SELECT id FROM projects WHERE team_id = ?").bind(id).fetch_all(&tx).await?;
    for (project_id,) in projects {
        delete_project_in(&tx, project_id).await?;
    }
    run_all(&tx, &["DELETE FROM team_members WHERE team_id = ?", "DELETE FROM teams WHERE id = ?"], id).await?;
    Ok(tx.commit().await?)
}
// Row decoding
// Row decoding: fields are read in the order the queries above select them.
crate::row_struct!(Board { id, name, agent_system_prompt });
crate::row_struct!(Column { id, board_id, name, agent_prompt, agent_service_id, agent_model });
crate::row_struct!(Card { id, board_id, column_id, title, description, created_by, created_at, author, comment_count });
crate::row_struct!(Label { id, name, color });
crate::row_struct!(Person { id, username, display_name });
