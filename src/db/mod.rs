//! Database access behind traits, so the same application code runs on SQLite (the default),
//! PostgreSQL, MySQL / MariaDB and Microsoft SQL Server.
//!
//! * [`Conn`] is anything that can run a statement: a database handle or an open transaction.
//! * [`Database`] adds `begin()`; [`Transaction`] adds `commit()` (dropping one rolls it back).
//! * Application code writes SQL once, in a deliberately small common subset: `?` placeholders,
//!   no vendor functions, no upserts, no `RETURNING`, integers for booleans, and UTC timestamps
//!   passed in as text. Each backend only has to translate placeholders and report generated ids.
//!
//! Adding a backend means implementing the three traits and a [`Dialect`] for its DDL (see `schema`).
mod mssql;
mod schema;
mod sqlx_backends;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::sync::Arc;

pub use schema::column_exists;

pub type Db = Arc<dyn Database>;
pub type Tx = Box<dyn Transaction>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
    Mysql,
    Mssql,
}

impl Dialect {
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Sqlite => "SQLite",
            Dialect::Postgres => "PostgreSQL",
            Dialect::Mysql => "MySQL",
            Dialect::Mssql => "SQL Server",
        }
    }

    /// Rewrite `?` placeholders into the backend's style. Question marks inside quoted strings
    /// are left alone.
    pub fn placeholders(self, sql: &str) -> String {
        let prefix = match self {
            Dialect::Sqlite | Dialect::Mysql => return sql.to_string(),
            Dialect::Postgres => "$",
            Dialect::Mssql => "@P",
        };
        let mut out = String::with_capacity(sql.len() + 16);
        let mut n = 0;
        let mut quoted = false;
        for c in sql.chars() {
            match c {
                '\'' => {
                    quoted = !quoted;
                    out.push(c);
                }
                '?' if !quoted => {
                    n += 1;
                    out.push_str(prefix);
                    out.push_str(&n.to_string());
                }
                _ => out.push(c),
            }
        }
        out
    }
}

/// A bound parameter. NULLs keep their type because PostgreSQL and SQL Server want to know it.
#[derive(Clone, Debug)]
pub enum Value {
    Int(i64),
    Text(String),
    NullInt,
    NullText,
}

/// Rust values that can be bound to a statement.
pub trait Bind {
    fn value(self) -> Value;
    fn null() -> Value;
}

macro_rules! bind_int {
    ($($t:ty),*) => {$(
        impl Bind for $t {
            fn value(self) -> Value { Value::Int(self as i64) }
            fn null() -> Value { Value::NullInt }
        }
        impl Bind for &$t {
            fn value(self) -> Value { Value::Int(*self as i64) }
            fn null() -> Value { Value::NullInt }
        }
    )*};
}
bind_int!(i64, i32, usize, bool);

impl Bind for &str {
    fn value(self) -> Value {
        Value::Text(self.to_string())
    }
    fn null() -> Value {
        Value::NullText
    }
}
impl Bind for String {
    fn value(self) -> Value {
        Value::Text(self)
    }
    fn null() -> Value {
        Value::NullText
    }
}
impl Bind for &String {
    fn value(self) -> Value {
        Value::Text(self.clone())
    }
    fn null() -> Value {
        Value::NullText
    }
}
impl<T: Bind> Bind for Option<T> {
    fn value(self) -> Value {
        match self {
            Some(v) => v.value(),
            None => T::null(),
        }
    }
    fn null() -> Value {
        T::null()
    }
}

/// One column of one result row, reduced to what the application stores: integers and text.
#[derive(Clone, Debug)]
pub enum Cell {
    Null,
    Int(i64),
    Text(String),
}

pub type Row = Vec<Cell>;

pub trait FromCell: Sized {
    fn from_cell(cell: Cell) -> anyhow::Result<Self>;
}
impl FromCell for i64 {
    fn from_cell(cell: Cell) -> anyhow::Result<Self> {
        match cell {
            Cell::Int(i) => Ok(i),
            other => Err(anyhow!("expected an integer, found {other:?}")),
        }
    }
}
impl FromCell for bool {
    fn from_cell(cell: Cell) -> anyhow::Result<Self> {
        i64::from_cell(cell).map(|i| i != 0)
    }
}
impl FromCell for String {
    fn from_cell(cell: Cell) -> anyhow::Result<Self> {
        match cell {
            Cell::Text(s) => Ok(s),
            other => Err(anyhow!("expected text, found {other:?}")),
        }
    }
}
impl<T: FromCell> FromCell for Option<T> {
    fn from_cell(cell: Cell) -> anyhow::Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            other => T::from_cell(other).map(Some),
        }
    }
}

