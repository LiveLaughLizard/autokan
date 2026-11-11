mod agent;
mod auth;
mod db;
mod error;
mod handlers;
mod ldap;
mod llm;
mod markdown;
mod oidc;
mod ops;
mod perm;
mod saml;
mod sync;
mod tls;
mod views;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct Config {
    /// Externally visible base URL, used for the OIDC redirect URI and cookie security.
    pub public_url: String,
    pub bind: String,
    pub data_dir: PathBuf,
    pub max_upload_bytes: usize,
}

impl Config {
    fn from_env() -> Self {
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        Config {
            public_url: var("AUTOKAN_PUBLIC_URL", "http://localhost:3000").trim_end_matches('/').to_string(),
            bind: var("AUTOKAN_BIND", "127.0.0.1:3000"),
            data_dir: PathBuf::from(var("AUTOKAN_DATA_DIR", "data")),
            max_upload_bytes: var("AUTOKAN_MAX_UPLOAD_MB", "200").parse::<usize>().unwrap_or(200) * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: db::Db,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub throttle: Arc<Mutex<auth::Throttle>>,
    pub ldap_throttle: Arc<Mutex<auth::KeyedThrottle>>,
    /// Asks the background worker to sync a board with GitHub / GitLab soon.
    pub sync: sync::SyncHandle,
}

impl AppState {
    pub fn uploads_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("uploads")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "autokan=info,tower_http=warn".into()),
        )
        .init();

    let cfg = Config::from_env();
    std::fs::create_dir_all(cfg.data_dir.join("uploads"))?;
    // SQLite in the data directory unless AUTOKAN_DATABASE_URL names another backend.
    let database_url = std::env::var("AUTOKAN_DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| format!("sqlite://{}", cfg.data_dir.join("autokan.db").display()));
    let db = db::connect(&database_url).await?;
    tracing::info!("database: {}", db::Conn::dialect(&db).name());

    if std::env::args().nth(1).as_deref() == Some("reset-breakglass") {
        let (username, password) = auth::reset_breakglass(&db).await?;
        println!("Break glass account reset.\n  username: {username}\n  password: {password}");
        return Ok(());
    }

    auth::ensure_breakglass(&db).await?;
    if db::column_exists(&db, "boards", "agent_base_url").await? {
        llm::migrate_legacy(&db).await?;
    }

    let (sync_handle, sync_requests) = sync::channel();
    let state = AppState {
        sync: sync_handle,
        db,
        http: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()?,
        throttle: Default::default(),
        ldap_throttle: Default::default(),
        cfg: Arc::new(cfg),
    };

    sync::spawn(state.clone(), sync_requests);

    let app = handlers::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::origin_guard))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&state.cfg.bind).await?;
    tracing::info!("autokan listening on http://{} (public URL {})", state.cfg.bind, state.cfg.public_url);
    axum::serve(listener, app).await?;
    Ok(())
}
