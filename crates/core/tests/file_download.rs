use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap as AxHeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use futures_util::stream;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use streamdock_core::contract::{DownloadExecOptions, InputTypeKind};
use streamdock_core::file_download::file_download;
use streamdock_core::merge::is_support_ffmpeg;

async fn spawn_test_router(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, handle)
}

#[tokio::test]
async fn file_download_range_resume_ownership_and_etag_change() {
    let payload: Vec<u8> = (0..120_000usize).map(|i| (i % 251) as u8).collect();
    let etag_mode = Arc::new(AtomicUsize::new(1)); // 1 = "v1" (first req aborts mid-stream), 2 = "v2"
    let first_req_done = Arc::new(AtomicUsize::new(0));
    let payload_arc = Arc::new(payload.clone());

    let app = Router::new().route(
        "/video.mp4",
        get({
            let etag_mode = etag_mode.clone();
            let first_req_done = first_req_done.clone();
            let data = payload_arc.clone();
            move |hdrs: AxHeaderMap| {
                let etag_mode = etag_mode.clone();
                let first_req_done = first_req_done.clone();
                let data = data.clone();
                async move {
                    let cur_etag = if etag_mode.load(Ordering::SeqCst) == 1 {
                        "\"v1\""
                    } else {
                        "\"v2\""
                    };
                    let total = data.len();
                    let if_range = hdrs
                        .get(header::IF_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let range_hdr = hdrs
                        .get(header::RANGE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    if !range_hdr.is_empty() && (if_range.is_empty() || if_range == cur_etag) {
                        if let Some(rest) = range_hdr.strip_prefix("bytes=") {
                            let start_str = rest.split('-').next().unwrap_or("0");
                            let start = start_str.parse::<usize>().unwrap_or(0);
                            if start >= total {
                                return Response::builder()
                                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                                    .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                                    .body(Body::empty())
                                    .unwrap();
                            }
                            let slice = data[start..].to_vec();
                            return Response::builder()
                                .status(StatusCode::PARTIAL_CONTENT)
                                .header(header::CONTENT_TYPE, "video/mp4")
                                .header(header::ACCEPT_RANGES, "bytes")
                                .header(header::ETAG, cur_etag)
                                .header(
                                    header::CONTENT_RANGE,
                                    format!("bytes {start}-{}/{total}", total - 1),
                                )
                                .header(header::CONTENT_LENGTH, slice.len().to_string())
                                .body(Body::from(slice))
                                .unwrap();
                        }
                    }

                    // Initial full response: stream first half then stall so test cancels for pause/resume
                    let is_first = first_req_done.fetch_add(1, Ordering::SeqCst) == 0;
                    if is_first {
                        let first_half = Bytes::from(data[..total / 2].to_vec());
                        let s = stream::unfold(0u8, move |step| {
                            let half = first_half.clone();
                            async move {
                                match step {
                                    0 => Some((Ok::<Bytes, std::io::Error>(half), 1)),
                                    1 => {
                                        tokio::time::sleep(Duration::from_secs(5)).await;
                                        None
                                    }
                                    _ => None,
                                }
                            }
                        });
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "video/mp4")
                            .header(header::ACCEPT_RANGES, "bytes")
                            .header(header::ETAG, cur_etag)
                            .header(header::CONTENT_LENGTH, total.to_string())
                            .body(Body::from_stream(s))
                            .unwrap();
                    }

                    // If ETag changed (`If-Range` mismatch), server returns 200 full with new bytes
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "video/mp4")
                        .header(header::ACCEPT_RANGES, "bytes")
                        .header(header::ETAG, cur_etag)
                        .header(header::CONTENT_LENGTH, total.to_string())
                        .body(Body::from(data.as_ref().clone()))
                        .unwrap()
                }
            }
        }),
    );

    let (port, server) = spawn_test_router(app).await;
    let url = format!("http://127.0.0.1:{port}/video.mp4");
    let tmp = tempfile::tempdir().unwrap();
    let save_dir = tmp.path().join("save");
    let cache_dir = tmp.path().join("cache");

    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    let half_target = (payload.len() / 2) as u64;
    let on_prog: streamdock_core::contract::ProgressCallback = Arc::new(move |st| {
        if st.downloaded_size.unwrap_or(0) >= half_target {
            cancel_trigger.cancel();
        }
        true
    });

    // 1. Partial download + cancel
    let r_pause = file_download(
        &url,
        &DownloadExecOptions {
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            on_progress: Some(on_prog),
            ..Default::default()
        },
        None,
        cancel,
    )
    .await
    .unwrap();
    assert!(r_pause.paused);

    // 2. Resume using Range 206 -> exact payload published
    let r_done = file_download(
        &url,
        &DownloadExecOptions {
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(r_done.errmsg.is_none(), "{:?}", r_done.errmsg);
    let out = PathBuf::from(r_done.filepath.as_ref().unwrap());
    assert_eq!(tokio::fs::read(&out).await.unwrap(), payload);

    // 3. Repeat same URL hits ownership marker `is_exist = true`
    let r_exist = file_download(
        &url,
        &DownloadExecOptions {
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(r_exist.is_exist);

    // 4. ETag change with `force = true` redownloads cleanly even when full 200 returned
    etag_mode.store(2, Ordering::SeqCst);
    let r_force = file_download(
        &url,
        &DownloadExecOptions {
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            force: true,
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!r_force.is_exist);
    assert_eq!(tokio::fs::read(&out).await.unwrap(), payload);
    server.abort();
}

#[tokio::test]
async fn file_download_preserves_subtitles_and_rejects_html_error_pages() {
    let vtt_body = "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHello Rust\n";
    let app = Router::new()
        .route(
            "/sub.vtt",
            get(move || async move {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/vtt; charset=utf-8")
                    .header(header::CONTENT_LENGTH, vtt_body.len().to_string())
                    .body(Body::from(vtt_body))
                    .unwrap()
            }),
        )
        .route(
            "/broken.mp4",
            get(|| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from("<html><body>Error page</body></html>"))
                    .unwrap()
            }),
        );

    let (port, server) = spawn_test_router(app).await;
    let tmp = tempfile::tempdir().unwrap();
    let save_dir = tmp.path().join("save");
    let cache_dir = tmp.path().join("cache");

    let sub_res = file_download(
        &format!("http://127.0.0.1:{port}/sub.vtt"),
        &DownloadExecOptions {
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let sub_path = Path::new(sub_res.filepath.as_ref().unwrap());
    assert!(sub_path.to_string_lossy().ends_with(".vtt"));
    assert_eq!(tokio::fs::read_to_string(sub_path).await.unwrap(), vtt_body);

    let html_err = file_download(
        &format!("http://127.0.0.1:{port}/broken.mp4"),
        &DownloadExecOptions {
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(html_err.errmsg.is_some());
    assert!(html_err.filepath.is_none());
    server.abort();
}

#[tokio::test]
async fn file_download_falls_back_to_hls_on_mpegurl_content_type() {
    if !is_support_ffmpeg("").await {
        eprintln!(
            "SKIP: ffmpeg unavailable for file_download_falls_back_to_hls_on_mpegurl_content_type"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let seg_path = tmp.path().join("tiny.ts");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=0.3:size=64x64:rate=10",
            "-f",
            "mpegts",
        ])
        .arg(&seg_path)
        .status()
        .await
        .unwrap();
    if !status.success() {
        return;
    }
    let ts_bytes = Arc::new(tokio::fs::read(&seg_path).await.unwrap());
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:0.3,\n/clip.ts\n#EXT-X-ENDLIST\n";

    let app = Router::new()
        .route(
            "/stream.mp4",
            get(move || async move {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                    .body(Body::from(playlist))
                    .unwrap()
            }),
        )
        .route(
            "/clip.ts",
            get({
                let d = ts_bytes.clone();
                move || {
                    let d = d.clone();
                    async move { d.as_ref().clone() }
                }
            }),
        );

    let (port, server) = spawn_test_router(app).await;
    let res = file_download(
        &format!("http://127.0.0.1:{port}/stream.mp4"),
        &DownloadExecOptions {
            download_type: Some(InputTypeKind::File),
            save_dir: Some(tmp.path().join("save").to_string_lossy().into_owned()),
            cache_dir: Some(tmp.path().join("cache").to_string_lossy().into_owned()),
            ..Default::default()
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(res.errmsg.is_none(), "{:?}", res.errmsg);
    assert!(Path::new(res.filepath.as_ref().unwrap()).exists());
    server.abort();
}
