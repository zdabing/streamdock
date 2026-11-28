use crate::contract::{ScrapeCandidate, ScrapeSourceType};
use crate::error::{CoreError, CoreResult};
use crate::http::{decode_b64_utf8, md5_hex};
use crate::rss_parser::{decode_xml_entities, strip_cdata};
use crate::scraper::detail::{
    DEFAULT_SCRAPER_UA, extract_actors_from_text, extract_html_image_urls, extract_path_id,
    resolve_http_url, scraper_http_client, split_tag_list, strip_html_tags,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use unicode_normalization::UnicodeNormalization;
use url::Url;

pub const CATALOG_TTL_MS: i64 = 15 * 60_000;
const REQUEST_TIMEOUT_SECS: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawRssCatalogItem {
    pub guid: String,
    pub title: String,
    pub page_url: String,
    pub pub_date: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NormalizedMatchText {
    pub cleaned: String,
    pub compact: String,
    pub no_bracket: String,
    pub bigrams: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct IndexedCatalogItem {
    pub guid: String,
    pub title: String,
    pub page_url: String,
    pub pub_date: String,
    pub tags: Vec<String>,
    pub path_id: String,
    pub compact: String,
    pub no_bracket: String,
    pub bigrams: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct RssCatalog {
    pub rss_url: String,
    pub feed_title: String,
    pub fetched_at: i64,
    pub items: Vec<IndexedCatalogItem>,
    pub by_path_id: HashMap<String, usize>,
    pub bigram_index: HashMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskRssCatalog {
    fetched_at: i64,
    feed_title: String,
    raw_items: Vec<RawRssCatalogItem>,
}

fn build_bigrams(text: &str) -> HashSet<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut set = HashSet::new();
    if chars.len() < 2 {
        if chars.len() == 1 {
            set.insert(chars[0].to_string());
        }
        return set;
    }
    for w in chars.windows(2) {
        let mut s = String::with_capacity(8);
        s.push(w[0]);
        s.push(w[1]);
        set.insert(s);
    }
    set
}

fn strip_compact_punctuation(text: &str) -> String {
    static RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s\p{P}\p{S}_]+").unwrap());
    RX.replace_all(&text.to_lowercase(), "").into_owned()
}

pub fn normalize_match_text(raw: &str) -> NormalizedMatchText {
    static SITE_SUFFIX_RX: LazyLock<Regex> = LazyLock::new(|| {
        let names = decode_b64_utf8("57OW5b+DVmxvZ3zns5blv4Pop4bpopF857OW5b+D5Lyg5aqS");
        let pat = format!(r"(?i)\s*[-|｜]\s*(?:{names})[^\r\n]*$");
        Regex::new(&pat).unwrap()
    });
    static COPY_NUM_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s*\(\d{1,3}\)\s*$").unwrap());
    static QUALITY_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:[\s._-]*[\[(]?(?:4k|2160p|1080p|720p|480p|360p|uhd|fhd|hd)[\])]?)\s*$")
            .unwrap()
    });
    static LEADING_BRACKET_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:【[^】]{1,40}】|\[[^\]]{1,40}\])\s*").unwrap());
    static HANDLE_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*-\s*@[^\s-]+").unwrap());

    let decoded = decode_xml_entities(raw);
    let mut text: String = decoded.nfkc().collect();
    text = text.trim().to_string();

    if let Some(dot) = text.rfind('.') {
        let ext = text[dot..].to_ascii_lowercase();
        if matches!(
            ext.as_str(),
            ".mp4"
                | ".mkv"
                | ".ts"
                | ".mov"
                | ".webm"
                | ".avi"
                | ".m4v"
                | ".flv"
                | ".wmv"
                | ".srt"
                | ".vtt"
                | ".ass"
                | ".ssa"
        ) {
            text.truncate(dot);
        }
    }

    text = SITE_SUFFIX_RX.replace(&text, "").trim().to_string();
    loop {
        let prev = text.clone();
        let s1 = COPY_NUM_RX.replace(&text, "");
        let s2 = QUALITY_RX.replace(&s1, "");
        text = s2.trim().to_string();
        if text == prev {
            break;
        }
    }

    let compact = strip_compact_punctuation(&text);
    let s1 = LEADING_BRACKET_RX.replace_all(&text, "");
    let s2 = HANDLE_RX.replace_all(&s1, "");
    let stripped = s2.trim().to_string();
    let no_bracket = strip_compact_punctuation(&stripped);
    let mut bigrams = build_bigrams(&compact);
    if !no_bracket.is_empty() && no_bracket != compact {
        bigrams.extend(build_bigrams(&no_bracket));
    }
    NormalizedMatchText {
        cleaned: text,
        compact,
        no_bracket,
        bigrams,
    }
}

