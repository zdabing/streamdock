use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap as AxHeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use streamdock_core::cat_catch::cat_catch_tasks;
use streamdock_core::contract::{DownloadTypeKind, HeaderMap};
use streamdock_core::extractor::{ExtractMediaOptions, decode_page_text, extract_media};
use streamdock_core::http::decode_b64_utf8;
use streamdock_core::signed_hls_proxy::{RefreshFn, SignedHlsProxy};
use streamdock_core::video_parser::VideoParser;
use streamdock_core::video_parser::request::{PageData, ParserContext};
use streamdock_core::video_parser::signed_site::{parse_signed_site_detail, refresh_token_session};

async fn spawn_test_router(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, handle)
}

#[tokio::test]
async fn extract_media_bfs_path_filter_and_cross_origin_isolation() {
    let cross_received_auth = Arc::new(Mutex::new(false));
    let cross_app = Router::new().route(
        "/embed",
        get({
            let auth_seen = cross_received_auth.clone();
            move |hdrs: AxHeaderMap| {
                let auth_seen = auth_seen.clone();
                async move {
                    if hdrs.contains_key("cookie") || hdrs.contains_key("authorization") {
                        *auth_seen.lock().unwrap() = true;
                    }
                    r#"<html><title>Embed</title><body><video src="/cross/video.mp4"></video></body></html>"#
                }
            }
        }),
    );
    let (cross_port, cross_srv) = spawn_test_router(cross_app).await;

    let root_html = format!(
        r#"<html>
        <title>Root &amp; Show</title>
        <body>
          <script>var u = "https:\/\/127.0.0.1:{}/escaped\u002Ffirst.m3u8?a=1\u0026b=2";</script>
          <a href="/watch/ep2">Ep2</a>
          <a href="/about">About</a>
          <iframe src="http://127.0.0.1:{cross_port}/embed"></iframe>
        </body>
        </html>"#,
        0 // replaced below after binding
    );

    let root_port_holder = Arc::new(Mutex::new(0u16));
    let root_app = Router::new()
        .route(
            "/",
            get({
                let p = root_port_holder.clone();
                move || {
                    let port = *p.lock().unwrap();
                    let html = root_html.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
                    async move { html }
                }
            }),
        )
        .route(
            "/watch/ep2",
            get(|| async { r#"<html><title>Ep2</title><body><video src="/media/ep2.m3u8"></video></body></html>"# }),
        )
        .route(
            "/about",
            get(|| async { r#"<html><title>About</title><body><video src="/media/ignored.mp4"></video></body></html>"# }),
        );
    let (root_port, root_srv) = spawn_test_router(root_app).await;
    *root_port_holder.lock().unwrap() = root_port;

    let decoded = decode_page_text(r"https:\/\/a.com\/v\u002F1\x3d2&amp;k=3");
    assert_eq!(decoded, "https://a.com/v/1=2&k=3");

    let mut hdrs = HeaderMap::new();
    hdrs.insert("cookie".to_string(), "secret=1".to_string());
    hdrs.insert("authorization".to_string(), "Bearer xyz".to_string());

    let res = extract_media(
        ExtractMediaOptions {
            url: format!("http://127.0.0.1:{root_port}/"),
            urls: None,
            headers: hdrs,
            deep: Some(1),
            path_filter: Some("/watch".to_string()),
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(
        !*cross_received_auth.lock().unwrap(),
        "cookie/auth leaked to cross-origin iframe!"
    );
    let urls: Vec<&str> = res.items.iter().map(|i| i.url.as_str()).collect();
    assert!(
        urls.iter()
            .any(|u| u.contains("/escaped/first.m3u8?a=1&b=2")),
        "urls={urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.contains("/media/ep2.m3u8")),
        "urls={urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.contains("/cross/video.mp4")),
        "urls={urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.contains("ignored.mp4")),
        "pathFilter leaked /about: {urls:?}"
    );

    cross_srv.abort();
    root_srv.abort();
}

#[test]
fn cat_catch_validates_payload_preserves_empty_referer_and_rejects_dash() {
    let ok_payload = json!({
        "action": "catch",
        "data": [{
            "url": "https://cdn.example.com/stream/video",
            "title": "Sample Clip",
            "type": "video/x-matroska",
            "referer": "",
            "requestHeaders": [
                { "name": "Cookie", "value": "sid=99" },
                { "name": "Host", "value": "evil.example" }
            ]
        }]
    });
    let tasks = cat_catch_tasks(&ok_payload).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].options.filename.as_deref(),
        Some("Sample Clip.mkv")
    );
    assert_eq!(
        tasks[0].options.download_type,
        Some(DownloadTypeKind::File).map(|_| streamdock_core::contract::InputTypeKind::File)
    );
    assert_eq!(tasks[0].options.use_default_headers, Some(false));
    if let Some(streamdock_core::contract::HeaderInput::Map(map)) = &tasks[0].options.headers {
        assert_eq!(map.get("referer").and_then(|v| v.as_str()), Some(""));
        assert_eq!(map.get("cookie").and_then(|v| v.as_str()), Some("sid=99"));
        assert!(!map.contains_key("host"));
    } else {
        panic!("expected header map");
    }

    let dash_payload = json!({
        "action": "catch",
        "data": [{ "url": "https://cdn.example.com/manifest.mpd" }]
    });
    assert!(cat_catch_tasks(&dash_payload).is_err());

    let wrong_action = json!({
        "action": "other",
        "data": [{ "url": "https://cdn.example.com/a.mp4" }]
    });
    assert!(cat_catch_tasks(&wrong_action).is_err());
}

#[tokio::test]
async fn video_parsers_douyin_pipixia_weibo_signed_site_via_mock_hooks() {
    let site_host = decode_b64_utf8("amF2cmF0ZS5jb20=");
    let site_origin = decode_b64_utf8("aHR0cHM6Ly93d3cuamF2cmF0ZS5jb20=");
    let cdn_bcdn = decode_b64_utf8("aHR0cHM6Ly9jZG4uYi1jZG4ubmV0");

    let site_host_hook = site_host.clone();
    let cdn_bcdn_hook = cdn_bcdn.clone();
    let req_hook: streamdock_core::video_parser::request::RequestHook = Arc::new(
        move |url, _req| {
            let url = url.to_string();
            let site_host = site_host_hook.clone();
            let cdn_bcdn = cdn_bcdn_hook.clone();
            Box::pin(async move {
                if url.contains("iesdouyin.com/share/video") {
                    let html = r#"<script id="RENDER_DATA" type="application/json">%7B%22app%22%3A%7B%22videoInfoRes%22%3A%7B%22item_list%22%3A%5B%7B%22desc%22%3A%22Douyin+Demo%22%2C%22author%22%3A%7B%22nickname%22%3A%22Alice%22%7D%2C%22video%22%3A%7B%22play_addr%22%3A%7B%22url_list%22%3A%5B%22https%3A%2F%2Faweme.snssdk.com%2Faweme%2Fv1%2Fplaywm%2F%3Fvideo_id%3Dv123%22%5D%7D%2C%22cover%22%3A%7B%22url_list%22%3A%5B%22https%3A%2F%2Fp.douyin.com%2Fc.jpg%22%5D%7D%7D%7D%5D%7D%7D%7D</script>"#;
                    return Ok(PageData {
                        url,
                        text: html.to_string(),
                        content_type: "text/html".to_string(),
                    });
                }
                if url.contains("h5.pipix.com/bds/cell/cell_h5_comment") {
                    let body = json!({
                    "status_code": 0,
                    "data": {
                        "item": {
                            "content": "Pipixia Demo",
                            "author": { "name": "Bob", "avatar": { "url_list": [{ "url": "https://ppx.test/a.jpg" }] } },
                            "video": {
                                "video_high": { "url_list": [{ "url": "https://ppx.test/high.mp4" }] },
                                "cover_image": { "url_list": [{ "url": "https://ppx.test/cover.jpg" }] }
                            }
                        }
                    }
                })
                .to_string();
                    return Ok(PageData {
                        url,
                        text: body,
                        content_type: "application/json".to_string(),
                    });
                }
                if url.contains("m.weibo.cn/statuses/show") {
                    let body = json!({
                        "ok": 1,
                        "data": {
                            "idstr": "490001",
                            "text_raw": "Weibo Clip",
                            "user": { "screen_name": "Wei", "avatar_hd": "https://wb.test/av.jpg" },
                            "page_info": {
                                "media_info": {
                                    "stream_url_hd": "https://wb.test/hd.mp4",
                                    "stream_url": "https://wb.test/sd.mp4"
                                },
                                "page_pic": { "url": "https://wb.test/pic.jpg" }
                            }
                        }
                    })
                    .to_string();
                    return Ok(PageData {
                        url,
                        text: body,
                        content_type: "application/json".to_string(),
                    });
                }
                if url.contains(&format!("{site_host}/movie/abc-123")) {
                    let html = format!(
                        r#"<html><head><title>DEMO-001 Demo Title</title><meta property="og:image" content="https://site.test/poster.jpg"></head><body><script>var hls="{cdn_bcdn}/live/abc-123/index.m3u8?token=t1";</script></body></html>"#
                    );
                    return Ok(PageData {
                        url,
                        text: html,
                        content_type: "text/html".to_string(),
                    });
                }
                Ok(PageData {
                    url,
                    text: String::new(),
                    content_type: "text/plain".to_string(),
                })
            })
        },
    );

    let res_hook: streamdock_core::video_parser::request::ResolveHook = Arc::new(|url, _req| {
        let u = url.to_string();
        Box::pin(async move {
            if u.contains("v.douyin.com") {
                Ok("https://www.iesdouyin.com/share/video/7123456789012345678/".to_string())
            } else if u.contains("pipix.com") {
                Ok("https://h5.pipix.com/item/700001".to_string())
            } else {
                Ok(u)
            }
        })
    });

    let mut ctx = ParserContext::new(
        streamdock_core::http::default_http_client().clone(),
        CancellationToken::new(),
    );
    ctx.request_override = Some(req_hook);
    ctx.resolve_override = Some(res_hook);

    let dy =
        VideoParser::parse_with_context("https://v.douyin.com/short123/", &HeaderMap::new(), &ctx)
            .await
            .unwrap();
    assert_eq!(dy.code, 0, "douyin err: {:?}", dy.message);
    assert!(dy.data.unwrap().url.contains("play/?video_id=v123"));

    let ppx = VideoParser::parse_with_context(
        "https://h5.pipix.com/item/700001",
        &HeaderMap::new(),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(ppx.code, 0, "pipixia err: {:?}", ppx.message);
    assert_eq!(ppx.data.unwrap().url, "https://ppx.test/high.mp4");

    let mut wb_headers = HeaderMap::new();
    wb_headers.insert("cookie".to_string(), "SUB=demo".to_string());
    let wb = VideoParser::parse_with_context("https://weibo.com/1234/490001", &wb_headers, &ctx)
        .await
        .unwrap();
    assert_eq!(wb.code, 0, "weibo err: {:?}", wb.message);
    assert_eq!(wb.data.unwrap().url, "https://wb.test/hd.mp4");

    // Verify Weibo rejects requests missing Cookie
    let wb_no_cookie =
        VideoParser::parse_with_context("https://weibo.com/1234/490001", &HeaderMap::new(), &ctx)
            .await
            .unwrap();
    assert_eq!(wb_no_cookie.code, 1);

    let site_res = VideoParser::parse_with_context(
        &format!("{site_origin}/movie/abc-123.html"),
        &HeaderMap::new(),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(site_res.code, 0, "signed_site err: {:?}", site_res.message);
    assert!(site_res.data.unwrap().url.contains("abc-123/index.m3u8"));
}

#[tokio::test]
async fn signed_hls_proxy_refreshes_on_403_and_normalizes_identity() {
    let refresh_count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/hls/index.m3u8",
            get(
                |q: axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    if q.get("token").map(String::as_str) != Some("fresh") {
                        return Response::builder()
                            .status(StatusCode::FORBIDDEN)
                            .body(axum::body::Body::from("expired"))
                            .unwrap();
                    }
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(axum::body::Body::from(
                            "#EXTM3U\n#EXTINF:1,\nseg0.ts?token=fresh\n#EXT-X-ENDLIST\n",
                        ))
                        .unwrap()
                },
            ),
        )
        .route("/hls/seg0.ts", get(|| async { b"tsdata".to_vec() }));

    let (port, srv) = spawn_test_router(app).await;
    let initial = format!("http://127.0.0.1:{port}/hls/index.m3u8?token=old&expires=100");

    let rc = refresh_count.clone();
    let refresh: RefreshFn = Arc::new(move || {
        let rc = rc.clone();
        Box::pin(async move {
            rc.fetch_add(1, Ordering::SeqCst);
            Ok(format!(
                "http://127.0.0.1:{port}/hls/index.m3u8?token=fresh&expires=999"
            ))
        })
    });

    let client = streamdock_core::http::default_http_client().clone();
    let proxy = SignedHlsProxy::start(
        client.clone(),
        &initial,
        HeaderMap::new(),
        CancellationToken::new(),
        refresh,
    )
    .await
    .unwrap();

    let manifest = client
        .get(&proxy.entry_url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(manifest.contains("#EXTM3U"));
    assert_eq!(refresh_count.load(Ordering::SeqCst), 1);

    let proxied_seg = manifest
        .lines()
        .find(|l| l.starts_with("http://"))
        .expect("expected rewritten proxy segment URL");
    let seg_bytes = client
        .get(proxied_seg)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(seg_bytes.as_ref(), b"tsdata");
    assert_eq!(
        proxy.identity(proxied_seg),
        format!("http://127.0.0.1:{port}/hls/seg0.ts")
    );

    proxy.close().await;
    srv.abort();
}

#[tokio::test]
async fn signed_site_player_v2_hmac_refresh_api_secret_issued_at_and_cloud_type_1() {
    assert!(streamdock_core::store::default_rss_url().starts_with("https://"));
    assert!(streamdock_core::store::default_wp_url().starts_with("https://"));

    let site_host = decode_b64_utf8("amF2cmF0ZS5jb20=");
    let site_origin = decode_b64_utf8("aHR0cHM6Ly93d3cuamF2cmF0ZS5jb20=");
    let cdn_avk = decode_b64_utf8("aHR0cHM6Ly92aWRlb2Nkbi5hdmtpbmcueHl6");
    let cdn_avk_escaped = cdn_avk.replace('/', "\\/");

    let seen_bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_bodies_hook = seen_bodies.clone();
    let site_host_hook = site_host.clone();
    let site_origin_hook = site_origin.clone();
    let cdn_avk_hook = cdn_avk.clone();
    let cdn_avk_esc_hook = cdn_avk_escaped.clone();

    let req_hook: streamdock_core::video_parser::request::RequestHook = Arc::new(
        move |url, req| {
            let url = url.to_string();
            let post_body = req.method_post_body.clone();
            let bodies = seen_bodies_hook.clone();
            let site_host = site_host_hook.clone();
            let site_origin = site_origin_hook.clone();
            let cdn_avk = cdn_avk_hook.clone();
            let cdn_avk_esc = cdn_avk_esc_hook.clone();
            Box::pin(async move {
                if url.contains(&format!("{site_host}/movie/v2-demo")) {
                    return Ok(PageData {
                    url,
                    text: r#"<html><title>V2 Demo</title><iframe src="/player/v2?payload=abc"></iframe></html>"#
                        .to_string(),
                    content_type: "text/html".to_string(),
                });
                }
                if url.contains(&format!("{site_host}/player/v2?payload=abc")) {
                    return Ok(PageData {
                        url,
                        text: format!(
                            r#"<script>createPlayer({{
                        cloudType: 0,
                        initialSignedUrl: "{cdn_avk_esc}\/20261030\/movie\/playlist.m3u8",
                        initialToken: "init-tok",
                        initialExpires: 1999999999,
                        initialTokenPath: "%2Fmovie%2F",
                        movieId: "movie-1",
                        sessionId: "sess-1",
                        guidId: "guid-1",
                        apiSecret: "secret-123",
                        apiSecretIssuedAt: 1793295000,
                        timeDiff: 0
                    }});</script>"#
                        ),
                        content_type: "text/html".to_string(),
                    });
                }
                if url.ends_with("/api/token/generate") {
                    let body_val: serde_json::Value =
                        serde_json::from_str(post_body.as_deref().unwrap_or("{}")).unwrap();
                    bodies.lock().unwrap().push(body_val.clone());
                    assert_eq!(body_val["apiSecretIssuedAt"], 1793295000);
                    assert_eq!(body_val["movieId"], "movie-1");
                    assert_eq!(body_val["sessionId"], "sess-1");
                    assert_eq!(body_val["guidId"], "guid-1");
                    assert_eq!(
                        body_val["url"],
                        format!("{site_origin}/player/v2?payload=abc")
                    );
                    let sig = body_val["signature"].as_str().unwrap_or("");
                    assert_eq!(sig.len(), 44, "expected base64 sha256 hmac, got {sig}");
                    return Ok(PageData {
                        url,
                        text: json!({
                            "data": {
                                "token": "renewed-tok",
                                "expires": 1999999999,
                                "tokenPath": "%2Fmovie%2F"
                            }
                        })
                        .to_string(),
                        content_type: "application/json".to_string(),
                    });
                }
                if url.contains(&format!("{site_host}/movie/cloud1-demo")) {
                    return Ok(PageData {
                    url,
                    text: r#"<html><title>Direct</title><iframe src="/player/v2?payload=c1"></iframe></html>"#
                        .to_string(),
                    content_type: "text/html".to_string(),
                });
                }
                if url.contains(&format!("{site_host}/player/v2?payload=c1")) {
                    return Ok(PageData {
                        url,
                        text: format!(
                            r#"<script>createPlayer({{ cloudType: 1, m3u8Url: "{cdn_avk}/direct/playlist.m3u8" }});</script>"#
                        ),
                        content_type: "text/html".to_string(),
                    });
                }
                if url.contains(&format!("{site_host}/movie/bcdn-path")) {
                    return Ok(PageData {
                        url,
                        text: format!(
                            r#"<html><title>Bcdn Path</title><script>var s="{cdn_avk}/bcdn_token=tok1&expires=1999999999&token_path=%2Fabc%2F/abc/playlist.m3u8";</script></html>"#
                        ),
                        content_type: "text/html".to_string(),
                    });
                }
                Ok(PageData {
                    url,
                    text: String::new(),
                    content_type: "text/plain".to_string(),
                })
            })
        },
    );

    let mut ctx = ParserContext::new(
        streamdock_core::http::default_http_client().clone(),
        CancellationToken::new(),
    );
    ctx.request_override = Some(req_hook);

    let v2 = parse_signed_site_detail(
        &format!("{site_origin}/movie/v2-demo.html"),
        &HeaderMap::new(),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(v2.code, 0, "err={:?}", v2.message);
    let detail = v2.data.unwrap();
    assert!(detail.is_signed);
    assert!(detail.info.url.contains("token=init-tok"));
    assert!(detail.info.url.contains("token_path=%2Fmovie%2F"));
    assert!(!detail.info.url.contains("%252F"));
    let sess = detail.session.expect("expected token session");

    let renewed = refresh_token_session(&sess, &HeaderMap::new(), &ctx)
        .await
        .unwrap();
    assert!(renewed.contains("token=renewed-tok"));
    assert!(renewed.contains("token_path=%2Fmovie%2F"));
    assert!(!renewed.contains("%252F"));
    assert_eq!(seen_bodies.lock().unwrap().len(), 1);

    let c1 = parse_signed_site_detail(
        &format!("{site_origin}/movie/cloud1-demo.html"),
        &HeaderMap::new(),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(c1.code, 0, "err={:?}", c1.message);
    let c1_data = c1.data.unwrap();
    assert!(!c1_data.is_signed);
    assert!(c1_data.session.is_none());
    assert_eq!(c1_data.info.url, format!("{cdn_avk}/direct/playlist.m3u8"));

    let bcdn = parse_signed_site_detail(
        &format!("{site_origin}/movie/bcdn-path.html"),
        &HeaderMap::new(),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(bcdn.code, 0, "err={:?}", bcdn.message);
    assert!(bcdn.data.unwrap().is_signed);
}

#[tokio::test]
async fn signed_hls_proxy_supports_date_prefix_no_double_encode_and_bcdn_path_token() {
    // Sub-case A: 8-digit date prefix + double-encoded initial token_path + sibling key
    let app_a = Router::new()
        .route(
            "/20261030/abc-123/720p/video.m3u8",
            get(|uri: axum::http::Uri| async move {
                let q = uri.query().unwrap_or("");
                assert!(!q.contains("%252F"), "double-encoded token_path leaked: {q}");
                if !q.contains("token=fresh") {
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("expired"))
                        .unwrap();
                }
                Response::builder()
                    .status(StatusCode::OK)
                    .body(axum::body::Body::from(
                        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../enc.key\"\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
                    ))
                    .unwrap()
            }),
        )
        .route(
            "/20261030/abc-123/enc.key",
            get(|uri: axum::http::Uri| async move {
                let q = uri.query().unwrap_or("");
                assert!(q.contains("token=fresh"), "sibling key missing token: {q}");
                assert!(
                    q.contains("token_path=%2Fabc-123%2F"),
                    "sibling key missing single-encoded token_path: {q}"
                );
                b"0123456789abcdef".to_vec()
            }),
        )
        .route(
            "/20261030/abc-123/720p/seg0.ts",
            get(|uri: axum::http::Uri| async move {
                let q = uri.query().unwrap_or("");
                assert!(q.contains("token=fresh"), "seg missing token: {q}");
                b"seg-date-ok".to_vec()
            }),
        );

    let (port_a, srv_a) = spawn_test_router(app_a).await;
    let initial_a = format!(
        "http://127.0.0.1:{port_a}/20261030/abc-123/720p/video.m3u8?token=old&expires=100&token_path=%252Fabc-123%252F"
    );
    let refresh_a: RefreshFn = Arc::new(move || {
        Box::pin(async move {
            Ok(format!(
                "http://127.0.0.1:{port_a}/20261030/abc-123/720p/video.m3u8?token=fresh&expires=1999999999&token_path=%2Fabc-123%2F"
            ))
        })
    });

    let client = streamdock_core::http::default_http_client().clone();
    let proxy_a = SignedHlsProxy::start(
        client.clone(),
        &initial_a,
        HeaderMap::new(),
        CancellationToken::new(),
        refresh_a,
    )
    .await
    .unwrap();

    let manifest_a = client
        .get(&proxy_a.entry_url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(manifest_a.contains("#EXTM3U"));
    let key_proxy_url = manifest_a
        .split("URI=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("rewritten key URL");
    let key_bytes = client
        .get(key_proxy_url)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(key_bytes.as_ref(), b"0123456789abcdef");

    let seg_proxy_url = manifest_a
        .lines()
        .find(|l| l.starts_with("http://"))
        .expect("rewritten segment URL");
    let seg_bytes_a = client
        .get(seg_proxy_url)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(seg_bytes_a.as_ref(), b"seg-date-ok");
    assert_eq!(
        proxy_a.identity(seg_proxy_url),
        format!("http://127.0.0.1:{port_a}/20261030/abc-123/720p/seg0.ts")
    );
    proxy_a.close().await;
    srv_a.abort();

    // Sub-case B: path-based bcdn_token prefix renewal and identity normalization
    let app_b = Router::new()
        .route(
            "/{bcdn}/abc/720p/video.m3u8",
            get(
                |axum::extract::Path(bcdn): axum::extract::Path<String>| async move {
                    if !bcdn.contains("bcdn_token=fresh") {
                        return Response::builder()
                            .status(StatusCode::FORBIDDEN)
                            .body(axum::body::Body::from("expired"))
                            .unwrap();
                    }
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(axum::body::Body::from(
                            "#EXTM3U\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
                        ))
                        .unwrap()
                },
            ),
        )
        .route(
            "/{bcdn}/abc/720p/seg0.ts",
            get(
                |axum::extract::Path(bcdn): axum::extract::Path<String>| async move {
                    assert!(bcdn.contains("bcdn_token=fresh"));
                    b"bcdn-seg".to_vec()
                },
            ),
        );

    let (port_b, srv_b) = spawn_test_router(app_b).await;
    let initial_b = format!(
        "http://127.0.0.1:{port_b}/bcdn_token=old&expires=100&token_path=%2Fabc%2F/abc/720p/video.m3u8"
    );
    let refresh_b: RefreshFn = Arc::new(move || {
        Box::pin(async move {
            Ok(format!(
                "http://127.0.0.1:{port_b}/bcdn_token=fresh&expires=1999999999&token_path=%2Fabc%2F/abc/720p/video.m3u8"
            ))
        })
    });

    let proxy_b = SignedHlsProxy::start(
        client.clone(),
        &initial_b,
        HeaderMap::new(),
        CancellationToken::new(),
        refresh_b,
    )
    .await
    .unwrap();
    let manifest_b = client
        .get(&proxy_b.entry_url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let seg_b = manifest_b
        .lines()
        .find(|l| l.starts_with("http://"))
        .expect("rewritten bcdn segment URL");
    let seg_bytes_b = client
        .get(seg_b)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(seg_bytes_b.as_ref(), b"bcdn-seg");
    assert_eq!(
        proxy_b.identity(seg_b),
        format!("http://127.0.0.1:{port_b}/abc/720p/seg0.ts")
    );

    proxy_b.close().await;
    srv_b.abort();
}
