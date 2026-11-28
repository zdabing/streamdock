use crate::contract::{HeaderInput, InputTypeKind, TaskOptions};
use crate::error::{CoreError, CoreResult};
use crate::format_options::{DIRECT_FILE_EXTENSIONS, path_extname_lower};
use crate::http::{is_valid_header_value_str, md5_hex, normalize_download_url};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use url::Url;

#[derive(Debug, Clone)]
pub struct CatCatchTaskInput {
    pub url: String,
    pub options: TaskOptions,
}

#[derive(Debug, Deserialize)]
struct RawCatCatchPayload {
    action: Option<String>,
    data: Option<serde_json::Value>,
}

fn mime_to_ext(mime_raw: &str) -> String {
    let mime = mime_raw
        .to_ascii_lowercase()
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if mime.is_empty() {
        return String::new();
    }
    let mapped = match mime.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/x-mp3" | "audio/x-mpeg" => ".mp3",
        "audio/mp4" | "audio/x-m4a" => ".m4a",
        "audio/aacp" | "audio/x-aac" => ".aac",
        "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => ".wav",
        "audio/x-flac" => ".flac",
        "audio/webm" => ".weba",
        "video/x-matroska" => ".mkv",
        "video/quicktime" => ".mov",
        "text/vtt" => ".vtt",
        "application/x-subrip" | "text/x-srt" => ".srt",
        "text/x-ass" => ".ass",
        "text/x-ssa" => ".ssa",
        _ => "",
    };
    if !mapped.is_empty() {
        return mapped.to_string();
    }
    match mime.split_once('/') {
        Some((_, sub)) if !sub.is_empty() => format!(".{sub}"),
        _ => String::new(),
    }
}

