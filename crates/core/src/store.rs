use crate::contract::{
    ProxyMode, RssFeedItem, RssItemCounts, RssItemStatus, RssItemsPage, RssSource, RssSourceInput,
    RssSourceMode, ScrapeConfig, ScrapeDataSource, ScrapeFolderTemplate, ScrapeItemOutcome,
    ScrapeJobSnapshot, ScrapeJobStatus, ScrapeMode, ScrapePosterCrop, ScrapeRunRequest,
    ScrapeSourceType, ScrapeTransferMode, ServerConfig, TaskItem, TaskScrapeStatus, TaskStatus,
};
use crate::error::{CoreError, CoreResult};
use crate::http::{decode_b64_utf8, format_headers_str};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use url::Url;

pub fn default_rss_url() -> String {
    static FALLBACK: LazyLock<String> =
        LazyLock::new(|| decode_b64_utf8("aHR0cHM6Ly90YW5neGludmxvZy5hcHAvcnNzLnhtbA=="));
    std::env::var("DS_DEFAULT_RSS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| FALLBACK.clone())
}

pub fn default_wp_url() -> String {
    static FALLBACK: LazyLock<String> =
        LazyLock::new(|| decode_b64_utf8("aHR0cHM6Ly9tYWRvdXF1LmNvbQ=="));
    std::env::var("DS_DEFAULT_WP_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| FALLBACK.clone())
}

fn default_rss_name() -> String {
    static VAL: LazyLock<String> = LazyLock::new(|| decode_b64_utf8("57OW5b+DVmxvZyAoUlNTKQ=="));
    VAL.clone()
}

fn default_wp_id() -> String {
    static VAL: LazyLock<String> = LazyLock::new(|| decode_b64_utf8("d3AtbWFkb3VxdQ=="));
    VAL.clone()
}

fn default_wp_name() -> String {
    static VAL: LazyLock<String> =
        LazyLock::new(|| decode_b64_utf8("6bq76LGG5Yy6IChXb3JkUHJlc3Mp"));
    VAL.clone()
}

fn default_wp_host() -> String {
    static VAL: LazyLock<String> = LazyLock::new(|| decode_b64_utf8("bWFkb3VxdS5jb20="));
    VAL.clone()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeTimingTotals {
    pub count: usize,
    pub total_ms: u64,
    pub detail_ms: u64,
    pub transfer_ms: u64,
    pub artwork_ms: u64,
    pub crop_ms: u64,
    pub cross_fs_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedScrapeJob {
    pub snapshot: ScrapeJobSnapshot,
    pub revision: u64,
    pub request: ScrapeRunRequest,
    pub failed_results: Vec<ScrapeItemOutcome>,
    pub recent_succeeded: Vec<ScrapeItemOutcome>,
    pub succeeded_paths: Vec<String>,
    pub processed_paths: Vec<String>,
    pub failed_paths: Vec<String>,
    pub timing_totals: ScrapeTimingTotals,
    pub elapsed_before_run_ms: u64,
}

#[derive(Debug, Clone)]
pub struct EnvConfigDefaults {
    pub save_dir: String,
    pub ffmpeg_path: String,
    pub proxy_mode: ProxyMode,
    pub proxy_url: String,
    pub no_proxy: String,
}

impl Default for EnvConfigDefaults {
    fn default() -> Self {
        let proxy_mode = match std::env::var("DS_PROXY_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "custom" => ProxyMode::Custom,
            "disabled" => ProxyMode::Disabled,
            _ => ProxyMode::System,
        };
        Self {
            save_dir: std::env::var("DS_SAVE_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "./downloads".to_string()),
            ffmpeg_path: std::env::var("DS_FFMPEG_PATH").unwrap_or_default(),
            proxy_mode,
            proxy_url: std::env::var("DS_PROXY_URL").unwrap_or_default(),
            no_proxy: std::env::var("DS_NO_PROXY").unwrap_or_default(),
        }
    }
}

pub fn default_server_config(env: &EnvConfigDefaults) -> ServerConfig {
    ServerConfig {
        thread_num: 4,
        save_dir: env.save_dir.clone(),
        del_cache: true,
        max_downloads: 2,
        ffmpeg_path: env.ffmpeg_path.clone(),
        headers: String::new(),
        merge_timeout: 180,
        proxy_mode: env.proxy_mode,
        proxy_url: env.proxy_url.clone(),
        no_proxy: env.no_proxy.clone(),
    }
}

pub fn default_scrape_sources() -> Vec<ScrapeDataSource> {
    vec![
        ScrapeDataSource {
            id: "rss-default".to_string(),
            name: default_rss_name(),
            source_type: ScrapeSourceType::Rss,
            url: default_rss_url(),
            enabled: true,
            referer: Some(String::new()),
        },
        ScrapeDataSource {
            id: default_wp_id(),
            name: default_wp_name(),
            source_type: ScrapeSourceType::Wordpress,
            url: default_wp_url(),
            enabled: true,
            referer: Some(String::new()),
        },
    ]
}

pub fn default_scrape_config(save_dir: &str) -> ScrapeConfig {
    let dir = if save_dir.trim().is_empty() {
        "./downloads"
    } else {
        save_dir.trim()
    };
    ScrapeConfig {
        scan_dirs: vec![dir.to_string()],
        sources: default_scrape_sources(),
        feed_preset: "custom".to_string(),
        rss_url: default_rss_url(),
        referer: String::new(),
        mode: ScrapeMode::InPlace,
        output_dir: String::new(),
        transfer_mode: ScrapeTransferMode::Move,
        folder_template: ScrapeFolderTemplate::ActorTitle,
        poster_crop: ScrapePosterCrop::Center,
        min_score: 55,
        recursive: true,
        skip_existing_nfo: true,
        download_images: true,
        overwrite_nfo: false,
        concurrency: 3,
    }
}

pub fn sanitize_source_id(raw: &str) -> String {
    let replaced: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut collapsed = String::with_capacity(replaced.len());
    let mut prev_dash = false;
    for c in replaced.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push('-');
                prev_dash = true;
            }
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    collapsed.trim_matches('-').chars().take(40).collect()
}

pub fn normalize_http_href(raw: Option<&str>, label: &str, required: bool) -> CoreResult<String> {
    let Some(s) = raw else {
        if required {
            return Err(CoreError::msg(format!("{label}不能为空")));
        }
        return Ok(String::new());
    };
    let text = s.trim();
    if text.is_empty() {
        if required {
            return Err(CoreError::msg(format!("{label}不能为空")));
        }
        return Ok(String::new());
    }
    let parsed = Url::parse(text)
        .map_err(|_| CoreError::msg(format!("{label}应为有效的 http 或 https 地址")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CoreError::msg(format!("{label}应为 http 或 https 地址")));
    }
    Ok(parsed.to_string())
}

pub fn normalize_scrape_config_value(
    val: &serde_json::Value,
    fallback_save_dir: &str,
) -> ScrapeConfig {
    let base = default_scrape_config(fallback_save_dir);
    let Some(obj) = val.as_object() else {
        return base;
    };
    let mut cfg = base.clone();

    if let Some(arr) = obj.get("scanDirs").and_then(|v| v.as_array()) {
        let mut seen = HashSet::new();
        let mut dirs = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                let t = s.trim();
                if t.is_empty() {
                    continue;
                }
                let key = if cfg!(windows) {
                    t.to_ascii_lowercase()
                } else {
                    t.to_string()
                };
                if seen.insert(key) {
                    dirs.push(t.to_string());
                    if dirs.len() >= 20 {
                        break;
                    }
                }
            }
        }
        if !dirs.is_empty() {
            cfg.scan_dirs = dirs;
        }
    } else if let Some(sd) = obj.get("scanDir").and_then(|v| v.as_str()) {
        if !sd.trim().is_empty() {
            cfg.scan_dirs = vec![sd.trim().to_string()];
        }
    }

    if let Some(arr) = obj.get("sources").and_then(|v| v.as_array()) {
        let mut sources = Vec::new();
        let mut seen_ids = HashSet::new();
        for (idx, item) in arr.iter().enumerate() {
            let Some(rec) = item.as_object() else {
                continue;
            };
            let url = rec
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if url.is_empty() {
                continue;
            }
            let stype = match rec.get("type").and_then(|v| v.as_str()) {
                Some("wordpress") => ScrapeSourceType::Wordpress,
                _ => ScrapeSourceType::Rss,
            };
            let mut id = rec
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                id = format!("src-{}", idx + 1);
            }
            if seen_ids.contains(&id) {
                id = format!("{id}-{}", seen_ids.len());
            }
            seen_ids.insert(id.clone());

            let host_fallback = Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| {
                    if stype == ScrapeSourceType::Wordpress {
                        "WordPress 站点".to_string()
                    } else {
                        "RSS 订阅源".to_string()
                    }
                });
            let name_raw = rec
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let name: String = if name_raw.is_empty() {
                host_fallback
            } else {
                name_raw.chars().take(60).collect()
            };
            let enabled = rec.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let referer = rec
                .get("referer")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            sources.push(ScrapeDataSource {
                id,
                name,
                source_type: stype,
                url,
                enabled,
                referer: Some(referer),
            });
            if sources.len() >= 20 {
                break;
            }
        }
        if !sources.is_empty() {
            cfg.sources = sources;
        }
    } else if obj.get("rssUrl").and_then(|v| v.as_str()).is_some()
        || obj.get("referer").and_then(|v| v.as_str()).is_some()
    {
        let def_rss = default_rss_url();
        let legacy_rss = obj
            .get("rssUrl")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| def_rss.clone());
        let legacy_ref = obj
            .get("referer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let mut srcs = vec![ScrapeDataSource {
            id: "rss-default".to_string(),
            name: if legacy_rss == def_rss {
                default_rss_name()
            } else {
                "自定义 RSS".to_string()
            },
            source_type: ScrapeSourceType::Rss,
            url: legacy_rss.clone(),
            enabled: true,
            referer: Some(legacy_ref),
        }];
        let wp_host = default_wp_host();
        if wp_host.is_empty() || !legacy_rss.contains(&wp_host) {
            srcs.push(default_scrape_sources().remove(1));
        }
        cfg.sources = srcs;
    }

    if let Some(fp) = obj.get("feedPreset").and_then(|v| v.as_str()) {
        if !fp.trim().is_empty() {
            cfg.feed_preset = fp.trim().to_string();
        }
    }
    let primary_rss = cfg
        .sources
        .iter()
        .find(|s| s.source_type == ScrapeSourceType::Rss && s.enabled)
        .or_else(|| {
            cfg.sources
                .iter()
                .find(|s| s.source_type == ScrapeSourceType::Rss)
        })
        .map(|s| s.url.clone());
    cfg.rss_url = obj
        .get("rssUrl")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(primary_rss)
        .unwrap_or_else(default_rss_url);
    if let Some(r) = obj.get("referer").and_then(|v| v.as_str()) {
        cfg.referer = r.trim().to_string();
    }
    if let Some(m) = obj.get("mode").and_then(|v| v.as_str()) {
        cfg.mode = if m == "organize" {
            ScrapeMode::Organize
        } else {
            ScrapeMode::InPlace
        };
    }
    if let Some(o) = obj.get("outputDir").and_then(|v| v.as_str()) {
        cfg.output_dir = o.trim().to_string();
    }
    if let Some(t) = obj.get("transferMode").and_then(|v| v.as_str()) {
        cfg.transfer_mode = match t {
            "hardlink" => ScrapeTransferMode::Hardlink,
            "copy" => ScrapeTransferMode::Copy,
            _ => ScrapeTransferMode::Move,
        };
    }
    if let Some(ft) = obj.get("folderTemplate").and_then(|v| v.as_str()) {
        cfg.folder_template = if ft == "title" {
            ScrapeFolderTemplate::Title
        } else {
            ScrapeFolderTemplate::ActorTitle
        };
    }
    if let Some(pc) = obj.get("posterCrop").and_then(|v| v.as_str()) {
        cfg.poster_crop = match pc {
            "right" => ScrapePosterCrop::Right,
            "none" => ScrapePosterCrop::None,
            _ => ScrapePosterCrop::Center,
        };
    }
    if let Some(ms) = obj.get("minScore").and_then(|v| v.as_u64()) {
        cfg.min_score = (ms as u32).clamp(0, 100);
    }
    if let Some(b) = obj.get("recursive").and_then(|v| v.as_bool()) {
        cfg.recursive = b;
    }
    if let Some(b) = obj.get("skipExistingNfo").and_then(|v| v.as_bool()) {
        cfg.skip_existing_nfo = b;
    }
    if let Some(b) = obj.get("downloadImages").and_then(|v| v.as_bool()) {
        cfg.download_images = b;
    }
    if let Some(b) = obj.get("overwriteNfo").and_then(|v| v.as_bool()) {
        cfg.overwrite_nfo = b;
    }
    if let Some(c) = obj.get("concurrency").and_then(|v| v.as_u64()) {
        cfg.concurrency = (c as usize).clamp(1, 10);
    }
    cfg
}

