use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde::ser::SerializeMap;
use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use streamdock_core::contract::{
    DownloadExecOptions, DownloadStats, DownloadTypeKind, HeaderInput, InputTypeKind, QueueStatus,
    RssSource, ScrapeConfig, ScrapeFolderTemplate, ScrapeItemOutcome, ScrapeJobSnapshot,
    ScrapeJobStatus, ScrapeJobTimingStats, ScrapeMode, ScrapeOutcomeStatus, ScrapePosterCrop,
    ScrapeRunItemInput, ScrapeRunRequest, ScrapeTransferActual, ScrapeTransferMode, ServerConfig,
    ServerInfo, TaskItem, TaskOptions, TaskPhase, TaskScrapeStatus, TaskStatus,
};
use streamdock_core::download::download_resource;
use streamdock_core::error::{CoreError, CoreResult};
use streamdock_core::format_options::{absolutize, format_options, path_extname_lower, source_url};
use streamdock_core::http::{
    ProxySettings, format_headers, format_headers_str, global_http_client, init_global_proxy,
    md5_hex, merge_header_maps,
};
use streamdock_core::m3u8_download::stop_download;
use streamdock_core::rss_service::{
    EnqueueOutcome, EnqueueRssTaskInput, RssSubscriptionDeps, RssSubscriptionService,
};
use streamdock_core::scraper::{ScrapeRunOptions, run_media_scrape};
use streamdock_core::store::{
    EnvConfigDefaults, PersistedScrapeJob, ScrapeTimingTotals, Store, normalize_scrape_config_value,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const SCRAPE_JOB_MAX_RESULTS: usize = 200;
const SCRAPE_JOB_MAX_FAILED: usize = 150;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn canonical_path(file_path: &str) -> String {
    let abs = absolutize(Path::new(file_path.trim()));
    let s = abs.to_string_lossy();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s.into_owned()
    }
}

fn path_basename(raw: &str) -> String {
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(raw)
        .to_string()
}

pub struct OrderedTasksOwned(pub Vec<(String, TaskItem)>);

impl Serialize for OrderedTasksOwned {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

#[derive(Serialize)]
struct WsEnvelope<T: Serialize> {
    #[serde(rename = "type")]
    kind: &'static str,
    data: T,
}

struct FinishGuard {
    tx: watch::Sender<bool>,
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(true);
    }
}

#[derive(Clone)]
pub struct RunHandle {
    pub run_id: u64,
    pub cancel: CancellationToken,
    pub done_rx: watch::Receiver<bool>,
}

pub struct ScrapeJobRuntime {
    pub snapshot: ScrapeJobSnapshot,
    pub revision: u64,
    pub request: ScrapeRunRequest,
    pub cancel: Option<CancellationToken>,
    pub done_rx: Option<watch::Receiver<bool>>,
    pub failed_results: Vec<ScrapeItemOutcome>,
    pub recent_succeeded: Vec<ScrapeItemOutcome>,
    pub succeeded_paths: Vec<String>,
    pub processed_paths: HashSet<String>,
    pub failed_paths: HashSet<String>,
    pub elapsed_before_run_ms: u64,
    pub run_started_at: i64,
    pub timing_totals: ScrapeTimingTotals,
}

impl ScrapeJobRuntime {
    pub fn from_persisted(mut p: PersistedScrapeJob) -> Self {
        if p.snapshot.status == ScrapeJobStatus::Running {
            p.snapshot.status = ScrapeJobStatus::Stopped;
            p.snapshot.current_file = None;
            if p.snapshot.finished_at.is_none() {
                p.snapshot.finished_at = Some(now_ms());
            }
        }
        let rev = p.revision.max(p.snapshot.revision.unwrap_or(1));
        Self {
            snapshot: p.snapshot,
            revision: rev,
            request: p.request,
            cancel: None,
            done_rx: None,
            failed_results: p.failed_results,
            recent_succeeded: p.recent_succeeded,
            succeeded_paths: p.succeeded_paths,
            processed_paths: p.processed_paths.into_iter().collect(),
            failed_paths: p.failed_paths.into_iter().collect(),
            elapsed_before_run_ms: p.elapsed_before_run_ms,
            run_started_at: 0,
            timing_totals: p.timing_totals,
        }
    }

    pub fn to_persisted(&self) -> PersistedScrapeJob {
        let elapsed_now = if self.snapshot.status == ScrapeJobStatus::Running {
            (now_ms() - self.run_started_at).max(0) as u64
        } else {
            0
        };
        PersistedScrapeJob {
            snapshot: self.snapshot.clone(),
            revision: self.revision,
            request: self.request.clone(),
            failed_results: self.failed_results.clone(),
            recent_succeeded: self.recent_succeeded.clone(),
            succeeded_paths: self.succeeded_paths.clone(),
            processed_paths: self.processed_paths.iter().cloned().collect(),
            failed_paths: self.failed_paths.iter().cloned().collect(),
            timing_totals: self.timing_totals.clone(),
            elapsed_before_run_ms: self.elapsed_before_run_ms.saturating_add(elapsed_now),
        }
    }

    pub fn build_snapshot(&self, include_paths: bool) -> ScrapeJobSnapshot {
        let count = self.timing_totals.count;
        let timing = if count > 0 {
            let round_avg = |total: u64| ((total as f64) / (count as f64)).round() as u64;
            Some(ScrapeJobTimingStats {
                count,
                avg_total_ms: round_avg(self.timing_totals.total_ms),
                avg_detail_ms: round_avg(self.timing_totals.detail_ms),
                avg_transfer_ms: round_avg(self.timing_totals.transfer_ms),
                avg_artwork_ms: round_avg(self.timing_totals.artwork_ms),
                avg_crop_ms: round_avg(self.timing_totals.crop_ms),
                cross_fs_count: self.timing_totals.cross_fs_count,
            })
        } else {
            None
        };

        let remaining = self
            .request
            .items
            .iter()
            .filter(|it| {
                !self
                    .processed_paths
                    .contains(&canonical_path(&it.file_path))
            })
            .count();

        let tail_limit = SCRAPE_JOB_MAX_RESULTS.saturating_sub(self.failed_results.len());
        let tail_start = self.recent_succeeded.len().saturating_sub(tail_limit);
        let mut results = self.failed_results.clone();
        results.extend(self.recent_succeeded[tail_start..].iter().cloned());

        let elapsed_now = if self.snapshot.status == ScrapeJobStatus::Running {
            (now_ms() - self.run_started_at).max(0) as u64
        } else {
            0
        };

        let mut snap = self.snapshot.clone();
        snap.revision = Some(self.revision);
        snap.remaining = remaining;
        snap.can_resume = self.snapshot.status != ScrapeJobStatus::Running && remaining > 0;
        snap.can_retry_failed =
            self.snapshot.status != ScrapeJobStatus::Running && !self.failed_paths.is_empty();
        snap.elapsed_ms = Some(self.elapsed_before_run_ms.saturating_add(elapsed_now));
        snap.timing = timing;
        snap.total_results = self.snapshot.done;
        snap.results = results;
        snap.succeeded_paths = include_paths.then(|| self.succeeded_paths.clone());
        snap
    }
}

