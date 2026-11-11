//! Two-way synchronization of a board with a GitHub project or a GitLab issue board.
//!
//! Each cycle fetches the remote board and compares three states per column and per card: local,
//! remote, and the "base" both sides agreed on at the end of the previous cycle. Whichever side
//! differs from the base has changed and is copied to the other; if both changed the same thing,
//! the remote side wins. Columns, card titles, descriptions, which column a card is in, and the
//! order of cards inside a column are synchronized. Comments, labels and assignees are not.
//!
//! Local edits trigger a cycle within a couple of seconds (`SyncHandle::poke`); remote edits are
//! picked up by polling every `POLL_SECONDS`.
mod github;
mod gitlab;

use crate::db::{now, query, query_as, Db};
use crate::error::Res;
use crate::ops;
use crate::AppState;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;

pub const POLL_SECONDS: u64 = 60;
const CYCLE_TIMEOUT: Duration = Duration::from_secs(180);

pub const PROVIDERS: &[(&str, &str)] = &[("github", "GitHub Projects"), ("gitlab", "GitLab issue board")];

pub struct RemoteColumn {
    pub id: String,
    pub name: String,
    /// Built-in column that cannot be renamed, deleted or reordered (GitLab's Open / Closed,
    /// GitHub's "No Status").
    pub fixed: bool,
}

pub struct RemoteCard {
    pub id: String,
    pub title: String,
    pub body: String,
    /// Remote column id.
    pub column: String,
}

pub struct RemoteBoard {
    /// In board order.
    pub columns: Vec<RemoteColumn>,
    /// In board order (the order of cards within a column follows from it).
    pub cards: Vec<RemoteCard>,
    /// The fixed column cards land in when their own column disappears.
    pub fallback: String,
}

/// One remote board. Implementations keep whatever they learned from the last `fetch` so that
/// later calls can translate ids.
#[async_trait]
pub trait Remote: Send {
    /// `known` lists the remote ids of cards already paired, so a provider can look up ones its
    /// normal listing no longer returns (GitLab issues that were closed).
    async fn fetch(&mut self, known: &[String]) -> Result<RemoteBoard, String>;
    async fn create_column(&mut self, name: &str) -> Result<RemoteColumn, String>;
    async fn rename_column(&mut self, id: &str, name: &str) -> Result<(), String>;
    async fn delete_column(&mut self, id: &str) -> Result<(), String>;
    /// `ids`: every non-fixed column, in the wanted order.
    async fn order_columns(&mut self, ids: &[String]) -> Result<(), String>;
    async fn create_card(&mut self, title: &str, body: &str, column: &str) -> Result<String, String>;
    async fn update_card(&mut self, id: &str, title: &str, body: &str) -> Result<(), String>;
    async fn move_card(&mut self, id: &str, column: &str) -> Result<(), String>;
    /// Position a card between two neighbours in its column (either may be absent).
    async fn place_card(&mut self, id: &str, previous: Option<&str>, next: Option<&str>) -> Result<(), String>;
    /// Returns false when the provider could only close / archive the card; it then stays ignored.
    async fn delete_card(&mut self, id: &str) -> Result<bool, String>;
    /// Old -> new column ids, if a column operation caused the provider to re-issue them.
    fn take_column_remap(&mut self) -> HashMap<String, String> {
        HashMap::new()
    }
}

#[derive(Clone)]
pub struct Link {
    pub board_id: i64,
    pub provider: String,
    pub base_url: String,
    pub token: String,
    pub target: String,
    pub user_id: Option<i64>,
    pub enabled: bool,
    pub last_sync: Option<String>,
    pub last_error: String,
}
crate::row_struct!(Link { board_id, provider, base_url, token, target, user_id, enabled, last_sync, last_error });

pub async fn link(db: &Db, board_id: i64) -> Res<Option<Link>> {
    Ok(query_as(
        "SELECT board_id, provider, base_url, token, target, user_id, enabled, last_sync, last_error FROM board_sync WHERE board_id = ?",
    )
    .bind(board_id)
    .fetch_optional(db)
    .await?)
}

