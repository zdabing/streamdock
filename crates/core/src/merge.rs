use crate::container::{ContainerType, detect_container_by_file};
use crate::contract::MergeProgressCallback;
use crate::error::{CoreError, CoreResult};
use crate::m3u8_parse::M3u8Info;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub async fn is_support_ffmpeg(binary: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let bin = if binary.trim().is_empty() {
        "ffmpeg".to_string()
    } else {
        binary.trim().to_string()
    };
    {
        let cache = CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        if let Some(&ok) = cache.get(&bin) {
            return ok;
        }
    }
    let mut cmd = Command::new(&bin);
    cmd.arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let ok = match tokio::time::timeout(Duration::from_secs(5), cmd.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    };
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    cache.insert(bin, ok);
    ok
}

struct MergeQueueState {
    active: bool,
    waiters: VecDeque<(u64, oneshot::Sender<()>)>,
    next_id: u64,
}

struct MergeQueue {
    state: Mutex<MergeQueueState>,
}

impl MergeQueue {
    fn global() -> &'static Self {
        static QUEUE: OnceLock<MergeQueue> = OnceLock::new();
        QUEUE.get_or_init(|| MergeQueue {
            state: Mutex::new(MergeQueueState {
                active: false,
                waiters: VecDeque::new(),
                next_id: 1,
            }),
        })
    }

    fn is_busy(&self) -> bool {
        self.state.lock().unwrap().active
    }

    async fn acquire(&self, cancel: &CancellationToken) -> CoreResult<MergePermit> {
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let maybe_wait = {
            let mut guard = self.state.lock().unwrap();
            if !guard.active {
                guard.active = true;
                None
            } else {
                let id = guard.next_id;
                guard.next_id += 1;
                let (tx, rx) = oneshot::channel();
                guard.waiters.push_back((id, tx));
                Some((id, rx))
            }
        };

        if let Some((id, mut rx)) = maybe_wait {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let should_release = {
                        let mut guard = self.state.lock().unwrap();
                        let pos = guard.waiters.iter().position(|(wid, _)| *wid == id);
                        if let Some(idx) = pos {
                            guard.waiters.remove(idx);
                            false
                        } else {
                            // Sender was popped just as cancel fired; check if granted.
                            rx.try_recv().is_ok()
                        }
                    };
                    if should_release {
                        self.release_inner();
                    }
                    return Err(CoreError::aborted());
                }
                res = &mut rx => {
                    if res.is_err() {
                        return Err(CoreError::aborted());
                    }
                }
            }
        }

        Ok(MergePermit { released: false })
    }

    fn release_inner(&self) {
        let mut guard = self.state.lock().unwrap();
        while let Some((_, tx)) = guard.waiters.pop_front() {
            if tx.send(()).is_ok() {
                return;
            }
        }
        guard.active = false;
    }
}

struct MergePermit {
    released: bool,
}

impl Drop for MergePermit {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            MergeQueue::global().release_inner();
        }
    }
}

pub fn repair_ts_prefix(chunk: &[u8]) -> Cow<'_, [u8]> {
    if chunk.is_empty() || chunk[0] == 0x47 {
        return Cow::Borrowed(chunk);
    }
    let limit = chunk.len().saturating_sub(376).min(188 * 50);
    for offset in 1..limit {
        if chunk[offset] == 0x47 && chunk[offset + 188] == 0x47 && chunk[offset + 376] == 0x47 {
            return Cow::Borrowed(&chunk[offset..]);
        }
    }
    if chunk.len() > 376 && chunk[188] == 0x47 && chunk[376] == 0x47 {
        let mut owned = chunk.to_vec();
        owned[0] = 0x47;
        return Cow::Owned(owned);
    }
    Cow::Borrowed(chunk)
}