fn extract_captured_headers(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> CoreResult<BTreeMap<String, serde_json::Value>> {
    let supported: HashSet<&'static str> =
        ["referer", "origin", "cookie", "user-agent", "authorization"]
            .into_iter()
            .collect();
    let mut pairs: Vec<(String, String)> = Vec::new();

    for key in ["referer", "origin", "cookie", "user-agent", "authorization"] {
        if let Some(val) = obj.get(key).and_then(|v| v.as_str()) {
            pairs.push((key.to_string(), val.to_string()));
        }
    }
    if let Some(val) = obj.get("userAgent").and_then(|v| v.as_str()) {
        pairs.push(("user-agent".to_string(), val.to_string()));
    }
    if let Some(rh) = obj.get("requestHeaders") {
        match rh {
            serde_json::Value::Array(arr) => {
                for entry in arr {
                    if let (Some(n), Some(v)) = (
                        entry.get("name").and_then(|x| x.as_str()),
                        entry.get("value").and_then(|x| x.as_str()),
                    ) {
                        pairs.push((n.to_string(), v.to_string()));
                    }
                }
            }
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        pairs.push((k.clone(), s.to_string()));
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = BTreeMap::new();
    for (k, v) in pairs {
        let name = k.trim().to_ascii_lowercase();
        if !supported.contains(name.as_str()) {
            continue;
        }
        let trimmed = v.trim();
        if trimmed.is_empty() && name != "referer" {
            continue;
        }
        if !is_valid_header_value_str(&v) {
            return Err(CoreError::msg(format!("猫抓请求头无效: {name}")));
        }
        let stored = if name == "referer" && trimmed.is_empty() {
            String::new()
        } else {
            v
        };
        out.insert(name, serde_json::Value::String(stored));
    }
    Ok(out)
}

pub fn cat_catch_tasks(payload: &serde_json::Value) -> CoreResult<Vec<CatCatchTaskInput>> {
    let Some(obj) = payload.as_object() else {
        return Err(CoreError::msg("仅支持猫抓资源发送（action: catch）"));
    };
    let parsed: RawCatCatchPayload = serde_json::from_value(serde_json::Value::Object(obj.clone()))
        .map_err(|_| CoreError::msg("仅支持猫抓资源发送（action: catch）"))?;
    if parsed.action.as_deref() != Some("catch") {
        return Err(CoreError::msg("仅支持猫抓资源发送（action: catch）"));
    }
    let Some(data_val) = parsed.data else {
        return Err(CoreError::msg("每次请发送 1–100 条猫抓资源"));
    };
    let resources: Vec<&serde_json::Value> = match &data_val {
        serde_json::Value::Array(arr) => arr.iter().collect(),
        serde_json::Value::Object(_) => vec![&data_val],
        _ => return Err(CoreError::msg("猫抓资源缺少下载链接")),
    };
    if resources.is_empty() || resources.len() > 100 {
        return Err(CoreError::msg("每次请发送 1–100 条猫抓资源"));
    }

    let mut tasks = Vec::with_capacity(resources.len());
    for item_val in resources {
        let Some(item) = item_val.as_object() else {
            return Err(CoreError::msg("猫抓资源缺少下载链接"));
        };
        let Some(raw_url) = item.get("url").and_then(|v| v.as_str()) else {
            return Err(CoreError::msg("猫抓资源缺少下载链接"));
        };
        let url = normalize_download_url(raw_url)?;
        let source_url = match item.get("sourceUrl").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => Some(normalize_download_url(s)?),
            _ => None,
        };
        let headers = extract_captured_headers(item)?;

        let mime_raw = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mime = mime_raw
            .to_ascii_lowercase()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let mime_ext = mime_to_ext(mime_raw);

        let ext_raw = item
            .get("ext")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let hint_ext = if ext_raw.is_empty() {
            String::new()
        } else {
            format!(".{}", ext_raw.to_ascii_lowercase().trim_start_matches('.'))
        };

        let parsed_url = Url::parse(&url)?;
        let url_path_name = parsed_url.path().rsplit('/').next().unwrap_or("");
        let url_ext = path_extname_lower(url_path_name);

        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("fullFileName").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        let name_ext = path_extname_lower(&name);

        let hints = [&hint_ext, &url_ext, &name_ext];
        if hints.iter().any(|h| *h == ".mpd") || mime == "application/dash+xml" {
            return Err(CoreError::msg("当前下载器不支持 DASH/MPD"));
        }

        let is_hls =
            mime.contains("mpegurl") || hints.iter().any(|h| *h == ".m3u8" || *h == ".m3u");
        let file_ext = hints
            .iter()
            .map(|h| h.as_str())
            .chain(std::iter::once(mime_ext.as_str()))
            .find(|ext| DIRECT_FILE_EXTENSIONS.contains(ext))
            .map(str::to_string);

        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let raw_base = if !name.is_empty() {
            name.as_str()
        } else if !title.is_empty() {
            title
        } else if !url_path_name.is_empty() {
            url_path_name
        } else {
            ""
        };
        let fallback_md5 = md5_hex(&url);
        let base_stem = if raw_base.is_empty() {
            fallback_md5.as_str()
        } else {
            raw_base.rsplit(['/', '\\']).next().unwrap_or(raw_base)
        };
        let base: String = base_stem.chars().take(120).collect();

        let filename = if is_hls && !base.is_empty() {
            let trimmed = strip_hls_suffix(&base);
            Some(format!("{trimmed}.mp4"))
        } else if let Some(ref ext) = file_ext {
            if !base.is_empty() && !base.to_ascii_lowercase().ends_with(ext.as_str()) {
                Some(format!("{base}{ext}"))
            } else if base.is_empty() {
                None
            } else {
                Some(base)
            }
        } else if base.is_empty() {
            None
        } else {
            Some(base)
        };

        let task_type = if !is_hls && file_ext.is_some() {
            InputTypeKind::File
        } else {
            InputTypeKind::M3u8
        };

        tasks.push(CatCatchTaskInput {
            url: url.clone(),
            options: TaskOptions {
                use_default_headers: Some(false),
                download_type: Some(task_type),
                source_url,
                header_source_url: Some(url),
                filename,
                save_dir: None,
                ignore_segments: None,
                headers: Some(HeaderInput::Map(headers)),
                scrape_meta: None,
                retry_failed_only: None,
            },
        });
    }

    Ok(tasks)
}

fn strip_hls_suffix(base: &str) -> &str {
    let lower = base.to_ascii_lowercase();
    for suffix in [".m3u8", ".m3u", ".txt", ".mp4"] {
        if lower.ends_with(suffix) {
            return &base[..base.len() - suffix.len()];
        }
    }
    base
}
