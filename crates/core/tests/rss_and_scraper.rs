use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, OnceCell};
use tokio_util::sync::CancellationToken;

use streamdock_core::contract::{
    RssItemStatus, RssRejectReason, RssSource, RssSourceInput, RssSourceMode, ScrapeDataSource,
    ScrapeFolderTemplate, ScrapeMode, ScrapeOutcomeStatus, ScrapePosterCrop, ScrapeRunItemInput,
    ScrapeRunRequest, ScrapeScanRequest, ScrapeSourceType, ScrapeTransferMode,
};
use streamdock_core::error::CoreResult;
use streamdock_core::merge::is_support_ffmpeg;
use streamdock_core::rss_filter::{
    RssFilterRule, RssFilterVerdict, compile_rss_filter, evaluate_rss_filter, extract_episode_key,
    parse_human_size,
};
use streamdock_core::rss_parser::{FetchFeedOptions, fetch_and_parse_feed};
use streamdock_core::rss_service::{
    EnqueueOutcome, EnqueueRssTaskInput, RssItemAction, RssSubscriptionDeps, RssSubscriptionService,
};
use streamdock_core::scraper::{ScanOptions, ScrapeRunOptions, run_media_scrape, scan_for_scrape};
use streamdock_core::store::{EnvConfigDefaults, Store};

static SAMPLE_PNG_AND_WEBP: OnceCell<Option<(Vec<u8>, Vec<u8>)>> = OnceCell::const_new();

async fn get_sample_images() -> Option<&'static (Vec<u8>, Vec<u8>)> {
    SAMPLE_PNG_AND_WEBP
        .get_or_init(|| async {
            if !is_support_ffmpeg("").await {
                return None;
            }
            let dir = tempfile::tempdir().ok()?;
            let jpg = dir.path().join("cover.jpg");
            let webp = dir.path().join("cover.webp");
            let st1 = tokio::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=blue:s=320x180",
                    "-frames:v",
                    "1",
                ])
                .arg(&jpg)
                .status()
                .await
                .ok()?;
            let st2 = tokio::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=green:s=320x180",
                    "-frames:v",
                    "1",
                ])
                .arg(&webp)
                .status()
                .await
                .ok()?;
            if !st1.success() || !st2.success() {
                return None;
            }
            Some((
                tokio::fs::read(&jpg).await.ok()?,
                tokio::fs::read(&webp).await.ok()?,
            ))
        })
        .await
        .as_ref()
}

async fn spawn_test_router(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, handle)
}

#[test]
fn rss_filter_episode_keys_size_and_rules() {
    assert_eq!(
        extract_episode_key("[Sub] Show Name S01E02 1080p"),
        "S01E02"
    );
    assert_eq!(extract_episode_key("动画名称 第12集 简日双语"), "E12");
    assert_eq!(extract_episode_key("Show EP05 HEVC"), "E05");
    assert_eq!(extract_episode_key("[Group] Title [08] [1080p]"), "E08");
    assert_eq!(extract_episode_key("Title - 09 [END]"), "E09");
    assert_eq!(extract_episode_key("1080p Full Movie"), "");

    assert_eq!(parse_human_size("1.5GB"), Some(1_500_000_000));
    assert_eq!(parse_human_size("1.5GiB"), Some(1_610_612_736));

    let rule = RssFilterRule {
        include_pattern: "1080p|2160p".to_string(),
        exclude_pattern: "cam|ts".to_string(),
        use_regex: false,
        smart_episode: true,
        size_min_bytes: 1000,
        size_max_bytes: 100_000,
    };
    let compiled = compile_rss_filter(&rule);
    let mut seen = HashSet::new();

    assert!(matches!(
        evaluate_rss_filter(&compiled, "Show [01] 1080p", 5000, Some(&mut seen)),
        RssFilterVerdict::Accepted { .. }
    ));
    // Duplicate episode E01 rejected by smart_episode
    assert!(matches!(
        evaluate_rss_filter(&compiled, "Show [01] 2160p", 5000, Some(&mut seen)),
        RssFilterVerdict::Rejected {
            status: RssItemStatus::DuplicateEpisode,
            reason: RssRejectReason::DupEpisode,
            ..
        }
    ));
    // Excluded keyword
    assert!(matches!(
        evaluate_rss_filter(&compiled, "Show [02] 1080p CAM", 5000, Some(&mut seen)),
        RssFilterVerdict::Rejected {
            status: RssItemStatus::Filtered,
            reason: RssRejectReason::Excluded,
            ..
        }
    ));
    // Too small
    assert!(matches!(
        evaluate_rss_filter(&compiled, "Show [03] 1080p", 500, Some(&mut seen)),
        RssFilterVerdict::Rejected {
            status: RssItemStatus::Filtered,
            reason: RssRejectReason::TooSmall,
            ..
        }
    ));
}

