use crate::contract::{HeaderInput, HeaderMap, ProxyMode};
use crate::error::{CoreError, CoreResult, HttpError};
use bytes::Bytes;
use md5::{Digest as _, Md5};
use regex::Regex;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, Method, NoProxy, Proxy, Response};
use sha2::Sha256;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const DEFAULT_CHROME_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/132.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M3u8ByteRange {
    pub length: u64,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub url: String,
    pub no_proxy: String,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: ProxyMode::System,
            url: String::new(),
            no_proxy: String::new(),
        }
    }
}

pub fn md5_hex(value: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn decode_b64_utf8(encoded: &str) -> String {
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    let mut out = Vec::with_capacity(encoded.len() * 3 / 4);
    for b in encoded.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => continue,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn format_byte_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB"];
    let f = bytes as f64;
    let i = ((f.ln() / 1024_f64.ln()).floor() as usize).min(3);
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{:.1} {}", f / 1024_f64.powi(i as i32), units[i])
    }
}

fn merge_no_proxy_lists(user_no_proxy: &str) -> String {
    let mut entries: Vec<String> = Vec::new();
    let mut push_list = |raw: &str| {
        for part in raw.split([',', '\n', '\r']) {
            let item = part.trim();
            if !item.is_empty() && !entries.iter().any(|x| x.eq_ignore_ascii_case(item)) {
                entries.push(item.to_string());
            }
        }
    };
    if let Ok(env_val) = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")) {
        push_list(&env_val);
    }
    push_list(user_no_proxy);
    entries.join(",")
}

fn build_http_client_with(
    proxy: &ProxySettings,
    redirect: reqwest::redirect::Policy,
    timeout: Duration,
) -> CoreResult<Client> {
    let mut builder = Client::builder()
        .redirect(redirect)
        .connect_timeout(timeout)
        .read_timeout(timeout);

    let clean_no_proxy: String = proxy
        .no_proxy
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(",");

    match proxy.mode {
        ProxyMode::Disabled => {
            builder = builder.no_proxy();
        }
        ProxyMode::Custom => {
            let trimmed = proxy.url.trim();
            if !trimmed.is_empty() {
                let mut p = Proxy::all(trimmed)
                    .map_err(|e| CoreError::msg(format!("无效代理地址: {e}")))?;
                if !clean_no_proxy.is_empty() {
                    p = p.no_proxy(NoProxy::from_string(&clean_no_proxy));
                }
                builder = builder.proxy(p);
            } else {
                builder = builder.no_proxy();
            }
        }
        ProxyMode::System => {
            if !clean_no_proxy.is_empty() {
                let merged = merge_no_proxy_lists(&clean_no_proxy);
                let http_proxy = std::env::var("HTTP_PROXY")
                    .or_else(|_| std::env::var("http_proxy"))
                    .ok();
                let https_proxy = std::env::var("HTTPS_PROXY")
                    .or_else(|_| std::env::var("https_proxy"))
                    .ok();
                let all_proxy = std::env::var("ALL_PROXY")
                    .or_else(|_| std::env::var("all_proxy"))
                    .ok();
                let mut applied = false;
                if let Some(val) = http_proxy.filter(|s| !s.trim().is_empty()) {
                    if let Ok(p) = Proxy::http(val.trim()) {
                        builder = builder.proxy(p.no_proxy(NoProxy::from_string(&merged)));
                        applied = true;
                    }
                }
                if let Some(val) = https_proxy.filter(|s| !s.trim().is_empty()) {
                    if let Ok(p) = Proxy::https(val.trim()) {
                        builder = builder.proxy(p.no_proxy(NoProxy::from_string(&merged)));
                        applied = true;
                    }
                }
                if let Some(val) = all_proxy.filter(|s| !s.trim().is_empty()) {
                    if let Ok(p) = Proxy::all(val.trim()) {
                        builder = builder.proxy(p.no_proxy(NoProxy::from_string(&merged)));
                        applied = true;
                    }
                }
                if !applied {
                    builder = builder.no_proxy();
                }
            }
        }
    }

    Ok(builder.build()?)
}

pub fn build_http_client(proxy: &ProxySettings) -> CoreResult<Client> {
    build_http_client_with(
        proxy,
        reqwest::redirect::Policy::none(),
        Duration::from_secs(30),
    )
}

pub fn build_redirect_http_client(proxy: &ProxySettings) -> CoreResult<Client> {
    build_http_client_with(
        proxy,
        reqwest::redirect::Policy::limited(10),
        Duration::from_secs(20),
    )
}

