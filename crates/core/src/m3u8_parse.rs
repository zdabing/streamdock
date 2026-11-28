use crate::container::{
    ContainerType, detect_container_by_file, detect_container_by_header, detect_container_type,
};
use crate::contract::{HeaderMap, SegmentIdentityFn};
use crate::crypto::{CryptoKeyInfo, decrypt_segment_data, parse_iv};
use crate::error::{CoreError, CoreResult};
use crate::http::{
    M3u8ByteRange, get_retry, md5_hex, scope_credentials, slice_byterange, with_range_header,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct M3u8MapInfo {
    pub uri: String,
    pub ts_out: PathBuf,
    pub ts_size: u64,
    pub byterange: Option<M3u8ByteRange>,
    pub key_uri: String,
    pub container: ContainerType,
}

#[derive(Debug, Clone)]
pub struct TsItemInfo {
    pub index: usize,
    pub duration: f64,
    pub timeline: f64,
    pub uri: String,
    pub ts_out: PathBuf,
    pub ts_size: u64,
    pub byterange: Option<M3u8ByteRange>,
    pub key_uri: String,
    pub iv: Option<[u8; 16]>,
    pub m3u8: String,
}

#[derive(Debug, Clone)]
pub struct M3u8Info {
    pub ts_count: usize,
    pub duration: f64,
    pub data: Vec<TsItemInfo>,
    pub crypto: HashMap<String, CryptoKeyInfo>,
    pub container: ContainerType,
    pub init_segment: Option<M3u8MapInfo>,
}

#[derive(Debug, Clone)]
struct RawKey {
    method: String,
    uri: String,
    iv: Option<String>,
}

#[derive(Debug, Clone)]
struct RawMap {
    uri: String,
    byterange: Option<M3u8ByteRange>,
    key: Option<RawKey>,
}

#[derive(Debug, Clone)]
struct RawPlaylistVariant {
    uri: String,
    bandwidth: u64,
}

#[derive(Debug, Clone)]
struct RawSegment {
    uri: String,
    duration: f64,
    timeline: f64,
    byterange: Option<M3u8ByteRange>,
    key: Option<RawKey>,
    map: Option<RawMap>,
}

#[derive(Debug, Default)]
struct ParsedManifest {
    playlists: Vec<RawPlaylistVariant>,
    segments: Vec<RawSegment>,
    media_sequence: u64,
}

fn parse_attrs(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            continue;
        }
        let key = raw[key_start..i].trim().to_ascii_uppercase();
        i += 1;
        let val = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let s = raw[val_start..i].to_string();
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
            }
            s
        } else {
            let val_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            raw[val_start..i].trim().to_string()
        };
        map.insert(key, val);
    }
    map
}

fn parse_byterange_str(raw: &str) -> Option<M3u8ByteRange> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((len_s, off_s)) = trimmed.split_once('@') {
        let length = len_s.trim().parse::<u64>().ok()?;
        let offset = off_s.trim().parse::<u64>().ok()?;
        Some(M3u8ByteRange {
            length,
            offset: Some(offset),
        })
    } else {
        let length = trimmed.parse::<u64>().ok()?;
        Some(M3u8ByteRange {
            length,
            offset: None,
        })
    }
}

