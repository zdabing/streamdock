use crate::contract::{ApiResponse, HeaderMap, VideoInfo};
use crate::error::{CoreError, CoreResult};
use crate::extractor::decode_page_text;
use crate::format_options::percent_decode_lossy;
use crate::http::{decode_b64_utf8, normalize_download_url};
use crate::video_parser::request::{PageRequest, ParserContext, read_page};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use url::Url;

#[derive(Debug, Clone)]
pub struct SignedSiteTokenSession {
    pub origin: String,
    pub player_url: String,
    pub base_signed_url: String,
    pub session_id: String,
    pub movie_id: String,
    pub guid_id: String,
    pub api_secret: String,
    pub api_secret_issued_at: Option<serde_json::Value>,
    pub time_diff_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SignedSiteParseDetail {
    pub info: VideoInfo,
    pub is_signed: bool,
    pub session: Option<SignedSiteTokenSession>,
}

#[derive(Debug, Default)]
struct CreatePlayerConfig {
    cloud_type: Option<i64>,
    m3u8_url: Option<String>,
    initial_signed_url: Option<String>,
    signed_url: Option<String>,
    initial_token: Option<String>,
    initial_expires: Option<String>,
    initial_token_path: Option<String>,
    movie_id: Option<String>,
    session_id: Option<String>,
    guid_id: Option<String>,
    api_secret: Option<String>,
    api_secret_issued_at: Option<serde_json::Value>,
    time_diff_ms: i64,
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK_LEN: usize = 64;
    let mut k_block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        let mut h = Sha256::new();
        h.update(key);
        let digest = h.finalize();
        k_block[..32].copy_from_slice(&digest);
    } else {
        k_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_LEN];
    let mut opad = [0x5cu8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        ipad[i] ^= k_block[i];
        opad[i] ^= k_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn normalize_token_path(raw: &str) -> String {
    let mut out = raw.trim().to_string();
    for _ in 0..2 {
        let needs_decode = out.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("%2f"))
            || out
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("%252f"));
        if needs_decode {
            out = percent_decode_lossy(&out);
        } else {
            break;
        }
    }
    out
}

fn normalize_query_token_path(url_str: &str) -> CoreResult<String> {
    let mut u = Url::parse(&normalize_download_url(url_str)?)?;
    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .map(|(k, v)| {
            if k == "token_path" {
                (k.into_owned(), normalize_token_path(&v))
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();
    if !pairs.is_empty() {
        let mut qp = u.query_pairs_mut();
        qp.clear();
        for (k, v) in pairs {
            qp.append_pair(&k, &v);
        }
    }
    Ok(u.to_string())
}

fn apply_query_token(
    base_url: &str,
    token: &str,
    expires: &str,
    token_path: &str,
) -> CoreResult<String> {
    let mut u = Url::parse(&normalize_download_url(base_url)?)?;
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !matches!(k.as_ref(), "token" | "expires" | "token_path"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut qp = u.query_pairs_mut();
        qp.clear();
        for (k, v) in kept {
            qp.append_pair(&k, &v);
        }
        qp.append_pair("token", token.trim());
        if !expires.trim().is_empty() {
            qp.append_pair("expires", expires.trim());
        }
        let norm_tp = normalize_token_path(token_path);
        if !norm_tp.is_empty() {
            qp.append_pair("token_path", &norm_tp);
        }
    }
    Ok(u.to_string())
}

fn is_allowed_signed_cdn_host(host: &str) -> bool {
    static HOSTS: LazyLock<(String, String)> = LazyLock::new(|| {
        (
            decode_b64_utf8("YXZraW5nLnh5eg=="),
            decode_b64_utf8("Yi1jZG4ubmV0"),
        )
    });
    let (primary, alt) = (&HOSTS.0, &HOSTS.1);
    let h = host.to_ascii_lowercase();
    h == *primary
        || h.ends_with(&format!(".{primary}"))
        || h == *alt
        || h.ends_with(&format!(".{alt}"))
}

fn find_create_player_object(html: &str) -> Option<&str> {
    static CALL_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bcreatePlayer\s*\(\s*\{").unwrap());
    let m = CALL_RX.find(html)?;
    let open_rel = html[m.start()..m.end()].rfind('{')?;
    let start = m.start() + open_rel;
    let rest = &html[start..];

    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in rest.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&rest[..=idx]);
                }
            }
            _ => {}
        }
    }
    None
}