/// Human description of what a link points at, for the settings page.
pub fn describe(link: &Link) -> String {
    let target: serde_json::Value = serde_json::from_str(&link.target).unwrap_or_default();
    match link.provider.as_str() {
        "github" => format!("GitHub project #{} of {}", target["number"], target["owner"].as_str().unwrap_or("?")),
        _ => format!(
            "GitLab board “{}” of {}",
            target["board_name"].as_str().unwrap_or("?"),
            target["project"].as_str().unwrap_or("?")
        ),
    }
}

fn connect(state: &AppState, link: &Link) -> Result<Box<dyn Remote>, String> {
    match link.provider.as_str() {
        "github" => Ok(Box::new(github::GitHub::new(state.http.clone(), link)?)),
        "gitlab" => Ok(Box::new(gitlab::GitLab::new(state.http.clone(), link)?)),
        other => Err(format!("unknown sync provider '{other}'")),
    }
}

/// Check the credentials and locate the remote board. Returns the `target` JSON to store.
pub async fn resolve_target(state: &AppState, provider: &str, base_url: &str, token: &str, form: &HashMap<String, String>) -> Result<String, String> {
    match provider {
        "github" => github::resolve(&state.http, base_url, token, form).await,
        "gitlab" => gitlab::resolve(&state.http, base_url, token, form).await,
        other => Err(format!("unknown sync provider '{other}'")),
    }
}

struct ColumnPair {
    remote_id: String,
    column_id: i64,
    base_name: String,
    base_pos: i64,
}

struct CardPair {
    remote_id: String,
    card_id: i64,
    base_title: String,
    base_body: String,
    base_column: String,
    base_pos: i64,
    ignored: bool,
}

async fn save_card_pair(db: &Db, board_id: i64, p: &CardPair) -> Result<(), String> {
    let e = |e: anyhow::Error| format!("database error: {e:#}");
    query("DELETE FROM sync_cards WHERE board_id = ? AND remote_id = ?").bind(board_id).bind(&p.remote_id).execute(db).await.map_err(e)?;
    query("INSERT INTO sync_cards (board_id, remote_id, card_id, base_title, base_body, base_column, base_pos, ignored) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(board_id)
        .bind(&p.remote_id)
        .bind(p.card_id)
        .bind(&p.base_title)
        .bind(&p.base_body)
        .bind(&p.base_column)
        .bind(p.base_pos)
        .bind(p.ignored)
        .execute(db)
        .await
        .map_err(e)?;
    Ok(())
}

async fn save_column_pair(db: &Db, board_id: i64, p: &ColumnPair) -> Result<(), String> {
    let e = |e: anyhow::Error| format!("database error: {e:#}");
    query("DELETE FROM sync_columns WHERE board_id = ? AND remote_id = ?").bind(board_id).bind(&p.remote_id).execute(db).await.map_err(e)?;
    query("INSERT INTO sync_columns (board_id, remote_id, column_id, base_name, base_pos) VALUES (?, ?, ?, ?, ?)")
        .bind(board_id)
        .bind(&p.remote_id)
        .bind(p.column_id)
        .bind(&p.base_name)
        .bind(p.base_pos)
        .execute(db)
        .await
        .map_err(e)?;
    Ok(())
}

/// Local card titles live in a 255-character column.
fn clip_title(title: &str) -> String {
    let t = title.trim();
    let t = if t.is_empty() { "(untitled)" } else { t };
    t.chars().take(255).collect()
}

/// The elements of `seq` that are in `keep`, in order.
fn restrict<'a>(seq: &'a [String], keep: &HashSet<&str>) -> Vec<&'a str> {
    seq.iter().map(String::as_str).filter(|s| keep.contains(s)).collect()
}

#[derive(Default)]
pub struct Summary {
    pub pulled: usize,
    pub pushed: usize,
}

impl Summary {
    pub fn describe(&self) -> String {
        match (self.pulled, self.pushed) {
            (0, 0) => "Already in sync.".to_string(),
            (a, b) => format!("Synced: {a} change(s) brought in, {b} sent out."),
        }
    }
}

