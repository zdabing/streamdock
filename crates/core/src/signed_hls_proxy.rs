use crate::contract::{HeaderMap, SegmentIdentityFn};
use crate::error::{CoreError, CoreResult};
use crate::http::{build_request_headers, normalize_download_url, open_stream, scope_credentials};
use crate::video_parser::signed_site::normalize_token_path;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap as AxumHeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use futures_util::TryStreamExt;
use reqwest::Method;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use url::Url;

const AUTH_KEYS: &[&str] = &["token", "expires", "token_path", "vid"];
const MAX_PLAYLIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESOURCES: usize = 50_000;

pub type RefreshFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = CoreResult<String>> + Send>> + Send + Sync>;

struct ProxyInner {
    secret: String,
    base_url: OnceLock<String>,
    origin_url: Url,
    dir_prefix: String,
    headers: HeaderMap,
    client: reqwest::Client,
    cancel: CancellationToken,
    refresh: RefreshFn,
    current: Mutex<Url>,
    inflight: Mutex<Option<watch::Receiver<Option<Result<Url, String>>>>>,
    identities: StdMutex<HashMap<String, usize>>,
    resources: StdMutex<Vec<Url>>,
}

pub struct SignedHlsProxy {
    pub entry_url: String,
    base_url: String,
    inner: Arc<ProxyInner>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server_task: Option<tokio::task::JoinHandle<()>>,
}

fn split_bcdn_path_prefix(path: &str) -> (Option<&str>, &str) {
    if let Some(after_lead) = path.strip_prefix("/bcdn_token=") {
        if let Some(slash_rel) = after_lead.find('/') {
            let split_at = "/bcdn_token=".len() + slash_rel;
            return (Some(&path[..split_at]), &path[split_at..]);
        }
    }
    (None, path)
}

fn clean_path_without_bcdn(path: &str) -> &str {
    split_bcdn_path_prefix(path).1
}

fn extract_bcdn_param<'a>(prefix: &'a str, key: &str) -> Option<&'a str> {
    let body = prefix.strip_prefix('/')?;
    for part in body.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == key {
                return Some(v);
            }
        }
    }
    None
}

fn extract_token_path(url: &Url) -> Option<String> {
    let raw = url
        .query_pairs()
        .find(|(k, _)| k == "token_path")
        .map(|(_, v)| v.into_owned())
        .or_else(|| {
            let (prefix, _) = split_bcdn_path_prefix(url.path());
            prefix.and_then(|p| extract_bcdn_param(p, "token_path").map(str::to_string))
        })?;
    let norm = normalize_token_path(&raw);
    if norm.starts_with('/') && norm.ends_with('/') && !norm.contains("..") {
        Some(norm)
    } else {
        None
    }
}

fn compute_asset_root(url: &Url) -> String {
    let clean = clean_path_without_bcdn(url.path());
    if let Some(tp) = extract_token_path(url) {
        if clean.starts_with(&tp) {
            return tp;
        }
        if clean.len() >= 9
            && clean.as_bytes()[0] == b'/'
            && clean.as_bytes()[1..9].iter().all(|b| b.is_ascii_digit())
            && clean[9..].starts_with(&tp)
        {
            return format!("{}{}", &clean[..9], tp);
        }
    }
    let slash_idx = clean.rfind('/').unwrap_or(0);
    clean[..=slash_idx].to_string()
}

fn extract_expires_secs(url: &Url) -> Option<i64> {
    let exp = url
        .query_pairs()
        .find(|(k, _)| k == "expires")
        .map(|(_, v)| v.into_owned())
        .or_else(|| {
            let (prefix, _) = split_bcdn_path_prefix(url.path());
            prefix.and_then(|p| extract_bcdn_param(p, "expires").map(str::to_string))
        })?;
    if exp.len() != 10 || !exp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    exp.parse::<i64>().ok()
}