fn index_catalog_items(
    rss_url: &str,
    feed_title: &str,
    fetched_at: i64,
    raw_items: Vec<RawRssCatalogItem>,
) -> RssCatalog {
    let mut items = Vec::with_capacity(raw_items.len());
    let mut by_path_id = HashMap::new();
    let mut bigram_index: HashMap<String, Vec<usize>> = HashMap::new();

    for raw in raw_items {
        let norm = normalize_match_text(&raw.title);
        let path_id = extract_path_id(&raw.page_url);
        let idx = items.len();
        if path_id.len() >= 12 {
            by_path_id.insert(path_id.to_ascii_lowercase(), idx);
        }
        for gram in &norm.bigrams {
            bigram_index.entry(gram.clone()).or_default().push(idx);
        }
        items.push(IndexedCatalogItem {
            guid: raw.guid,
            title: raw.title,
            page_url: raw.page_url,
            pub_date: raw.pub_date,
            tags: raw.tags,
            path_id,
            compact: norm.compact,
            no_bracket: norm.no_bracket,
            bigrams: norm.bigrams,
        });
    }

    RssCatalog {
        rss_url: rss_url.to_string(),
        feed_title: feed_title.to_string(),
        fetched_at,
        items,
        by_path_id,
        bigram_index,
    }
}

fn extract_catalog_tag(block: &str, tag: &str) -> String {
    let pattern = format!(r"(?is)<{tag}\b[^>]*>([\s\S]*?)</{tag}>");
    let Ok(rx) = Regex::new(&pattern) else {
        return String::new();
    };
    let Some(caps) = rx.captures(block) else {
        return String::new();
    };
    let inner = &caps[1];
    if inner.contains("<![CDATA[") {
        strip_cdata(inner)
    } else {
        decode_xml_entities(inner).trim().to_string()
    }
}

pub fn parse_rss_catalog_xml(xml: &str, rss_url: &str) -> (String, Vec<RawRssCatalogItem>) {
    static FIRST_ITEM_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<item\b").unwrap());
    static ITEM_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<item\b[^>]*>([\s\S]*?)</item>").unwrap());

    let first_idx = FIRST_ITEM_RX.find(xml).map(|m| m.start());
    let header = match first_idx {
        Some(i) => &xml[..i],
        None => xml,
    };
    let host_fallback = Url::parse(rss_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "RSS 订阅源".to_string());
    let feed_title = {
        let t = extract_catalog_tag(header, "title");
        if t.is_empty() { host_fallback } else { t }
    };

    let mut raw_items = Vec::new();
    let mut seen = HashSet::new();
    for caps in ITEM_RX.captures_iter(xml) {
        let block = &caps[1];
        let title = extract_catalog_tag(block, "title");
        let raw_link = extract_catalog_tag(block, "link");
        let page_url = if raw_link.is_empty() {
            String::new()
        } else {
            resolve_http_url(&raw_link, Some(rss_url))
        };
        let guid = {
            let g = extract_catalog_tag(block, "guid");
            if !g.is_empty() {
                g
            } else if !page_url.is_empty() {
                page_url.clone()
            } else {
                title.clone()
            }
        };
        let pub_date = extract_catalog_tag(block, "pubDate");
        let description = extract_catalog_tag(block, "description");
        if guid.is_empty() || page_url.is_empty() || title.is_empty() || !seen.insert(guid.clone())
        {
            continue;
        }
        raw_items.push(RawRssCatalogItem {
            guid,
            title,
            page_url,
            pub_date,
            tags: split_tag_list(&description),
        });
    }
    (feed_title, raw_items)
}

