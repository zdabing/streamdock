use crate::error::{CoreError, CoreResult};
use crate::http::decode_b64_utf8;
use crate::rss_parser::{decode_xml_entities, parse_pub_date};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const DEFAULT_SCRAPER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub fn scraper_http_client() -> reqwest::Client {
    crate::http::global_redirect_http_client()
}

#[derive(Debug, Clone, Default)]
pub struct DetailMetadata {
    pub title: String,
    pub plot: String,
    pub actors: Vec<String>,
    pub premiered: String,
    pub year: String,
    pub runtime: u32,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub cover_url: String,
    pub cover_urls: Vec<String>,
    pub studio: String,
    pub unique_id: String,
    pub unique_id_type: String,
    pub page_url: String,
}

#[derive(Debug, Clone, Default)]
pub struct DetailFallback {
    pub title: Option<String>,
    pub pub_date: Option<String>,
    pub tags: Option<Vec<String>>,
    pub referer: Option<String>,
    pub studio: Option<String>,
    pub actors: Option<Vec<String>>,
    pub cover_url: Option<String>,
}

pub fn resolve_http_url(raw: &str, base_url: Option<&str>) -> String {
    let clean = decode_xml_entities(raw.trim())
        .trim_end_matches('\\')
        .to_string();
    if clean.is_empty() {
        return String::new();
    }
    let parsed = match base_url {
        Some(base) => Url::parse(base).ok().and_then(|b| b.join(&clean).ok()),
        None => Url::parse(&clean).ok(),
    };
    let Some(u) = parsed else {
        return String::new();
    };
    if matches!(u.scheme(), "http" | "https") {
        u.to_string()
    } else {
        String::new()
    }
}

pub fn strip_html_tags(raw: &str) -> String {
    static TAG_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    let no_tags = TAG_RX.replace_all(raw, " ");
    let decoded = decode_xml_entities(&no_tags);
    WS_RX.replace_all(&decoded, " ").trim().to_string()
}

pub fn split_tag_list(raw: &str) -> Vec<String> {
    static SPLIT_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[/,，、|]+").unwrap());
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in SPLIT_RX.split(raw) {
        let decoded = decode_xml_entities(part);
        let clean = WS_RX.replace_all(&decoded, " ").trim().to_string();
        if clean.is_empty() || clean.chars().count() > 40 {
            continue;
        }
        if seen.insert(clean.to_lowercase()) {
            out.push(clean);
        }
    }
    out
}

pub fn extract_path_id(page_url: &str) -> String {
    static HTM_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.html?$").unwrap());
    let Ok(u) = Url::parse(page_url) else {
        return String::new();
    };
    let last = u
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or("");
    HTM_RX.replace(last, "").into_owned()
}

pub fn parse_iso_duration_minutes(iso: &str) -> u32 {
    static RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^PT(?:(\d+)H)?(?:(\d+)M)?(?:(\d+(?:\.\d+)?)S)?$").unwrap()
    });
    let Some(caps) = RX.captures(iso.trim()) else {
        return 0;
    };
    let hours: f64 = caps
        .get(1)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0.0);
    let mins: f64 = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0.0);
    let secs: f64 = caps
        .get(3)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0.0);
    let total_secs = hours * 3600.0 + mins * 60.0 + secs;
    if total_secs > 0.0 {
        ((total_secs / 60.0).round() as u32).max(1)
    } else {
        0
    }
}

pub fn parse_clock_duration_minutes(text: &str) -> u32 {
    let parts: Vec<u32> = text
        .trim()
        .split(':')
        .filter_map(|p| p.trim().parse::<u32>().ok())
        .collect();
    if parts.len() == 2 && text.trim().split(':').count() == 2 {
        let secs = parts[0] * 60 + parts[1];
        return ((secs as f64 / 60.0).round() as u32).max(1);
    }
    if parts.len() == 3 && text.trim().split(':').count() == 3 {
        let secs = parts[0] * 3600 + parts[1] * 60 + parts[2];
        return ((secs as f64 / 60.0).round() as u32).max(1);
    }
    0
}