/// Builds a value from a result row, reading columns in SELECT order.
pub trait FromRow: Sized {
    fn from_row(row: Row) -> anyhow::Result<Self>;
}

/// Pulls typed columns off the front of a row.
pub struct Cells(std::vec::IntoIter<Cell>, usize);

impl Cells {
    pub fn new(row: Row) -> Self {
        Cells(row.into_iter(), 0)
    }
    pub fn next<T: FromCell>(&mut self) -> anyhow::Result<T> {
        self.1 += 1;
        let cell = self.0.next().ok_or_else(|| anyhow!("row has no column {}", self.1))?;
        T::from_cell(cell).with_context(|| format!("column {}", self.1))
    }
}

macro_rules! tuple_from_row {
    ($($t:ident),+) => {
        impl<$($t: FromCell),+> FromRow for ($($t,)+) {
            fn from_row(row: Row) -> anyhow::Result<Self> {
                let mut cells = Cells::new(row);
                Ok(($(cells.next::<$t>()?,)+))
            }
        }
    };
}
tuple_from_row!(A);
tuple_from_row!(A, B);
tuple_from_row!(A, B, C);
tuple_from_row!(A, B, C, D);
tuple_from_row!(A, B, C, D, E);
tuple_from_row!(A, B, C, D, E, F);
tuple_from_row!(A, B, C, D, E, F, G);
tuple_from_row!(A, B, C, D, E, F, G, H);

/// Implements [`FromRow`] for a struct whose fields are listed in SELECT order.
#[macro_export]
macro_rules! row_struct {
    ($name:ident { $($field:ident),+ $(,)? }) => {
        impl $crate::db::FromRow for $name {
            fn from_row(row: $crate::db::Row) -> anyhow::Result<Self> {
                let mut cells = $crate::db::Cells::new(row);
                Ok($name { $($field: cells.next()?),+ })
            }
        }
    };
}

