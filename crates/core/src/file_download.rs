use crate::contract::{
    DownloadExecOptions, DownloadResult, DownloadStats, DownloadTypeKind, HeaderMap, TaskPhase,
};
use crate::error::{CoreError, CoreResult};
use crate::format_options::format_options;
use crate::http::{format_byte_size, get_response, global_http_client, md5_hex};
use crate::m3u8_download::{m3u8_download, register_active_download, write_json_atomic};
use regex::Regex;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing::error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectFileState {
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishedMarker {
    version: u32,
    url: String,
    size: u64,
    file_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modified: Option<String>,
}

fn marker_path(target: &Path) -> PathBuf {
    let raw = target.to_string_lossy();
    let key = if cfg!(windows) {
        raw.to_ascii_lowercase()
    } else {
        raw.into_owned()
    };
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{}.download.json", md5_hex(&key)))
}

fn file_identity(path: &Path, meta: &std::fs::Metadata) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        Some(format!(
            "{}:{}:{}",
            meta.dev(),
            meta.ino(),
            meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let mut hasher = Sha256::new();
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut head = [0u8; 4096];
            if let Ok(n) = f.read(&mut head) {
                hasher.update(&head[..n]);
            }
            let len = meta.len();
            if len > 4096 {
                use std::io::Seek;
                if f.seek(std::io::SeekFrom::End(-4096)).is_ok() {
                    let mut tail = [0u8; 4096];
                    if let Ok(n) = f.read(&mut tail) {
                        hasher.update(&tail[..n]);
                    }
                }
            }
        }
        let digest = hex::encode(hasher.finalize());
        Some(format!(
            "{}:{}:{}:{}",
            meta.creation_time(),
            meta.last_write_time(),
            meta.file_size(),
            &digest[..16]
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Some(format!("{}:{}", meta.len(), mtime))
    }
}

fn parse_content_range(header: &str) -> Option<(u64, u64, u64)> {
    static RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^bytes (\d+)-(\d+)/(\d+)$").unwrap());
    let caps = RX.captures(header.trim())?;
    let start: u64 = caps[1].parse().ok()?;
    let end: u64 = caps[2].parse().ok()?;
    let total: u64 = caps[3].parse().ok()?;
    Some((start, end, total))
}

fn header_str<'a>(headers: &'a reqwest::header::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub async fn file_download(
    input: &str,
    opts: &DownloadExecOptions,
    source_identity: Option<&str>,
    cancel: CancellationToken,
) -> CoreResult<DownloadResult> {
    let mut patched_opts = opts.clone();
    patched_opts.download_type = Some(crate::contract::InputTypeKind::File);
    let formatted = format_options(input, &patched_opts)?;
    let url = formatted.url.clone();
    let url_md5 = md5_hex(source_identity.unwrap_or(&url));

    let child_cancel = cancel.child_token();
    let active_guard = register_active_download(&url, child_cancel.clone())?;
    let cache_dir = formatted.cache_dir.join(&url_md5);
    let partial_path = cache_dir.join("download.part");
    let state_path = cache_dir.join("file.json");
    let target = formatted.save_dir.join(&formatted.filename);
    let marker = marker_path(&target);
    let client = opts.client.clone().unwrap_or_else(global_http_client);

    let mut stats = DownloadStats {
        download_type: Some(DownloadTypeKind::File),
        start_time: Some(now_ms()),
        progress: Some(0.0),
        phase: Some(TaskPhase::Downloading),
        downloaded_size: Some(0),
        speed: Some(0.0),
        speed_desc: Some(String::new()),
        remaining_time: Some(0),
        filename: Some(formatted.filename.clone()),
        ..Default::default()
    };

    let report = |st: &DownloadStats| {
        if let Some(ref cb) = opts.on_progress {
            if !cb(st.clone()) {
                child_cancel.cancel();
            }
        }
    };

    let mut playlist_fallback = false;
    let mut is_exist = false;
    let res: CoreResult<Option<PathBuf>> = async {
        if !formatted.force {
            if let Ok(target_meta) = std::fs::metadata(&target) {
                let resumable = tokio::fs::try_exists(&partial_path).await.unwrap_or(false);
                if !resumable {
                    if let Ok(marker_txt) = tokio::fs::read_to_string(&marker).await {
                        if let Ok(pub_marker) = serde_json::from_str::<PublishedMarker>(&marker_txt) {
                            let cur_id = file_identity(&target, &target_meta).unwrap_or_default();
                            if pub_marker.version == 1
                                && pub_marker.url == url
                                && pub_marker.size == target_meta.len()
                                && pub_marker.file_id == cur_id
                            {
                                stats.size = Some(target_meta.len());
                                stats.downloaded_size = Some(target_meta.len());
                                stats.progress = Some(100.0);
                                stats.phase = Some(TaskPhase::Done);
                                stats.end_time = Some(now_ms());
                                is_exist = true;
                                return Ok(Some(target.clone()));
                            }
                        }
                    }
                }
            }
        }

        tokio::fs::create_dir_all(&cache_dir).await?;
        let mut state: Option<DirectFileState> = tokio::fs::read_to_string(&state_path)
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        let mut offset: u64 = tokio::fs::metadata(&partial_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        let valid_state = state
            .as_ref()
            .is_some_and(|s| s.url == url && (s.etag.is_some() || s.modified.is_some()));
        if !valid_state {
            offset = 0;
            state = None;
            let _ = tokio::fs::write(&partial_path, b"").await;
        }

        let mut base_headers: HeaderMap = formatted.headers.clone();
        base_headers.remove("range");
        base_headers.remove("if-range");

        let mut final_downloaded: u64 = 0;
        let mut final_etag: Option<String> = None;
        let mut final_modified: Option<String> = None;

        for attempt in 0..4usize {
            if child_cancel.is_cancelled() {
                return Err(CoreError::aborted());
            }

            let attempt_res: CoreResult<()> = async {
                let mut req_headers = base_headers.clone();
                if offset > 0 {
                    if let Some(ref st) = state {
                        if let Some(validator) = st.etag.as_deref().or(st.modified.as_deref()) {
                            req_headers.insert("range".to_string(), format!("bytes={offset}-"));
                            req_headers.insert("if-range".to_string(), validator.to_string());
                        }
                    }
                }

                let mut stream =
                    get_response(&client, &url, &req_headers, &child_cancel, None).await?;

                let content_type = header_str(&stream.headers, "content-type")
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if content_type.contains("mpegurl") {
                    return Err(CoreError::PlaylistFallback);
                }
                let lower_fn = formatted.filename.to_ascii_lowercase();
                let is_media_name = [".mp4", ".vtt", ".mkv", ".webm", ".mov", ".srt"]
                    .iter()
                    .any(|ext| lower_fn.ends_with(ext));
                if is_media_name
                    && (content_type.contains("text/html")
                        || content_type.contains("application/json")
                        || content_type.contains("application/problem+json"))
                {
                    return Err(CoreError::msg(
                        "服务器返回了网页或错误信息，请检查链接、Referer 和 Cookie",
                    ));
                }

                if let Some(enc) = header_str(&stream.headers, "content-encoding") {
                    if !enc.trim().eq_ignore_ascii_case("identity") {
                        return Err(CoreError::msg("直链下载要求服务器返回未压缩内容"));
                    }
                }

                let resp_len: Option<u64> = match header_str(&stream.headers, "content-length") {
                    Some(raw) => {
                        let parsed = raw
                            .trim()
                            .parse::<u64>()
                            .map_err(|_| CoreError::msg("服务器文件长度无效"))?;
                        Some(parsed)
                    }
                    None => None,
                };

                let resp_etag = header_str(&stream.headers, "etag")
                    .filter(|s| !s.starts_with("W/"))
                    .map(str::to_string);
                let resp_modified = header_str(&stream.headers, "last-modified").map(str::to_string);

                let total_expected: Option<u64>;
                if stream.status == 206 {
                    let cr_raw = header_str(&stream.headers, "content-range")
                        .ok_or(CoreError::RestartDownload)?;
                    let (start, end, total) =
                        parse_content_range(cr_raw).ok_or(CoreError::RestartDownload)?;
                    let prev_total = state.as_ref().and_then(|s| s.total);
                    let prev_etag = state.as_ref().and_then(|s| s.etag.as_deref());
                    let prev_mod = state.as_ref().and_then(|s| s.modified.as_deref());
                    let range_ok = start == offset
                        && end >= offset
                        && end + 1 == total
                        && resp_len.is_none_or(|len| len == end - offset + 1)
                        && (offset == 0 || prev_total.is_none_or(|pt| pt == total))
                        && (offset == 0
                            || prev_etag.is_none()
                            || resp_etag.is_none()
                            || prev_etag == resp_etag.as_deref())
                        && (offset == 0
                            || prev_etag.is_some()
                            || prev_mod.is_none()
                            || resp_modified.is_none()
                            || prev_mod == resp_modified.as_deref());
                    if !range_ok || total == 0 {
                        return Err(CoreError::RestartDownload);
                    }
                    total_expected = Some(total);
                } else if stream.status == 200 {
                    if offset > 0 {
                        offset = 0;
                        let _ = tokio::fs::write(&partial_path, b"").await;
                    }
                    total_expected = resp_len;
                } else {
                    return Err(CoreError::msg(format!("直链响应状态不支持: {}", stream.status)));
                }

                let new_state = DirectFileState {
                    url: url.clone(),
                    etag: resp_etag.clone(),
                    modified: resp_modified.clone(),
                    total: total_expected,
                };
                write_json_atomic(&state_path, &new_state).await?;
                state = Some(new_state);

                let downloaded = Arc::new(AtomicU64::new(offset));
                let attempt_start = now_ms();
                let last_sample = Arc::new(std::sync::Mutex::new((attempt_start, offset)));
                stats.size = total_expected;
                stats.downloaded_size = Some(offset);
                stats.progress = Some(total_expected.map_or(0.0, |t| {
                    if t > 0 {
                        ((offset as f64 / t as f64) * 100.0 * 10.0).round() / 10.0
                    } else {
                        0.0
                    }
                    .min(99.0)
                }));
                report(&stats);

                let mut file = if offset > 0 {
                    let mut f = tokio::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .open(&partial_path)
                        .await?;
                    f.seek(std::io::SeekFrom::Start(offset)).await?;
                    f.set_len(offset).await?;
                    f
                } else {
                    tokio::fs::File::create(&partial_path).await?
                };

                let tick_cancel = child_cancel.child_token();
                let tick_dl = downloaded.clone();
                let tick_last = last_sample.clone();
                let tick_stats = Arc::new(std::sync::Mutex::new(stats.clone()));
                let tick_cb = opts.on_progress.clone();
                let main_cancel = child_cancel.clone();
                let tick_handle = {
                    let tok = tick_cancel.clone();
                    let ts = tick_stats.clone();
                    tokio::spawn(async move {
                        let mut iv = tokio::time::interval(std::time::Duration::from_millis(500));
                        iv.tick().await;
                        loop {
                            tokio::select! {
                                _ = tok.cancelled() => break,
                                _ = iv.tick() => {
                                    if main_cancel.is_cancelled() {
                                        break;
                                    }
                                    let now = now_ms();
                                    let cur = tick_dl.load(Ordering::Relaxed);
                                    let (lt, lb) = {
                                        let mut g = tick_last.lock().unwrap();
                                        let prev = *g;
                                        *g = (now, cur);
                                        prev
                                    };
                                    let speed = ((cur.saturating_sub(lb) as f64) * 1000.0 / (now - lt).max(1) as f64).max(0.0);
                                    let snap = {
                                        let mut s = ts.lock().unwrap();
                                        let st_time = s.start_time.unwrap_or(now);
                                        let avg = ((cur as f64) * 1000.0 / (now - st_time).max(1) as f64).max(0.0);
                                        s.downloaded_size = Some(cur);
                                        s.speed = Some(speed);
                                        s.speed_desc = Some(format!("{}/s", format_byte_size(speed as u64)));
                                        if let Some(tot) = s.size {
                                            if tot > 0 {
                                                let pct = ((cur as f64 / tot as f64) * 100.0 * 10.0).round() / 10.0;
                                                s.progress = Some(pct.min(99.0));
                                            }
                                            let rem = if avg > 0.0 && tot > cur {
                                                ((tot - cur) as f64 / avg).ceil() as u64
                                            } else {
                                                0
                                            };
                                            s.remaining_time = Some(rem);
                                        }
                                        s.clone()
                                    };
                                    if let Some(ref cb) = tick_cb {
                                        if !cb(snap) {
                                            main_cancel.cancel();
                                        }
                                    }
                                }
                            }
                        }
                    })
                };

                let stream_res: CoreResult<()> = async {
                    loop {
                        let chunk = tokio::select! {
                            _ = child_cancel.cancelled() => return Err(CoreError::aborted()),
                            c = stream.response.chunk() => c?,
                        };
                        let Some(bytes) = chunk else {
                            break;
                        };
                        let new_dl = downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed)
                            + bytes.len() as u64;
                        if let Some(tot) = total_expected {
                            if new_dl > tot {
                                return Err(CoreError::msg("下载内容超出服务器声明的长度"));
                            }
                        }
                        file.write_all(&bytes).await?;
                    }
                    file.flush().await?;
                    Ok(())
                }
                .await;

                tick_cancel.cancel();
                let _ = tick_handle.await;
                stats = tick_stats.lock().unwrap().clone();
                stream_res?;

                let actual = downloaded.load(Ordering::Relaxed);
                if let Some(tot) = total_expected {
                    if actual != tot {
                        return Err(CoreError::msg("文件尚未下载完整，缓存已保留"));
                    }
                }
                if actual == 0 {
                    return Err(CoreError::msg("服务器返回了空文件"));
                }
                final_downloaded = actual;
                final_etag = resp_etag;
                final_modified = resp_modified;
                Ok(())
            }
            .await;

            match attempt_res {
                Ok(()) => break,
                Err(CoreError::PlaylistFallback) => {
                    playlist_fallback = true;
                    return Err(CoreError::PlaylistFallback);
                }
                Err(err) => {
                    if child_cancel.is_cancelled() || err.is_aborted() {
                        return Err(CoreError::aborted());
                    }
                    let is_416 = err.as_http().is_some_and(|h| h.status == 416);
                    let restart = matches!(err, CoreError::RestartDownload) || is_416;
                    if restart {
                        offset = 0;
                        state = None;
                        let _ = tokio::fs::write(&partial_path, b"").await;
                    } else {
                        let can_resume = state
                            .as_ref()
                            .is_some_and(|s| s.etag.is_some() || s.modified.is_some());
                        offset = if can_resume {
                            tokio::fs::metadata(&partial_path)
                                .await
                                .map(|m| m.len())
                                .unwrap_or(0)
                        } else {
                            0
                        };
                    }
                    let stop = attempt >= 3 || (!err.is_retryable_http() && !restart);
                    if stop {
                        return Err(err);
                    }
                    let backoff_ms = (500u64 << attempt).min(3000);
                    tokio::select! {
                        _ = child_cancel.cancelled() => return Err(CoreError::aborted()),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)) => {}
                    }
                }
            }
        }

        // Atomic cross-drive publish
        tokio::fs::create_dir_all(&formatted.save_dir).await?;
        let uuid = uuid::Uuid::new_v4();
        let publishing = formatted.save_dir.join(format!(".{uuid}.download.part"));
        let marker_part = formatted
            .save_dir
            .join(format!(".{uuid}.download.part.json"));

        let pub_res: CoreResult<()> = async {
            tokio::fs::copy(&partial_path, &publishing).await?;
            let pub_meta = std::fs::metadata(&publishing)?;
            let pub_file_id = file_identity(&publishing, &pub_meta).unwrap_or_default();
            let pub_marker = PublishedMarker {
                version: 1,
                url: url.clone(),
                size: pub_meta.len(),
                file_id: pub_file_id,
                etag: final_etag,
                modified: final_modified,
            };
            tokio::fs::write(&marker_part, serde_json::to_string(&pub_marker)?.as_bytes()).await?;
            if child_cancel.is_cancelled() {
                return Err(CoreError::aborted());
            }
            let _ = tokio::fs::remove_file(&marker).await;
            tokio::fs::rename(&publishing, &target).await?;
            tokio::fs::rename(&marker_part, &marker).await?;
            Ok(())
        }
        .await;

        let _ = tokio::fs::remove_file(&publishing).await;
        let _ = tokio::fs::remove_file(&marker_part).await;
        pub_res?;

        if formatted.del_cache {
            if cache_dir != formatted.cache_dir && cache_dir.starts_with(&formatted.cache_dir) {
                let _ = tokio::fs::remove_dir_all(&cache_dir).await;
            }
        }

        stats.phase = Some(TaskPhase::Done);
        stats.progress = Some(100.0);
        stats.end_time = Some(now_ms());
        stats.size = Some(final_downloaded);
        stats.downloaded_size = Some(final_downloaded);
        stats.local_video = Some(target.to_string_lossy().into_owned());
        report(&stats);

        Ok(Some(target.clone()))
    }
    .await;

    active_guard.release();

    if playlist_fallback && !child_cancel.is_cancelled() {
        return m3u8_download(input, opts, source_identity, cancel).await;
    }

    match res {
        Ok(fp) => Ok(DownloadResult {
            url,
            filepath: fp.map(|p| p.to_string_lossy().into_owned()),
            errmsg: None,
            is_exist,
            paused: false,
            stats,
            segment_files: Vec::new(),
        }),
        Err(err) => {
            if child_cancel.is_cancelled() || err.is_aborted() {
                Ok(DownloadResult {
                    url,
                    filepath: None,
                    errmsg: None,
                    is_exist: false,
                    paused: true,
                    stats,
                    segment_files: Vec::new(),
                })
            } else {
                let msg = err.to_string();
                error!("{}", msg);
                stats.errmsg = Some(msg.clone());
                Ok(DownloadResult {
                    url,
                    filepath: None,
                    errmsg: Some(msg),
                    is_exist: false,
                    paused: false,
                    stats,
                    segment_files: Vec::new(),
                })
            }
        }
    }
}
