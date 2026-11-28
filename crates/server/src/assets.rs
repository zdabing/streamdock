use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../client"]
struct EmbeddedClient;

pub const SCRIPT_CAT_JS: &str = include_str!("../../../examples/scriptcat-downloader.user.js");

fn sanitize_rel_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let mut out = PathBuf::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    Some(out)
}

pub async fn read_web_asset(web_dir: Option<&Path>, rel_path: &str) -> Option<(Vec<u8>, String)> {
    let rel = sanitize_rel_path(rel_path)?;
    let mime = mime_guess::from_path(&rel)
        .first_or_octet_stream()
        .essence_str()
        .to_string();

    if let Some(dir) = web_dir {
        let candidate = dir.join(&rel);
        if let Ok(bytes) = tokio::fs::read(&candidate).await {
            return Some((bytes, mime));
        }
    }

    let embed_key = rel.to_string_lossy().replace('\\', "/");
    EmbeddedClient::get(&embed_key).map(|f| (f.data.into_owned(), mime))
}

pub async fn serve_index(web_dir: Option<&Path>) -> Response {
    match read_web_asset(web_dir, "index.html").await {
        Some((bytes, _mime)) => {
            let mut resp = Response::new(Body::from(bytes));
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        None => (StatusCode::NOT_FOUND, "WebUI index.html not found").into_response(),
    }
}

pub async fn serve_asset(web_dir: Option<&Path>, asset_path: &str) -> Response {
    let clean = asset_path.trim_start_matches('/');
    if clean.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let rel = format!("assets/{clean}");
    match read_web_asset(web_dir, &rel).await {
        Some((bytes, mime)) => {
            let mut resp = Response::new(Body::from(bytes));
            if let Ok(val) = HeaderValue::from_str(&mime) {
                resp.headers_mut().insert(header::CONTENT_TYPE, val);
            }
            resp
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub fn serve_scriptcat_user_js() -> Response {
    let mut resp = Response::new(Body::from(SCRIPT_CAT_JS));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}
