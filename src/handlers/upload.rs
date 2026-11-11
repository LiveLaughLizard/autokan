//! Image, video and document attachments for card descriptions and comments.
use crate::db::{query, query_as};
use crate::auth::User;
use crate::error::{bad, AppError, Res};
use crate::markdown::{FILE_EXTS, IMAGE_EXTS, VIDEO_EXTS};
use crate::perm::{board_access, require};
use crate::AppState;
use axum::extract::{Multipart, Path, Request, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use tower_http::services::ServeFile;

pub async fn upload(State(state): State<AppState>, user: User, Path(board_id): Path<i64>, mut mp: Multipart) -> Res<Json<Value>> {
    require(board_access(&state.db, &user, board_id).await?.participant())?;
    let Some(mut field) = mp.next_field().await.map_err(|e| bad(e.to_string()))? else {
        return Err(bad("No file was uploaded"));
    };
    let filename = field.file_name().unwrap_or("file").to_string();
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let media = IMAGE_EXTS.contains(&ext.as_str()) || VIDEO_EXTS.contains(&ext.as_str());
    if !media && !FILE_EXTS.contains(&ext.as_str()) {
        return Err(bad(format!(
            "Unsupported file type. Allowed: {}, {} and {}",
            IMAGE_EXTS.join(", "),
            VIDEO_EXTS.join(", "),
            FILE_EXTS.join(", ")
        )));
    }
    let id = format!("{}.{ext}", uuid::Uuid::new_v4());
    let path = state.uploads_dir().join(&id);
    let mut size = 0i64;
    let written: Result<(), String> = async {
        let mut file = tokio::fs::File::create(&path).await.map_err(|e| e.to_string())?;
        while let Some(chunk) = field.chunk().await.map_err(|e| e.to_string())? {
            size += chunk.len() as i64;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(bad(format!("Upload failed (file too large?): {e}")));
    }
    query("INSERT INTO uploads (id, board_id, user_id, filename, size, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(board_id)
        .bind(user.id)
        .bind(&filename)
        .bind(size)
        .bind(crate::db::now())
        .execute(&state.db)
        .await?;
    // Images and videos use Markdown image syntax (videos are detected by extension when
    // rendering); any other file becomes an ordinary link that downloads it.
    let alt = filename.replace(['[', ']', '\n'], "");
    let markdown = if media { format!("![{alt}](/uploads/{id})") } else { format!("[{alt}](/uploads/{id})") };
    Ok(Json(json!({ "url": format!("/uploads/{id}"), "markdown": markdown })))
}

pub async fn serve(State(state): State<AppState>, user: User, Path(id): Path<String>, req: Request) -> Res<Response> {
    let (board_id,): (i64,) = query_as("SELECT board_id FROM uploads WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    board_access(&state.db, &user, board_id).await?;
    // `id` matched a database row we generated ('<uuid>.<ext>'), so it is safe as a file name.
    // ServeFile handles Range requests, which video seeking needs.
    let mut resp = ServeFile::new(state.uploads_dir().join(&id))
        .oneshot(req)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .map(axum::body::Body::new);
    let headers = resp.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
    // Only images and videos are shown in the page; everything else is offered as a download, so
    // an uploaded document can never run in this site's origin.
    let ext = id.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if !IMAGE_EXTS.contains(&ext) && !VIDEO_EXTS.contains(&ext) {
        headers.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    }
    Ok(resp.into_response())
}
