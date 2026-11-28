use std::collections::HashSet;
use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap as HttpHeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use serde::Serialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use url::Url;

use streamdock_core::cat_catch::cat_catch_tasks;
use streamdock_core::contract::{
    ApiResponse, RssSourceInput, RssValidateRequest, ScrapeDataSource, ScrapeFolderTemplate,
    ScrapeJobSnapshot, ScrapeJobStatus, ScrapeMode, ScrapePosterCrop, ScrapeRunItemInput,
    ScrapeRunRequest, ScrapeScanRequest, ScrapeSourceType, ScrapeTransferMode, SegmentProgress,
    SegmentSnapshot, SegmentStatus, TaskOptions, TaskStatus,
};
use streamdock_core::error::CoreError;
use streamdock_core::extractor::{ExtractMediaOptions, extract_media};
use streamdock_core::format_options::{absolutize, format_options, source_url};
use streamdock_core::http::{
    format_headers, format_headers_str, md5_hex, merge_header_maps, normalize_download_url,
};
use streamdock_core::rss_service::RssItemAction;
use streamdock_core::scraper::matcher::resolve_wordpress_api_url;
use streamdock_core::scraper::{ScanOptions, ScrapeRunOptions, run_media_scrape, scan_for_scrape};
use streamdock_core::store::{default_rss_url, normalize_http_href, validate_server_config_update};
use streamdock_core::video_parser::VideoParser;

use crate::assets::{serve_asset, serve_index, serve_scriptcat_user_js};
use crate::state::{
    EnqueueTaskInput, ScrapeJobRuntime, ServerState, VERSION, build_exec_options_from,
    canonical_path, now_ms, rebuild_client_for_config, scrape_data_source_from_json_defaults,
};
use crate::ws::ws_handler;

pub struct AppError {
    status: StatusCode,
    code: i32,
    message: String,
    data: Option<serde_json::Value>,
}

impl AppError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: 1,
            message: message.into(),
            data: None,
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: 1,
            message: message.into(),
            data: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: 1,
            message: message.into(),
            data: None,
        }
    }

    pub fn conflict(message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: 1,
            message: message.into(),
            data,
        }
    }
}

impl From<CoreError> for AppError {
    fn from(err: CoreError) -> Self {
        Self::bad_request(err.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::bad_request(err.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        Self::bad_request(err.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut obj = serde_json::Map::new();
        obj.insert("code".to_string(), json!(self.code));
        obj.insert("message".to_string(), json!(self.message));
        if let Some(d) = self.data {
            obj.insert("data".to_string(), d);
        }
        (self.status, Json(serde_json::Value::Object(obj))).into_response()
    }
}

struct ExtractionGuard {
    state: Arc<ServerState>,
    id: u64,
    token: CancellationToken,
}

impl ExtractionGuard {
    async fn acquire(state: &Arc<ServerState>) -> Result<Self, AppError> {
        if state.inner.lock().unwrap().closing {
            return Err(AppError::bad_request("服务正在关闭"));
        }
        let (id, token) = state.register_extraction(None).await;
        Ok(Self {
            state: state.clone(),
            id,
            token,
        })
    }
}

impl Drop for ExtractionGuard {
    fn drop(&mut self) {
        self.token.cancel();
        let st = self.state.clone();
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                st.unregister_extraction(id).await;
            });
        }
    }
}

fn parse_body_value(bytes: &Bytes) -> Result<serde_json::Value, AppError> {
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|e| AppError::bad_request(format!("请求体 JSON 解析失败: {e}")))
}