pub struct MutableState {
    pub cfg: ServerConfig,
    pub client: reqwest::Client,
    pub scrape_cfg: ScrapeConfig,
    pub task_order: Vec<String>,
    pub tasks: HashMap<String, TaskItem>,
    pub runs: HashMap<String, RunHandle>,
    pub next_run_id: u64,
    pub scrape_job: Option<ScrapeJobRuntime>,
    pub extractions: HashSet<u64>,
    pub extraction_tokens: HashMap<u64, CancellationToken>,
    pub next_extraction_id: u64,
    pub closing: bool,
}

impl MutableState {
    pub fn ordered_tasks(&self) -> OrderedTasksOwned {
        let list = self
            .task_order
            .iter()
            .filter_map(|k| self.tasks.get(k).map(|v| (k.clone(), v.clone())))
            .collect();
        OrderedTasksOwned(list)
    }

    pub fn ordered_tasks_vec(&self) -> Vec<TaskItem> {
        self.task_order
            .iter()
            .filter_map(|k| self.tasks.get(k).cloned())
            .collect()
    }

    pub fn upsert_task(&mut self, item: TaskItem) {
        let key = item.url.clone();
        if !self.tasks.contains_key(&key) {
            self.task_order.push(key.clone());
        }
        self.tasks.insert(key, item);
    }

    pub fn remove_task(&mut self, url: &str) -> Option<TaskItem> {
        self.task_order.retain(|k| k != url);
        self.tasks.remove(url)
    }
}

pub struct ServerState {
    pub token: String,
    pub cache_dir: PathBuf,
    pub web_dir: Option<PathBuf>,
    pub store: Store,
    pub inner: std::sync::Mutex<MutableState>,
    pub rss_service: Arc<RssSubscriptionService>,
    pub ws_tx: broadcast::Sender<Arc<String>>,
    pub shutdown_cancel: CancellationToken,
    persist_tasks_lock: Mutex<()>,
    persist_scrape_lock: Mutex<()>,
    tasks_broadcast_scheduled: AtomicBool,
    tasks_save_scheduled: AtomicBool,
    scrape_broadcast_scheduled: AtomicBool,
    scrape_save_scheduled: AtomicBool,
}

#[derive(Clone)]
pub struct EnqueueTaskInput {
    pub url: String,
    pub options: TaskOptions,
    pub browser_capture: bool,
    pub browser_source: Option<String>,
}

struct ServerRssDeps {
    weak_state: std::sync::Weak<ServerState>,
}

impl RssSubscriptionDeps for ServerRssDeps {
    fn enqueue_task(
        &self,
        input: EnqueueRssTaskInput,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CoreResult<EnqueueOutcome>> + Send>>
    {
        let weak = self.weak_state.clone();
        Box::pin(async move {
            let Some(state) = weak.upgrade() else {
                return Err(CoreError::msg("服务正在关闭"));
            };
            let header_map = input
                .headers
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            let added = state
                .enqueue(EnqueueTaskInput {
                    url: input.url,
                    options: TaskOptions {
                        filename: Some(input.filename),
                        save_dir: input.save_dir,
                        headers: Some(HeaderInput::Map(header_map)),
                        source_url: Some(input.source_url),
                        scrape_meta: input.scrape_meta,
                        ..Default::default()
                    },
                    browser_capture: false,
                    browser_source: None,
                })
                .await?;
            Ok(if added {
                EnqueueOutcome::Queued
            } else {
                EnqueueOutcome::Skipped
            })
        })
    }

    fn is_task_done_on_disk(
        &self,
        task_url: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
        let weak = self.weak_state.clone();
        let target = task_url.to_string();
        Box::pin(async move {
            let Some(state) = weak.upgrade() else {
                return false;
            };
            let key = source_url(&target).unwrap_or(target);
            let local_video = {
                let guard = state.inner.lock().unwrap();
                guard.tasks.get(&key).and_then(|t| {
                    if t.status == TaskStatus::Done {
                        t.stats.local_video.clone()
                    } else {
                        None
                    }
                })
            };
            match local_video {
                Some(path) if !path.trim().is_empty() => {
                    tokio::fs::try_exists(path).await.unwrap_or(false)
                }
                _ => false,
            }
        })
    }

    fn on_change(&self, sources: Vec<RssSource>) {
        if let Some(state) = self.weak_state.upgrade() {
            state.broadcast_rss(&sources);
        }
    }
}

pub fn rebuild_client_for_config(cfg: &ServerConfig) -> CoreResult<reqwest::Client> {
    init_global_proxy(&ProxySettings {
        mode: cfg.proxy_mode,
        url: cfg.proxy_url.clone(),
        no_proxy: cfg.no_proxy.clone(),
    })
}

impl ServerState {
    pub async fn init(
        cache_dir: PathBuf,
        token: String,
        web_dir: Option<PathBuf>,
    ) -> CoreResult<Arc<Self>> {
        let abs_cache = absolutize(&cache_dir);
        tokio::fs::create_dir_all(&abs_cache).await?;

        let env_defaults = EnvConfigDefaults::default();
        let store = Store::open(&abs_cache, &env_defaults)?;
        let cfg = store.load_server_config(&env_defaults)?;
        let scrape_cfg = store.load_scrape_config(&cfg.save_dir)?;
        let persisted_job = store.load_scrape_job()?;
        let loaded_tasks = store.load_tasks_ordered()?;
        let client = rebuild_client_for_config(&cfg).unwrap_or_else(|err| {
            error!("构建代理客户端失败，回退系统默认客户端: {err}");
            global_http_client()
        });

        let mut task_order = Vec::with_capacity(loaded_tasks.len());
        let mut tasks = HashMap::with_capacity(loaded_tasks.len());
        for t in loaded_tasks {
            task_order.push(t.url.clone());
            tasks.insert(t.url.clone(), t);
        }

        let scrape_job = persisted_job.map(ScrapeJobRuntime::from_persisted);
        let (ws_tx, _) = broadcast::channel(256);
        let shutdown_cancel = CancellationToken::new();

        let state = Arc::new_cyclic(|weak| {
            let deps = Arc::new(ServerRssDeps {
                weak_state: weak.clone(),
            });
            let rss_service = RssSubscriptionService::new(store.clone(), deps)
                .expect("failed to initialize RSS subscription service from SQLite store");
            Self {
                token,
                cache_dir: abs_cache,
                web_dir,
                store,
                inner: std::sync::Mutex::new(MutableState {
                    cfg,
                    client,
                    scrape_cfg,
                    task_order,
                    tasks,
                    runs: HashMap::new(),
                    next_run_id: 1,
                    scrape_job,
                    extractions: HashSet::new(),
                    extraction_tokens: HashMap::new(),
                    next_extraction_id: 1,
                    closing: false,
                }),
                rss_service,
                ws_tx,
                shutdown_cancel,
                persist_tasks_lock: Mutex::new(()),
                persist_scrape_lock: Mutex::new(()),
                tasks_broadcast_scheduled: AtomicBool::new(false),
                tasks_save_scheduled: AtomicBool::new(false),
                scrape_broadcast_scheduled: AtomicBool::new(false),
                scrape_save_scheduled: AtomicBool::new(false),
            }
        });

        state.rss_service.start_scheduler().await;
        Ok(state)
    }

