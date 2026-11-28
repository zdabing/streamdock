use crate::contract::{DownloadExecOptions, HeaderMap, InputTypeKind};
use crate::error::{CoreError, CoreResult};
use crate::http::{
    decode_b64_utf8, format_headers, md5_hex, merge_header_maps, normalize_download_url,
};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use url::Url;

pub const DIRECT_FILE_EXTENSIONS: &[&str] = &[
    ".mp4", ".mkv", ".mov", ".m4v", ".avi", ".flv", ".webm", ".wmv", ".ts", ".m4s", ".mp3", ".wav",
    ".ogg", ".m4a", ".aac", ".flac", ".opus", ".weba", ".vtt", ".srt", ".ass", ".ssa",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPlatformKind {
    Douyin,
    Weibo,
    Pipixia,
    SignedSite,
    Unknown,
}

impl VideoPlatformKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Douyin => "douyin",
            Self::Weibo => "weibo",
            Self::Pipixia => "pipixia",
            Self::SignedSite => "signed_site",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectedPlatform {
    pub url: String,
    pub platform: VideoPlatformKind,
    pub errmsg: Option<String>,
}

pub fn source_url(input: &str) -> CoreResult<String> {
    let text = input.trim();
    if text.is_empty() {
        return Err(CoreError::msg(
            "请填写 http/https 链接，或包含链接的分享文本",
        ));
    }
    static BAD_CHARS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"[\s<>"'，。！？、）]"#).unwrap());
    let lower = text.to_ascii_lowercase();
    if (lower.starts_with("http://") || lower.starts_with("https://")) && !BAD_CHARS.is_match(text)
    {
        return normalize_download_url(text);
    }
    static EXTRACT_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s<>"'，。！？、）]+"#).unwrap());
    let Some(m) = EXTRACT_RX.find(text) else {
        return Err(CoreError::msg(
            "请填写 http/https 链接，或包含链接的分享文本",
        ));
    };
    static TRIM_TAIL_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[)\]，。；;]+$").unwrap());
    let candidate = TRIM_TAIL_RX.replace(m.as_str(), "");
    normalize_download_url(&candidate)
}

fn match_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

pub fn detect_platform(input: &str) -> DetectedPlatform {
    match source_url(input) {
        Ok(url) => {
            let host = Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                .unwrap_or_default();
            let platform =
                if match_domain(&host, "douyin.com") || match_domain(&host, "iesdouyin.com") {
                    VideoPlatformKind::Douyin
                } else if match_domain(&host, "weibo.com")
                    || match_domain(&host, "weibo.cn")
                    || match_domain(&host, "t.cn")
                {
                    VideoPlatformKind::Weibo
                } else if match_domain(&host, "pipix.com") {
                    VideoPlatformKind::Pipixia
                } else if {
                    static SIGNED_SITE_DOMAIN: LazyLock<String> =
                        LazyLock::new(|| decode_b64_utf8("amF2cmF0ZS5jb20="));
                    match_domain(&host, &SIGNED_SITE_DOMAIN)
                } {
                    VideoPlatformKind::SignedSite
                } else {
                    VideoPlatformKind::Unknown
                };
            DetectedPlatform {
                url,
                platform,
                errmsg: None,
            }
        }
        Err(e) => DetectedPlatform {
            url: String::new(),
            platform: VideoPlatformKind::Unknown,
            errmsg: Some(e.to_string()),
        },
    }
}

#[derive(Debug, Clone)]
pub struct FormattedOptions {
    pub url: String,
    pub url_md5: String,
    pub download_type: InputTypeKind,
    pub filename: String,
    pub save_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub thread_num: usize,
    pub del_cache: bool,
    pub convert: bool,
    pub force: bool,
    pub headers: HeaderMap,
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                || (c as u32) < 0x20
            {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn url_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("").to_string()
}

pub fn percent_decode_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn path_extname_lower(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let Some(dot) = base.rfind('.') else {
        return String::new();
    };
    if dot == 0 {
        return String::new();
    }
    base[dot..].to_ascii_lowercase()
}

pub fn format_options(input: &str, opts: &DownloadExecOptions) -> CoreResult<FormattedOptions> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    let url = if (lower.starts_with("http://") || lower.starts_with("https://"))
        && !trimmed.chars().any(char::is_whitespace)
    {
        normalize_download_url(input)?
    } else if lower.contains("http://") || lower.contains("https://") {
        source_url(input)?
    } else {
        input.to_string()
    };

    let remote_name = Url::parse(&url)
        .ok()
        .map(|u| url_basename(&percent_decode_lossy(u.path())))
        .unwrap_or_default();
    let remote_ext = path_extname_lower(&remote_name);

    let download_type = match opts.download_type {
        Some(t) if t != InputTypeKind::Auto => t,
        _ => {
            if detect_platform(&url).platform != VideoPlatformKind::Unknown {
                InputTypeKind::Parser
            } else if DIRECT_FILE_EXTENSIONS.contains(&remote_ext.as_str()) {
                InputTypeKind::File
            } else {
                InputTypeKind::M3u8
            }
        }
    };

    let raw_name = opts
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if download_type == InputTypeKind::File && !remote_name.is_empty() {
                Some(remote_name.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| md5_hex(&url));

    let base_name = raw_name.rsplit(['/', '\\']).next().unwrap_or(&raw_name);
    let filename = sanitize_filename(base_name);

    let mut known_suffixes: Vec<&str> = DIRECT_FILE_EXTENSIONS.to_vec();
    known_suffixes.push(".txt");
    let lower_name = filename.to_ascii_lowercase();
    let has_known_ext = known_suffixes.iter().any(|s| lower_name.ends_with(s));
    let usable_remote_ext = if known_suffixes.contains(&remote_ext.as_str()) {
        remote_ext.as_str()
    } else {
        ""
    };

    let final_filename = if has_known_ext {
        filename
    } else if download_type == InputTypeKind::File {
        format!(
            "{filename}{}",
            if usable_remote_ext.is_empty() {
                ".bin"
            } else {
                usable_remote_ext
            }
        )
    } else {
        format!("{filename}.mp4")
    };

    let save_dir_raw = opts
        .save_dir
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("./downloads");
    let cache_dir_raw = opts
        .cache_dir
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("./cache");
    let save_dir = absolutize(Path::new(save_dir_raw));
    let cache_dir = absolutize(Path::new(cache_dir_raw));

    let mut parsed_headers = format_headers(opts.headers.as_ref())?;
    if let Some(ref extra) = opts.extra_headers {
        parsed_headers = merge_header_maps(&parsed_headers, extra);
    }

    Ok(FormattedOptions {
        url_md5: md5_hex(&url),
        url,
        download_type,
        filename: final_filename,
        save_dir,
        cache_dir,
        thread_num: opts.thread_num.unwrap_or(4).clamp(1, 16),
        del_cache: opts.del_cache.unwrap_or(true),
        convert: opts.convert.unwrap_or(true),
        force: opts.force,
        headers: parsed_headers,
    })
}

pub fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        clean_lexically(path)
    } else if let Ok(cwd) = std::env::current_dir() {
        clean_lexically(&cwd.join(path))
    } else {
        path.to_path_buf()
    }
}

fn clean_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push(comp);
                }
            }
            c => out.push(c),
        }
    }
    out
}