fn global_clients_lock() -> &'static RwLock<(Client, Client)> {
    static GLOBAL_CLIENTS: OnceLock<RwLock<(Client, Client)>> = OnceLock::new();
    GLOBAL_CLIENTS.get_or_init(|| {
        let def = ProxySettings::default();
        let direct = build_http_client(&def).expect("default global reqwest client build");
        let redirect =
            build_redirect_http_client(&def).expect("default global redirect reqwest client build");
        RwLock::new((direct, redirect))
    })
}

pub fn init_global_proxy(proxy: &ProxySettings) -> CoreResult<Client> {
    let direct = build_http_client(proxy)?;
    let redirect = build_redirect_http_client(proxy)?;
    let mut guard = global_clients_lock().write().unwrap();
    *guard = (direct.clone(), redirect);
    Ok(direct)
}

pub fn global_http_client() -> Client {
    global_clients_lock().read().unwrap().0.clone()
}

pub fn global_redirect_http_client() -> Client {
    global_clients_lock().read().unwrap().1.clone()
}

pub fn default_http_client() -> &'static Client {
    static DEFAULT_CLIENT: OnceLock<Client> = OnceLock::new();
    DEFAULT_CLIENT.get_or_init(|| {
        build_http_client(&ProxySettings::default()).expect("default reqwest client build")
    })
}

pub fn normalize_download_url(value: &str) -> CoreResult<String> {
    static UNESCAPE_RX: OnceLock<Regex> = OnceLock::new();
    let rx = UNESCAPE_RX.get_or_init(|| Regex::new(r"\\([_,])").unwrap());
    let clean = rx.replace_all(value.trim(), "$1");
    let lower = clean.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(CoreError::msg(
            "请输入以 http:// 或 https:// 开头的下载链接",
        ));
    }
    let parsed =
        Url::parse(&clean).map_err(|_| CoreError::msg("请输入有效的 http:// 或 https:// 链接"))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CoreError::msg("请通过请求头填写认证信息"));
    }
    Ok(parsed.to_string())
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            matches!(
                b,
                b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^'
                | b'_' | b'`' | b'|' | b'~' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z'
            )
        })
}

pub fn is_valid_header_value_str(value: &str) -> bool {
    !value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
        && HeaderValue::from_str(value).is_ok()
}

fn push_normalized_entry(
    out: &mut HeaderMap,
    key: &str,
    value: &serde_json::Value,
) -> CoreResult<()> {
    let name = key.trim().to_ascii_lowercase();
    if name.starts_with(':')
        || matches!(
            name.as_str(),
            "host" | "content-length" | "connection" | "accept-encoding"
        )
    {
        return Ok(());
    }
    if !is_valid_header_name(&name) {
        return Err(CoreError::msg(format!("无效的请求头: {name}")));
    }
    let val_str = match value {
        serde_json::Value::Null => return Ok(()),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Array(arr) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            parts.join(", ")
        }
        serde_json::Value::Object(_) => {
            return Err(CoreError::msg(format!("无效的请求头: {name}")));
        }
    };
    if val_str.contains('\r') || val_str.contains('\n') {
        return Err(CoreError::msg(format!("无效的请求头: {name}")));
    }
    out.insert(name, val_str);
    Ok(())
}

pub fn format_headers_str(input: &str) -> CoreResult<HeaderMap> {
    let text = input.trim();
    if text.is_empty() {
        return Ok(HeaderMap::new());
    }
    if text.starts_with('{') {
        let val: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| CoreError::msg(format!("请求头 JSON 解析失败: {e}")))?;
        return format_headers_json(&val);
    }
    static INLINE_SPLIT_RX: OnceLock<Regex> = OnceLock::new();
    let split_rx = INLINE_SPLIT_RX.get_or_init(|| Regex::new(r",\s*([a-zA-Z0-9_-]+:)").unwrap());
    let expanded = split_rx.replace_all(text, "\n$1");
    let mut out = HeaderMap::new();
    for line in expanded.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some(idx) = line.find(':') else {
            return Err(CoreError::msg(
                "请求头请使用「名称: 值」格式，每行一项，或填写 JSON 对象",
            ));
        };
        if idx < 1 {
            return Err(CoreError::msg(
                "请求头请使用「名称: 值」格式，每行一项，或填写 JSON 对象",
            ));
        }
        let k = line[..idx].trim();
        let v = serde_json::Value::String(line[idx + 1..].trim().to_string());
        push_normalized_entry(&mut out, k, &v)?;
    }
    Ok(out)
}

