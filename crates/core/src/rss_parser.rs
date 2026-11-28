use crate::contract::{HeaderMap, RssFeedItem, RssSource, RssSourceMode};
use crate::error::{CoreError, CoreResult};
use crate::extractor::{ExtractMediaOptions, extract_media};
use crate::http::{global_http_client, sha256_hex};
use chrono::DateTime;
use regex::Regex;
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const MAX_FEED_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_ITEMS_PER_SOURCE: usize = 50_000;
const RSS_DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const FETCH_TIMEOUT_SECS: u64 = 25;
const MAX_REDIRECTS: usize = 5;

static MEDIA_EXT_RX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(?:m3u8|mp4|mkv|mov|m4v|avi|flv|webm|wmv|ts|m4s|mp3|wav|ogg|m4a|aac|flac|opus|weba|vtt|srt|ass|ssa)(?:\?|$|#)").unwrap()
});
static STATIC_ASSET_RX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(?:css|js|mjs|cjs|png|jpe?g|gif|webp|svg|ico|woff2?|ttf|eot|map|json|xml)$")
        .unwrap()
});

#[derive(Debug, Clone)]
pub struct ParsedFeedItem {
    pub guid: String,
    pub title: String,
    pub link: String,
    pub enclosure_url: String,
    pub enclosure_length: u64,
    pub pub_date: i64,
    pub pub_date_text: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedFeedResult {
    pub feed_title: String,
    pub items: Vec<ParsedFeedItem>,
    pub resolved_url: String,
}

#[derive(Debug, Clone)]
pub struct FetchFeedOptions {
    pub url: String,
    pub mode: RssSourceMode,
    pub item_pattern: String,
    pub referer: String,
    pub cookies: String,
    pub user_agent: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedRssMedia {
    pub media_url: String,
    pub page_url: String,
    pub title: String,
    pub referer: String,
    pub headers: HeaderMap,
}

pub fn decode_xml_entities(text: &str) -> String {
    static RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"&(#x?[0-9a-fA-F]+|amp|lt|gt|quot|apos|#39|nbsp);").unwrap());
    RX.replace_all(text, |caps: &regex::Captures| {
        let ent = &caps[1];
        match ent {
            "amp" => "&".to_string(),
            "lt" => "<".to_string(),
            "gt" => ">".to_string(),
            "quot" => "\"".to_string(),
            "apos" | "#39" => "'".to_string(),
            "nbsp" => " ".to_string(),
            _ => {
                let code =
                    if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                        u32::from_str_radix(hex, 16).ok()
                    } else if let Some(dec) = ent.strip_prefix('#') {
                        dec.parse::<u32>().ok()
                    } else {
                        None
                    };
                if let Some(c) = code.and_then(|n| if n > 0 { char::from_u32(n) } else { None }) {
                    c.to_string()
                } else {
                    caps[0].to_string()
                }
            }
        }
    })
    .into_owned()
}

pub fn strip_cdata(raw: &str) -> String {
    static RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<!\[CDATA\[([\s\S]*?)\]\]>").unwrap());
    RX.replace_all(raw, "$1").trim().to_string()
}

fn extract_xml_tag(block: &str, tag: &str) -> String {
    let pattern = format!(r"(?is)<{tag}\b[^>]*>([\s\S]*?)</{tag}>");
    let Ok(rx) = Regex::new(&pattern) else {
        return String::new();
    };
    let Some(caps) = rx.captures(block) else {
        return String::new();
    };
    let inner = &caps[1];
    if inner.contains("<![CDATA[") {
        decode_xml_entities(&strip_cdata(inner)).trim().to_string()
    } else {
        decode_xml_entities(inner).trim().to_string()
    }
}

