use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pause,
    Resume,
    Done,
    Pending,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskPhase {
    Queued,
    Parsing,
    Downloading,
    Merging,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentStatus {
    Pending,
    Downloading,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentProgress {
    pub index: usize,
    pub status: SegmentStatus,
    pub size: u64,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentSnapshot {
    pub fingerprint: String,
    pub items: Vec<SegmentProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadTypeKind {
    M3u8,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputTypeKind {
    Auto,
    M3u8,
    File,
    Parser,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_type: Option<DownloadTypeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<TaskPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_success: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_failed: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_segment_details: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloaded_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_video: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_num: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrapeMode {
    InPlace,
    Organize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrapeTransferMode {
    Move,
    Hardlink,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrapeTransferActual {
    None,
    Rename,
    CopyCrossFs,
    Hardlink,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrapePosterCrop {
    Center,
    Right,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrapeFolderTemplate {
    ActorTitle,
    Title,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskScrapeMeta {
    pub page_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ScrapeMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_mode: Option<ScrapeTransferMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_template: Option<ScrapeFolderTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_crop: Option<ScrapePosterCrop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_nfo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HeaderInput {
    Text(String),
    Map(BTreeMap<String, serde_json::Value>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HlsTokenSession {
    pub origin: String,
    pub player_url: String,
    #[serde(default)]
    pub base_signed_url: String,
    pub session_id: String,
    pub movie_id: String,
    #[serde(default)]
    pub guid_id: String,
    pub api_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_secret_issued_at: Option<serde_json::Value>,
    #[serde(default)]
    pub time_diff_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_default_headers: Option<bool>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub download_type: Option<InputTypeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_segments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_session: Option<HlsTokenSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrape_meta: Option<TaskScrapeMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_failed_only: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskScrapeStatus {
    Idle,
    Scraping,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskItem {
    #[serde(flatten)]
    pub stats: DownloadStats,
    pub url: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_capture: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<TaskOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dl_options: Option<TaskOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrape_meta: Option<TaskScrapeMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrape_status: Option<TaskScrapeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrape_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfo_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueStatus {
    pub queue_length: usize,
    pub active_downloads: Vec<String>,
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    Custom,
    System,
    Disabled,
}

fn deserialize_string_or_object<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<serde_json::Value>::deserialize(deserializer)?;
    match val {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(s)) => Ok(s),
        Some(other) => Ok(other.to_string()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub thread_num: usize,
    pub save_dir: String,
    pub del_cache: bool,
    pub max_downloads: usize,
    pub ffmpeg_path: String,
    #[serde(default, deserialize_with = "deserialize_string_or_object")]
    pub headers: String,
    pub merge_timeout: u64,
    pub proxy_mode: ProxyMode,
    pub proxy_url: String,
    pub no_proxy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse<T> {
    pub code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            code: 0,
            message: None,
            data: Some(data),
        }
    }

    pub fn ok_msg(message: impl Into<String>, data: Option<T>) -> Self {
        Self {
            code: 0,
            message: Some(message.into()),
            data,
        }
    }

    pub fn err(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: Some(message.into()),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoMusicInfo {
    pub author: String,
    pub avatar: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoInfo {
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub like: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub music: Option<VideoMusicInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractedMedia {
    pub url: String,
    pub title: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_source: Option<String>,
    #[serde(rename = "type")]
    pub media_type: DownloadTypeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractionErrorItem {
    pub url: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractionResult {
    pub items: Vec<ExtractedMedia>,
    pub errors: Vec<ExtractionErrorItem>,
    pub pages_visited: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeItemTiming {
    pub detail_ms: u64,
    pub transfer_ms: u64,
    pub artwork_ms: u64,
    pub crop_ms: u64,
    pub total_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_actual: Option<ScrapeTransferActual>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrapeSourceType {
    Rss,
    Wordpress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeDataSource {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub source_type: ScrapeSourceType,
    pub url: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeScanSourceSummary {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub source_type: ScrapeSourceType,
    pub url: String,
    pub catalog_count: usize,
    pub queried_count: usize,
    pub matched_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeCandidate {
    pub guid: String,
    pub title: String,
    pub page_url: String,
    pub pub_date: String,
    pub tags: Vec<String>,
    pub score: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<ScrapeSourceType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrapePreviewStatus {
    Matched,
    LowConfidence,
    Unmatched,
    SkippedNfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapePreviewItem {
    pub file_path: String,
    pub relative_path: String,
    pub filename: String,
    pub size: u64,
    pub has_nfo: bool,
    pub status: ScrapePreviewStatus,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_item: Option<ScrapeCandidate>,
    pub candidates: Vec<ScrapeCandidate>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeScanRequest {
    #[serde(default)]
    pub scan_dir: Option<String>,
    #[serde(default)]
    pub scan_dirs: Option<Vec<String>>,
    #[serde(default)]
    pub sources: Option<Vec<ScrapeDataSource>>,
    #[serde(default)]
    pub rss_url: Option<String>,
    #[serde(default)]
    pub feed_id: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub recursive: Option<bool>,
    #[serde(default)]
    pub skip_existing_nfo: Option<bool>,
    #[serde(default)]
    pub min_score: Option<u32>,
    #[serde(default)]
    pub refresh_rss: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeScanResult {
    pub scan_dir: String,
    pub scan_dirs: Vec<String>,
    pub rss_url: String,
    pub feed_title: String,
    pub catalog_count: usize,
    pub scanned_files: usize,
    pub matched_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_summaries: Option<Vec<ScrapeScanSourceSummary>>,
    pub items: Vec<ScrapePreviewItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeConfig {
    pub scan_dirs: Vec<String>,
    pub sources: Vec<ScrapeDataSource>,
    pub feed_preset: String,
    pub rss_url: String,
    pub referer: String,
    pub mode: ScrapeMode,
    pub output_dir: String,
    pub transfer_mode: ScrapeTransferMode,
    pub folder_template: ScrapeFolderTemplate,
    pub poster_crop: ScrapePosterCrop,
    pub min_score: u32,
    pub recursive: bool,
    pub skip_existing_nfo: bool,
    pub download_images: bool,
    pub overwrite_nfo: bool,
    pub concurrency: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeRunItemInput {
    pub file_path: String,
    pub page_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeRunRequest {
    pub items: Vec<ScrapeRunItemInput>,
    pub mode: ScrapeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_mode: Option<ScrapeTransferMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_template: Option<ScrapeFolderTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_crop: Option<ScrapePosterCrop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_nfo: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrapeOutcomeStatus {
    Success,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeItemOutcome {
    pub file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_video_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfo_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fanart_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub status: ScrapeOutcomeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<ScrapeItemTiming>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeRunResult {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub results: Vec<ScrapeItemOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrapeJobStatus {
    Idle,
    Running,
    Done,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeJobTimingStats {
    pub count: usize,
    pub avg_total_ms: u64,
    pub avg_detail_ms: u64,
    pub avg_transfer_ms: u64,
    pub avg_artwork_ms: u64,
    pub avg_crop_ms: u64,
    pub cross_fs_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeJobSnapshot {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub status: ScrapeJobStatus,
    pub mode: ScrapeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    pub transfer_mode: ScrapeTransferMode,
    pub concurrency: usize,
    pub total: usize,
    pub done: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub remaining: usize,
    pub can_resume: bool,
    pub can_retry_failed: bool,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<ScrapeJobTimingStats>,
    pub results: Vec<ScrapeItemOutcome>,
    pub total_results: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succeeded_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RssSourceMode {
    Rss,
    Html,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RssItemStatus {
    #[default]
    New,
    Downloaded,
    Ignored,
    Filtered,
    DuplicateEpisode,
    SeedSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RssRejectReason {
    #[default]
    #[serde(rename = "")]
    None,
    NotIncluded,
    Excluded,
    TooSmall,
    TooLarge,
    DupEpisode,
    SeedSkipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssSource {
    pub id: String,
    pub name: String,
    pub mode: RssSourceMode,
    pub url: String,
    pub enabled: bool,
    pub auto_download: bool,
    pub interval_minutes: u32,
    pub max_per_fetch: usize,
    pub item_pattern: String,
    pub include_pattern: String,
    pub exclude_pattern: String,
    pub use_regex: bool,
    pub smart_episode: bool,
    pub size_min_bytes: u64,
    pub size_max_bytes: u64,
    pub save_dir: String,
    pub referer: String,
    pub cookies: String,
    pub user_agent: String,
    pub auto_scrape: bool,
    pub scrape_mode: ScrapeMode,
    pub scrape_output_dir: String,
    pub scrape_transfer_mode: ScrapeTransferMode,
    pub scrape_folder_template: ScrapeFolderTemplate,
    pub scrape_poster_crop: ScrapePosterCrop,
    pub scrape_download_images: bool,
    pub last_fetch_at: i64,
    pub last_success_at: i64,
    pub last_error: String,
    pub fail_count: u32,
    pub seeded: bool,
    pub position: i64,
    pub total_count: usize,
    pub unread_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssSourceInput {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mode: Option<RssSourceMode>,
    pub url: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub auto_download: Option<bool>,
    #[serde(default)]
    pub interval_minutes: Option<u32>,
    #[serde(default)]
    pub max_per_fetch: Option<usize>,
    #[serde(default)]
    pub item_pattern: Option<String>,
    #[serde(default)]
    pub include_pattern: Option<String>,
    #[serde(default)]
    pub exclude_pattern: Option<String>,
    #[serde(default)]
    pub use_regex: Option<bool>,
    #[serde(default)]
    pub smart_episode: Option<bool>,
    #[serde(default)]
    pub size_min_bytes: Option<u64>,
    #[serde(default)]
    pub size_max_bytes: Option<u64>,
    #[serde(default)]
    pub save_dir: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub auto_scrape: Option<bool>,
    #[serde(default)]
    pub scrape_mode: Option<ScrapeMode>,
    #[serde(default)]
    pub scrape_output_dir: Option<String>,
    #[serde(default)]
    pub scrape_transfer_mode: Option<ScrapeTransferMode>,
    #[serde(default)]
    pub scrape_folder_template: Option<ScrapeFolderTemplate>,
    #[serde(default)]
    pub scrape_poster_crop: Option<ScrapePosterCrop>,
    #[serde(default)]
    pub scrape_download_images: Option<bool>,
    #[serde(default)]
    pub position: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssFeedItem {
    #[serde(default)]
    pub source_id: String,
    pub guid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub enclosure_url: String,
    #[serde(default)]
    pub enclosure_length: u64,
    #[serde(default)]
    pub pub_date: i64,
    #[serde(default)]
    pub pub_date_text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub fetched_at: i64,
    #[serde(default)]
    pub status: RssItemStatus,
    #[serde(default)]
    pub task_url: String,
    #[serde(default)]
    pub episode_key: String,
    #[serde(default)]
    pub reason: RssRejectReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssValidateRequest {
    pub url: String,
    #[serde(default)]
    pub mode: Option<RssSourceMode>,
    #[serde(default)]
    pub item_pattern: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssValidateResult {
    pub url: String,
    pub mode: RssSourceMode,
    pub feed_title: String,
    pub total_items: usize,
    pub sample_items: Vec<RssFeedItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssItemCounts {
    pub all: usize,
    pub new: usize,
    pub downloaded: usize,
    pub ignored: usize,
    pub filtered: usize,
    pub duplicate_episode: usize,
    pub seed_skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssItemsPage {
    pub source_id: String,
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
    pub counts: RssItemCounts,
    pub items: Vec<RssFeedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssActionErrorItem {
    pub guid: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssActionResult {
    pub queued: usize,
    pub skipped: usize,
    pub updated: usize,
    pub errors: Vec<RssActionErrorItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssRefreshResult {
    pub source: RssSource,
    pub added: usize,
    pub auto_queued: usize,
    pub total_items: usize,
}

// Note: ScrapePreviewItem uses `match` in TS (`match?: ScrapeCandidate`).
// Rust reserves `match`, so let's make sure the JSON key is `"match"`!
// Wait: let's inspect ScrapePreviewItem above: we wrote `pub match_item: Option<ScrapeCandidate>`.
// We MUST add `#[serde(rename = "match")]` on `match_item`! Let's fix that right below or keep a separate patch.

pub type HeaderMap = BTreeMap<String, String>;
pub type ProgressCallback = std::sync::Arc<dyn Fn(DownloadStats) -> bool + Send + Sync>;
pub type MergeProgressCallback = std::sync::Arc<dyn Fn(f64, String) + Send + Sync>;
pub type SegmentIdentityFn = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Clone, Default)]
pub struct DownloadExecOptions {
    pub download_type: Option<InputTypeKind>,
    pub source_url: Option<String>,
    pub header_source_url: Option<String>,
    pub retry_failed_only: bool,
    pub merge_timeout_secs: Option<f64>,
    pub thread_num: Option<usize>,
    pub filename: Option<String>,
    pub save_dir: Option<String>,
    pub cache_dir: Option<String>,
    pub ignore_segments: Option<String>,
    pub del_cache: Option<bool>,
    pub force: bool,
    pub headers: Option<HeaderInput>,
    pub convert: Option<bool>,
    pub ffmpeg_path: Option<String>,
    pub proxy_mode: Option<ProxyMode>,
    pub proxy_url: Option<String>,
    pub no_proxy: Option<String>,
    pub token_session: Option<HlsTokenSession>,
    pub segment_identity: Option<SegmentIdentityFn>,
    pub on_progress: Option<ProgressCallback>,
    pub on_merge_progress: Option<MergeProgressCallback>,
    pub extra_headers: Option<HeaderMap>,
    pub client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, Default)]
pub struct DownloadResult {
    pub url: String,
    pub filepath: Option<String>,
    pub errmsg: Option<String>,
    pub is_exist: bool,
    pub paused: bool,
    pub stats: DownloadStats,
    pub segment_files: Vec<String>,
}

pub type TasksMap = HashMap<String, TaskItem>;
