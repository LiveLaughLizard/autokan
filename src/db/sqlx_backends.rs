//! SQLite, PostgreSQL and MySQL backends, all on sqlx. The three differ only in connection setup,
//! placeholder style and how a generated id is reported, so one macro writes all of them.
use super::{Cell, Conn, Database, Dialect, Row, Transaction, Tx, Value};
use async_trait::async_trait;
use sqlx::{Column, Row as _};
use tokio::sync::Mutex;

/// The three operations, written once against any sqlx executor (a pool or a transaction).
macro_rules! run {
    ($db:ident, $dialect:expr, $returning:expr, $last_id:expr; $exec:expr, execute, $sql:expr, $params:expr) => {{
        let sql = $dialect.placeholders($sql);
        Ok($db::bind(&sql, $params).execute($exec).await?.rows_affected())
    }};
    ($db:ident, $dialect:expr, $returning:expr, $last_id:expr; $exec:expr, insert, $sql:expr, $params:expr) => {{
        if $returning {
            let sql = format!("{} RETURNING id", $dialect.placeholders($sql));
            let row = $db::bind(&sql, $params).fetch_one($exec).await?;
            Ok(row.try_get::<i64, _>(0)?)
        } else {
            let sql = $dialect.placeholders($sql);
            let result = $db::bind(&sql, $params).execute($exec).await?;
            #[allow(clippy::redundant_closure_call)]
            Ok(($last_id)(&result))
        }
    }};
    ($db:ident, $dialect:expr, $returning:expr, $last_id:expr; $exec:expr, fetch, $sql:expr, $params:expr) => {{
        let sql = $dialect.placeholders($sql);
        let rows = $db::bind(&sql, $params).fetch_all($exec).await?;
        rows.iter().map($db::decode).collect()
    }};
}

macro_rules! sqlx_backend {
    ($db_name:ident, $tx_name:ident, $driver:ty, $dialect:expr, returning_id = $returning:expr, last_id = $last_id:expr) => {
        pub struct $db_name {
            pool: sqlx::Pool<$driver>,
        }

        pub struct $tx_name {
            // sqlx transactions need `&mut`; the mutex lets the shared `Conn` interface use `&self`.
            tx: Mutex<sqlx::Transaction<'static, $driver>>,
        }

        impl $db_name {
            fn bind<'q>(
                sql: &'q str,
                params: &[Value],
            ) -> sqlx::query::Query<'q, $driver, <$driver as sqlx::Database>::Arguments<'q>> {
                let mut q = sqlx::query(sql);
                for p in params {
                    q = match p {
                        Value::Int(i) => q.bind(*i),
                        Value::Text(s) => q.bind(s.clone()),
                        Value::NullInt => q.bind(None::<i64>),
                        Value::NullText => q.bind(None::<String>),
                    };
                }
                q
            }

            /// Reduce a driver row to integers and text. Each column is tried as the types the
            /// schema uses; a NULL decodes on the first attempt whatever its declared type.
            fn decode(row: &<$driver as sqlx::Database>::Row) -> anyhow::Result<Row> {
                let mut out = Vec::with_capacity(row.len());
                for i in 0..row.len() {
                    let cell = if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                        v.map(Cell::Int)
                    } else if let Ok(v) = row.try_get::<Option<i32>, _>(i) {
                        v.map(|n| Cell::Int(n as i64))
                    } else if let Ok(v) = row.try_get::<Option<i16>, _>(i) {
                        v.map(|n| Cell::Int(n as i64))
                    } else if let Ok(v) = row.try_get::<Option<bool>, _>(i) {
                        v.map(|b| Cell::Int(b as i64))
                    } else if let Ok(v) = row.try_get::<Option<String>, _>(i) {
                        v.map(Cell::Text)
                    } else if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(i) {
                        v.map(|b| Cell::Text(String::from_utf8_lossy(&b).into_owned()))
                    } else {
                        anyhow::bail!("column '{}' has a type this application does not use", row.column(i).name());
                    };
                    out.push(cell.unwrap_or(Cell::Null));
                }
                Ok(out)
            }
        }

        #[async_trait]
        impl Conn for $db_name {
            fn dialect(&self) -> Dialect {
                $dialect
            }
            async fn execute(&self, sql: &str, params: &[Value]) -> anyhow::Result<u64> {
                run!($db_name, $dialect, $returning, $last_id; &self.pool, execute, sql, params)
            }
            async fn insert(&self, sql: &str, params: &[Value]) -> anyhow::Result<i64> {
                run!($db_name, $dialect, $returning, $last_id; &self.pool, insert, sql, params)
            }
            async fn fetch(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Row>> {
                run!($db_name, $dialect, $returning, $last_id; &self.pool, fetch, sql, params)
            }
        }

        #[async_trait]
        impl Database for $db_name {
            async fn begin(&self) -> anyhow::Result<Tx> {
                Ok(Box::new($tx_name { tx: Mutex::new(self.pool.begin().await?) }))
            }
        }

        #[async_trait]
        impl Conn for $tx_name {
            fn dialect(&self) -> Dialect {
                $dialect
            }
            async fn execute(&self, sql: &str, params: &[Value]) -> anyhow::Result<u64> {
                let mut tx = self.tx.lock().await;
                run!($db_name, $dialect, $returning, $last_id; &mut **tx, execute, sql, params)
            }
            async fn insert(&self, sql: &str, params: &[Value]) -> anyhow::Result<i64> {
                let mut tx = self.tx.lock().await;
                run!($db_name, $dialect, $returning, $last_id; &mut **tx, insert, sql, params)
            }
            async fn fetch(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Row>> {
                let mut tx = self.tx.lock().await;
                run!($db_name, $dialect, $returning, $last_id; &mut **tx, fetch, sql, params)
            }
        }

        #[async_trait]
        impl Transaction for $tx_name {
            async fn commit(self: Box<Self>) -> anyhow::Result<()> {
                Ok(self.tx.into_inner().commit().await?)
            }
        }
    };
}