pub fn normalize_rss_source_input(
    input: &RssSourceInput,
    existing: Option<&RssSource>,
    default_pos: i64,
) -> CoreResult<RssSource> {
    let url = normalize_http_href(Some(&input.url), "订阅地址", true)?;
    let referer = match input.referer.as_deref() {
        Some(r) => normalize_http_href(Some(r), "Referer", false)?,
        None => existing.map(|e| e.referer.clone()).unwrap_or_default(),
    };
    let mode = input
        .mode
        .or_else(|| existing.map(|e| e.mode))
        .unwrap_or(RssSourceMode::Rss);
    let interval_minutes = input
        .interval_minutes
        .or_else(|| existing.map(|e| e.interval_minutes))
        .unwrap_or(30)
        .clamp(1, 1440);
    let max_per_fetch = input
        .max_per_fetch
        .or_else(|| existing.map(|e| e.max_per_fetch))
        .unwrap_or(20)
        .clamp(1, 100);
    let size_min_bytes = input
        .size_min_bytes
        .or_else(|| existing.map(|e| e.size_min_bytes))
        .unwrap_or(0);
    let size_max_bytes = input
        .size_max_bytes
        .or_else(|| existing.map(|e| e.size_max_bytes))
        .unwrap_or(0);
    if size_min_bytes > 0 && size_max_bytes > 0 && size_min_bytes > size_max_bytes {
        return Err(CoreError::msg("体积下限不能大于体积上限"));
    }

    let scrape_mode = input
        .scrape_mode
        .or_else(|| existing.map(|e| e.scrape_mode))
        .unwrap_or(ScrapeMode::InPlace);
    let scrape_output_dir = match input.scrape_output_dir.as_deref() {
        Some(s) => s.trim().to_string(),
        None => existing
            .map(|e| e.scrape_output_dir.clone())
            .unwrap_or_default(),
    };
    let auto_scrape = input
        .auto_scrape
        .or_else(|| existing.map(|e| e.auto_scrape))
        .unwrap_or(true);
    if auto_scrape && scrape_mode == ScrapeMode::Organize && scrape_output_dir.is_empty() {
        return Err(CoreError::msg("启用归档整理模式时请填写刮削输出目录"));
    }

    let scrape_transfer_mode = input
        .scrape_transfer_mode
        .or_else(|| existing.map(|e| e.scrape_transfer_mode))
        .unwrap_or(ScrapeTransferMode::Move);
    let scrape_folder_template = input
        .scrape_folder_template
        .or_else(|| existing.map(|e| e.scrape_folder_template))
        .unwrap_or(ScrapeFolderTemplate::ActorTitle);
    let scrape_poster_crop = input
        .scrape_poster_crop
        .or_else(|| existing.map(|e| e.scrape_poster_crop))
        .unwrap_or(ScrapePosterCrop::Center);

    let fallback_host = Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "RSS 订阅".to_string());
    let raw_name = match input.name.as_deref() {
        Some(n) => n.trim().to_string(),
        None => existing.map(|e| e.name.clone()).unwrap_or_default(),
    };
    let name: String = if raw_name.is_empty() {
        fallback_host
    } else {
        raw_name.chars().take(80).collect()
    };

    let id = existing
        .map(|e| e.id.clone())
        .or_else(|| {
            input
                .id
                .as_deref()
                .map(sanitize_source_id)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| format!("rss-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));

    let trim_limit = |opt: Option<&str>, ex: Option<&str>, cap: usize| -> String {
        let val = opt.unwrap_or_else(|| ex.unwrap_or("")).trim();
        val.chars().take(cap).collect()
    };

    Ok(RssSource {
        id,
        name,
        mode,
        url,
        enabled: input
            .enabled
            .or_else(|| existing.map(|e| e.enabled))
            .unwrap_or(true),
        auto_download: input
            .auto_download
            .or_else(|| existing.map(|e| e.auto_download))
            .unwrap_or(false),
        interval_minutes,
        max_per_fetch,
        item_pattern: trim_limit(
            input.item_pattern.as_deref(),
            existing.map(|e| e.item_pattern.as_str()),
            200,
        ),
        include_pattern: trim_limit(
            input.include_pattern.as_deref(),
            existing.map(|e| e.include_pattern.as_str()),
            500,
        ),
        exclude_pattern: trim_limit(
            input.exclude_pattern.as_deref(),
            existing.map(|e| e.exclude_pattern.as_str()),
            500,
        ),
        use_regex: input
            .use_regex
            .or_else(|| existing.map(|e| e.use_regex))
            .unwrap_or(false),
        smart_episode: input
            .smart_episode
            .or_else(|| existing.map(|e| e.smart_episode))
            .unwrap_or(false),
        size_min_bytes,
        size_max_bytes,
        save_dir: trim_limit(
            input.save_dir.as_deref(),
            existing.map(|e| e.save_dir.as_str()),
            300,
        ),
        referer,
        cookies: trim_limit(
            input.cookies.as_deref(),
            existing.map(|e| e.cookies.as_str()),
            4000,
        ),
        user_agent: trim_limit(
            input.user_agent.as_deref(),
            existing.map(|e| e.user_agent.as_str()),
            400,
        ),
        auto_scrape,
        scrape_mode,
        scrape_output_dir: scrape_output_dir.chars().take(300).collect(),
        scrape_transfer_mode,
        scrape_folder_template,
        scrape_poster_crop,
        scrape_download_images: input
            .scrape_download_images
            .or_else(|| existing.map(|e| e.scrape_download_images))
            .unwrap_or(true),
        last_fetch_at: existing.map(|e| e.last_fetch_at).unwrap_or(0),
        last_success_at: existing.map(|e| e.last_success_at).unwrap_or(0),
        last_error: existing.map(|e| e.last_error.clone()).unwrap_or_default(),
        fail_count: existing.map(|e| e.fail_count).unwrap_or(0),
        seeded: existing.map(|e| e.seeded).unwrap_or(false),
        position: input
            .position
            .or_else(|| existing.map(|e| e.position))
            .unwrap_or(default_pos)
            .clamp(0, 10_000),
        total_count: existing.map(|e| e.total_count).unwrap_or(0),
        unread_count: existing.map(|e| e.unread_count).unwrap_or(0),
    })
}

fn rss_status_str(st: RssItemStatus) -> &'static str {
    match st {
        RssItemStatus::New => "new",
        RssItemStatus::Downloaded => "downloaded",
        RssItemStatus::Ignored => "ignored",
        RssItemStatus::Filtered => "filtered",
        RssItemStatus::DuplicateEpisode => "duplicate-episode",
        RssItemStatus::SeedSkipped => "seed-skipped",
    }
}

fn task_status_str(st: TaskStatus) -> &'static str {
    match st {
        TaskStatus::Pause => "pause",
        TaskStatus::Resume => "resume",
        TaskStatus::Done => "done",
        TaskStatus::Pending => "pending",
        TaskStatus::Error => "error",
    }
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    cache_dir: PathBuf,
}