fn parse_attr(attrs: &str, name: &str) -> String {
    let pattern = format!(r#"(?i)\b{name}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))"#);
    let Ok(rx) = Regex::new(&pattern) else {
        return String::new();
    };
    let Some(caps) = rx.captures(attrs) else {
        return String::new();
    };
    let raw = caps
        .get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map(|m| m.as_str().trim())
        .unwrap_or("");
    decode_xml_entities(raw)
}

fn resolve_http_url(raw: &str, base: &str) -> String {
    let clean = decode_xml_entities(raw.trim())
        .trim_end_matches('\\')
        .to_string();
    if clean.is_empty() {
        return String::new();
    }
    let Ok(base_url) = Url::parse(base) else {
        return String::new();
    };
    let Ok(u) = base_url.join(&clean) else {
        return String::new();
    };
    if matches!(u.scheme(), "http" | "https") {
        u.to_string()
    } else {
        String::new()
    }
}

pub fn parse_pub_date(raw: &str) -> i64 {
    let text = raw.trim();
    if text.is_empty() {
        return 0;
    }
    if let Ok(dt) = DateTime::parse_from_rfc2822(text) {
        return dt.timestamp_millis().max(0);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return dt.timestamp_millis().max(0);
    }
    // Common fallback formats: YYYY-MM-DD HH:MM:SS or YYYY-MM-DD
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S") {
        return ndt.and_utc().timestamp_millis().max(0);
    }
    if let Ok(nd) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        if let Some(ndt) = nd.and_hms_opt(0, 0, 0) {
            return ndt.and_utc().timestamp_millis().max(0);
        }
    }
    0
}

fn extract_tags_from_block(block: &str, description: &str) -> Vec<String> {
    static TAG_STRIP_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    static CAT_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<category\b([^>]*)>([\s\S]*?)</category>").unwrap());
    static CAT_SELF_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<category\b([^>]*)/>").unwrap());
    static SPLIT_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[/,，、|]+").unwrap());

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push_tag = |raw: &str| {
        let stripped = TAG_STRIP_RX.replace_all(raw, " ");
        let decoded = decode_xml_entities(&stripped);
        let clean = WS_RX.replace_all(&decoded, " ").trim().to_string();
        let lower = clean.to_ascii_lowercase();
        if clean.is_empty()
            || clean.chars().count() > 40
            || lower.starts_with("http://")
            || lower.starts_with("https://")
        {
            return;
        }
        let key = clean.to_lowercase();
        if seen.insert(key) {
            out.push(clean);
        }
    };

    for caps in CAT_RX.captures_iter(block) {
        let inner = if caps[2].contains("<![CDATA[") {
            strip_cdata(&caps[2])
        } else {
            caps[2].to_string()
        };
        let term = parse_attr(&caps[1], "term");
        if !inner.trim().is_empty() {
            push_tag(&inner);
        } else if !term.is_empty() {
            push_tag(&term);
        }
    }
    for caps in CAT_SELF_RX.captures_iter(block) {
        let term = parse_attr(&caps[1], "term");
        if !term.is_empty() {
            push_tag(&term);
        }
    }

    let plain_desc = WS_RX
        .replace_all(
            &decode_xml_entities(&TAG_STRIP_RX.replace_all(description, " ")),
            " ",
        )
        .trim()
        .to_string();
    let desc_lower = plain_desc.to_ascii_lowercase();
    if !plain_desc.is_empty()
        && plain_desc.chars().count() <= 240
        && !desc_lower.starts_with("source: http:")
        && !desc_lower.starts_with("source: https:")
        && !desc_lower.starts_with("source:http:")
        && !desc_lower.starts_with("source:https:")
    {
        for part in SPLIT_RX.split(&plain_desc) {
            push_tag(part);
        }
    }

    out.truncate(15);
    out
}

fn matches_item_pattern(url: &str, item_pattern: &str) -> bool {
    let p = item_pattern.trim().to_lowercase();
    p.is_empty() || url.to_lowercase().contains(&p)
}

fn is_media_enclosure(url: &str, mime_type: &str) -> bool {
    let lower = mime_type.to_ascii_lowercase();
    if lower.starts_with("image/") {
        return false;
    }
    if lower.contains("mpegurl")
        || lower.starts_with("video/")
        || lower.starts_with("audio/")
        || lower == "application/octet-stream"
    {
        return true;
    }
    MEDIA_EXT_RX.is_match(url)
}

