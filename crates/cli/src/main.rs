use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use tokio_util::sync::CancellationToken;

use streamdock_core::contract::{
    DownloadExecOptions, HeaderInput, InputTypeKind, ScrapeDataSource, ScrapeFolderTemplate,
    ScrapeMode, ScrapePosterCrop, ScrapePreviewStatus, ScrapeRunItemInput, ScrapeRunRequest,
    ScrapeScanRequest, ScrapeSourceType, ScrapeTransferMode,
};
use streamdock_core::download::batch_download;
use streamdock_core::extractor::{ExtractMediaOptions, extract_media};
use streamdock_core::http::format_headers_str;
use streamdock_core::scraper::{ScanOptions, ScrapeRunOptions, run_media_scrape, scan_for_scrape};
use streamdock_server::{ServerOptions, StreamDockServer};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliDownloadType {
    Auto,
    M3u8,
    File,
    Parser,
}

impl From<CliDownloadType> for InputTypeKind {
    fn from(v: CliDownloadType) -> Self {
        match v {
            CliDownloadType::Auto => Self::Auto,
            CliDownloadType::M3u8 => Self::M3u8,
            CliDownloadType::File => Self::File,
            CliDownloadType::Parser => Self::Parser,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliScrapeMode {
    #[value(name = "in-place")]
    InPlace,
    Organize,
}

impl From<CliScrapeMode> for ScrapeMode {
    fn from(v: CliScrapeMode) -> Self {
        match v {
            CliScrapeMode::InPlace => Self::InPlace,
            CliScrapeMode::Organize => Self::Organize,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliTransferMode {
    Move,
    Hardlink,
    Copy,
}

impl From<CliTransferMode> for ScrapeTransferMode {
    fn from(v: CliTransferMode) -> Self {
        match v {
            CliTransferMode::Move => Self::Move,
            CliTransferMode::Hardlink => Self::Hardlink,
            CliTransferMode::Copy => Self::Copy,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliFolderTemplate {
    #[value(name = "actor-title")]
    ActorTitle,
    Title,
}

impl From<CliFolderTemplate> for ScrapeFolderTemplate {
    fn from(v: CliFolderTemplate) -> Self {
        match v {
            CliFolderTemplate::ActorTitle => Self::ActorTitle,
            CliFolderTemplate::Title => Self::Title,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPosterCrop {
    Center,
    Right,
    None,
}

impl From<CliPosterCrop> for ScrapePosterCrop {
    fn from(v: CliPosterCrop) -> Self {
        match v {
            CliPosterCrop::Center => Self::Center,
            CliPosterCrop::Right => Self::Right,
            CliPosterCrop::None => Self::None,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "streamdock",
    version,
    about = "StreamDock — M3U8/HLS、直链下载与 Emby NFO 刮削工具",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    download: DownloadArgs,
}

#[derive(Args, Debug)]
struct DownloadArgs {
    /// M3U8、视频/字幕直链，或抖音、微博、皮皮虾分享链接
    #[arg(value_name = "urls")]
    urls: Vec<String>,

    /// 下载类型：auto、m3u8、file、parser
    #[arg(short = 'T', long = "type", value_enum, default_value_t = CliDownloadType::Auto)]
    download_type: CliDownloadType,

    /// 保存文件名
    #[arg(short = 'f', long = "filename")]
    filename: Option<String>,

    /// 保存目录
    #[arg(short = 'S', long = "save-dir", default_value = "./downloads")]
    save_dir: String,

    /// 分片缓存目录
    #[arg(short = 'C', long = "cache-dir", default_value = "./cache")]
    cache_dir: String,

    /// 并发分片数
    #[arg(short = 'n', long = "thread-num", default_value_t = 4)]
    thread_num: usize,

    /// 请求头：每行名称: 值或 JSON
    #[arg(short = 'H', long = "headers")]
    headers: Option<String>,

    /// 合并成功后保留分片缓存
    #[arg(long = "no-del-cache")]
    no_del_cache: bool,

    /// FFmpeg 可执行文件路径
    #[arg(long = "ffmpeg-path")]
    ffmpeg_path: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 从网页或平台分享链接提取媒体地址，输出 JSON
    Extract {
        /// 网页或分享链接，每次最多 20 个
        #[arg(required = true, value_name = "urls")]
        urls: Vec<String>,

        /// 子页面搜索深度（0–3）
        #[arg(short = 'd', long = "depth", default_value_t = 1)]
        depth: usize,

        /// 仅搜索地址包含该文本的站内链接
        #[arg(long = "path-filter")]
        path_filter: Option<String>,

        /// 来源网页请求头或平台 Cookie
        #[arg(short = 'H', long = "headers")]
        headers: Option<String>,
    },

    /// 启动 Web 下载器
    Server {
        /// 服务端口（默认读取 DS_PORT 或 6600）
        #[arg(short = 'P', long = "port")]
        port: Option<u16>,

        /// 访问密码（默认读取 DS_SECRET / DS_TOKEN）
        #[arg(short = 't', long = "token")]
        token: Option<String>,

        /// 外部 WebUI 静态资源目录（覆盖内嵌前端）
        #[arg(long = "web-dir")]
        web_dir: Option<PathBuf>,

        /// 缓存与 SQLite 数据库目录（默认读取 DS_CACHE_DIR）
        #[arg(short = 'C', long = "cache-dir")]
        cache_dir: Option<PathBuf>,
    },

    /// 按 RSS 与 WordPress 数据源刮削本地视频并生成 Emby NFO 与海报
    Scrape {
        /// 待扫描的本地视频目录（可传多个）
        #[arg(value_name = "dirs")]
        dirs: Vec<String>,

        /// RSS 订阅地址（默认读取内置默认源）
        #[arg(long = "rss")]
        rss: Option<String>,

        /// 附加 WordPress 兜底检索站点（可重复传入）
        #[arg(long = "wp", action = ArgAction::Append)]
        wp: Vec<String>,

        /// 详情页与封面请求 Referer
        #[arg(long = "referer")]
        referer: Option<String>,

        /// 刮削模式：in-place（原地生成 NFO）或 organize（按目录归档）
        #[arg(long = "mode", value_enum, default_value_t = CliScrapeMode::InPlace)]
        mode: CliScrapeMode,

        /// 归档模式目标输出目录
        #[arg(short = 'o', long = "output")]
        output: Option<String>,

        /// 归档文件处理方式：move、hardlink 或 copy
        #[arg(long = "transfer", value_enum, default_value_t = CliTransferMode::Move)]
        transfer: CliTransferMode,

        /// 归档目录结构：actor-title 或 title
        #[arg(long = "folder", value_enum, default_value_t = CliFolderTemplate::ActorTitle)]
        folder: CliFolderTemplate,

        /// 竖版海报裁切方式：center、right 或 none
        #[arg(long = "crop", value_enum, default_value_t = CliPosterCrop::Center)]
        crop: CliPosterCrop,

        /// 最低匹配分值（0–100）
        #[arg(long = "min-score", default_value_t = 55)]
        min_score: u32,

        /// 不递归子目录
        #[arg(long = "no-recursive")]
        no_recursive: bool,

        /// 覆盖已有 NFO 和图片
        #[arg(long = "overwrite")]
        overwrite: bool,

        /// 不下载封面图片
        #[arg(long = "no-images")]
        no_images: bool,

        /// FFmpeg 可执行文件路径
        #[arg(long = "ffmpeg-path")]
        ffmpeg_path: Option<String>,

        /// 仅扫描预览匹配结果，不写入文件
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

fn spawn_sigint_cancel(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    if let Some(ref mut s) = sigterm {
                        s.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        cancel.cancel();
    });
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .init();

    let cli = Cli::parse();
    match cli.command {
        None => {
            if cli.download.urls.is_empty() {
                let mut cmd = Cli::command();
                let _ = cmd.print_help();
                println!();
                return ExitCode::SUCCESS;
            }
            let cancel = CancellationToken::new();
            spawn_sigint_cancel(cancel.clone());
            let opts = DownloadExecOptions {
                download_type: Some(cli.download.download_type.into()),
                filename: cli.download.filename,
                save_dir: Some(cli.download.save_dir),
                cache_dir: Some(cli.download.cache_dir),
                thread_num: Some(cli.download.thread_num),
                headers: cli.download.headers.map(HeaderInput::Text),
                del_cache: Some(!cli.download.no_del_cache),
                ffmpeg_path: cli.download.ffmpeg_path,
                ..Default::default()
            };
            match batch_download(&cli.download.urls, &opts, cancel).await {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::from(1),
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::from(1)
                }
            }
        }
        Some(Commands::Extract {
            urls,
            depth,
            path_filter,
            headers,
        }) => {
            let cancel = CancellationToken::new();
            spawn_sigint_cancel(cancel.clone());
            let header_map = match headers.as_deref() {
                Some(raw) => match format_headers_str(raw) {
                    Ok(m) => m,
                    Err(err) => {
                        eprintln!("{err}");
                        return ExitCode::from(1);
                    }
                },
                None => Default::default(),
            };
            let first = urls[0].clone();
            match extract_media(
                ExtractMediaOptions {
                    url: first,
                    urls: Some(urls),
                    headers: header_map,
                    deep: Some(depth),
                    path_filter,
                    ..Default::default()
                },
                &cancel,
            )
            .await
            {
                Ok(result) => {
                    let empty = result.items.is_empty();
                    match serde_json::to_string_pretty(&result) {
                        Ok(json) => println!("{json}"),
                        Err(err) => {
                            eprintln!("{err}");
                            return ExitCode::from(1);
                        }
                    }
                    if empty {
                        ExitCode::from(1)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::from(1)
                }
            }
        }
        Some(Commands::Server {
            port,
            token,
            web_dir,
            cache_dir,
        }) => {
            let server = match StreamDockServer::start(ServerOptions {
                port,
                host: None,
                cache_dir,
                token,
                web_dir,
            })
            .await
            {
                Ok(s) => s,
                Err(err) => {
                    eprintln!("服务器启动失败: {err}");
                    return ExitCode::from(1);
                }
            };

            let shutdown = CancellationToken::new();
            spawn_sigint_cancel(shutdown.clone());
            shutdown.cancelled().await;
            server.close().await;
            ExitCode::SUCCESS
        }
        Some(Commands::Scrape {
            dirs,
            rss,
            wp,
            referer,
            mode,
            output,
            transfer,
            folder,
            crop,
            min_score,
            no_recursive,
            overwrite,
            no_images,
            ffmpeg_path,
            dry_run,
        }) => {
            let rss = rss
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(streamdock_core::store::default_rss_url);
            let cancel = CancellationToken::new();
            spawn_sigint_cancel(cancel.clone());
            let scan_dirs = if dirs.is_empty() {
                vec!["./downloads".to_string()]
            } else {
                dirs
            };

            let sources = if !wp.is_empty() {
                let mut list = vec![ScrapeDataSource {
                    id: "rss-cli".to_string(),
                    name: "RSS".to_string(),
                    source_type: ScrapeSourceType::Rss,
                    url: rss.clone(),
                    enabled: true,
                    referer: Some(referer.clone().unwrap_or_default()),
                }];
                for (idx, wp_url) in wp.into_iter().enumerate() {
                    list.push(ScrapeDataSource {
                        id: format!("wp-cli-{}", idx + 1),
                        name: format!("WordPress-{}", idx + 1),
                        source_type: ScrapeSourceType::Wordpress,
                        url: wp_url,
                        enabled: true,
                        referer: Some(referer.clone().unwrap_or_default()),
                    });
                }
                Some(list)
            } else {
                None
            };

            let scan = match scan_for_scrape(
                &ScrapeScanRequest {
                    scan_dir: scan_dirs.first().cloned(),
                    scan_dirs: Some(scan_dirs),
                    sources,
                    rss_url: Some(rss),
                    feed_id: None,
                    referer: referer.clone(),
                    recursive: Some(!no_recursive),
                    skip_existing_nfo: Some(!overwrite),
                    min_score: Some(min_score),
                    refresh_rss: None,
                },
                &ScanOptions {
                    cache_dir: None,
                    log: true,
                },
                &cancel,
            )
            .await
            {
                Ok(s) => s,
                Err(err) => {
                    eprintln!("{err}");
                    return ExitCode::from(1);
                }
            };

            println!(
                "扫描目录: {}（索引 {} 条，发现 {} 个视频，高置信匹配 {} 个）",
                scan.scan_dirs.join(", "),
                scan.catalog_count,
                scan.scanned_files,
                scan.matched_count
            );

            if dry_run {
                match serde_json::to_string_pretty(&scan.items) {
                    Ok(json) => println!("{json}"),
                    Err(err) => {
                        eprintln!("{err}");
                        return ExitCode::from(1);
                    }
                }
                return ExitCode::SUCCESS;
            }

            let targets: Vec<_> = scan
                .items
                .into_iter()
                .filter(|item| {
                    item.status == ScrapePreviewStatus::Matched && item.match_item.is_some()
                })
                .collect();

            if targets.is_empty() {
                println!("未找到可自动刮削的高置信匹配视频。");
                return ExitCode::SUCCESS;
            }

            let run_items: Vec<ScrapeRunItemInput> = targets
                .into_iter()
                .filter_map(|item| {
                    let m = item.match_item?;
                    Some(ScrapeRunItemInput {
                        file_path: item.file_path,
                        page_url: m.page_url,
                        title: Some(m.title),
                        pub_date: (!m.pub_date.is_empty()).then_some(m.pub_date),
                        tags: (!m.tags.is_empty()).then_some(m.tags),
                        guid: (!m.guid.is_empty()).then_some(m.guid),
                        actors: m.actors.filter(|v| !v.is_empty()),
                        studio: m.studio.filter(|s| !s.is_empty()),
                        cover_url: m.cover_url.filter(|s| !s.is_empty()),
                        referer: m.referer.filter(|s| !s.is_empty()),
                    })
                })
                .collect();

            match run_media_scrape(
                &ScrapeRunRequest {
                    items: run_items,
                    mode: mode.into(),
                    output_dir: output,
                    transfer_mode: Some(transfer.into()),
                    folder_template: Some(folder.into()),
                    poster_crop: Some(crop.into()),
                    download_images: Some(!no_images),
                    overwrite_nfo: Some(overwrite),
                    referer,
                    studio: None,
                    concurrency: None,
                },
                &ScrapeRunOptions {
                    ffmpeg_path: ffmpeg_path.unwrap_or_default(),
                    log: true,
                    on_item_start: None,
                    on_item_done: None,
                },
                &cancel,
            )
            .await
            {
                Ok(run) => {
                    println!(
                        "刮削完成：成功 {}/{}，失败 {}",
                        run.succeeded, run.total, run.failed
                    );
                    if run.failed > 0 {
                        ExitCode::from(1)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::from(1)
                }
            }
        }
    }
}