fn unescape_js_str(raw: &str) -> String {
    decode_page_text(raw).replace(r"\/", "/")
}

fn json_value_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(unescape_js_str(s)),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn json_value_to_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn extract_js_field_scalar(block: &str, name: &str) -> Option<serde_json::Value> {
    let pattern = format!(
        r#"(?m)(?:^|[{{,\s])(?:["']?{}["']?)\s*:\s*(?:"([^"\r\n\\]*(?:\\.[^"\r\n\\]*)*)"|'([^'\r\n\\]*(?:\\.[^'\r\n\\]*)*)'|(-?\d+(?:\.\d+)?))"#,
        regex::escape(name)
    );
    let rx = Regex::new(&pattern).ok()?;
    let caps = rx.captures(block)?;
    if let Some(dq) = caps.get(1) {
        return Some(serde_json::Value::String(unescape_js_str(dq.as_str())));
    }
    if let Some(sq) = caps.get(2) {
        return Some(serde_json::Value::String(unescape_js_str(sq.as_str())));
    }
    if let Some(num) = caps.get(3) {
        let raw = num.as_str();
        if let Ok(i) = raw.parse::<i64>() {
            return Some(serde_json::Value::Number(i.into()));
        }
        if let Ok(f) = raw.parse::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return Some(serde_json::Value::Number(n));
            }
        }
        return Some(serde_json::Value::String(raw.to_string()));
    }
    None
}

fn extract_create_player_config(html: &str) -> Option<CreatePlayerConfig> {
    let block = find_create_player_object(html)?;
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(block) {
        let get_str = |k: &str| map.get(k).and_then(json_value_to_string);
        return Some(CreatePlayerConfig {
            cloud_type: map.get("cloudType").and_then(json_value_to_i64),
            m3u8_url: get_str("m3u8Url"),
            initial_signed_url: get_str("initialSignedUrl"),
            signed_url: get_str("signedUrl"),
            initial_token: get_str("initialToken"),
            initial_expires: get_str("initialExpires"),
            initial_token_path: get_str("initialTokenPath"),
            movie_id: get_str("movieId"),
            session_id: get_str("sessionId"),
            guid_id: get_str("guidId"),
            api_secret: get_str("apiSecret"),
            api_secret_issued_at: map
                .get("apiSecretIssuedAt")
                .and_then(|v| if v.is_null() { None } else { Some(v.clone()) }),
            time_diff_ms: map.get("timeDiff").and_then(json_value_to_i64).unwrap_or(0),
        });
    }

    let get_str = |k: &str| {
        extract_js_field_scalar(block, k)
            .as_ref()
            .and_then(json_value_to_string)
    };
    Some(CreatePlayerConfig {
        cloud_type: extract_js_field_scalar(block, "cloudType")
            .as_ref()
            .and_then(json_value_to_i64),
        m3u8_url: get_str("m3u8Url"),
        initial_signed_url: get_str("initialSignedUrl"),
        signed_url: get_str("signedUrl"),
        initial_token: get_str("initialToken"),
        initial_expires: get_str("initialExpires"),
        initial_token_path: get_str("initialTokenPath"),
        movie_id: get_str("movieId"),
        session_id: get_str("sessionId"),
        guid_id: get_str("guidId"),
        api_secret: get_str("apiSecret"),
        api_secret_issued_at: extract_js_field_scalar(block, "apiSecretIssuedAt"),
        time_diff_ms: extract_js_field_scalar(block, "timeDiff")
            .as_ref()
            .and_then(json_value_to_i64)
            .unwrap_or(0),
    })
}

fn adjusted_now_ms(time_diff_ms: i64) -> i64 {
    let now = chrono::Utc::now().timestamp_millis();
    if time_diff_ms.abs() > 20_000 {
        now - time_diff_ms
    } else {
        now
    }
}