fn parse_manifest_text(text: &str) -> ParsedManifest {
    let mut out = ParsedManifest::default();
    let mut active_key: Option<RawKey> = None;
    let mut active_map: Option<RawMap> = None;
    let mut next_duration: f64 = 0.0;
    let mut next_byterange: Option<M3u8ByteRange> = None;
    let mut pending_stream_bandwidth: Option<u64> = None;
    let mut timeline: f64 = 0.0;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(tag) = line.strip_prefix('#') {
            if let Some(rest) = tag.strip_prefix("EXT-X-MEDIA-SEQUENCE:") {
                if let Ok(seq) = rest.trim().parse::<u64>() {
                    out.media_sequence = seq;
                }
            } else if let Some(rest) = tag.strip_prefix("EXT-X-STREAM-INF:") {
                let attrs = parse_attrs(rest);
                let bw = attrs
                    .get("BANDWIDTH")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                pending_stream_bandwidth = Some(bw);
            } else if let Some(rest) = tag.strip_prefix("EXT-X-KEY:") {
                let attrs = parse_attrs(rest);
                let method = attrs.get("METHOD").cloned().unwrap_or_default();
                if method.eq_ignore_ascii_case("NONE") {
                    active_key = None;
                } else {
                    active_key = Some(RawKey {
                        method,
                        uri: attrs.get("URI").cloned().unwrap_or_default(),
                        iv: attrs.get("IV").cloned(),
                    });
                }
            } else if let Some(rest) = tag.strip_prefix("EXT-X-MAP:") {
                let attrs = parse_attrs(rest);
                if let Some(uri) = attrs.get("URI").filter(|s| !s.is_empty()) {
                    active_map = Some(RawMap {
                        uri: uri.clone(),
                        byterange: attrs.get("BYTERANGE").and_then(|s| parse_byterange_str(s)),
                        key: active_key.clone(),
                    });
                }
            } else if let Some(rest) = tag.strip_prefix("EXTINF:") {
                let dur_str = rest.split(',').next().unwrap_or("").trim();
                next_duration = dur_str.parse::<f64>().unwrap_or(0.0).max(0.0);
            } else if let Some(rest) = tag.strip_prefix("EXT-X-BYTERANGE:") {
                next_byterange = parse_byterange_str(rest);
            }
            continue;
        }

        if let Some(bw) = pending_stream_bandwidth.take() {
            out.playlists.push(RawPlaylistVariant {
                uri: line.to_string(),
                bandwidth: bw,
            });
            continue;
        }

        out.segments.push(RawSegment {
            uri: line.to_string(),
            duration: next_duration,
            timeline,
            byterange: next_byterange.take(),
            key: active_key.clone(),
            map: active_map.clone(),
        });
        timeline += next_duration;
        next_duration = 0.0;
    }
    out
}

fn resolve_uri(base: &str, relative: &str) -> CoreResult<String> {
    if relative.contains("://") {
        return Ok(relative.to_string());
    }
    if let Ok(base_url) = Url::parse(base) {
        if let Ok(joined) = base_url.join(relative) {
            return Ok(joined.to_string());
        }
    }
    let base_path = Path::new(base);
    let resolved = base_path
        .parent()
        .map(|p| p.join(relative))
        .unwrap_or_else(|| PathBuf::from(relative));
    Ok(resolved.to_string_lossy().into_owned())
}

fn headers_for(
    credential_source: Option<&str>,
    target: &str,
    headers: &HeaderMap,
) -> CoreResult<HeaderMap> {
    match credential_source {
        Some(src) => scope_credentials(src, target, headers),
        None => Ok(headers.clone()),
    }
}

async fn fetch_key_info(
    client: &reqwest::Client,
    method: &str,
    uri: &str,
    crypto: &mut HashMap<String, CryptoKeyInfo>,
    iv: Option<&str>,
    headers: &HeaderMap,
    cancel: &CancellationToken,
) -> CoreResult<()> {
    if uri.is_empty() || crypto.contains_key(uri) {
        return Ok(());
    }
    if !method.eq_ignore_ascii_case("AES-128") {
        return Err(CoreError::msg(format!(
            "暂不支持加密方式 {method}，不能直接下载合并"
        )));
    }
    let resp = get_retry(client, uri, headers, 3, cancel, None).await?;
    if resp.status != 200 {
        return Ok(());
    }
    if resp.body.len() != 16 {
        return Err(CoreError::msg(
            "AES-128 密钥长度无效，请检查 Referer 或 Cookie",
        ));
    }
    let mut key = [0u8; 16];
    key.copy_from_slice(&resp.body);
    crypto.insert(
        uri.to_string(),
        CryptoKeyInfo {
            uri: uri.to_string(),
            method: method.to_ascii_uppercase(),
            key,
            iv: iv.map(|s| parse_iv(Some(s), 0)),
        },
    );
    Ok(())
}

