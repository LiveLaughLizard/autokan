//! The schema, declared once and rendered as DDL for each dialect.
//!
//! Column types are deliberately few: generated ids, 64-bit integers (also used for booleans and
//! epoch seconds), short strings that may be indexed, and long text. Timestamps are UTC text
//! (`YYYY-MM-DD HH:MM:SS`) written by the application, so no backend-specific date handling is needed.
use super::{query, query_as, Conn as _, Db, Dialect};

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    /// Generated 64-bit primary key named `id`.
    Id,
    Int,
    /// String of at most 255 characters; may be part of a key or index.
    Key,
    /// Unbounded text.
    Text,
}

#[derive(Clone, Copy, PartialEq)]
enum OnDelete {
    Cascade,
    SetNull,
}

struct Col {
    name: &'static str,
    ty: Ty,
    nullable: bool,
    default: Option<&'static str>,
    references: Option<(&'static str, OnDelete)>,
}

impl Col {
    fn null(mut self) -> Self {
        self.nullable = true;
        self
    }
    fn default(mut self, value: &'static str) -> Self {
        self.default = Some(value);
        self
    }
    /// Foreign key to `table(id)`, removed with its parent.
    fn owned_by(mut self, table: &'static str) -> Self {
        self.references = Some((table, OnDelete::Cascade));
        self
    }
    /// Optional foreign key to `table(id)`, cleared when the parent goes away.
    fn refers_to(mut self, table: &'static str) -> Self {
        self.references = Some((table, OnDelete::SetNull));
        self.nullable = true;
        self
    }
}

fn col(name: &'static str, ty: Ty) -> Col {
    Col { name, ty, nullable: false, default: None, references: None }
}
fn id() -> Col {
    col("id", Ty::Id)
}
fn int(name: &'static str) -> Col {
    col(name, Ty::Int)
}
fn key(name: &'static str) -> Col {
    col(name, Ty::Key)
}
fn text(name: &'static str) -> Col {
    col(name, Ty::Text)
}
/// `created_at`-style column. Always written by the application; the default only exists so the
/// column can be added to a table that already has rows.
fn stamp(name: &'static str) -> Col {
    key(name).default("")
}

struct Table {
    name: &'static str,
    cols: Vec<Col>,
    /// Primary key columns, for tables without a generated id.
    primary_key: &'static [&'static str],
    unique: &'static [&'static [&'static str]],
}

fn table(name: &'static str, cols: Vec<Col>) -> Table {
    Table { name, cols, primary_key: &[], unique: &[] }
}

/// Tables in dependency order (a table appears after everything it references).
fn tables() -> Vec<Table> {
    vec![
        Table {
            unique: &[&["subject"]],
            ..table(
                "users",
                vec![
                    id(),
                    key("kind"),               // 'breakglass' | 'oidc' | 'ldap' | 'saml'
                    key("subject").null(),     // stable identity from the provider
                    key("username"),
                    key("display_name"),
                    key("email").null(),
                    key("password_hash").null(),
                    int("disabled").default("0"),
                    stamp("created_at"),
                    key("last_login").null(),
                ],
            )
        },
        Table {
            primary_key: &["user_id", "group_name"],
            ..table("user_groups", vec![int("user_id").owned_by("users"), key("group_name")])
        },
        Table {
            primary_key: &["token_hash"],
            ..table("sessions", vec![key("token_hash"), int("user_id").owned_by("users"), int("expires_at")])
        },
        // Personal API keys (see `auth::ApiUser`). Only a hash of the key is stored.
        Table {
            unique: &[&["token_hash"]],
            ..table(
                "api_keys",
                vec![
                    id(),
                    int("user_id").owned_by("users"),
                    key("name"),
                    key("token_hash"),
                    key("prefix"), // the first characters of the key, so its owner can tell keys apart
                    stamp("created_at"),
                    key("last_used").null(),
                    int("expires_at").null(), // epoch seconds; NULL never expires
                ],
            )
        },
        Table {
            primary_key: &["state"],
            ..table("oidc_states", vec![key("state"), key("nonce"), key("verifier"), int("expires_at")])
        },
        Table { primary_key: &["skey"], ..table("settings", vec![key("skey"), text("svalue")]) },
        Table {
            unique: &[&["name"]],
            ..table("roles", vec![id(), key("name"), text("permissions").default("[]")])
        },
        Table {
            unique: &[&["group_name", "role_id"]],
            ..table("group_mappings", vec![id(), key("group_name"), int("role_id").owned_by("roles")])
        },
        table("teams", vec![id(), key("name"), stamp("created_at")]),
        Table {
            primary_key: &["team_id", "user_id"],
            ..table(
                "team_members",
                vec![int("team_id").owned_by("teams"), int("user_id").owned_by("users"), key("role").default("member")],
            )
        },
        Table {
            unique: &[&["name"]],
            ..table(
                "llm_services",
                vec![
                    id(),
                    key("name"),
                    key("kind"),
                    text("base_url").default(""),
                    text("api_key").default(""),
                    text("models").default("[]"),
                    int("enabled").default("1"),
                ],
            )
        },
        table(
            "projects",
            vec![
                id(),
                int("team_id").owned_by("teams"),
                key("name"),
                text("description").default(""),
                stamp("created_at"),
                int("agent_service_id").refers_to("llm_services"),
                key("agent_model").default(""),
            ],
        ),
        Table {
            primary_key: &["project_id", "user_id"],
            ..table(
                "project_members",
                vec![
                    int("project_id").owned_by("projects"),
                    int("user_id").owned_by("users"),
                    int("can_manage").default("0"),
                    int("can_create_cards").default("1"),
                    int("can_comment").default("1"),
                    int("can_edit_own").default("1"),
                    int("can_edit_all").default("0"),
                ],
            )
        },
        table(
            "boards",
            vec![
                id(),
                int("project_id").owned_by("projects"),
                key("name"),
                text("agent_system_prompt").default(""),
                stamp("created_at"),
            ],
        ),
        table(
            "columns",
            vec![
                id(),
                int("board_id").owned_by("boards"),
                key("name"),
                int("position").default("0"),
                text("agent_prompt").default(""),
                int("agent_service_id").refers_to("llm_services"),
                key("agent_model").default(""),
            ],
        ),
        table(
            "cards",
            vec![
                id(),
                int("board_id").owned_by("boards"),
                int("column_id").owned_by("columns"),
                key("title"),
                text("description").default(""),
                int("position").default("0"),
                int("created_by").refers_to("users"),
                stamp("created_at"),
                stamp("updated_at"),
            ],
        ),
        table("labels", vec![id(), int("board_id").owned_by("boards"), key("name"), key("color").default("gray")]),
        Table {
            primary_key: &["card_id", "label_id"],
            ..table("card_labels", vec![int("card_id").owned_by("cards"), int("label_id").owned_by("labels")])
        },
        Table {
            primary_key: &["card_id", "user_id"],
            ..table("card_assignees", vec![int("card_id").owned_by("cards"), int("user_id").owned_by("users")])
        },
        table(
            "comments",
            vec![
                id(),
                int("card_id").owned_by("cards"),
                int("user_id").refers_to("users"),
                text("body"),
                stamp("created_at"),
                key("updated_at").null(),
            ],
        ),
        Table {
            primary_key: &["id"],
            ..table(
                "uploads",
                vec![
                    key("id"), // '<uuid>.<ext>', also the file name on disk
                    int("board_id").owned_by("boards"),
                    int("user_id").refers_to("users"),
                    text("filename"),
                    int("size"),
                    stamp("created_at"),
                ],
            )
        },
        table(
            "agent_messages",
            vec![
                id(),
                int("board_id").owned_by("boards"),
                int("user_id").owned_by("users"),
                text("message"), // provider-neutral chat message JSON
                stamp("created_at"),
            ],
        ),
        // ---- GitHub / GitLab board synchronization (see `sync`) ----
        Table {
            primary_key: &["board_id"],
            ..table(
                "board_sync",
                vec![
                    int("board_id").owned_by("boards"),
                    key("provider"), // 'github' | 'gitlab'
                    text("base_url").default(""),
                    text("token").default(""),
                    text("target").default("{}"), // provider-specific JSON: which remote board
                    int("user_id").refers_to("users"), // recorded as the creator of imported cards
                    int("enabled").default("1"),
                    key("last_sync").null(),
                    text("last_error").default(""),
                ],
            )
        },
        // Pairings of local and remote things, plus the state both sides agreed on at the last
        // sync ("base"), which is what makes a three-way comparison possible. `column_id` and
        // `card_id` are deliberately not foreign keys: a row must outlive its local half so the
        // deletion can be carried over to the remote side.
        Table {
            primary_key: &["board_id", "remote_id"],
            ..table(
                "sync_columns",
                vec![int("board_id").owned_by("boards"), key("remote_id"), int("column_id"), key("base_name"), int("base_pos").default("0")],
            )
        },
        Table {
            primary_key: &["board_id", "remote_id"],
            ..table(
                "sync_cards",
                vec![
                    int("board_id").owned_by("boards"),
                    key("remote_id"),
                    int("card_id"),
                    text("base_title").default(""),
                    text("base_body").default(""),
                    key("base_column").default(""),
                    int("base_pos").default("0"),
                    int("ignored").default("0"), // deleted here but only closeable there: never re-import
                ],
            )
        },
        // ---- Jira (see `jira`). A project's connection, and per-card links to issues. ----
        Table {
            primary_key: &["project_id"],
            ..table(
                "jira_settings",
                vec![
                    int("project_id").owned_by("projects"),
                    text("base_url").default(""),
                    key("auth").default("basic"), // 'basic' (user + token/password) | 'bearer' (personal access token)
                    key("username").default(""),
                    text("token").default(""),
                    key("project_key").default(""),
                    key("issue_type").default("Task"),
                    text("ca_cert").default(""),
                    int("enabled").default("0"),
                    key("source").default("own"), // 'own' connection, or the organization's ('platform')
                ],
            )
        },
        // What was last sent is kept so unchanged cards cost no API calls. These tables refer to
        // cards, comments and uploads by plain id (no foreign keys); `ops::delete_*` tidies them.
        Table {
            primary_key: &["card_id"],
            ..table(
                "jira_links",
                vec![
                    int("card_id"),
                    key("issue_key"),
                    text("pushed_summary").default(""),
                    text("pushed_description").default(""),
                    text("last_error").default(""),
                    key("synced_at").null(),
                ],
            )
        },
        Table {
            primary_key: &["comment_id"],
            ..table("jira_comments", vec![int("comment_id"), int("card_id"), key("jira_comment_id"), text("pushed_body").default("")])
        },
        Table {
            primary_key: &["card_id", "upload_id"],
            ..table("jira_files", vec![int("card_id"), key("upload_id"), key("filename")])
        },
    ]
}

struct Index {
    name: &'static str,
    table: &'static str,
    cols: &'static [&'static str],
    /// Unique, with the last column compared case-insensitively (it is always `name`).
    unique_name: bool,
}