#[derive(Clone)]
pub struct ConvertOptions {
    pub filename: String,
    pub save_dir: PathBuf,
    pub ffmpeg_path: String,
    pub merge_timeout_secs: f64,
    pub force: bool,
    pub on_merge_progress: Option<MergeProgressCallback>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            filename: "video".to_string(),
            save_dir: PathBuf::from("./downloads"),
            ffmpeg_path: String::new(),
            merge_timeout_secs: 180.0,
            force: false,
            on_merge_progress: None,
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub async fn m3u8_convert(
    opts: &ConvertOptions,
    info: &M3u8Info,
    cancel: &CancellationToken,
) -> CoreResult<PathBuf> {
    if info.data.is_empty() {
        return Err(CoreError::msg("没有可合并的视频分片"));
    }
    let busy = MergeQueue::global().is_busy();
    if let Some(ref cb) = opts.on_merge_progress {
        cb(
            0.0,
            if busy {
                "等待其他任务合并完成".to_string()
            } else {
                "准备合并".to_string()
            },
        );
    }

    let _permit = MergeQueue::global().acquire(cancel).await?;
    if cancel.is_cancelled() {
        return Err(CoreError::aborted());
    }

    let fmp4 = info.container == ContainerType::Fmp4;
    let mut files = Vec::with_capacity(info.data.len() + 1);
    if fmp4 {
        let Some(ref init) = info.init_segment else {
            return Err(CoreError::msg(
                "缺少 fMP4 初始化分片，无法合并；请检查来源链接或请求头",
            ));
        };
        if init.ts_out.as_os_str().is_empty() {
            return Err(CoreError::msg(
                "缺少 fMP4 初始化分片，无法合并；请检查来源链接或请求头",
            ));
        }
        files.push(init.ts_out.clone());
    }
    for seg in &info.data {
        files.push(seg.ts_out.clone());
    }

    let mut total_bytes: u64 = 0;
    for f in &files {
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let meta = tokio::fs::metadata(f).await?;
        if meta.len() == 0 {
            return Err(CoreError::msg(format!("分片文件为空: {}", f.display())));
        }
        total_bytes += meta.len();
    }

    let binary = if opts.ffmpeg_path.trim().is_empty() {
        "ffmpeg"
    } else {
        opts.ffmpeg_path.trim()
    };
    let ffmpeg = is_support_ffmpeg(binary).await;
    if !ffmpeg && !opts.ffmpeg_path.trim().is_empty() {
        return Err(CoreError::msg(
            "指定的 FFmpeg 无法运行，请检查下载设置中的路径",
        ));
    }

    let extension = if ffmpeg || fmp4 { ".mp4" } else { ".ts" };
    let raw_name = if opts.filename.trim().is_empty() {
        "video"
    } else {
        opts.filename.trim()
    };
    let stem = match raw_name.rfind('.') {
        Some(idx) if idx > 0 => &raw_name[..idx],
        _ => raw_name,
    };
    let filepath = opts.save_dir.join(format!("{stem}{extension}"));
    if let Some(parent) = filepath.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if !opts.force && tokio::fs::try_exists(&filepath).await.unwrap_or(false) {
        return Err(CoreError::msg("目标文件已存在，请更换文件名以免覆盖"));
    }

    let temp_name = format!(".{}.part{}", uuid::Uuid::new_v4(), extension);
    let temporary = filepath
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(temp_name);

    let result: CoreResult<PathBuf> = async {
        let last_report = Arc::new(AtomicI64::new(0));
        let report = {
            let cb = opts.on_merge_progress.clone();
            let last_report = last_report.clone();
            Arc::new(move |percent: f64, msg: &str, force: bool| {
                let now = now_ms();
                let prev = last_report.load(Ordering::Relaxed);
                if force || now - prev > 300 {
                    last_report.store(now, Ordering::Relaxed);
                    if let Some(ref f) = cb {
                        f(percent, msg.to_string());
                    }
                }
            })
        };

        if ffmpeg {
            let mut args: Vec<String> = vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-stats_period",
                "1",
                "-progress",
                "pipe:1",
                "-fflags",
                "+genpts",
            ]
            .into_iter()
            .map(String::from)
            .collect();
            if !fmp4 {
                args.push("-f".to_string());
                args.push("mpegts".to_string());
            }
            args.extend([
                "-i".to_string(),
                "pipe:0".to_string(),
                "-map".to_string(),
                "0:v?".to_string(),
                "-map".to_string(),
                "0:a?".to_string(),
                "-c".to_string(),
                "copy".to_string(),
                "-movflags".to_string(),
                "+faststart".to_string(),
                "-y".to_string(),
                temporary.to_string_lossy().into_owned(),
            ]);

            let idle_timeout = if opts.merge_timeout_secs > 0.0 {
                opts.merge_timeout_secs
            } else {
                180.0
            };
            run_media_process(
                binary,
                &args,
                &files,
                !fmp4,
                info.duration,
                idle_timeout,
                cancel,
                report.clone(),
            )
            .await?;
        } else {
            stream_concat_raw(
                &files,
                &temporary,
                !fmp4,
                total_bytes.max(1),
                cancel,
                report.clone(),
            )
            .await?;
        }

        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let meta = tokio::fs::metadata(&temporary).await?;
        if meta.len() == 0 {
            return Err(CoreError::msg("合并结果为空，缓存已保留"));
        }
        tokio::fs::rename(&temporary, &filepath).await?;
        report(100.0, "合并完成", true);
        Ok(filepath.clone())
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn stream_concat_raw(
    files: &[PathBuf],
    dest: &Path,
    repair: bool,
    total_bytes: u64,
    cancel: &CancellationToken,
    report: Arc<dyn Fn(f64, &str, bool) + Send + Sync>,
) -> CoreResult<()> {
    let mut out = tokio::fs::File::create(dest).await?;
    let mut fed: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];
    for file in files {
        if cancel.is_cancelled() {
            return Err(CoreError::aborted());
        }
        let mut f = tokio::fs::File::open(file).await?;
        let mut first = true;
        loop {
            let n = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::aborted()),
                r = f.read(&mut buf) => r?,
            };
            if n == 0 {
                break;
            }
            let slice = if first && repair {
                first = false;
                repair_ts_prefix(&buf[..n])
            } else {
                first = false;
                Cow::Borrowed(&buf[..n])
            };
            out.write_all(&slice).await?;
            fed += slice.len() as u64;
            let pct = ((fed * 100) / total_bytes).min(99) as f64;
            report(pct, "正在顺序合并分片", false);
        }
    }
    out.flush().await?;
    Ok(())
}