pub async fn refresh_token_session(
    session: &SignedSiteTokenSession,
    headers: &HeaderMap,
    ctx: &ParserContext,
) -> CoreResult<String> {
    let now_ms = adjusted_now_ms(session.time_diff_ms);
    let timestamp = now_ms / 1000;
    let sig_input = format!("{}|{}|{}", session.movie_id, timestamp, session.session_id);
    let signature = base64_encode(&hmac_sha256(
        session.api_secret.as_bytes(),
        sig_input.as_bytes(),
    ));

    let mut payload = serde_json::Map::new();
    payload.insert(
        "sessionId".to_string(),
        serde_json::Value::String(session.session_id.clone()),
    );
    payload.insert(
        "movieId".to_string(),
        serde_json::Value::String(session.movie_id.clone()),
    );
    if !session.guid_id.is_empty() {
        payload.insert(
            "guidId".to_string(),
            serde_json::Value::String(session.guid_id.clone()),
        );
    }
    if let Some(ref issued_at) = session.api_secret_issued_at {
        payload.insert("apiSecretIssuedAt".to_string(), issued_at.clone());
    }
    payload.insert("timestamp".to_string(), serde_json::json!(timestamp));
    payload.insert(
        "url".to_string(),
        serde_json::Value::String(session.player_url.clone()),
    );
    payload.insert(
        "signature".to_string(),
        serde_json::Value::String(signature),
    );

    let endpoint = format!(
        "{}/api/token/generate",
        session.origin.trim_end_matches('/')
    );
    let mut api_headers = headers.clone();
    api_headers.insert("referer".to_string(), session.player_url.clone());
    api_headers.insert("origin".to_string(), session.origin.clone());
    api_headers.insert("content-type".to_string(), "application/json".to_string());
    api_headers.insert("accept".to_string(), "application/json".to_string());

    let issued = read_page(
        ctx,
        &endpoint,
        &PageRequest {
            headers: api_headers,
            method_post_body: Some(serde_json::Value::Object(payload).to_string()),
            ..Default::default()
        },
    )
    .await?;

    let parsed: serde_json::Value = serde_json::from_str(&issued.text)
        .map_err(|e| CoreError::msg(format!("刷新播放凭证响应解析失败: {e}")))?;
    let data = parsed.get("data").unwrap_or(&parsed);
    let token = data["token"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::msg("站点未返回有效的刷新 token"))?;
    let token_path = data["tokenPath"].as_str().unwrap_or("");
    let expires = json_value_to_string(&data["expires"]).unwrap_or_default();
    if expires.trim().is_empty() {
        return Err(CoreError::msg("站点未返回刷新 token 有效期"));
    }

    apply_query_token(&session.base_signed_url, token, &expires, token_path)
}