const INDEXES: &[Index] = &[
    Index { name: "cards_board", table: "cards", cols: &["board_id"], unique_name: false },
    Index { name: "comments_card", table: "comments", cols: &["card_id"], unique_name: false },
    Index { name: "agent_messages_board", table: "agent_messages", cols: &["board_id", "user_id"], unique_name: false },
    // Backstops for `ops::unique_name`.
    Index { name: "teams_name_unique", table: "teams", cols: &["name"], unique_name: true },
    Index { name: "projects_name_unique", table: "projects", cols: &["team_id", "name"], unique_name: true },
    Index { name: "boards_name_unique", table: "boards", cols: &["project_id", "name"], unique_name: true },
];

fn column_ddl(d: Dialect, c: &Col) -> String {
    let ty = match (c.ty, d) {
        (Ty::Id, Dialect::Sqlite) => return format!("{} INTEGER PRIMARY KEY", c.name),
        (Ty::Id, Dialect::Postgres) => return format!("{} BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY", c.name),
        (Ty::Id, Dialect::Mysql) => return format!("{} BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY", c.name),
        (Ty::Id, Dialect::Mssql) => return format!("{} BIGINT IDENTITY(1,1) PRIMARY KEY", c.name),
        (Ty::Int, Dialect::Sqlite) => "INTEGER",
        (Ty::Int, _) => "BIGINT",
        (Ty::Key, Dialect::Sqlite | Dialect::Postgres) => "TEXT",
        (Ty::Key, Dialect::Mysql) => "VARCHAR(255)",
        (Ty::Key, Dialect::Mssql) => "NVARCHAR(255)",
        (Ty::Text, Dialect::Sqlite | Dialect::Postgres) => "TEXT",
        (Ty::Text, Dialect::Mysql) => "LONGTEXT",
        (Ty::Text, Dialect::Mssql) => "NVARCHAR(MAX)",
    };
    let mut out = format!("{} {ty}", c.name);
    out.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if let Some(v) = c.default {
        let literal = if c.ty == Ty::Int { v.to_string() } else { format!("'{v}'") };
        // MySQL only accepts a default on a TEXT column when it is written as an expression.
        if d == Dialect::Mysql && c.ty == Ty::Text {
            out.push_str(&format!(" DEFAULT ({literal})"));
        } else {
            out.push_str(&format!(" DEFAULT {literal}"));
        }
    }
    out
}