pub fn make_safe_feed_slug(title: &str, fallback_id: &str) -> String {
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    static EXT_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\.(?:m3u8?|mp4|mkv|mov|ts|webm|flv|avi)$").unwrap());
    let mut cleaned = String::with_capacity(title.len());
    for ch in title.chars() {
        if (ch as u32) < 0x20 || "<>:\"/\\|?*#%".contains(ch) {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
    }
    let collapsed = WS_RX.replace_all(&cleaned, " ").trim().to_string();
    let sliced: String = collapsed.chars().take(80).collect();
    let stripped = EXT_RX.replace(&sliced, "");
    let final_slug = stripped.trim_end_matches(['.', ' ']).to_string();
    if !final_slug.is_empty() {
        return final_slug;
    }
    let id_input = if fallback_id.is_empty() {
        "item"
    } else {
        fallback_id
    };
    let hash = sha256_hex(id_input);
    format!("video-{}", &hash[..12])
}

pub fn parse_rss_or_atom_feed(xml: &str, feed_url: &str, item_pattern: &str) -> ParsedFeedResult {
    static ITEM_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<item\b").unwrap());
    static ENTRY_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<entry\b").unwrap());
    static ITEM_BLOCK_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<item\b[^>]*>([\s\S]*?)</item>").unwrap());
    static ENTRY_BLOCK_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<entry\b[^>]*>([\s\S]*?)</entry>").unwrap());
    static LINK_TAG_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)<link\b([^>]*)").unwrap());
    static ENC_TAG_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)<(?:enclosure|media:content)\b([^>]*)").unwrap());

    let first_item = ITEM_RX.find(xml).map(|m| m.start());
    let is_atom = first_item.is_none() && ENTRY_RX.is_match(xml);
    let first_idx = if is_atom {
        ENTRY_RX.find(xml).map(|m| m.start())
    } else {
        first_item
    };
    let header = match first_idx {
        Some(idx) => &xml[..idx],
        None => xml,
    };

    let host_fallback = Url::parse(feed_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "RSS 订阅".to_string());
    let feed_title = {
        let t = extract_xml_tag(header, "title");
        if t.is_empty() { host_fallback } else { t }
    };

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let block_rx = if is_atom {
        &*ENTRY_BLOCK_RX
    } else {
        &*ITEM_BLOCK_RX
    };

    for caps in block_rx.captures_iter(xml) {
        let block = &caps[1];
        let title = extract_xml_tag(block, "title");

        let mut link = String::new();
        if is_atom {
            for l_caps in LINK_TAG_RX.captures_iter(block) {
                let rel = parse_attr(&l_caps[1], "rel").to_ascii_lowercase();
                let href = resolve_http_url(&parse_attr(&l_caps[1], "href"), feed_url);
                if href.is_empty() {
                    continue;
                }
                if rel.is_empty() || rel == "alternate" {
                    link = href;
                    break;
                }
            }
        } else {
            link = resolve_http_url(&extract_xml_tag(block, "link"), feed_url);
        }

        let mut enclosure_url = String::new();
        let mut enclosure_length = 0u64;
        if is_atom {
            for l_caps in LINK_TAG_RX.captures_iter(block) {
                if !parse_attr(&l_caps[1], "rel").eq_ignore_ascii_case("enclosure") {
                    continue;
                }
                let raw_u = parse_attr(&l_caps[1], "href");
                let mime = parse_attr(&l_caps[1], "type");
                let resolved = resolve_http_url(&raw_u, feed_url);
                if resolved.is_empty() || !is_media_enclosure(&resolved, &mime) {
                    continue;
                }
                enclosure_url = resolved;
                let len_s = parse_attr(&l_caps[1], "length");
                if let Ok(len) = len_s.parse::<u64>() {
                    enclosure_length = len;
                }
                break;
            }
        } else {
            for e_caps in ENC_TAG_RX.captures_iter(block) {
                let raw_u = parse_attr(&e_caps[1], "url");
                let mime = parse_attr(&e_caps[1], "type");
                let resolved = resolve_http_url(&raw_u, feed_url);
                if resolved.is_empty() || !is_media_enclosure(&resolved, &mime) {
                    continue;
                }
                enclosure_url = resolved;
                let len_s = {
                    let l = parse_attr(&e_caps[1], "length");
                    if l.is_empty() {
                        parse_attr(&e_caps[1], "fileSize")
                    } else {
                        l
                    }
                };
                if let Ok(len) = len_s.parse::<u64>() {
                    enclosure_length = len;
                }
                break;
            }
        }

        let page_url = if !link.is_empty() {
            link.clone()
        } else {
            enclosure_url.clone()
        };
        let raw_guid = extract_xml_tag(block, if is_atom { "id" } else { "guid" });
        let guid = if !raw_guid.is_empty() {
            raw_guid
        } else if !link.is_empty() {
            link.clone()
        } else if !enclosure_url.is_empty() {
            enclosure_url.clone()
        } else {
            title.clone()
        };
        let pub_date_text = {
            let p = extract_xml_tag(block, if is_atom { "published" } else { "pubDate" });
            if p.is_empty() && is_atom {
                extract_xml_tag(block, "updated")
            } else {
                p
            }
        };
        let description = {
            let d = extract_xml_tag(block, if is_atom { "summary" } else { "description" });
            if d.is_empty() && is_atom {
                extract_xml_tag(block, "content")
            } else {
                d
            }
        };

        if guid.is_empty() || page_url.is_empty() || !matches_item_pattern(&page_url, item_pattern)
        {
            continue;
        }
        if !seen.insert(guid.clone()) {
            continue;
        }

        items.push(ParsedFeedItem {
            guid: guid.clone(),
            title: if title.is_empty() { guid } else { title },
            link: page_url,
            enclosure_url,
            enclosure_length,
            pub_date: parse_pub_date(&pub_date_text),
            pub_date_text,
            tags: extract_tags_from_block(block, &description),
        });

        if items.len() >= MAX_ITEMS_PER_SOURCE {
            break;
        }
    }

    ParsedFeedResult {
        feed_title,
        items,
        resolved_url: feed_url.to_string(),
    }
}