fn extract_query_param(query: Option<&str>, key: &str) -> String {
    let Some(q) = query else {
        return String::new();
    };
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

pub fn build_router(state: Arc<ServerState>) -> Router {
    let api = Router::new()
        .route("/platforms", get(handle_platforms))
        .route("/parse", post(handle_parse))
        .route("/extract", post(handle_extract))
        .route("/getM3u8Urls", post(handle_legacy_extract))
        .route(
            "/scrape/config",
            get(handle_get_scrape_config).post(handle_set_scrape_config),
        )
        .route("/scrape/scan", post(handle_scrape_scan))
        .route("/scrape/run", post(handle_scrape_run))
        .route(
            "/scrape/job",
            get(handle_get_scrape_job).post(handle_start_scrape_job),
        )
        .route("/scrape/job/resume", post(handle_resume_scrape_job))
        .route("/scrape/job/stop", post(handle_stop_scrape_job))
        .route("/scrape/job/clear", post(handle_clear_scrape_job))
        .route("/tasks/scrape", post(handle_task_scrape))
        .route(
            "/rss/sources",
            get(handle_list_rss_sources).post(handle_create_rss_source),
        )
        .route("/rss/validate", post(handle_validate_rss))
        .route("/rss/sources/{id}", post(handle_update_rss_source))
        .route("/rss/sources/{id}/delete", post(handle_delete_rss_source))
        .route("/rss/sources/{id}/refresh", post(handle_refresh_rss_source))
        .route("/rss/sources/{id}/items", get(handle_query_rss_items))
        .route("/rss/sources/{id}/actions", post(handle_rss_actions))
        .route(
            "/rss/sources/{id}/mark-all-read",
            post(handle_rss_mark_all_read),
        )
        .route("/config", get(handle_get_config).post(handle_set_config))
        .route("/tasks", get(handle_get_tasks))
        .route("/segments", get(handle_get_segments))
        .route("/retry-failed", post(handle_retry_failed))
        .route("/queue/status", get(handle_queue_status))
        .route("/cat-catch", post(handle_cat_catch))
        .route("/download", post(handle_download))
        .route("/pause", post(handle_pause))
        .route("/resume", post(handle_resume))
        .route("/delete", post(handle_delete))
        .route("/file", get(handle_download_file))
        .layer(DefaultBodyLimit::max(5 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api_auth_middleware,
        ));

    Router::new()
        .route("/healthcheck", get(handle_healthcheck))
        .route(
            "/scriptcat.user.js",
            get(|| async { serve_scriptcat_user_js() }),
        )
        .route("/", get(handle_index))
        .route("/index.html", get(handle_index))
        .route("/page", get(handle_index))
        .route("/page/{*path}", get(handle_page_wildcard))
        .route("/assets/{*path}", get(handle_assets))
        .route("/ws", get(ws_handler))
        .nest("/api", api)
        .with_state(state)
}

async fn api_auth_middleware(
    State(state): State<Arc<ServerState>>,
    req: Request,
    next: Next,
) -> Response {
    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let token = if !header_token.is_empty() {
        header_token
    } else if req.uri().path() == "/file" {
        extract_query_param(req.uri().query(), "token")
    } else {
        String::new()
    };

    if !state.authorized(&token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": 1008, "message": "请输入正确的访问密码" })),
        )
            .into_response();
    }

    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn handle_healthcheck() -> Json<serde_json::Value> {
    Json(json!({ "code": 0, "message": "ok", "version": VERSION }))
}

async fn handle_index(State(state): State<Arc<ServerState>>) -> Response {
    serve_index(state.web_dir.as_deref()).await
}

async fn handle_page_wildcard(
    State(state): State<Arc<ServerState>>,
    AxumPath(_path): AxumPath<String>,
) -> Response {
    serve_index(state.web_dir.as_deref()).await
}

async fn handle_assets(
    State(state): State<Arc<ServerState>>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    serve_asset(state.web_dir.as_deref(), &path).await
}

async fn handle_platforms() -> Json<serde_json::Value> {
    Json(json!({ "code": 0, "data": VideoParser::get_supported_platforms() }))
}

async fn parse_header_override(
    cfg_headers: &str,
    body_headers: Option<&serde_json::Value>,
) -> Result<streamdock_core::contract::HeaderMap, AppError> {
    let base = format_headers_str(cfg_headers)?;
    let over = match body_headers {
        None | Some(serde_json::Value::Null) => Default::default(),
        Some(serde_json::Value::String(s)) => format_headers_str(s)?,
        Some(serde_json::Value::Object(obj)) => {
            let input = streamdock_core::contract::HeaderInput::Map(
                obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            );
            format_headers(Some(&input))?
        }
        _ => return Err(AppError::bad_request("请求头必须是对象")),
    };
    Ok(merge_header_maps(&base, &over))
}

async fn handle_parse(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let val = parse_body_value(&body)?;
    let (cfg_headers, client) = {
        let g = state.inner.lock().unwrap();
        (g.cfg.headers.clone(), g.client.clone())
    };
    let headers = parse_header_override(&cfg_headers, val.get("headers")).await?;
    let target = val.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let timeout_tok = guard.token.child_token();
    let timer_tok = timeout_tok.clone();
    let timer_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        timer_tok.cancel();
    });
    let res = VideoParser::parse(target, &headers, client, timeout_tok).await;
    timer_handle.abort();
    let resp = res?;
    Ok(Json(serde_json::to_value(resp)?))
}

async fn run_extract_common(
    state: Arc<ServerState>,
    body: Bytes,
) -> Result<streamdock_core::contract::ExtractionResult, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let val = parse_body_value(&body)?;
    let (cfg_headers, client) = {
        let g = state.inner.lock().unwrap();
        (g.cfg.headers.clone(), g.client.clone())
    };
    let headers = parse_header_override(&cfg_headers, val.get("headers")).await?;

    let deep = match val.get("deep") {
        None | Some(serde_json::Value::Null) => 1usize,
        Some(v) => {
            let Some(n) = v.as_i64() else {
                return Err(AppError::bad_request("搜索深度应为 0–3"));
            };
            if !(0..=3).contains(&n) {
                return Err(AppError::bad_request("搜索深度应为 0–3"));
            }
            n as usize
        }
    };

    let path_filter_val = val.get("pathFilter").or_else(|| val.get("subUrlRegex"));
    let path_filter = match path_filter_val {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => {
            if s.chars().count() > 200 {
                return Err(AppError::bad_request("子页面路径筛选最多 200 个字符"));
            }
            s.clone()
        }
        _ => return Err(AppError::bad_request("子页面路径筛选最多 200 个字符")),
    };

    let url = val
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let urls = val.get("urls").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect::<Vec<_>>()
    });

    let result = extract_media(
        ExtractMediaOptions {
            url,
            urls,
            headers,
            deep: Some(deep),
            max_pages: Some(30),
            max_urls: Some(100),
            max_html_size: None,
            concurrency: None,
            path_filter: Some(path_filter),
            sub_url_regex: None,
            client: Some(client),
        },
        &guard.token,
    )
    .await?;
    Ok(result)
}

async fn handle_extract(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let result = run_extract_common(state, body).await?;
    Ok(Json(json!({ "code": 0, "data": result })))
}

async fn handle_legacy_extract(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let result = run_extract_common(state, body).await?;
    let legacy: Vec<[String; 2]> = result
        .items
        .into_iter()
        .map(|it| [it.url, it.title])
        .collect();
    Ok(Json(json!({ "code": 0, "data": legacy })))
}

async fn handle_get_scrape_config(
    State(state): State<Arc<ServerState>>,
) -> Json<serde_json::Value> {
    let cfg = state.inner.lock().unwrap().scrape_cfg.clone();
    Json(json!({ "code": 0, "data": cfg }))
}

async fn handle_set_scrape_config(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let next = {
        let mut g = state.inner.lock().unwrap();
        let updated = scrape_data_source_from_json_defaults(&val, &g.scrape_cfg, &g.cfg.save_dir);
        state.store.save_scrape_config(&updated)?;
        g.scrape_cfg = updated.clone();
        updated
    };
    state.broadcast_scrape_config().await;
    Ok(Json(json!({
        "code": 0,
        "message": "刮削配置已保存",
        "data": next
    })))
}