pub fn build_rss_catalog(
    rss_url: &str,
    feed_title: &str,
    raw_items: Vec<RawRssCatalogItem>,
) -> Arc<RssCatalog> {
    Arc::new(index_catalog_items(
        rss_url,
        feed_title,
        chrono::Utc::now().timestamp_millis(),
        raw_items,
    ))
}

static CATALOG_CACHE: OnceLock<Mutex<HashMap<String, Arc<RssCatalog>>>> = OnceLock::new();
static CATALOG_INFLIGHT: OnceLock<
    Mutex<HashMap<String, watch::Receiver<Option<Result<Arc<RssCatalog>, String>>>>>,
> = OnceLock::new();

pub async fn load_rss_catalog(
    rss_url: &str,
    cache_dir: Option<&Path>,
    refresh_rss: bool,
    cancel: &CancellationToken,
) -> CoreResult<Arc<RssCatalog>> {
    let parsed = Url::parse(rss_url)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CoreError::msg("RSS 地址应以 http:// 或 https:// 开头"));
    }
    let href = parsed.to_string();
    let cache_map = CATALOG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let inflight_map = CATALOG_INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()));
    let now_ms = chrono::Utc::now().timestamp_millis();

    if !refresh_rss {
        let guard = cache_map.lock().await;
        if let Some(cat) = guard.get(&href) {
            if now_ms - cat.fetched_at < CATALOG_TTL_MS {
                return Ok(cat.clone());
            }
        }
    }

    let existing_rx = if !refresh_rss {
        inflight_map.lock().await.get(&href).cloned()
    } else {
        None
    };

    if let Some(mut rx) = existing_rx {
        loop {
            if let Some(res) = rx.borrow().clone() {
                return res.map_err(CoreError::msg);
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::aborted()),
                chg = rx.changed() => {
                    if chg.is_err() {
                        return Err(CoreError::msg("加载 RSS 索引失败"));
                    }
                }
            }
        }
    }

    let (tx, rx) = watch::channel(None);
    inflight_map.lock().await.insert(href.clone(), rx);

    let res: CoreResult<Arc<RssCatalog>> = async {
        let disk_path = cache_dir.map(|d| d.join(format!("rss-scraper-{}.json", md5_hex(&href))));
        if !refresh_rss {
            if let Some(ref dp) = disk_path {
                if let Ok(raw) = tokio::fs::read_to_string(dp).await {
                    if let Ok(saved) = serde_json::from_str::<DiskRssCatalog>(&raw) {
                        if chrono::Utc::now().timestamp_millis() - saved.fetched_at < CATALOG_TTL_MS
                        {
                            let cat = Arc::new(index_catalog_items(
                                &href,
                                if saved.feed_title.is_empty() {
                                    parsed.host_str().unwrap_or("RSS 订阅源")
                                } else {
                                    &saved.feed_title
                                },
                                saved.fetched_at,
                                saved.raw_items,
                            ));
                            cache_map.lock().await.insert(href.clone(), cat.clone());
                            return Ok(cat);
                        }
                    }
                }
            }
        }

        let client = scraper_http_client();
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(CoreError::aborted()),
            _ = tokio::time::sleep(Duration::from_secs(REQUEST_TIMEOUT_SECS)) => {
                return Err(CoreError::msg("拉取 RSS 超时"));
            }
            r = client
                .get(&href)
                .header("user-agent", DEFAULT_SCRAPER_UA)
                .header("accept", "application/rss+xml,application/xml,text/xml;q=0.9,*/*;q=0.8")
                .send() => r?,
        };
        if !resp.status().is_success() {
            let st = resp.status().as_u16();
            let reason = resp.status().canonical_reason().unwrap_or("");
            return Err(CoreError::msg(format!("拉取 RSS 失败：HTTP {st} {reason}")));
        }
        let xml = tokio::select! {
            _ = cancel.cancelled() => return Err(CoreError::aborted()),
            t = resp.text() => t?,
        };
        let (feed_title, raw_items) = parse_rss_catalog_xml(&xml, &href);
        let fetched_at = chrono::Utc::now().timestamp_millis();

        if let Some(ref dp) = disk_path {
            let disk = DiskRssCatalog {
                fetched_at,
                feed_title: feed_title.clone(),
                raw_items: raw_items.clone(),
            };
            if let Ok(json) = serde_json::to_string(&disk) {
                if let Some(p) = dp.parent() {
                    let _ = tokio::fs::create_dir_all(p).await;
                }
                let tmp = dp.with_extension("json.tmp");
                if tokio::fs::write(&tmp, json.as_bytes()).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, dp).await;
                }
            }
        }

        let cat = Arc::new(index_catalog_items(
            &href,
            &feed_title,
            fetched_at,
            raw_items,
        ));
        cache_map.lock().await.insert(href.clone(), cat.clone());
        Ok(cat)
    }
    .await;

    inflight_map.lock().await.remove(&href);
    let sent = match &res {
        Ok(c) => Ok(c.clone()),
        Err(e) => Err(e.to_string()),
    };
    let _ = tx.send(Some(sent));
    res
}