pub async fn parse_signed_site_detail(
    url: &str,
    headers: &HeaderMap,
    ctx: &ParserContext,
) -> CoreResult<ApiResponse<SignedSiteParseDetail>> {
    let source = Url::parse(url)?;
    let source_origin = source.origin();
    let mut outgoing = HeaderMap::new();
    outgoing.insert(
        "referer".to_string(),
        format!("{}/", source_origin.ascii_serialization()),
    );
    for (k, v) in headers {
        outgoing.insert(k.clone(), v.clone());
    }

    let res: CoreResult<SignedSiteParseDetail> = async {
        let page = read_page(
            ctx,
            url,
            &PageRequest {
                headers: outgoing.clone(),
                ..Default::default()
            },
        )
        .await?;

        static TITLE_RX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>([^<]*)</title>").unwrap());
        static IFRAME_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)<iframe\b[^>]*\bsrc=["']([^"']*?/player/[^"']*)["']"#).unwrap()
        });
        static SIGNED_M3U8_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)https?://[^\s"'<>\\]+\.m3u8\?[^\s"'<>\\]+"#).unwrap()
        });
        static BCDN_PATH_M3U8_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r#"(?i)https?://[^\s"'<>\\]+/bcdn_token=[^\s"'<>\\]+\.m3u8(?:[^\s"'<>\\]*)?"#,
            )
            .unwrap()
        });
        static BCDN_TOKEN_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"bcdn_token=[a-zA-Z0-9\-_]+&expires=\d+&token_path=(%2F[^&"'\s\\]+%2F)"#)
                .unwrap()
        });
        static CDN_HOST_RX: LazyLock<Regex> = LazyLock::new(|| {
            let alt = regex::escape(&decode_b64_utf8("Yi1jZG4ubmV0"));
            let brand = regex::escape(&decode_b64_utf8("YXZraW5n"));
            let pat = format!(r#"(?i)https?://[a-zA-Z0-9._-]*(?:{alt}|{brand}\.[a-z]+)"#);
            Regex::new(&pat).unwrap()
        });
        static CONTENT_URL_RX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"(?i)"contentUrl"\s*:\s*"([^"\r\n]+)""#).unwrap());

        let title = decode_page_text(
            TITLE_RX
                .captures(&page.text)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
                .unwrap_or(""),
        )
        .trim()
        .to_string();

        let page_u = Url::parse(&page.url)?;
        let iframe_src = IFRAME_RX
            .captures(&page.text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str());

        let player_url = if let Some(src) = iframe_src {
            let joined = page_u.join(&decode_page_text(src))?;
            normalize_download_url(joined.as_str())?
        } else {
            page.url.clone()
        };

        let player_u = Url::parse(&player_url)?;
        if player_u.origin() != source_origin && player_u.origin() != page_u.origin() {
            return Err(CoreError::msg("播放器来源已变化，请重新检查站点结构"));
        }

        let player = if iframe_src.is_some() {
            let mut ph = outgoing.clone();
            ph.insert("referer".to_string(), page.url.clone());
            read_page(
                ctx,
                &player_url,
                &PageRequest {
                    headers: ph,
                    ..Default::default()
                },
            )
            .await?
        } else {
            page.clone()
        };

        let mut session: Option<SignedSiteTokenSession> = None;
        let mut is_signed = true;

        let media = if let Some(cfg) = extract_create_player_config(&player.text) {
            if cfg.cloud_type == Some(1) {
                let raw_m3u8 = cfg
                    .m3u8_url
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| CoreError::msg("普通存储源未包含播放地址"))?;
                let joined = player_u.join(raw_m3u8.trim())?;
                is_signed = false;
                normalize_download_url(joined.as_str())?
            } else if let Some(raw_base) = cfg
                .initial_signed_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| cfg.signed_url.as_deref().filter(|s| !s.trim().is_empty()))
                .or_else(|| cfg.m3u8_url.as_deref().filter(|s| !s.trim().is_empty()))
            {
                let base_joined = player_u.join(raw_base.trim())?;
                let base_signed_url = normalize_download_url(base_joined.as_str())?;
                let init_token = cfg.initial_token.clone().unwrap_or_default();
                let init_expires = cfg.initial_expires.clone().unwrap_or_default();
                let init_tp = cfg.initial_token_path.clone().unwrap_or_default();

                let initial_url = if !init_token.trim().is_empty() {
                    apply_query_token(&base_signed_url, &init_token, &init_expires, &init_tp)?
                } else {
                    normalize_query_token_path(&base_signed_url)?
                };

                let movie_id = cfg.movie_id.unwrap_or_default();
                let session_id = cfg.session_id.unwrap_or_default();
                let api_secret = cfg.api_secret.unwrap_or_default();
                if !movie_id.trim().is_empty()
                    && !session_id.trim().is_empty()
                    && !api_secret.trim().is_empty()
                {
                    session = Some(SignedSiteTokenSession {
                        origin: player_u.origin().ascii_serialization(),
                        player_url: player_url.clone(),
                        base_signed_url,
                        session_id,
                        movie_id,
                        guid_id: cfg.guid_id.unwrap_or_default(),
                        api_secret,
                        api_secret_issued_at: cfg.api_secret_issued_at,
                        time_diff_ms: cfg.time_diff_ms,
                    });
                }

                let exp_secs = init_expires.trim().parse::<i64>().ok();
                let now_ms = adjusted_now_ms(cfg.time_diff_ms);
                let expiring_soon = exp_secs.is_some_and(|s| s * 1000 < now_ms + 12_000);
                let still_unexpired = exp_secs.is_some_and(|s| s * 1000 > now_ms);

                if (expiring_soon || init_token.trim().is_empty()) && session.is_some() {
                    match refresh_token_session(session.as_ref().unwrap(), &outgoing, ctx).await {
                        Ok(refreshed) => refreshed,
                        Err(_) if !init_token.trim().is_empty() && still_unexpired => initial_url,
                        Err(e) => return Err(e),
                    }
                } else {
                    initial_url
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let media = if !media.is_empty() {
            media
        } else {
            let decoded_player = decode_page_text(&player.text);
            if let Some(m) = SIGNED_M3U8_RX.find(&decoded_player) {
                normalize_query_token_path(m.as_str())?
            } else if let Some(m) = BCDN_PATH_M3U8_RX.find(&decoded_player) {
                normalize_download_url(m.as_str())?
            } else if let Some(caps) = BCDN_TOKEN_RX.captures(&decoded_player) {
                let token_str = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                let uuid_path = percent_decode_lossy(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
                static DEFAULT_CDN_ORIGIN: LazyLock<String> =
                    LazyLock::new(|| decode_b64_utf8("aHR0cHM6Ly92aWRlb2Nkbi5hdmtpbmcueHl6"));
                let host = CDN_HOST_RX
                    .find(&decoded_player)
                    .map(|m| m.as_str().trim_end_matches('/'))
                    .unwrap_or(DEFAULT_CDN_ORIGIN.as_str());
                format!("{host}/{token_str}{uuid_path}playlist.m3u8")
            } else {
                let raw = CONTENT_URL_RX
                    .captures(&page.text)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str())
                    .ok_or_else(|| {
                        CoreError::msg("未找到播放地址，页面可能需要运行播放器脚本或完成验证")
                    })?;
                let bare = normalize_download_url(&decode_page_text(raw))?;
                let token_api = format!(
                    "{}/api/bunnytoken/generate",
                    player_u.origin().ascii_serialization()
                );
                let mut api_headers = outgoing.clone();
                api_headers.insert("content-type".to_string(), "application/json".to_string());
                api_headers.insert("accept".to_string(), "application/json".to_string());
                let issued = read_page(
                    ctx,
                    &token_api,
                    &PageRequest {
                        headers: api_headers,
                        method_post_body: Some(serde_json::json!({ "m3u8Url": bare }).to_string()),
                        ..Default::default()
                    },
                )
                .await?;
                let parsed_json: serde_json::Value = serde_json::from_str(&issued.text)?;
                let ok = parsed_json["success"].as_bool().unwrap_or(false);
                let signed = parsed_json["data"]["signedUrl"].as_str().unwrap_or("");
                if !ok || signed.is_empty() {
                    return Err(CoreError::msg("站点未返回有效播放凭证"));
                }
                signed.to_string()
            }
        };

        let target = Url::parse(&normalize_download_url(&media)?)?;
        if is_signed {
            let has_query_token = target
                .query_pairs()
                .any(|(k, v)| k == "token" && !v.is_empty());
            let has_path_token = target.path().contains("/bcdn_token=");
            if !has_query_token && !has_path_token {
                return Err(CoreError::msg("站点未返回带 token 的播放地址"));
            }
        }
        let host = target.host_str().unwrap_or("");
        if !is_allowed_signed_cdn_host(host) {
            return Err(CoreError::msg("媒体来源已变化，请重新检查站点结构"));
        }

        Ok(SignedSiteParseDetail {
            info: VideoInfo {
                url: target.to_string(),
                title,
                referer: Some(player_url),
                platform: Some("signed_site".to_string()),
                ..Default::default()
            },
            is_signed,
            session,
        })
    }
    .await;

    match res {
        Ok(detail) => Ok(ApiResponse::ok(detail)),
        Err(e) if ctx.cancel.is_cancelled() || e.is_aborted() => Err(e),
        Err(e) => Ok(ApiResponse::err(1, format!("签名站点解析失败：{e}"))),
    }
}

pub async fn parse_signed_site(
    url: &str,
    headers: &HeaderMap,
    ctx: &ParserContext,
) -> CoreResult<ApiResponse<VideoInfo>> {
    let detail = parse_signed_site_detail(url, headers, ctx).await?;
    Ok(ApiResponse {
        code: detail.code,
        message: detail.message,
        data: detail.data.map(|d| d.info),
    })
}