    pub fn authorized(&self, candidate: &str) -> bool {
        if self.token.is_empty() {
            return true;
        }
        if candidate == self.token {
            return true;
        }
        let short_hash = &md5_hex(self.token.trim())[..8];
        candidate == short_hash
    }

    pub fn has_token(&self) -> bool {
        !self.token.trim().is_empty()
    }

    pub fn send_ws_json<T: Serialize>(&self, kind: &'static str, data: &T) {
        if let Ok(json) = serde_json::to_string(&WsEnvelope { kind, data }) {
            let _ = self.ws_tx.send(Arc::new(json));
        }
    }

    pub fn broadcast_rss(&self, sources: &[RssSource]) {
        self.send_ws_json("rssSources", &sources);
    }

    pub async fn broadcast_scrape_config(&self) {
        let sc = self.inner.lock().unwrap().scrape_cfg.clone();
        self.send_ws_json("scrapeConfig", &sc);
    }

    pub async fn broadcast_tasks_now(self: &Arc<Self>) {
        let ordered = self.inner.lock().unwrap().ordered_tasks();
        self.send_ws_json("tasks", &ordered);
    }

    pub fn schedule_tasks_broadcast(self: &Arc<Self>, immediate: bool) {
        if immediate {
            let this = self.clone();
            tokio::spawn(async move {
                this.broadcast_tasks_now().await;
            });
            return;
        }
        if !self.tasks_broadcast_scheduled.swap(true, Ordering::AcqRel) {
            let this = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                this.tasks_broadcast_scheduled
                    .store(false, Ordering::Release);
                if !this.shutdown_cancel.is_cancelled() {
                    this.broadcast_tasks_now().await;
                }
            });
        }
    }

    pub async fn flush_tasks(&self) -> CoreResult<()> {
        let _guard = self.persist_tasks_lock.lock().await;
        let snapshot = self.inner.lock().unwrap().ordered_tasks_vec();
        self.store.replace_all_tasks(&snapshot)
    }

    pub fn schedule_tasks_save(self: &Arc<Self>, immediate: bool) {
        if immediate {
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(err) = this.flush_tasks().await {
                    error!("任务保存失败: {err}");
                }
            });
            return;
        }
        if !self.tasks_save_scheduled.swap(true, Ordering::AcqRel) {
            let this = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                this.tasks_save_scheduled.store(false, Ordering::Release);
                if let Err(err) = this.flush_tasks().await {
                    error!("任务保存失败: {err}");
                }
            });
        }
    }

    pub async fn build_scrape_job_snapshot(
        &self,
        include_paths: bool,
    ) -> Option<ScrapeJobSnapshot> {
        let guard = self.inner.lock().unwrap();
        guard
            .scrape_job
            .as_ref()
            .map(|j| j.build_snapshot(include_paths))
    }

    pub async fn broadcast_scrape_job_now(&self) {
        let snap = self.build_scrape_job_snapshot(false).await;
        self.send_ws_json("scrapeJob", &snap);
    }

    pub fn schedule_scrape_broadcast(self: &Arc<Self>, immediate: bool) {
        if immediate {
            let this = self.clone();
            tokio::spawn(async move {
                this.broadcast_scrape_job_now().await;
            });
            return;
        }
        if !self.scrape_broadcast_scheduled.swap(true, Ordering::AcqRel) {
            let this = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                this.scrape_broadcast_scheduled
                    .store(false, Ordering::Release);
                if !this.shutdown_cancel.is_cancelled() {
                    this.broadcast_scrape_job_now().await;
                }
            });
        }
    }

    pub async fn flush_scrape_job(&self) -> CoreResult<()> {
        let _guard = self.persist_scrape_lock.lock().await;
        let persisted = {
            let guard = self.inner.lock().unwrap();
            guard
                .scrape_job
                .as_ref()
                .map(ScrapeJobRuntime::to_persisted)
        };
        self.store.save_scrape_job(persisted.as_ref())
    }

    pub fn schedule_scrape_save(self: &Arc<Self>, immediate: bool) {
        if immediate {
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(err) = this.flush_scrape_job().await {
                    error!("刮削任务保存失败: {err}");
                }
            });
            return;
        }
        if !self.scrape_save_scheduled.swap(true, Ordering::AcqRel) {
            let this = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                this.scrape_save_scheduled.store(false, Ordering::Release);
                if let Err(err) = this.flush_scrape_job().await {
                    error!("刮削任务保存失败: {err}");
                }
            });
        }
    }

    pub async fn initial_ws_frames(&self) -> Vec<String> {
        let (tasks_json, scrape_cfg_json, scrape_job_json) = {
            let guard = self.inner.lock().unwrap();
            let tasks = serde_json::to_string(&WsEnvelope {
                kind: "tasks",
                data: guard.ordered_tasks(),
            })
            .unwrap_or_default();
            let sc = serde_json::to_string(&WsEnvelope {
                kind: "scrapeConfig",
                data: &guard.scrape_cfg,
            })
            .unwrap_or_default();
            let sj = serde_json::to_string(&WsEnvelope {
                kind: "scrapeJob",
                data: guard.scrape_job.as_ref().map(|j| j.build_snapshot(false)),
            })
            .unwrap_or_default();
            (tasks, sc, sj)
        };
        let info_json = serde_json::to_string(&WsEnvelope {
            kind: "serverInfo",
            data: ServerInfo {
                version: VERSION.to_string(),
            },
        })
        .unwrap_or_default();
        let rss_sources = self.rss_service.list_sources().await;
        let rss_json = serde_json::to_string(&WsEnvelope {
            kind: "rssSources",
            data: &rss_sources,
        })
        .unwrap_or_default();

        vec![
            info_json,
            tasks_json,
            rss_json,
            scrape_cfg_json,
            scrape_job_json,
        ]
    }

    pub async fn register_extraction(
        &self,
        parent: Option<&CancellationToken>,
    ) -> (u64, CancellationToken) {
        let tok = match parent {
            Some(p) => p.child_token(),
            None => self.shutdown_cancel.child_token(),
        };
        let mut guard = self.inner.lock().unwrap();
        if guard.closing {
            tok.cancel();
        }
        let id = guard.next_extraction_id;
        guard.next_extraction_id += 1;
        guard.extractions.insert(id);
        guard.extraction_tokens.insert(id, tok.clone());
        (id, tok)
    }

    pub async fn unregister_extraction(&self, id: u64) {
        let mut guard = self.inner.lock().unwrap();
        guard.extractions.remove(&id);
        guard.extraction_tokens.remove(&id);
    }

    fn split_stem_ext(original: &str) -> (String, String) {
        let ext = path_extname_lower(original);
        if ext.is_empty() {
            (original.to_string(), String::new())
        } else {
            let cut = original.len() - ext.len();
            (original[..cut].to_string(), original[cut..].to_string())
        }
    }

    fn reserve_filename_in_state(
        guard: &MutableState,
        item_url: &str,
        original: &str,
        save_dir: &Path,
    ) -> String {
        let (stem, ext) = Self::split_stem_ext(original);
        for index in 1u32.. {
            let candidate = if index == 1 {
                original.to_string()
            } else {
                format!("{stem} ({index}){ext}")
            };
            let target = save_dir.join(&candidate);
            let exists_disk = target.exists();
            let exists_task = guard.tasks.values().any(|t| {
                t.url != item_url && t.stats.filename.as_deref() == Some(candidate.as_str())
            });
            if !exists_disk && !exists_task {
                return candidate;
            }
        }
        unreachable!()
    }

    pub async fn enqueue(self: &Arc<Self>, input: EnqueueTaskInput) -> CoreResult<bool> {
        if input.browser_capture || input.url.starts_with("browser:") {
            return Err(CoreError::msg(
                "浏览器保存已移除，请提交网页或媒体链接重新下载",
            ));
        }
        let url = source_url(&input.url)?;

        let (cfg, existing_opt, running_wait) = {
            let guard = self.inner.lock().unwrap();
            if guard.closing {
                return Err(CoreError::msg("服务正在关闭"));
            }
            let existing = guard.tasks.get(&url).cloned();
            if let Some(ref ex) = existing {
                if ex.status == TaskStatus::Done {
                    return Ok(false);
                }
                if ex.browser_capture.unwrap_or(false) != input.browser_capture {
                    return Ok(false);
                }
            }
            if let Some(run) = guard.runs.get(&url) {
                if !run.cancel.is_cancelled() {
                    return Ok(false);
                }
                (guard.cfg.clone(), existing, Some(run.done_rx.clone()))
            } else {
                (guard.cfg.clone(), existing, None)
            }
        };

        if let Some(mut rx) = running_wait {
            let _ = rx.wait_for(|&done| done).await;
        }

        let merged_opts = merge_task_options(
            existing_opt.as_ref().and_then(|e| e.options.as_ref()),
            &input.options,
        );
        let exec_opts = build_exec_options_from(&cfg, &self.cache_dir, &merged_opts);
        let formatted = format_options(&url, &exec_opts)?;

        let scrape_meta = merged_opts
            .scrape_meta
            .clone()
            .or_else(|| existing_opt.as_ref().and_then(|e| e.scrape_meta.clone()));
        let dl_type = if matches!(
            formatted.download_type,
            InputTypeKind::File | InputTypeKind::Parser
        ) {
            DownloadTypeKind::File
        } else {
            DownloadTypeKind::M3u8
        };

        let task_cache_dir = self
            .cache_dir
            .join(md5_hex(&url))
            .to_string_lossy()
            .to_string();

        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(run) = guard.runs.get(&url) {
                if !run.cancel.is_cancelled() {
                    return Ok(false);
                }
            }

            let mut item = existing_opt.unwrap_or_else(|| TaskItem {
                stats: DownloadStats::default(),
                url: url.clone(),
                status: TaskStatus::Pending,
                browser_capture: None,
                browser_source: None,
                cache_dir: None,
                options: None,
                dl_options: None,
                scrape_meta: None,
                scrape_status: None,
                scrape_message: None,
                nfo_path: None,
                poster_path: None,
            });

            let final_filename = if item.options.is_some()
                && guard.tasks.contains_key(&url)
                && input.options.filename.is_none()
            {
                item.stats
                    .filename
                    .clone()
                    .unwrap_or_else(|| formatted.filename.clone())
            } else if !guard.tasks.contains_key(&url) {
                Self::reserve_filename_in_state(
                    &guard,
                    &url,
                    &formatted.filename,
                    &formatted.save_dir,
                )
            } else {
                formatted.filename.clone()
            };

            let mut stored_opts = merged_opts;
            stored_opts.filename = Some(final_filename.clone());

            item.url = url.clone();
            item.browser_capture = input.browser_capture.then_some(true);
            item.browser_source = input.browser_source;
            item.status = TaskStatus::Pending;
            item.stats.phase = Some(TaskPhase::Queued);
            item.stats.errmsg = Some(String::new());
            item.stats.merge_message = Some(String::new());
            item.stats.merge_progress = Some(0.0);
            item.stats.filename = Some(final_filename);
            item.stats.download_type = Some(dl_type);
            item.options = Some(stored_opts);
            item.cache_dir = Some(task_cache_dir);
            if let Some(meta) = scrape_meta {
                item.scrape_meta = Some(meta);
                item.scrape_status = Some(TaskScrapeStatus::Idle);
                item.scrape_message = Some(String::new());
            }

            guard.upsert_task(item);
        }

        let _ = self.flush_tasks().await;
        self.broadcast_tasks_now().await;
        self.start_next().await;
        Ok(true)
    }

    pub fn start_next<'a>(
        self: &'a Arc<Self>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut to_spawn: Vec<(
                TaskItem,
                ServerConfig,
                reqwest::Client,
                u64,
                CancellationToken,
                watch::Sender<bool>,
            )> = Vec::new();

            {
                let mut guard = self.inner.lock().unwrap();
                if guard.closing {
                    return;
                }
                let max_downloads = guard.cfg.max_downloads.max(1);
                let order = guard.task_order.clone();
                for url in order {
                    if guard.runs.len() >= max_downloads {
                        break;
                    }
                    if guard.runs.contains_key(&url) {
                        continue;
                    }
                    let Some(item) = guard.tasks.get_mut(&url) else {
                        continue;
                    };
                    if item.status != TaskStatus::Pending {
                        continue;
                    }
                    item.status = TaskStatus::Resume;
                    let snapshot = item.clone();
                    let run_id = guard.next_run_id;
                    guard.next_run_id += 1;
                    let cancel = self.shutdown_cancel.child_token();
                    let (done_tx, done_rx) = watch::channel(false);
                    guard.runs.insert(
                        url,
                        RunHandle {
                            run_id,
                            cancel: cancel.clone(),
                            done_rx,
                        },
                    );
                    to_spawn.push((
                        snapshot,
                        guard.cfg.clone(),
                        guard.client.clone(),
                        run_id,
                        cancel,
                        done_tx,
                    ));
                }
            }

            if !to_spawn.is_empty() {
                self.broadcast_tasks_now().await;
            }

            for (item, cfg, client, run_id, cancel, done_tx) in to_spawn {
                let state = self.clone();
                tokio::spawn(async move {
                    let _finish_guard = FinishGuard { tx: done_tx };
                    state
                        .run_download_task(item, cfg, client, run_id, cancel)
                        .await;

                    {
                        let mut guard = state.inner.lock().unwrap();
                        let matching_url = guard
                            .runs
                            .iter()
                            .find_map(|(k, v)| (v.run_id == run_id).then_some(k.clone()));
                        if let Some(u) = matching_url {
                            guard.runs.remove(&u);
                        }
                    }
                    let _ = state.flush_tasks().await;
                    state.broadcast_tasks_now().await;
                    state.start_next().await;
                });
            }
        })
    }

    async fn run_download_task(
        self: &Arc<Self>,
        item: TaskItem,
        cfg: ServerConfig,
        client: reqwest::Client,
        run_id: u64,
        cancel: CancellationToken,
    ) {
        let task_url = item.url.clone();
        if item.browser_capture.unwrap_or(false) {
            let mut guard = self.inner.lock().unwrap();
            if let Some(t) = guard.tasks.get_mut(&task_url) {
                t.status = TaskStatus::Error;
                t.stats.errmsg = Some("浏览器保存已移除，请提交网页或媒体链接重新下载".to_string());
                t.stats.end_time = Some(now_ms());
            }
            return;
        }

        let opts = item.options.clone().unwrap_or_default();
        let headers_res = build_effective_headers(&cfg, &opts);
        let merged_headers = match headers_res {
            Ok(h) => h,
            Err(err) => {
                let mut guard = self.inner.lock().unwrap();
                if let Some(t) = guard.tasks.get_mut(&task_url) {
                    t.status = TaskStatus::Error;
                    t.stats.errmsg = Some(err.to_string());
                    t.stats.end_time = Some(now_ms());
                }
                return;
            }
        };

        let state_prog = self.clone();
        let url_prog = task_url.clone();
        let cancel_prog = cancel.clone();
        let on_progress: streamdock_core::contract::ProgressCallback = Arc::new(move |patch| {
            if cancel_prog.is_cancelled() {
                return false;
            }
            let accepted = {
                let mut guard = state_prog.inner.lock().unwrap();
                if guard.closing {
                    false
                } else if !guard
                    .runs
                    .get(&url_prog)
                    .is_some_and(|r| r.run_id == run_id)
                {
                    false
                } else if let Some(t) = guard.tasks.get_mut(&url_prog) {
                    merge_download_stats(&mut t.stats, &patch);
                    true
                } else {
                    false
                }
            };
            if accepted {
                state_prog.schedule_tasks_broadcast(false);
                state_prog.schedule_tasks_save(false);
            }
            accepted
        });

        let state_merge = self.clone();
        let url_merge = task_url.clone();
        let cancel_merge = cancel.clone();
        let on_merge_progress: streamdock_core::contract::MergeProgressCallback =
            Arc::new(move |pct, msg| {
                if cancel_merge.is_cancelled() {
                    return;
                }
                let accepted = {
                    let mut guard = state_merge.inner.lock().unwrap();
                    if guard.closing {
                        false
                    } else if !guard
                        .runs
                        .get(&url_merge)
                        .is_some_and(|r| r.run_id == run_id)
                    {
                        false
                    } else if let Some(t) = guard.tasks.get_mut(&url_merge) {
                        t.stats.phase = Some(TaskPhase::Merging);
                        t.stats.merge_progress = Some(pct);
                        t.stats.merge_message = Some(msg);
                        true
                    } else {
                        false
                    }
                };
                if accepted {
                    state_merge.schedule_tasks_broadcast(false);
                }
            });

        let mut exec = build_exec_options_from(&cfg, &self.cache_dir, &opts);
        exec.headers = Some(HeaderInput::Map(
            merged_headers
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect(),
        ));
        exec.client = Some(client);
        exec.convert = Some(true);
        exec.on_progress = Some(on_progress);
        exec.on_merge_progress = Some(on_merge_progress);

        let res = download_resource(&task_url, &exec, cancel.clone()).await;

        let mut should_scrape = false;
        match res {
            Ok(result) => {
                let filepath_size = match result.filepath {
                    Some(ref p) => tokio::fs::metadata(p).await.ok().map(|m| m.len()),
                    None => None,
                };
                let mut guard = self.inner.lock().unwrap();
                let still_owns = guard
                    .runs
                    .get(&task_url)
                    .is_some_and(|r| r.run_id == run_id);
                if still_owns {
                    if let Some(t) = guard.tasks.get_mut(&task_url) {
                        if let Some(ref mut topts) = t.options {
                            topts.retry_failed_only = None;
                        }
                        if result.paused || cancel.is_cancelled() {
                            t.status = TaskStatus::Pause;
                            t.stats.speed = Some(0.0);
                            t.stats.speed_desc = Some(String::new());
                            t.stats.end_time = Some(now_ms());
                        } else if let Some(errmsg) = result.errmsg {
                            t.status = TaskStatus::Error;
                            t.stats.errmsg = Some(errmsg);
                            t.stats.speed = Some(0.0);
                            t.stats.speed_desc = Some(String::new());
                            t.stats.end_time = Some(now_ms());
                        } else if let Some(fp) = result.filepath {
                            merge_download_stats(&mut t.stats, &result.stats);
                            t.status = TaskStatus::Done;
                            t.stats.phase = Some(TaskPhase::Done);
                            t.stats.progress = Some(100.0);
                            t.stats.filename = Some(path_basename(&fp));
                            t.stats.local_video = Some(fp);
                            if let Some(sz) = filepath_size {
                                t.stats.downloaded_size = Some(sz);
                            }
                            t.stats.speed = Some(0.0);
                            t.stats.speed_desc = Some(String::new());
                            t.stats.errmsg = Some(String::new());
                            t.stats.end_time = Some(now_ms());
                            should_scrape = t
                                .scrape_meta
                                .as_ref()
                                .is_some_and(|m| !m.page_url.trim().is_empty());
                        } else {
                            t.status = TaskStatus::Error;
                            t.stats.errmsg = Some("未生成完整文件，缓存已保留".to_string());
                            t.stats.end_time = Some(now_ms());
                        }
                    }
                }
            }
            Err(err) => {
                let mut guard = self.inner.lock().unwrap();
                let still_owns = guard
                    .runs
                    .get(&task_url)
                    .is_some_and(|r| r.run_id == run_id);
                if still_owns {
                    if let Some(t) = guard.tasks.get_mut(&task_url) {
                        if let Some(ref mut topts) = t.options {
                            topts.retry_failed_only = None;
                        }
                        if cancel.is_cancelled() || err.is_aborted() {
                            t.status = TaskStatus::Pause;
                            t.stats.errmsg = Some(String::new());
                        } else {
                            t.status = TaskStatus::Error;
                            t.stats.errmsg = Some(err.to_string());
                        }
                        t.stats.speed = Some(0.0);
                        t.stats.speed_desc = Some(String::new());
                        t.stats.end_time = Some(now_ms());
                    }
                }
            }
        }

        if should_scrape && !cancel.is_cancelled() {
            let _ = self.run_task_scrape(&task_url, &cancel, false).await;
        }
    }

    pub async fn run_task_scrape(
        self: &Arc<Self>,
        task_url: &str,
        cancel: &CancellationToken,
        overwrite: bool,
    ) -> CoreResult<TaskItem> {
        let (req, ffmpeg_path) = {
            let mut guard = self.inner.lock().unwrap();
            let Some(t) = guard.tasks.get_mut(task_url) else {
                return Err(CoreError::msg("任务不存在或尚未下载完成"));
            };
            let local_video = match (&t.status, &t.stats.local_video) {
                (TaskStatus::Done, Some(lv)) if !lv.is_empty() => lv.clone(),
                _ => return Err(CoreError::msg("任务不存在或尚未下载完成")),
            };
            let Some(meta) = t.scrape_meta.clone() else {
                return Err(CoreError::msg(
                    "该任务未绑定详情页元数据，请前往「媒体刮削」扫描匹配",
                ));
            };
            if meta.page_url.trim().is_empty() {
                return Err(CoreError::msg(
                    "该任务未绑定详情页元数据，请前往「媒体刮削」扫描匹配",
                ));
            }
            t.scrape_status = Some(TaskScrapeStatus::Scraping);
            t.scrape_message = Some(String::new());
            let req = ScrapeRunRequest {
                items: vec![ScrapeRunItemInput {
                    file_path: local_video,
                    page_url: meta.page_url,
                    title: meta.title,
                    pub_date: meta.pub_date,
                    tags: meta.tags,
                    guid: meta.guid,
                    actors: None,
                    studio: meta.studio.clone(),
                    cover_url: None,
                    referer: meta.referer.clone(),
                }],
                mode: meta.mode.unwrap_or(ScrapeMode::InPlace),
                output_dir: meta.output_dir,
                transfer_mode: Some(meta.transfer_mode.unwrap_or(ScrapeTransferMode::Move)),
                folder_template: Some(
                    meta.folder_template
                        .unwrap_or(ScrapeFolderTemplate::ActorTitle),
                ),
                poster_crop: Some(meta.poster_crop.unwrap_or(ScrapePosterCrop::Center)),
                download_images: Some(meta.download_images.unwrap_or(true)),
                overwrite_nfo: Some(overwrite || meta.overwrite_nfo.unwrap_or(false)),
                referer: meta.referer,
                studio: meta.studio,
                concurrency: None,
            };
            (req, guard.cfg.ffmpeg_path.clone())
        };

        self.broadcast_tasks_now().await;
        self.schedule_tasks_save(false);

        let scrape_res = run_media_scrape(
            &req,
            &ScrapeRunOptions {
                ffmpeg_path,
                log: false,
                on_item_start: None,
                on_item_done: None,
            },
            cancel,
        )
        .await;

        let updated = {
            let mut guard = self.inner.lock().unwrap();
            let Some(t) = guard.tasks.get_mut(task_url) else {
                return Err(CoreError::msg("任务已被移除"));
            };
            match scrape_res {
                Ok(res) => {
                    if let Some(outcome) = res.results.into_iter().next() {
                        if outcome.status == ScrapeOutcomeStatus::Success {
                            t.scrape_status = Some(TaskScrapeStatus::Done);
                            t.scrape_message = Some(String::new());
                            t.nfo_path = outcome.nfo_path;
                            t.poster_path = outcome.poster_path;
                            if let Some(tv) = outcome.target_video_path {
                                t.stats.filename = Some(path_basename(&tv));
                                t.stats.local_video = Some(tv);
                            }
                        } else {
                            t.scrape_status = Some(TaskScrapeStatus::Error);
                            t.scrape_message = Some(
                                outcome
                                    .message
                                    .unwrap_or_else(|| "自动刮削失败".to_string()),
                            );
                        }
                    } else {
                        t.scrape_status = Some(TaskScrapeStatus::Error);
                        t.scrape_message = Some("自动刮削失败".to_string());
                    }
                }
                Err(err) => {
                    t.scrape_status = Some(TaskScrapeStatus::Error);
                    t.scrape_message = Some(err.to_string());
                }
            }
            t.clone()
        };

        let _ = self.flush_tasks().await;
        self.broadcast_tasks_now().await;
        Ok(updated)
    }

    pub async fn pause_tasks(self: &Arc<Self>, all: bool, urls: Vec<String>) {
        let mut stopping = Vec::new();
        {
            let mut guard = self.inner.lock().unwrap();
            let target_urls = if all { guard.task_order.clone() } else { urls };
            for url in target_urls {
                let should_pause = guard
                    .tasks
                    .get(&url)
                    .is_some_and(|t| matches!(t.status, TaskStatus::Resume | TaskStatus::Pending));
                if !should_pause {
                    continue;
                }
                if let Some(t) = guard.tasks.get_mut(&url) {
                    t.status = TaskStatus::Pause;
                    t.stats.speed = Some(0.0);
                    t.stats.speed_desc = Some(String::new());
                }
                if let Some(run) = guard.runs.get(&url) {
                    run.cancel.cancel();
                    stop_download(&url);
                    stopping.push(run.done_rx.clone());
                }
            }
        }
        self.broadcast_tasks_now().await;
        let _ = self.flush_tasks().await;
        for mut rx in stopping {
            let _ = rx.wait_for(|&done| done).await;
        }
    }

    pub async fn resume_tasks(self: &Arc<Self>, all: bool, urls: Vec<String>) -> CoreResult<()> {
        let list = {
            let guard = self.inner.lock().unwrap();
            let target_urls = if all { guard.task_order.clone() } else { urls };
            target_urls
                .into_iter()
                .filter_map(|u| {
                    guard.tasks.get(&u).and_then(|t| {
                        if matches!(t.status, TaskStatus::Pause | TaskStatus::Error) {
                            Some((
                                u,
                                t.options.clone().unwrap_or_default(),
                                t.browser_capture.unwrap_or(false),
                                t.browser_source.clone(),
                            ))
                        } else {
                            None
                        }
                    })
                })
                .collect::<Vec<_>>()
        };

        for (u, opts, bc, bs) in list {
            let _ = self
                .enqueue(EnqueueTaskInput {
                    url: u,
                    options: opts,
                    browser_capture: bc,
                    browser_source: bs,
                })
                .await;
        }
        Ok(())
    }

    pub async fn delete_tasks(
        self: &Arc<Self>,
        urls: Vec<String>,
        delete_cache: bool,
        delete_video: bool,
    ) -> CoreResult<()> {
        let mut to_clean = Vec::new();
        for url in urls {
            let (removed, run_rx) = {
                let mut guard = self.inner.lock().unwrap();
                let item = guard.remove_task(&url);
                let rx = guard.runs.get(&url).map(|r| {
                    r.cancel.cancel();
                    stop_download(&url);
                    r.done_rx.clone()
                });
                (item, rx)
            };
            if let Some(mut rx) = run_rx {
                let _ = rx.wait_for(|&done| done).await;
            }
            if let Some(item) = removed {
                to_clean.push((url, item.stats.local_video));
            }
        }

        for (url, local_video) in to_clean {
            if delete_cache {
                let target = self.cache_dir.join(md5_hex(&url));
                if target != self.cache_dir && target.starts_with(&self.cache_dir) {
                    let _ = tokio::fs::remove_dir_all(&target).await;
                }
            }
            if delete_video {
                if let Some(lv) = local_video {
                    if !lv.trim().is_empty() {
                        if let Err(err) = tokio::fs::remove_file(&lv).await {
                            if err.kind() != std::io::ErrorKind::NotFound {
                                return Err(err.into());
                            }
                        }
                    }
                }
            }
        }

        let _ = self.flush_tasks().await;
        self.broadcast_tasks_now().await;
        self.start_next().await;
        Ok(())
    }

    pub async fn queue_status(&self) -> QueueStatus {
        let guard = self.inner.lock().unwrap();
        let queue_length = guard
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .count();
        let active_downloads: Vec<String> = guard
            .task_order
            .iter()
            .filter(|u| guard.runs.contains_key(*u))
            .cloned()
            .collect();
        QueueStatus {
            queue_length,
            active_downloads,
            max_concurrent: guard.cfg.max_downloads,
        }
    }

    pub async fn launch_scrape_job_runner(self: &Arc<Self>, items_to_run: Vec<ScrapeRunItemInput>) {
        let (request, ffmpeg_path, cancel, done_tx) = {
            let mut guard = self.inner.lock().unwrap();
            let Some(job) = guard.scrape_job.as_mut() else {
                return;
            };
            let cancel = self.shutdown_cancel.child_token();
            let (done_tx, done_rx) = watch::channel(false);
            job.cancel = Some(cancel.clone());
            job.done_rx = Some(done_rx);
            job.run_started_at = now_ms();
            job.revision += 1;
            job.snapshot.status = ScrapeJobStatus::Running;
            job.snapshot.finished_at = None;
            job.snapshot.error = None;
            job.snapshot.current_file = items_to_run.first().map(|it| path_basename(&it.file_path));

            let mut req = job.request.clone();
            req.items = items_to_run;
            (req, guard.cfg.ffmpeg_path.clone(), cancel, done_tx)
        };

        self.broadcast_scrape_job_now().await;
        let _ = self.flush_scrape_job().await;

        let state = self.clone();
        tokio::spawn(async move {
            let _guard = FinishGuard { tx: done_tx };
            let st_start = state.clone();
            let on_item_start: streamdock_core::scraper::OnScrapeItemStart =
                Arc::new(move |item, _, _| {
                    let name = path_basename(&item.file_path);
                    {
                        let mut g = st_start.inner.lock().unwrap();
                        if let Some(ref mut job) = g.scrape_job {
                            job.revision += 1;
                            job.snapshot.current_file = Some(name);
                        }
                    }
                    st_start.schedule_scrape_broadcast(false);
                });

            let st_done = state.clone();
            let on_item_done: streamdock_core::scraper::OnScrapeItemDone =
                Arc::new(move |outcome, _, _| {
                    {
                        let mut g = st_done.inner.lock().unwrap();
                        if let Some(ref mut job) = g.scrape_job {
                            let key = canonical_path(&outcome.file_path);
                            job.processed_paths.insert(key.clone());
                            if outcome.status == ScrapeOutcomeStatus::Success {
                                job.snapshot.succeeded += 1;
                                job.failed_paths.remove(&key);
                                job.succeeded_paths.push(outcome.file_path.clone());
                                job.recent_succeeded.push(outcome.clone());
                                if job.recent_succeeded.len() > SCRAPE_JOB_MAX_RESULTS {
                                    job.recent_succeeded.remove(0);
                                }
                            } else {
                                job.snapshot.failed += 1;
                                job.failed_paths.insert(key);
                                job.failed_results.push(outcome.clone());
                                if job.failed_results.len() > SCRAPE_JOB_MAX_FAILED {
                                    job.failed_results.remove(0);
                                }
                            }
                            job.snapshot.done = job.snapshot.succeeded + job.snapshot.failed;
                            if let Some(ref tm) = outcome.timing {
                                job.timing_totals.count += 1;
                                job.timing_totals.total_ms += tm.total_ms;
                                job.timing_totals.detail_ms += tm.detail_ms;
                                job.timing_totals.transfer_ms += tm.transfer_ms;
                                job.timing_totals.artwork_ms += tm.artwork_ms;
                                job.timing_totals.crop_ms += tm.crop_ms;
                                if tm.transfer_actual == Some(ScrapeTransferActual::CopyCrossFs) {
                                    job.timing_totals.cross_fs_count += 1;
                                }
                            }
                            job.revision += 1;
                        }
                    }
                    st_done.schedule_scrape_broadcast(false);
                    st_done.schedule_scrape_save(false);
                });

            let run_res = run_media_scrape(
                &request,
                &ScrapeRunOptions {
                    ffmpeg_path,
                    log: true,
                    on_item_start: Some(on_item_start),
                    on_item_done: Some(on_item_done),
                },
                &cancel,
            )
            .await;

            {
                let mut g = state.inner.lock().unwrap();
                if let Some(ref mut job) = g.scrape_job {
                    match run_res {
                        Ok(_) => {
                            let remaining = job
                                .request
                                .items
                                .iter()
                                .filter(|it| {
                                    !job.processed_paths.contains(&canonical_path(&it.file_path))
                                })
                                .count();
                            job.snapshot.status = if cancel.is_cancelled() || remaining > 0 {
                                ScrapeJobStatus::Stopped
                            } else {
                                ScrapeJobStatus::Done
                            };
                        }
                        Err(err) => {
                            job.snapshot.status = if cancel.is_cancelled() || err.is_aborted() {
                                ScrapeJobStatus::Stopped
                            } else {
                                job.snapshot.error = Some(err.to_string());
                                ScrapeJobStatus::Error
                            };
                        }
                    }
                    job.snapshot.finished_at = Some(now_ms());
                    job.snapshot.current_file = None;
                    job.revision += 1;
                    let delta = (now_ms() - job.run_started_at).max(0) as u64;
                    job.elapsed_before_run_ms = job.elapsed_before_run_ms.saturating_add(delta);
                    job.cancel = None;
                }
            }

            let _ = state.flush_scrape_job().await;
            state.broadcast_scrape_job_now().await;
        });
    }

    pub async fn close(&self) {
        self.shutdown_cancel.cancel();
        let (run_rxs, scrape_rx) = {
            let mut guard = self.inner.lock().unwrap();
            guard.closing = true;
            for tok in guard.extraction_tokens.values() {
                tok.cancel();
            }
            let rxs: Vec<watch::Receiver<bool>> = guard
                .runs
                .iter()
                .map(|(url, r)| {
                    r.cancel.cancel();
                    stop_download(url);
                    r.done_rx.clone()
                })
                .collect();
            let s_rx = guard.scrape_job.as_ref().and_then(|j| {
                if let Some(ref c) = j.cancel {
                    c.cancel();
                }
                j.done_rx.clone()
            });
            (rxs, s_rx)
        };

        self.rss_service.close().await;
        for mut rx in run_rxs {
            let _ = rx.wait_for(|&done| done).await;
        }
        if let Some(mut rx) = scrape_rx {
            let _ = rx.wait_for(|&done| done).await;
        }
        let _ = self.flush_tasks().await;
        let _ = self.flush_scrape_job().await;
        info!("StreamDock 服务已停止");
    }
}

