use crate::contract::{
    HeaderMap, RssActionErrorItem, RssActionResult, RssFeedItem, RssItemStatus, RssItemsPage,
    RssRefreshResult, RssRejectReason, RssSource, RssSourceInput, RssSourceMode,
    RssValidateRequest, RssValidateResult, TaskScrapeMeta,
};
use crate::error::{CoreError, CoreResult};
use crate::rss_filter::{
    RssFilterRule, RssFilterVerdict, compile_rss_filter, evaluate_rss_filter, extract_episode_key,
};
use crate::rss_parser::{
    FetchFeedOptions, fetch_and_parse_feed, make_safe_feed_slug, resolve_rss_item_media,
};
use crate::store::{Store, normalize_http_href, normalize_rss_source_input};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

const MAX_BACKOFF_MS: i64 = 6 * 3600 * 1000;
const SCHEDULER_TICK_SECS: u64 = 30;
const DOWNLOAD_CONCURRENCY: usize = 3;

#[derive(Debug, Clone)]
pub struct EnqueueRssTaskInput {
    pub url: String,
    pub filename: String,
    pub save_dir: Option<String>,
    pub headers: HeaderMap,
    pub source_url: String,
    pub scrape_meta: Option<TaskScrapeMeta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued,
    Skipped,
}