pub fn format_headers_json(val: &serde_json::Value) -> CoreResult<HeaderMap> {
    let Some(obj) = val.as_object() else {
        return Err(CoreError::msg("请求头必须是对象"));
    };
    let mut out = HeaderMap::new();
    for (k, v) in obj {
        push_normalized_entry(&mut out, k, v)?;
    }
    Ok(out)
}

pub fn format_headers(input: Option<&HeaderInput>) -> CoreResult<HeaderMap> {
    let Some(input) = input else {
        return Ok(HeaderMap::new());
    };
    match input {
        HeaderInput::Text(s) => format_headers_str(s),
        HeaderInput::Map(map) => {
            let mut out = HeaderMap::new();
            for (k, v) in map {
                push_normalized_entry(&mut out, k, v)?;
            }
            Ok(out)
        }
    }
}

pub fn merge_header_maps(base: &HeaderMap, over: &HeaderMap) -> HeaderMap {
    let mut merged = base.clone();
    for (k, v) in over {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

pub fn build_request_headers(url: &str, custom: &HeaderMap) -> CoreResult<HeaderMap> {
    let parsed = Url::parse(url)?;
    let mut headers = HeaderMap::new();
    headers.insert("user-agent".to_string(), DEFAULT_CHROME_UA.to_string());
    headers.insert("accept".to_string(), "*/*".to_string());
    headers.insert(
        "referer".to_string(),
        format!("{}/", parsed.origin().ascii_serialization()),
    );
    for (k, v) in custom {
        headers.insert(k.clone(), v.clone());
    }
    if headers.get("referer").is_some_and(|v| v.is_empty()) {
        headers.remove("referer");
    }
    Ok(headers)
}

pub fn scope_credentials(source: &str, target: &str, input: &HeaderMap) -> CoreResult<HeaderMap> {
    let mut headers = input.clone();
    let s = Url::parse(source)?;
    let t = Url::parse(target)?;
    if s.origin() != t.origin() {
        headers.remove("cookie");
        headers.remove("authorization");
    }
    Ok(headers)
}

pub fn headers_for_target(source: &str, target: &str, input: &HeaderMap) -> CoreResult<HeaderMap> {
    let mut headers = scope_credentials(source, target, input)?;
    if !headers.contains_key("referer") {
        headers.insert("referer".to_string(), source.to_string());
    }
    Ok(headers)
}

pub fn with_range_header(headers: &HeaderMap, range: Option<M3u8ByteRange>) -> HeaderMap {
    let Some(r) = range else {
        return headers.clone();
    };
    if r.length == 0 {
        return headers.clone();
    }
    let mut out = headers.clone();
    let offset = r.offset.unwrap_or(0);
    out.insert(
        "range".to_string(),
        format!("bytes={offset}-{}", offset + r.length - 1),
    );
    out
}

pub fn slice_byterange(buffer: Bytes, range: Option<M3u8ByteRange>, status: u16) -> Bytes {
    let Some(r) = range else {
        return buffer;
    };
    if r.length == 0 || status == 206 {
        return buffer;
    }
    let offset = (r.offset.unwrap_or(0) as usize).min(buffer.len());
    let end = offset.saturating_add(r.length as usize).min(buffer.len());
    buffer.slice(offset..end)
}

pub struct StreamResponse {
    pub url: String,
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub response: Response,
}

#[derive(Debug, Clone)]
pub struct BufferedResponse {
    pub url: String,
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Bytes,
}

impl BufferedResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn to_reqwest_headers(map: &HeaderMap) -> CoreResult<reqwest::header::HeaderMap> {
    let mut out = reqwest::header::HeaderMap::new();
    for (k, v) in map {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| CoreError::msg(format!("无效的请求头: {k}")))?;
        let val =
            HeaderValue::from_str(v).map_err(|_| CoreError::msg(format!("无效的请求头: {k}")))?;
        out.insert(name, val);
    }
    Ok(out)
}

pub async fn open_stream(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    cancel: &CancellationToken,
    method: Method,
    body: Option<String>,
) -> CoreResult<StreamResponse> {
    let mut current_url = url.to_string();
    let mut current_headers = headers.clone();
    let mut current_method = method;
    let mut current_body = body;

    for redirects in 0..=5 {
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let parsed = Url::parse(&current_url)?;
        let req_headers = to_reqwest_headers(&current_headers)?;
        let mut req = client
            .request(current_method.clone(), &current_url)
            .headers(req_headers);
        if let Some(ref b) = current_body {
            req = req.body(b.clone());
        }

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(CoreError::aborted()),
            res = req.send() => res.map_err(|e| {
                if e.is_timeout() {
                    CoreError::msg("请求超过 30 秒未响应")
                } else {
                    CoreError::Reqwest(e)
                }
            })?,
        };

        let status = response.status().as_u16();
        let challenge = response
            .headers()
            .get("cf-mitigated")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("challenge"));
        if challenge {
            drop(response);
            return Err(HttpError::new(status, &current_url, true).into());
        }

        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if let Some(loc) = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
            {
                if redirects >= 5 {
                    return Err(CoreError::msg("重定向次数过多"));
                }
                let joined = parsed
                    .join(loc)
                    .map_err(|_| CoreError::msg("重定向地址无效"))?;
                let next_url = normalize_download_url(joined.as_str())?;
                let next_parsed = Url::parse(&next_url)?;
                if next_parsed.origin() != parsed.origin() {
                    current_headers.remove("cookie");
                    current_headers.remove("authorization");
                }
                let switch_get = status == 303
                    || (current_method == Method::POST && (status == 301 || status == 302));
                if switch_get {
                    current_method = Method::GET;
                    current_headers.remove("content-type");
                    current_headers.remove("content-length");
                    current_body = None;
                }
                current_url = next_url;
                continue;
            }
        }

        if !(200..300).contains(&status) {
            drop(response);
            return Err(HttpError::new(status, &current_url, false).into());
        }

        let resp_headers = response.headers().clone();
        return Ok(StreamResponse {
            url: current_url,
            status,
            headers: resp_headers,
            response,
        });
    }

    Err(CoreError::msg("重定向次数过多"))
}