fn normalize_scan_sources_from_json(arr: &[serde_json::Value]) -> Vec<ScrapeDataSource> {
    let mut out = Vec::new();
    let mut seen_ids = HashSet::new();
    for item in arr {
        let Some(rec) = item.as_object() else {
            continue;
        };
        let stype = if rec.get("type").and_then(|v| v.as_str()) == Some("wordpress") {
            ScrapeSourceType::Wordpress
        } else {
            ScrapeSourceType::Rss
        };
        let url = rec
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() {
            continue;
        }
        let prefix = if stype == ScrapeSourceType::Wordpress {
            "wordpress"
        } else {
            "rss"
        };
        let mut id = rec
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{prefix}-{}", out.len() + 1));
        while seen_ids.contains(&id) {
            id = format!("{prefix}-{}-{}", out.len() + 1, seen_ids.len());
        }
        seen_ids.insert(id.clone());
        let fallback_name = Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| {
                if stype == ScrapeSourceType::Wordpress {
                    "WordPress 站点".to_string()
                } else {
                    "RSS 订阅源".to_string()
                }
            });
        let name: String = rec
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(60).collect())
            .unwrap_or(fallback_name);
        let referer = rec
            .get("referer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(ScrapeDataSource {
            id,
            name,
            source_type: stype,
            url,
            enabled: rec.get("enabled").and_then(|v| v.as_bool()) != Some(false),
            referer: Some(referer),
        });
        if out.len() >= 20 {
            break;
        }
    }
    out
}

async fn handle_scrape_scan(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let val = parse_body_value(&body)?;
    let obj = val.as_object();

    let sources: Option<Vec<ScrapeDataSource>> = if let Some(arr) = obj
        .and_then(|o| o.get("sources"))
        .and_then(|v| v.as_array())
    {
        if arr.is_empty() {
            None
        } else {
            let normalized = normalize_scan_sources_from_json(arr);
            let mut validated = Vec::with_capacity(normalized.len());
            for src in normalized {
                if !src.enabled {
                    validated.push(src);
                    continue;
                }
                let label = if src.source_type == ScrapeSourceType::Wordpress {
                    "WordPress 数据源地址"
                } else {
                    "RSS 数据源地址"
                };
                let validated_url = normalize_http_href(Some(&src.url), label, true)?;
                if src.source_type == ScrapeSourceType::Wordpress {
                    resolve_wordpress_api_url(&validated_url)?;
                }
                let validated_ref = normalize_http_href(src.referer.as_deref(), "Referer", false)?;
                validated.push(ScrapeDataSource {
                    url: validated_url,
                    referer: Some(validated_ref),
                    ..src
                });
            }
            if !validated.iter().any(|s| s.enabled) {
                return Err(AppError::bad_request("请至少启用一个元数据数据源"));
            }
            Some(validated)
        }
    } else {
        None
    };

    let feed_id = obj
        .and_then(|o| o.get("feedId"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let feed = if feed_id.is_empty() {
        None
    } else {
        state.rss_service.get_source(feed_id).await
    };

    let raw_rss = obj
        .and_then(|o| o.get("rssUrl"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| feed.as_ref().map(|f| f.url.clone()))
        .unwrap_or_default();

    let rss_url = if sources.is_some() {
        raw_rss
    } else {
        let cand = if raw_rss.is_empty() {
            default_rss_url()
        } else {
            raw_rss
        };
        normalize_http_href(Some(&cand), "RSS 订阅地址", true)?
    };

    let referer = match obj
        .and_then(|o| o.get("referer"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(r) => Some(normalize_http_href(Some(r), "Referer", false)?),
        None => feed.as_ref().map(|f| f.referer.clone()),
    };

    let default_scan_dirs = state.inner.lock().unwrap().scrape_cfg.scan_dirs.clone();
    let scan_dirs = if let Some(arr) = obj
        .and_then(|o| o.get("scanDirs"))
        .and_then(|v| v.as_array())
    {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for v in arr {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if t.is_empty() {
                    continue;
                }
                let k = if cfg!(windows) {
                    t.to_ascii_lowercase()
                } else {
                    t.to_string()
                };
                if seen.insert(k) {
                    out.push(t.to_string());
                    if out.len() >= 20 {
                        break;
                    }
                }
            }
        }
        if out.is_empty() {
            default_scan_dirs
        } else {
            out
        }
    } else if let Some(sd) = obj
        .and_then(|o| o.get("scanDir"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        vec![sd.to_string()]
    } else {
        default_scan_dirs
    };

    let min_score = match obj.and_then(|o| o.get("minScore")) {
        None | Some(serde_json::Value::Null) => 55u32,
        Some(v) => {
            let Some(num) = v.as_f64() else {
                return Err(AppError::bad_request("最低匹配分数应为 0–100"));
            };
            if !num.is_finite() || !(0.0..=100.0).contains(&num) {
                return Err(AppError::bad_request("最低匹配分数应为 0–100"));
            }
            num as u32
        }
    };

    let recursive = obj
        .and_then(|o| o.get("recursive"))
        .and_then(|v| v.as_bool())
        != Some(false);
    let skip_existing_nfo = obj
        .and_then(|o| o.get("skipExistingNfo"))
        .and_then(|v| v.as_bool())
        != Some(false);
    let refresh_rss = obj
        .and_then(|o| o.get("refreshRss"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let req = ScrapeScanRequest {
        scan_dir: scan_dirs.first().cloned(),
        scan_dirs: Some(scan_dirs),
        sources,
        rss_url: Some(rss_url),
        feed_id: feed.map(|f| f.id),
        referer,
        recursive: Some(recursive),
        skip_existing_nfo: Some(skip_existing_nfo),
        min_score: Some(min_score),
        refresh_rss: Some(refresh_rss),
    };

    let result = scan_for_scrape(
        &req,
        &ScanOptions {
            cache_dir: Some(state.cache_dir.clone()),
            log: true,
        },
        &guard.token,
    )
    .await?;

    Ok(Json(json!({ "code": 0, "data": result })))
}

fn parse_scrape_run_body(val: &serde_json::Value) -> Result<ScrapeRunRequest, AppError> {
    let Some(obj) = val.as_object() else {
        return Err(AppError::bad_request("请选择 1–20000 个待刮削视频"));
    };
    let Some(items_arr) = obj.get("items").and_then(|v| v.as_array()) else {
        return Err(AppError::bad_request("请选择 1–20000 个待刮削视频"));
    };
    if items_arr.is_empty() || items_arr.len() > 20_000 {
        return Err(AppError::bad_request("请选择 1–20000 个待刮削视频"));
    }

    let mut normalized_items = Vec::with_capacity(items_arr.len());
    for raw_item in items_arr {
        let Some(rec) = raw_item.as_object() else {
            return Err(AppError::bad_request("待刮削文件路径无效"));
        };
        let file_path = rec
            .get("filePath")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| AppError::bad_request("待刮削文件路径无效"))?
            .to_string();
        let page_url = normalize_http_href(
            rec.get("pageUrl").and_then(|v| v.as_str()),
            "详情页地址",
            true,
        )?;
        let cover_url = match rec
            .get("coverUrl")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(c) => Some(normalize_http_href(Some(c), "封面地址", false)?),
            None => None,
        };
        let item_referer = match rec
            .get("referer")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(r) => Some(normalize_http_href(Some(r), "Referer", false)?),
            None => None,
        };
        let actors = rec
            .get("actors")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                let list: Vec<String> = arr
                    .iter()
                    .filter_map(|x| {
                        x.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                    .take(10)
                    .collect();
                (!list.is_empty()).then_some(list)
            });
        let tags = rec.get("tags").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });
        normalized_items.push(ScrapeRunItemInput {
            file_path,
            page_url,
            title: rec
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            pub_date: rec
                .get("pubDate")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tags,
            guid: rec.get("guid").and_then(|v| v.as_str()).map(str::to_string),
            actors,
            studio: rec
                .get("studio")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            cover_url,
            referer: item_referer,
        });
    }

    let mode = if obj.get("mode").and_then(|v| v.as_str()) == Some("organize") {
        ScrapeMode::Organize
    } else {
        ScrapeMode::InPlace
    };
    let output_dir = obj
        .get("outputDir")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if mode == ScrapeMode::Organize && output_dir.is_empty() {
        return Err(AppError::bad_request("归档整理模式请填写目标输出目录"));
    }

    let transfer_mode = match obj.get("transferMode").and_then(|v| v.as_str()) {
        Some("hardlink") => ScrapeTransferMode::Hardlink,
        Some("copy") => ScrapeTransferMode::Copy,
        _ => ScrapeTransferMode::Move,
    };
    let folder_template = if obj.get("folderTemplate").and_then(|v| v.as_str()) == Some("title") {
        ScrapeFolderTemplate::Title
    } else {
        ScrapeFolderTemplate::ActorTitle
    };
    let poster_crop = match obj.get("posterCrop").and_then(|v| v.as_str()) {
        Some("right") => ScrapePosterCrop::Right,
        Some("none") => ScrapePosterCrop::None,
        _ => ScrapePosterCrop::Center,
    };
    let concurrency = obj
        .get("concurrency")
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite())
        .map(|n| (n.round() as usize).clamp(1, 10));
    let referer = match obj
        .get("referer")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        Some(r) => Some(normalize_http_href(Some(r), "Referer", false)?),
        None => None,
    };
    let studio = obj
        .get("studio")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    Ok(ScrapeRunRequest {
        items: normalized_items,
        mode,
        output_dir: Some(output_dir),
        transfer_mode: Some(transfer_mode),
        folder_template: Some(folder_template),
        poster_crop: Some(poster_crop),
        download_images: Some(obj.get("downloadImages").and_then(|v| v.as_bool()) != Some(false)),
        overwrite_nfo: Some(
            obj.get("overwriteNfo")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        referer,
        studio,
        concurrency,
    })
}

