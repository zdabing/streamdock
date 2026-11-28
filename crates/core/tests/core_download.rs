use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aes::Aes128;
use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::http::{HeaderMap as AxHeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use cbc::Encryptor;
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use tokio::net::TcpListener;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use streamdock_core::contract::{
    DownloadExecOptions, HeaderInput, HeaderMap, InputTypeKind, ProxyMode,
};
use streamdock_core::download::download_resource;
use streamdock_core::format_options::format_options;
use streamdock_core::http::{ProxySettings, build_http_client, format_headers, scope_credentials};
use streamdock_core::m3u8_download::m3u8_download;
use streamdock_core::m3u8_parse::parse_m3u8;
use streamdock_core::merge::is_support_ffmpeg;

static TEST_TS_BYTES: OnceCell<Option<(Vec<u8>, Vec<u8>)>> = OnceCell::const_new();

async fn get_test_ts_pair() -> Option<&'static (Vec<u8>, Vec<u8>)> {
    TEST_TS_BYTES
        .get_or_init(|| async {
            if !is_support_ffmpeg("").await {
                return None;
            }
            let dir = tempfile::tempdir().ok()?;
            let mut out = Vec::new();
            for idx in 0..2 {
                let p = dir.path().join(format!("seg{idx}.ts"));
                let status = tokio::process::Command::new("ffmpeg")
                    .args([
                        "-y",
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc=duration=0.3:size=64x64:rate=10",
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:duration=0.3",
                        "-c:v",
                        "mpeg2video",
                        "-c:a",
                        "mp2",
                        "-f",
                        "mpegts",
                    ])
                    .arg(&p)
                    .status()
                    .await
                    .ok()?;
                if !status.success() {
                    return None;
                }
                out.push(tokio::fs::read(&p).await.ok()?);
            }
            Some((out[0].clone(), out[1].clone()))
        })
        .await
        .as_ref()
}

fn aes128_cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let enc = Encryptor::<Aes128>::new_from_slices(key, iv).unwrap();
    enc.encrypt_padded_vec_mut::<Pkcs7>(data)
}

async fn spawn_test_router(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, handle)
}

#[tokio::test]
async fn format_options_and_headers_preserve_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = DownloadExecOptions {
        save_dir: Some(tmp.path().to_string_lossy().into_owned()),
        filename: Some("unsafe:name?test*file".to_string()),
        ..Default::default()
    };

    let f_m3u8 = format_options("https://cdn.example.com/v/index.m3u8?a=1", &opts).unwrap();
    assert_eq!(f_m3u8.download_type, InputTypeKind::M3u8);
    assert_eq!(f_m3u8.filename, "unsafe_name_test_file.mp4");

    let f_sub = format_options(
        "https://cdn.example.com/subs/movie.vtt?token=1",
        &DownloadExecOptions {
            save_dir: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f_sub.download_type, InputTypeKind::File);
    assert_eq!(f_sub.filename, "movie.vtt");

    // Inline / JSON header parsing and empty Referer preservation
    let parsed = format_headers(Some(&HeaderInput::Text(
        "User-Agent: CustomUA/1.0\nReferer: ".to_string(),
    )))
    .unwrap();
    assert_eq!(
        parsed.get("user-agent").map(String::as_str),
        Some("CustomUA/1.0")
    );
    assert_eq!(parsed.get("referer").map(String::as_str), Some(""));

    // CRLF header injection rejection (via Map or JSON value)
    let mut bad_map = std::collections::BTreeMap::new();
    bad_map.insert(
        "Referer".to_string(),
        serde_json::Value::String("https://ok.com/\r\nX-Evil: 1".to_string()),
    );
    let bad = format_headers(Some(&HeaderInput::Map(bad_map)));
    assert!(bad.is_err());

    // Cross-origin credential isolation strips Cookie & Authorization but preserves Referer
    let mut h = HeaderMap::new();
    h.insert("cookie".to_string(), "sid=abc".to_string());
    h.insert("authorization".to_string(), "Bearer tok".to_string());
    h.insert(
        "referer".to_string(),
        "https://origin-a.test/watch".to_string(),
    );
    let scoped_same = scope_credentials(
        "https://origin-a.test/page",
        "https://origin-a.test/seg.ts",
        &h,
    )
    .unwrap();
    assert_eq!(
        scoped_same.get("cookie").map(String::as_str),
        Some("sid=abc")
    );
    let scoped_cross = scope_credentials(
        "https://origin-a.test/page",
        "https://cdn-b.test/seg.ts",
        &h,
    )
    .unwrap();
    assert!(!scoped_cross.contains_key("cookie"));
    assert!(!scoped_cross.contains_key("authorization"));
    assert_eq!(
        scoped_cross.get("referer").map(String::as_str),
        Some("https://origin-a.test/watch")
    );
}

