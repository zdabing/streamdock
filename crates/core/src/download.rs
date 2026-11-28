use crate::contract::{DownloadExecOptions, DownloadResult, HeaderInput, InputTypeKind};
use crate::error::CoreResult;
use crate::file_download::file_download;
use crate::format_options::format_options;
use crate::http::{format_headers, headers_for_target};
use crate::m3u8_download::m3u8_download;
use crate::video_parser::VideoParser;
use tokio_util::sync::CancellationToken;

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
        _ => m3u8_download(&formatted.url, &effective, None, cancel).await,
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