fn table_ddl(d: Dialect, t: &Table) -> String {
    let mut parts: Vec<String> = t.cols.iter().map(|c| column_ddl(d, c)).collect();
    if !t.primary_key.is_empty() {
        parts.push(format!("PRIMARY KEY ({})", t.primary_key.join(", ")));
    }
    for u in t.unique {
        parts.push(format!("UNIQUE ({})", u.join(", ")));
    }
    for c in &t.cols {
        if let Some((parent, action)) = c.references {
            // SQL Server rejects cascades that can reach a table by more than one path (cards are
            // reachable from boards directly and through columns), so there the keys only check
            // integrity. The application deletes children explicitly on every backend (`ops::delete_*`).
            let action = match (d, action) {
                (Dialect::Mssql, _) => "",
                (_, OnDelete::Cascade) => " ON DELETE CASCADE",
                (_, OnDelete::SetNull) => " ON DELETE SET NULL",
            };
            parts.push(format!("FOREIGN KEY ({}) REFERENCES {parent}(id){action}", c.name));
        }
    }
    let body = parts.join(",\n  ");
    match d {
        Dialect::Mssql => format!("IF OBJECT_ID(N'{0}', N'U') IS NULL CREATE TABLE {0} (\n  {body}\n)", t.name),
        Dialect::Mysql => format!("CREATE TABLE IF NOT EXISTS {} (\n  {body}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4", t.name),
        _ => format!("CREATE TABLE IF NOT EXISTS {} (\n  {body}\n)", t.name),
    }
}