async fn handle_scrape_run(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let val = parse_body_value(&body)?;
    let req = parse_scrape_run_body(&val)?;
    let ffmpeg_path = state.inner.lock().unwrap().cfg.ffmpeg_path.clone();
    let result = run_media_scrape(
        &req,
        &ScrapeRunOptions {
            ffmpeg_path,
            log: true,
            on_item_start: None,
            on_item_done: None,
        },
        &guard.token,
    )
    .await?;
    Ok(Json(json!({ "code": 0, "data": result })))
}

async fn handle_get_scrape_job(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let include_paths = matches!(
        params.get("includePaths").map(String::as_str),
        Some("1" | "true")
    );
    let snap = state.build_scrape_job_snapshot(include_paths).await;
    Json(json!({ "code": 0, "data": snap }))
}

async fn handle_start_scrape_job(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    {
        let g = state.inner.lock().unwrap();
        if g.closing {
            return Err(AppError::bad_request("服务正在关闭"));
        }
        if let Some(ref job) = g.scrape_job {
            if job.snapshot.status == ScrapeJobStatus::Running {
                let snap = serde_json::to_value(job.build_snapshot(false))?;
                return Err(AppError::conflict(
                    "已有后台刮削任务正在运行，请等待完成或先停止当前任务",
                    Some(snap),
                ));
            }
        }
    }

    let val = parse_body_value(&body)?;
    let mut req = parse_scrape_run_body(&val)?;
    let mut deduped = Vec::with_capacity(req.items.len());
    let mut seen = HashSet::new();
    for item in req.items {
        let key = canonical_path(&item.file_path);
        if seen.insert(key) {
            deduped.push(item);
        }
    }
    req.items = deduped;
    let items_to_run = req.items.clone();
    let total = items_to_run.len();

    {
        let mut g = state.inner.lock().unwrap();
        g.scrape_job = Some(ScrapeJobRuntime {
            snapshot: ScrapeJobSnapshot {
                id: format!("scrape-{}", now_ms()),
                revision: None,
                status: ScrapeJobStatus::Running,
                mode: req.mode,
                output_dir: (req.mode == ScrapeMode::Organize)
                    .then(|| req.output_dir.clone())
                    .flatten(),
                transfer_mode: req.transfer_mode.unwrap_or(ScrapeTransferMode::Move),
                concurrency: req.concurrency.unwrap_or(3),
                total,
                done: 0,
                succeeded: 0,
                failed: 0,
                remaining: total,
                can_resume: false,
                can_retry_failed: false,
                started_at: now_ms(),
                finished_at: None,
                elapsed_ms: None,
                current_file: None,
                error: None,
                timing: None,
                results: Vec::new(),
                total_results: 0,
                succeeded_paths: None,
            },
            revision: 0,
            request: req,
            cancel: None,
            done_rx: None,
            failed_results: Vec::new(),
            recent_succeeded: Vec::new(),
            succeeded_paths: Vec::new(),
            processed_paths: HashSet::new(),
            failed_paths: HashSet::new(),
            elapsed_before_run_ms: 0,
            run_started_at: now_ms(),
            timing_totals: Default::default(),
        });
    }

    state.launch_scrape_job_runner(items_to_run).await;
    let snap = state.build_scrape_job_snapshot(false).await;
    Ok(Json(json!({
        "code": 0,
        "message": format!("已启动后台刮削（共 {total} 个视频）"),
        "data": snap
    })))
}