pub fn merge_task_options(base: Option<&TaskOptions>, over: &TaskOptions) -> TaskOptions {
    let Some(b) = base else {
        return over.clone();
    };
    TaskOptions {
        use_default_headers: over.use_default_headers.or(b.use_default_headers),
        download_type: over.download_type.or(b.download_type),
        source_url: over.source_url.clone().or_else(|| b.source_url.clone()),
        header_source_url: over
            .header_source_url
            .clone()
            .or_else(|| b.header_source_url.clone()),
        filename: over.filename.clone().or_else(|| b.filename.clone()),
        save_dir: over.save_dir.clone().or_else(|| b.save_dir.clone()),
        ignore_segments: over
            .ignore_segments
            .clone()
            .or_else(|| b.ignore_segments.clone()),
        headers: over.headers.clone().or_else(|| b.headers.clone()),
        token_session: over
            .token_session
            .clone()
            .or_else(|| b.token_session.clone()),
        scrape_meta: over.scrape_meta.clone().or_else(|| b.scrape_meta.clone()),
        retry_failed_only: over.retry_failed_only.or(b.retry_failed_only),
    }
}

pub fn build_exec_options_from(
    cfg: &ServerConfig,
    cache_dir: &Path,
    opts: &TaskOptions,
) -> DownloadExecOptions {
    DownloadExecOptions {
        download_type: opts.download_type,
        source_url: opts.source_url.clone(),
        header_source_url: opts.header_source_url.clone(),
        retry_failed_only: opts.retry_failed_only.unwrap_or(false),
        merge_timeout_secs: Some(cfg.merge_timeout as f64),
        thread_num: Some(cfg.thread_num),
        filename: opts.filename.clone(),
        save_dir: Some(
            opts.save_dir
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| cfg.save_dir.clone()),
        ),
        cache_dir: Some(cache_dir.to_string_lossy().to_string()),
        ignore_segments: opts.ignore_segments.clone(),
        del_cache: Some(cfg.del_cache),
        force: false,
        headers: opts.headers.clone(),
        convert: Some(true),
        ffmpeg_path: (!cfg.ffmpeg_path.is_empty()).then(|| cfg.ffmpeg_path.clone()),
        proxy_mode: Some(cfg.proxy_mode),
        proxy_url: Some(cfg.proxy_url.clone()),
        no_proxy: Some(cfg.no_proxy.clone()),
        token_session: opts.token_session.clone(),
        segment_identity: None,
        on_progress: None,
        on_merge_progress: None,
        extra_headers: None,
        client: None,
    }
}