pub fn format_date_ymd(raw: &str) -> String {
    let val = raw.trim();
    if val.is_empty() {
        return String::new();
    }
    static YMD_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{4}-\d{2}-\d{2})").unwrap());
    if let Some(caps) = YMD_RX.captures(val) {
        return caps[1].to_string();
    }
    let ms = parse_pub_date(val);
    if ms <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

pub fn extract_meta_content(html: &str, name: &str) -> String {
    let esc = regex::escape(name);
    let pat1 = format!(
        r#"(?i)<meta\b[^>]*\b(?:property|name)=["']{esc}["'][^>]*\bcontent=["']([^"']*)["']"#
    );
    let pat2 = format!(
        r#"(?i)<meta\b[^>]*\bcontent=["']([^"']*)["'][^>]*\b(?:property|name)=["']{esc}["']"#
    );
    let raw = Regex::new(&pat1)
        .ok()
        .and_then(|rx| rx.captures(html))
        .or_else(|| Regex::new(&pat2).ok().and_then(|rx| rx.captures(html)))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or("");
    decode_xml_entities(raw).trim().to_string()
}

pub fn is_thumbnail_image_url(url_str: &str) -> bool {
    static RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)-\d{2,4}x\d{2,4}\.[a-z0-9]+(?:$|\?)|-responsive-").unwrap()
    });
    RX.is_match(url_str)
}

pub fn extract_html_image_urls(html: &str, base_url: &str) -> Vec<String> {
    static IMG_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<img\b([^>]*)>").unwrap());
    static SKIP_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)/(?:logo|avatar|gravatar|favicon|icon|qrcode|badge|emoji|spinner|loading|placeholder)[^/]*$").unwrap()
    });
    static LOGO_PREFIX_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/logo[-_.]").unwrap());
    static FULL_UPLOAD_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/wp-content/uploads/|/cover\.[a-z0-9]+$").unwrap());
    static ATTR_DATA_SRC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\bdata-src\s*=\s*["']([^"']+)["']"#).unwrap());
    static ATTR_DATA_ORIG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\bdata-original\s*=\s*["']([^"']+)["']"#).unwrap());
    static ATTR_DATA_LAZY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\bdata-lazy-src\s*=\s*["']([^"']+)["']"#).unwrap());
    static ATTR_SRC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\bsrc\s*=\s*["']([^"']+)["']"#).unwrap());

    let mut tier_full = Vec::new();
    let mut tier_other = Vec::new();
    let mut tier_thumb = Vec::new();
    let mut seen = HashSet::new();

    for caps in IMG_RX.captures_iter(html) {
        let attrs = &caps[1];
        let mut chosen = String::new();
        for rx in [
            &*ATTR_DATA_SRC,
            &*ATTR_DATA_ORIG,
            &*ATTR_DATA_LAZY,
            &*ATTR_SRC,
        ] {
            if let Some(m) = rx.captures(attrs).and_then(|c| c.get(1)) {
                let resolved = resolve_http_url(m.as_str(), Some(base_url));
                if !resolved.is_empty() {
                    chosen = resolved;
                    break;
                }
            }
        }
        if chosen.is_empty() || seen.contains(&chosen) {
            continue;
        }
        let Ok(u) = Url::parse(&chosen) else {
            continue;
        };
        let pathname = u.path();
        if SKIP_RX.is_match(pathname) || LOGO_PREFIX_RX.is_match(pathname) {
            continue;
        }
        seen.insert(chosen.clone());
        if is_thumbnail_image_url(&chosen) {
            tier_thumb.push(chosen);
        } else if FULL_UPLOAD_RX.is_match(pathname) {
            tier_full.push(chosen);
        } else {
            tier_other.push(chosen);
        }
    }

    tier_full.extend(tier_other);
    tier_full.extend(tier_thumb);
    tier_full
}

