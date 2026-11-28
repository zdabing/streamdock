use crate::contract::{
    DownloadExecOptions, DownloadResult, DownloadStats, DownloadTypeKind, SegmentProgress,
    SegmentSnapshot, SegmentStatus, TaskPhase,
};
use crate::crypto::{CryptoKeyInfo, decrypt_segment_data};
use crate::error::{CoreError, CoreResult};
use crate::format_options::format_options;
use crate::http::{
    format_byte_size, get_retry, global_http_client, md5_hex, scope_credentials, slice_byterange,
    with_range_header,
};
use crate::m3u8_parse::{M3u8Info, TsItemInfo, parse_m3u8};
use crate::merge::{ConvertOptions, m3u8_convert};
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

static ACTIVE_DOWNLOADS: OnceLock<StdMutex<HashMap<String, CancellationToken>>> = OnceLock::new();

pub fn active_downloads_map() -> &'static StdMutex<HashMap<String, CancellationToken>> {
    ACTIVE_DOWNLOADS.get_or_init(|| StdMutex::new(HashMap::new()))
}

pub fn stop_download(url: &str) {
    let token = active_downloads_map().lock().unwrap().get(url).cloned();
    if let Some(tok) = token {
        tok.cancel();
    }
}

pub fn is_download_active(url: &str) -> bool {
    active_downloads_map().lock().unwrap().contains_key(url)
}

pub fn register_active_download(url: &str, token: CancellationToken) -> CoreResult<ActiveGuard> {
    let mut map = active_downloads_map().lock().unwrap();
    if map.contains_key(url) {
        return Err(CoreError::msg("该链接正在下载，请先暂停现有任务"));
    }
    map.insert(url.to_string(), token);
    Ok(ActiveGuard {
        url: Some(url.to_string()),
    })
}

pub struct ActiveGuard {
    url: Option<String>,
}

impl ActiveGuard {
    pub fn release(mut self) {
        if let Some(u) = self.url.take() {
            active_downloads_map().lock().unwrap().remove(&u);
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Some(u) = self.url.take() {
            active_downloads_map().lock().unwrap().remove(&u);
        }
    }
}

#[derive(Serialize)]
struct FingerprintByterange {
    length: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<u64>,
}

fn compute_fingerprint(info: &M3u8Info, opts: &DownloadExecOptions) -> String {
    let lines: Vec<String> = info
        .data
        .iter()
        .map(|seg| {
            let id = opts
                .segment_identity
                .as_ref()
                .map(|f| f(&seg.uri))
                .unwrap_or_else(|| seg.uri.clone());
            let br_json = match seg.byterange {
                Some(br) => serde_json::to_string(&FingerprintByterange {
                    length: br.length,
                    offset: br.offset,
                })
                .unwrap_or_else(|_| "null".to_string()),
                None => "null".to_string(),
            };
            format!("{id}@{br_json}")
        })
        .collect();
    md5_hex(&lines.join("\n"))
}

fn parse_ignore_ranges(spec: &str, total_duration: f64) -> Vec<(f64, f64)> {
    static END_RX: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)^END-(\d+(?:\.\d+)?)$").unwrap());
    spec.split(',')
        .filter_map(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Some(caps) = END_RX.captures(trimmed) {
                let secs: f64 = caps[1].parse().ok()?;
                return Some(((total_duration - secs).max(0.0), total_duration));
            }
            let (a, b) = trimmed.split_once('-')?;
            let start: f64 = a.trim().parse().ok()?;
            let end: f64 = b.trim().parse().ok()?;
            Some((start, end))
        })
        .collect()
}