async fn fetch_init_segment(
    client: &reqwest::Client,
    map: &RawMap,
    base_url: &str,
    cache_dir: &Path,
    crypto: &mut HashMap<String, CryptoKeyInfo>,
    credential_source: Option<&str>,
    base_headers: &HeaderMap,
    cancel: &CancellationToken,
) -> CoreResult<Option<M3u8MapInfo>> {
    let uri = resolve_uri(base_url, &map.uri)?;
    let mut key_uri = String::new();
    let res: CoreResult<Option<M3u8MapInfo>> = async {
        if let Some(ref k) = map.key {
            if !k.uri.is_empty() {
                key_uri = resolve_uri(base_url, &k.uri)?;
                let h = headers_for(credential_source, &key_uri, base_headers)?;
                fetch_key_info(
                    client,
                    &k.method,
                    &key_uri,
                    crypto,
                    k.iv.as_deref(),
                    &h,
                    cancel,
                )
                .await?;
            }
        }
        let ts_out = cache_dir.join(format!("init-{}.mp4", md5_hex(&uri)));
        if let Ok(meta) = tokio::fs::metadata(&ts_out).await {
            if meta.len() > 0 {
                let container = detect_container_by_file(&ts_out);
                return Ok(Some(M3u8MapInfo {
                    uri: uri.clone(),
                    ts_out,
                    ts_size: meta.len(),
                    byterange: map.byterange,
                    key_uri,
                    container,
                }));
            }
        }
        let h = with_range_header(
            &headers_for(credential_source, &uri, base_headers)?,
            map.byterange,
        );
        let resp = get_retry(client, &uri, &h, 3, cancel, None).await?;
        if resp.body.is_empty() {
            return Ok(None);
        }
        let init_buf = slice_byterange(resp.body, map.byterange, resp.status);
        let key_info = if key_uri.is_empty() {
            None
        } else {
            crypto.get(&key_uri)
        };
        let data = decrypt_segment_data(&init_buf, key_info, None)?;
        let detected = detect_container_by_header(&data[..data.len().min(256)]);
        let container = if detected != ContainerType::Unknown {
            detected
        } else {
            ContainerType::Fmp4
        };
        tokio::fs::create_dir_all(cache_dir).await?;
        let tmp = cache_dir.join(format!("init-{}.mp4.part", md5_hex(&uri)));
        let write_res: CoreResult<()> = async {
            tokio::fs::write(&tmp, &data).await?;
            if cancel.is_cancelled() {
                return Err(CoreError::aborted());
            }
            tokio::fs::rename(&tmp, &ts_out).await?;
            Ok(())
        }
        .await;
        if write_res.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            write_res?;
        }
        Ok(Some(M3u8MapInfo {
            uri: uri.clone(),
            ts_out,
            ts_size: data.len() as u64,
            byterange: map.byterange,
            key_uri,
            container,
        }))
    }
    .await;

    res.map_err(|e| {
        if e.is_aborted() {
            e
        } else {
            CoreError::msg(format!("初始化分片下载失败: {e}"))
        }
    })
}

fn resolve_container(init: Option<&M3u8MapInfo>, first_seg_uri: &str) -> ContainerType {
    if let Some(m) = init {
        if m.container != ContainerType::Unknown {
            return m.container;
        }
    }
    let uri = init.map(|m| m.uri.as_str()).unwrap_or(first_seg_uri);
    let by_uri = detect_container_type(uri);
    if by_uri != ContainerType::Unknown {
        return by_uri;
    }
    if init.is_some() {
        ContainerType::Fmp4
    } else {
        ContainerType::Ts
    }
}