#[tokio::test]
async fn m3u8_parse_selects_highest_bandwidth_and_handles_byterange_and_map() {
    let master_body = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=500000
/low/media.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2400000
/high/media.m3u8
";
    let child_body = "\
#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@0\"
#EXTINF:1.5,
#EXT-X-BYTERANGE:200@100
seg.m4s
#EXTINF:1.5,
#EXT-X-BYTERANGE:300
seg.m4s
#EXT-X-ENDLIST
";

    let app = Router::new()
        .route("/master.m3u8", get(move || async move { master_body }))
        .route("/high/media.m3u8", get(move || async move { child_body }))
        .route(
            "/high/init.mp4",
            get(|| async {
                let mut init = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00isommp42".to_vec();
                init.resize(100, 0);
                init
            }),
        );

    let (port, server) = spawn_test_router(app).await;
    let tmp = tempfile::tempdir().unwrap();
    let client = streamdock_core::http::default_http_client();
    let cancel = CancellationToken::new();

    let info = parse_m3u8(
        client,
        &format!("http://127.0.0.1:{port}/master.m3u8"),
        tmp.path(),
        &HeaderMap::new(),
        &cancel,
        None,
    )
    .await
    .unwrap();

    assert_eq!(info.ts_count, 2);
    assert!(info.init_segment.is_some());
    assert_eq!(
        info.init_segment.as_ref().unwrap().uri,
        format!("http://127.0.0.1:{port}/high/init.mp4")
    );
    assert_eq!(info.data[0].byterange.unwrap().length, 200);
    assert_eq!(info.data[0].byterange.unwrap().offset, Some(100));
    assert_eq!(info.data[1].byterange.unwrap().length, 300);
    assert_eq!(info.data[1].byterange.unwrap().offset, Some(300));
    server.abort();
}

