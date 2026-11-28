use crate::contract::HeaderMap;
use crate::error::{CoreError, CoreResult};
use crate::http::get_response;
use brotli::DecompressorWriter;
use flate2::write::{GzDecoder, ZlibDecoder};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct PageRequest {
    pub headers: HeaderMap,
    pub max_bytes: usize,
    pub method_post_body: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PageData {
    pub url: String,
    pub text: String,
    pub content_type: String,
}

pub type RequestHook = Arc<
    dyn Fn(&str, &PageRequest) -> Pin<Box<dyn Future<Output = CoreResult<PageData>> + Send>>
        + Send
        + Sync,
>;
pub type ResolveHook = Arc<
    dyn Fn(&str, &PageRequest) -> Pin<Box<dyn Future<Output = CoreResult<String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ParserContext {
    pub client: Arc<reqwest::Client>,
    pub cancel: CancellationToken,
    pub request_override: Option<RequestHook>,
    pub resolve_override: Option<ResolveHook>,
}

impl ParserContext {
    pub fn new(client: reqwest::Client, cancel: CancellationToken) -> Self {
        Self {
            client: Arc::new(client),
            cancel,
            request_override: None,
            resolve_override: None,
        }
    }
}

struct BoundedWriter {
    buf: Vec<u8>,
    max: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "页面内容超过大小限制",
            ));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum DecoderStream {
    Identity(BoundedWriter),
    Gzip(GzDecoder<BoundedWriter>),
    Deflate(ZlibDecoder<BoundedWriter>),
    Brotli(Box<DecompressorWriter<BoundedWriter>>),
}

impl DecoderStream {
    fn new(encoding: &str, max: usize) -> Self {
        let bw = BoundedWriter {
            buf: Vec::new(),
            max,
        };
        match encoding.trim().to_ascii_lowercase().as_str() {
            "gzip" => Self::Gzip(GzDecoder::new(bw)),
            "deflate" => Self::Deflate(ZlibDecoder::new(bw)),
            "br" => Self::Brotli(Box::new(DecompressorWriter::new(bw, 4096))),
            _ => Self::Identity(bw),
        }
    }

    fn write_chunk(&mut self, data: &[u8]) -> CoreResult<()> {
        let res = match self {
            Self::Identity(w) => w.write_all(data),
            Self::Gzip(w) => w.write_all(data),
            Self::Deflate(w) => w.write_all(data),
            Self::Brotli(w) => w.write_all(data),
        };
        res.map_err(|e| CoreError::msg(e.to_string()))
    }

    fn finish(self) -> CoreResult<Vec<u8>> {
        let bw = match self {
            Self::Identity(w) => w,
            Self::Gzip(w) => w.finish().map_err(|e| CoreError::msg(e.to_string()))?,
            Self::Deflate(w) => w.finish().map_err(|e| CoreError::msg(e.to_string()))?,
            Self::Brotli(mut w) => {
                w.flush().map_err(|e| CoreError::msg(e.to_string()))?;
                w.into_inner()
                    .map_err(|_| CoreError::msg("Brotli 解压失败"))?
            }
        };
        Ok(bw.buf)
    }
}

pub async fn request_page(
    client: &reqwest::Client,
    url: &str,
    options: &PageRequest,
    cancel: &CancellationToken,
) -> CoreResult<PageData> {
    if cancel.is_cancelled() {
        return Err(CoreError::aborted());
    }
    let mut stream = get_response(
        client,
        url,
        &options.headers,
        cancel,
        options.method_post_body.clone(),
    )
    .await?;

    let encoding = stream
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let content_type = stream
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let max_bytes = if options.max_bytes == 0 {
        5 * 1024 * 1024
    } else {
        options.max_bytes
    };

    let mut decoder = DecoderStream::new(&encoding, max_bytes);
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(CoreError::aborted()),
            c = stream.response.chunk() => c?,
        };
        let Some(bytes) = chunk else {
            break;
        };
        decoder.write_chunk(&bytes)?;
    }
    let raw = decoder.finish()?;
    Ok(PageData {
        url: stream.url,
        text: String::from_utf8_lossy(&raw).into_owned(),
        content_type,
    })
}

pub async fn resolve_location(
    client: &reqwest::Client,
    url: &str,
    options: &PageRequest,
    cancel: &CancellationToken,
) -> CoreResult<String> {
    let stream = get_response(client, url, &options.headers, cancel, None).await?;
    Ok(stream.url)
}

pub async fn read_page(
    ctx: &ParserContext,
    url: &str,
    options: &PageRequest,
) -> CoreResult<PageData> {
    if let Some(ref hook) = ctx.request_override {
        hook(url, options).await
    } else {
        request_page(&ctx.client, url, options, &ctx.cancel).await
    }
}

pub async fn locate(ctx: &ParserContext, url: &str, options: &PageRequest) -> CoreResult<String> {
    if let Some(ref hook) = ctx.resolve_override {
        hook(url, options).await
    } else {
        resolve_location(&ctx.client, url, options, &ctx.cancel).await
    }
}