pub async fn parse_m3u8(
    client: &reqwest::Client,
    input: &str,
    cache_dir: &Path,
    headers: &HeaderMap,
    cancel: &CancellationToken,
    identity: Option<&SegmentIdentityFn>,
) -> CoreResult<M3u8Info> {
    let lower = input.to_ascii_lowercase();
    let credential_source =
        (lower.starts_with("http://") || lower.starts_with("https://")).then(|| input.to_string());

    let (mut current_url, mut content) = if credential_source.is_some() {
        let resp = get_retry(client, input, headers, 3, cancel, None).await?;
        let text = resp.text();
        (resp.url, text)
    } else if !input.contains('\n') && Path::new(input).exists() {
        let abs = std::fs::canonicalize(input).unwrap_or_else(|_| PathBuf::from(input));
        let text = tokio::fs::read_to_string(&abs).await?;
        (abs.to_string_lossy().into_owned(), text)
    } else {
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("playlist.m3u8");
        (cwd.to_string_lossy().into_owned(), input.to_string())
    };

    if !content.trim_start().starts_with("#EXTM3U") {
        return Err(CoreError::msg(
            "响应不是有效的 M3U8 播放列表，请检查链接和请求头",
        ));
    }
    debug!("starting parsing m3u8 file: {}", current_url);

    let mut manifest = parse_manifest_text(&content);
    let mut depth = 0;
    while !manifest.playlists.is_empty() {
        depth += 1;
        if depth > 5 {
            return Err(CoreError::msg("播放列表嵌套过深"));
        }
        let mut best = &manifest.playlists[0];
        for p in &manifest.playlists[1..] {
            if p.bandwidth > best.bandwidth {
                best = p;
            }
        }
        let next_uri = resolve_uri(&current_url, &best.uri)?;
        let h = headers_for(credential_source.as_deref(), &next_uri, headers)?;
        let resp = get_retry(client, &next_uri, &h, 3, cancel, None).await?;
        content = resp.text();
        current_url = resp.url;
        if !content.trim_start().starts_with("#EXTM3U") {
            return Err(CoreError::msg("子播放列表无效，请检查请求头"));
        }
        manifest = parse_manifest_text(&content);
    }

    if manifest.segments.is_empty() {
        return Err(CoreError::msg("播放列表中没有视频分片"));
    }

    let map_uris: Vec<&str> = manifest
        .segments
        .iter()
        .filter_map(|s| s.map.as_ref().map(|m| m.uri.as_str()))
        .collect();
    if map_uris.len() > 1 {
        let uniq: HashSet<&str> = map_uris.iter().copied().collect();
        if uniq.len() > 1 {
            warn!("检测到多个 EXT-X-MAP 初始化段，当前仅使用第一个");
        }
    }
    let first_map = manifest.segments.iter().find_map(|s| s.map.clone());
    let mut crypto = HashMap::new();
    let init_segment = match first_map {
        Some(m) if !m.uri.is_empty() => {
            fetch_init_segment(
                client,
                &m,
                &current_url,
                cache_dir,
                &mut crypto,
                credential_source.as_deref(),
                headers,
                cancel,
            )
            .await?
        }
        _ => None,
    };

    let first_resolved_uri = resolve_uri(&current_url, &manifest.segments[0].uri)?;
    let container = resolve_container(init_segment.as_ref(), &first_resolved_uri);
    let segment_ext = container.segment_ext();

    let mut last_byte_end: HashMap<String, u64> = HashMap::new();
    let mut items = Vec::with_capacity(manifest.segments.len());
    let mut total_duration = 0.0_f64;

    for (i, seg) in manifest.segments.into_iter().enumerate() {
        let seg_uri = resolve_uri(&current_url, &seg.uri)?;
        let mut key_uri = String::new();
        let mut seg_iv = None;
        if let Some(ref k) = seg.key {
            if !k.uri.is_empty() {
                key_uri = resolve_uri(&current_url, &k.uri)?;
                let h = headers_for(credential_source.as_deref(), &key_uri, headers)?;
                fetch_key_info(
                    client,
                    &k.method,
                    &key_uri,
                    &mut crypto,
                    k.iv.as_deref(),
                    &h,
                    cancel,
                )
                .await?;
            }
            seg_iv = Some(parse_iv(
                k.iv.as_deref(),
                manifest.media_sequence + i as u64,
            ));
        }

        let mut byterange = seg.byterange;
        if let Some(ref mut br) = byterange {
            if br.offset.is_none() {
                if let Some(&prev) = last_byte_end.get(&seg_uri) {
                    br.offset = Some(prev);
                }
            }
            last_byte_end.insert(seg_uri.clone(), br.offset.unwrap_or(0) + br.length);
        }

        let res_id = identity
            .map(|f| f(&seg_uri))
            .unwrap_or_else(|| seg_uri.clone());
        let name_seed = match byterange {
            Some(br) => format!("{res_id}@{}+{}", br.offset.unwrap_or(0), br.length),
            None => res_id,
        };
        let ts_out = cache_dir.join(format!("{}{}", md5_hex(&name_seed), segment_ext));
        let timeline = if seg.timeline > 0.0 {
            seg.timeline
        } else {
            total_duration
        };
        items.push(TsItemInfo {
            index: i,
            duration: seg.duration,
            timeline,
            uri: seg_uri,
            ts_out,
            ts_size: 0,
            byterange,
            key_uri,
            iv: seg_iv,
            m3u8: current_url.clone(),
        });
        total_duration += seg.duration;
    }

    let rounded_dur = (total_duration * 100.0).round() / 100.0;
    Ok(M3u8Info {
        ts_count: items.len(),
        duration: rounded_dur,
        data: items,
        crypto,
        container,
        init_segment,
    })
}