pub async fn get_response(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    cancel: &CancellationToken,
    post_body: Option<String>,
) -> CoreResult<StreamResponse> {
    let normalized = normalize_download_url(url)?;
    let mut outgoing = build_request_headers(&normalized, headers)?;
    outgoing.insert("accept-encoding".to_string(), "identity".to_string());
    let method = if post_body.is_some() {
        Method::POST
    } else {
        Method::GET
    };
    open_stream(client, &normalized, &outgoing, cancel, method, post_body).await
}

pub async fn request_once<'a, 'b>(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    cancel: &CancellationToken,
    mut progress: Option<&'a mut (dyn FnMut(u64) + Send + 'b)>,
) -> CoreResult<BufferedResponse> {
    const MAX_SEGMENT_BYTES: usize = 128 * 1024 * 1024;
    let mut outgoing = headers.clone();
    outgoing.insert("accept-encoding".to_string(), "identity".to_string());
    let mut stream = open_stream(client, url, &outgoing, cancel, Method::GET, None).await?;
    let mut buf = Vec::new();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(CoreError::aborted()),
            chunk = stream.response.chunk() => chunk,
        };
        match next {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_SEGMENT_BYTES {
                    return Err(CoreError::msg("单个分片超过 128 MB 限制"));
                }
                buf.extend_from_slice(&chunk);
                if let Some(cb) = progress.as_deref_mut() {
                    cb(buf.len() as u64);
                }
            }
            Ok(None) => break,
            Err(e) => {
                if cancel.is_cancelled() {
                    return Err(CoreError::aborted());
                }
                if e.is_timeout() {
                    return Err(CoreError::msg("请求超过 30 秒未响应"));
                }
                if e.is_body() || e.is_decode() {
                    return Err(CoreError::msg("远端提前关闭响应，请重试"));
                }
                return Err(CoreError::Reqwest(e));
            }
        }
    }
    Ok(BufferedResponse {
        url: stream.url,
        status: stream.status,
        headers: stream.headers,
        body: Bytes::from(buf),
    })
}

pub async fn get_retry<'a, 'b>(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    retries: usize,
    cancel: &CancellationToken,
    mut progress: Option<&'a mut (dyn FnMut(u64) + Send + 'b)>,
) -> CoreResult<BufferedResponse> {
    let normalized = normalize_download_url(url)?;
    let max_attempts = retries.max(1);
    for attempt in 0..max_attempts {
        if let Some(cb) = progress.as_deref_mut() {
            cb(0);
        }
        let req_headers = build_request_headers(&normalized, headers)?;
        let res = request_once(
            client,
            &normalized,
            &req_headers,
            cancel,
            progress.as_deref_mut(),
        )
        .await;
        match res {
            Ok(ok) => return Ok(ok),
            Err(err) => {
                if cancel.is_cancelled() || err.is_aborted() {
                    return Err(CoreError::aborted());
                }
                if attempt >= max_attempts - 1 || !err.is_retryable_http() {
                    return Err(err);
                }
                let backoff_ms = (500u64 << attempt.min(10)).min(3000);
                tokio::select! {
                    _ = cancel.cancelled() => return Err(CoreError::aborted()),
                    _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
                }
            }
        }
    }
    unreachable!()
}