pub fn parse_html_feed(
    html: &str,
    list_url: &str,
    item_pattern: &str,
    feed_name: &str,
) -> CoreResult<ParsedFeedResult> {
    static TAG_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    static A_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<a\b([^>]*)>([\s\S]*?)</a>").unwrap());

    let mut list_parsed = Url::parse(list_url)?;
    list_parsed.set_fragment(None);
    let page_title = WS_RX
        .replace_all(
            &TAG_RX.replace_all(&extract_xml_tag(html, "title"), ""),
            " ",
        )
        .trim()
        .to_string();
    let feed_title = if !feed_name.is_empty() {
        feed_name.to_string()
    } else if !page_title.is_empty() {
        page_title
    } else {
        list_parsed.host_str().unwrap_or("RSS 订阅").to_string()
    };

    let mut items = Vec::new();
    let mut seen_urls = HashSet::new();
    let pattern = item_pattern.trim();

    for caps in A_RX.captures_iter(html) {
        let attrs = &caps[1];
        let inner = &caps[2];
        let raw_href = parse_attr(attrs, "href");
        let raw_lower = raw_href.to_ascii_lowercase();
        if raw_href.is_empty()
            || raw_href.starts_with('#')
            || raw_lower.starts_with("javascript:")
            || raw_lower.starts_with("mailto:")
            || raw_lower.starts_with("tel:")
            || raw_lower.starts_with("data:")
        {
            continue;
        }
        let resolved_str = resolve_http_url(&raw_href, list_url);
        if resolved_str.is_empty() {
            continue;
        }
        let Ok(mut resolved) = Url::parse(&resolved_str) else {
            continue;
        };
        resolved.set_fragment(None);
        if resolved.as_str() == list_parsed.as_str() || STATIC_ASSET_RX.is_match(resolved.path()) {
            continue;
        }
        if !pattern.is_empty() {
            if !matches_item_pattern(resolved.as_str(), pattern) {
                continue;
            }
        } else if resolved.origin() != list_parsed.origin() || resolved.path().len() <= 1 {
            continue;
        }

        let href = resolved.to_string();
        if !seen_urls.insert(href.clone()) {
            continue;
        }

        let inner_text = WS_RX
            .replace_all(&decode_xml_entities(&TAG_STRIP_STR(&TAG_RX, inner)), " ")
            .trim()
            .to_string();
        let attr_title = {
            let t = parse_attr(attrs, "title");
            if !t.is_empty() {
                t
            } else {
                let al = parse_attr(attrs, "aria-label");
                if !al.is_empty() {
                    al
                } else {
                    parse_attr(inner, "alt")
                }
            }
        };
        let path_tail = resolved
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .next_back()
            .map(str::to_string)
            .unwrap_or_else(|| resolved.host_str().unwrap_or("").to_string());

        let raw_title = if !inner_text.is_empty() {
            inner_text
        } else if !attr_title.is_empty() {
            attr_title
        } else {
            path_tail
        };
        let title: String = raw_title.chars().take(160).collect();
        let direct_media = if MEDIA_EXT_RX.is_match(&href) {
            href.clone()
        } else {
            String::new()
        };

        items.push(ParsedFeedItem {
            guid: href.clone(),
            title: if title.is_empty() {
                href.clone()
            } else {
                title
            },
            link: href,
            enclosure_url: direct_media,
            enclosure_length: 0,
            pub_date: 0,
            pub_date_text: String::new(),
            tags: Vec::new(),
        });

        if items.len() >= MAX_ITEMS_PER_SOURCE {
            break;
        }
    }

    Ok(ParsedFeedResult {
        feed_title,
        items,
        resolved_url: list_url.to_string(),
    })
}

