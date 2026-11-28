use crate::contract::ScrapePosterCrop;
use crate::error::{CoreError, CoreResult};
use crate::scraper::detail::{DEFAULT_SCRAPER_UA, scraper_http_client};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use url::Url;

const FFMPEG_TIMEOUT_SECS: u64 = 15;
const REQUEST_TIMEOUT_SECS: u64 = 20;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub async fn write_atomic_bytes(target_path: &Path, data: &[u8]) -> CoreResult<()> {
    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        target_path.display(),
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::rename(&tmp, target_path).await?;
    Ok(())
}

pub fn is_webp_buffer(buf: &[u8], content_type: &str) -> bool {
    if buf.len() >= 12 && &buf[0..4] == b"RIFF" && &buf[8..12] == b"WEBP" {
        return true;
    }
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("image/webp")
}

pub fn is_valid_image_response(content_type: &str, buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ct.starts_with("text/")
        || matches!(
            ct.as_str(),
            "application/json" | "application/xml" | "application/xhtml+xml" | "image/svg+xml"
        )
    {
        return false;
    }
    let head_bytes = &buf[..buf.len().min(64)];
    let head = String::from_utf8_lossy(head_bytes)
        .trim_start()
        .to_ascii_lowercase();
    if head.starts_with("<!doctype")
        || head.starts_with("<html")
        || head.starts_with("<?xml")
        || head.starts_with("<svg")
        || head.starts_with('{')
    {
        return false;
    }
    if ct.starts_with("image/") {
        return true;
    }
    let is_jpeg = buf.len() >= 3 && buf[0] == 0xff && buf[1] == 0xd8 && buf[2] == 0xff;
    let is_png = buf.len() >= 4 && &buf[0..4] == b"\x89PNG";
    let is_gif = buf.len() >= 6 && (&buf[0..6] == b"GIF87a" || &buf[0..6] == b"GIF89a");
    let is_avif = buf.len() >= 12 && &buf[4..8] == b"ftyp";
    is_jpeg || is_png || is_webp_buffer(buf, &ct) || is_gif || is_avif
}

async fn run_ffmpeg_image(
    source_path: &Path,
    target_path: &Path,
    vf_expr: Option<&str>,
    ffmpeg_path: &str,
    cancel: &CancellationToken,
) -> bool {
    let bin = if ffmpeg_path.trim().is_empty() {
        "ffmpeg"
    } else {
        ffmpeg_path.trim()
    };
    let mut cmd = Command::new(bin);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-i"])
        .arg(source_path);
    if let Some(vf) = vf_expr {
        cmd.args(["-vf", vf]);
    }
    cmd.args(["-frames:v", "1", "-q:v", "2", "-y"])
        .arg(target_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let Ok(mut child) = cmd.spawn() else {
        return false;
    };

    let res = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            false
        }
        _ = tokio::time::sleep(Duration::from_secs(FFMPEG_TIMEOUT_SECS)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            false
        }
        st = child.wait() => {
            match st {
                Ok(s) if s.success() && !cancel.is_cancelled() => {
                    tokio::fs::metadata(target_path).await.is_ok_and(|m| m.len() > 0)
                }
                _ => false,
            }
        }
    };
    res
}