fn score_strings_pair(a: &str, b: &str) -> u32 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let len_a = a.chars().count();
    let len_b = b.chars().count();
    let (shorter, longer, s_len, l_len) = if len_a <= len_b {
        (a, b, len_a, len_b)
    } else {
        (b, a, len_b, len_a)
    };
    let mut best = 0u32;
    if s_len >= 6 && longer.contains(shorter) {
        let ratio = s_len as f64 / l_len as f64;
        best = (78.0 + 20.0 * ratio).round() as u32;
    }
    let grams_a = build_bigrams(a);
    let grams_b = build_bigrams(b);
    if !grams_a.is_empty() && !grams_b.is_empty() {
        let (small, big) = if grams_a.len() <= grams_b.len() {
            (&grams_a, &grams_b)
        } else {
            (&grams_b, &grams_a)
        };
        let inter = small.iter().filter(|g| big.contains(*g)).count();
        if inter > 0 {
            let dice = (2 * inter) as f64 / (grams_a.len() + grams_b.len()) as f64;
            let overlap = inter as f64 / small.len() as f64;
            let len_factor = (s_len as f64 / 10.0).min(1.0);
            let hybrid = (100.0 * (0.65 * dice + 0.35 * overlap * len_factor)).round() as u32;
            if hybrid > best {
                best = hybrid;
            }
        }
    }
    best
}

pub fn score_pair(query: &NormalizedMatchText, item: &IndexedCatalogItem) -> u32 {
    if query.compact.is_empty() || item.compact.is_empty() {
        return 0;
    }
    if query.compact == item.compact {
        return 100;
    }
    if !query.no_bracket.is_empty()
        && !item.no_bracket.is_empty()
        && query.no_bracket.chars().count() >= 4
        && query.no_bracket == item.no_bracket
    {
        return 96;
    }
    let mut best = score_strings_pair(&query.compact, &item.compact);
    if !query.no_bracket.is_empty()
        && !item.no_bracket.is_empty()
        && (query.no_bracket != query.compact || item.no_bracket != item.compact)
    {
        let s = score_strings_pair(&query.no_bracket, &item.no_bracket);
        if s > best {
            best = s;
        }
    }
    best.min(100)
}

