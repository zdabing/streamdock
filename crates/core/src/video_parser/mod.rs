pub mod douyin;
pub mod pipixia;
pub mod request;
pub mod signed_site;
pub mod weibo;

use crate::contract::{
    ApiResponse, DownloadExecOptions, DownloadResult, DownloadStats, DownloadTypeKind, HeaderInput,
    HeaderMap, InputTypeKind, ProxyMode, TaskPhase, VideoInfo,
};
use crate::error::{CoreError, CoreResult};
use crate::file_download::file_download;
use crate::format_options::{VideoPlatformKind, detect_platform, format_options};
use crate::http::{ProxySettings, build_http_client, global_http_client, headers_for_target};
use crate::m3u8_download::{m3u8_download, register_active_download};
use crate::signed_hls_proxy::{RefreshFn, SignedHlsProxy};
use request::ParserContext;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use url::Url;

pub struct VideoParser;

impl VideoParser {
    pub fn get_supported_platforms() -> &'static [&'static str] {
        &["douyin", "pipixia", "weibo", "signed_site"]
    }

    pub async fn parse(
        input: &str,
        headers: &HeaderMap,
        client: reqwest::Client,
        cancel: CancellationToken,
    ) -> CoreResult<ApiResponse<VideoInfo>> {
        let ctx = ParserContext::new(client, cancel);
        Self::parse_with_context(input, headers, &ctx).await
    }

    pub async fn parse_with_context(
        input: &str,
        headers: &HeaderMap,
        ctx: &ParserContext,
    ) -> CoreResult<ApiResponse<VideoInfo>> {
        let info = detect_platform(input);
        if info.platform == VideoPlatformKind::Unknown {
            return Ok(ApiResponse::err(
                1,
                info.errmsg.unwrap_or_else(|| {
                    "支持抖音、微博、皮皮虾及内置签名站点；其他播放页面请使用网页提取".to_string()
                }),
            ));
        }
        if ctx.cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        match info.platform {
            VideoPlatformKind::Douyin => douyin::parse_douyin(&info.url, headers, ctx).await,
            VideoPlatformKind::Pipixia => pipixia::parse_pipixia(&info.url, headers, ctx).await,
            VideoPlatformKind::Weibo => weibo::parse_weibo(&info.url, headers, ctx).await,
            VideoPlatformKind::SignedSite => {
                signed_site::parse_signed_site(&info.url, headers, ctx).await
            }
            VideoPlatformKind::Unknown => Ok(ApiResponse::err(1, "不支持的视频平台")),
        }
    }

    pub async fn download(
        input: &str,
        opts: &DownloadExecOptions,
        cancel: CancellationToken,
    ) -> CoreResult<DownloadResult> {
        let formatted = format_options(input, opts)?;
        let url = formatted.url.clone();
        let child_cancel = cancel.child_token();
        let _active_guard = register_active_download(&url, child_cancel.clone())
            .map_err(|_| CoreError::msg("该链接正在解析或下载，请先暂停现有任务"))?;
        let client = opts.client.clone().unwrap_or_else(global_http_client);

        let mut init_stats = DownloadStats {
            download_type: Some(DownloadTypeKind::File),
            start_time: Some(chrono::Utc::now().timestamp_millis()),
            progress: Some(0.0),
            phase: Some(TaskPhase::Parsing),
            ts_count: Some(0),
            ts_success: Some(0),
            ts_failed: Some(0),
            duration: Some(0.0),
            downloaded_size: Some(0),
            speed: Some(0.0),
            speed_desc: Some(String::new()),
            remaining_time: Some(0),
            filename: Some(formatted.filename.clone()),
            thread_num: Some(formatted.thread_num),
            ..Default::default()
        };

        if let Some(ref cb) = opts.on_progress {
            if !cb(init_stats.clone()) {
                child_cancel.cancel();
            }
        }

        let platform_kind = detect_platform(&url).platform;
        let ctx = ParserContext::new(client.clone(), child_cancel.clone());
        let parse_res: CoreResult<(
            ApiResponse<VideoInfo>,
            bool,
            Option<signed_site::SignedSiteTokenSession>,
        )> = async {
            if platform_kind == VideoPlatformKind::SignedSite {
                let detail_resp =
                    signed_site::parse_signed_site_detail(&url, &formatted.headers, &ctx).await?;
                let (signed, sess, info_opt) = match detail_resp.data {
                    Some(d) => (d.is_signed, d.session, Some(d.info)),
                    None => (true, None, None),
                };
                Ok((
                    ApiResponse {
                        code: detail_resp.code,
                        message: detail_resp.message,
                        data: info_opt,
                    },
                    signed,
                    sess,
                ))
            } else {
                let p = Self::parse_with_context(&url, &formatted.headers, &ctx).await?;
                Ok((p, false, None))
            }
        }
        .await;

        let (parsed, signed_site_hls, signed_site_session) = match parse_res {
            Ok(v) => v,
            Err(err) => {
                if child_cancel.is_cancelled() || err.is_aborted() {
                    return Ok(DownloadResult {
                        url: url.clone(),
                        paused: true,
                        stats: init_stats,
                        ..Default::default()
                    });
                }
                let msg = err.to_string();
                init_stats.errmsg = Some(msg.clone());
                return Ok(DownloadResult {
                    url: url.clone(),
                    errmsg: Some(msg),
                    stats: init_stats,
                    ..Default::default()
                });
            }
        };

        if parsed.code != 0 || parsed.data.is_none() {
            let msg = parsed.message.unwrap_or_else(|| "视频解析失败".to_string());
            init_stats.errmsg = Some(msg.clone());
            return Ok(DownloadResult {
                url,
                errmsg: Some(msg),
                stats: init_stats,
                ..Default::default()
            });
        }

        let info = parsed.data.unwrap();
        if info.url == url {
            let msg = "平台未返回独立的视频直链".to_string();
            init_stats.errmsg = Some(msg.clone());
            return Ok(DownloadResult {
                url,
                errmsg: Some(msg),
                stats: init_stats,
                ..Default::default()
            });
        }

        let cred_src = info.referer.as_deref().unwrap_or(&url);
        let mut request_headers = headers_for_target(cred_src, &info.url, &formatted.headers)?;
        if info.platform.as_deref() == Some("signed_site")
            && !request_headers.contains_key("origin")
        {
            if let Ok(src_url) = Url::parse(cred_src) {
                if matches!(src_url.scheme(), "http" | "https") {
                    request_headers
                        .insert("origin".to_string(), src_url.origin().ascii_serialization());
                }
            }
        }
        let header_map_val = request_headers
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();

        let is_hls = Url::parse(&info.url)
            .ok()
            .is_some_and(|u| u.path().to_ascii_lowercase().ends_with(".m3u8"));

        let chosen_filename = opts
            .filename
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                if info.title.trim().is_empty() {
                    None
                } else {
                    let short: String = info.title.chars().take(120).collect();
                    Some(format!("{short}.mp4"))
                }
            });

        let mut next_opts = opts.clone();
        next_opts.filename = chosen_filename;
        next_opts.headers = Some(HeaderInput::Map(header_map_val));
        next_opts.extra_headers = None;
        next_opts.source_url = None;
        next_opts.header_source_url = None;

        let mut result =
            if is_hls && info.platform.as_deref() == Some("signed_site") && signed_site_hls {
                let refresh_url = url.clone();
                let refresh_headers = formatted.headers.clone();
                let refresh_client = client.clone();
                let refresh_cancel = child_cancel.clone();
                let session_slot = Arc::new(tokio::sync::Mutex::new(signed_site_session));
                let refresh_fn: RefreshFn = Arc::new(move || {
                    let u = refresh_url.clone();
                    let h = refresh_headers.clone();
                    let c = refresh_client.clone();
                    let tok = refresh_cancel.clone();
                    let slot = session_slot.clone();
                    Box::pin(async move {
                        let refresh_ctx = ParserContext::new(c, tok.clone());
                        let maybe_sess = slot.lock().await.clone();
                        if let Some(sess) = maybe_sess {
                            match signed_site::refresh_token_session(&sess, &h, &refresh_ctx).await
                            {
                                Ok(next_url) => return Ok(next_url),
                                Err(e) if tok.is_cancelled() || e.is_aborted() => return Err(e),
                                Err(_) => {}
                            }
                        }

                        let mut fresh_headers = h;
                        fresh_headers.insert("cache-control".to_string(), "no-cache".to_string());
                        fresh_headers.insert("pragma".to_string(), "no-cache".to_string());
                        let refreshed =
                            signed_site::parse_signed_site_detail(&u, &fresh_headers, &refresh_ctx)
                                .await?;
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
                        Ok(detail.info.url)
                    })
                });

                let proxy = SignedHlsProxy::start(
                    client,
                    &info.url,
                    request_headers,
                    child_cancel.clone(),
                    refresh_fn,
                )
                .await?;
                let entry = proxy.entry_url.clone();
                next_opts.headers = Some(HeaderInput::Map(Default::default()));
                next_opts.segment_identity = Some(proxy.identity_fn());
                next_opts.download_type = Some(InputTypeKind::M3u8);
                next_opts.client = Some(build_http_client(&ProxySettings {
                    mode: ProxyMode::Disabled,
                    ..Default::default()
                })?);
                let dl = m3u8_download(&entry, &next_opts, Some(&url), child_cancel.clone()).await;
                proxy.close().await;
                dl?
            } else if is_hls {
                next_opts.download_type = Some(InputTypeKind::M3u8);
                m3u8_download(&info.url, &next_opts, Some(&url), child_cancel.clone()).await?
            } else {
                next_opts.download_type = Some(InputTypeKind::File);
                file_download(&info.url, &next_opts, Some(&url), child_cancel.clone()).await?
            };

        result.url = url;
        Ok(result)
    }
}