pub trait RssSubscriptionDeps: Send + Sync {
    fn enqueue_task(
        &self,
        input: EnqueueRssTaskInput,
    ) -> Pin<Box<dyn Future<Output = CoreResult<EnqueueOutcome>> + Send>>;
    fn is_task_done_on_disk(&self, task_url: &str) -> Pin<Box<dyn Future<Output = bool> + Send>>;
    fn on_change(&self, sources: Vec<RssSource>);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RssItemAction {
    Download,
    Ignore,
    MarkNew,
}

pub fn is_source_due(source: &RssSource, now_ms: i64) -> bool {
    if !source.enabled {
        return false;
    }
    if source.last_fetch_at == 0 {
        return true;
    }
    let interval_ms = (source.interval_minutes.max(1) as i64) * 60_000;
    let delay_ms = if source.fail_count > 0 {
        let shift = source.fail_count.min(8);
        let exp = interval_ms.saturating_mul(1i64 << shift);
        exp.max(interval_ms).min(MAX_BACKOFF_MS)
    } else {
        interval_ms
    };
    now_ms - source.last_fetch_at >= delay_ms
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub struct RssSubscriptionService {
    store: Store,
    deps: Arc<dyn RssSubscriptionDeps>,
    sources: Mutex<HashMap<String, RssSource>>,
    inflight_refreshes:
        Mutex<HashMap<String, watch::Receiver<Option<Result<RssRefreshResult, String>>>>>,
    shutdown_cancel: CancellationToken,
    scheduler_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl RssSubscriptionService {
    pub fn new(store: Store, deps: Arc<dyn RssSubscriptionDeps>) -> CoreResult<Arc<Self>> {
        let list = store.list_rss_sources()?;
        let mut map = HashMap::new();
        for mut src in list {
            let counts = store.compute_rss_counts(&src.id)?;
            src.total_count = counts.all;
            src.unread_count = counts.new;
            map.insert(src.id.clone(), src);
        }
        Ok(Arc::new(Self {
            store,
            deps,
            sources: Mutex::new(map),
            inflight_refreshes: Mutex::new(HashMap::new()),
            shutdown_cancel: CancellationToken::new(),
            scheduler_handle: Mutex::new(None),
        }))
    }

    pub async fn start_scheduler(self: &Arc<Self>) {
        let mut handle = self.scheduler_handle.lock().await;
        if handle.is_some() || self.shutdown_cancel.is_cancelled() {
            return;
        }
        let svc = self.clone();
        let tok = self.shutdown_cancel.clone();
        *handle = Some(tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(SCHEDULER_TICK_SECS));
            iv.tick().await;
            loop {
                tokio::select! {
                    _ = tok.cancelled() => break,
                    _ = iv.tick() => {
                        let now = now_ms();
                        let sources = svc.list_sources().await;
                        for src in sources {
                            if is_source_due(&src, now) {
                                let already = svc.inflight_refreshes.lock().await.contains_key(&src.id);
                                if !already {
                                    let svc_run = svc.clone();
                                    let id = src.id.clone();
                                    tokio::spawn(async move {
                                        let _ = svc_run.refresh_source(&id, CancellationToken::new()).await;
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }));
    }

    pub async fn close(&self) {
        self.shutdown_cancel.cancel();
        if let Some(h) = self.scheduler_handle.lock().await.take() {
            let _ = h.await;
        }
        // Wait for any in-flight refreshes triggered before close.
        let receivers: Vec<_> = self
            .inflight_refreshes
            .lock()
            .await
            .values()
            .cloned()
            .collect();
        for mut rx in receivers {
            while rx.borrow().is_none() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    pub async fn list_sources(&self) -> Vec<RssSource> {
        let mut list: Vec<RssSource> = self.sources.lock().await.values().cloned().collect();
        list.sort_by_key(|s| s.position);
        list
    }

    pub async fn get_source(&self, id: &str) -> Option<RssSource> {
        self.sources.lock().await.get(id.trim()).cloned()
    }

    async fn sync_counters_and_save(&self, id: &str) -> CoreResult<Option<RssSource>> {
        let counts = self.store.compute_rss_counts(id)?;
        let mut map = self.sources.lock().await;
        let Some(src) = map.get_mut(id) else {
            return Ok(None);
        };
        src.total_count = counts.all;
        src.unread_count = counts.new;
        let cloned = src.clone();
        drop(map);
        self.store.save_rss_source(&cloned)?;
        Ok(Some(cloned))
    }

    async fn notify_change(&self) {
        let list = self.list_sources().await;
        self.deps.on_change(list);
    }

    pub async fn validate(
        &self,
        req: &RssValidateRequest,
        cancel: CancellationToken,
    ) -> CoreResult<RssValidateResult> {
        let url = normalize_http_href(Some(&req.url), "订阅地址", true)?;
        let mode = req.mode.unwrap_or(RssSourceMode::Rss);
        let referer = normalize_http_href(req.referer.as_deref(), "Referer", false)?;
        let combined = combine_tokens(&self.shutdown_cancel, &cancel);
        let parsed = fetch_and_parse_feed(
            &FetchFeedOptions {
                url,
                mode,
                item_pattern: req.item_pattern.as_deref().unwrap_or("").trim().to_string(),
                referer,
                cookies: req.cookies.as_deref().unwrap_or("").trim().to_string(),
                user_agent: req.user_agent.as_deref().unwrap_or("").trim().to_string(),
            },
            &combined,
        )
        .await?;

        let now = now_ms();
        let total_items = parsed.items.len();
        let sample_items = parsed
            .items
            .into_iter()
            .take(50)
            .map(|it| {
                let episode_key = extract_episode_key(&it.title);
                RssFeedItem {
                    source_id: String::new(),
                    guid: it.guid,
                    title: it.title,
                    link: it.link,
                    enclosure_url: it.enclosure_url,
                    enclosure_length: it.enclosure_length,
                    pub_date: it.pub_date,
                    pub_date_text: it.pub_date_text,
                    tags: it.tags,
                    fetched_at: now,
                    status: RssItemStatus::New,
                    task_url: String::new(),
                    episode_key,
                    reason: RssRejectReason::None,
                }
            })
            .collect();

        Ok(RssValidateResult {
            url: parsed.resolved_url,
            mode,
            feed_title: parsed.feed_title,
            total_items,
            sample_items,
        })
    }

    pub async fn create_source(&self, input: &RssSourceInput) -> CoreResult<RssSource> {
        let mut map = self.sources.lock().await;
        if map.len() >= 50 {
            return Err(CoreError::msg("订阅源数量已达上限（50 个）"));
        }
        let source = normalize_rss_source_input(input, None, map.len() as i64)?;
        if map.contains_key(&source.id) {
            return Err(CoreError::msg(format!("订阅源 ID 已存在：{}", source.id)));
        }
        self.store.save_rss_source(&source)?;
        map.insert(source.id.clone(), source.clone());
        drop(map);
        self.notify_change().await;
        Ok(source)
    }

    pub async fn update_source(&self, id: &str, input: serde_json::Value) -> CoreResult<RssSource> {
        let key = id.trim();
        let existing = self
            .sources
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| CoreError::msg("订阅源不存在"))?;

        let mut merged_val = serde_json::to_value(&existing)?;
        if let (Some(base), Some(patch)) = (merged_val.as_object_mut(), input.as_object()) {
            for (k, v) in patch {
                if !v.is_null() {
                    base.insert(k.clone(), v.clone());
                }
            }
            base.insert(
                "id".to_string(),
                serde_json::Value::String(existing.id.clone()),
            );
        }
        let parsed_input: RssSourceInput = serde_json::from_value(merged_val)?;
        let mut next =
            normalize_rss_source_input(&parsed_input, Some(&existing), existing.position)?;

        if !existing.auto_download && next.auto_download {
            let mut items = self.store.load_rss_items(key)?;
            let mut changed = false;
            for it in &mut items {
                if it.status == RssItemStatus::New {
                    it.status = RssItemStatus::SeedSkipped;
                    it.reason = RssRejectReason::SeedSkipped;
                    changed = true;
                }
            }
            if changed {
                self.store.replace_rss_items(key, &items)?;
            }
            next.seeded = true;
        }

        let counts = self.store.compute_rss_counts(key)?;
        next.total_count = counts.all;
        next.unread_count = counts.new;
        self.store.save_rss_source(&next)?;
        self.sources
            .lock()
            .await
            .insert(key.to_string(), next.clone());
        self.notify_change().await;
        Ok(next)
    }

    pub async fn delete_source(&self, id: &str) -> CoreResult<()> {
        let key = id.trim();
        let removed = self.sources.lock().await.remove(key).is_some();
        if !removed {
            return Err(CoreError::msg("订阅源不存在"));
        }
        self.store.delete_rss_source(key)?;
        self.notify_change().await;
        Ok(())
    }

    fn build_scrape_meta(
        source: &RssSource,
        item: &RssFeedItem,
        referer: &str,
    ) -> Option<TaskScrapeMeta> {
        if !source.auto_scrape {
            return None;
        }
        let eff_ref = if !referer.is_empty() {
            Some(referer.to_string())
        } else if !source.referer.is_empty() {
            Some(source.referer.clone())
        } else {
            None
        };
        Some(TaskScrapeMeta {
            page_url: item.link.clone(),
            title: Some(item.title.clone()),
            pub_date: (!item.pub_date_text.is_empty()).then(|| item.pub_date_text.clone()),
            tags: (!item.tags.is_empty()).then(|| item.tags.clone()),
            guid: Some(item.guid.clone()),
            referer: eff_ref,
            studio: (!source.name.is_empty()).then(|| source.name.clone()),
            mode: Some(source.scrape_mode),
            output_dir: (source.scrape_mode == crate::contract::ScrapeMode::Organize)
                .then(|| source.scrape_output_dir.clone()),
            transfer_mode: Some(source.scrape_transfer_mode),
            folder_template: Some(source.scrape_folder_template),
            poster_crop: Some(source.scrape_poster_crop),
            download_images: Some(source.scrape_download_images),
            overwrite_nfo: None,
        })
    }

    async fn dispatch_download(
        &self,
        source: &RssSource,
        item: &mut RssFeedItem,
        cancel: &CancellationToken,
    ) -> CoreResult<EnqueueOutcome> {
        if item.status == RssItemStatus::Downloaded && !item.task_url.is_empty() {
            if self.deps.is_task_done_on_disk(&item.task_url).await {
                return Ok(EnqueueOutcome::Skipped);
            }
        }
        let resolved = resolve_rss_item_media(item, source, cancel).await?;
        let filename = make_safe_feed_slug(&item.title, &item.guid);
        let outcome = self
            .deps
            .enqueue_task(EnqueueRssTaskInput {
                url: resolved.media_url.clone(),
                filename,
                save_dir: (!source.save_dir.is_empty()).then(|| source.save_dir.clone()),
                headers: resolved.headers,
                source_url: item.link.clone(),
                scrape_meta: Self::build_scrape_meta(source, item, &resolved.referer),
            })
            .await?;
        item.status = RssItemStatus::Downloaded;
        item.task_url = resolved.media_url;
        item.reason = RssRejectReason::None;
        Ok(outcome)
    }

    pub async fn refresh_source(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> CoreResult<RssRefreshResult> {
        let key = id.trim().to_string();
        let existing_rx = {
            let mut map = self.inflight_refreshes.lock().await;
            if let Some(rx) = map.get(&key) {
                Some(rx.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                map.insert(key.clone(), rx);
                drop(map);
                let result = self.do_refresh_source(&key, cancel).await;
                let sent = match &result {
                    Ok(ok) => Ok(ok.clone()),
                    Err(e) => Err(e.to_string()),
                };
                self.inflight_refreshes.lock().await.remove(&key);
                let _ = tx.send(Some(sent));
                return result;
            }
        };

        if let Some(mut rx) = existing_rx {
            loop {
                if let Some(res) = rx.borrow().clone() {
                    return res.map_err(CoreError::msg);
                }
                if rx.changed().await.is_err() {
                    return Err(CoreError::msg("刷新订阅失败"));
                }
            }
        }
        unreachable!()
    }

    async fn do_refresh_source(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> CoreResult<RssRefreshResult> {
        let mut source = self
            .sources
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::msg("订阅源不存在"))?;

        let combined = combine_tokens(&self.shutdown_cancel, &cancel);
        let now = now_ms();
        source.last_fetch_at = now;
        if let Some(s) = self.sources.lock().await.get_mut(id) {
            s.last_fetch_at = now;
        }

        let res: CoreResult<RssRefreshResult> = async {
            let parsed = fetch_and_parse_feed(
                &FetchFeedOptions {
                    url: source.url.clone(),
                    mode: source.mode,
                    item_pattern: source.item_pattern.clone(),
                    referer: source.referer.clone(),
                    cookies: source.cookies.clone(),
                    user_agent: source.user_agent.clone(),
                },
                &combined,
            )
            .await?;

            if !self.sources.lock().await.contains_key(id) {
                return Err(CoreError::msg("订阅源已被删除"));
            }

            let existing_items = self.store.load_rss_items(id)?;
            let mut existing_by_guid: HashMap<String, RssFeedItem> = existing_items
                .into_iter()
                .map(|it| (it.guid.clone(), it))
                .collect();

            let mut merged: Vec<RssFeedItem> =
                Vec::with_capacity(parsed.items.len() + existing_by_guid.len());
            let mut new_indices: Vec<usize> = Vec::new();

            for raw in parsed.items {
                if let Some(mut old) = existing_by_guid.remove(&raw.guid) {
                    old.title = raw.title;
                    old.link = raw.link;
                    if !raw.enclosure_url.is_empty() {
                        old.enclosure_url = raw.enclosure_url;
                    }
                    if raw.enclosure_length > 0 {
                        old.enclosure_length = raw.enclosure_length;
                    }
                    if raw.pub_date > 0 {
                        old.pub_date = raw.pub_date;
                    }
                    if !raw.pub_date_text.is_empty() {
                        old.pub_date_text = raw.pub_date_text;
                    }
                    if !raw.tags.is_empty() {
                        old.tags = raw.tags;
                    }
                    merged.push(old);
                } else {
                    let episode_key = extract_episode_key(&raw.title);
                    let created = RssFeedItem {
                        source_id: id.to_string(),
                        guid: raw.guid,
                        title: raw.title,
                        link: raw.link,
                        enclosure_url: raw.enclosure_url,
                        enclosure_length: raw.enclosure_length,
                        pub_date: raw.pub_date,
                        pub_date_text: raw.pub_date_text,
                        tags: raw.tags,
                        fetched_at: now,
                        status: RssItemStatus::New,
                        task_url: String::new(),
                        episode_key,
                        reason: RssRejectReason::None,
                    };
                    new_indices.push(merged.len());
                    merged.push(created);
                }
            }

            for leftover in existing_by_guid.into_values() {
                merged.push(leftover);
            }

            let added_count = new_indices.len();
            let mut auto_queued = 0usize;

            if !source.auto_download {
                source.seeded = true;
            } else if !source.seeded {
                for idx in new_indices {
                    merged[idx].status = RssItemStatus::SeedSkipped;
                    merged[idx].reason = RssRejectReason::SeedSkipped;
                }
                source.seeded = true;
            } else {
                let compiled = compile_rss_filter(&RssFilterRule {
                    include_pattern: source.include_pattern.clone(),
                    exclude_pattern: source.exclude_pattern.clone(),
                    use_regex: source.use_regex,
                    smart_episode: source.smart_episode,
                    size_min_bytes: source.size_min_bytes,
                    size_max_bytes: source.size_max_bytes,
                });

                let mut seen_episodes = HashSet::new();
                if source.smart_episode {
                    for it in &merged {
                        if it.status != RssItemStatus::Downloaded {
                            continue;
                        }
                        let ep = if it.episode_key.is_empty() {
                            extract_episode_key(&it.title)
                        } else {
                            it.episode_key.clone()
                        };
                        if !ep.is_empty() {
                            seen_episodes.insert(ep);
                        }
                    }
                }

                let mut candidate_indices: Vec<usize> = merged
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| it.status == RssItemStatus::New)
                    .map(|(i, _)| i)
                    .collect();
                candidate_indices.sort_by_key(|&i| {
                    let it = &merged[i];
                    if it.pub_date > 0 {
                        it.pub_date
                    } else {
                        it.fetched_at
                    }
                });

                let mut to_dispatch: Vec<usize> = Vec::new();
                for idx in candidate_indices {
                    let item = &mut merged[idx];
                    let verdict = evaluate_rss_filter(
                        &compiled,
                        &item.title,
                        item.enclosure_length,
                        Some(&mut seen_episodes),
                    );
                    match verdict {
                        RssFilterVerdict::Rejected {
                            status,
                            reason,
                            episode_key,
                        } => {
                            item.status = status;
                            item.reason = reason;
                            if !episode_key.is_empty() {
                                item.episode_key = episode_key;
                            }
                        }
                        RssFilterVerdict::Accepted { episode_key } => {
                            if !episode_key.is_empty() {
                                item.episode_key = episode_key;
                            }
                            item.reason = RssRejectReason::None;
                            if to_dispatch.len() < source.max_per_fetch {
                                to_dispatch.push(idx);
                            }
                        }
                    }
                }

                for idx in to_dispatch {
                    if combined.is_cancelled() || !self.sources.lock().await.contains_key(id) {
                        break;
                    }
                    let mut item_clone = merged[idx].clone();
                    match self
                        .dispatch_download(&source, &mut item_clone, &combined)
                        .await
                    {
                        Ok(outcome) => {
                            merged[idx] = item_clone;
                            if outcome == EnqueueOutcome::Queued {
                                auto_queued += 1;
                            }
                        }
                        Err(_) => {
                            if source.smart_episode && !merged[idx].episode_key.is_empty() {
                                seen_episodes.remove(&merged[idx].episode_key);
                            }
                        }
                    }
                }
            }

            self.store.replace_rss_items(id, &merged)?;
            let counts = self.store.compute_rss_counts(id)?;
            source.last_success_at = now_ms();
            source.last_error.clear();
            source.fail_count = 0;
            source.total_count = counts.all;
            source.unread_count = counts.new;

            self.store.save_rss_source(&source)?;
            self.sources
                .lock()
                .await
                .insert(id.to_string(), source.clone());
            self.notify_change().await;

            Ok(RssRefreshResult {
                source: source.clone(),
                added: added_count,
                auto_queued,
                total_items: merged.len(),
            })
        }
        .await;

        if let Err(ref err) = res {
            let mut map = self.sources.lock().await;
            if let Some(s) = map.get_mut(id) {
                s.fail_count = s.fail_count.saturating_add(1);
                s.last_error = {
                    let msg = err.to_string();
                    if msg.is_empty() {
                        "抓取订阅失败".to_string()
                    } else {
                        msg
                    }
                };
                let cloned = s.clone();
                drop(map);
                let _ = self.store.save_rss_source(&cloned);
                self.notify_change().await;
            }
        }

        res
    }

    pub async fn query_items(
        &self,
        id: &str,
        status: &str,
        q: &str,
        sort_oldest: bool,
        page: usize,
        page_size: usize,
    ) -> CoreResult<RssItemsPage> {
        let key = id.trim();
        if !self.sources.lock().await.contains_key(key) {
            return Err(CoreError::msg("订阅源不存在"));
        }
        self.store
            .query_rss_items(key, status, q, sort_oldest, page, page_size)
    }

    pub async fn act_on_items(
        &self,
        id: &str,
        guids: &[String],
        action: RssItemAction,
        cancel: CancellationToken,
    ) -> CoreResult<RssActionResult> {
        let key = id.trim();
        let source = self
            .sources
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| CoreError::msg("订阅源不存在"))?;
        if guids.is_empty() {
            return Err(CoreError::msg("请选择至少一条订阅条目"));
        }
        if guids.len() > 200 {
            return Err(CoreError::msg("单次最多操作 200 条订阅条目"));
        }

        let mut items = self.store.load_rss_items(key)?;
        let idx_by_guid: HashMap<String, usize> = items
            .iter()
            .enumerate()
            .map(|(i, it)| (it.guid.clone(), i))
            .collect();

        let mut queued = 0usize;
        let mut skipped = 0usize;
        let mut updated = 0usize;
        let mut errors: Vec<RssActionErrorItem> = Vec::new();

        match action {
            RssItemAction::Ignore | RssItemAction::MarkNew => {
                let next_status = if action == RssItemAction::Ignore {
                    RssItemStatus::Ignored
                } else {
                    RssItemStatus::New
                };
                for guid in guids {
                    if let Some(&idx) = idx_by_guid.get(guid) {
                        items[idx].status = next_status;
                        items[idx].reason = RssRejectReason::None;
                        updated += 1;
                    }
                }
            }
            RssItemAction::Download => {
                let target_indices: Vec<usize> = guids
                    .iter()
                    .filter_map(|g| idx_by_guid.get(g).copied())
                    .collect();

                // Process with bounded concurrency = DOWNLOAD_CONCURRENCY (3)
                let sem = Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_CONCURRENCY));
                let mut handles = Vec::with_capacity(target_indices.len());
                for idx in target_indices {
                    let sem = sem.clone();
                    let deps = self.deps.clone();
                    let src_clone = source.clone();
                    let mut item_clone = items[idx].clone();
                    let tok = cancel.clone();
                    handles.push(tokio::spawn(async move {
                        let _permit = sem.acquire().await.ok();
                        if item_clone.status == RssItemStatus::Downloaded
                            && !item_clone.task_url.is_empty()
                            && deps.is_task_done_on_disk(&item_clone.task_url).await
                        {
                            return (idx, Ok((EnqueueOutcome::Skipped, item_clone)));
                        }
                        let res = async {
                            let resolved =
                                resolve_rss_item_media(&item_clone, &src_clone, &tok).await?;
                            let filename = make_safe_feed_slug(&item_clone.title, &item_clone.guid);
                            let outcome = deps
                                .enqueue_task(EnqueueRssTaskInput {
                                    url: resolved.media_url.clone(),
                                    filename,
                                    save_dir: (!src_clone.save_dir.is_empty())
                                        .then(|| src_clone.save_dir.clone()),
                                    headers: resolved.headers,
                                    source_url: item_clone.link.clone(),
                                    scrape_meta: RssSubscriptionService::build_scrape_meta(
                                        &src_clone,
                                        &item_clone,
                                        &resolved.referer,
                                    ),
                                })
                                .await?;
                            item_clone.status = RssItemStatus::Downloaded;
                            item_clone.task_url = resolved.media_url;
                            item_clone.reason = RssRejectReason::None;
                            Ok::<_, CoreError>((outcome, item_clone))
                        }
                        .await;
                        (idx, res)
                    }));
                }

                for h in handles {
                    let (idx, res) = h.await.map_err(|e| CoreError::msg(e.to_string()))?;
                    match res {
                        Ok((outcome, updated_item)) => {
                            items[idx] = updated_item;
                            if outcome == EnqueueOutcome::Queued {
                                queued += 1;
                            } else {
                                skipped += 1;
                            }
                            updated += 1;
                        }
                        Err(err) => {
                            errors.push(RssActionErrorItem {
                                guid: items[idx].guid.clone(),
                                message: err.to_string(),
                            });
                        }
                    }
                }
            }
        }

        if self.sources.lock().await.contains_key(key) {
            self.store.replace_rss_items(key, &items)?;
            self.sync_counters_and_save(key).await?;
            self.notify_change().await;
        }

        Ok(RssActionResult {
            queued,
            skipped,
            updated,
            errors,
        })
    }

    pub async fn mark_all_read(&self, id: &str) -> CoreResult<(usize, RssSource)> {
        let key = id.trim();
        if !self.sources.lock().await.contains_key(key) {
            return Err(CoreError::msg("订阅源不存在"));
        }
        let mut items = self.store.load_rss_items(key)?;
        let mut updated = 0usize;
        for it in &mut items {
            if it.status == RssItemStatus::New {
                it.status = RssItemStatus::Ignored;
                it.reason = RssRejectReason::None;
                updated += 1;
            }
        }
        self.store.replace_rss_items(key, &items)?;
        let src = self
            .sync_counters_and_save(key)
            .await?
            .ok_or_else(|| CoreError::msg("订阅源不存在"))?;
        self.notify_change().await;
        Ok((updated, src))
    }
}

fn combine_tokens(a: &CancellationToken, b: &CancellationToken) -> CancellationToken {
    let combined = CancellationToken::new();
    let a = a.clone();
    let b = b.clone();
    let c = combined.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = a.cancelled() => c.cancel(),
            _ = b.cancelled() => c.cancel(),
            _ = c.cancelled() => {}
        }
    });
    combined
}