#[allow(non_snake_case)]
fn TAG_STRIP_STR<'a>(rx: &Regex, input: &'a str) -> String {
    rx.replace_all(input, " ").into_owned()
}

async fn fetch_text_safe(
    target_url: &str,
    credential_origin: Option<&url::Origin>,
    cookies: &str,
    referer: &str,
    user_agent: &str,
    accept: &str,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> CoreResult<(String, String)> {
    let mut current = Url::parse(target_url)?;
    if !matches!(current.scheme(), "http" | "https") {
        return Err(CoreError::msg("地址仅支持 http:// 或 https://"));
    }

    let client = global_http_client();
    let timeout_fut = tokio::time::sleep(Duration::from_secs(FETCH_TIMEOUT_SECS));
    tokio::pin!(timeout_fut);

    let ua = if user_agent.trim().is_empty() {
        RSS_DEFAULT_UA
    } else {
        user_agent.trim()
    };

    for hop in 0..=MAX_REDIRECTS {
        let mut req = client
            .get(current.as_str())
            .header("user-agent", ua)
            .header("accept", accept);
        if !referer.trim().is_empty() {
            req = req.header("referer", referer.trim());
        }
        if !cookies.trim().is_empty() {
            if let Some(orig) = credential_origin {
                if &current.origin() == orig {
                    req = req.header("cookie", cookies.trim());
                }
            }
        }

        let mut resp = tokio::select! {
            _ = cancel.cancelled() => return Err(CoreError::aborted()),
            _ = &mut timeout_fut => return Err(CoreError::msg("请求超时")),
            r = req.send() => r?,
        };

        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            if loc.is_none() || hop == MAX_REDIRECTS {
                return Err(CoreError::msg(format!("请求重定向失败：HTTP {status}")));
            }
            let next = current
                .join(&loc.unwrap())
                .map_err(|_| CoreError::msg("重定向目标协议无效"))?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(CoreError::msg("重定向目标协议无效"));
            }
            current = next;
            continue;
        }

        if !(200..300).contains(&status) {
            let reason = resp.status().canonical_reason().unwrap_or("");
            return Err(CoreError::msg(format!("HTTP {status} {reason}")));
        }

        if let Some(cl) = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
        {
            if cl > max_bytes {
                return Err(CoreError::msg(format!(
                    "订阅响应体积过大 ({cl} 字节，上限 {max_bytes} 字节)"
                )));
            }
        }

        let mut buf = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::aborted()),
                _ = &mut timeout_fut => return Err(CoreError::msg("请求超时")),
                c = resp.chunk() => c?,
            };
            let Some(bytes) = chunk else {
                break;
            };
            if buf.len() + bytes.len() > max_bytes {
                return Err(CoreError::msg(format!(
                    "订阅响应体积超出上限 ({max_bytes} 字节)"
                )));
            }
            buf.extend_from_slice(&bytes);
        }
        return Ok((
            current.to_string(),
            String::from_utf8_lossy(&buf).into_owned(),
        ));
    }
    Err(CoreError::msg("请求重定向次数过多"))
}

