use crate::contract::{DownloadExecOptions, DownloadResult, HeaderInput, InputTypeKind, ProxyMode};
use crate::error::{CoreError, CoreResult};
use crate::file_download::file_download;
use crate::format_options::{VideoPlatformKind, detect_platform, format_options};
use crate::http::{
    ProxySettings, build_http_client, format_headers, global_http_client, headers_for_target,
};
use crate::m3u8_download::m3u8_download;
use crate::signed_hls_proxy::{RefreshFn, SignedHlsProxy};
use crate::video_parser::VideoParser;
use crate::video_parser::request::ParserContext;
use crate::video_parser::signed_site;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use url::Url;

fn is_signed_hls_url(raw: &str) -> bool {
    let Ok(u) = Url::parse(raw) else {
        return false;
    };
    if u.path().contains("/bcdn_token=") {
        return true;
    }
    let mut has_token = false;
    let mut has_expires = false;
    for (k, v) in u.query_pairs() {
        if k == "token" && !v.is_empty() {
            has_token = true;
        } else if k == "expires" && !v.is_empty() {
            has_expires = true;
        }
    }
    has_token && has_expires
}

async fn download_hls_with_optional_refresh(
    target_url: &str,
    effective: &DownloadExecOptions,
    cancel: CancellationToken,
) -> CoreResult<DownloadResult> {
    let has_session = effective.token_session.is_some();
    let signed_source_url = effective
        .source_url
        .as_deref()
        .map(str::trim)
        .filter(|s| detect_platform(s).platform == VideoPlatformKind::SignedSite)
        .map(str::to_string);

    if !has_session && !(signed_source_url.is_some() && is_signed_hls_url(target_url)) {
        return m3u8_download(target_url, effective, None, cancel).await;
    }

    let client = effective.client.clone().unwrap_or_else(global_http_client);
    let request_headers = format_headers(effective.headers.as_ref())?;
    let mut init_session = effective.token_session.clone();
    if let Some(ref mut s) = init_session {
        if s.base_signed_url.trim().is_empty() {
            s.base_signed_url = target_url.to_string();
        }
    }

    let session_slot = Arc::new(tokio::sync::Mutex::new(init_session));
    let refresh_headers = request_headers.clone();
    let refresh_client = client.clone();
    let refresh_cancel = cancel.clone();
    let fallback_source = signed_source_url;
    let base_url_for_fallback = target_url.to_string();

    let refresh_fn: RefreshFn = Arc::new(move || {
        let slot = session_slot.clone();
        let h = refresh_headers.clone();
        let c = refresh_client.clone();
        let tok = refresh_cancel.clone();
        let src_opt = fallback_source.clone();
        let base_u = base_url_for_fallback.clone();
        Box::pin(async move {
            let ctx = ParserContext::new(c, tok.clone());
            let mut maybe_sess = slot.lock().await.clone();
            if let Some(ref mut sess) = maybe_sess {
                if sess.base_signed_url.trim().is_empty() {
                    sess.base_signed_url = base_u.clone();
                }
                match signed_site::refresh_token_session(sess, &h, &ctx).await {
                    Ok(next_url) => return Ok(next_url),
                    Err(e) if tok.is_cancelled() || e.is_aborted() => return Err(e),
                    Err(e) if src_opt.is_none() => return Err(e),
                    Err(_) => {}
                }
            }

            if let Some(src_url) = src_opt {
                let mut fresh_headers = h;
                fresh_headers.insert("cache-control".to_string(), "no-cache".to_string());
                fresh_headers.insert("pragma".to_string(), "no-cache".to_string());
                let refreshed =
                    signed_site::parse_signed_site_detail(&src_url, &fresh_headers, &ctx).await?;
                if refreshed.code != 0 {
                    return Err(CoreError::msg(
                        refreshed
                            .message
                            .unwrap_or_else(|| "播放凭证刷新失败".to_string()),
                    ));
                }
                let detail = refreshed
                    .data
                    .ok_or_else(|| CoreError::msg("播放凭证刷新失败"))?;
                *slot.lock().await = detail.session;
                if detail.info.url.is_empty() {
                    return Err(CoreError::msg("播放凭证刷新失败"));
                }
                return Ok(detail.info.url);
            }

            Err(CoreError::msg("缺少播放凭证刷新会话，请重新发送视频链接"))
        })
    });

    let proxy = SignedHlsProxy::start(
        client,
        target_url,
        request_headers,
        cancel.clone(),
        refresh_fn,
    )
    .await?;
    let entry = proxy.entry_url.clone();

    let mut proxy_opts = effective.clone();
    proxy_opts.headers = Some(HeaderInput::Map(Default::default()));
    proxy_opts.extra_headers = None;
    proxy_opts.segment_identity = Some(proxy.identity_fn());
    proxy_opts.download_type = Some(InputTypeKind::M3u8);
    proxy_opts.client = Some(build_http_client(&ProxySettings {
        mode: ProxyMode::Disabled,
        ..Default::default()
    })?);

    let dl = m3u8_download(&entry, &proxy_opts, Some(target_url), cancel).await;
    proxy.close().await;
    dl
}

pub async fn download_resource(
    input: &str,
    opts: &DownloadExecOptions,
    cancel: CancellationToken,
) -> CoreResult<DownloadResult> {
    let formatted = format_options(input, opts)?;
    let mut effective = opts.clone();
    if let Some(ref src) = opts.source_url {
        let base_hdrs = format_headers(opts.headers.as_ref())?;
        let cred_src = opts.header_source_url.as_deref().unwrap_or(src);
        let mut scoped = headers_for_target(cred_src, &formatted.url, &base_hdrs)?;
        if !base_hdrs.contains_key("referer") {
            scoped.insert("referer".to_string(), src.clone());
        }
        let map_val = scoped
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();
        effective.headers = Some(HeaderInput::Map(map_val));
        effective.extra_headers = None;
    }

    match formatted.download_type {
        InputTypeKind::Parser => VideoParser::download(&formatted.url, &effective, cancel).await,
        InputTypeKind::File => file_download(&formatted.url, &effective, None, cancel).await,
        _ => download_hls_with_optional_refresh(&formatted.url, &effective, cancel).await,
    }
}

pub async fn batch_download(
    urls: &[String],
    opts: &DownloadExecOptions,
    cancel: CancellationToken,
) -> CoreResult<bool> {
    let mut success = true;
    for url in urls {
        let res = download_resource(url, opts, cancel.clone()).await?;
        if res.errmsg.is_some() || res.paused {
            success = false;
        }
    }
    Ok(success)
}