async fn crop_poster_with_ffmpeg(
    source_path: &Path,
    target_path: &Path,
    crop_mode: ScrapePosterCrop,
    ffmpeg_path: &str,
    cancel: &CancellationToken,
) -> bool {
    if crop_mode == ScrapePosterCrop::None {
        return false;
    }
    let expr = match crop_mode {
        ScrapePosterCrop::Right => {
            "crop=trunc(min(iw\\,ih*2/3)/2)*2:trunc(min(ih\\,iw*3/2)/2)*2:iw-ow:(ih-oh)/2"
        }
        ScrapePosterCrop::Center => {
            "crop=trunc(min(iw\\,ih*2/3)/2)*2:trunc(min(ih\\,iw*3/2)/2)*2:(iw-ow)/2:(ih-oh)/2"
        }
        ScrapePosterCrop::None => return false,
    };
    let tmp_out = PathBuf::from(format!(
        "{}.{}.{}.tmp.jpg",
        target_path.display(),
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    let ok = run_ffmpeg_image(source_path, &tmp_out, Some(expr), ffmpeg_path, cancel).await;
    if ok && tokio::fs::rename(&tmp_out, target_path).await.is_ok() {
        return true;
    }
    let _ = tokio::fs::remove_file(&tmp_out).await;
    false
}

async fn convert_webp_to_jpeg_buffer(
    buf: &[u8],
    ffmpeg_path: &str,
    cancel: &CancellationToken,
) -> Option<Vec<u8>> {
    let tmp_dir = std::env::temp_dir();
    let token = format!(
        "{}.{}.{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let tmp_in = tmp_dir.join(format!("streamdock-cover-{token}.webp"));
    let tmp_out = tmp_dir.join(format!("streamdock-cover-{token}.jpg"));

    let out = async {
        tokio::fs::write(&tmp_in, buf).await.ok()?;
        let ok = run_ffmpeg_image(&tmp_in, &tmp_out, None, ffmpeg_path, cancel).await;
        if !ok {
            return None;
        }
        let converted = tokio::fs::read(&tmp_out).await.ok()?;
        (!converted.is_empty()).then_some(converted)
    }
    .await;

    let _ = tokio::fs::remove_file(&tmp_in).await;
    let _ = tokio::fs::remove_file(&tmp_out).await;
    out
}

#[derive(Debug, Clone, Default)]
pub struct SavedArtwork {
    pub fanart_path: Option<PathBuf>,
    pub thumb_path: Option<PathBuf>,
    pub poster_path: Option<PathBuf>,
    pub download_ms: u64,
    pub crop_ms: u64,
}

pub struct SaveArtworkOptions<'a> {
    pub cover_url: &'a str,
    pub cover_urls: &'a [String],
    pub page_url: &'a str,
    pub referer: Option<&'a str>,
    pub fanart_path: &'a Path,
    pub thumb_path: &'a Path,
    pub poster_path: &'a Path,
    pub poster_crop: ScrapePosterCrop,
    pub overwrite: bool,
    pub ffmpeg_path: &'a str,
}

pub async fn save_artwork(
    opts: SaveArtworkOptions<'_>,
    cancel: &CancellationToken,
) -> CoreResult<SavedArtwork> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for u in opts
        .cover_urls
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(opts.cover_url))
    {
        let t = u.trim();
        if !t.is_empty() && seen.insert(t.to_string()) {
            candidates.push(t.to_string());
        }
    }
    if candidates.is_empty() {
        return Ok(SavedArtwork::default());
    }

    let need_fanart = opts.overwrite
        || !tokio::fs::try_exists(opts.fanart_path)
            .await
            .unwrap_or(false);
    let need_thumb = opts.overwrite
        || !tokio::fs::try_exists(opts.thumb_path)
            .await
            .unwrap_or(false);
    let need_poster = opts.overwrite
        || !tokio::fs::try_exists(opts.poster_path)
            .await
            .unwrap_or(false);
    if !need_fanart && !need_thumb && !need_poster {
        return Ok(SavedArtwork {
            fanart_path: Some(opts.fanart_path.to_path_buf()),
            thumb_path: Some(opts.thumb_path.to_path_buf()),
            poster_path: Some(opts.poster_path.to_path_buf()),
            download_ms: 0,
            crop_ms: 0,
        });
    }

    let dl_start = std::time::Instant::now();
    let page_origin = Url::parse(opts.page_url)?.origin().ascii_serialization();
    let effective_referer = opts
        .referer
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{page_origin}/"));
    let effective_origin = Url::parse(&effective_referer)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or(page_origin);

    let client = scraper_http_client();
    let mut raw_buf: Option<Vec<u8>> = None;
    let mut fetched_ct = String::new();
    let mut last_error: Option<CoreError> = None;

    for cand in candidates {
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let attempt = async {
            let resp = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::aborted()),
                _ = tokio::time::sleep(Duration::from_secs(REQUEST_TIMEOUT_SECS)) => {
                    return Err(CoreError::msg("封面下载超时"));
                }
                r = client
                    .get(&cand)
                    .header("user-agent", DEFAULT_SCRAPER_UA)
                    .header("referer", &effective_referer)
                    .header("origin", &effective_origin)
                    .send() => r?,
            };
            if !resp.status().is_success() {
                return Err(CoreError::msg(format!(
                    "封面下载失败：HTTP {}",
                    resp.status().as_u16()
                )));
            }
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::aborted()),
                b = resp.bytes() => b?,
            };
            if !is_valid_image_response(&ct, &bytes) {
                let short_ct = ct.split(';').next().unwrap_or("").trim();
                return Err(CoreError::msg(format!(
                    "封面返回非图片内容 ({})",
                    if short_ct.is_empty() {
                        "empty"
                    } else {
                        short_ct
                    }
                )));
            }
            Ok((bytes.to_vec(), ct))
        }
        .await;

        match attempt {
            Ok((buf, ct)) => {
                raw_buf = Some(buf);
                fetched_ct = ct;
                break;
            }
            Err(e) => {
                if cancel.is_cancelled() || e.is_aborted() {
                    return Err(CoreError::aborted());
                }
                last_error = Some(e);
            }
        }
    }

    let Some(raw) = raw_buf else {
        return Err(last_error.unwrap_or_else(|| CoreError::msg("封面下载失败")));
    };
    let download_ms = dl_start.elapsed().as_millis() as u64;

    if cancel.is_cancelled() {
        return Err(CoreError::aborted());
    }
    let output_buf = if is_webp_buffer(&raw, &fetched_ct) {
        convert_webp_to_jpeg_buffer(&raw, opts.ffmpeg_path, cancel)
            .await
            .unwrap_or(raw)
    } else {
        raw
    };

    if cancel.is_cancelled() {
        return Err(CoreError::aborted());
    }
    if need_fanart {
        write_atomic_bytes(opts.fanart_path, &output_buf).await?;
    }
    if need_thumb {
        write_atomic_bytes(opts.thumb_path, &output_buf).await?;
    }

    let mut crop_ms = 0u64;
    if need_poster {
        let src_for_crop = if tokio::fs::try_exists(opts.fanart_path)
            .await
            .unwrap_or(false)
        {
            opts.fanart_path
        } else {
            opts.thumb_path
        };
        let mut cropped = false;
        if opts.poster_crop != ScrapePosterCrop::None {
            let crop_start = std::time::Instant::now();
            cropped = crop_poster_with_ffmpeg(
                src_for_crop,
                opts.poster_path,
                opts.poster_crop,
                opts.ffmpeg_path,
                cancel,
            )
            .await;
            crop_ms = crop_start.elapsed().as_millis() as u64;
        }
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        if !cropped {
            write_atomic_bytes(opts.poster_path, &output_buf).await?;
        }
    }

    Ok(SavedArtwork {
        fanart_path: Some(opts.fanart_path.to_path_buf()),
        thumb_path: Some(opts.thumb_path.to_path_buf()),
        poster_path: Some(opts.poster_path.to_path_buf()),
        download_ms,
        crop_ms,
    })
}