/// Lower-cased column names of a table that exists; empty if it does not.
async fn table_columns(db: &Db, table: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> = match db.dialect() {
        // PRAGMA functions do not take the table name as a bound parameter on every SQLite build;
        // `table` only ever comes from this file.
        Dialect::Sqlite => query_as(&format!("SELECT name FROM pragma_table_info('{table}')")).fetch_all(db).await?,
        Dialect::Postgres => {
            query_as("SELECT CAST(column_name AS TEXT) FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = ?")
                .bind(table)
                .fetch_all(db)
                .await?
        }
        Dialect::Mysql => {
            query_as("SELECT column_name FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = ?")
                .bind(table)
                .fetch_all(db)
                .await?
        }
        Dialect::Mssql => {
            query_as("SELECT CAST(column_name AS NVARCHAR(255)) FROM information_schema.columns WHERE table_name = ?")
                .bind(table)
                .fetch_all(db)
                .await?
        }
    };
    Ok(rows.into_iter().map(|r| r.0.to_lowercase()).collect())
}

pub async fn column_exists(db: &Db, table: &str, column: &str) -> anyhow::Result<bool> {
    Ok(table_columns(db, table).await?.iter().any(|c| c == column))
}

async fn index_exists(db: &Db, name: &str) -> anyhow::Result<bool> {
    let sql = match db.dialect() {
        Dialect::Sqlite => "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
        Dialect::Postgres => "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = current_schema() AND indexname = ?",
        Dialect::Mysql => "SELECT COUNT(*) FROM information_schema.statistics WHERE table_schema = DATABASE() AND index_name = ?",
        Dialect::Mssql => "SELECT COUNT(*) FROM sys.indexes WHERE name = ?",
    };
    let (n,): (i64,) = query_as(sql).bind(name).fetch_one(db).await?;
    Ok(n > 0)
}

fn index_ddl(d: Dialect, ix: &Index) -> String {
    let mut cols: Vec<String> = ix.cols.iter().map(|c| c.to_string()).collect();
    if ix.unique_name {
        let last = cols.last_mut().expect("an index has columns");
        *last = match d {
            Dialect::Sqlite => format!("{last} COLLATE NOCASE"),
            Dialect::Postgres => format!("lower({last})"),
            // MySQL and SQL Server compare text case-insensitively under their default collations.
            Dialect::Mysql | Dialect::Mssql => last.clone(),
        };
    }
    format!(
        "CREATE {}INDEX {} ON {} ({})",
        if ix.unique_name { "UNIQUE " } else { "" },
        ix.name,
        ix.table,
        cols.join(", ")
    )
}

/// Create anything that is missing: tables, then columns added in later releases, then indexes.
/// Safe to run on every start.
pub async fn migrate(db: &Db) -> anyhow::Result<()> {
    let d = db.dialect();

    // Databases created by the first SQLite-only release named the settings columns `key` and
    // `value`, which are reserved words elsewhere.
    if d == Dialect::Sqlite && column_exists(db, "settings", "key").await? {
        query("ALTER TABLE settings RENAME COLUMN \"key\" TO skey").execute(db).await?;
        query("ALTER TABLE settings RENAME COLUMN \"value\" TO svalue").execute(db).await?;
    }

    for t in tables() {
        let existing = table_columns(db, t.name).await?;
        if existing.is_empty() {
            query(&table_ddl(d, &t)).execute(db).await?;
            continue;
        }
        for c in t.cols.iter().filter(|c| !existing.iter().any(|e| e == c.name)) {
            // Every column added after a table's first release is nullable or has a default.
            let add = if d == Dialect::Mssql { "ADD" } else { "ADD COLUMN" };
            query(&format!("ALTER TABLE {} {add} {}", t.name, column_ddl(d, c))).execute(db).await?;
            tracing::info!("added column {}.{}", t.name, c.name);
        }
    }

    for ix in INDEXES {
        if index_exists(db, ix.name).await? {
            continue;
        }
        if let Err(e) = query(&index_ddl(d, ix)).execute(db).await {
            if ix.unique_name {
                // A database from before the uniqueness rule may already hold duplicate names.
                tracing::warn!("could not create the unique index {} (rename duplicate names to enable it): {e:#}", ix.name);
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