/// Anything that can run SQL: a database handle or an open transaction.
#[async_trait]
pub trait Conn: Send + Sync {
    fn dialect(&self) -> Dialect;
    /// Run a statement; returns the number of rows affected.
    async fn execute(&self, sql: &str, params: &[Value]) -> anyhow::Result<u64>;
    /// Run an INSERT into a table with a generated `id` column; returns the new id.
    async fn insert(&self, sql: &str, params: &[Value]) -> anyhow::Result<i64>;
    /// Run a query and return every row.
    async fn fetch(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Row>>;
}

#[async_trait]
pub trait Database: Conn {
    async fn begin(&self) -> anyhow::Result<Tx>;
}

/// An open transaction. Dropping it without calling `commit` rolls it back.
#[async_trait]
pub trait Transaction: Conn {
    async fn commit(self: Box<Self>) -> anyhow::Result<()>;
}

macro_rules! forward_conn {
    ($($ptr:ident),*) => {$(
        #[async_trait]
        impl<T: Conn + ?Sized> Conn for $ptr<T> {
            fn dialect(&self) -> Dialect {
                (**self).dialect()
            }
            async fn execute(&self, sql: &str, params: &[Value]) -> anyhow::Result<u64> {
                (**self).execute(sql, params).await
            }
            async fn insert(&self, sql: &str, params: &[Value]) -> anyhow::Result<i64> {
                (**self).insert(sql, params).await
            }
            async fn fetch(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Row>> {
                (**self).fetch(sql, params).await
            }
        }
    )*};
}
forward_conn!(Arc, Box);

/// A statement with bound parameters: `query("UPDATE t SET a = ? WHERE id = ?").bind(a).bind(id).execute(db)`.
pub struct Query {
    sql: String,
    params: Vec<Value>,
}

pub fn query(sql: &str) -> Query {
    Query { sql: sql.to_string(), params: Vec::new() }
}

impl Query {
    pub fn bind(mut self, v: impl Bind) -> Self {
        self.params.push(v.value());
        self
    }
    pub async fn execute<C: Conn + ?Sized>(self, conn: &C) -> anyhow::Result<u64> {
        conn.execute(&self.sql, &self.params).await.with_context(|| format!("statement failed: {}", self.sql))
    }
    /// For INSERTs into tables with a generated `id`.
    pub async fn insert<C: Conn + ?Sized>(self, conn: &C) -> anyhow::Result<i64> {
        conn.insert(&self.sql, &self.params).await.with_context(|| format!("insert failed: {}", self.sql))
    }
}

/// A query decoded into `T` (a tuple, or a struct declared with `row_struct!`).
pub struct QueryAs<T> {
    inner: Query,
    row: PhantomData<fn() -> T>,
}

pub fn query_as<T: FromRow>(sql: &str) -> QueryAs<T> {
    QueryAs { inner: query(sql), row: PhantomData }
}

impl<T: FromRow> QueryAs<T> {
    pub fn bind(mut self, v: impl Bind) -> Self {
        self.inner = self.inner.bind(v);
        self
    }
    pub async fn fetch_all<C: Conn + ?Sized>(self, conn: &C) -> anyhow::Result<Vec<T>> {
        let sql = self.inner.sql;
        let rows = conn.fetch(&sql, &self.inner.params).await.with_context(|| format!("query failed: {sql}"))?;
        rows.into_iter()
            .map(T::from_row)
            .collect::<anyhow::Result<Vec<T>>>()
            .with_context(|| format!("could not read the result of: {sql}"))
    }
    pub async fn fetch_optional<C: Conn + ?Sized>(self, conn: &C) -> anyhow::Result<Option<T>> {
        Ok(self.fetch_all(conn).await?.into_iter().next())
    }
    pub async fn fetch_one<C: Conn + ?Sized>(self, conn: &C) -> anyhow::Result<T> {
        self.fetch_optional(conn).await?.ok_or_else(|| anyhow!("query returned no rows"))
    }
}

/// Current UTC time as `YYYY-MM-DD HH:MM:SS`, the format every timestamp column stores.
pub fn now() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

/// Seconds since the Unix epoch.
pub fn epoch() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Open the database named by `url` and bring its schema up to date.
///
/// * `sqlite://path/to/file.db` (the default, created if missing)
/// * `postgres://user:password@host/database`
/// * `mysql://user:password@host/database` (also MariaDB)
/// * `mssql://user:password@host:1433/database?encrypt=true&trust_cert=false`
pub async fn connect(url: &str) -> anyhow::Result<Db> {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    let db: Db = match scheme.as_str() {
        "sqlite" => Arc::new(sqlx_backends::SqliteDb::connect(url).await?),
        "postgres" | "postgresql" => Arc::new(sqlx_backends::PostgresDb::connect(url).await?),
        "mysql" | "mariadb" => Arc::new(sqlx_backends::MysqlDb::connect(url).await?),
        "mssql" | "sqlserver" => Arc::new(mssql::MssqlDb::connect(url).await?),
        other => anyhow::bail!("unsupported database URL scheme '{other}' (use sqlite, postgres, mysql or mssql)"),
    };
    schema::migrate(&db).await.context("could not prepare the database schema")?;
    query("DELETE FROM sessions WHERE expires_at < ?").bind(epoch()).execute(&db).await?;
    query("DELETE FROM oidc_states WHERE expires_at < ?").bind(epoch()).execute(&db).await?;
    Ok(db)
}

pub async fn get_setting<T: DeserializeOwned + Default>(db: &Db, key: &str) -> T {
    let row: Option<(String,)> = query_as("SELECT svalue FROM settings WHERE skey = ?")
        .bind(key)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    row.and_then(|(v,)| serde_json::from_str(&v).ok()).unwrap_or_default()
}

pub async fn set_setting<T: Serialize>(db: &Db, key: &str, value: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string(value)?;
    // Look first rather than trusting the affected-row count, which some servers report as 0
    // when the new value equals the old one.
    let exists: Option<(String,)> = query_as("SELECT skey FROM settings WHERE skey = ?").bind(key).fetch_optional(db).await?;
    if exists.is_some() {
        query("UPDATE settings SET svalue = ? WHERE skey = ?").bind(&json).bind(key).execute(db).await?;
    } else {
        query("INSERT INTO settings (skey, svalue) VALUES (?, ?)").bind(key).bind(&json).execute(db).await?;
    }
    Ok(())
}