mod sqlite {
    use super::*;
    sqlx_backend!(SqliteDb, SqliteTx, sqlx::Sqlite, Dialect::Sqlite, returning_id = false,
        last_id = |r: &sqlx::sqlite::SqliteQueryResult| r.last_insert_rowid());

    impl SqliteDb {
        pub async fn connect(url: &str) -> anyhow::Result<Self> {
            use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
            use std::str::FromStr;
            let opts = SqliteConnectOptions::from_str(url)?
                .create_if_missing(true)
                .foreign_keys(true)
                .journal_mode(SqliteJournalMode::Wal)
                .busy_timeout(std::time::Duration::from_secs(10));
            Ok(SqliteDb { pool: SqlitePoolOptions::new().max_connections(8).connect_with(opts).await? })
        }
    }
}

mod postgres {
    use super::*;
    sqlx_backend!(PostgresDb, PostgresTx, sqlx::Postgres, Dialect::Postgres, returning_id = true,
        last_id = |_r: &sqlx::postgres::PgQueryResult| 0i64);

    impl PostgresDb {
        pub async fn connect(url: &str) -> anyhow::Result<Self> {
            Ok(PostgresDb { pool: sqlx::postgres::PgPoolOptions::new().max_connections(10).connect(url).await? })
        }
    }
}

mod mysql {
    use super::*;
    sqlx_backend!(MysqlDb, MysqlTx, sqlx::MySql, Dialect::Mysql, returning_id = false,
        last_id = |r: &sqlx::mysql::MySqlQueryResult| r.last_insert_id() as i64);

    impl MysqlDb {
        pub async fn connect(url: &str) -> anyhow::Result<Self> {
            // sqlx only knows the `mysql://` scheme; MariaDB speaks the same protocol.
            let url = url.replacen("mariadb://", "mysql://", 1);
            Ok(MysqlDb { pool: sqlx::mysql::MySqlPoolOptions::new().max_connections(10).connect(&url).await? })
        }
    }
}

pub use mysql::MysqlDb;
pub use postgres::PostgresDb;
pub use sqlite::SqliteDb;