async fn handle_resume_scrape_job(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let obj = val.as_object();

    let (items_to_run, retry_failed) = {
        let mut g = state.inner.lock().unwrap();
        if g.closing {
            return Err(AppError::bad_request("服务正在关闭"));
        }
        let Some(ref mut job) = g.scrape_job else {
            return Err(AppError::bad_request(
                "当前没有可继续的刮削任务，请先扫描并启动刮削",
            ));
        };
        if job.snapshot.status == ScrapeJobStatus::Running {
            let snap = serde_json::to_value(job.build_snapshot(false))?;
            return Err(AppError::conflict("后台刮削任务已在运行中", Some(snap)));
        }

        if let Some(o) = obj {
            if let Some(m) = o.get("mode").and_then(|v| v.as_str()) {
                job.request.mode = if m == "organize" {
                    ScrapeMode::Organize
                } else {
                    ScrapeMode::InPlace
                };
            }
            if let Some(od) = o.get("outputDir") {
                job.request.output_dir = Some(od.as_str().map(str::trim).unwrap_or("").to_string());
            }
            if job.request.mode == ScrapeMode::Organize
                && job
                    .request
                    .output_dir
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
            {
                return Err(AppError::bad_request("归档整理模式请填写目标输出目录"));
            }
            if let Some(tm) = o.get("transferMode").and_then(|v| v.as_str()) {
                job.request.transfer_mode = match tm {
                    "move" => Some(ScrapeTransferMode::Move),
                    "hardlink" => Some(ScrapeTransferMode::Hardlink),
                    "copy" => Some(ScrapeTransferMode::Copy),
                    _ => job.request.transfer_mode,
                };
            }
            if let Some(ft) = o.get("folderTemplate").and_then(|v| v.as_str()) {
                job.request.folder_template = match ft {
                    "title" => Some(ScrapeFolderTemplate::Title),
                    "actor-title" => Some(ScrapeFolderTemplate::ActorTitle),
                    _ => job.request.folder_template,
                };
            }
            if let Some(pc) = o.get("posterCrop").and_then(|v| v.as_str()) {
                job.request.poster_crop = match pc {
                    "center" => Some(ScrapePosterCrop::Center),
                    "right" => Some(ScrapePosterCrop::Right),
                    "none" => Some(ScrapePosterCrop::None),
                    _ => job.request.poster_crop,
                };
            }
            if let Some(di) = o.get("downloadImages") {
                job.request.download_images = Some(di.as_bool() != Some(false));
            }
            if let Some(on) = o.get("overwriteNfo") {
                job.request.overwrite_nfo = Some(on.as_bool().unwrap_or(false));
            }
            if let Some(rf) = o.get("referer") {
                job.request.referer = match rf.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(s) => Some(normalize_http_href(Some(s), "Referer", false)?),
                    None => None,
                };
            }
            if let Some(c) = o.get("concurrency").and_then(|v| v.as_f64()) {
                if c.is_finite() {
                    job.request.concurrency = Some((c.round() as usize).clamp(1, 10));
                }
            }
        }

        job.snapshot.mode = job.request.mode;
        job.snapshot.output_dir = (job.request.mode == ScrapeMode::Organize)
            .then(|| job.request.output_dir.clone())
            .flatten();
        job.snapshot.transfer_mode = job
            .request
            .transfer_mode
            .unwrap_or(ScrapeTransferMode::Move);
        job.snapshot.concurrency = job.request.concurrency.unwrap_or(3);

        let retry_failed = obj
            .and_then(|o| o.get("retryFailed"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let list: Vec<ScrapeRunItemInput> = if retry_failed {
            let list: Vec<ScrapeRunItemInput> = job
                .request
                .items
                .iter()
                .filter(|it| job.failed_paths.contains(&canonical_path(&it.file_path)))
                .cloned()
                .collect();
            if list.is_empty() {
                return Err(AppError::bad_request("没有需要重试的失败项"));
            }
            for item in &list {
                let k = canonical_path(&item.file_path);
                job.processed_paths.remove(&k);
                job.failed_paths.remove(&k);
            }
            job.failed_results.clear();
            job.snapshot.failed = 0;
            job.snapshot.done = job.snapshot.succeeded;
            list
        } else {
            let list: Vec<ScrapeRunItemInput> = job
                .request
                .items
                .iter()
                .filter(|it| !job.processed_paths.contains(&canonical_path(&it.file_path)))
                .cloned()
                .collect();
            if list.is_empty() {
                return Err(AppError::bad_request("所有视频均已处理完成，无需继续"));
            }
            list
        };
        (list, retry_failed)
    };

    let count = items_to_run.len();
    state.launch_scrape_job_runner(items_to_run).await;
    let snap = state.build_scrape_job_snapshot(false).await;
    let message = if retry_failed {
        format!("已重试 {count} 个失败视频")
    } else {
        format!("已继续后台刮削剩余 {count} 个视频")
    };
    Ok(Json(json!({ "code": 0, "message": message, "data": snap })))
}

async fn handle_stop_scrape_job(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let wait_rx = {
        let g = state.inner.lock().unwrap();
        match g.scrape_job {
            Some(ref job) if job.snapshot.status == ScrapeJobStatus::Running => {
                if let Some(ref c) = job.cancel {
                    c.cancel();
                }
                job.done_rx.clone()
            }
            _ => {
                let snap = g.scrape_job.as_ref().map(|j| j.build_snapshot(false));
                return Ok(Json(json!({
                    "code": 0,
                    "message": "当前没有运行中的刮削任务",
                    "data": snap
                })));
            }
        }
    };
    if let Some(mut rx) = wait_rx {
        let _ = rx.wait_for(|&done| done).await;
    }
    let snap = state.build_scrape_job_snapshot(true).await;
    Ok(Json(json!({
        "code": 0,
        "message": "已暂停后台刮削任务",
        "data": snap
    })))
}

async fn handle_clear_scrape_job(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    {
        let mut g = state.inner.lock().unwrap();
        if let Some(ref job) = g.scrape_job {
            if job.snapshot.status == ScrapeJobStatus::Running {
                return Err(AppError::bad_request(
                    "后台刮削正在运行，请先暂停再清空记录",
                ));
            }
        }
        g.scrape_job = None;
    }
    state.flush_scrape_job().await?;
    state.broadcast_scrape_job_now().await;
    Ok(Json(json!({
        "code": 0,
        "message": "已清空刮削记录",
        "data": serde_json::Value::Null
    })))
}

async fn handle_task_scrape(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let val = parse_body_value(&body)?;
    let url = val
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let overwrite = val.get("overwriteNfo").and_then(|v| v.as_bool()) != Some(false);
    let item = state.run_task_scrape(&url, &guard.token, overwrite).await?;
    let msg = if item.scrape_status == Some(streamdock_core::contract::TaskScrapeStatus::Done) {
        "NFO 与封面生成完成".to_string()
    } else {
        item.scrape_message
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "刮削失败".to_string())
    };
    Ok(Json(json!({ "code": 0, "message": msg, "data": item })))
}