impl Store {
    pub fn open(cache_dir: &Path, env_defaults: &EnvConfigDefaults) -> CoreResult<Self> {
        std::fs::create_dir_all(cache_dir)?;
        let db_path = cache_dir.join("streamdock.db");
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS kv_state (
                key TEXT PRIMARY KEY,
                value_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
                url TEXT PRIMARY KEY,
                position INTEGER NOT NULL,
                status TEXT NOT NULL,
                start_time INTEGER NOT NULL,
                payload_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS rss_sources (
                id TEXT PRIMARY KEY,
                position INTEGER NOT NULL,
                enabled INTEGER NOT NULL,
                payload_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS rss_items (
                source_id TEXT NOT NULL,
                guid TEXT NOT NULL,
                title TEXT NOT NULL,
                link TEXT NOT NULL,
                tags_text TEXT NOT NULL,
                episode_key TEXT NOT NULL,
                search_text TEXT NOT NULL,
                status TEXT NOT NULL,
                pub_date INTEGER NOT NULL,
                fetched_at INTEGER NOT NULL,
                position INTEGER NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY (source_id, guid),
                FOREIGN KEY (source_id) REFERENCES rss_sources(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_rss_items_query
                ON rss_items(source_id, status, pub_date DESC, fetched_at DESC, position ASC);
            ",
        )?;

        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            cache_dir: cache_dir.to_path_buf(),
        };
        store.migrate_legacy_if_needed(env_defaults)?;
        Ok(store)
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    fn migrate_legacy_if_needed(&self, env_defaults: &EnvConfigDefaults) -> CoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
        let migrated: Option<String> = conn
            .query_row(
                "SELECT value_json FROM kv_state WHERE key = 'legacy_migrated'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if migrated.is_some() {
            return Ok(());
        }

        let now = chrono::Utc::now().timestamp_millis();
        let tx = conn.transaction()?;

        // 1. config.json (+ extract legacy rssBridgeFeeds / rssBridgeFeedUrl)
        let mut legacy_feeds: Vec<LegacyFeed> = Vec::new();
        let mut effective_save_dir = env_defaults.save_dir.clone();
        if let Ok(raw) = std::fs::read_to_string(self.cache_dir.join("config.json")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(obj) = val.as_object() {
                    legacy_feeds = extract_legacy_feeds(obj);
                    let mut cfg = default_server_config(env_defaults);
                    merge_legacy_server_config(&mut cfg, obj);
                    effective_save_dir = cfg.save_dir.clone();
                    let json = serde_json::to_string(&cfg)?;
                    tx.execute(
                        "INSERT OR REPLACE INTO kv_state(key, value_json, updated_at) VALUES ('server_config', ?1, ?2)",
                        params![json, now],
                    )?;
                }
            }
        }

        // 2. scrape-config.json
        if let Ok(raw) = std::fs::read_to_string(self.cache_dir.join("scrape-config.json")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                let sc = normalize_scrape_config_value(&val, &effective_save_dir);
                let json = serde_json::to_string(&sc)?;
                tx.execute(
                    "INSERT OR REPLACE INTO kv_state(key, value_json, updated_at) VALUES ('scrape_config', ?1, ?2)",
                    params![json, now],
                )?;
            }
        }

        // 3. scrape-job.json
        if let Ok(raw) = std::fs::read_to_string(self.cache_dir.join("scrape-job.json")) {
            if let Ok(mut job) = serde_json::from_str::<PersistedScrapeJob>(&raw) {
                if job.snapshot.status == ScrapeJobStatus::Running {
                    job.snapshot.status = ScrapeJobStatus::Stopped;
                    job.snapshot.current_file = None;
                    if job.snapshot.finished_at.is_none() {
                        job.snapshot.finished_at = Some(now);
                    }
                }
                let json = serde_json::to_string(&job)?;
                tx.execute(
                    "INSERT OR REPLACE INTO kv_state(key, value_json, updated_at) VALUES ('scrape_job', ?1, ?2)",
                    params![json, now],
                )?;
            }
        }

        // 4. cache.json (tasks)
        if let Ok(raw) = std::fs::read_to_string(self.cache_dir.join("cache.json")) {
            if let Ok(entries) = serde_json::from_str::<Vec<(String, TaskItem)>>(&raw) {
                let mut stmt = tx.prepare(
                    "INSERT OR REPLACE INTO tasks(url, position, status, start_time, payload_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for (pos, (url, mut item)) in entries.into_iter().enumerate() {
                    if item.url.is_empty() {
                        item.url = url.clone();
                    }
                    if matches!(item.status, TaskStatus::Resume | TaskStatus::Pending) {
                        item.status = TaskStatus::Pause;
                    }
                    if item.options.is_none() && item.dl_options.is_some() {
                        item.options = item.dl_options.take();
                    }
                    if let Some(ref mut opts) = item.options {
                        opts.retry_failed_only = None;
                        if item.scrape_meta.is_none() {
                            item.scrape_meta = opts.scrape_meta.clone();
                        }
                    }
                    if item.scrape_status == Some(TaskScrapeStatus::Scraping) {
                        item.scrape_status = Some(TaskScrapeStatus::Idle);
                    }
                    let st = task_status_str(item.status);
                    let start_time = item.stats.start_time.unwrap_or(0);
                    let payload = serde_json::to_string(&item)?;
                    stmt.execute(params![item.url, pos as i64, st, start_time, payload])?;
                }
            }
        }

        // 5. rss-sources.json + rss-items/*.json (or legacy feeds)
        let rss_sources_path = self.cache_dir.join("rss-sources.json");
        let rss_existed = rss_sources_path.exists();
        if let Ok(raw) = std::fs::read_to_string(&rss_sources_path) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(arr) = parsed.get("sources").and_then(|v| v.as_array()) {
                    for (idx, src_val) in arr.iter().enumerate() {
                        let parsed_src = serde_json::from_value::<RssSource>(src_val.clone())
                            .ok()
                            .or_else(|| {
                                let inp = serde_json::from_value::<RssSourceInput>(src_val.clone())
                                    .ok()?;
                                let mut s =
                                    normalize_rss_source_input(&inp, None, idx as i64).ok()?;
                                if let Some(obj) = src_val.as_object() {
                                    if let Some(v) = obj.get("lastFetchAt").and_then(|v| v.as_i64())
                                    {
                                        s.last_fetch_at = v;
                                    }
                                    if let Some(v) =
                                        obj.get("lastSuccessAt").and_then(|v| v.as_i64())
                                    {
                                        s.last_success_at = v;
                                    }
                                    if let Some(v) = obj.get("lastError").and_then(|v| v.as_str()) {
                                        s.last_error = v.to_string();
                                    }
                                    if let Some(v) = obj.get("failCount").and_then(|v| v.as_u64()) {
                                        s.fail_count = v as u32;
                                    }
                                    if let Some(v) = obj.get("seeded").and_then(|v| v.as_bool()) {
                                        s.seeded = v;
                                    }
                                }
                                Some(s)
                            });
                        if let Some(mut src) = parsed_src {
                            if src.id.is_empty() || src.url.is_empty() {
                                continue;
                            }
                            let item_file =
                                self.cache_dir.join("rss-items").join(format!("{}.json", {
                                    let s = sanitize_source_id(&src.id);
                                    if s.is_empty() {
                                        "default".to_string()
                                    } else {
                                        s
                                    }
                                }));
                            let mut loaded_items: Vec<RssFeedItem> = Vec::new();
                            if let Ok(iraw) = std::fs::read_to_string(&item_file) {
                                if let Ok(ival) = serde_json::from_str::<serde_json::Value>(&iraw) {
                                    if let Some(iarr) = ival.get("items").and_then(|v| v.as_array())
                                    {
                                        for iv in iarr {
                                            if let Ok(it) =
                                                serde_json::from_value::<RssFeedItem>(iv.clone())
                                            {
                                                loaded_items.push(it);
                                            }
                                        }
                                    }
                                }
                            }
                            src.total_count = loaded_items.len();
                            src.unread_count = loaded_items
                                .iter()
                                .filter(|i| i.status == RssItemStatus::New)
                                .count();
                            let pos = if src.position == 0 && idx > 0 {
                                idx as i64
                            } else {
                                src.position
                            };
                            let src_json = serde_json::to_string(&src)?;
                            tx.execute(
                                "INSERT INTO rss_sources(id, position, enabled, payload_json) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(id) DO UPDATE SET position = excluded.position, enabled = excluded.enabled, payload_json = excluded.payload_json",
                                params![src.id, pos, if src.enabled { 1 } else { 0 }, src_json],
                            )?;
                            let mut istmt = tx.prepare(
                                "INSERT OR REPLACE INTO rss_items(source_id, guid, title, link, tags_text, episode_key, search_text, status, pub_date, fetched_at, position, payload_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                            )?;
                            for (ipos, mut it) in loaded_items.into_iter().enumerate() {
                                it.source_id = src.id.clone();
                                let tags_text = it.tags.join(" ");
                                let search_text = format!(
                                    "{} {} {} {}",
                                    it.title, it.link, tags_text, it.episode_key
                                )
                                .to_lowercase();
                                let pjson = serde_json::to_string(&it)?;
                                istmt.execute(params![
                                    it.source_id,
                                    it.guid,
                                    it.title,
                                    it.link,
                                    tags_text,
                                    it.episode_key,
                                    search_text,
                                    rss_status_str(it.status),
                                    it.pub_date,
                                    it.fetched_at,
                                    ipos as i64,
                                    pjson
                                ])?;
                            }
                        }
                    }
                }
            }
        } else if !rss_existed && !legacy_feeds.is_empty() {
            for (idx, lf) in legacy_feeds.into_iter().enumerate() {
                let input = RssSourceInput {
                    id: Some(sanitize_source_id(&lf.id)).filter(|s| !s.is_empty()),
                    name: Some(lf.name.unwrap_or_else(|| lf.id.clone())),
                    mode: Some(lf.mode),
                    url: lf.url,
                    enabled: Some(lf.enabled),
                    auto_download: Some(false),
                    auto_scrape: Some(true),
                    item_pattern: lf.item_pattern,
                    referer: lf.referer,
                    ..Default::default()
                };
                if let Ok(src) = normalize_rss_source_input(&input, None, idx as i64) {
                    let src_json = serde_json::to_string(&src)?;
                    tx.execute(
                        "INSERT OR IGNORE INTO rss_sources(id, position, enabled, payload_json) VALUES (?1, ?2, ?3, ?4)",
                        params![src.id, src.position, if src.enabled { 1 } else { 0 }, src_json],
                    )?;
                }
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO kv_state(key, value_json, updated_at) VALUES ('legacy_migrated', '1', ?1)",
            params![now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_server_config(&self, env_defaults: &EnvConfigDefaults) -> CoreResult<ServerConfig> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT value_json FROM kv_state WHERE key = 'server_config'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(json) = raw {
            if let Ok(cfg) = serde_json::from_str::<ServerConfig>(&json) {
                return Ok(cfg);
            }
        }
        Ok(default_server_config(env_defaults))
    }

    pub fn save_server_config(&self, cfg: &ServerConfig) -> CoreResult<()> {
        let json = serde_json::to_string(cfg)?;
        let now = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_state(key, value_json, updated_at) VALUES ('server_config', ?1, ?2)",
            params![json, now],
        )?;
        Ok(())
    }

    pub fn load_scrape_config(&self, fallback_save_dir: &str) -> CoreResult<ScrapeConfig> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT value_json FROM kv_state WHERE key = 'scrape_config'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(json) = raw {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json) {
                return Ok(normalize_scrape_config_value(&val, fallback_save_dir));
            }
        }
        Ok(default_scrape_config(fallback_save_dir))
    }

    pub fn save_scrape_config(&self, cfg: &ScrapeConfig) -> CoreResult<()> {
        let json = serde_json::to_string(cfg)?;
        let now = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_state(key, value_json, updated_at) VALUES ('scrape_config', ?1, ?2)",
            params![json, now],
        )?;
        Ok(())
    }

    pub fn load_scrape_job(&self) -> CoreResult<Option<PersistedScrapeJob>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT value_json FROM kv_state WHERE key = 'scrape_job'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(json) = raw {
            if let Ok(mut job) = serde_json::from_str::<PersistedScrapeJob>(&json) {
                if job.snapshot.status == ScrapeJobStatus::Running {
                    job.snapshot.status = ScrapeJobStatus::Stopped;
                    job.snapshot.current_file = None;
                    if job.snapshot.finished_at.is_none() {
                        job.snapshot.finished_at = Some(chrono::Utc::now().timestamp_millis());
                    }
                }
                return Ok(Some(job));
            }
        }
        Ok(None)
    }

    pub fn save_scrape_job(&self, job: Option<&PersistedScrapeJob>) -> CoreResult<()> {
        let conn = self.conn.lock().unwrap();
        match job {
            Some(j) => {
                let json = serde_json::to_string(j)?;
                let now = chrono::Utc::now().timestamp_millis();
                conn.execute(
                    "INSERT OR REPLACE INTO kv_state(key, value_json, updated_at) VALUES ('scrape_job', ?1, ?2)",
                    params![json, now],
                )?;
            }
            None => {
                conn.execute("DELETE FROM kv_state WHERE key = 'scrape_job'", [])?;
            }
        }
        Ok(())
    }

    pub fn load_tasks_ordered(&self) -> CoreResult<Vec<TaskItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT payload_json FROM tasks ORDER BY position ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row?;
            if let Ok(mut item) = serde_json::from_str::<TaskItem>(&json) {
                if matches!(item.status, TaskStatus::Resume | TaskStatus::Pending) {
                    item.status = TaskStatus::Pause;
                }
                if item.options.is_none() && item.dl_options.is_some() {
                    item.options = item.dl_options.take();
                }
                if let Some(ref mut opts) = item.options {
                    opts.retry_failed_only = None;
                }
                if item.scrape_status == Some(TaskScrapeStatus::Scraping) {
                    item.scrape_status = Some(TaskScrapeStatus::Idle);
                }
                out.push(item);
            }
        }
        Ok(out)
    }

    pub fn replace_all_tasks(&self, ordered_tasks: &[TaskItem]) -> CoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM tasks", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tasks(url, position, status, start_time, payload_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (pos, item) in ordered_tasks.iter().enumerate() {
                let st = task_status_str(item.status);
                let start_time = item.stats.start_time.unwrap_or(0);
                let json = serde_json::to_string(item)?;
                stmt.execute(params![item.url, pos as i64, st, start_time, json])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_rss_sources(&self) -> CoreResult<Vec<RssSource>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT payload_json FROM rss_sources ORDER BY position ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(src) = serde_json::from_str::<RssSource>(&row?) {
                out.push(src);
            }
        }
        Ok(out)
    }

    pub fn save_rss_source(&self, src: &RssSource) -> CoreResult<()> {
        let json = serde_json::to_string(src)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO rss_sources(id, position, enabled, payload_json) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(id) DO UPDATE SET position = excluded.position, enabled = excluded.enabled, payload_json = excluded.payload_json",
            params![src.id, src.position, if src.enabled { 1 } else { 0 }, json],
        )?;
        Ok(())
    }

    pub fn delete_rss_source(&self, id: &str) -> CoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM rss_sources WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn load_rss_items(&self, source_id: &str) -> CoreResult<Vec<RssFeedItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM rss_items WHERE source_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![source_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(item) = serde_json::from_str::<RssFeedItem>(&row?) {
                out.push(item);
            }
        }
        Ok(out)
    }

    pub fn replace_rss_items(&self, source_id: &str, items: &[RssFeedItem]) -> CoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM rss_items WHERE source_id = ?1",
            params![source_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO rss_items(source_id, guid, title, link, tags_text, episode_key, search_text, status, pub_date, fetched_at, position, payload_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for (pos, it) in items.iter().enumerate() {
                let tags_text = it.tags.join(" ");
                let search_text =
                    format!("{} {} {} {}", it.title, it.link, tags_text, it.episode_key)
                        .to_lowercase();
                let json = serde_json::to_string(it)?;
                stmt.execute(params![
                    source_id,
                    it.guid,
                    it.title,
                    it.link,
                    tags_text,
                    it.episode_key,
                    search_text,
                    rss_status_str(it.status),
                    it.pub_date,
                    it.fetched_at,
                    pos as i64,
                    json
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn compute_rss_counts(&self, source_id: &str) -> CoreResult<RssItemCounts> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM rss_items WHERE source_id = ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![source_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut counts = RssItemCounts::default();
        for row in rows {
            let (st, cnt) = row?;
            let n = cnt.max(0) as usize;
            counts.all += n;
            match st.as_str() {
                "new" => counts.new += n,
                "downloaded" => counts.downloaded += n,
                "ignored" => counts.ignored += n,
                "filtered" => counts.filtered += n,
                "duplicate-episode" => counts.duplicate_episode += n,
                "seed-skipped" => counts.seed_skipped += n,
                _ => {}
            }
        }
        Ok(counts)
    }

    pub fn query_rss_items(
        &self,
        source_id: &str,
        status_filter: &str,
        q: &str,
        sort_oldest: bool,
        page: usize,
        page_size: usize,
    ) -> CoreResult<RssItemsPage> {
        let counts = self.compute_rss_counts(source_id)?;
        let conn = self.conn.lock().unwrap();

        let mut where_sql = String::from("source_id = ?1");
        let mut params_vec: Vec<rusqlite::types::Value> =
            vec![rusqlite::types::Value::Text(source_id.to_string())];

        let st_trim = status_filter.trim();
        if !st_trim.is_empty() && st_trim != "all" {
            if st_trim == "filtered" {
                where_sql.push_str(" AND status IN ('filtered', 'duplicate-episode')");
            } else {
                params_vec.push(rusqlite::types::Value::Text(st_trim.to_string()));
                where_sql.push_str(&format!(" AND status = ?{}", params_vec.len()));
            }
        }

        let q_lower = q.trim().to_lowercase();
        if !q_lower.is_empty() {
            params_vec.push(rusqlite::types::Value::Text(q_lower));
            where_sql.push_str(&format!(
                " AND instr(search_text, ?{}) > 0",
                params_vec.len()
            ));
        }

        let count_sql = format!("SELECT COUNT(*) FROM rss_items WHERE {where_sql}");
        let total: usize = conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params_vec.iter()),
            |r| r.get::<_, i64>(0),
        )? as usize;

        let clamped_size = page_size.clamp(1, 200);
        let total_pages = (total.max(1) + clamped_size - 1) / clamped_size;
        let clamped_page = page.clamp(1, total_pages.max(1));
        let offset = (clamped_page - 1) * clamped_size;

        let order_sql = if sort_oldest {
            "CASE WHEN pub_date > 0 THEN pub_date ELSE fetched_at END ASC, position DESC"
        } else {
            "CASE WHEN pub_date > 0 THEN pub_date ELSE fetched_at END DESC, position ASC"
        };

        let mut query_params = params_vec;
        query_params.push(rusqlite::types::Value::Integer(clamped_size as i64));
        let limit_idx = query_params.len();
        query_params.push(rusqlite::types::Value::Integer(offset as i64));
        let offset_idx = query_params.len();

        let data_sql = format!(
            "SELECT payload_json FROM rss_items WHERE {where_sql} ORDER BY {order_sql} LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
        );
        let mut stmt = conn.prepare(&data_sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(query_params.iter()), |r| {
            r.get::<_, String>(0)
        })?;
        let mut items = Vec::new();
        for row in rows {
            if let Ok(it) = serde_json::from_str::<RssFeedItem>(&row?) {
                items.push(it);
            }
        }

        Ok(RssItemsPage {
            source_id: source_id.to_string(),
            total,
            page: clamped_page,
            page_size: clamped_size,
            counts,
            items,
        })
    }
}

struct LegacyFeed {
    id: String,
    name: Option<String>,
    enabled: bool,
    mode: RssSourceMode,
    url: String,
    item_pattern: Option<String>,
    referer: Option<String>,
}

fn extract_legacy_feeds(obj: &serde_json::Map<String, serde_json::Value>) -> Vec<LegacyFeed> {
    if let Some(arr) = obj.get("rssBridgeFeeds").and_then(|v| v.as_array()) {
        let mut out = Vec::new();
        for item in arr {
            let Some(m) = item.as_object() else {
                continue;
            };
            let url = m
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if url.is_empty() {
                continue;
            }
            let id = m
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            out.push(LegacyFeed {
                id,
                name: m.get("name").and_then(|v| v.as_str()).map(str::to_string),
                enabled: m.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                mode: if m.get("mode").and_then(|v| v.as_str()) == Some("html") {
                    RssSourceMode::Html
                } else {
                    RssSourceMode::Rss
                },
                url,
                item_pattern: m
                    .get("itemPattern")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string()),
                referer: m
                    .get("referer")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string()),
            });
        }
        return out;
    }

    if let Some(single_url) = obj.get("rssBridgeFeedUrl").and_then(|v| v.as_str()) {
        if !single_url.trim().is_empty() {
            return vec![LegacyFeed {
                id: "default".to_string(),
                name: Some("默认订阅源".to_string()),
                enabled: obj
                    .get("rssBridgeEnabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                mode: RssSourceMode::Rss,
                url: single_url.trim().to_string(),
                item_pattern: None,
                referer: obj
                    .get("rssBridgeReferer")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string()),
            }];
        }
    }

    Vec::new()
}