pub fn match_title_against_catalog(
    filename: &str,
    catalog: &RssCatalog,
    limit: usize,
) -> Vec<ScrapeCandidate> {
    static PATH_ID_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9]{12,}").unwrap());

    let query = normalize_match_text(filename);
    let mut scores: HashMap<usize, u32> = HashMap::new();

    for m in PATH_ID_RX.find_iter(&filename.to_ascii_lowercase()) {
        if let Some(&idx) = catalog.by_path_id.get(m.as_str()) {
            scores.insert(idx, 100);
        }
    }

    let mut candidate_hits: HashMap<usize, usize> = HashMap::new();
    let common_cap = 400usize.max((catalog.items.len() * 35) / 100);
    for gram in &query.bigrams {
        if let Some(bucket) = catalog.bigram_index.get(gram) {
            if catalog.items.len() > 200 && bucket.len() > common_cap {
                continue;
            }
            for &idx in bucket {
                *candidate_hits.entry(idx).or_insert(0) += 1;
            }
        }
    }

    let indices: Vec<usize> = if catalog.items.len() <= 500 || candidate_hits.is_empty() {
        (0..catalog.items.len()).collect()
    } else {
        let mut hits: Vec<(usize, usize)> = candidate_hits.into_iter().collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1));
        hits.into_iter().take(400).map(|(i, _)| i).collect()
    };

    for idx in indices {
        if scores.get(&idx) == Some(&100) {
            continue;
        }
        let s = score_pair(&query, &catalog.items[idx]);
        if s >= 25 {
            scores.insert(idx, s);
        }
    }

    let mut ranked: Vec<(usize, u32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    ranked
        .into_iter()
        .take(limit)
        .map(|(idx, score)| {
            let item = &catalog.items[idx];
            ScrapeCandidate {
                guid: item.guid.clone(),
                title: item.title.clone(),
                page_url: item.page_url.clone(),
                pub_date: item.pub_date.clone(),
                tags: item.tags.clone(),
                score,
                source_id: None,
                source_name: None,
                source_type: Some(ScrapeSourceType::Rss),
                actors: None,
                studio: None,
                cover_url: None,
                referer: None,
            }
        })
        .collect()
}

pub fn resolve_wordpress_api_url(raw_url: &str) -> CoreResult<String> {
    let parsed =
        Url::parse(raw_url.trim()).map_err(|_| CoreError::msg("WordPress 数据源地址无效"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CoreError::msg(
            "WordPress 数据源地址应以 http:// 或 https:// 开头",
        ));
    }
    let origin = parsed.origin().ascii_serialization();
    let trimmed_path = parsed.path().trim_end_matches('/');
    if trimmed_path.to_ascii_lowercase().ends_with("/wp-json") {
        return Ok(format!("{origin}{trimmed_path}/wp/v2/posts"));
    }
    if parsed.path().contains("/wp-json/") {
        return Ok(format!("{origin}{trimmed_path}"));
    }
    Ok(format!("{origin}/wp-json/wp/v2/posts"))
}

#[derive(Debug, Clone)]
pub struct StudioCode {
    pub prefix: String,
    pub num: u32,
    pub code: String,
}

pub fn extract_studio_codes(text: &str) -> Vec<StudioCode> {
    static CODE_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:^|[^a-z0-9])([a-z]{2,6})[-_]?(\d{2,5})").unwrap());
    let mut codes = Vec::new();
    let mut seen = HashSet::new();

    for caps in CODE_RX.captures_iter(text) {
        let full = caps.get(0).unwrap();
        let after_ok = text[full.end()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        if !after_ok {
            continue;
        }
        let prefix = caps[1].to_ascii_lowercase();
        let digits = &caps[2];
        let token = format!("{prefix}{digits}");
        if matches!(
            token.as_str(),
            "h264"
                | "h265"
                | "x264"
                | "x265"
                | "aac2"
                | "ac3"
                | "mp4"
                | "mkv"
                | "m4v"
                | "utf8"
                | "win32"
                | "win64"
                | "x86"
                | "x64"
        ) {
            continue;
        }
        let code = format!("{}-{digits}", prefix.to_ascii_uppercase());
        if seen.insert(code.clone()) {
            let num = digits.parse::<u32>().unwrap_or(0);
            codes.push(StudioCode { prefix, num, code });
        }
    }
    codes
}