pub fn build_effective_headers(
    cfg: &ServerConfig,
    opts: &TaskOptions,
) -> CoreResult<streamdock_core::contract::HeaderMap> {
    let base = if opts.use_default_headers == Some(false) {
        Default::default()
    } else {
        format_headers_str(&cfg.headers)?
    };
    let custom = format_headers(opts.headers.as_ref())?;
    Ok(merge_header_maps(&base, &custom))
}

fn merge_download_stats(target: &mut DownloadStats, patch: &DownloadStats) {
    if patch.download_type.is_some() {
        target.download_type = patch.download_type;
    }
    if patch.start_time.is_some() {
        target.start_time = patch.start_time;
    }
    if patch.end_time.is_some() {
        target.end_time = patch.end_time;
    }
    if patch.progress.is_some() {
        target.progress = patch.progress;
    }
    if patch.phase.is_some() {
        target.phase = patch.phase;
    }
    if patch.merge_progress.is_some() {
        target.merge_progress = patch.merge_progress;
    }
    if patch.merge_message.is_some() {
        target.merge_message = patch.merge_message.clone();
    }
    if patch.ts_count.is_some() {
        target.ts_count = patch.ts_count;
    }
    if patch.ts_success.is_some() {
        target.ts_success = patch.ts_success;
    }
    if patch.ts_failed.is_some() {
        target.ts_failed = patch.ts_failed;
    }
    if patch.has_segment_details.is_some() {
        target.has_segment_details = patch.has_segment_details;
    }
    if patch.duration.is_some() {
        target.duration = patch.duration;
    }
    if patch.size.is_some() {
        target.size = patch.size;
    }
    if patch.downloaded_size.is_some() {
        target.downloaded_size = patch.downloaded_size;
    }
    if patch.speed.is_some() {
        target.speed = patch.speed;
    }
    if patch.speed_desc.is_some() {
        target.speed_desc = patch.speed_desc.clone();
    }
    if patch.remaining_time.is_some() {
        target.remaining_time = patch.remaining_time;
    }
    if patch.local_video.is_some() {
        target.local_video = patch.local_video.clone();
    }
    if patch.filename.is_some() {
        target.filename = patch.filename.clone();
    }
    if patch.thread_num.is_some() {
        target.thread_num = patch.thread_num;
    }
    if patch.errmsg.is_some() {
        target.errmsg = patch.errmsg.clone();
    }
}

pub fn scrape_data_source_from_json_defaults(
    patch: &serde_json::Value,
    current: &ScrapeConfig,
    fallback_save_dir: &str,
) -> ScrapeConfig {
    let mut base_val = serde_json::to_value(current).unwrap_or(serde_json::Value::Null);
    if let (Some(base_obj), Some(patch_obj)) = (base_val.as_object_mut(), patch.as_object()) {
        for (k, v) in patch_obj {
            base_obj.insert(k.clone(), v.clone());
        }
        if !patch_obj.contains_key("sources")
            && (patch_obj.contains_key("rssUrl") || patch_obj.contains_key("referer"))
        {
            base_obj.remove("sources");
        }
    }
    normalize_scrape_config_value(&base_val, fallback_save_dir)
}
