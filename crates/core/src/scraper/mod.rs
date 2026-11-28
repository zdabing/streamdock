pub mod artwork;
pub mod detail;
pub mod matcher;
pub mod nfo;

use crate::contract::{
    ScrapeCandidate, ScrapeDataSource, ScrapeFolderTemplate, ScrapeItemOutcome, ScrapeItemTiming,
    ScrapeMode, ScrapeOutcomeStatus, ScrapePosterCrop, ScrapePreviewItem, ScrapePreviewStatus,
    ScrapeRunItemInput, ScrapeRunRequest, ScrapeRunResult, ScrapeScanRequest, ScrapeScanResult,
    ScrapeScanSourceSummary, ScrapeSourceType, ScrapeTransferActual, ScrapeTransferMode,
};
use crate::error::{CoreError, CoreResult};
use crate::scraper::artwork::{SaveArtworkOptions, save_artwork, write_atomic_bytes};
use crate::scraper::detail::{DetailFallback, fetch_detail_metadata};
use crate::scraper::matcher::{
    RssCatalog, WpPostItem, extract_search_queries, load_rss_catalog, match_title_against_catalog,
    resolve_wordpress_api_url, score_wordpress_item, search_wordpress_posts,
};
use crate::scraper::nfo::{NfoArtworkNames, render_emby_nfo};
use crate::store::{default_rss_url, default_scrape_sources};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use unicode_normalization::UnicodeNormalization;
use url::Url;

const MAX_SCAN_FILES: usize = 20_000;
const MAX_SCAN_DEPTH: usize = 6;
const SCRAPE_CONCURRENCY: usize = 3;
const WP_SCAN_CONCURRENCY: usize = 4;

fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        ".mp4" | ".mkv" | ".ts" | ".mov" | ".webm" | ".avi" | ".m4v" | ".flv" | ".wmv"
    )
}

fn is_subtitle_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        ".srt" | ".vtt" | ".ass" | ".ssa"
    )
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        "@eaDir" | "#recycle" | ".git" | "node_modules" | ".DS_Store"
    )
}

#[derive(Debug, Clone)]
struct ScannedFile {
    file_path: String,
    relative_path: String,
    filename: String,
    size: u64,
    has_nfo: bool,
}

fn canonical_path_key(path: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = abs.to_string_lossy();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s.into_owned()
    }
}

fn collect_videos_recursive(
    dir: &Path,
    root: &Path,
    dir_label: &str,
    recursive: bool,
    depth: usize,
    remaining_limit: &mut usize,
    seen_files: &mut HashSet<String>,
    out: &mut Vec<ScannedFile>,
) {
    if depth > MAX_SCAN_DEPTH || *remaining_limit == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut file_names = HashSet::new();
    let mut subdirs = Vec::new();
    let mut videos = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.ends_with(".part") || name.ends_with(".tmp") {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            if recursive && !is_ignored_dir(&name) {
                subdirs.push(entry.path());
            }
        } else if ft.is_file() {
            file_names.insert(name.clone());
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                let dot_ext = format!(".{ext}");
                if is_video_ext(&dot_ext) {
                    videos.push((name, path));
                }
            }
        }
    }

    let has_movie_nfo = file_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case("movie.nfo"));
    for (name, full_path) in videos {
        if *remaining_limit == 0 {
            break;
        }
        let key = canonical_path_key(&full_path);
        if !seen_files.insert(key) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&full_path) else {
            continue;
        };
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&name);
        let has_sidecar_nfo = file_names.contains(&format!("{stem}.nfo"));
        let rel = full_path
            .strip_prefix(root)
            .unwrap_or(&full_path)
            .to_string_lossy()
            .replace('\\', "/");
        let relative_path = if dir_label.is_empty() {
            rel
        } else {
            format!("{dir_label}/{rel}")
        };
        out.push(ScannedFile {
            file_path: full_path.to_string_lossy().into_owned(),
            relative_path,
            filename: name,
            size: meta.len(),
            has_nfo: has_sidecar_nfo
                || (videos_len_is_one(&file_names, &full_path) && has_movie_nfo),
        });
        *remaining_limit = remaining_limit.saturating_sub(1);
    }

    if recursive {
        for sub in subdirs {
            if *remaining_limit == 0 {
                break;
            }
            collect_videos_recursive(
                &sub,
                root,
                dir_label,
                recursive,
                depth + 1,
                remaining_limit,
                seen_files,
                out,
            );
        }
    }
}

fn videos_len_is_one(file_names: &HashSet<String>, _current: &Path) -> bool {
    let count = file_names
        .iter()
        .filter(|n| {
            Path::new(n)
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|e| is_video_ext(&format!(".{e}")))
        })
        .count();
    count == 1
}

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub cache_dir: Option<PathBuf>,
    pub log: bool,
}