pub fn extract_search_queries(filename: &str) -> Vec<String> {
    static DATE_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\d{4}[-./]\d{1,2}[-./]\d{1,2}(?:[\s_-]+|$)").unwrap());
    static NUM_PREFIX_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\d{1,8}(?:\s+[-—–_.]?\s*|[-_.]+\s*)").unwrap());
    static CODEC_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:h\.?264|h\.?265|x264|x265|hevc|avc|av1)\b").unwrap()
    });
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    static LEADING_BRACKET_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:【[^】]{1,40}】|\[[^\]]{1,40}\])\s*").unwrap());
    static SPLIT_SEG_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[_|｜/\\【】\[\]]+|\s+[-—–]+\s+").unwrap());
    static CJK_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\u{4e00}-\u{9fff}]").unwrap());

    let norm = normalize_match_text(filename);
    let s1 = DATE_RX.replace(&norm.cleaned, "");
    let s2 = NUM_PREFIX_RX.replace(&s1, "");
    let s3 = CODEC_RX.replace_all(&s2, " ");
    let cleaned = WS_RX.replace_all(&s3, " ").trim().to_string();
    if cleaned.is_empty() {
        return Vec::new();
    }

    let mut queries = Vec::new();
    let mut seen = HashSet::new();
    let push_query = |q: &str, queries: &mut Vec<String>, seen: &mut HashSet<String>| {
        let val = WS_RX.replace_all(q, " ").trim().to_string();
        if val.is_empty() {
            return;
        }
        let compact = strip_compact_punctuation(&val);
        if compact.chars().count() < 2 {
            return;
        }
        if seen.insert(compact) {
            queries.push(val);
        }
    };

    for sc in extract_studio_codes(&cleaned) {
        push_query(&sc.code, &mut queries, &mut seen);
    }

    let stripped = LEADING_BRACKET_RX.replace(&cleaned, "").trim().to_string();
    struct SegInfo {
        text: String,
        compact_len: usize,
        cjk_count: usize,
    }
    let raw_segments: Vec<SegInfo> = SPLIT_SEG_RX
        .split(&stripped)
        .filter_map(|part| {
            let trimmed = part.trim_matches(|c: char| {
                c.is_whitespace() || matches!(c, '-' | '_' | '.' | '·' | '•')
            });
            let text = WS_RX.replace_all(trimmed, " ").trim().to_string();
            if text.is_empty() {
                return None;
            }
            let compact_len = strip_compact_punctuation(&text).chars().count();
            let cjk_count = CJK_RX.find_iter(&text).count();
            if compact_len >= 4 || cjk_count >= 3 {
                Some(SegInfo {
                    text,
                    compact_len,
                    cjk_count,
                })
            } else {
                None
            }
        })
        .collect();

    let has_long = raw_segments.iter().any(|s| s.compact_len >= 5);
    let mut meaningful: Vec<SegInfo> = raw_segments
        .into_iter()
        .filter(|s| !has_long || s.compact_len >= 5)
        .collect();
    meaningful.sort_by(|a, b| b.compact_len.cmp(&a.compact_len));

    for seg in meaningful {
        if queries.len() >= 3 {
            break;
        }
        let _ = seg.cjk_count;
        push_query(&seg.text, &mut queries, &mut seen);
    }

    if queries.is_empty() && !stripped.is_empty() {
        let words: Vec<&str> = stripped.split_whitespace().collect();
        if stripped.chars().count() > 22 && words.len() > 2 {
            push_query(&words[..3].join(" "), &mut queries, &mut seen);
            push_query(&words[words.len() - 3..].join(" "), &mut queries, &mut seen);
        } else {
            push_query(&stripped, &mut queries, &mut seen);
        }
    }

    queries.truncate(3);
    queries
}

#[derive(Debug, Clone)]
pub struct WpPostItem {
    pub guid: String,
    pub title: String,
    pub page_url: String,
    pub pub_date: String,
    pub tags: Vec<String>,
    pub actors: Vec<String>,
    pub studio: String,
    pub cover_url: String,
    pub norm: NormalizedMatchText,
}

fn is_generic_wp_category(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "未分类" | "uncategorized" | "视频" | "全部" | "video" | "videos" | "默认分类"
    )
}

fn is_generic_wp_tag(tag: &str) -> bool {
    static RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:高清|中文字幕|字幕|无码|有码|国语|原画|完整版?|4k|2k|1080p|720p|480p|hd|mp4|magnet|推荐|精选|热门|最新)$").unwrap()
    });
    RX.is_match(tag)
}