fn merge_legacy_server_config(
    cfg: &mut ServerConfig,
    root: &serde_json::Map<String, serde_json::Value>,
) {
    let mut flat = serde_json::Map::new();
    if let Some(dl) = root.get("dlOptions").and_then(|v| v.as_object()) {
        for (k, v) in dl {
            flat.insert(k.clone(), v.clone());
        }
    } else {
        for (k, v) in root {
            flat.insert(k.clone(), v.clone());
        }
    }
    if let Some(web) = root.get("webOptions").and_then(|v| v.as_object()) {
        for (k, v) in web {
            flat.insert(k.clone(), v.clone());
        }
    }

    if let Some(n) = flat.get("threadNum").and_then(|v| v.as_u64()) {
        cfg.thread_num = (n as usize).clamp(1, 16);
    }
    if let Some(s) = flat.get("saveDir").and_then(|v| v.as_str()) {
        if !s.trim().is_empty() {
            cfg.save_dir = s.to_string();
        }
    }
    if let Some(b) = flat.get("delCache").and_then(|v| v.as_bool()) {
        cfg.del_cache = b;
    }
    if let Some(n) = flat.get("maxDownloads").and_then(|v| v.as_u64()) {
        cfg.max_downloads = (n as usize).clamp(1, 5);
    }
    if let Some(s) = flat.get("ffmpegPath").and_then(|v| v.as_str()) {
        cfg.ffmpeg_path = s.to_string();
    }
    if let Some(h) = flat.get("headers") {
        match h {
            serde_json::Value::String(s) => {
                if format_headers_str(s).is_ok() {
                    cfg.headers = s.clone();
                }
            }
            serde_json::Value::Object(_) => {
                cfg.headers = h.to_string();
            }
            _ => {}
        }
    }
    if let Some(n) = flat.get("mergeTimeout").and_then(|v| v.as_u64()) {
        if (15..=3600).contains(&n) {
            cfg.merge_timeout = n;
        }
    }
    if let Some(pm) = flat.get("proxyMode").and_then(|v| v.as_str()) {
        cfg.proxy_mode = match pm {
            "custom" => ProxyMode::Custom,
            "disabled" => ProxyMode::Disabled,
            _ => ProxyMode::System,
        };
    }
    if let Some(s) = flat.get("proxyUrl").and_then(|v| v.as_str()) {
        cfg.proxy_url = s.to_string();
    }
    if let Some(s) = flat.get("noProxy").and_then(|v| v.as_str()) {
        cfg.no_proxy = s.to_string();
    }
}