async fn handle_list_rss_sources(State(state): State<Arc<ServerState>>) -> Json<serde_json::Value> {
    let list = state.rss_service.list_sources().await;
    Json(json!({ "code": 0, "data": list }))
}

async fn handle_validate_rss(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let val = parse_body_value(&body)?;
    let req: RssValidateRequest = serde_json::from_value(val)?;
    let data = state
        .rss_service
        .validate(&req, guard.token.clone())
        .await?;
    Ok(Json(json!({ "code": 0, "data": data })))
}

async fn handle_create_rss_source(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    if state.inner.lock().unwrap().closing {
        return Err(AppError::bad_request("服务正在关闭"));
    }
    let val = parse_body_value(&body)?;
    let fetch_now = val.get("fetchNow").and_then(|v| v.as_bool()) != Some(false);
    let input: RssSourceInput = serde_json::from_value(val)?;
    let created = state.rss_service.create_source(&input).await?;
    if fetch_now {
        let rss = state.rss_service.clone();
        let id = created.id.clone();
        tokio::spawn(async move {
            let _ = rss.refresh_source(&id, CancellationToken::new()).await;
        });
    }
    Ok(Json(json!({
        "code": 0,
        "message": "已添加订阅源",
        "data": created
    })))
}

async fn handle_update_rss_source(
    State(state): State<Arc<ServerState>>,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let updated = state.rss_service.update_source(&id, val).await?;
    Ok(Json(json!({
        "code": 0,
        "message": "订阅源已更新",
        "data": updated
    })))
}

async fn handle_delete_rss_source(
    State(state): State<Arc<ServerState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.rss_service.delete_source(&id).await?;
    Ok(Json(json!({ "code": 0, "message": "已删除订阅源" })))
}

async fn handle_refresh_rss_source(
    State(state): State<Arc<ServerState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let result = state
        .rss_service
        .refresh_source(&id, guard.token.clone())
        .await?;
    let extra = if result.auto_queued > 0 {
        format!("，自动加入 {} 个任务", result.auto_queued)
    } else {
        String::new()
    };
    let msg = format!(
        "刷新完成：共 {} 条（新增 {} 条）{extra}",
        result.total_items, result.added
    );
    Ok(Json(json!({ "code": 0, "message": msg, "data": result })))
}

async fn handle_query_rss_items(
    State(state): State<Arc<ServerState>>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let page = params
        .get("page")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    let page_size = params
        .get("pageSize")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(50);
    let q = params.get("q").map(String::as_str).unwrap_or("");
    let status = params.get("status").map(String::as_str).unwrap_or("all");
    let sort_oldest = params.get("sort").map(String::as_str) == Some("oldest");
    let data = state
        .rss_service
        .query_items(&id, status, q, sort_oldest, page, page_size)
        .await?;
    Ok(Json(json!({ "code": 0, "data": data })))
}