pub fn parse_wordpress_posts(json: &serde_json::Value, base_url: &str) -> Vec<WpPostItem> {
    static WS_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    let Some(arr) = json.as_array() else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    for post in arr {
        let Some(obj) = post.as_object() else {
            continue;
        };
        let title = strip_html_tags(
            obj.get("title")
                .and_then(|t| t.get("rendered"))
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let page_url = resolve_http_url(
            obj.get("link").and_then(|v| v.as_str()).unwrap_or(""),
            Some(base_url),
        );
        let raw_guid = decode_xml_entities(
            obj.get("guid")
                .and_then(|g| g.get("rendered"))
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        )
        .trim()
        .to_string();
        let id_fallback = obj
            .get("id")
            .map(|id| format!("{base_url}#{}", id.to_string().trim_matches('"')))
            .unwrap_or_default();
        let guid = if !page_url.is_empty() {
            page_url.clone()
        } else if !raw_guid.is_empty() {
            raw_guid
        } else {
            id_fallback
        };
        if title.is_empty() || page_url.is_empty() || guid.is_empty() || !seen.insert(guid.clone())
        {
            continue;
        }

        let mut categories = Vec::new();
        let mut post_tags = Vec::new();
        if let Some(groups) = obj
            .get("_embedded")
            .and_then(|e| e.get("wp:term"))
            .and_then(|v| v.as_array())
        {
            for group in groups {
                let Some(terms) = group.as_array() else {
                    continue;
                };
                for term in terms {
                    let name = WS_RX
                        .replace_all(
                            &decode_xml_entities(
                                term.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                            ),
                            " ",
                        )
                        .trim()
                        .to_string();
                    if name.is_empty() || name.chars().count() > 40 {
                        continue;
                    }
                    match term.get("taxonomy").and_then(|v| v.as_str()) {
                        Some("category") => {
                            if !is_generic_wp_category(&name) {
                                categories.push(name);
                            }
                        }
                        Some("post_tag") => post_tags.push(name),
                        _ => {}
                    }
                }
            }
        }

        let mut tags = Vec::new();
        let mut seen_tags = HashSet::new();
        for t in categories.iter().chain(post_tags.iter()) {
            if seen_tags.insert(t.clone()) {
                tags.push(t.clone());
            }
        }
        let studio = categories.first().cloned().unwrap_or_default();
        let excerpt_raw = obj
            .get("excerpt")
            .and_then(|e| e.get("rendered"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                obj.get("content")
                    .and_then(|c| c.get("rendered"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");
        let excerpt = strip_html_tags(excerpt_raw);
        let mut actors = extract_actors_from_text(&excerpt);
        if actors.is_empty() {
            let mut seen_a = HashSet::new();
            for tag in &post_tags {
                let len = tag.chars().count();
                if (2..=15).contains(&len) && !is_generic_wp_tag(tag) && seen_a.insert(tag.clone())
                {
                    actors.push(tag.clone());
                    if actors.len() >= 5 {
                        break;
                    }
                }
            }
        }

        let content_html = obj
            .get("content")
            .and_then(|c| c.get("rendered"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content_imgs = extract_html_image_urls(content_html, &page_url);
        let featured = resolve_http_url(
            obj.get("_embedded")
                .and_then(|e| e.get("wp:featuredmedia"))
                .and_then(|a| a.get(0))
                .and_then(|m| m.get("source_url"))
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            Some(&page_url),
        );
        let cover_url = content_imgs
            .into_iter()
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(featured);

        let pub_date = obj
            .get("date_gmt")
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("date").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();

        items.push(WpPostItem {
            guid,
            norm: normalize_match_text(&title),
            title,
            page_url,
            pub_date,
            tags,
            actors,
            studio,
            cover_url,
        });
    }

    items
}

pub fn score_wordpress_item(filename: &str, item: &WpPostItem) -> u32 {
    static DATE_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\d{4}[-./]\d{1,2}[-./]\d{1,2}(?:[\s_-]+|$)").unwrap());
    static NUM_PREFIX_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\d{1,8}(?:\s+[-—–_.]?\s*|[-_.]+\s*)").unwrap());
    static SPLIT_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[_|｜/\\【】\[\]]+|\s+[-—–]+\s+|\s+").unwrap());
    static LEAD_CODE_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?i)[a-z]{2,6}\d{2,5}").unwrap());

    let query = normalize_match_text(filename);
    let indexed = IndexedCatalogItem {
        guid: item.guid.clone(),
        title: item.title.clone(),
        page_url: item.page_url.clone(),
        pub_date: item.pub_date.clone(),
        tags: item.tags.clone(),
        path_id: extract_path_id(&item.page_url),
        compact: item.norm.compact.clone(),
        no_bracket: item.norm.no_bracket.clone(),
        bigrams: item.norm.bigrams.clone(),
    };
    let mut best = score_pair(&query, &indexed);

    let file_codes = extract_studio_codes(&query.cleaned);
    let item_codes = extract_studio_codes(&item.norm.cleaned);
    if !file_codes.is_empty() && !item_codes.is_empty() {
        let mut matched_code = false;
        let mut conflicting_ep = false;
        for fc in &file_codes {
            for ic in &item_codes {
                if fc.prefix == ic.prefix && fc.num == ic.num {
                    matched_code = true;
                } else if fc.prefix == ic.prefix && fc.num != ic.num {
                    conflicting_ep = true;
                }
            }
        }
        if matched_code {
            best = best.max(98);
        } else if conflicting_ep {
            return best.min(20);
        }
    }

    let s1 = DATE_RX.replace(&query.cleaned, "");
    let stripped_file = NUM_PREFIX_RX.replace(&s1, "").trim().to_string();
    let mut chunks: Vec<String> = SPLIT_RX
        .split(&stripped_file)
        .map(strip_compact_punctuation)
        .filter(|c| c.chars().count() >= 6)
        .collect();
    let item_without_code = LEAD_CODE_RX.replace(&item.norm.no_bracket, "").into_owned();
    if item_without_code.chars().count() >= 6 && query.compact.contains(&item_without_code) {
        chunks.push(item_without_code.clone());
    }
    let q_len = query.compact.chars().count();
    let i_len = item.norm.compact.chars().count();
    for chunk in chunks {
        if item.norm.compact.contains(&chunk)
            || (chunk == item_without_code && query.compact.contains(&chunk))
        {
            let c_len = chunk.chars().count();
            let denom = c_len.max(q_len.min(i_len)).max(1);
            let ratio = (c_len as f64 / denom as f64).min(1.0);
            let seg_score = (74.0 + 22.0 * ratio).round() as u32;
            if seg_score > best {
                best = seg_score;
            }
        }
    }

    if best >= 40
        && item.actors.iter().any(|a| {
            let ca = strip_compact_punctuation(a);
            ca.chars().count() >= 2 && query.compact.contains(&ca)
        })
    {
        best = (best + 6).min(98);
    }

    best.min(100)
}

pub async fn search_wordpress_posts(
    api_url: &str,
    query: &str,
    referer: Option<&str>,
    cancel: &CancellationToken,
) -> CoreResult<(Vec<WpPostItem>, Option<usize>)> {
    let mut endpoint = Url::parse(api_url)?;
    endpoint
        .query_pairs_mut()
        .append_pair("search", query)
        .append_pair("per_page", "8")
        .append_pair("_embed", "1");
    let origin = endpoint.origin().ascii_serialization();
    let eff_ref = referer
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{origin}/"));

    let client = scraper_http_client();
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(CoreError::aborted()),
        _ = tokio::time::sleep(Duration::from_secs(REQUEST_TIMEOUT_SECS)) => {
            return Err(CoreError::msg("WordPress 检索超时"));
        }
        r = client
            .get(endpoint.as_str())
            .header("user-agent", DEFAULT_SCRAPER_UA)
            .header("accept", "application/json")
            .header("referer", &eff_ref)
            .send() => r?,
    };
    if !resp.status().is_success() {
        let st = resp.status().as_u16();
        let reason = resp.status().canonical_reason().unwrap_or("");
        return Err(CoreError::msg(format!(
            "WordPress 检索失败：HTTP {st} {reason}"
        )));
    }
    let wp_total = resp
        .headers()
        .get("x-wp-total")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    let json: serde_json::Value = tokio::select! {
        _ = cancel.cancelled() => return Err(CoreError::aborted()),
        j = resp.json() => j?,
    };
    Ok((parse_wordpress_posts(&json, &origin), wp_total))
}