pub async fn scan_for_scrape(
    req: &ScrapeScanRequest,
    opts: &ScanOptions,
    cancel: &CancellationToken,
) -> CoreResult<ScrapeScanResult> {
    let scan_start = std::time::Instant::now();
    let raw_dirs: Vec<String> = match req.scan_dirs {
        Some(ref list) if !list.is_empty() => list.clone(),
        _ => vec![
            req.scan_dir
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("./downloads")
                .to_string(),
        ],
    };

    let mut seen_dir_keys = HashSet::new();
    let mut scan_dirs = Vec::new();
    for raw in raw_dirs {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        let abs = crate::format_options::absolutize(Path::new(t));
        let key = {
            let s = abs.to_string_lossy();
            if cfg!(windows) {
                s.to_ascii_lowercase()
            } else {
                s.into_owned()
            }
        };
        if seen_dir_keys.insert(key) {
            scan_dirs.push(abs);
        }
    }
    if scan_dirs.is_empty() {
        return Err(CoreError::msg("请填写至少一个扫描目录"));
    }

    for dir in &scan_dirs {
        if !tokio::fs::metadata(dir).await.is_ok_and(|m| m.is_dir()) {
            return Err(CoreError::msg(format!(
                "扫描目录不存在或不是文件夹：{}",
                dir.display()
            )));
        }
    }

    let enabled_sources: Vec<ScrapeDataSource> = if let Some(ref srcs) = req.sources {
        if !srcs.is_empty() {
            let mut out = Vec::new();
            for (idx, s) in srcs.iter().enumerate() {
                if !s.enabled || s.url.trim().is_empty() {
                    continue;
                }
                let url_trim = s.url.trim();
                let parsed = Url::parse(url_trim).map_err(|_| CoreError::msg("数据源地址无效"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(CoreError::msg("数据源地址应以 http:// 或 https:// 开头"));
                }
                if s.source_type == ScrapeSourceType::Wordpress {
                    resolve_wordpress_api_url(url_trim)?;
                }
                out.push(ScrapeDataSource {
                    id: if s.id.trim().is_empty() {
                        format!("src-{}", idx + 1)
                    } else {
                        s.id.trim().to_string()
                    },
                    name: s.name.trim().to_string(),
                    source_type: s.source_type,
                    url: parsed.to_string(),
                    enabled: true,
                    referer: Some(s.referer.as_deref().unwrap_or("").trim().to_string()),
                });
            }
            if out.is_empty() {
                return Err(CoreError::msg("请至少启用一个元数据数据源"));
            }
            out
        } else {
            vec![ScrapeDataSource {
                id: "rss-default".to_string(),
                name: String::new(),
                source_type: ScrapeSourceType::Rss,
                url: req
                    .rss_url
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(default_rss_url),
                enabled: true,
                referer: req.referer.clone(),
            }]
        }
    } else {
        vec![ScrapeDataSource {
            id: "rss-default".to_string(),
            name: String::new(),
            source_type: ScrapeSourceType::Rss,
            url: req
                .rss_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(default_rss_url),
            enabled: true,
            referer: req.referer.clone(),
        }]
    };

    let recursive = req.recursive.unwrap_or(true);
    let skip_existing_nfo = req.skip_existing_nfo.unwrap_or(true);
    let min_score = req.min_score.unwrap_or(55).clamp(0, 100);
    let refresh_rss = req.refresh_rss.unwrap_or(false);

    // Parallel preload of RSS catalogs + local directory walk.
    let rss_start = std::time::Instant::now();
    let rss_sources: Vec<ScrapeDataSource> = enabled_sources
        .iter()
        .filter(|s| s.source_type == ScrapeSourceType::Rss)
        .cloned()
        .collect();

    let mut rss_handles = Vec::with_capacity(rss_sources.len());
    for rs in rss_sources {
        let cache_dir = opts.cache_dir.clone();
        let tok = cancel.clone();
        rss_handles.push(tokio::spawn(async move {
            let res = load_rss_catalog(&rs.url, cache_dir.as_deref(), refresh_rss, &tok).await;
            (rs.id, res)
        }));
    }

    let walk_start = std::time::Instant::now();
    let scan_dirs_clone = scan_dirs.clone();
    let files = tokio::task::spawn_blocking(move || {
        let mut dir_labels = vec![String::new(); scan_dirs_clone.len()];
        if scan_dirs_clone.len() > 1 {
            let mut base_counts: HashMap<String, usize> = HashMap::new();
            for d in &scan_dirs_clone {
                let b = d
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| d.to_string_lossy().into_owned());
                *base_counts.entry(b).or_insert(0) += 1;
            }
            for (i, d) in scan_dirs_clone.iter().enumerate() {
                let b = d
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| d.to_string_lossy().into_owned());
                if base_counts.get(&b).copied().unwrap_or(0) > 1 {
                    let parent = d
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    dir_labels[i] = if parent.is_empty() {
                        b
                    } else {
                        format!("{parent}/{b}")
                    };
                } else {
                    dir_labels[i] = b;
                }
            }
        }
        let mut remaining = MAX_SCAN_FILES;
        let mut seen_files = HashSet::new();
        let mut collected = Vec::new();
        for (i, dir) in scan_dirs_clone.iter().enumerate() {
            collect_videos_recursive(
                dir,
                dir,
                &dir_labels[i],
                recursive,
                0,
                &mut remaining,
                &mut seen_files,
                &mut collected,
            );
        }
        collected
    })
    .await
    .map_err(|e| CoreError::msg(e.to_string()))?;
    let walk_ms = walk_start.elapsed().as_millis() as u64;

    let mut rss_map: HashMap<String, Result<Arc<RssCatalog>, String>> = HashMap::new();
    for h in rss_handles {
        let (id, res) = h.await.map_err(|e| CoreError::msg(e.to_string()))?;
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        rss_map.insert(id, res.map_err(|e| e.to_string()));
    }
    let rss_ms = rss_start.elapsed().as_millis() as u64;

    #[derive(Clone)]
    struct RankedCand {
        cand: ScrapeCandidate,
        source_idx: usize,
    }
    #[derive(Default)]
    struct FileState {
        candidates: Vec<RankedCand>,
        winner: Option<RankedCand>,
        locked: bool,
    }

    let mut states: Vec<FileState> = (0..files.len()).map(|_| FileState::default()).collect();
    let mut summaries: Vec<ScrapeScanSourceSummary> = Vec::new();

    for (s_idx, src) in enabled_sources.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let mut summary = ScrapeScanSourceSummary {
            id: src.id.clone(),
            name: if src.name.is_empty() {
                Url::parse(&src.url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_else(|| src.url.clone())
            } else {
                src.name.clone()
            },
            source_type: src.source_type,
            url: src.url.clone(),
            catalog_count: 0,
            queried_count: 0,
            matched_count: 0,
            error: None,
        };

        if src.source_type == ScrapeSourceType::Rss {
            match rss_map.get(&src.id) {
                Some(Ok(catalog)) => {
                    if src.name.is_empty() && !catalog.feed_title.is_empty() {
                        summary.name = catalog.feed_title.clone();
                    }
                    summary.catalog_count = catalog.items.len();
                    summary.queried_count = files.len();

                    for (f_idx, file) in files.iter().enumerate() {
                        let cands = match_title_against_catalog(&file.filename, catalog, 3);
                        let mut tagged = Vec::with_capacity(cands.len());
                        for mut c in cands {
                            c.source_id = Some(src.id.clone());
                            c.source_name = Some(summary.name.clone());
                            c.source_type = Some(ScrapeSourceType::Rss);
                            if let Some(ref r) = src.referer {
                                if !r.is_empty() {
                                    c.referer = Some(r.clone());
                                }
                            }
                            tagged.push(RankedCand {
                                cand: c,
                                source_idx: s_idx,
                            });
                        }
                        if !tagged.is_empty() {
                            let st = &mut states[f_idx];
                            st.candidates.extend(tagged.clone());
                            let skip_nfo = file.has_nfo && skip_existing_nfo;
                            if !skip_nfo && !st.locked && tagged[0].cand.score >= min_score {
                                st.winner = Some(tagged[0].clone());
                                st.locked = true;
                                summary.matched_count += 1;
                            }
                        }
                    }
                }
                Some(Err(err_msg)) => {
                    summary.error = Some(err_msg.clone());
                }
                None => {}
            }
            summaries.push(summary);
            continue;
        }

        // WordPress source
        let api_url = match resolve_wordpress_api_url(&src.url) {
            Ok(u) => u,
            Err(e) => {
                summary.error = Some(e.to_string());
                summaries.push(summary);
                continue;
            }
        };
        let pending_indices: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(idx, f)| !(f.has_nfo && skip_existing_nfo) && !states[*idx].locked)
            .map(|(idx, _)| idx)
            .collect();

        if !pending_indices.is_empty() {
            type QueryResult = Result<(Vec<WpPostItem>, Option<usize>), String>;
            let query_cache: Arc<Mutex<HashMap<String, QueryResult>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let sem = Arc::new(tokio::sync::Semaphore::new(WP_SCAN_CONCURRENCY));
            let mut handles = Vec::with_capacity(pending_indices.len());
            let eff_ref = src
                .referer
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            for f_idx in pending_indices.clone() {
                let sem = sem.clone();
                let filename = files[f_idx].filename.clone();
                let api_url = api_url.clone();
                let eff_ref = eff_ref.clone();
                let query_cache = query_cache.clone();
                let tok = cancel.clone();

                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.ok();
                    let queries = extract_search_queries(&filename);
                    let mut by_url: HashMap<String, (WpPostItem, u32)> = HashMap::new();
                    let mut ok_q = 0usize;
                    let mut fail_q = 0usize;
                    let mut last_err = String::new();
                    let mut max_total: Option<usize> = None;

                    for q in queries {
                        if tok.is_cancelled() {
                            return Err(CoreError::aborted());
                        }
                        let q_key = q.to_ascii_lowercase();
                        let cached_res = { query_cache.lock().await.get(&q_key).cloned() };
                        let q_res = match cached_res {
                            Some(r) => r,
                            None => {
                                let fetched =
                                    search_wordpress_posts(&api_url, &q, eff_ref.as_deref(), &tok)
                                        .await;
                                if tok.is_cancelled() {
                                    return Err(CoreError::aborted());
                                }
                                let mapped = fetched.map_err(|e| e.to_string());
                                query_cache.lock().await.insert(q_key, mapped.clone());
                                mapped
                            }
                        };

                        match q_res {
                            Ok((items, total)) => {
                                ok_q += 1;
                                if let Some(t) = total {
                                    max_total = Some(max_total.map_or(t, |cur| cur.max(t)));
                                }
                                let mut hit_strong = false;
                                for post in items {
                                    let s = score_wordpress_item(&filename, &post);
                                    if s >= 25 {
                                        let replace = by_url
                                            .get(&post.page_url)
                                            .is_none_or(|(_, prev)| s > *prev);
                                        if replace {
                                            by_url.insert(post.page_url.clone(), (post, s));
                                        }
                                        if s >= min_score {
                                            hit_strong = true;
                                        }
                                    }
                                }
                                if hit_strong {
                                    break;
                                }
                            }
                            Err(msg) => {
                                fail_q += 1;
                                last_err = msg;
                            }
                        }
                    }

                    let mut scored: Vec<(WpPostItem, u32)> = by_url.into_values().collect();
                    scored.sort_by(|a, b| b.1.cmp(&a.1));
                    scored.truncate(3);
                    Ok((f_idx, scored, ok_q, fail_q, last_err, max_total))
                }));
            }

            let mut total_ok_q = 0usize;
            let mut total_fail_q = 0usize;
            let mut last_err = String::new();

            for h in handles {
                let res = h.await.map_err(|e| CoreError::msg(e.to_string()))??;
                let (f_idx, scored, ok_q, fail_q, err_msg, max_total) = res;
                summary.queried_count += 1;
                total_ok_q += ok_q;
                total_fail_q += fail_q;
                if !err_msg.is_empty() {
                    last_err = err_msg;
                }
                if let Some(t) = max_total {
                    summary.catalog_count = summary.catalog_count.max(t);
                }

                let mut tagged = Vec::with_capacity(scored.len());
                for (post, score) in scored {
                    let cand = ScrapeCandidate {
                        guid: post.guid,
                        title: post.title,
                        page_url: post.page_url,
                        pub_date: post.pub_date,
                        tags: post.tags,
                        score,
                        source_id: Some(src.id.clone()),
                        source_name: Some(summary.name.clone()),
                        source_type: Some(ScrapeSourceType::Wordpress),
                        actors: (!post.actors.is_empty()).then_some(post.actors),
                        studio: (!post.studio.is_empty()).then_some(post.studio),
                        cover_url: (!post.cover_url.is_empty()).then_some(post.cover_url),
                        referer: eff_ref.clone(),
                    };
                    tagged.push(RankedCand {
                        cand,
                        source_idx: s_idx,
                    });
                }

                if !tagged.is_empty() {
                    let st = &mut states[f_idx];
                    st.candidates.extend(tagged.clone());
                    if !st.locked && tagged[0].cand.score >= min_score {
                        st.winner = Some(tagged[0].clone());
                        st.locked = true;
                        summary.matched_count += 1;
                    }
                }
            }

            if total_ok_q == 0 && total_fail_q > 0 {
                summary.error = Some(if last_err.is_empty() {
                    "WordPress 检索失败".to_string()
                } else {
                    last_err
                });
            }
        }

        summaries.push(summary);
    }

    if !summaries.is_empty() && summaries.iter().all(|s| s.error.is_some()) {
        if summaries.len() == 1 {
            return Err(CoreError::msg(
                summaries[0].error.clone().unwrap_or_default(),
            ));
        }
        let joined: Vec<String> = summaries
            .iter()
            .map(|s| format!("{}: {}", s.name, s.error.as_deref().unwrap_or("")))
            .collect();
        return Err(CoreError::msg(joined.join("；")));
    }

    let cand_limit = if enabled_sources.len() > 1 { 5 } else { 3 };
    let mut matched_count = 0usize;
    let mut items = Vec::with_capacity(files.len());

    for (idx, file) in files.into_iter().enumerate() {
        let st = &mut states[idx];
        let mut by_url: HashMap<String, RankedCand> = HashMap::new();
        for rc in st.candidates.drain(..) {
            let replace = match by_url.get(&rc.cand.page_url) {
                None => true,
                Some(prev) => {
                    rc.cand.score > prev.cand.score
                        || (rc.cand.score == prev.cand.score && rc.source_idx < prev.source_idx)
                }
            };
            if replace {
                by_url.insert(rc.cand.page_url.clone(), rc);
            }
        }
        let mut merged: Vec<RankedCand> = by_url.into_values().collect();
        let winner_url = st.winner.as_ref().map(|w| w.cand.page_url.clone());
        merged.sort_by(|a, b| {
            let a_win = winner_url.as_deref() == Some(a.cand.page_url.as_str());
            let b_win = winner_url.as_deref() == Some(b.cand.page_url.as_str());
            if a_win && !b_win {
                return std::cmp::Ordering::Less;
            }
            if b_win && !a_win {
                return std::cmp::Ordering::Greater;
            }
            b.cand
                .score
                .cmp(&a.cand.score)
                .then_with(|| a.source_idx.cmp(&b.source_idx))
        });
        let candidates: Vec<ScrapeCandidate> = merged
            .into_iter()
            .take(cand_limit)
            .map(|r| r.cand)
            .collect();
        let top = candidates.first().cloned();
        let status = if file.has_nfo && skip_existing_nfo {
            ScrapePreviewStatus::SkippedNfo
        } else if top.as_ref().is_some_and(|t| t.score >= min_score) {
            matched_count += 1;
            ScrapePreviewStatus::Matched
        } else if top.as_ref().is_some_and(|t| t.score >= 30) {
            ScrapePreviewStatus::LowConfidence
        } else {
            ScrapePreviewStatus::Unmatched
        };

        items.push(ScrapePreviewItem {
            file_path: file.file_path,
            relative_path: file.relative_path,
            filename: file.filename,
            size: file.size,
            has_nfo: file.has_nfo,
            status,
            match_item: if status == ScrapePreviewStatus::SkippedNfo {
                None
            } else {
                top
            },
            candidates,
        });
    }

    if opts.log {
        let dir_str: Vec<String> = scan_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        let src_log: Vec<String> = summaries
            .iter()
            .map(|s| {
                if let Some(ref e) = s.error {
                    format!("{}[异常:{e}]", s.name)
                } else if s.source_type == ScrapeSourceType::Rss {
                    format!("{}[{}条,命中{}]", s.name, s.catalog_count, s.matched_count)
                } else {
                    format!(
                        "{}[检索{},命中{}]",
                        s.name, s.queried_count, s.matched_count
                    )
                }
            })
            .collect();
        info!(
            "[刮削扫描] 目录={} | RSS加载={}ms | 遍历={}ms({}个视频) | 高置信={} | 数据源={} | 总计={}ms",
            dir_str.join(", "),
            rss_ms,
            walk_ms,
            items.len(),
            matched_count,
            src_log.join(" -> "),
            scan_start.elapsed().as_millis()
        );
    }

    let rss_catalog_count: usize = summaries
        .iter()
        .filter(|s| s.source_type == ScrapeSourceType::Rss)
        .map(|s| s.catalog_count)
        .sum();
    let wp_catalog_count: usize = summaries
        .iter()
        .filter(|s| s.source_type == ScrapeSourceType::Wordpress)
        .map(|s| s.catalog_count)
        .sum();
    let feed_title = if summaries.len() == 1 {
        summaries[0].name.clone()
    } else {
        summaries
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let primary_rss_url = summaries
        .iter()
        .find(|s| s.source_type == ScrapeSourceType::Rss)
        .or_else(|| summaries.first())
        .map(|s| s.url.clone())
        .unwrap_or_else(default_rss_url);

    let dir_strings: Vec<String> = scan_dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();

    Ok(ScrapeScanResult {
        scan_dir: dir_strings[0].clone(),
        scan_dirs: dir_strings,
        rss_url: primary_rss_url,
        feed_title,
        catalog_count: if rss_catalog_count > 0 {
            rss_catalog_count
        } else {
            wp_catalog_count
        },
        scanned_files: items.len(),
        matched_count,
        source_summaries: Some(summaries),
        items,
    })
}