pub fn extract_actors_from_text(text: &str) -> Vec<String> {
    static FIELD_RX: LazyLock<Regex> = LazyLock::new(|| {
        let model_label = regex::escape(&decode_b64_utf8("6bq76LGG5aWz6YOO"));
        let pat = format!(
            r"(?:{model_label}|出演女优|女优|女郎|演员|主演|模特)\s*[：:]\s*([^\r\n<>;；|｜]{{1,50}})"
        );
        Regex::new(&pat).unwrap()
    });
    static STOP_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\s*(?:视频编号|编号|类型|标签|分类|下载|磁力|片名|时长|大小)\s*[：:].*$")
            .unwrap()
    });
    static SPLIT_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[/,，、\s]+").unwrap());

    let Some(caps) = FIELD_RX.captures(text) else {
        return Vec::new();
    };
    let trimmed = STOP_RX.replace(&caps[1], "").trim().to_string();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in SPLIT_RX.split(&trimmed) {
        let clean = part.trim();
        let len = clean.chars().count();
        if clean.is_empty() || len < 2 || len > 20 || matches!(clean, "未知" | "无" | "暂无") {
            continue;
        }
        if seen.insert(clean.to_string()) {
            out.push(clean.to_string());
        }
        if out.len() >= 5 {
            break;
        }
    }
    out
}

fn collect_json_ld_nodes<'a>(
    input: &'a serde_json::Value,
    out: &mut Vec<&'a serde_json::Map<String, serde_json::Value>>,
) {
    match input {
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_json_ld_nodes(item, out);
            }
        }
        serde_json::Value::Object(obj) => {
            out.push(obj);
            if let Some(graph) = obj.get("@graph") {
                if graph.is_array() {
                    collect_json_ld_nodes(graph, out);
                }
            }
        }
        _ => {}
    }
}

fn has_ld_type(node: &serde_json::Map<String, serde_json::Value>, target: &str) -> bool {
    match node.get("@type") {
        Some(serde_json::Value::String(s)) => s == target,
        Some(serde_json::Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(target)),
        _ => false,
    }
}