async fn handle_rss_actions(
    State(state): State<Arc<ServerState>>,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let guard = ExtractionGuard::acquire(&state).await?;
    let val = parse_body_value(&body)?;
    let action_str = val.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let action = match action_str {
        "download" => RssItemAction::Download,
        "ignore" => RssItemAction::Ignore,
        "mark-new" => RssItemAction::MarkNew,
        _ => return Err(AppError::bad_request("不支持的订阅操作")),
    };
    let guids: Vec<String> = val
        .get("guids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let data = state
        .rss_service
        .act_on_items(&id, &guids, action, guard.token.clone())
        .await?;
    let message = if action == RssItemAction::Download {
        let skipped = if data.skipped > 0 {
            format!("，跳过 {} 个", data.skipped)
        } else {
            String::new()
        };
        let errs = if !data.errors.is_empty() {
            format!("，失败 {} 个", data.errors.len())
        } else {
            String::new()
        };
        format!("已加入 {} 个下载任务{skipped}{errs}", data.queued)
    } else {
        format!("已更新 {} 条订阅状态", data.updated)
    };
    Ok(Json(json!({ "code": 0, "message": message, "data": data })))
}

async fn handle_rss_mark_all_read(
    State(state): State<Arc<ServerState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let (updated, source) = state.rss_service.mark_all_read(&id).await?;
    Ok(Json(json!({
        "code": 0,
        "message": format!("已将 {updated} 条新条目设为已忽略"),
        "data": { "updated": updated, "source": source }
    })))
}

async fn handle_get_config(State(state): State<Arc<ServerState>>) -> Json<serde_json::Value> {
    let cfg = state.inner.lock().unwrap().cfg.clone();
    Json(json!({ "code": 0, "data": cfg }))
}

async fn handle_set_config(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let patch = parse_body_value(&body)?;
    {
        let mut g = state.inner.lock().unwrap();
        let next = validate_server_config_update(&g.cfg, &patch)?;
        let client = rebuild_client_for_config(&next)?;
        state.store.save_server_config(&next)?;
        g.cfg = next;
        g.client = client;
    }
    state.start_next().await;
    Ok(Json(json!({ "code": 0, "message": "设置已保存" })))
}

async fn handle_get_tasks(State(state): State<Arc<ServerState>>) -> Json<serde_json::Value> {
    let ordered = state.inner.lock().unwrap().ordered_tasks();
    Json(json!({ "code": 0, "data": ordered }))
}

async fn handle_get_segments(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let url = params.get("url").map(String::as_str).unwrap_or("");
    let item = {
        let g = state.inner.lock().unwrap();
        g.tasks.get(url).cloned()
    };
    let Some(item) = item else {
        return Err(AppError::not_found("任务不存在"));
    };
    let seg_path = state.cache_dir.join(md5_hex(url)).join("segments.json");
    let snapshot: SegmentSnapshot = match tokio::fs::read_to_string(&seg_path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| fallback_segments(&item)),
        Err(_) => fallback_segments(&item),
    };
    Ok(Json(json!({ "code": 0, "data": snapshot })))
}

fn fallback_segments(item: &streamdock_core::contract::TaskItem) -> SegmentSnapshot {
    let items = if item.status == TaskStatus::Done {
        (0..item.stats.ts_count.unwrap_or(0))
            .map(|index| SegmentProgress {
                index,
                status: SegmentStatus::Done,
                size: 0,
                attempts: 0,
                error: None,
            })
            .collect()
    } else {
        Vec::new()
    };
    SegmentSnapshot {
        fingerprint: String::new(),
        items,
    }
}

async fn handle_retry_failed(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let all = val.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    let urls: Vec<String> = {
        let g = state.inner.lock().unwrap();
        if all {
            g.task_order.clone()
        } else {
            val.get("urls")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        }
    };

    let mut count = 0usize;
    for url in urls {
        let candidate = {
            let g = state.inner.lock().unwrap();
            let item = g.tasks.get(&url);
            if item.is_none_or(|t| {
                t.status != TaskStatus::Error
                    || t.browser_capture.unwrap_or(false)
                    || g.runs.contains_key(&url)
            }) {
                None
            } else {
                Some(item.unwrap().options.clone().unwrap_or_default())
            }
        };
        let Some(mut opts) = candidate else {
            continue;
        };
        let seg_path = state.cache_dir.join(md5_hex(&url)).join("segments.json");
        let Ok(content) = tokio::fs::read_to_string(&seg_path).await else {
            if all {
                continue;
            }
            return Err(AppError::bad_request("暂无失败分片记录，请使用继续下载"));
        };
        let Ok(snap) = serde_json::from_str::<SegmentSnapshot>(&content) else {
            continue;
        };
        if !snap.items.iter().any(|s| s.status == SegmentStatus::Error) {
            continue;
        }
        opts.retry_failed_only = Some(true);
        state
            .enqueue(EnqueueTaskInput {
                url,
                options: opts,
                browser_capture: false,
                browser_source: None,
            })
            .await?;
        count += 1;
    }

    let message = if count > 0 {
        format!("已重试 {count} 个任务的失败分片")
    } else {
        "没有可重试的失败分片".to_string()
    };
    Ok(Json(json!({ "code": 0, "message": message })))
}

async fn handle_queue_status(State(state): State<Arc<ServerState>>) -> Json<serde_json::Value> {
    let status = state.queue_status().await;
    Json(json!({ "code": 0, "data": status }))
}

async fn handle_cat_catch(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    if !state.has_token() {
        return Err(AppError::forbidden(
            "请先设置下载器访问密码（DS_SECRET），再启用猫抓发送",
        ));
    }
    if state.inner.lock().unwrap().closing {
        return Err(AppError::bad_request("服务正在关闭"));
    }
    let val = parse_body_value(&body)?;
    let list = cat_catch_tasks(&val)?;
    let total = list.len();
    let mut added = 0usize;
    for item in list {
        let is_pending = {
            let g = state.inner.lock().unwrap();
            g.tasks
                .get(&item.url)
                .is_some_and(|t| t.status == TaskStatus::Pending)
        };
        if is_pending {
            continue;
        }
        if state
            .enqueue(EnqueueTaskInput {
                url: item.url,
                options: item.options,
                browser_capture: false,
                browser_source: None,
            })
            .await?
        {
            added += 1;
        }
    }
    let skipped = total.saturating_sub(added);
    Ok(Json(json!({
        "code": 0,
        "message": format!("已加入 {added} 个下载任务"),
        "data": { "added": added, "skipped": skipped }
    })))
}

