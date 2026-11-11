//! Microsoft SQL Server backend (tiberius). sqlx has no SQL Server driver, which is the main
//! reason database access sits behind traits rather than directly on sqlx.
use super::{Cell, Conn, Database, Dialect, Row, Transaction, Tx, Value};
use anyhow::Context;
use async_trait::async_trait;
use tiberius::{AuthMethod, ColumnData, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type Client = tiberius::Client<Compat<TcpStream>>;

const MAX_IDLE: usize = 8;

pub struct MssqlDb {
    config: Config,
    /// Idle connections. A connection that returned an error is closed instead of reused.
    idle: std::sync::Mutex<Vec<Client>>,
}

impl MssqlDb {
    /// `mssql://user:password@host:1433/database?encrypt=true&trust_cert=false`, or an ADO.NET
    /// connection string (`Server=tcp:host,1433;Database=...;User Id=...;Password=...`).
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let config = if url.contains("://") {
            let u = url::Url::parse(url).context("invalid SQL Server URL")?;
            let mut c = Config::new();
            c.host(u.host_str().context("the SQL Server URL needs a host")?);
            c.port(u.port().unwrap_or(1433));
            let database = u.path().trim_start_matches('/');
            if !database.is_empty() {
                c.database(database);
            }
            let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
            c.authentication(AuthMethod::sql_server(decode(u.username()), decode(u.password().unwrap_or(""))));
            let mut encrypt = true;
            for (k, v) in u.query_pairs() {
                let on = matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
                match k.to_ascii_lowercase().as_str() {
                    "encrypt" => encrypt = on,
                    "trust_cert" | "trustservercertificate" if on => c.trust_cert(),
                    "instance" => c.instance_name(v.as_ref()),
                    _ => {}
                }
            }
            c.encryption(if encrypt { EncryptionLevel::Required } else { EncryptionLevel::NotSupported });
            c
        } else {
            Config::from_ado_string(url).context("invalid SQL Server connection string")?
        };
        let db = MssqlDb { config, idle: Default::default() };
        let client = db.open().await?;
        db.release(client);
        Ok(db)
    }

    async fn open(&self) -> anyhow::Result<Client> {
        let tcp = TcpStream::connect(self.config.get_addr()).await.context("could not reach SQL Server")?;
        tcp.set_nodelay(true)?;
        Client::connect(self.config.clone(), tcp.compat_write())
            .await
            .context("SQL Server refused the connection")
    }

    async fn checkout(&self) -> anyhow::Result<Client> {
        let idle = self.idle.lock().unwrap().pop();
        match idle {
            Some(c) => Ok(c),
            None => self.open().await,
        }
    }

    fn release(&self, client: Client) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < MAX_IDLE {
            idle.push(client);
        }
    }

}

/// Run one operation on a pooled connection, returning the connection to the pool only on success.
macro_rules! pooled {
    ($db:expr, $op:ident, $sql:expr, $params:expr) => {{
        let mut client = $db.checkout().await?;
        let result = $op(&mut client, $sql, $params).await;
        if result.is_ok() {
            $db.release(client);
        }
        result
    }};
}

fn bind<'a>(sql: &'a str, params: &'a [Value]) -> tiberius::Query<'a> {
    let mut q = tiberius::Query::new(sql);
    for p in params {
        match p {
            Value::Int(i) => q.bind(*i),
            Value::Text(s) => q.bind(s.as_str()),
            Value::NullInt => q.bind(Option::<i64>::None),
            Value::NullText => q.bind(Option::<&str>::None),
        }
    }
    q
}

fn decode(row: tiberius::Row) -> anyhow::Result<Row> {
    row.into_iter()
        .map(|col| {
            Ok(match col {
                ColumnData::U8(v) => v.map(|n| Cell::Int(n as i64)),
                ColumnData::I16(v) => v.map(|n| Cell::Int(n as i64)),
                ColumnData::I32(v) => v.map(|n| Cell::Int(n as i64)),
                ColumnData::I64(v) => v.map(Cell::Int),
                ColumnData::Bit(v) => v.map(|b| Cell::Int(b as i64)),
                ColumnData::String(v) => v.map(|s| Cell::Text(s.into_owned())),
                ColumnData::Numeric(v) => v.map(|n| Cell::Int(n.int_part() as i64)),
                other => anyhow::bail!("SQL Server returned a column type this application does not use: {other:?}"),
            }
            .unwrap_or(Cell::Null))
        })
        .collect()
}

async fn execute(client: &mut Client, sql: &str, params: &[Value]) -> anyhow::Result<u64> {
    let sql = Dialect::Mssql.placeholders(sql);
    Ok(bind(&sql, params).execute(client).await?.total())
}

async fn insert(client: &mut Client, sql: &str, params: &[Value]) -> anyhow::Result<i64> {
    // SCOPE_IDENTITY() is the id generated by this statement batch on this connection.
    let sql = format!("{}; SELECT CAST(SCOPE_IDENTITY() AS BIGINT);", Dialect::Mssql.placeholders(sql));
    let rows = bind(&sql, params).query(client).await?.into_first_result().await?;
    let row = rows.into_iter().next().context("SQL Server did not report the new row's id")?;
    row.try_get::<i64, _>(0)?.context("the INSERT did not generate an id")
}

async fn fetch(client: &mut Client, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Row>> {
    let sql = Dialect::Mssql.placeholders(sql);
    let rows = bind(&sql, params).query(client).await?.into_first_result().await?;
    rows.into_iter().map(decode).collect()
}

#[async_trait]
impl Conn for MssqlDb {
    fn dialect(&self) -> Dialect {
        Dialect::Mssql
    }
    async fn execute(&self, sql: &str, params: &[Value]) -> anyhow::Result<u64> {
        pooled!(self, execute, sql, params)
    }
    async fn insert(&self, sql: &str, params: &[Value]) -> anyhow::Result<i64> {
        pooled!(self, insert, sql, params)
    }
    async fn fetch(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Row>> {
        pooled!(self, fetch, sql, params)
    }
}

#[async_trait]
impl Database for MssqlDb {
    async fn begin(&self) -> anyhow::Result<Tx> {
        // A transaction owns a dedicated connection. If the transaction is dropped without a
        // commit, the connection is closed and the server rolls the work back.
        let mut client = self.open().await?;
        client.simple_query("BEGIN TRANSACTION").await?.into_results().await?;
        Ok(Box::new(MssqlTx { client: Mutex::new(client) }))
    }
}

pub struct MssqlTx {
    client: Mutex<Client>,
}

#[async_trait]
impl Conn for MssqlTx {
    fn dialect(&self) -> Dialect {
        Dialect::Mssql
    }
    async fn execute(&self, sql: &str, params: &[Value]) -> anyhow::Result<u64> {
        execute(&mut *self.client.lock().await, sql, params).await
    }
    async fn insert(&self, sql: &str, params: &[Value]) -> anyhow::Result<i64> {
        insert(&mut *self.client.lock().await, sql, params).await
    }
    async fn fetch(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<Row>> {
        fetch(&mut *self.client.lock().await, sql, params).await
    }
}

#[async_trait]
impl Transaction for MssqlTx {
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        let mut client = self.client.into_inner();
        client.simple_query("COMMIT TRANSACTION").await?.into_results().await?;
        Ok(())
    }
}