pub async fn fetch_and_parse_feed(
    opts: &FetchFeedOptions,
    cancel: &CancellationToken,
) -> CoreResult<ParsedFeedResult> {
    let source_parsed = Url::parse(&opts.url)?;
    if !matches!(source_parsed.scheme(), "http" | "https") {
        return Err(CoreError::msg("订阅地址应以 http:// 或 https:// 开头"));
    }
    let referer = if opts.referer.trim().is_empty() {
        format!("{}/", source_parsed.origin().ascii_serialization())
    } else {
        opts.referer.trim().to_string()
    };
    let accept = if opts.mode == RssSourceMode::Html {
        "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8"
    } else {
        "application/rss+xml,application/atom+xml,application/xml,text/xml;q=0.9,*/*;q=0.8"
    };
    let origin = source_parsed.origin();
    let (resolved_url, text) = fetch_text_safe(
        source_parsed.as_str(),
        Some(&origin),
        &opts.cookies,
        &referer,
        &opts.user_agent,
        accept,
        MAX_FEED_BYTES,
        cancel,
    )
    .await?;

    match opts.mode {
        RssSourceMode::Html => parse_html_feed(&text, &resolved_url, &opts.item_pattern, ""),
        RssSourceMode::Rss => Ok(parse_rss_or_atom_feed(
            &text,
            &resolved_url,
            &opts.item_pattern,
        )),
    }
}

pub fn extract_media_from_detail_html(html: &str, page_url: &str) -> String {
    static LD_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)<script\b[^>]*type=["']application/ld\+json["'][^>]*>([\s\S]*?)</script>"#,
        )
        .unwrap()
    });
    static ABS_M3U8_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)https?://[^\s"'<>\\]+?\.m3u8(?:\?[^\s"'<>\\]*)?"#).unwrap()
    });
    static REL_M3U8_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(?:src|url|file)\s*[:=]\s*['"]([^'"]+?\.m3u8(?:\?[^'"]*)?)['"]"#).unwrap()
    });
    static VIDEO_ATTR_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)<(?:video|source)\b[^>]*\bsrc\s*=\s*["']([^"']+)["']"#).unwrap()
    });
    static DIRECT_MEDIA_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)https?://[^\s"'<>\\]+?\.(?:mp4|mkv|mov|webm)(?:\?[^\s"'<>\\]*)?"#)
            .unwrap()
    });

    let unescaped = html.replace("\\/", "/");
    let candidates = [html, unescaped.as_str()];

    for caps in LD_RX.captures_iter(html) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&caps[1]) {
            let mut url_candidates = Vec::new();
            if let Some(cu) = val.get("contentUrl").and_then(|v| v.as_str()) {
                url_candidates.push(cu);
            }
            if let Some(arr) = val.get("@graph").and_then(|v| v.as_array()) {
                for node in arr {
                    if let Some(cu) = node.get("contentUrl").and_then(|v| v.as_str()) {
                        url_candidates.push(cu);
                    }
                }
            }
            for cand in url_candidates {
                let resolved = resolve_http_url(cand, page_url);
                if !resolved.is_empty() && MEDIA_EXT_RX.is_match(&resolved) {
                    return resolved;
                }
            }
        }
    }

    for text in candidates {
        if let Some(m) = ABS_M3U8_RX.find(text) {
            let resolved = resolve_http_url(m.as_str(), page_url);
            if !resolved.is_empty() {
                return resolved;
            }
        }
    }

    for text in candidates {
        if let Some(caps) = REL_M3U8_RX.captures(text) {
            let resolved = resolve_http_url(&caps[1], page_url);
            if !resolved.is_empty() {
                return resolved;
            }
        }
    }

    if let Some(caps) = VIDEO_ATTR_RX.captures(html) {
        let resolved = resolve_http_url(&caps[1], page_url);
        if !resolved.is_empty() {
            if let Ok(u) = Url::parse(&resolved) {
                if !STATIC_ASSET_RX.is_match(u.path()) {
                    return resolved;
                }
            }
        }
    }

    for text in candidates {
        if let Some(m) = DIRECT_MEDIA_RX.find(text) {
            let resolved = resolve_http_url(m.as_str(), page_url);
            if !resolved.is_empty() {
                return resolved;
            }
        }
    }

    String::new()
}