async fn handle_download(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let list_val = val
        .get("list")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppError::bad_request("每次请添加 1–100 个下载链接"))?;
    if list_val.is_empty() || list_val.len() > 100 {
        return Err(AppError::bad_request("每次请添加 1–100 个下载链接"));
    }

    let cfg = state.inner.lock().unwrap().cfg.clone();
    let mut parsed_batch = Vec::with_capacity(list_val.len());
    for raw in list_val {
        let obj = raw
            .as_object()
            .ok_or_else(|| AppError::bad_request("无效的下载项"))?;
        let url_raw = obj.get("url").and_then(|v| v.as_str()).unwrap_or("");
        source_url(url_raw)?;

        let mut opts: TaskOptions = serde_json::from_value(raw.clone())?;
        if let Some(ref su) = opts.source_url {
            if !su.trim().is_empty() {
                normalize_download_url(su)?;
            } else {
                opts.source_url = None;
            }
        }
        if let Some(ref hsu) = opts.header_source_url {
            if !hsu.trim().is_empty() {
                normalize_download_url(hsu)?;
            } else {
                opts.header_source_url = None;
            }
        }
        if let Some(ref mut sm) = opts.scrape_meta {
            if !sm.page_url.trim().is_empty() {
                sm.page_url = normalize_http_href(Some(&sm.page_url), "详情页地址", true)?;
            }
        }
        format_headers(opts.headers.as_ref())?;
        let exec = build_exec_options_from(&cfg, &state.cache_dir, &opts);
        format_options(url_raw, &exec)?;

        parsed_batch.push(EnqueueTaskInput {
            url: url_raw.to_string(),
            options: opts,
            browser_capture: false,
            browser_source: None,
        });
    }

    for item in parsed_batch {
        state.enqueue(item).await?;
    }
    Ok(Json(json!({ "code": 0, "message": "已加入下载队列" })))
}

async fn handle_pause(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let all = val.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    let urls = val
        .get("urls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    state.pause_tasks(all, urls).await;
    Ok(Json(
        json!({ "code": 0, "message": "已暂停，分片缓存已保留" }),
    ))
}

async fn handle_resume(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let all = val.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    let urls = val
        .get("urls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    state.resume_tasks(all, urls).await?;
    Ok(Json(json!({ "code": 0, "message": "已继续下载" })))
}

async fn handle_delete(
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let val = parse_body_value(&body)?;
    let urls = val
        .get("urls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let delete_cache = val
        .get("deleteCache")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let delete_video = val
        .get("deleteVideo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    state.delete_tasks(urls, delete_cache, delete_video).await?;
    Ok(Json(json!({ "code": 0, "message": "任务已移除" })))
}

fn percent_encode_rfc5987(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for b in input.as_bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'!'
            | b'#'
            | b'$'
            | b'&'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn ascii_fallback_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' && !c.is_control() {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.trim().is_empty() {
        "download.bin".to_string()
    } else {
        s
    }
}

fn parse_byte_range(header_val: &str, file_len: u64) -> Option<(u64, u64)> {
    let raw = header_val.trim();
    let spec = raw.strip_prefix("bytes=")?.trim();
    if spec.contains(',') || file_len == 0 {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;
    if start_s.is_empty() {
        let suffix = end_s.trim().parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = file_len.saturating_sub(suffix);
        return Some((start, file_len - 1));
    }
    let start = start_s.trim().parse::<u64>().ok()?;
    if start >= file_len {
        return None;
    }
    let end = if end_s.trim().is_empty() {
        file_len - 1
    } else {
        let parsed_end = end_s.trim().parse::<u64>().ok()?;
        if parsed_end < start {
            return None;
        }
        parsed_end.min(file_len - 1)
    };
    Some((start, end))
}

async fn handle_download_file(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    req_headers: HttpHeaderMap,
) -> Result<Response, AppError> {
    let url = params.get("url").map(String::as_str).unwrap_or("");
    let local_video = {
        let g = state.inner.lock().unwrap();
        g.tasks.get(url).and_then(|item| {
            if item.status == TaskStatus::Done {
                item.stats.local_video.clone()
            } else {
                None
            }
        })
    };

    let Some(raw_path) = local_video else {
        return Err(AppError::not_found("文件不存在或尚未完成合并"));
    };
    let abs_path = absolutize(Path::new(&raw_path));
    let meta = match tokio::fs::metadata(&abs_path).await {
        Ok(m) if m.is_file() => m,
        _ => return Err(AppError::not_found("文件不存在或尚未完成合并")),
    };

    let file_len = meta.len();
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("\"{file_len:x}-{mtime_secs:x}\"");
    let filename = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.bin".to_string());
    let mime = mime_guess::from_path(&abs_path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_fallback_filename(&filename),
        percent_encode_rfc5987(&filename)
    );

    if let Some(range_hdr) = req_headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        let Some((start, end)) = parse_byte_range(range_hdr, file_len) else {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{file_len}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return Ok(resp);
        };

        let mut file = tokio::fs::File::open(&abs_path).await?;
        file.seek(SeekFrom::Start(start)).await?;
        let chunk_len = end - start + 1;
        let stream = ReaderStream::new(file.take(chunk_len));
        let mut resp = Response::new(Body::from_stream(stream));
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        let hdrs = resp.headers_mut();
        hdrs.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        if let Ok(v) = HeaderValue::from_str(&mime) {
            hdrs.insert(header::CONTENT_TYPE, v);
        }
        if let Ok(v) = HeaderValue::from_str(&chunk_len.to_string()) {
            hdrs.insert(header::CONTENT_LENGTH, v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{file_len}")) {
            hdrs.insert(header::CONTENT_RANGE, v);
        }
        if let Ok(v) = HeaderValue::from_str(&disposition) {
            hdrs.insert(header::CONTENT_DISPOSITION, v);
        }
        if let Ok(v) = HeaderValue::from_str(&etag) {
            hdrs.insert(header::ETAG, v);
        }
        return Ok(resp);
    }

    let file = tokio::fs::File::open(&abs_path).await?;
    let stream = ReaderStream::new(file);
    let mut resp = Response::new(Body::from_stream(stream));
    let hdrs = resp.headers_mut();
    hdrs.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&mime) {
        hdrs.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&file_len.to_string()) {
        hdrs.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        hdrs.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        hdrs.insert(header::ETAG, v);
    }
    Ok(resp)
}

#[allow(dead_code)]
fn _check_api_response_used<T: Serialize>(_r: ApiResponse<T>) {}
