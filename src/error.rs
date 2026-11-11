use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub enum AppError {
    NotFound,
    Forbidden,
    Bad(String),
    Internal(anyhow::Error),
}

pub type Res<T> = Result<T, AppError>;

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError::Internal(e.into())
    }
}

pub fn bad(msg: impl Into<String>) -> AppError {
    AppError::Bad(msg.into())
}

impl AppError {
    /// Short human-readable message (also used for agent tool results).
    pub fn message(&self) -> String {
        match self {
            AppError::NotFound => "Not found".into(),
            AppError::Forbidden => "You do not have permission to do that".into(),
            AppError::Bad(m) => m.clone(),
            AppError::Internal(_) => "Internal error".into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::Bad(_) => StatusCode::BAD_REQUEST,
            AppError::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, self.message()).into_response()
    }
}