pub async fn resolve_rss_item_media(
    item: &RssFeedItem,
    source: &RssSource,
    cancel: &CancellationToken,
) -> CoreResult<ResolvedRssMedia> {
    let source_parsed = Url::parse(&source.url)?;
    let source_origin = source_parsed.origin();
    let page_origin = Url::parse(&item.link)
        .map(|u| u.origin())
        .unwrap_or_else(|_| source_origin.clone());
    let referer = if source.referer.trim().is_empty() {
        format!("{}/", page_origin.ascii_serialization())
    } else {
        source.referer.trim().to_string()
    };
    let user_agent = source.user_agent.trim().to_string();

    let build_media_headers = |target_url: &str| -> HeaderMap {
        let mut h = BTreeMap::new();
        h.insert("referer".to_string(), referer.clone());
        if !user_agent.is_empty() {
            h.insert("user-agent".to_string(), user_agent.clone());
        }
        if !source.cookies.trim().is_empty() {
            if let Ok(tu) = Url::parse(target_url) {
                if tu.origin() == source_origin {
                    h.insert("cookie".to_string(), source.cookies.trim().to_string());
                }
            }
        }
        h
    };

    if !item.enclosure_url.is_empty() && MEDIA_EXT_RX.is_match(&item.enclosure_url) {
        return Ok(ResolvedRssMedia {
            media_url: item.enclosure_url.clone(),
            page_url: item.link.clone(),
            title: item.title.clone(),
            referer: referer.clone(),
            headers: build_media_headers(&item.enclosure_url),
        });
    }

    let (page_url, page_html) = fetch_text_safe(
        &item.link,
        Some(&source_origin),
        &source.cookies,
        &referer,
        &user_agent,
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        4 * 1024 * 1024,
        cancel,
    )
    .await?;

    let mut media_url = extract_media_from_detail_html(&page_html, &page_url);
    if media_url.is_empty() {
        let mut page_headers = BTreeMap::new();
        page_headers.insert("referer".to_string(), referer.clone());
        if !user_agent.is_empty() {
            page_headers.insert("user-agent".to_string(), user_agent.clone());
        }
        if !source.cookies.trim().is_empty() {
            if let Ok(pu) = Url::parse(&page_url) {
                if pu.origin() == source_origin {
                    page_headers.insert("cookie".to_string(), source.cookies.trim().to_string());
                }
            }
        }
        let extracted = extract_media(
            ExtractMediaOptions {
                url: page_url.clone(),
                urls: None,
                headers: page_headers,
                deep: Some(0),
                max_pages: Some(1),
                max_urls: Some(10),
                client: Some(global_http_client()),
                ..Default::default()
            },
            cancel,
        )
        .await?;
        if let Some(first) = extracted.items.into_iter().next() {
            media_url = first.url;
        }
    }

    if media_url.is_empty() {
        return Err(CoreError::msg("详情页未解析到可下载的视频或 M3U8 地址"));
    }

    Ok(ResolvedRssMedia {
        headers: build_media_headers(&media_url),
        media_url,
        page_url,
        title: item.title.clone(),
        referer,
    })
}