/// Run one cycle for a board and record the outcome on its link.
pub async fn sync_board(state: &AppState, board_id: i64) -> Result<Summary, String> {
    let Some(link) = link(&state.db, board_id).await.map_err(|e| e.message())? else {
        return Err("This board is not linked".into());
    };
    // One cycle at a time: the background worker and a "Sync now" click must not interleave, or
    // both would import the same remote card.
    let _running = state.sync.1.lock().await;
    let result = match tokio::time::timeout(CYCLE_TIMEOUT, cycle(state, &link)).await {
        Ok(r) => r,
        Err(_) => Err("The sync took too long and was stopped".to_string()),
    };
    let error = result.as_ref().err().cloned().unwrap_or_default();
    if !error.is_empty() {
        tracing::warn!("sync of board {board_id} failed: {error}");
    }
    let _ = query("UPDATE board_sync SET last_sync = ?, last_error = ? WHERE board_id = ?")
        .bind(now())
        .bind(error.chars().take(2000).collect::<String>())
        .bind(board_id)
        .execute(&state.db)
        .await;
    result
}

async fn cycle(state: &AppState, link: &Link) -> Result<Summary, String> {
    let db = &state.db;
    let board_id = link.board_id;
    let dbe = |e: anyhow::Error| format!("database error: {e:#}");
    let ope = |e: crate::error::AppError| e.message();
    let mut summary = Summary::default();
    let mut remote = connect(state, link)?;

    let mut col_pairs: Vec<ColumnPair> = query_as::<(String, i64, String, i64)>(
        "SELECT remote_id, column_id, base_name, base_pos FROM sync_columns WHERE board_id = ? ORDER BY base_pos",
    )
    .bind(board_id)
    .fetch_all(db)
    .await
    .map_err(dbe)?
    .into_iter()
    .map(|(remote_id, column_id, base_name, base_pos)| ColumnPair { remote_id, column_id, base_name, base_pos })
    .collect();
    let mut card_pairs: Vec<CardPair> = query_as::<(String, i64, String, String, String, i64, bool)>(
        "SELECT remote_id, card_id, base_title, base_body, base_column, base_pos, ignored FROM sync_cards WHERE board_id = ? ORDER BY base_pos",
    )
    .bind(board_id)
    .fetch_all(db)
    .await
    .map_err(dbe)?
    .into_iter()
    .map(|(remote_id, card_id, base_title, base_body, base_column, base_pos, ignored)| CardPair {
        remote_id,
        card_id,
        base_title,
        base_body,
        base_column,
        base_pos,
        ignored,
    })
    .collect();
    let first_sync = col_pairs.is_empty();

    let known: Vec<String> = card_pairs.iter().filter(|p| !p.ignored).map(|p| p.remote_id.clone()).collect();
    let mut rb = remote.fetch(&known).await?;

    // ------------------------------------------------------------------ columns
    let mut local_cols = ops::columns(db, board_id).await.map_err(ope)?;
    let base_col_order: Vec<String> = col_pairs.iter().map(|p| p.remote_id.clone()).collect();
    let mut drop_local_columns: Vec<i64> = Vec::new();
    let mut columns_added = false;
    let mut kept: Vec<ColumnPair> = Vec::new();
    for mut p in std::mem::take(&mut col_pairs) {
        let local = local_cols.iter().find(|c| c.id == p.column_id).cloned();
        let remote_col = rb.columns.iter().position(|c| c.id == p.remote_id);
        match (local, remote_col) {
            (None, None) => continue,
            (Some(l), None) => {
                // Removed remotely: delete it here once its cards have been dealt with.
                drop_local_columns.push(l.id);
                summary.pulled += 1;
                continue;
            }
            (None, Some(i)) if rb.columns[i].fixed => {
                // A built-in remote column cannot be deleted, so its local twin comes back.
                p.column_id = ops::create_column(db, board_id, &rb.columns[i].name).await.map_err(ope)?;
                p.base_name = rb.columns[i].name.clone();
                summary.pulled += 1;
            }
            (None, Some(i)) => {
                // Deleted here: delete it there. Its remote cards fall into the fallback column.
                let gone = rb.columns.remove(i);
                remote.delete_column(&gone.id).await?;
                for card in rb.cards.iter_mut().filter(|c| c.column == gone.id) {
                    card.column = rb.fallback.clone();
                }
                summary.pushed += 1;
                continue;
            }
            (Some(l), Some(i)) => {
                let r = &mut rb.columns[i];
                if r.name != p.base_name {
                    if l.name != r.name {
                        query("UPDATE columns SET name = ? WHERE id = ?").bind(clip_title(&r.name)).bind(l.id).execute(db).await.map_err(dbe)?;
                        summary.pulled += 1;
                    }
                    p.base_name = r.name.clone();
                } else if l.name != p.base_name && !r.fixed {
                    remote.rename_column(&r.id, &l.name).await?;
                    r.name = l.name.clone();
                    p.base_name = l.name.clone();
                    summary.pushed += 1;
                }
            }
        }
        kept.push(p);
    }
    col_pairs = kept;

    // Remote columns not yet paired: adopt a same-named local column, or create one.
    local_cols = ops::columns(db, board_id).await.map_err(ope)?;
    for r in &rb.columns {
        if col_pairs.iter().any(|p| p.remote_id == r.id) {
            continue;
        }
        let unpaired = local_cols
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(&r.name) && !col_pairs.iter().any(|p| p.column_id == l.id) && !drop_local_columns.contains(&l.id));
        let column_id = match unpaired {
            Some(l) => l.id,
            None => {
                summary.pulled += 1;
                columns_added = true;
                ops::create_column(db, board_id, &clip_title(&r.name)).await.map_err(ope)?
            }
        };
        let pair = ColumnPair { remote_id: r.id.clone(), column_id, base_name: r.name.clone(), base_pos: 0 };
        save_column_pair(db, board_id, &pair).await?;
        col_pairs.push(pair);
    }
    // Local columns not yet paired: create them remotely.
    local_cols = ops::columns(db, board_id).await.map_err(ope)?;
    for l in &local_cols {
        if col_pairs.iter().any(|p| p.column_id == l.id) || drop_local_columns.contains(&l.id) {
            continue;
        }
        let created = remote.create_column(&l.name).await?;
        let pair = ColumnPair { remote_id: created.id.clone(), column_id: l.id, base_name: created.name.clone(), base_pos: 0 };
        save_column_pair(db, board_id, &pair).await?;
        col_pairs.push(pair);
        // New columns go after the existing ones but before any built-in trailing column (GitLab's Closed).
        let at = rb.columns.iter().rposition(|c| !c.fixed).map(|i| i + 1).unwrap_or(rb.columns.len());
        rb.columns.insert(at, created);
        columns_added = true;
        summary.pushed += 1;
    }

    // Column order.
    let remote_order: Vec<String> = rb.columns.iter().map(|c| c.id.clone()).collect();
    let local_order: Vec<String> = local_cols
        .iter()
        .filter_map(|l| col_pairs.iter().find(|p| p.column_id == l.id).map(|p| p.remote_id.clone()))
        .collect();
    let common: HashSet<&str> = base_col_order
        .iter()
        .map(String::as_str)
        .filter(|id| remote_order.iter().any(|r| r == id) && local_order.iter().any(|l| l == id))
        .collect();
    // A column created on either side this cycle takes its place from the remote board.
    let apply_remote_order = first_sync || columns_added || restrict(&remote_order, &common) != restrict(&base_col_order, &common);
    if apply_remote_order {
        let wanted: Vec<i64> = remote_order
            .iter()
            .filter_map(|id| col_pairs.iter().find(|p| &p.remote_id == id).map(|p| p.column_id))
            .collect();
        let current: Vec<i64> = local_cols.iter().map(|c| c.id).filter(|id| wanted.contains(id)).collect();
        if wanted != current {
            ops::set_column_order(db, board_id, &wanted).await.map_err(ope)?;
            summary.pulled += 1;
        }
    } else if restrict(&local_order, &common) != restrict(&base_col_order, &common) {
        let movable: Vec<String> = local_order
            .iter()
            .filter(|id| rb.columns.iter().any(|c| &c.id == *id && !c.fixed))
            .cloned()
            .collect();
        remote.order_columns(&movable).await?;
        summary.pushed += 1;
    }

    // Some providers re-issue column ids when the column list is edited. Follow the new ids, and
    // because such an edit can also drop cards out of their columns remotely, re-assert every
    // card's column from the local side this cycle.
    let remap = remote.take_column_remap();
    let force_push_columns = !remap.is_empty();
    if force_push_columns {
        let renew = |id: &mut String| {
            if let Some(new) = remap.get(id.as_str()) {
                *id = new.clone();
            }
        };
        query("DELETE FROM sync_columns WHERE board_id = ?").bind(board_id).execute(db).await.map_err(dbe)?;
        for p in col_pairs.iter_mut() {
            renew(&mut p.remote_id);
            save_column_pair(db, board_id, p).await?;
        }
        for c in rb.columns.iter_mut() {
            renew(&mut c.id);
        }
        for p in card_pairs.iter_mut() {
            renew(&mut p.base_column);
        }
        // What the remote cards' columns are now is unknown; the fallback is the safe assumption.
        for c in rb.cards.iter_mut() {
            c.column = rb.fallback.clone();
        }
    }

    let remote_of_local = |column_id: i64, pairs: &[ColumnPair]| pairs.iter().find(|p| p.column_id == column_id).map(|p| p.remote_id.clone());
    let local_of_remote = |remote_id: &str, pairs: &[ColumnPair]| pairs.iter().find(|p| p.remote_id == remote_id).map(|p| p.column_id);

    // ------------------------------------------------------------------ cards
    let data = ops::load_board(db, board_id).await.map_err(ope)?;
    let mut kept: Vec<CardPair> = Vec::new();
    for mut p in std::mem::take(&mut card_pairs) {
        let local = data.cards.iter().find(|c| c.id == p.card_id);
        let ri = rb.cards.iter().position(|c| c.id == p.remote_id);
        if p.ignored {
            // Deleted here earlier but still present there (closed): keep ignoring until it is gone.
            if let Some(i) = ri {
                rb.cards.remove(i);
                kept.push(p);
            }
            continue;
        }
        match (local, ri) {
            (None, None) => continue,
            (Some(l), None) => {
                ops::delete_card(db, l.id).await.map_err(ope)?;
                summary.pulled += 1;
                continue;
            }
            (None, Some(i)) => {
                let gone = rb.cards.remove(i);
                summary.pushed += 1;
                if !remote.delete_card(&gone.id).await? {
                    p.ignored = true;
                    save_card_pair(db, board_id, &p).await?;
                    kept.push(p);
                }
                continue;
            }
            (Some(l), Some(i)) => {
                let r = &mut rb.cards[i];
                // Title and description.
                if r.title != p.base_title || r.body != p.base_body {
                    let title = clip_title(&r.title);
                    if l.title != title || l.description != r.body {
                        query("UPDATE cards SET title = ?, description = ?, updated_at = ? WHERE id = ?")
                            .bind(&title)
                            .bind(&r.body)
                            .bind(now())
                            .bind(l.id)
                            .execute(db)
                            .await
                            .map_err(dbe)?;
                        summary.pulled += 1;
                    }
                    p.base_title = r.title.clone();
                    p.base_body = r.body.clone();
                } else if l.title != clip_title(&p.base_title) || l.description != p.base_body {
                    remote.update_card(&r.id, &l.title, &l.description).await?;
                    p.base_title = l.title.clone();
                    p.base_body = l.description.clone();
                    summary.pushed += 1;
                }
                // Column.
                if let Some(local_remote) = remote_of_local(l.column_id, &col_pairs) {
                    if force_push_columns {
                        if r.column != local_remote {
                            remote.move_card(&r.id, &local_remote).await?;
                            r.column = local_remote.clone();
                        }
                        p.base_column = local_remote;
                    } else if r.column != p.base_column {
                        if r.column != local_remote {
                            if let Some(target) = local_of_remote(&r.column, &col_pairs) {
                                ops::move_card(db, l, target, None).await.map_err(ope)?;
                                summary.pulled += 1;
                            }
                        }
                        p.base_column = r.column.clone();
                    } else if local_remote != p.base_column {
                        remote.move_card(&r.id, &local_remote).await?;
                        r.column = local_remote.clone();
                        p.base_column = local_remote;
                        summary.pushed += 1;
                    }
                }
            }
        }
        kept.push(p);
    }
    card_pairs = kept;

    // Remote cards not yet paired: bring them in.
    let creator = link.user_id.unwrap_or(0);
    for r in &rb.cards {
        if card_pairs.iter().any(|p| p.remote_id == r.id) {
            continue;
        }
        let Some(column_id) = local_of_remote(&r.column, &col_pairs) else { continue };
        let (position,): (i64,) = query_as("SELECT COALESCE(MAX(position), -1) + 1 FROM cards WHERE column_id = ?")
            .bind(column_id)
            .fetch_one(db)
            .await
            .map_err(dbe)?;
        let stamp = now();
        let card_id = query(
            "INSERT INTO cards (board_id, column_id, title, description, created_by, position, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(board_id)
        .bind(column_id)
        .bind(clip_title(&r.title))
        .bind(&r.body)
        .bind((creator > 0).then_some(creator))
        .bind(position)
        .bind(&stamp)
        .bind(&stamp)
        .insert(db)
        .await
        .map_err(dbe)?;
        let pair = CardPair {
            remote_id: r.id.clone(),
            card_id,
            base_title: r.title.clone(),
            base_body: r.body.clone(),
            base_column: r.column.clone(),
            base_pos: position,
            ignored: false,
        };
        // Saved at once: if a later step fails, the next cycle must not import this card again.
        save_card_pair(db, board_id, &pair).await?;
        card_pairs.push(pair);
        summary.pulled += 1;
    }
    // Local cards not yet paired: create them remotely. (Re-read first: cards deleted above must not be recreated.)
    let data = ops::load_board(db, board_id).await.map_err(ope)?;
    for l in &data.cards {
        if card_pairs.iter().any(|p| p.card_id == l.id) {
            continue;
        }
        let Some(column) = remote_of_local(l.column_id, &col_pairs) else { continue };
        let remote_id = remote.create_card(&l.title, &l.description, &column).await?;
        let pair = CardPair {
            remote_id: remote_id.clone(),
            card_id: l.id,
            base_title: l.title.clone(),
            base_body: l.description.clone(),
            base_column: column.clone(),
            base_pos: i64::MAX,
            ignored: false,
        };
        save_card_pair(db, board_id, &pair).await?;
        card_pairs.push(pair);
        rb.cards.push(RemoteCard { id: remote_id, title: l.title.clone(), body: l.description.clone(), column });
        summary.pushed += 1;
    }

    // Columns removed remotely: move any stragglers to the fallback column, then delete.
    if !drop_local_columns.is_empty() {
        let fallback_local = local_of_remote(&rb.fallback, &col_pairs);
        let data = ops::load_board(db, board_id).await.map_err(ope)?;
        for column_id in &drop_local_columns {
            for card in data.cards.iter().filter(|c| c.column_id == *column_id) {
                if let Some(target) = fallback_local {
                    ops::move_card(db, card, target, None).await.map_err(ope)?;
                }
            }
            ops::delete_column(db, *column_id).await.map_err(ope)?;
        }
    }

    // ------------------------------------------------------------------ order of cards in each column
    let data = ops::load_board(db, board_id).await.map_err(ope)?;
    for col in &col_pairs {
        let remote_seq: Vec<String> = rb
            .cards
            .iter()
            .filter(|c| c.column == col.remote_id && card_pairs.iter().any(|p| p.remote_id == c.id && !p.ignored))
            .map(|c| c.id.clone())
            .collect();
        let local_cards: Vec<&ops::Card> = data.cards.iter().filter(|c| c.column_id == col.column_id).collect();
        let local_seq: Vec<String> = local_cards
            .iter()
            .filter_map(|c| card_pairs.iter().find(|p| p.card_id == c.id && !p.ignored).map(|p| p.remote_id.clone()))
            .collect();
        let base_seq: Vec<String> = card_pairs
            .iter()
            .filter(|p| p.base_column == col.remote_id && p.base_pos != i64::MAX)
            .map(|p| p.remote_id.clone())
            .collect();
        let common: HashSet<&str> = base_seq
            .iter()
            .map(String::as_str)
            .filter(|id| remote_seq.iter().any(|r| r == id) && local_seq.iter().any(|l| l == id))
            .collect();
        if restrict(&remote_seq, &common) != restrict(&base_seq, &common) {
            // Reordered remotely: follow the remote order here (cards only known locally keep their place at the end).
            let mut wanted: Vec<i64> = remote_seq
                .iter()
                .filter_map(|id| card_pairs.iter().find(|p| &p.remote_id == id).map(|p| p.card_id))
                .collect();
            wanted.extend(local_cards.iter().map(|c| c.id).filter(|id| !wanted.contains(id)).collect::<Vec<_>>());
            let current: Vec<i64> = local_cards.iter().map(|c| c.id).collect();
            if wanted != current {
                for (i, id) in wanted.iter().enumerate() {
                    query("UPDATE cards SET position = ? WHERE id = ?").bind(i).bind(id).execute(db).await.map_err(dbe)?;
                }
                summary.pulled += 1;
            }
        } else if restrict(&local_seq, &common) != restrict(&base_seq, &common) {
            // Reordered here: replay the local order remotely, moving only cards that are out of place.
            let mut sim = remote_seq.clone();
            let mut moved = false;
            for (i, id) in local_seq.iter().enumerate() {
                if sim.get(i) == Some(id) {
                    continue;
                }
                sim.retain(|x| x != id);
                let at = i.min(sim.len());
                let previous = if at > 0 { sim.get(at - 1).cloned() } else { None };
                let next = sim.get(at).cloned();
                remote.place_card(id, previous.as_deref(), next.as_deref()).await?;
                sim.insert(at, id.clone());
                moved = true;
            }
            if moved {
                summary.pushed += 1;
            }
        }
    }

    // ------------------------------------------------------------------ record the new base
    let data = ops::load_board(db, board_id).await.map_err(ope)?;
    let local_cols = ops::columns(db, board_id).await.map_err(ope)?;
    query("DELETE FROM sync_columns WHERE board_id = ?").bind(board_id).execute(db).await.map_err(dbe)?;
    for p in col_pairs.iter_mut() {
        p.base_pos = local_cols.iter().position(|c| c.id == p.column_id).unwrap_or(usize::MAX >> 1) as i64;
        save_column_pair(db, board_id, p).await?;
    }
    query("DELETE FROM sync_cards WHERE board_id = ?").bind(board_id).execute(db).await.map_err(dbe)?;
    for p in card_pairs.iter_mut() {
        if let Some(pos) = data.cards.iter().position(|c| c.id == p.card_id) {
            p.base_pos = pos as i64;
        }
        if p.base_pos == i64::MAX {
            p.base_pos = data.cards.len() as i64;
        }
        save_card_pair(db, board_id, p).await?;
    }
    Ok(summary)
}

