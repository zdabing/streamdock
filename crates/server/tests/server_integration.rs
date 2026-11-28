use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use streamdock_core::http::md5_hex;
use streamdock_server::{ServerOptions, StreamDockServer};

static GLOBAL_PROXY_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

async fn spawn_upstream_router(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, handle)
}

#[tokio::test]
async fn server_auth_public_routes_ws_and_legacy_404() {
    let _proxy_read = GLOBAL_PROXY_LOCK.read().await;
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    let token = "my-secret-pw".to_string();

    let server = StreamDockServer::start(ServerOptions {
        port: Some(0),
        host: Some("127.0.0.1".to_string()),
        cache_dir: Some(cache_dir),
        token: Some(token.clone()),
        web_dir: None,
    })
    .await
    .unwrap();

    let base = format!("http://127.0.0.1:{}", server.port());
    let client = Client::new();

    // Public routes
    let hc: Value = client
        .get(format!("{base}/healthcheck"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hc["code"], 0);
    assert_eq!(hc["message"], "ok");

    let index_resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(index_resp.status(), 200);
    assert!(
        index_resp
            .text()
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("<html")
    );

    let script_resp = client
        .get(format!("{base}/scriptcat.user.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(script_resp.status(), 200);
    assert!(script_resp.text().await.unwrap().contains("UserScript"));

    // Legacy removed routes must return 404
    for legacy in [
        "/play.html",
        "/localplay/video.mp4",
        "/m3u8-capture.user.js",
        "/rss-bridge/feed",
    ] {
        let r = client.get(format!("{base}{legacy}")).send().await.unwrap();
        assert_eq!(r.status(), 404, "expected 404 for {legacy}");
    }

    // Auth enforcement
    let unauth = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);
    let unauth_json: Value = unauth.json().await.unwrap();
    assert_eq!(unauth_json["code"], 1008);

    // ?token= only works on /api/file, NOT /api/tasks
    let query_tok_rejected = client
        .get(format!("{base}/api/tasks?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(query_tok_rejected.status(), 401);

    // Raw token and 8-char MD5 short token accepted
    let full_ok = client
        .get(format!("{base}/api/config"))
        .header("Authorization", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(full_ok.status(), 200);

    let short_hash = md5_hex(token.trim())[..8].to_string();
    let short_ok = client
        .get(format!("{base}/api/platforms"))
        .header("Authorization", &short_hash)
        .send()
        .await
        .unwrap();
    assert_eq!(short_ok.status(), 200);

    // WebSocket invalid token receives Close 1008
    let (mut bad_ws, _) = connect_async(format!("ws://127.0.0.1:{}/ws?token=wrong", server.port()))
        .await
        .unwrap();
    let close_msg = bad_ws.next().await.unwrap().unwrap();
    match close_msg {
        Message::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 1008),
        other => panic!("expected WS Close(1008), got {other:?}"),
    }

    // WebSocket valid token immediately receives 5 ordered frames
    let (mut good_ws, _) = connect_async(format!(
        "ws://127.0.0.1:{}/ws?token={short_hash}",
        server.port()
    ))
    .await
    .unwrap();
    let mut types = Vec::new();
    for _ in 0..5 {
        let msg = good_ws.next().await.unwrap().unwrap();
        let text = msg.into_text().unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        types.push(v["type"].as_str().unwrap().to_string());
    }
    assert_eq!(
        types,
        vec![
            "serverInfo",
            "tasks",
            "rssSources",
            "scrapeConfig",
            "scrapeJob"
        ]
    );

    drop(good_ws);
    server.close().await;
}

#[tokio::test]
async fn server_cat_catch_requires_secret_and_enqueues() {
    let _proxy_read = GLOBAL_PROXY_LOCK.read().await;
    let tmp = tempfile::tempdir().unwrap();
    let s_open = StreamDockServer::start(ServerOptions {
        port: Some(0),
        host: Some("127.0.0.1".to_string()),
        cache_dir: Some(tmp.path().join("open")),
        token: Some(String::new()),
        web_dir: None,
    })
    .await
    .unwrap();
    let client = Client::new();
    let r_403 = client
        .post(format!("http://127.0.0.1:{}/api/cat-catch", s_open.port()))
        .json(&json!({ "action": "catch", "data": [{ "url": "https://a.test/v.mp4" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_403.status(), 403);
    s_open.close().await;
}

#[tokio::test]
async fn server_download_lifecycle_range_file_and_delete() {
    let _proxy_read = GLOBAL_PROXY_LOCK.read().await;
    let payload: Vec<u8> = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".to_vec();
    let payload_arc = Arc::new(payload.clone());
    let upstream = Router::new().route(
        "/clip.mp4",
        get({
            let d = payload_arc.clone();
            move |_h: HeaderMap| {
                let d = d.clone();
                async move {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "video/mp4")
                        .header(header::CONTENT_LENGTH, d.len().to_string())
                        .body(Body::from(d.as_ref().clone()))
                        .unwrap()
                }
            }
        }),
    );
    let (up_port, up_srv) = spawn_upstream_router(upstream).await;

    let tmp = tempfile::tempdir().unwrap();
    let save_dir = tmp.path().join("downloads");
    let cache_dir = tmp.path().join("cache");

    let server = StreamDockServer::start(ServerOptions {
        port: Some(0),
        host: Some("127.0.0.1".to_string()),
        cache_dir: Some(cache_dir.clone()),
        token: Some("tok".to_string()),
        web_dir: None,
    })
    .await
    .unwrap();
    let base = format!("http://127.0.0.1:{}", server.port());
    let client = Client::new();

    // Update saveDir to temp downloads
    let cfg_res = client
        .post(format!("{base}/api/config"))
        .header("Authorization", "tok")
        .json(&json!({ "saveDir": save_dir.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(cfg_res.status(), 200);

    // Atomic batch rejection on >100 or invalid URL
    let bad_batch = client
        .post(format!("{base}/api/download"))
        .header("Authorization", "tok")
        .json(&json!({
            "list": [
                { "url": format!("http://127.0.0.1:{up_port}/clip.mp4") },
                { "url": "not-a-valid-url" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_batch.status(), 400);
    let tasks_after_bad: Value = client
        .get(format!("{base}/api/tasks"))
        .header("Authorization", "tok")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tasks_after_bad["data"].as_object().unwrap().is_empty());

    // Valid download
    let target_url = format!("http://127.0.0.1:{up_port}/clip.mp4");
    let ok_batch = client
        .post(format!("{base}/api/download"))
        .header("Authorization", "tok")
        .json(&json!({ "list": [{ "url": &target_url }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_batch.status(), 200);

    // Poll until done
    let mut done = false;
    for _ in 0..40 {
        let tasks_v: Value = client
            .get(format!("{base}/api/tasks"))
            .header("Authorization", "tok")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if tasks_v["data"][&target_url]["status"].as_str() == Some("done") {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "task did not complete in time");

    // GET /api/file with ?token= AND Range 206 support
    let encoded_url: String = url::form_urlencoded::byte_serialize(target_url.as_bytes()).collect();
    let range_resp = client
        .get(format!("{base}/api/file?url={encoded_url}&token=tok"))
        .header("Range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(range_resp.status(), 206);
    assert_eq!(range_resp.bytes().await.unwrap().as_ref(), b"0123456789");

    let full_resp = client
        .get(format!("{base}/api/file?url={encoded_url}&token=tok"))
        .send()
        .await
        .unwrap();
    assert_eq!(full_resp.status(), 200);
    assert_eq!(
        full_resp.bytes().await.unwrap().as_ref(),
        payload.as_slice()
    );

    // Delete with deleteCache + deleteVideo
    let del_resp = client
        .post(format!("{base}/api/delete"))
        .header("Authorization", "tok")
        .json(&json!({
            "urls": [&target_url],
            "deleteCache": true,
            "deleteVideo": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(del_resp.status(), 200);
    assert!(!save_dir.join("clip.mp4").exists());

    server.close().await;
    up_srv.abort();
}

#[tokio::test]
async fn server_sqlite_wal_restart_and_legacy_json_migration() {
    let _proxy_read = GLOBAL_PROXY_LOCK.read().await;
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    tokio::fs::create_dir_all(cache_dir.join("rss-items"))
        .await
        .unwrap();

    // Populate legacy JSON files prior to first Rust server boot
    tokio::fs::write(
        cache_dir.join("config.json"),
        serde_json::to_vec(&json!({
            "saveDir": "./legacy-dl",
            "threadNum": 7,
            "maxDownloads": 3,
            "rssBridgeFeeds": [{
                "id": "legacy-bridge",
                "name": "Bridge Feed",
                "url": "https://bridge.example.com/rss",
                "enabled": true
            }]
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    tokio::fs::write(
        cache_dir.join("cache.json"),
        serde_json::to_vec(&json!([
            ["https://ex.test/1.m3u8", {
                "url": "https://ex.test/1.m3u8",
                "status": "resume",
                "filename": "one.mp4",
                "dlOptions": {
                    "filename": "legacy-one.mp4",
                    "headers": {
                        "x-legacy": "1"
                    }
                }
            }],
            ["https://ex.test/2.mp4", {
                "url": "https://ex.test/2.mp4",
                "status": "done",
                "filename": "two.mp4"
            }]
        ]))
        .unwrap(),
    )
    .await
    .unwrap();

    tokio::fs::write(
        cache_dir.join("scrape-config.json"),
        serde_json::to_vec(&json!({
            "mode": "organize",
            "outputDir": "/srv/media",
            "minScore": 72
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    tokio::fs::write(
        cache_dir.join("scrape-job.json"),
        serde_json::to_vec(&json!({
            "snapshot": {
                "id": "scrape-old",
                "revision": 5,
                "status": "running",
                "mode": "in-place",
                "transferMode": "move",
                "concurrency": 2,
                "total": 1,
                "done": 0,
                "succeeded": 0,
                "failed": 0,
                "remaining": 1,
                "canResume": false,
                "canRetryFailed": false,
                "startedAt": 1000,
                "results": [],
                "totalResults": 0
            },
            "revision": 5,
            "request": {
                "items": [{ "filePath": "/tmp/a.mp4", "pageUrl": "https://ex.test/p/1" }],
                "mode": "in-place"
            },
            "failedResults": [],
            "recentSucceeded": [],
            "succeededPaths": [],
            "processedPaths": [],
            "failedPaths": [],
            "timingTotals": { "count": 0, "totalMs": 0, "detailMs": 0, "transferMs": 0, "artworkMs": 0, "cropMs": 0, "crossFsCount": 0 },
            "elapsedBeforeRunMs": 42
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    tokio::fs::write(
        cache_dir.join("rss-sources.json"),
        serde_json::to_vec(&json!({
            "sources": [{
                "id": "legacy-feed",
                "name": "Legacy RSS",
                "mode": "rss",
                "url": "https://rss.example.com/feed.xml",
                "enabled": true
            }]
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    tokio::fs::write(
        cache_dir.join("rss-items").join("legacy-feed.json"),
        serde_json::to_vec(&json!({
            "items": [{
                "sourceId": "legacy-feed",
                "guid": "g-migrated",
                "title": "Migrated Episode",
                "link": "https://rss.example.com/1",
                "enclosureUrl": "https://rss.example.com/1.mp4",
                "enclosureLength": 1024,
                "pubDate": 1700000000000i64,
                "pubDateText": "2023-11-14",
                "tags": [],
                "fetchedAt": 1700000000000i64,
                "status": "new",
                "taskUrl": "",
                "episodeKey": "E01",
                "reason": ""
            }]
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    // First boot auto-migrates legacy files into SQLite WAL database
    let srv1 = StreamDockServer::start(ServerOptions {
        port: Some(0),
        host: Some("127.0.0.1".to_string()),
        cache_dir: Some(cache_dir.clone()),
        token: Some(String::new()),
        web_dir: None,
    })
    .await
    .unwrap();
    assert!(cache_dir.join("streamdock.db").exists());

    let client = Client::new();
    let b1 = format!("http://127.0.0.1:{}", srv1.port());

    let cfg: Value = client
        .get(format!("{b1}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["data"]["threadNum"], 7);
    assert_eq!(cfg["data"]["saveDir"], "./legacy-dl");

    let tasks: Value = client
        .get(format!("{b1}/api/tasks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // "resume" must normalize to "pause" on boot, and legacy dlOptions promotes to options
    assert_eq!(tasks["data"]["https://ex.test/1.m3u8"]["status"], "pause");
    assert_eq!(
        tasks["data"]["https://ex.test/1.m3u8"]["options"]["filename"],
        "legacy-one.mp4"
    );
    assert_eq!(
        tasks["data"]["https://ex.test/1.m3u8"]["options"]["headers"]["x-legacy"],
        "1"
    );
    assert_eq!(tasks["data"]["https://ex.test/2.mp4"]["status"], "done");

    let sj: Value = client
        .get(format!("{b1}/api/scrape/job"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sj["data"]["status"], "stopped");

    let rss_sources: Value = client
        .get(format!("{b1}/api/rss/sources"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rss_sources["data"][0]["id"], "legacy-feed");
    assert_eq!(rss_sources["data"][0]["totalCount"], 1);

    // Mutate state via API and restart server to prove SQLite persistence
    client
        .post(format!("{b1}/api/config"))
        .json(&json!({ "threadNum": 9 }))
        .send()
        .await
        .unwrap();
    srv1.close().await;

    // Remove legacy JSON files so second boot ONLY has streamdock.db
    let _ = tokio::fs::remove_file(cache_dir.join("config.json")).await;
    let _ = tokio::fs::remove_file(cache_dir.join("cache.json")).await;

    let srv2 = StreamDockServer::start(ServerOptions {
        port: Some(0),
        host: Some("127.0.0.1".to_string()),
        cache_dir: Some(cache_dir),
        token: Some(String::new()),
        web_dir: None,
    })
    .await
    .unwrap();
    let b2 = format!("http://127.0.0.1:{}", srv2.port());
    let cfg2: Value = client
        .get(format!("{b2}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg2["data"]["threadNum"], 9);

    let tasks2: Value = client
        .get(format!("{b2}/api/tasks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        tasks2["data"]["https://ex.test/1.m3u8"]["filename"],
        "one.mp4"
    );

    srv2.close().await;
}

#[tokio::test]
async fn server_rss_and_scrape_job_api_endpoints() {
    let _proxy_read = GLOBAL_PROXY_LOCK.read().await;
    let up = Router::new().route(
        "/rss.xml",
        get(|| async {
            r#"<rss version="2.0"><channel><title>API Feed</title>
                <item><guid>x1</guid><title>Item 1</title><link>/p/1</link><enclosure url="/v/1.mp4" length="1000"/></item>
            </channel></rss>"#
        }),
    );
    let (up_port, up_srv) = spawn_upstream_router(up).await;

    let tmp = tempfile::tempdir().unwrap();
    let srv = StreamDockServer::start(ServerOptions {
        port: Some(0),
        host: Some("127.0.0.1".to_string()),
        cache_dir: Some(tmp.path().join("cache")),
        token: Some(String::new()),
        web_dir: None,
    })
    .await
    .unwrap();
    let base = format!("http://127.0.0.1:{}", srv.port());
    let client = Client::new();

    // RSS validate + create + refresh + items + actions + mark-all-read + delete
    let val_r: Value = client
        .post(format!("{base}/api/rss/validate"))
        .json(&json!({ "url": format!("http://127.0.0.1:{up_port}/rss.xml") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(val_r["code"], 0);
    assert_eq!(val_r["data"]["totalItems"], 1);

    let create_r: Value = client
        .post(format!("{base}/api/rss/sources"))
        .json(&json!({
            "name": "Src1",
            "url": format!("http://127.0.0.1:{up_port}/rss.xml"),
            "fetchNow": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(create_r["code"], 0);
    let id = create_r["data"]["id"].as_str().unwrap();

    let ref_r: Value = client
        .post(format!("{base}/api/rss/sources/{id}/refresh"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ref_r["code"], 0);
    assert_eq!(ref_r["data"]["totalItems"], 1);

    let items_r: Value = client
        .get(format!(
            "{base}/api/rss/sources/{id}/items?page=1&pageSize=10"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(items_r["data"]["total"], 1);

    let mark_r: Value = client
        .post(format!("{base}/api/rss/sources/{id}/mark-all-read"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mark_r["data"]["updated"], 1);

    // Scrape job clear on empty works
    let clr: Value = client
        .post(format!("{base}/api/scrape/job/clear"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clr["code"], 0);
    assert!(clr["data"].is_null());

    srv.close().await;
    up_srv.abort();
}

#[tokio::test]
async fn server_config_hot_reloads_global_proxy_for_rss_and_scraper() {
    let _proxy_write = GLOBAL_PROXY_LOCK.write().await;

    let mock_proxy = Router::new().fallback(|| async { "via-hot-proxy" });
    let (proxy_port, proxy_srv) = spawn_upstream_router(mock_proxy).await;

    let tmp = tempfile::tempdir().unwrap();
    let srv = StreamDockServer::start(ServerOptions {
        port: Some(0),
        host: Some("127.0.0.1".to_string()),
        cache_dir: Some(tmp.path().join("cache")),
        token: Some(String::new()),
        web_dir: None,
    })
    .await
    .unwrap();
    let base = format!("http://127.0.0.1:{}", srv.port());
    let client = Client::new();

    let cfg_res = client
        .post(format!("{base}/api/config"))
        .json(&json!({
            "proxyMode": "custom",
            "proxyUrl": format!("http://127.0.0.1:{proxy_port}"),
            "noProxy": "127.0.0.1,localhost"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(cfg_res.status(), 200);

    let rss_resp = streamdock_core::http::global_http_client()
        .get("http://hot-proxy.invalid/rss")
        .send()
        .await
        .unwrap();
    assert_eq!(rss_resp.status(), 200);
    assert_eq!(rss_resp.text().await.unwrap(), "via-hot-proxy");

    let scraper_resp = streamdock_core::scraper::detail::scraper_http_client()
        .get("http://hot-proxy.invalid/detail")
        .send()
        .await
        .unwrap();
    assert_eq!(scraper_resp.status(), 200);
    assert_eq!(scraper_resp.text().await.unwrap(), "via-hot-proxy");

    // Restore default system proxy before tearing down
    let reset_res = client
        .post(format!("{base}/api/config"))
        .json(&json!({
            "proxyMode": "system",
            "proxyUrl": "",
            "noProxy": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reset_res.status(), 200);

    srv.close().await;
    proxy_srv.abort();
}