fn extract_actors_from_ld(creator: Option<&serde_json::Value>) -> Vec<String> {
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push_name = |s: &str| {
        let dec = decode_xml_entities(s);
        let clean = WS_RX.replace_all(&dec, " ").trim().to_string();
        if !clean.is_empty() && seen.insert(clean.clone()) {
            out.push(clean);
        }
    };
    let Some(val) = creator else {
        return out;
    };
    match val {
        serde_json::Value::String(s) => push_name(s),
        serde_json::Value::Object(obj) => {
            if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                push_name(name);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                match item {
                    serde_json::Value::String(s) => push_name(s),
                    serde_json::Value::Object(obj) => {
                        if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                            push_name(name);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

fn extract_meta_title_clean(raw: &str) -> String {
    static SITE_RX: LazyLock<Regex> = LazyLock::new(|| {
        let names = decode_b64_utf8("57OW5b+DVmxvZ3zns5blv4Pop4bpopF857OW5b+D5Lyg5aqS");
        let pat = format!(r"(?i)\s*[-|｜]\s*(?:{names})[^\r\n]*$");
        Regex::new(&pat).unwrap()
    });
    static HANDLE_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*-\s*@[^\s-]+$").unwrap());
    let s1 = SITE_RX.replace(raw, "");
    HANDLE_RX.replace(&s1, "").trim().to_string()
}

fn site_identifier(page_url: &str) -> String {
    let Ok(u) = Url::parse(page_url) else {
        return "rss".to_string();
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let stripped = host.strip_prefix("www.").unwrap_or(&host);
    let parts: Vec<&str> = stripped.split('.').filter(|s| !s.is_empty()).collect();
    let stem = if parts.len() >= 2 {
        parts[parts.len() - 2]
    } else {
        parts.first().copied().unwrap_or("rss")
    };
    let clean: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if clean.is_empty() {
        "rss".to_string()
    } else {
        clean
    }
}

fn extract_breadcrumb_studio(node: Option<&&serde_json::Map<String, serde_json::Value>>) -> String {
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    static GENERIC_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(?:首页|主页|home|视频|videos|全部|all)$").unwrap());
    let Some(list) = node
        .and_then(|n| n.get("itemListElement"))
        .and_then(|v| v.as_array())
    else {
        return String::new();
    };
    if list.len() < 2 {
        return String::new();
    }
    let second = &list[1];
    let raw = second
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| {
            second
                .get("item")
                .and_then(|i| i.get("name"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");
    let clean = WS_RX
        .replace_all(&decode_xml_entities(raw), " ")
        .trim()
        .to_string();
    if clean.is_empty() || GENERIC_RX.is_match(&clean) {
        String::new()
    } else {
        clean
    }
}

fn collect_ld_images(val: Option<&serde_json::Value>, out: &mut Vec<String>) {
    let Some(v) = val else {
        return;
    };
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_ld_images(Some(item), out);
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(u) = obj.get("url").and_then(|x| x.as_str()) {
                out.push(u.to_string());
            }
        }
        _ => {}
    }
}

fn extract_ld_keywords(node: Option<&&serde_json::Map<String, serde_json::Value>>) -> Vec<String> {
    let Some(val) = node.and_then(|n| n.get("keywords")) else {
        return Vec::new();
    };
    match val {
        serde_json::Value::String(s) => split_tag_list(s),
        serde_json::Value::Array(arr) => {
            let joined = arr
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(",");
            split_tag_list(&joined)
        }
        _ => Vec::new(),
    }
}

pub async fn fetch_detail_metadata(
    page_url: &str,
    fallback: &DetailFallback,
    cancel: &CancellationToken,
) -> CoreResult<DetailMetadata> {
    let parsed = Url::parse(page_url)?;
    let referer = fallback
        .referer
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}/", parsed.origin().ascii_serialization()));

    let client = scraper_http_client();
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(CoreError::aborted()),
        _ = tokio::time::sleep(Duration::from_secs(20)) => {
            return Err(CoreError::msg("详情页请求超时"));
        }
        r = client
            .get(parsed.as_str())
            .header("user-agent", DEFAULT_SCRAPER_UA)
            .header("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("referer", &referer)
            .send() => r?,
    };
    if !resp.status().is_success() {
        let st = resp.status().as_u16();
        let reason = resp.status().canonical_reason().unwrap_or("");
        return Err(CoreError::msg(format!(
            "详情页请求失败：HTTP {st} {reason}"
        )));
    }
    let html = tokio::select! {
        _ = cancel.cancelled() => return Err(CoreError::aborted()),
        t = resp.text() => t?,
    };

    static LD_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)<script\b[^>]*type=["']application/ld\+json["'][^>]*>([\s\S]*?)</script>"#,
        )
        .unwrap()
    });
    let mut ld_values = Vec::new();
    for caps in LD_RX.captures_iter(&html) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&caps[1]) {
            ld_values.push(v);
        }
    }
    let mut ld_nodes = Vec::new();
    for val in &ld_values {
        collect_json_ld_nodes(val, &mut ld_nodes);
    }

    let video_node = ld_nodes.iter().find(|n| has_ld_type(n, "VideoObject"));
    let article_node = ld_nodes.iter().find(|n| {
        has_ld_type(n, "Article")
            || has_ld_type(n, "NewsArticle")
            || has_ld_type(n, "BlogPosting")
            || has_ld_type(n, "WebPage")
    });
    let breadcrumb_node = ld_nodes.iter().find(|n| has_ld_type(n, "BreadcrumbList"));
    let org_node = ld_nodes.iter().find(|n| has_ld_type(n, "Organization"));

    let og_title = {
        let t = extract_meta_content(&html, "og:title");
        if t.is_empty() {
            extract_meta_content(&html, "title")
        } else {
            t
        }
    };
    let og_desc = {
        let d = extract_meta_content(&html, "og:description");
        if d.is_empty() {
            extract_meta_content(&html, "description")
        } else {
            d
        }
    };
    let og_site = extract_meta_content(&html, "og:site_name");
    let og_image = {
        let im = extract_meta_content(&html, "og:image");
        if im.is_empty() {
            extract_meta_content(&html, "twitter:image")
        } else {
            im
        }
    };

    let ld_name = video_node
        .and_then(|n| n.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| decode_xml_entities(s).trim().to_string())
        .unwrap_or_default();
    let article_headline = article_node
        .and_then(|n| n.get("headline"))
        .and_then(|v| v.as_str())
        .map(|s| decode_xml_entities(s).trim().to_string())
        .unwrap_or_default();
    let article_name = article_node
        .and_then(|n| n.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| decode_xml_entities(s).trim().to_string())
        .unwrap_or_default();

    let title = [
        ld_name,
        article_headline,
        extract_meta_title_clean(&og_title),
        extract_meta_title_clean(&article_name),
        fallback.title.as_deref().unwrap_or("").trim().to_string(),
        extract_path_id(page_url),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or_else(|| "未命名视频".to_string());

    let ld_desc = video_node
        .and_then(|n| n.get("description"))
        .and_then(|v| v.as_str())
        .map(|s| decode_xml_entities(s).trim().to_string())
        .unwrap_or_default();
    let article_desc = article_node
        .and_then(|n| n.get("description"))
        .and_then(|v| v.as_str())
        .map(|s| decode_xml_entities(s).trim().to_string())
        .unwrap_or_default();

    let mut actors = extract_actors_from_ld(video_node.and_then(|n| n.get("creator")));
    if actors.is_empty() && !og_title.is_empty() {
        static ACTOR_HANDLE_RX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"-\s*@([^\s-]+)\s*(?:-|$)").unwrap());
        if let Some(caps) = ACTOR_HANDLE_RX.captures(&og_title) {
            let a = caps[1].trim();
            if !a.is_empty() {
                actors.push(a.to_string());
            }
        }
    }
    if actors.is_empty() {
        static ANCHOR_ACTOR_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?is)<a\b[^>]*href=["']/a/[^"']+["'][^>]*>([\s\S]*?)</a>"#).unwrap()
        });
        static TAG_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
        static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
        let mut seen_a = HashSet::new();
        for caps in ANCHOR_ACTOR_RX.captures_iter(&html) {
            let stripped = TAG_RX.replace_all(&caps[1], "");
            let dec = decode_xml_entities(&stripped);
            let clean = WS_RX.replace_all(&dec, " ").trim().to_string();
            if !clean.is_empty()
                && clean != "演员"
                && clean != "所有演员"
                && seen_a.insert(clean.clone())
            {
                actors.push(clean);
                if actors.len() >= 5 {
                    break;
                }
            }
        }
    }
    if actors.is_empty() {
        let desc_src = if !ld_desc.is_empty() {
            &ld_desc
        } else if !og_desc.is_empty() {
            &og_desc
        } else {
            &article_desc
        };
        actors = extract_actors_from_text(desc_src);
    }
    if actors.is_empty() {
        static BODY_RX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?is)<body\b[^>]*>([\s\S]*?)</body>").unwrap());
        if let Some(caps) = BODY_RX.captures(&html) {
            let slice: String = caps[1].chars().take(30_000).collect();
            let body_text = strip_html_tags(&slice);
            actors = extract_actors_from_text(&body_text);
        }
    }
    if actors.is_empty() {
        if let Some(ref fb_actors) = fallback.actors {
            let mut seen_a = HashSet::new();
            for a in fb_actors {
                let clean = decode_xml_entities(a).trim().to_string();
                if !clean.is_empty() && seen_a.insert(clean.clone()) {
                    actors.push(clean);
                    if actors.len() >= 5 {
                        break;
                    }
                }
            }
        }
    }

    let ld_kw = extract_ld_keywords(video_node);
    let art_kw = extract_ld_keywords(article_node);
    let mut tags = Vec::new();
    let mut seen_tags = HashSet::new();
    for t in ld_kw
        .into_iter()
        .chain(art_kw)
        .chain(fallback.tags.clone().unwrap_or_default())
    {
        if !t.is_empty() && seen_tags.insert(t.clone()) {
            tags.push(t);
        }
    }

    let plot = if !ld_desc.is_empty() {
        ld_desc
    } else if !article_desc.is_empty() {
        article_desc
    } else if !og_desc.is_empty() {
        og_desc
    } else if !tags.is_empty() {
        tags.join(" / ")
    } else {
        title.clone()
    };

    let premiered = [
        format_date_ymd(
            video_node
                .and_then(|n| n.get("uploadDate"))
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        ),
        format_date_ymd(
            article_node
                .and_then(|n| n.get("datePublished"))
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        ),
        format_date_ymd(&extract_meta_content(&html, "article:published_time")),
        format_date_ymd(fallback.pub_date.as_deref().unwrap_or("")),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or_default();
    let year = premiered.chars().take(4).collect();

    let mut runtime = parse_iso_duration_minutes(
        video_node
            .and_then(|n| n.get("duration"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    if runtime == 0 {
        static CLOCK_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r#"(?i)class=["'][^"']*duration[^"']*["'][^>]*>\s*(\d{1,2}:\d{2}(?::\d{2})?)\s*<"#,
            )
            .unwrap()
        });
        if let Some(caps) = CLOCK_RX.captures(&html) {
            runtime = parse_clock_duration_minutes(&caps[1]);
        }
    }

    let mut cover_urls = Vec::new();
    let mut seen_covers = HashSet::new();
    let mut push_cover = |raw: &str| {
        let href = resolve_http_url(raw, Some(parsed.as_str()));
        if !href.is_empty() && seen_covers.insert(href.clone()) {
            cover_urls.push(href);
        }
    };

    let mut video_thumbs = Vec::new();
    collect_ld_images(
        video_node.and_then(|n| n.get("thumbnailUrl")),
        &mut video_thumbs,
    );
    for u in video_thumbs {
        push_cover(&u);
    }
    if let Some(ref fb_cov) = fallback.cover_url {
        push_cover(fb_cov);
    }

    static UPLOAD_OR_COVER_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)/wp-content/uploads/|/cover\.[a-z0-9]+(?:\?|$)").unwrap()
    });
    let html_images = extract_html_image_urls(&html, parsed.as_str());
    for img in &html_images {
        if !is_thumbnail_image_url(img) && UPLOAD_OR_COVER_RX.is_match(img) {
            push_cover(img);
        }
    }
    push_cover(&og_image);
    let mut article_imgs = Vec::new();
    collect_ld_images(article_node.and_then(|n| n.get("image")), &mut article_imgs);
    for u in article_imgs {
        push_cover(&u);
    }
    for img in &html_images {
        push_cover(img);
    }
    static COVER_FILE_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)https?://[^\s"'<>\\]+?/cover\.(?:jpg|jpeg|webp|png)(?:\?[^\s"'<>\\]*)?"#)
            .unwrap()
    });
    if let Some(m) = COVER_FILE_RX.find(&html) {
        push_cover(m.as_str());
    }

    let cover_url = cover_urls.first().cloned().unwrap_or_default();
    let studio = [
        fallback.studio.as_deref().unwrap_or("").trim().to_string(),
        extract_breadcrumb_studio(breadcrumb_node),
        og_site,
        org_node
            .and_then(|n| n.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| decode_xml_entities(s).trim().to_string())
            .unwrap_or_default(),
        parsed.host_str().unwrap_or("").to_string(),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or_default();

    Ok(DetailMetadata {
        title,
        plot,
        actors,
        premiered,
        year,
        runtime,
        genres: tags.clone(),
        tags,
        cover_url,
        cover_urls,
        studio,
        unique_id: extract_path_id(parsed.as_str()),
        unique_id_type: site_identifier(parsed.as_str()),
        page_url: parsed.to_string(),
    })
}
