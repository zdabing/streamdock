use crate::contract::{
    DownloadTypeKind, ExtractedMedia, ExtractionErrorItem, ExtractionResult, HeaderMap,
};
use crate::error::{CoreError, CoreResult};
use crate::format_options::{percent_decode_lossy, source_url};
use crate::http::{global_http_client, headers_for_target, normalize_download_url};
use crate::video_parser::VideoParser;
use crate::video_parser::request::{PageRequest, ParserContext, request_page};
use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

pub fn decode_page_text(input: &str) -> String {
    static U_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\\u00(2f|3a|26|3d)").unwrap());
    static X_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\\x(2f|3a|26|3d)").unwrap());
    static ENT_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)&(?:amp|quot|apos|lt|gt);|&#(?:x[\da-f]+|\d+);").unwrap()
    });

    let s1 = U_RX.replace_all(input, |caps: &regex::Captures| {
        match caps[1].to_ascii_lowercase().as_str() {
            "2f" => "/",
            "3a" => ":",
            "26" => "&",
            "3d" => "=",
            _ => "",
        }
    });
    let s2 = X_RX.replace_all(&s1, |caps: &regex::Captures| {
        match caps[1].to_ascii_lowercase().as_str() {
            "2f" => "/",
            "3a" => ":",
            "26" => "&",
            "3d" => "=",
            _ => "",
        }
    });
    let s3 = s2.replace("\\/", "/");
    ENT_RX
        .replace_all(&s3, |caps: &regex::Captures| {
            let ent = &caps[0];
            let lower = ent.to_ascii_lowercase();
            match lower.as_str() {
                "&amp;" => return "&".to_string(),
                "&quot;" => return "\"".to_string(),
                "&apos;" => return "'".to_string(),
                "&lt;" => return "<".to_string(),
                "&gt;" => return ">".to_string(),
                _ => {}
            }
            let inner = &ent[2..ent.len() - 1];
            let code = if inner.starts_with('x') || inner.starts_with('X') {
                u32::from_str_radix(&inner[1..], 16).ok()
            } else {
                inner.parse::<u32>().ok()
            };
            if let Some(c) = code.and_then(|n| {
                if n > 0 && n <= 0x10ffff {
                    char::from_u32(n)
                } else {
                    None
                }
            }) {
                c.to_string()
            } else {
                ent.to_string()
            }
        })
        .into_owned()
}

pub fn media_type_from_url(url_str: &str) -> Option<DownloadTypeKind> {
    static EXT_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\.(m3u8|mp4|mkv|mov|m4v|webm|mp3|m4a|aac|vtt|srt|ass|ssa)$").unwrap()
    });
    let parsed = Url::parse(url_str).ok()?;
    let caps = EXT_RX.captures(parsed.path())?;
    if caps[1].eq_ignore_ascii_case("m3u8") {
        Some(DownloadTypeKind::M3u8)
    } else {
        Some(DownloadTypeKind::File)
    }
}

fn resolve_link(value: &str, base: &str) -> String {
    let mut decoded = decode_page_text(value).trim().to_string();
    let lower = decoded.to_ascii_lowercase();
    if lower.starts_with("http%3a") || lower.starts_with("https%3a") {
        decoded = percent_decode_lossy(&decoded);
    }
    let Ok(base_url) = Url::parse(base) else {
        return String::new();
    };
    let Ok(mut joined) = base_url.join(&decoded) else {
        return String::new();
    };
    joined.set_fragment(None);
    normalize_download_url(joined.as_str()).unwrap_or_default()
}

fn extract_title(html: &str) -> String {
    static TITLE_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>([\s\S]*?)</title>").unwrap());
    static TAG_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    let raw = TITLE_RX
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or("");
    let decoded = decode_page_text(raw);
    let stripped = TAG_RX.replace_all(&decoded, "");
    stripped.trim().chars().take(150).collect()
}

#[derive(Clone, Default)]
pub struct ExtractMediaOptions {
    pub url: String,
    pub urls: Option<Vec<String>>,
    pub headers: HeaderMap,
    pub deep: Option<usize>,
    pub max_pages: Option<usize>,
    pub max_urls: Option<usize>,
    pub max_html_size: Option<usize>,
    pub concurrency: Option<usize>,
    pub path_filter: Option<String>,
    pub sub_url_regex: Option<String>,
    pub client: Option<reqwest::Client>,
}