pub async fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> CoreResult<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = PathBuf::from(format!("{}.part", path.display()));
    let json = serde_json::to_string(value)?;
    tokio::fs::write(&tmp, json.as_bytes()).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn ts_download(
    client: &reqwest::Client,
    segment: &TsItemInfo,
    crypto: Option<&CryptoKeyInfo>,
    headers: &crate::contract::HeaderMap,
    cancel: &CancellationToken,
    mut on_bytes: impl FnMut(u64) + Send,
) -> CoreResult<u64> {
    if let Ok(meta) = tokio::fs::metadata(&segment.ts_out).await {
        if meta.len() > 0 {
            return Ok(meta.len());
        }
    }
    let req_headers = with_range_header(headers, segment.byterange);
    let resp = get_retry(
        client,
        &segment.uri,
        &req_headers,
        3,
        cancel,
        Some(&mut on_bytes),
    )
    .await?;
    let raw = slice_byterange(resp.body, segment.byterange, resp.status);
    let data = decrypt_segment_data(&raw, crypto, segment.iv)?;
    if data.is_empty() {
        return Err(CoreError::msg(format!(
            "视频分片为空: {}",
            segment.index + 1
        )));
    }
    if let Some(parent) = segment.ts_out.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = PathBuf::from(format!("{}.part", segment.ts_out.display()));
    let write_res: CoreResult<()> = async {
        tokio::fs::write(&tmp, &data).await?;
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        tokio::fs::rename(&tmp, &segment.ts_out).await?;
        Ok(())
    }
    .await;
    if write_res.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        write_res?;
    }
    Ok(data.len() as u64)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub async fn m3u8_download(
    input: &str,
    opts: &DownloadExecOptions,
    source_identity: Option<&str>,
    cancel: CancellationToken,
) -> CoreResult<DownloadResult> {
    let mut patched_opts = opts.clone();
    patched_opts.download_type = Some(crate::contract::InputTypeKind::M3u8);
    let formatted = format_options(input, &patched_opts)?;
    let url = formatted.url.clone();
    let url_md5 = md5_hex(source_identity.unwrap_or(&url));

    let child_cancel = cancel.child_token();
    let _active_guard = register_active_download(&url, child_cancel.clone())?;
    let cache_dir = formatted.cache_dir.join(&url_md5);
    let state_path = cache_dir.join("segments.json");
    let client = opts.client.clone().unwrap_or_else(global_http_client);

    let mut stats = DownloadStats {
        download_type: Some(DownloadTypeKind::M3u8),
        start_time: Some(now_ms()),
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

    let report = |st: &DownloadStats| {
        if let Some(ref cb) = opts.on_progress {
            if !cb(st.clone()) {
                child_cancel.cancel();
            }
        }
    };

    let mut segment_state: Option<Arc<StdMutex<SegmentSnapshot>>> = None;
    let write_lock = Arc::new(Mutex::new(()));
    let mut cache_removed = false;
    let mut is_exist = false;
    let mut segment_files: Vec<String> = Vec::new();

    let run_res: CoreResult<Option<PathBuf>> = async {
        let existing = formatted.save_dir.join(&formatted.filename);
        if !formatted.force && tokio::fs::try_exists(&existing).await.unwrap_or(false) {
            is_exist = true;
            return Ok(Some(existing));
        }

        report(&stats);

        let mut info = parse_m3u8(
            &client,
            &url,
            &cache_dir,
            &formatted.headers,
            &child_cancel,
            opts.segment_identity.as_ref(),
        )
        .await?;

        if let Some(ref ignore) = opts.ignore_segments {
            if !ignore.trim().is_empty() {
                let ranges = parse_ignore_ranges(ignore, info.duration);
                let mut timeline = 0.0_f64;
                info.data.retain(|seg| {
                    let mid = timeline + seg.duration / 2.0;
                    timeline += seg.duration;
                    !ranges.iter().any(|(s, e)| mid >= *s && mid <= *e)
                });
                info.ts_count = info.data.len();
                info.duration = info.data.iter().map(|s| s.duration).sum();
            }
        }
        if info.ts_count == 0 {
            return Err(CoreError::msg("过滤后没有可下载的视频分片"));
        }

        segment_files = info
            .data
            .iter()
            .map(|s| s.ts_out.to_string_lossy().into_owned())
            .collect();

        tokio::fs::create_dir_all(&cache_dir).await?;
        let info_json = serde_json::json!({
            "url": url,
            "filename": formatted.filename,
        });
        tokio::fs::write(
            cache_dir.join("info.json"),
            serde_json::to_string(&info_json)?.as_bytes(),
        )
        .await?;

        let fingerprint = compute_fingerprint(&info, opts);
        let previous: Option<SegmentSnapshot> = tokio::fs::read_to_string(&state_path)
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        if opts.retry_failed_only {
            let Some(ref prev) = previous else {
                return Err(CoreError::msg(
                    "缺少匹配的失败分片记录，或播放列表已变化，请使用继续下载",
                ));
            };
            if prev.fingerprint != fingerprint {
                return Err(CoreError::msg(
                    "缺少匹配的失败分片记录，或播放列表已变化，请使用继续下载",
                ));
            }
        }

        let prev_items = previous
            .filter(|p| p.fingerprint == fingerprint)
            .map(|p| p.items)
            .unwrap_or_default();

        let failed_set: HashSet<usize> = prev_items
            .iter()
            .filter(|i| i.status == SegmentStatus::Error)
            .map(|i| i.index)
            .collect();

        if opts.retry_failed_only && failed_set.is_empty() {
            return Err(CoreError::msg("没有需要重试的失败分片"));
        }

        let mut items = Vec::with_capacity(info.data.len());
        let mut queue = Vec::new();
        let completed_bytes = Arc::new(AtomicU64::new(0));
        let mut init_success = 0usize;

        for (index, seg) in info.data.iter_mut().enumerate() {
            if child_cancel.is_cancelled() {
                return Err(CoreError::aborted());
            }
            let size = tokio::fs::metadata(&seg.ts_out)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            let cached = size > 0;
            if opts.retry_failed_only && !failed_set.contains(&index) && !cached {
                return Err(CoreError::msg(format!(
                    "第 {} 个分片缓存缺失，请使用继续下载",
                    index + 1
                )));
            }
            let prev_attempts = prev_items.get(index).map(|i| i.attempts).unwrap_or(0);
            items.push(SegmentProgress {
                index,
                status: if cached {
                    SegmentStatus::Done
                } else {
                    SegmentStatus::Pending
                },
                size,
                attempts: prev_attempts,
                error: None,
            });
            if cached {
                seg.ts_size = size;
                completed_bytes.fetch_add(size, Ordering::Relaxed);
                init_success += 1;
            } else {
                queue.push(index);
            }
        }

        let snap_arc = Arc::new(StdMutex::new(SegmentSnapshot {
            fingerprint: fingerprint.clone(),
            items,
        }));
        segment_state = Some(snap_arc.clone());

        {
            let _g = write_lock.lock().await;
            let snapshot_clone = snap_arc.lock().unwrap().clone();
            write_json_atomic(&state_path, &snapshot_clone).await?;
        }

        stats.phase = Some(TaskPhase::Downloading);
        stats.ts_count = Some(info.ts_count);
        stats.ts_success = Some(init_success);
        stats.duration = Some(info.duration);
        stats.has_segment_details = Some(true);

        let shared_stats = Arc::new(StdMutex::new(stats.clone()));
        let partial_bytes: Arc<StdMutex<HashMap<usize, u64>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let last_received: Arc<StdMutex<HashMap<usize, u64>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let network_bytes = Arc::new(AtomicU64::new(0));
        let samples = Arc::new(StdMutex::new(VecDeque::from([(now_ms(), 0u64)])));

        let refresh_stats = {
            let shared_stats = shared_stats.clone();
            let samples = samples.clone();
            let network_bytes = network_bytes.clone();
            let completed_bytes = completed_bytes.clone();
            let partial_bytes = partial_bytes.clone();
            move || -> DownloadStats {
                let now = now_ms();
                let net = network_bytes.load(Ordering::Relaxed);
                let mut smp = samples.lock().unwrap();
                smp.push_back((now, net));
                while smp.len() > 2 && smp[1].0 < now - 3000 {
                    smp.pop_front();
                }
                let first = smp.front().copied().unwrap_or((now, net));
                let elapsed_ms = (now - first.0).max(1) as f64;
                let speed = ((net.saturating_sub(first.1) as f64) * 1000.0 / elapsed_ms).max(0.0);
                drop(smp);

                let partial_sum: u64 = partial_bytes.lock().unwrap().values().sum();
                let downloaded = completed_bytes.load(Ordering::Relaxed) + partial_sum;

                let mut st = shared_stats.lock().unwrap();
                st.speed = Some(speed);
                st.speed_desc = Some(format!("{}/s", format_byte_size(speed as u64)));
                st.downloaded_size = Some(downloaded);
                let ts_total = st.ts_count.unwrap_or(0).max(1);
                let ts_ok = st.ts_success.unwrap_or(0);
                st.progress = Some((((ts_ok * 100) / ts_total).min(99)) as f64);
                let start = st.start_time.unwrap_or(now);
                st.remaining_time = if ts_ok > 0 {
                    let secs_so_far = (now - start).max(0) as f64 / 1000.0;
                    Some(((secs_so_far / ts_ok as f64) * (ts_total.saturating_sub(ts_ok) as f64)).ceil() as u64)
                } else {
                    Some(0)
                };
                st.clone()
            }
        };

        report(&refresh_stats());

        let dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tick_cancel = child_cancel.child_token();
        let tick_handle = {
            let tick_tok = tick_cancel.clone();
            let shared_stats = shared_stats.clone();
            let refresh_stats = refresh_stats.clone();
            let on_progress = opts.on_progress.clone();
            let main_cancel = child_cancel.clone();
            let dirty = dirty.clone();
            let snap_arc = snap_arc.clone();
            let write_lock = write_lock.clone();
            let state_path = state_path.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(500));
                interval.tick().await; // skip immediate tick
                loop {
                    tokio::select! {
                        _ = tick_tok.cancelled() => break,
                        _ = interval.tick() => {
                            let is_downloading = shared_stats.lock().unwrap().phase == Some(TaskPhase::Downloading);
                            if !is_downloading || main_cancel.is_cancelled() {
                                break;
                            }
                            let snap = refresh_stats();
                            if let Some(ref cb) = on_progress {
                                if !cb(snap) {
                                    main_cancel.cancel();
                                }
                            }
                            if dirty.swap(false, Ordering::Relaxed) {
                                let snapshot_clone = snap_arc.lock().unwrap().clone();
                                let _g = write_lock.lock().await;
                                let _ = write_json_atomic(&state_path, &snapshot_clone).await;
                            }
                        }
                    }
                }
            })
        };

        let cursor = Arc::new(AtomicUsize::new(0));
        let queue_arc = Arc::new(queue);
        let info_arc = Arc::new(info);
        let first_failure = Arc::new(StdMutex::new(String::new()));
        let expired_failure = Arc::new(StdMutex::new(String::new()));
        let is_http_source = url.to_ascii_lowercase().starts_with("http://")
            || url.to_ascii_lowercase().starts_with("https://");

        let worker_count = formatted.thread_num.min(queue_arc.len());
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let cursor = cursor.clone();
            let queue_arc = queue_arc.clone();
            let info_arc = info_arc.clone();
            let snap_arc = snap_arc.clone();
            let dirty = dirty.clone();
            let client = client.clone();
            let formatted_headers = formatted.headers.clone();
            let url_clone = url.clone();
            let child_cancel = child_cancel.clone();
            let partial_bytes = partial_bytes.clone();
            let last_received = last_received.clone();
            let network_bytes = network_bytes.clone();
            let shared_stats = shared_stats.clone();
            let completed_bytes = completed_bytes.clone();
            let first_failure = first_failure.clone();
            let expired_failure = expired_failure.clone();
            let refresh_stats = refresh_stats.clone();
            let on_progress = opts.on_progress.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    if child_cancel.is_cancelled() {
                        return Ok::<(), CoreError>(());
                    }
                    let q_idx = cursor.fetch_add(1, Ordering::Relaxed);
                    if q_idx >= queue_arc.len() {
                        return Ok(());
                    }
                    let seg_idx = queue_arc[q_idx];
                    let seg = &info_arc.data[seg_idx];
                    let mut failure: Option<CoreError> = None;
                    let mut final_size: u64 = 0;

                    for attempt in 0..3 {
                        {
                            let mut s = snap_arc.lock().unwrap();
                            if let Some(item) = s.items.get_mut(seg_idx) {
                                item.status = SegmentStatus::Downloading;
                                item.attempts += 1;
                                item.error = None;
                            }
                        }
                        dirty.store(true, Ordering::Relaxed);

                        let seg_headers = if is_http_source {
                            scope_credentials(&url_clone, &seg.uri, &formatted_headers)?
                        } else {
                            formatted_headers.clone()
                        };
                        let key_info = if seg.key_uri.is_empty() {
                            None
                        } else {
                            info_arc.crypto.get(&seg.key_uri)
                        };

                        let res = ts_download(
                            &client,
                            seg,
                            key_info,
                            &seg_headers,
                            &child_cancel,
                            |received| {
                                let mut lr = last_received.lock().unwrap();
                                let prev = lr.get(&seg_idx).copied().unwrap_or(0);
                                if received > prev {
                                    network_bytes.fetch_add(received - prev, Ordering::Relaxed);
                                }
                                lr.insert(seg_idx, received);
                                drop(lr);

                                partial_bytes.lock().unwrap().insert(seg_idx, received);
                                let mut s = snap_arc.lock().unwrap();
                                if let Some(item) = s.items.get_mut(seg_idx) {
                                    item.size = received;
                                }
                            },
                        )
                        .await;

                        match res {
                            Ok(sz) => {
                                final_size = sz;
                                failure = None;
                                break;
                            }
                            Err(err) => {
                                if child_cancel.is_cancelled() || err.is_aborted() {
                                    {
                                        let mut s = snap_arc.lock().unwrap();
                                        if let Some(item) = s.items.get_mut(seg_idx) {
                                            item.status = SegmentStatus::Pending;
                                            item.size = 0;
                                        }
                                    }
                                    partial_bytes.lock().unwrap().remove(&seg_idx);
                                    return Err(CoreError::aborted());
                                }
                                let retryable = err.is_retryable_http();
                                failure = Some(err);
                                if !retryable {
                                    break;
                                }
                                if attempt < 2 {
                                    tokio::select! {
                                        _ = child_cancel.cancelled() => return Err(CoreError::aborted()),
                                        _ = tokio::time::sleep(Duration::from_millis(1000 * (attempt as u64 + 1))) => {}
                                    }
                                }
                            }
                        }
                    }

                    partial_bytes.lock().unwrap().remove(&seg_idx);
                    if let Some(err) = failure {
                        let msg = err.to_string();
                        let is_expired = err.as_http().is_some_and(|h| h.expired);
                        {
                            let mut st = shared_stats.lock().unwrap();
                            st.ts_failed = Some(st.ts_failed.unwrap_or(0) + 1);
                        }
                        {
                            let mut s = snap_arc.lock().unwrap();
                            if let Some(item) = s.items.get_mut(seg_idx) {
                                item.status = SegmentStatus::Error;
                                item.size = 0;
                                item.error = Some(msg.clone());
                            }
                        }
                        {
                            let mut ff = first_failure.lock().unwrap();
                            if ff.is_empty() {
                                *ff = msg.clone();
                            }
                        }
                        if is_expired {
                            let mut ef = expired_failure.lock().unwrap();
                            if ef.is_empty() {
                                *ef = msg;
                            }
                        }
                    } else {
                        {
                            let mut s = snap_arc.lock().unwrap();
                            if let Some(item) = s.items.get_mut(seg_idx) {
                                item.status = SegmentStatus::Done;
                                item.size = final_size;
                            }
                        }
                        completed_bytes.fetch_add(final_size, Ordering::Relaxed);
                        let mut st = shared_stats.lock().unwrap();
                        st.ts_success = Some(st.ts_success.unwrap_or(0) + 1);
                    }

                    let snap = refresh_stats();
                    if let Some(ref cb) = on_progress {
                        if !cb(snap) {
                            child_cancel.cancel();
                        }
                    }
                    dirty.store(true, Ordering::Relaxed);
                }
            }));
        }

        let mut worker_err: Option<CoreError> = None;
        for h in handles {
            match h.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if worker_err.is_none() {
                        worker_err = Some(e);
                    }
                }
                Err(e) => {
                    if worker_err.is_none() {
                        worker_err = Some(CoreError::msg(e.to_string()));
                    }
                }
            }
        }

        tick_cancel.cancel();
        let _ = tick_handle.await;

        if child_cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        if let Some(e) = worker_err {
            return Err(e);
        }

        {
            let _g = write_lock.lock().await;
            let snapshot_clone = snap_arc.lock().unwrap().clone();
            write_json_atomic(&state_path, &snapshot_clone).await?;
        }

        stats = shared_stats.lock().unwrap().clone();
        let failed_count = stats.ts_failed.unwrap_or(0);
        if failed_count > 0 {
            let exp_msg = expired_failure.lock().unwrap().clone();
            let fst_msg = first_failure.lock().unwrap().clone();
            let detail = if !exp_msg.is_empty() {
                format!("。{exp_msg}")
            } else {
                format!("，可继续下载重试。{fst_msg}")
            };
            return Err(CoreError::msg(format!("{failed_count} 个分片下载失败{detail}")));
        }

        let mut out_filepath: Option<PathBuf> = None;
        if formatted.convert {
            stats.phase = Some(TaskPhase::Merging);
            stats.speed = Some(0.0);
            stats.speed_desc = Some(String::new());
            stats.remaining_time = Some(0);
            report(&stats);

            let stats_for_merge = Arc::new(StdMutex::new(stats.clone()));
            let merge_cb_user = opts.on_merge_progress.clone();
            let prog_cb_user = opts.on_progress.clone();
            let stats_for_cb = stats_for_merge.clone();
            let cancel_for_cb = child_cancel.clone();

            let merge_cb: crate::contract::MergeProgressCallback = Arc::new(move |pct, msg| {
                let snap = {
                    let mut s = stats_for_cb.lock().unwrap();
                    s.merge_progress = Some(pct);
                    s.merge_message = Some(msg.clone());
                    s.clone()
                };
                if let Some(ref cb) = prog_cb_user {
                    if !cb(snap) {
                        cancel_for_cb.cancel();
                    }
                }
                if let Some(ref cb) = merge_cb_user {
                    cb(pct, msg);
                }
            });

            let convert_opts = ConvertOptions {
                filename: formatted.filename.clone(),
                save_dir: formatted.save_dir.clone(),
                ffmpeg_path: opts.ffmpeg_path.clone().unwrap_or_default(),
                merge_timeout_secs: opts.merge_timeout_secs.unwrap_or(180.0),
                force: formatted.force,
                on_merge_progress: Some(merge_cb),
            };

            let merged_path = m3u8_convert(&convert_opts, &info_arc, &child_cancel).await?;
            stats = stats_for_merge.lock().unwrap().clone();

            let meta = tokio::fs::metadata(&merged_path).await.map_err(|_| {
                CoreError::msg("合并失败，分片缓存已保留，可继续任务重试")
            })?;
            stats.size = Some(meta.len());
            stats.local_video = Some(merged_path.to_string_lossy().into_owned());
            out_filepath = Some(merged_path);

            if formatted.del_cache {
                if cache_dir != formatted.cache_dir && cache_dir.starts_with(&formatted.cache_dir) {
                    let _g = write_lock.lock().await;
                    let _ = tokio::fs::remove_dir_all(&cache_dir).await;
                    cache_removed = true;
                }
            }
        }

        stats.phase = Some(TaskPhase::Done);
        stats.progress = Some(100.0);
        stats.merge_progress = Some(100.0);
        stats.end_time = Some(now_ms());
        report(&stats);

        Ok(out_filepath)
    }
    .await;

    if let Some(ref snap_arc) = segment_state {
        if !cache_removed {
            let snapshot_clone = {
                let mut s = snap_arc.lock().unwrap();
                for item in &mut s.items {
                    if item.status == SegmentStatus::Downloading {
                        item.status = SegmentStatus::Pending;
                        item.size = 0;
                    }
                }
                s.clone()
            };
            let _g = write_lock.lock().await;
            if let Err(e) = write_json_atomic(&state_path, &snapshot_clone).await {
                warn!("分片状态保存失败: {}", e);
            }
        }
    }

    match run_res {
        Ok(fp) => Ok(DownloadResult {
            url,
            filepath: fp.map(|p| p.to_string_lossy().into_owned()),
            errmsg: None,
            is_exist,
            paused: false,
            stats,
            segment_files,
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
                    segment_files,
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
                    segment_files,
                })
            }
        }
    }
}