#[tokio::test]
async fn rss_parser_handles_rss2_atom_and_html_list_mode() {
    let rss2_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Sample RSS2</title>
  <item>
    <guid>g1</guid>
    <title>Episode 01 &amp; Demo</title>
    <link>/ep/1</link>
    <enclosure url="/cdn/ep1.mp4" length="2048" type="video/mp4"/>
    <category>TagA</category>
  </item>
</channel></rss>"#;

    let atom_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>Sample Atom</title>
  <entry>
    <id>atom-1</id>
    <title>Atom Clip</title>
    <link rel="alternate" href="/watch/a1"/>
    <link rel="enclosure" href="/media/a1.m3u8" length="4096"/>
  </entry>
</feed>"#;

    let html_list = r#"<html><title>Index List</title><body>
      <a href="/post/movie-101">Movie 101</a>
      <a href="/post/movie-102">Movie 102</a>
      <a href="/other/ignore">Other</a>
    </body></html>"#;

    let app = Router::new()
        .route("/rss.xml", get(move || async move { rss2_xml }))
        .route("/atom.xml", get(move || async move { atom_xml }))
        .route("/list.html", get(move || async move { html_list }));

    let (port, srv) = spawn_test_router(app).await;
    let cancel = CancellationToken::new();

    let r1 = fetch_and_parse_feed(
        &FetchFeedOptions {
            url: format!("http://127.0.0.1:{port}/rss.xml"),
            mode: RssSourceMode::Rss,
            item_pattern: String::new(),
            referer: String::new(),
            cookies: String::new(),
            user_agent: String::new(),
        },
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(r1.feed_title, "Sample RSS2");
    assert_eq!(r1.items.len(), 1);
    assert_eq!(r1.items[0].title, "Episode 01 & Demo");
    assert_eq!(
        r1.items[0].enclosure_url,
        format!("http://127.0.0.1:{port}/cdn/ep1.mp4")
    );

    let r2 = fetch_and_parse_feed(
        &FetchFeedOptions {
            url: format!("http://127.0.0.1:{port}/atom.xml"),
            mode: RssSourceMode::Rss,
            item_pattern: String::new(),
            referer: String::new(),
            cookies: String::new(),
            user_agent: String::new(),
        },
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(r2.feed_title, "Sample Atom");
    assert_eq!(
        r2.items[0].enclosure_url,
        format!("http://127.0.0.1:{port}/media/a1.m3u8")
    );

    let r3 = fetch_and_parse_feed(
        &FetchFeedOptions {
            url: format!("http://127.0.0.1:{port}/list.html"),
            mode: RssSourceMode::Html,
            item_pattern: "/post/".to_string(),
            referer: String::new(),
            cookies: String::new(),
            user_agent: String::new(),
        },
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(r3.items.len(), 2);
    srv.abort();
}

struct MockDeps {
    queued: Mutex<Vec<EnqueueRssTaskInput>>,
    changes: AtomicUsize,
}

impl RssSubscriptionDeps for MockDeps {
    fn enqueue_task(
        &self,
        input: EnqueueRssTaskInput,
    ) -> Pin<Box<dyn std::future::Future<Output = CoreResult<EnqueueOutcome>> + Send>> {
        // Note: RssSubscriptionDeps receives &self synchronously; record synchronously via try_lock
        if let Ok(mut q) = self.queued.try_lock() {
            q.push(input);
        }
        Box::pin(async { Ok(EnqueueOutcome::Queued) })
    }

    fn is_task_done_on_disk(
        &self,
        _task_url: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
        Box::pin(async { false })
    }

    fn on_change(&self, _sources: Vec<RssSource>) {
        self.changes.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn rss_subscription_service_lifecycle_seed_auto_queue_and_actions() {
    let phase = Arc::new(AtomicUsize::new(1));
    let app = Router::new().route(
        "/feed.xml",
        get({
            let ph = phase.clone();
            move || {
                let p = ph.load(Ordering::SeqCst);
                async move {
                    if p == 1 {
                        r#"<rss version="2.0"><channel><title>Feed</title>
                            <item><guid>id-1</guid><title>Show [01] 1080p</title><link>/p/1</link><enclosure url="/v/1.mp4" length="2000"/></item>
                        </channel></rss>"#.to_string()
                    } else {
                        r#"<rss version="2.0"><channel><title>Feed</title>
                            <item><guid>id-2</guid><title>Show [02] 1080p</title><link>/p/2</link><enclosure url="/v/2.mp4" length="2000"/></item>
                            <item><guid>id-1</guid><title>Show [01] 1080p</title><link>/p/1</link><enclosure url="/v/1.mp4" length="2000"/></item>
                        </channel></rss>"#.to_string()
                    }
                }
            }
        }),
    );
    let (port, srv) = spawn_test_router(app).await;

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), &EnvConfigDefaults::default()).unwrap();
    let deps = Arc::new(MockDeps {
        queued: Mutex::new(Vec::new()),
        changes: AtomicUsize::new(0),
    });
    let svc = RssSubscriptionService::new(store, deps.clone()).unwrap();

    let created = svc
        .create_source(&RssSourceInput {
            name: Some("MyFeed".to_string()),
            url: format!("http://127.0.0.1:{port}/feed.xml"),
            auto_download: Some(true),
            auto_scrape: Some(true),
            smart_episode: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

    // First refresh seeds history without auto-downloading id-1
    let r1 = svc
        .refresh_source(&created.id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r1.auto_queued, 0);
    assert_eq!(deps.queued.lock().await.len(), 0);

    // Second refresh discovers id-2 and auto-queues it with scrape_meta attached!
    phase.store(2, Ordering::SeqCst);
    let r2 = svc
        .refresh_source(&created.id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r2.added, 1);
    assert_eq!(r2.auto_queued, 1);
    let q = deps.queued.lock().await;
    assert_eq!(q.len(), 1);
    assert!(q[0].url.ends_with("/v/2.mp4"));
    assert!(q[0].scrape_meta.is_some());
    drop(q);

    // Manual Ignore + MarkNew + MarkAllRead
    let page = svc
        .query_items(&created.id, "all", "", false, 1, 20)
        .await
        .unwrap();
    assert_eq!(page.total, 2);

    let act_res = svc
        .act_on_items(
            &created.id,
            &["id-1".to_string()],
            RssItemAction::MarkNew,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(act_res.updated, 1);

    let (read_cnt, _) = svc.mark_all_read(&created.id).await.unwrap();
    assert_eq!(read_cnt, 1);

    svc.close().await;
    srv.abort();
}

#[tokio::test]
async fn scraper_scan_and_run_in_place_and_organize() {
    let Some((jpg_bytes, webp_bytes)) = get_sample_images().await else {
        eprintln!("SKIP: ffmpeg unavailable for scraper_scan_and_run_in_place_and_organize");
        return;
    };

    let jpg_arc = Arc::new(jpg_bytes.clone());
    let webp_arc = Arc::new(webp_bytes.clone());
    let port_box = Arc::new(std::sync::Mutex::new(0u16));

    let app = Router::new()
        .route(
            "/rss.xml",
            get({
                let p = port_box.clone();
                move || {
                    let port = *p.lock().unwrap();
                    async move {
                        format!(
                            r#"<?xml version="1.0" encoding="UTF-8"?>
                            <rss version="2.0"><channel><title>Studio Feed</title>
                              <item>
                                <guid>mv-100</guid>
                                <title>TX-100 精彩测试影片 演员甲</title>
                                <link>http://127.0.0.1:{port}/detail/tx-100</link>
                                <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
                              </item>
                            </channel></rss>"#
                        )
                    }
                }
            }),
        )
        .route(
            "/wp-json/wp/v2/posts",
            get({
                let p = port_box.clone();
                move |Query(q): Query<std::collections::HashMap<String, String>>| {
                    let port = *p.lock().unwrap();
                    let search = q.get("search").cloned().unwrap_or_default();
                    async move {
                        if search.to_ascii_uppercase().contains("WP-200") {
                            serde_json::to_string(&json!([{
                                "id": 200,
                                "date": "2024-02-02T10:00:00",
                                "link": format!("http://127.0.0.1:{port}/detail/wp-200"),
                                "title": { "rendered": "WP-200 WordPress 影片 演员乙" },
                                "_embedded": {
                                    "wp:featuredmedia": [{ "source_url": format!("http://127.0.0.1:{port}/img/cover.webp") }]
                                }
                            }]))
                            .unwrap()
                        } else {
                            "[]".to_string()
                        }
                    }
                }
            }),
        )
        .route(
            "/detail/tx-100",
            get({
                let p = port_box.clone();
                move || {
                    let port = *p.lock().unwrap();
                    async move {
                        format!(
                            r#"<html><head>
                            <title>TX-100 精彩测试影片</title>
                            <meta property="og:image" content="http://127.0.0.1:{port}/img/cover.jpg" />
                            <script type="application/ld+json">{{"@type":"Movie","name":"TX-100 精彩测试影片","actor":[{{"name":"演员甲"}}],"genre":["剧情"],"datePublished":"2024-01-01"}}</script>
                            </head><body></body></html>"#
                        )
                    }
                }
            }),
        )
        .route(
            "/detail/wp-200",
            get({
                let p = port_box.clone();
                move || {
                    let port = *p.lock().unwrap();
                    async move {
                        format!(
                            r#"<html><head>
                            <title>WP-200 WordPress 影片</title>
                            <meta property="og:image" content="http://127.0.0.1:{port}/img/cover.webp" />
                            </head><body>演员：演员乙</body></html>"#
                        )
                    }
                }
            }),
        )
        .route(
            "/img/cover.jpg",
            get({
                let d = jpg_arc.clone();
                move || {
                    let d = d.clone();
                    async move {
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "image/jpeg")
                            .body(axum::body::Body::from(d.as_ref().clone()))
                            .unwrap()
                    }
                }
            }),
        )
        .route(
            "/img/cover.webp",
            get({
                let d = webp_arc.clone();
                move || {
                    let d = d.clone();
                    async move {
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "image/webp")
                            .body(axum::body::Body::from(d.as_ref().clone()))
                            .unwrap()
                    }
                }
            }),
        );

    let (port, srv) = spawn_test_router(app).await;
    *port_box.lock().unwrap() = port;

    let tmp = tempfile::tempdir().unwrap();
    let scan_dir = tmp.path().join("media");
    let organize_out = tmp.path().join("organized");
    let cache_dir = tmp.path().join("cache");
    tokio::fs::create_dir_all(&scan_dir).await.unwrap();

    let v1 = scan_dir.join("TX-100 精彩测试影片.mp4");
    let v2 = scan_dir.join("WP-200 WordPress 影片.mp4");
    let v2_srt = scan_dir.join("WP-200 WordPress 影片.srt");
    tokio::fs::write(&v1, b"fake video 1").await.unwrap();
    tokio::fs::write(&v2, b"fake video 2").await.unwrap();
    tokio::fs::write(&v2_srt, b"1\n00:00:00,000 --> 00:00:01,000\nhi\n")
        .await
        .unwrap();

    let sources = vec![
        ScrapeDataSource {
            id: "rss-1".to_string(),
            name: "RSS".to_string(),
            source_type: ScrapeSourceType::Rss,
            url: format!("http://127.0.0.1:{port}/rss.xml"),
            enabled: true,
            referer: None,
        },
        ScrapeDataSource {
            id: "wp-1".to_string(),
            name: "WP".to_string(),
            source_type: ScrapeSourceType::Wordpress,
            url: format!("http://127.0.0.1:{port}"),
            enabled: true,
            referer: None,
        },
    ];

    let scan = scan_for_scrape(
        &ScrapeScanRequest {
            scan_dir: None,
            scan_dirs: Some(vec![scan_dir.to_string_lossy().into_owned()]),
            sources: Some(sources),
            rss_url: None,
            feed_id: None,
            referer: None,
            recursive: Some(true),
            skip_existing_nfo: Some(true),
            min_score: Some(50),
            refresh_rss: Some(true),
        },
        &ScanOptions {
            cache_dir: Some(cache_dir),
            log: false,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(scan.scanned_files, 2);
    assert_eq!(scan.matched_count, 2, "items: {:?}", scan.items);

    // Run item 1 in-place
    let m1 = scan
        .items
        .iter()
        .find(|i| i.filename == "TX-100 精彩测试影片.mp4")
        .unwrap()
        .match_item
        .clone()
        .unwrap();
    let res1 = run_media_scrape(
        &ScrapeRunRequest {
            items: vec![ScrapeRunItemInput {
                file_path: v1.to_string_lossy().into_owned(),
                page_url: m1.page_url,
                title: Some(m1.title),
                pub_date: (!m1.pub_date.is_empty()).then_some(m1.pub_date),
                tags: (!m1.tags.is_empty()).then_some(m1.tags),
                guid: (!m1.guid.is_empty()).then_some(m1.guid),
                actors: m1.actors,
                studio: m1.studio,
                cover_url: m1.cover_url,
                referer: None,
            }],
            mode: ScrapeMode::InPlace,
            output_dir: None,
            transfer_mode: Some(ScrapeTransferMode::Move),
            folder_template: Some(ScrapeFolderTemplate::ActorTitle),
            poster_crop: Some(ScrapePosterCrop::Center),
            download_images: Some(true),
            overwrite_nfo: Some(true),
            referer: None,
            studio: None,
            concurrency: Some(1),
        },
        &ScrapeRunOptions::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(res1.succeeded, 1);
    let nfo1 = tokio::fs::read_to_string(res1.results[0].nfo_path.as_ref().unwrap())
        .await
        .unwrap();
    assert!(nfo1.contains("<movie>") && nfo1.contains("演员甲"));
    assert!(std::path::Path::new(res1.results[0].poster_path.as_ref().unwrap()).exists());

    // Run item 2 in organize mode (WebP -> JPG + sidecar subtitle move)
    let m2 = scan
        .items
        .iter()
        .find(|i| i.filename == "WP-200 WordPress 影片.mp4")
        .unwrap()
        .match_item
        .clone()
        .unwrap();
    let res2 = run_media_scrape(
        &ScrapeRunRequest {
            items: vec![ScrapeRunItemInput {
                file_path: v2.to_string_lossy().into_owned(),
                page_url: m2.page_url,
                title: Some(m2.title),
                pub_date: None,
                tags: None,
                guid: None,
                actors: m2.actors,
                studio: m2.studio,
                cover_url: m2.cover_url,
                referer: None,
            }],
            mode: ScrapeMode::Organize,
            output_dir: Some(organize_out.to_string_lossy().into_owned()),
            transfer_mode: Some(ScrapeTransferMode::Move),
            folder_template: Some(ScrapeFolderTemplate::ActorTitle),
            poster_crop: Some(ScrapePosterCrop::None),
            download_images: Some(true),
            overwrite_nfo: Some(true),
            referer: None,
            studio: None,
            concurrency: Some(1),
        },
        &ScrapeRunOptions::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(res2.succeeded, 1);
    assert_eq!(res2.results[0].status, ScrapeOutcomeStatus::Success);
    let target_video =
        std::path::PathBuf::from(res2.results[0].target_video_path.as_ref().unwrap());
    assert!(target_video.exists());
    assert!(!v2.exists(), "source video should be moved");
    assert!(
        target_video.with_extension("srt").exists(),
        "sidecar subtitle should be moved too"
    );

    srv.abort();
}