pub async fn run_media_process(
    binary: &str,
    args: &[String],
    files: &[PathBuf],
    repair_ts: bool,
    duration: f64,
    idle_timeout_secs: f64,
    cancel: &CancellationToken,
    report: Arc<dyn Fn(f64, &str, bool) + Send + Sync>,
) -> CoreResult<()> {
    if cancel.is_cancelled() {
        return Err(CoreError::aborted());
    }
    let mut cmd = Command::new(binary);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::msg(format!("无法启动 FFmpeg: {e}")))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let last_activity = Arc::new(AtomicI64::new(now_ms()));
    let feed_cancel = cancel.child_token();

    // Drain stderr continuously keeping trailing 16 KiB.
    let stderr_task = tokio::spawn(async move {
        let mut reader = stderr;
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 16_384 {
                        let cut = tail.len() - 16_384;
                        tail.drain(..cut);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });

    // Parse -progress pipe:1 stdout lines.
    let stdout_activity = last_activity.clone();
    let stdout_report = report.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut last_time: f64 = -1.0;
        let mut last_size = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some((key, value)) = line.split_once('=') {
                let mut changed = false;
                if key == "out_time_us" {
                    if let Ok(us) = value.trim().parse::<f64>() {
                        let secs = (us / 1_000_000.0).max(0.0);
                        if secs.is_finite() && secs > last_time {
                            last_time = secs;
                            let pct = ((secs / duration.max(1.0)) * 100.0).floor().min(98.0);
                            stdout_report(pct, "正在无损封装视频", false);
                            changed = true;
                        }
                    }
                } else if key == "total_size" {
                    if value != last_size {
                        last_size = value.to_string();
                        changed = true;
                    }
                } else if key == "progress" && value.trim() == "end" {
                    stdout_report(99.0, "正在完成文件写入", true);
                    changed = true;
                }
                if changed {
                    stdout_activity.store(now_ms(), Ordering::Relaxed);
                }
            }
        }
    });

    // Feed input files to stdin without loading everything in RAM.
    let files_owned = files.to_vec();
    let feed_activity = last_activity.clone();
    let feed_tok = feed_cancel.clone();
    let feeder_task = tokio::spawn(async move {
        let mut writer = stdin;
        let mut buf = vec![0u8; 256 * 1024];
        for path in files_owned {
            if feed_tok.is_cancelled() {
                return Err(CoreError::aborted());
            }
            let mut f = tokio::fs::File::open(&path).await?;
            let mut first = true;
            loop {
                let n = tokio::select! {
                    _ = feed_tok.cancelled() => return Err(CoreError::aborted()),
                    r = f.read(&mut buf) => r?,
                };
                if n == 0 {
                    break;
                }
                feed_activity.store(now_ms(), Ordering::Relaxed);
                let slice = if first && repair_ts {
                    first = false;
                    repair_ts_prefix(&buf[..n])
                } else {
                    first = false;
                    Cow::Borrowed(&buf[..n])
                };
                if let Err(e) = writer.write_all(&slice).await {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(CoreError::Io(e));
                }
            }
        }
        let _ = writer.shutdown().await;
        Ok(())
    });

    let poll_ms = ((idle_timeout_secs * 100.0) as u64).clamp(25, 1000);
    let idle_ms = (idle_timeout_secs * 1000.0) as i64;
    let mut watchdog = tokio::time::interval(Duration::from_millis(poll_ms));

    let mut watchdog_err: Option<CoreError> = None;
    let mut aborted = false;

    let status = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                aborted = true;
                break None;
            }
            _ = watchdog.tick() => {
                if now_ms() - last_activity.load(Ordering::Relaxed) > idle_ms {
                    let display = if (idle_timeout_secs.fract()).abs() < f64::EPSILON {
                        format!("{}", idle_timeout_secs as u64)
                    } else {
                        format!("{idle_timeout_secs}")
                    };
                    watchdog_err = Some(CoreError::msg(format!(
                        "合并连续 {display} 秒无进展，已停止。分片缓存已保留，可重试或调整超时。"
                    )));
                    break None;
                }
            }
            wait_res = child.wait() => {
                break Some(wait_res);
            }
        }
    };

    if aborted || watchdog_err.is_some() {
        feed_cancel.cancel();
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    let feed_res = feeder_task
        .await
        .unwrap_or_else(|e| Err(CoreError::msg(e.to_string())));
    let _ = stdout_task.await;
    let stderr_tail = stderr_task.await.unwrap_or_default();

    if aborted || cancel.is_cancelled() {
        return Err(CoreError::aborted());
    }
    if let Some(err) = watchdog_err {
        return Err(err);
    }

    let exit_status = status
        .expect("child waited")
        .map_err(|e| CoreError::msg(format!("FFmpeg 运行异常: {e}")))?;
    if !exit_status.success() {
        let code = exit_status.code().unwrap_or(-1);
        let detail = stderr_tail.trim();
        let detail = if detail.is_empty() {
            "未返回错误详情"
        } else {
            detail
        };
        return Err(CoreError::msg(format!(
            "FFmpeg 合并失败（退出码 {code}）: {detail}"
        )));
    }
    feed_res?;
    Ok(())
}

pub fn infer_container_from_segments(files: &[PathBuf]) -> ContainerType {
    files
        .first()
        .map(|p| detect_container_by_file(p))
        .unwrap_or(ContainerType::Ts)
}