#[derive(Clone)]
struct CrawlEntry {
    url: String,
    depth: usize,
    root: String,
    title: String,
}

pub async fn extract_media(
    opts: ExtractMediaOptions,
    cancel: &CancellationToken,
) -> CoreResult<ExtractionResult> {
    let seeds = match opts.urls {
        Some(ref list) => list.clone(),
        None if !opts.url.trim().is_empty() => vec![opts.url.clone()],
        None => Vec::new(),
    };
    if seeds.is_empty() || seeds.len() > 20 {
        return Err(CoreError::msg("每次可提取 1–20 个网页或分享链接"));
    }

    let depth = opts.deep.unwrap_or(1).clamp(0, 3);
    let max_pages = opts.max_pages.unwrap_or(30).clamp(1, 60);
    let max_urls = opts.max_urls.unwrap_or(100).clamp(1, 500);
    let parallel = opts.concurrency.unwrap_or(3).clamp(1, 4);
    let max_bytes = opts
        .max_html_size
        .unwrap_or(5 * 1024 * 1024)
        .min(5 * 1024 * 1024);
    let page_budget = Duration::from_secs(60);
    let client = opts.client.clone().unwrap_or_else(global_http_client);

    let mut visited: HashSet<String> = HashSet::new();
    let mut found_order: Vec<String> = Vec::new();
    let mut found_map: HashMap<String, ExtractedMedia> = HashMap::new();
    let mut errors: Vec<ExtractionErrorItem> = Vec::new();
    let mut pages_visited = 0usize;
    let mut truncated = false;
    let mut queue: VecDeque<CrawlEntry> = VecDeque::new();

    for seed in seeds {
        let u = source_url(&seed)?;
        if !u.is_empty() && !visited.contains(&u) {
            if visited.len() >= max_pages {
                truncated = true;
            } else {
                visited.insert(u.clone());
                queue.push_back(CrawlEntry {
                    url: u.clone(),
                    depth: 0,
                    root: u,
                    title: String::new(),
                });
            }
        }
    }

    static BASE_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)<base\b[^>]*\bhref\s*=\s*["']([^"']+)["']"#).unwrap());
    static BARE_URL_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s<>"'`\\]+"#).unwrap());
    static QUOTED_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"["']([^"'<>\r\n]{1,4096})["']"#).unwrap());
    static TAG_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<(video|source|track|iframe|a)\b([^>]*)>").unwrap());
    static ATTR_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"([\w-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap()
    });
    static PLAYLIST_CT_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:application|audio)/(?:vnd\.apple\.mpegurl|x-mpegurl)").unwrap()
    });
    static HLS_ATTR_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)mpegurl|hls").unwrap());

    let path_filter = opts.path_filter.as_deref().unwrap_or("");
    let sub_url_filter = opts.sub_url_regex.as_deref().unwrap_or("").replace('*', "");

    let add_item = |url: String,
                    source: String,
                    title: String,
                    media_type: Option<DownloadTypeKind>,
                    platform: Option<String>,
                    cred_source: String,
                    found_order: &mut Vec<String>,
                    found_map: &mut HashMap<String, ExtractedMedia>,
                    truncated: &mut bool| {
        if url.is_empty() || found_map.contains_key(&url) {
            return;
        }
        if found_map.len() >= max_urls {
            *truncated = true;
            return;
        }
        let mtype = media_type
            .or_else(|| media_type_from_url(&url))
            .unwrap_or(DownloadTypeKind::File);
        found_order.push(url.clone());
        found_map.insert(
            url.clone(),
            ExtractedMedia {
                url,
                title,
                source,
                credential_source: Some(cred_source),
                media_type: mtype,
                platform,
            },
        );
    };

    while !queue.is_empty() && found_map.len() < max_urls {
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let batch_len = parallel.min(queue.len());
        let mut batch = Vec::with_capacity(batch_len);
        for _ in 0..batch_len {
            if let Some(item) = queue.pop_front() {
                batch.push(item);
            }
        }
        pages_visited += batch.len();

        let mut handles = Vec::with_capacity(batch.len());
        for entry in batch {
            let client = client.clone();
            let base_headers = opts.headers.clone();
            let parent_cancel = cancel.clone();
            handles.push(tokio::spawn(async move {
                let entry_url = entry.url.clone();
                let page_cancel = parent_cancel.child_token();
                let timeout_cancel = page_cancel.clone();
                let timer = tokio::spawn(async move {
                    tokio::time::sleep(page_budget).await;
                    timeout_cancel.cancel();
                });
                let res: CoreResult<CrawlOutcome> = async {
                    let outgoing = headers_for_target(&entry.root, &entry.url, &base_headers)?;
                    let plat = crate::format_options::detect_platform(&entry.url).platform;
                    if plat != crate::format_options::VideoPlatformKind::Unknown {
                        let ctx = ParserContext {
                            client: Arc::new(client),
                            cancel: page_cancel.clone(),
                            request_override: None,
                            resolve_override: None,
                        };
                        let parsed =
                            VideoParser::parse_with_context(&entry.url, &outgoing, &ctx).await?;
                        if parsed.code != 0 || parsed.data.as_ref().is_none_or(|d| d.url.is_empty())
                        {
                            return Err(CoreError::msg(
                                parsed
                                    .message
                                    .unwrap_or_else(|| "平台未返回媒体链接".to_string()),
                            ));
                        }
                        let info = parsed.data.unwrap();
                        Ok(CrawlOutcome::Single {
                            url: info.url,
                            source: entry.url.clone(),
                            title: if info.title.is_empty() {
                                entry.title.clone()
                            } else {
                                info.title
                            },
                            media_type: None,
                            platform: info.platform,
                            cred_source: entry.root.clone(),
                        })
                    } else if let Some(direct) = media_type_from_url(&entry.url) {
                        Ok(CrawlOutcome::Single {
                            url: entry.url.clone(),
                            source: entry.root.clone(),
                            title: entry.title.clone(),
                            media_type: Some(direct),
                            platform: None,
                            cred_source: entry.root.clone(),
                        })
                    } else {
                        let page = request_page(
                            &client,
                            &entry.url,
                            &PageRequest {
                                headers: outgoing,
                                max_bytes,
                                method_post_body: None,
                            },
                            &page_cancel,
                        )
                        .await;
                        match page {
                            Ok(p) => Ok(CrawlOutcome::Page { entry, page: p }),
                            Err(e) => {
                                if page_cancel.is_cancelled() && !parent_cancel.is_cancelled() {
                                    Err(CoreError::msg("单个页面请求超时"))
                                } else {
                                    Err(e)
                                }
                            }
                        }
                    }
                }
                .await;
                timer.abort();
                (entry_url, res)
            }));
        }

        for h in handles {
            let (entry_url, outcome) = h.await.map_err(|e| CoreError::msg(e.to_string()))?;
            if cancel.is_cancelled() {
                return Err(CoreError::aborted());
            }
            match outcome {
                Err(err) => {
                    errors.push(ExtractionErrorItem {
                        url: entry_url,
                        message: err.to_string(),
                    });
                }
                Ok(CrawlOutcome::Single {
                    url,
                    source,
                    title,
                    media_type,
                    platform,
                    cred_source,
                }) => {
                    add_item(
                        url,
                        source,
                        title,
                        media_type,
                        platform,
                        cred_source,
                        &mut found_order,
                        &mut found_map,
                        &mut truncated,
                    );
                }
                Ok(CrawlOutcome::Page { entry, page }) => {
                    if PLAYLIST_CT_RX.is_match(&page.content_type)
                        || page.text.trim_start().starts_with("#EXTM3U")
                    {
                        add_item(
                            page.url,
                            entry.root.clone(),
                            entry.title,
                            Some(DownloadTypeKind::M3u8),
                            None,
                            entry.root,
                            &mut found_order,
                            &mut found_map,
                            &mut truncated,
                        );
                        continue;
                    }

                    let html = decode_page_text(&page.text);
                    let title = {
                        let t = extract_title(&html);
                        if t.is_empty() { entry.title.clone() } else { t }
                    };
                    let base = BASE_RX
                        .captures(&html)
                        .and_then(|c| c.get(1))
                        .map(|m| resolve_link(m.as_str(), &page.url))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| page.url.clone());

                    for m in BARE_URL_RX.find_iter(&html) {
                        let u = resolve_link(m.as_str(), &base);
                        if !u.is_empty() && media_type_from_url(&u).is_some() {
                            add_item(
                                u,
                                page.url.clone(),
                                title.clone(),
                                None,
                                None,
                                entry.root.clone(),
                                &mut found_order,
                                &mut found_map,
                                &mut truncated,
                            );
                        }
                    }

                    for caps in QUOTED_RX.captures_iter(&html) {
                        let u = resolve_link(&caps[1], &base);
                        if !u.is_empty() && media_type_from_url(&u).is_some() {
                            add_item(
                                u,
                                page.url.clone(),
                                title.clone(),
                                None,
                                None,
                                entry.root.clone(),
                                &mut found_order,
                                &mut found_map,
                                &mut truncated,
                            );
                        }
                    }

                    let page_origin = Url::parse(&page.url).ok().map(|u| u.origin());
                    for caps in TAG_RX.captures_iter(&html) {
                        let tag = caps[1].to_ascii_lowercase();
                        let attr_text = &caps[2];
                        let mut attrs: HashMap<String, String> = HashMap::new();
                        for ac in ATTR_RX.captures_iter(attr_text) {
                            let k = ac[1].to_ascii_lowercase();
                            let v = ac
                                .get(2)
                                .or_else(|| ac.get(3))
                                .or_else(|| ac.get(4))
                                .map(|m| m.as_str().to_string())
                                .unwrap_or_default();
                            attrs.insert(k, v);
                        }
                        let raw_ref = attrs
                            .get("src")
                            .or_else(|| attrs.get("data-src"))
                            .or_else(|| attrs.get("href"))
                            .map(String::as_str)
                            .unwrap_or("");
                        if raw_ref.is_empty() {
                            continue;
                        }
                        let u = resolve_link(raw_ref, &base);
                        if u.is_empty() {
                            continue;
                        }
                        if media_type_from_url(&u).is_some() {
                            add_item(
                                u,
                                page.url.clone(),
                                title.clone(),
                                None,
                                None,
                                entry.root.clone(),
                                &mut found_order,
                                &mut found_map,
                                &mut truncated,
                            );
                            continue;
                        }
                        if matches!(tag.as_str(), "video" | "source" | "track") {
                            let mtype =
                                if attrs.get("type").is_some_and(|t| HLS_ATTR_RX.is_match(t)) {
                                    DownloadTypeKind::M3u8
                                } else {
                                    DownloadTypeKind::File
                                };
                            add_item(
                                u,
                                page.url.clone(),
                                title.clone(),
                                Some(mtype),
                                None,
                                entry.root.clone(),
                                &mut found_order,
                                &mut found_map,
                                &mut truncated,
                            );
                            continue;
                        }
                        if entry.depth >= depth {
                            continue;
                        }
                        if tag == "a" {
                            let link_origin = Url::parse(&u).ok().map(|lu| lu.origin());
                            if link_origin != page_origin {
                                continue;
                            }
                            if !path_filter.is_empty() && !u.contains(path_filter) {
                                continue;
                            }
                            if !sub_url_filter.is_empty() && !u.contains(&sub_url_filter) {
                                continue;
                            }
                        }
                        if !visited.contains(&u) {
                            if visited.len() >= max_pages {
                                truncated = true;
                            } else {
                                visited.insert(u.clone());
                                queue.push_back(CrawlEntry {
                                    url: u,
                                    depth: entry.depth + 1,
                                    root: entry.root.clone(),
                                    title: title.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    if !queue.is_empty() {
        truncated = true;
    }

    let items = found_order
        .into_iter()
        .filter_map(|k| found_map.remove(&k))
        .collect();
    Ok(ExtractionResult {
        items,
        errors,
        pages_visited,
        truncated,
    })
}

enum CrawlOutcome {
    Single {
        url: String,
        source: String,
        title: String,
        media_type: Option<DownloadTypeKind>,
        platform: Option<String>,
        cred_source: String,
    },
    Page {
        entry: CrawlEntry,
        page: crate::video_parser::request::PageData,
    },
}