impl SignedHlsProxy {
    pub async fn start(
        client: reqwest::Client,
        initial_url: &str,
        headers: HeaderMap,
        cancel: CancellationToken,
        refresh: RefreshFn,
    ) -> CoreResult<Self> {
        let parsed = Url::parse(&normalize_download_url(initial_url)?)?;
        let dir_prefix = compute_asset_root(&parsed);
        let secret = uuid::Uuid::new_v4().to_string();

        let inner = Arc::new(ProxyInner {
            secret: secret.clone(),
            base_url: OnceLock::new(),
            origin_url: parsed.clone(),
            dir_prefix,
            headers,
            client,
            cancel,
            refresh,
            current: Mutex::new(parsed.clone()),
            inflight: Mutex::new(None),
            identities: StdMutex::new(HashMap::new()),
            resources: StdMutex::new(Vec::new()),
        });

        let entry_url_inner = inner.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let base_url = format!("http://127.0.0.1:{port}/{secret}");
        let _ = inner.base_url.set(base_url.clone());
        let entry_url = inner.register_url(&base_url, &parsed, &parsed)?;

        let app = Router::new()
            .route(&format!("/{secret}/{{id}}"), get(handle_proxy_request))
            .with_state(entry_url_inner);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });

        Ok(Self {
            entry_url,
            base_url,
            inner,
            shutdown_tx: Some(tx),
            server_task: Some(server_task),
        })
    }

    pub fn identity_fn(&self) -> SegmentIdentityFn {
        let base = self.base_url.clone();
        let inner = self.inner.clone();
        Arc::new(move |url: &str| inner.identity(&base, url))
    }

    pub fn identity(&self, url: &str) -> String {
        self.inner.identity(&self.base_url, url)
    }

    pub async fn close(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.server_task.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for SignedHlsProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl ProxyInner {
    fn is_credential_target(&self, res: &Url) -> bool {
        res.origin() == self.origin_url.origin()
            && clean_path_without_bcdn(res.path()).starts_with(&self.dir_prefix)
    }

    fn strip_auth_from_url(&self, url: &mut Url) {
        if !self.is_credential_target(url) {
            return;
        }
        let clean_path = clean_path_without_bcdn(url.path()).to_string();
        if clean_path != url.path() {
            url.set_path(&clean_path);
        }
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !AUTH_KEYS.contains(&k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            url.set_query(None);
        } else {
            let mut qp = url.query_pairs_mut();
            qp.clear();
            for (k, v) in kept {
                qp.append_pair(&k, &v);
            }
        }
    }

    fn canonicalize_resource(&self, raw: &str, parent: &Url) -> CoreResult<Url> {
        let joined = parent
            .join(raw)
            .map_err(|_| CoreError::msg("无效的 HLS 子资源地址"))?;
        let mut normalized = Url::parse(
            &normalize_download_url(joined.as_str())
                .map_err(|_| CoreError::msg("无效的 HLS 子资源地址"))?,
        )?;
        normalized.set_fragment(None);
        self.strip_auth_from_url(&mut normalized);
        Ok(normalized)
    }

    fn register_url(&self, base_url: &str, raw_url: &Url, _parent: &Url) -> CoreResult<String> {
        let mut normalized = raw_url.clone();
        normalized.set_fragment(None);
        self.strip_auth_from_url(&mut normalized);
        let key = normalized.to_string();
        let mut ids = self.identities.lock().unwrap();
        if let Some(&id) = ids.get(&key) {
            return Ok(format!("{base_url}/{id}"));
        }
        let mut res = self.resources.lock().unwrap();
        if res.len() >= MAX_RESOURCES {
            return Err(CoreError::msg("播放列表资源数超过限制"));
        }
        let id = res.len();
        res.push(normalized);
        ids.insert(key, id);
        Ok(format!("{base_url}/{id}"))
    }

    fn register(&self, base_url: &str, raw: &str, parent: &Url) -> CoreResult<String> {
        let canonical = self.canonicalize_resource(raw, parent)?;
        let key = canonical.to_string();
        let mut ids = self.identities.lock().unwrap();
        if let Some(&id) = ids.get(&key) {
            return Ok(format!("{base_url}/{id}"));
        }
        let mut res = self.resources.lock().unwrap();
        if res.len() >= MAX_RESOURCES {
            return Err(CoreError::msg("播放列表资源数超过限制"));
        }
        let id = res.len();
        res.push(canonical);
        ids.insert(key, id);
        Ok(format!("{base_url}/{id}"))
    }

    fn identity(&self, base_url: &str, url: &str) -> String {
        let prefix = format!("{base_url}/");
        if let Some(rest) = url.strip_prefix(&prefix) {
            if let Ok(id) = rest.parse::<usize>() {
                let res = self.resources.lock().unwrap();
                if let Some(u) = res.get(id) {
                    return u.to_string();
                }
            }
        }
        url.to_string()
    }

    fn sign_url(&self, resource: &Url, current: &Url) -> Url {
        if !self.is_credential_target(resource) {
            return resource.clone();
        }
        let mut out = resource.clone();
        self.strip_auth_from_url(&mut out);
        if let (Some(prefix), _) = split_bcdn_path_prefix(current.path()) {
            let prefixed_path = format!("{prefix}{}", out.path());
            out.set_path(&prefixed_path);
        }
        let mut existing: Vec<(String, String)> = out
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        for (k, v) in current.query_pairs() {
            if AUTH_KEYS.contains(&k.as_ref()) {
                let val = if k == "token_path" {
                    normalize_token_path(&v)
                } else {
                    v.into_owned()
                };
                existing.push((k.into_owned(), val));
            }
        }
        if existing.is_empty() {
            out.set_query(None);
        } else {
            let mut qp = out.query_pairs_mut();
            qp.clear();
            for (k, v) in existing {
                qp.append_pair(&k, &v);
            }
        }
        out
    }

    fn is_expiring_soon(&self, current: &Url) -> bool {
        let Some(secs) = extract_expires_secs(current) else {
            return false;
        };
        secs * 1000 < chrono::Utc::now().timestamp_millis() + 15_000
    }

    fn is_already_expired(&self, current: &Url) -> bool {
        let Some(secs) = extract_expires_secs(current) else {
            return false;
        };
        secs * 1000 <= chrono::Utc::now().timestamp_millis()
    }

    async fn update_credentials(&self) -> CoreResult<Url> {
        let existing_rx = {
            let mut guard = self.inflight.lock().await;
            if let Some(rx) = guard.clone() {
                Some(rx)
            } else {
                let (tx, rx) = watch::channel(None);
                *guard = Some(rx);
                drop(guard);
                let result = async {
                    let refreshed = (self.refresh)().await?;
                    let next = Url::parse(&normalize_download_url(&refreshed)?)?;
                    if next.origin() != self.origin_url.origin()
                        || clean_path_without_bcdn(next.path())
                            != clean_path_without_bcdn(self.origin_url.path())
                    {
                        return Err(CoreError::msg("刷新后的媒体地址已变化，请重新开始下载"));
                    }
                    *self.current.lock().await = next.clone();
                    Ok(next)
                }
                .await;
                let send_val = match &result {
                    Ok(u) => Ok(u.clone()),
                    Err(e) => Err(e.to_string()),
                };
                *self.inflight.lock().await = None;
                let _ = tx.send(Some(send_val));
                return result;
            }
        };

        if let Some(mut rx) = existing_rx {
            loop {
                if let Some(res) = rx.borrow().clone() {
                    return res.map_err(CoreError::msg);
                }
                if rx.changed().await.is_err() {
                    return Err(CoreError::msg("刷新播放凭证失败"));
                }
            }
        }
        unreachable!()
    }
}

async fn handle_proxy_request(
    State(inner): State<Arc<ProxyInner>>,
    AxumPath(id_str): AxumPath<String>,
    req_headers: AxumHeaderMap,
) -> Response {
    let Ok(id) = id_str.parse::<usize>() else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap();
    };
    let resource = {
        let guard = inner.resources.lock().unwrap();
        guard.get(id).cloned()
    };
    let Some(resource) = resource else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap();
    };

    let base_url = inner
        .base_url
        .get()
        .cloned()
        .unwrap_or_else(|| format!("http://127.0.0.1/{}", inner.secret));

    let result: CoreResult<Response> = async {
        let is_cred = inner.is_credential_target(&resource);
        let mut current = inner.current.lock().await.clone();
        if is_cred && inner.is_expiring_soon(&current) {
            match inner.update_credentials().await {
                Ok(next) => current = next,
                Err(e) => {
                    if inner.is_already_expired(&current) {
                        return Err(e);
                    }
                }
            }
        }

        let range_hdr = req_headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let mut upstream = None;
        for attempt in 0..3 {
            let signed = inner.sign_url(&resource, &current);
            let mut scoped =
                scope_credentials(inner.origin_url.as_str(), signed.as_str(), &inner.headers)?;
            if let Some(ref r) = range_hdr {
                scoped.insert("range".to_string(), r.clone());
            }
            let outgoing = build_request_headers(signed.as_str(), &scoped)?;
            let mut final_hdrs = outgoing;
            final_hdrs.insert("accept-encoding".to_string(), "identity".to_string());
            match open_stream(
                &inner.client,
                signed.as_str(),
                &final_hdrs,
                &inner.cancel,
                Method::GET,
                None,
            )
            .await
            {
                Ok(stream) => {
                    upstream = Some(stream);
                    break;
                }
                Err(CoreError::Http(ref http_err))
                    if attempt < 2 && is_cred && matches!(http_err.status, 401 | 403) =>
                {
                    let latest = inner.current.lock().await.clone();
                    if attempt == 0 && latest != current {
                        current = latest;
                    } else {
                        current = inner.update_credentials().await?;
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        let mut stream = upstream.ok_or_else(|| CoreError::msg("媒体请求失败"))?;
        let ct = stream
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_manifest =
            resource.path().to_ascii_lowercase().ends_with(".m3u8") || ct.contains("mpegurl");

        if is_manifest {
            let mut buf = Vec::new();
            while let Some(chunk) = stream.response.chunk().await? {
                if buf.len() + chunk.len() > MAX_PLAYLIST_BYTES {
                    return Err(CoreError::msg("播放列表超过大小限制"));
                }
                buf.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&buf);
            if !text.trim_start().starts_with("#EXTM3U") {
                return Err(CoreError::msg("媒体响应不是有效的 HLS 播放列表"));
            }
            let parent_url = Url::parse(&stream.url)?;
            let mut rewritten_lines = Vec::new();
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    rewritten_lines.push(line.to_string());
                    continue;
                }
                if trimmed.starts_with('#') {
                    let mut out_line = String::with_capacity(line.len());
                    let mut rest = line;
                    while let Some(pos) = rest.find("URI=\"") {
                        out_line.push_str(&rest[..pos + 5]);
                        let after = &rest[pos + 5..];
                        if let Some(end_q) = after.find('"') {
                            let uri_val = &after[..end_q];
                            let proxied = inner.register(&base_url, uri_val, &parent_url)?;
                            out_line.push_str(&proxied);
                            out_line.push('"');
                            rest = &after[end_q + 1..];
                        } else {
                            rest = after;
                            break;
                        }
                    }
                    out_line.push_str(rest);
                    rewritten_lines.push(out_line);
                } else {
                    let proxied = inner.register(&base_url, trimmed, &parent_url)?;
                    rewritten_lines.push(proxied);
                }
            }
            let body_text = rewritten_lines.join("\n");
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                .body(Body::from(body_text))
                .unwrap());
        }

        let mut builder = Response::builder()
            .status(StatusCode::from_u16(stream.status).unwrap_or(StatusCode::OK));
        for key in [
            "content-type",
            "content-length",
            "content-range",
            "accept-ranges",
        ] {
            if let Some(val) = stream.headers.get(key) {
                builder = builder.header(key, val);
            }
        }
        let body_stream = stream
            .response
            .bytes_stream()
            .map_err(std::io::Error::other);
        Ok(builder.body(Body::from_stream(body_stream)).unwrap())
    }
    .await;

    match result {
        Ok(resp) => resp,
        Err(CoreError::Http(http_err)) => Response::builder()
            .status(StatusCode::from_u16(http_err.status).unwrap_or(StatusCode::BAD_GATEWAY))
            .body(Body::from("媒体请求失败，请检查来源页面和播放凭证"))
            .unwrap(),
        Err(_) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("媒体请求失败，请检查来源页面和播放凭证"))
            .unwrap(),
    }
}