/// Handle used by request handlers to ask for a prompt sync after a local change.
#[derive(Clone)]
pub struct SyncHandle(mpsc::UnboundedSender<i64>, std::sync::Arc<tokio::sync::Mutex<()>>);

impl SyncHandle {
    pub fn poke(&self, board_id: i64) {
        let _ = self.0.send(board_id);
    }
}

pub fn channel() -> (SyncHandle, mpsc::UnboundedReceiver<i64>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (SyncHandle(tx, Default::default()), rx)
}

/// Background worker: syncs poked boards after a short debounce, and every linked board on a timer.
pub fn spawn(state: AppState, mut rx: mpsc::UnboundedReceiver<i64>) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(POLL_SECONDS));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let mut due: HashSet<i64> = HashSet::new();
            tokio::select! {
                poked = rx.recv() => {
                    let Some(id) = poked else { return };
                    due.insert(id);
                    // Let a burst of edits (a drag, an agent run) settle into one cycle.
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    while let Ok(id) = rx.try_recv() {
                        due.insert(id);
                    }
                }
                _ = timer.tick() => {
                    let linked: Vec<(i64,)> = query_as("SELECT board_id FROM board_sync WHERE enabled = 1").fetch_all(&state.db).await.unwrap_or_default();
                    due.extend(linked.into_iter().map(|r| r.0));
                }
            }
            for board_id in due {
                // Only linked, enabled boards do any work; a poke for any other board is a cheap lookup.
                match link(&state.db, board_id).await {
                    Ok(Some(l)) if l.enabled => {
                        let _ = sync_board(&state, board_id).await;
                    }
                    _ => {}
                }
            }
        }
    });
}