pub fn validate_server_config_update(
    current: &ServerConfig,
    patch: &serde_json::Value,
) -> CoreResult<ServerConfig> {
    let Some(obj) = patch.as_object() else {
        return Err(CoreError::msg("无效的配置格式"));
    };
    let mut next = current.clone();
    if let Some(v) = obj.get("threadNum") {
        next.thread_num = v
            .as_u64()
            .filter(|&n| (1..=16).contains(&n))
            .map(|n| n as usize)
            .ok_or_else(|| CoreError::msg("并发分片为 1–16，任务为 1–5，合并超时为 15–3600 秒"))?;
    }
    if let Some(v) = obj.get("maxDownloads") {
        next.max_downloads = v
            .as_u64()
            .filter(|&n| (1..=5).contains(&n))
            .map(|n| n as usize)
            .ok_or_else(|| CoreError::msg("并发分片为 1–16，任务为 1–5，合并超时为 15–3600 秒"))?;
    }
    if let Some(v) = obj.get("mergeTimeout") {
        next.merge_timeout = v
            .as_u64()
            .filter(|&n| (15..=3600).contains(&n))
            .ok_or_else(|| CoreError::msg("并发分片为 1–16，任务为 1–5，合并超时为 15–3600 秒"))?;
    }
    if let Some(v) = obj.get("saveDir") {
        let s = v.as_str().map(str::trim).unwrap_or("");
        if s.is_empty() {
            return Err(CoreError::msg("保存目录不能为空"));
        }
        next.save_dir = s.to_string();
    }
    if let Some(v) = obj.get("delCache") {
        next.del_cache = v.as_bool().unwrap_or(next.del_cache);
    }
    if let Some(v) = obj.get("ffmpegPath") {
        next.ffmpeg_path = v.as_str().unwrap_or("").to_string();
    }
    if let Some(v) = obj.get("headers") {
        match v {
            serde_json::Value::String(s) => {
                format_headers_str(s)?;
                next.headers = s.clone();
            }
            serde_json::Value::Object(_) => {
                crate::http::format_headers_json(v)?;
                next.headers = v.to_string();
            }
            serde_json::Value::Null => next.headers = String::new(),
            _ => return Err(CoreError::msg("请求头必须是对象")),
        }
    }
    if let Some(v) = obj.get("proxyMode") {
        match v.as_str() {
            Some("custom") => next.proxy_mode = ProxyMode::Custom,
            Some("system") => next.proxy_mode = ProxyMode::System,
            Some("disabled") => next.proxy_mode = ProxyMode::Disabled,
            _ => return Err(CoreError::msg("无效的代理模式")),
        }
    }
    if let Some(v) = obj.get("proxyUrl") {
        next.proxy_url = v.as_str().unwrap_or("").to_string();
    }
    if let Some(v) = obj.get("noProxy") {
        next.no_proxy = v.as_str().unwrap_or("").to_string();
    }
    if next.proxy_mode == ProxyMode::Custom {
        let lower = next.proxy_url.to_ascii_lowercase();
        if !lower.starts_with("http://") && !lower.starts_with("https://") {
            return Err(CoreError::msg("代理地址应以 http:// 或 https:// 开头"));
        }
    }
    Ok(next)
}