pub fn sanitize_path_segment(value: &str, fallback: &str, max_len: usize) -> String {
    static WS_RX: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s+").unwrap());
    let nfkc: String = value.nfkc().collect();
    let mut cleaned = String::with_capacity(nfkc.len());
    for ch in nfkc.chars() {
        if (ch as u32) < 0x20 || "<>:\"/\\|?*".contains(ch) {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
    }
    let collapsed = WS_RX.replace_all(&cleaned, " ").trim().to_string();
    let sliced: String = collapsed.chars().take(max_len).collect();
    let out = sliced.trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

async fn resolve_unique_target_path(dir: &Path, stem: &str, ext: &str, src_path: &Path) -> PathBuf {
    let src_abs = crate::format_options::absolutize(src_path);
    for idx in 1..1000usize {
        let name = if idx == 1 {
            format!("{stem}{ext}")
        } else {
            format!("{stem} ({idx}){ext}")
        };
        let cand = crate::format_options::absolutize(&dir.join(name));
        if cand == src_abs || !tokio::fs::try_exists(&cand).await.unwrap_or(false) {
            return cand;
        }
    }
    dir.join(format!(
        "{stem}-{}{ext}",
        chrono::Utc::now().timestamp_millis()
    ))
}

fn is_cross_fs_err(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::CrossesDevices
        || err.raw_os_error() == Some(18)
        || err.raw_os_error() == Some(17)
}

async fn transfer_file(
    src: &Path,
    dst: &Path,
    mode: ScrapeTransferMode,
) -> CoreResult<ScrapeTransferActual> {
    if crate::format_options::absolutize(src) == crate::format_options::absolutize(dst) {
        return Ok(ScrapeTransferActual::None);
    }
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match mode {
        ScrapeTransferMode::Copy => {
            tokio::fs::copy(src, dst).await?;
            Ok(ScrapeTransferActual::Copy)
        }
        ScrapeTransferMode::Hardlink => match tokio::fs::hard_link(src, dst).await {
            Ok(()) => Ok(ScrapeTransferActual::Hardlink),
            Err(e) if is_cross_fs_err(&e) => Err(CoreError::msg(
                "源视频与目标归档目录不在同一个文件系统/挂载卷，无法创建硬链接",
            )),
            Err(e) => Err(CoreError::Io(e)),
        },
        ScrapeTransferMode::Move => match tokio::fs::rename(src, dst).await {
            Ok(()) => Ok(ScrapeTransferActual::Rename),
            Err(e) if is_cross_fs_err(&e) => {
                let tmp = PathBuf::from(format!(
                    "{}.{}.{}.tmp",
                    dst.display(),
                    std::process::id(),
                    chrono::Utc::now().timestamp_millis()
                ));
                let copy_res: CoreResult<()> = async {
                    tokio::fs::copy(src, &tmp).await?;
                    tokio::fs::rename(&tmp, dst).await?;
                    tokio::fs::remove_file(src).await?;
                    Ok(())
                }
                .await;
                if copy_res.is_err() {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    copy_res?;
                }
                Ok(ScrapeTransferActual::CopyCrossFs)
            }
            Err(e) => Err(CoreError::Io(e)),
        },
    }
}

async fn transfer_sidecar_subtitles(
    src_video_path: &Path,
    target_dir: &Path,
    target_stem: &str,
    mode: ScrapeTransferMode,
) {
    let Some(src_dir) = src_video_path.parent() else {
        return;
    };
    let src_stem = src_video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let Ok(mut rd) = tokio::fs::read_dir(src_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let Ok(ft) = entry.file_type().await else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext_raw) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        let ext = format!(".{ext_raw}");
        if !is_subtitle_ext(&ext) {
            continue;
        }
        let base = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if base != src_stem && !base.starts_with(&format!("{src_stem}.")) {
            continue;
        }
        let suffix = &base[src_stem.len()..];
        let dst =
            resolve_unique_target_path(target_dir, &format!("{target_stem}{suffix}"), &ext, &path)
                .await;
        let _ = transfer_file(&path, &dst, mode).await;
    }
}

pub type OnScrapeItemStart = Arc<dyn Fn(&ScrapeRunItemInput, usize, usize) + Send + Sync>;
pub type OnScrapeItemDone = Arc<dyn Fn(&ScrapeItemOutcome, usize, usize) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ScrapeRunOptions {
    pub ffmpeg_path: String,
    pub log: bool,
    pub on_item_start: Option<OnScrapeItemStart>,
    pub on_item_done: Option<OnScrapeItemDone>,
}

pub async fn run_media_scrape(
    req: &ScrapeRunRequest,
    opts: &ScrapeRunOptions,
    cancel: &CancellationToken,
) -> CoreResult<ScrapeRunResult> {
    if req.items.is_empty() {
        return Err(CoreError::msg("请选择至少一个需要刮削的视频文件"));
    }

    let mode = req.mode;
    let transfer_mode = req.transfer_mode.unwrap_or(ScrapeTransferMode::Move);
    let folder_template = req
        .folder_template
        .unwrap_or(ScrapeFolderTemplate::ActorTitle);
    let poster_crop = req.poster_crop.unwrap_or(ScrapePosterCrop::Center);
    let download_images = req.download_images.unwrap_or(true);
    let overwrite_nfo = req.overwrite_nfo.unwrap_or(false);
    let output_dir_str = req.output_dir.as_deref().unwrap_or("").trim();
    let output_dir = if output_dir_str.is_empty() {
        PathBuf::new()
    } else {
        crate::format_options::absolutize(Path::new(output_dir_str))
    };
    if mode == ScrapeMode::Organize && output_dir.as_os_str().is_empty() {
        return Err(CoreError::msg("归档整理模式需要填写目标输出目录"));
    }
    let concurrency = req.concurrency.unwrap_or(SCRAPE_CONCURRENCY).clamp(1, 10);

    let total_items = req.items.len();
    let batch_start = std::time::Instant::now();
    let completed_count = Arc::new(AtomicUsize::new(0));

    if opts.log {
        let mode_str = match mode {
            ScrapeMode::InPlace => "in-place".to_string(),
            ScrapeMode::Organize => {
                let tm = match transfer_mode {
                    ScrapeTransferMode::Move => "move",
                    ScrapeTransferMode::Hardlink => "hardlink",
                    ScrapeTransferMode::Copy => "copy",
                };
                format!("organize({tm} -> {})", output_dir.display())
            }
        };
        let poster_str = if !download_images {
            "off"
        } else {
            match poster_crop {
                ScrapePosterCrop::Center => "center",
                ScrapePosterCrop::Right => "right",
                ScrapePosterCrop::None => "none",
            }
        };
        info!(
            "[刮削开始] 共 {} 个 | 模式={} | 并发={} | 海报={}",
            total_items, mode_str, concurrency, poster_str
        );
    }

    let cursor = Arc::new(AtomicUsize::new(0));
    let items_arc = Arc::new(req.items.clone());
    let results_slots: Arc<std::sync::Mutex<Vec<Option<ScrapeItemOutcome>>>> =
        Arc::new(std::sync::Mutex::new(vec![None; total_items]));

    let workers = concurrency.min(total_items);
    let mut handles = Vec::with_capacity(workers);

    for _ in 0..workers {
        let cursor = cursor.clone();
        let items_arc = items_arc.clone();
        let results_slots = results_slots.clone();
        let tok = cancel.clone();
        let completed_count = completed_count.clone();
        let output_dir = output_dir.clone();
        let req_referer = req.referer.clone();
        let req_studio = req.studio.clone();
        let opts = opts.clone();

        handles.push(tokio::spawn(async move {
            loop {
                if tok.is_cancelled() {
                    break;
                }
                let index = cursor.fetch_add(1, Ordering::Relaxed);
                if index >= total_items {
                    break;
                }
                let item = &items_arc[index];
                if let Some(ref cb) = opts.on_item_start {
                    cb(item, index, total_items);
                }

                let src_path = crate::format_options::absolutize(Path::new(item.file_path.trim()));
                let file_label = src_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| item.file_path.clone());
                let total_start = std::time::Instant::now();
                let mut stage = "检查文件";
                let mut detail_ms = 0u64;
                let mut transfer_ms = 0u64;
                let mut artwork_ms = 0u64;
                let mut crop_ms = 0u64;
                let mut transfer_actual = ScrapeTransferActual::None;

                let work: CoreResult<ScrapeItemOutcome> = async {
                    if tok.is_cancelled() {
                        return Err(CoreError::aborted());
                    }
                    let meta_fs = tokio::fs::metadata(&src_path).await.ok();
                    if !meta_fs.is_some_and(|m| m.is_file()) {
                        return Err(CoreError::msg("源视频文件不存在"));
                    }
                    let src_ext = src_path
                        .extension()
                        .and_then(|s| s.to_str())
                        .map(|s| format!(".{s}"))
                        .unwrap_or_default();
                    if !is_video_ext(&src_ext) {
                        return Err(CoreError::msg("不支持的视频文件格式"));
                    }

                    if tok.is_cancelled() {
                        return Err(CoreError::aborted());
                    }
                    stage = "详情页";
                    let detail_start = std::time::Instant::now();
                    let item_referer = item.referer.clone().or_else(|| req_referer.clone());
                    let fb = DetailFallback {
                        title: item.title.clone(),
                        pub_date: item.pub_date.clone(),
                        tags: item.tags.clone(),
                        referer: item_referer.clone(),
                        studio: item.studio.clone().or_else(|| req_studio.clone()),
                        actors: item.actors.clone(),
                        cover_url: item.cover_url.clone(),
                    };
                    let meta = fetch_detail_metadata(&item.page_url, &fb, &tok).await?;
                    detail_ms = detail_start.elapsed().as_millis() as u64;

                    let mut target_video_path = src_path.clone();
                    let mut target_dir = src_path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from("."));
                    let mut file_stem = src_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("video")
                        .to_string();
                    let mut art_names = NfoArtworkNames {
                        poster: format!("{file_stem}-poster.jpg"),
                        fanart: format!("{file_stem}-fanart.jpg"),
                        thumb: format!("{file_stem}-thumb.jpg"),
                    };

                    if mode == ScrapeMode::Organize {
                        if tok.is_cancelled() {
                            return Err(CoreError::aborted());
                        }
                        stage = "移动/复制";
                        let tr_start = std::time::Instant::now();
                        let clean_title = sanitize_path_segment(&meta.title, &file_stem, 80);
                        let actor_raw = meta
                            .actors
                            .first()
                            .map(String::as_str)
                            .unwrap_or("未知演员");
                        let actor_folder = sanitize_path_segment(actor_raw, "未知演员", 40);
                        let movie_raw = if meta.premiered.is_empty() {
                            clean_title.clone()
                        } else {
                            format!("{} {clean_title}", meta.premiered)
                        };
                        let movie_folder = sanitize_path_segment(&movie_raw, &clean_title, 95);
                        target_dir = match folder_template {
                            ScrapeFolderTemplate::Title => output_dir.join(&movie_folder),
                            ScrapeFolderTemplate::ActorTitle => {
                                output_dir.join(&actor_folder).join(&movie_folder)
                            }
                        };
                        target_video_path = resolve_unique_target_path(
                            &target_dir,
                            &clean_title,
                            &src_ext,
                            &src_path,
                        )
                        .await;
                        file_stem = target_video_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&clean_title)
                            .to_string();
                        transfer_actual =
                            transfer_file(&src_path, &target_video_path, transfer_mode).await?;
                        transfer_sidecar_subtitles(
                            &src_path,
                            &target_dir,
                            &file_stem,
                            transfer_mode,
                        )
                        .await;
                        transfer_ms = tr_start.elapsed().as_millis() as u64;
                        art_names = NfoArtworkNames {
                            poster: "poster.jpg".to_string(),
                            fanart: "fanart.jpg".to_string(),
                            thumb: "thumb.jpg".to_string(),
                        };
                    }

                    let nfo_path = target_dir.join(format!("{file_stem}.nfo"));
                    let fanart_path = target_dir.join(&art_names.fanart);
                    let thumb_path = target_dir.join(&art_names.thumb);
                    let poster_path = target_dir.join(&art_names.poster);

                    if tok.is_cancelled() {
                        return Err(CoreError::aborted());
                    }
                    stage = "封面/裁图";
                    let saved_art = if download_images && !meta.cover_urls.is_empty() {
                        save_artwork(
                            SaveArtworkOptions {
                                cover_url: &meta.cover_url,
                                cover_urls: &meta.cover_urls,
                                page_url: &meta.page_url,
                                referer: item_referer.as_deref(),
                                fanart_path: &fanart_path,
                                thumb_path: &thumb_path,
                                poster_path: &poster_path,
                                poster_crop,
                                overwrite: overwrite_nfo,
                                ffmpeg_path: &opts.ffmpeg_path,
                            },
                            &tok,
                        )
                        .await?
                    } else {
                        Default::default()
                    };
                    artwork_ms = saved_art.download_ms;
                    crop_ms = saved_art.crop_ms;

                    let nfo_art = NfoArtworkNames {
                        poster: if saved_art.poster_path.is_some() {
                            art_names.poster
                        } else {
                            String::new()
                        },
                        fanart: if saved_art.fanart_path.is_some() {
                            art_names.fanart
                        } else {
                            String::new()
                        },
                        thumb: if saved_art.thumb_path.is_some() {
                            art_names.thumb
                        } else {
                            String::new()
                        },
                    };

                    if tok.is_cancelled() {
                        return Err(CoreError::aborted());
                    }
                    stage = "写入NFO";
                    if overwrite_nfo || !tokio::fs::try_exists(&nfo_path).await.unwrap_or(false) {
                        let xml = render_emby_nfo(&meta, &nfo_art);
                        write_atomic_bytes(&nfo_path, xml.as_bytes()).await?;
                    }

                    let timing = ScrapeItemTiming {
                        detail_ms,
                        transfer_ms,
                        artwork_ms,
                        crop_ms,
                        total_ms: total_start.elapsed().as_millis() as u64,
                        transfer_actual: Some(transfer_actual),
                    };

                    let done_count = completed_count.fetch_add(1, Ordering::Relaxed) + 1;
                    if opts.log
                        && (total_items <= 100
                            || timing.total_ms >= 2000
                            || transfer_actual == ScrapeTransferActual::CopyCrossFs
                            || done_count == 1
                            || done_count == total_items
                            || done_count % 25 == 0)
                    {
                        let cross_tag = if transfer_actual == ScrapeTransferActual::CopyCrossFs {
                            "[跨卷复制]"
                        } else {
                            ""
                        };
                        info!(
                            "[刮削 {}/{}] {} ({}ms: 详情{}ms 文件{}ms{} 封面{}ms 裁图{}ms)",
                            done_count,
                            total_items,
                            file_label,
                            timing.total_ms,
                            detail_ms,
                            transfer_ms,
                            cross_tag,
                            artwork_ms,
                            crop_ms
                        );
                    }

                    Ok(ScrapeItemOutcome {
                        file_path: src_path.to_string_lossy().into_owned(),
                        target_video_path: Some(target_video_path.to_string_lossy().into_owned()),
                        nfo_path: Some(nfo_path.to_string_lossy().into_owned()),
                        poster_path: saved_art
                            .poster_path
                            .map(|p| p.to_string_lossy().into_owned()),
                        fanart_path: saved_art
                            .fanart_path
                            .map(|p| p.to_string_lossy().into_owned()),
                        thumb_path: saved_art
                            .thumb_path
                            .map(|p| p.to_string_lossy().into_owned()),
                        title: Some(meta.title),
                        actor: Some(meta.actors.first().cloned().unwrap_or_default()),
                        status: ScrapeOutcomeStatus::Success,
                        message: None,
                        failed_stage: None,
                        timing: Some(timing),
                    })
                }
                .await;

                let outcome = match work {
                    Ok(ok) => ok,
                    Err(err) => {
                        if tok.is_cancelled() || err.is_aborted() {
                            break;
                        }
                        let timing = ScrapeItemTiming {
                            detail_ms,
                            transfer_ms,
                            artwork_ms,
                            crop_ms,
                            total_ms: total_start.elapsed().as_millis() as u64,
                            transfer_actual: Some(transfer_actual),
                        };
                        let done_count = completed_count.fetch_add(1, Ordering::Relaxed) + 1;
                        let msg = err.to_string();
                        if opts.log {
                            warn!(
                                "[刮削失败 {}/{}] {} 阶段={} ({}ms): {}",
                                done_count, total_items, file_label, stage, timing.total_ms, msg
                            );
                        }
                        ScrapeItemOutcome {
                            file_path: src_path.to_string_lossy().into_owned(),
                            target_video_path: None,
                            nfo_path: None,
                            poster_path: None,
                            fanart_path: None,
                            thumb_path: None,
                            title: None,
                            actor: None,
                            status: ScrapeOutcomeStatus::Failed,
                            message: Some(msg),
                            failed_stage: Some(stage.to_string()),
                            timing: Some(timing),
                        }
                    }
                };

                let done_now = completed_count.load(Ordering::Relaxed);
                if let Some(ref cb) = opts.on_item_done {
                    cb(&outcome, done_now, total_items);
                }
                results_slots.lock().unwrap()[index] = Some(outcome);
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let results: Vec<ScrapeItemOutcome> = results_slots
        .lock()
        .unwrap()
        .iter()
        .filter_map(|o| o.clone())
        .collect();
    let succeeded = results
        .iter()
        .filter(|r| r.status == ScrapeOutcomeStatus::Success)
        .count();
    let failed = results.len() - succeeded;

    if opts.log {
        let count = results.len().max(1);
        let avg = |pick: fn(&ScrapeItemTiming) -> u64| -> u64 {
            let sum: u64 = results
                .iter()
                .map(|r| r.timing.as_ref().map_or(0, pick))
                .sum();
            (sum as f64 / count as f64).round() as u64
        };
        let cross_fs = results
            .iter()
            .filter(|r| {
                r.timing.as_ref().and_then(|t| t.transfer_actual)
                    == Some(ScrapeTransferActual::CopyCrossFs)
            })
            .count();
        let cross_suffix = if cross_fs > 0 {
            format!(" | ⚠️跨卷复制={cross_fs}个")
        } else {
            String::new()
        };
        let summary_timing = format!(
            "均值: 详情{}ms 文件{}ms 封面{}ms 裁图{}ms | 总耗时={}ms{}",
            avg(|t| t.detail_ms),
            avg(|t| t.transfer_ms),
            avg(|t| t.artwork_ms),
            avg(|t| t.crop_ms),
            batch_start.elapsed().as_millis(),
            cross_suffix
        );
        if cancel.is_cancelled() {
            info!(
                "[刮削已停止] 已完成 {}/{}（已处理 {}，失败 {}）| {}",
                succeeded,
                total_items,
                results.len(),
                failed,
                summary_timing
            );
        } else {
            info!(
                "[刮削汇总] 完成 {}/{}（失败 {}）| {}",
                succeeded,
                results.len(),
                failed,
                summary_timing
            );
        }
    }

    Ok(ScrapeRunResult {
        total: results.len(),
        succeeded,
        failed,
        results,
    })
}

pub fn get_default_scrape_sources() -> Vec<ScrapeDataSource> {
    default_scrape_sources()
}