#[tokio::test]
async fn m3u8_download_aes128_and_merge_real_ffmpeg() {
    let Some((ts0, ts1)) = get_test_ts_pair().await else {
        eprintln!("SKIP: ffmpeg unavailable for m3u8_download_aes128_and_merge_real_ffmpeg");
        return;
    };

    let key: [u8; 16] = *b"0123456789abcdef";
    let explicit_iv: [u8; 16] = [0x11; 16];
    let mut seq_iv: [u8; 16] = [0u8; 16];
    seq_iv[8..16].copy_from_slice(&11u64.to_be_bytes());

    let enc0 = aes128_cbc_encrypt(&key, &explicit_iv, ts0);
    let enc1 = aes128_cbc_encrypt(&key, &seq_iv, ts1);

    let key_bytes = Arc::new(key.to_vec());
    let enc0_arc = Arc::new(enc0);
    let enc1_arc = Arc::new(enc1);

    let m3u8 = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-MEDIA-SEQUENCE:10
#EXT-X-TARGETDURATION:1
#EXT-X-KEY:METHOD=AES-128,URI=\"/enc.key\",IV=0x11111111111111111111111111111111
#EXTINF:0.3,
/seg0.ts
#EXT-X-KEY:METHOD=AES-128,URI=\"/enc.key\"
#EXTINF:0.3,
/seg1.ts
#EXT-X-ENDLIST
";

    let app = Router::new()
        .route("/index.m3u8", get(move || async move { m3u8 }))
        .route(
            "/enc.key",
            get({
                let k = key_bytes.clone();
                move || {
                    let k = k.clone();
                    async move { k.as_ref().clone() }
                }
            }),
        )
        .route(
            "/seg0.ts",
            get({
                let d = enc0_arc.clone();
                move || {
                    let d = d.clone();
                    async move { d.as_ref().clone() }
                }
            }),
        )
        .route(
            "/seg1.ts",
            get({
                let d = enc1_arc.clone();
                move || {
                    let d = d.clone();
                    async move { d.as_ref().clone() }
                }
            }),
        );

    let (port, server) = spawn_test_router(app).await;
    let tmp = tempfile::tempdir().unwrap();
    let save_dir = tmp.path().join("save");
    let cache_dir = tmp.path().join("cache");

    let res = m3u8_download(
        &format!("http://127.0.0.1:{port}/index.m3u8"),
        &DownloadExecOptions {
            filename: Some("aes_video".to_string()),
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            del_cache: Some(true),
            convert: Some(true),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(res.errmsg.is_none(), "unexpected error: {:?}", res.errmsg);
    let out_file = PathBuf::from(res.filepath.expect("expected merged filepath"));
    assert!(out_file.exists());
    assert!(tokio::fs::metadata(&out_file).await.unwrap().len() > 1000);
    // del_cache = true cleans task cache directory on merge success
    assert!(!cache_dir.exists() || std::fs::read_dir(&cache_dir).unwrap().count() == 0);
    server.abort();
}

#[tokio::test]
async fn m3u8_pause_resume_and_retry_failed_only() {
    let Some((ts0, ts1)) = get_test_ts_pair().await else {
        eprintln!("SKIP: ffmpeg unavailable for m3u8_pause_resume_and_retry_failed_only");
        return;
    };

    let seg0_hits = Arc::new(AtomicUsize::new(0));
    let seg1_fail_until = Arc::new(AtomicUsize::new(10)); // fail all attempts in run 1
    let seg1_hits = Arc::new(AtomicUsize::new(0));
    let ts0_arc = Arc::new(ts0.clone());
    let ts1_arc = Arc::new(ts1.clone());

    let playlist = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:1
#EXTINF:0.3,
/s0.ts
#EXTINF:0.3,
/s1.ts
#EXT-X-ENDLIST
";

    let app = Router::new()
        .route("/list.m3u8", get(move || async move { playlist }))
        .route(
            "/s0.ts",
            get({
                let hits = seg0_hits.clone();
                let d = ts0_arc.clone();
                move || {
                    let hits = hits.clone();
                    let d = d.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        d.as_ref().clone()
                    }
                }
            }),
        )
        .route(
            "/s1.ts",
            get({
                let fail_left = seg1_fail_until.clone();
                let hits = seg1_hits.clone();
                let d = ts1_arc.clone();
                move || {
                    let fail_left = fail_left.clone();
                    let hits = hits.clone();
                    let d = d.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let cur = fail_left.load(Ordering::SeqCst);
                        if cur > 0 {
                            fail_left.store(cur - 1, Ordering::SeqCst);
                            (StatusCode::INTERNAL_SERVER_ERROR, Vec::new()).into_response()
                        } else {
                            d.as_ref().clone().into_response()
                        }
                    }
                }
            }),
        );

    let (port, server) = spawn_test_router(app).await;
    let url = format!("http://127.0.0.1:{port}/list.m3u8");
    let tmp = tempfile::tempdir().unwrap();
    let save_dir = tmp.path().join("save");
    let cache_dir = tmp.path().join("cache");

    // Run 1: segment 1 fails after retries -> segments.json preserves s0 Done & s1 Error
    let r1 = m3u8_download(
        &url,
        &DownloadExecOptions {
            filename: Some("retry_demo".to_string()),
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            del_cache: Some(true),
            convert: Some(true),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(r1.errmsg.is_some());
    assert!(r1.filepath.is_none());

    let seg0_after_r1 = seg0_hits.load(Ordering::SeqCst);
    assert_eq!(seg0_after_r1, 1);

    // Fix upstream for segment 1 and retry only failed segments
    seg1_fail_until.store(0, Ordering::SeqCst);
    let r2 = m3u8_download(
        &url,
        &DownloadExecOptions {
            filename: Some("retry_demo".to_string()),
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            retry_failed_only: true,
            del_cache: Some(true),
            convert: Some(true),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(r2.errmsg.is_none(), "retry failed: {:?}", r2.errmsg);
    assert!(Path::new(r2.filepath.as_ref().unwrap()).exists());
    // s0 must NOT be re-downloaded on retry_failed_only!
    assert_eq!(seg0_hits.load(Ordering::SeqCst), 1);

    // Also verify pause during slow segment download preserves partial state & allows resume
    let slow_started = Arc::new(AtomicUsize::new(0));
    let pause_app = Router::new()
        .route("/slow.m3u8", get(move || async move { playlist }))
        .route(
            "/s0.ts",
            get({
                let d = ts0_arc.clone();
                move || {
                    let d = d.clone();
                    async move { d.as_ref().clone() }
                }
            }),
        )
        .route(
            "/s1.ts",
            get({
                let started = slow_started.clone();
                let d = ts1_arc.clone();
                move || {
                    let started = started.clone();
                    let d = d.clone();
                    async move {
                        let attempt = started.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        d.as_ref().clone()
                    }
                }
            }),
        );
    let (p2, srv2) = spawn_test_router(pause_app).await;
    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    tokio::spawn(async move {
        for _ in 0..50 {
            if slow_started.load(Ordering::SeqCst) > 0 {
                cancel_trigger.cancel();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let slow_url = format!("http://127.0.0.1:{p2}/slow.m3u8");
    let paused_res = download_resource(
        &slow_url,
        &DownloadExecOptions {
            filename: Some("pause_demo".to_string()),
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        cancel,
    )
    .await
    .unwrap();
    assert!(paused_res.paused);
    assert!(paused_res.filepath.is_none());

    // Resume completes cleanly
    let resumed_res = download_resource(
        &slow_url,
        &DownloadExecOptions {
            filename: Some("pause_demo".to_string()),
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(resumed_res.errmsg.is_none());
    assert!(Path::new(resumed_res.filepath.as_ref().unwrap()).exists());

    server.abort();
    srv2.abort();
}

#[tokio::test]
async fn m3u8_http_cloudflare_403_returns_clear_error() {
    let app = Router::new().route(
        "/cf.m3u8",
        get(|| async {
            Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header(header::CONTENT_TYPE, "text/html")
                .header("cf-mitigated", "challenge")
                .body(Body::from("<html><title>Just a moment...</title></html>"))
                .unwrap()
        }),
    );

    let (port, server) = spawn_test_router(app).await;
    let tmp = tempfile::tempdir().unwrap();
    let res = m3u8_download(
        &format!("http://127.0.0.1:{port}/cf.m3u8"),
        &DownloadExecOptions {
            save_dir: Some(tmp.path().join("save").to_string_lossy().into_owned()),
            cache_dir: Some(tmp.path().join("cache").to_string_lossy().into_owned()),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(res.filepath.is_none());
    let msg = res.errmsg.unwrap_or_default();
    assert!(
        msg.contains("Cloudflare") || msg.contains("403"),
        "msg={msg}"
    );
    server.abort();
}

#[tokio::test]
async fn custom_proxy_routes_remote_and_respects_no_proxy_bypass() {
    let proxy_app = Router::new().fallback(|| async { "via-proxy" });
    let direct_app = Router::new().route("/direct", get(|| async { "direct-ok" }));

    let (proxy_port, proxy_srv) = spawn_test_router(proxy_app).await;
    let (direct_port, direct_srv) = spawn_test_router(direct_app).await;

    let client = build_http_client(&ProxySettings {
        mode: ProxyMode::Custom,
        url: format!("http://127.0.0.1:{proxy_port}"),
        no_proxy: "127.0.0.1".to_string(),
    })
    .unwrap();

    let proxied = client
        .get("http://proxied.invalid/test")
        .send()
        .await
        .unwrap();
    assert_eq!(proxied.status(), StatusCode::OK);
    assert_eq!(proxied.text().await.unwrap(), "via-proxy");

    let bypassed = client
        .get(format!("http://127.0.0.1:{direct_port}/direct"))
        .send()
        .await
        .unwrap();
    assert_eq!(bypassed.status(), StatusCode::OK);
    assert_eq!(bypassed.text().await.unwrap(), "direct-ok");

    proxy_srv.abort();
    direct_srv.abort();
}

#[allow(dead_code)]
fn _unused(_h: AxHeaderMap, _m: HashMap<String, String>, _p: AxumPath<String>) {}
