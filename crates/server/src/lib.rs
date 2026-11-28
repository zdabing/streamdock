pub mod assets;
pub mod routes;
pub mod state;
pub mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use streamdock_core::error::{CoreError, CoreResult};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::info;

use crate::routes::build_router;
use crate::state::{ServerState, VERSION};

#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub token: Option<String>,
    pub web_dir: Option<PathBuf>,
}

pub struct StreamDockServer {
    state: Arc<ServerState>,
    addr: SocketAddr,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl StreamDockServer {
    pub async fn start(options: ServerOptions) -> CoreResult<Self> {
        let port = options.port.unwrap_or_else(|| {
            std::env::var("DS_PORT")
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(6600)
        });
        let host = options.host.unwrap_or_else(|| {
            std::env::var("DS_HOST")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "0.0.0.0".to_string())
        });
        let cache_dir = options.cache_dir.unwrap_or_else(|| {
            if let Ok(env_dir) = std::env::var("DS_CACHE_DIR") {
                if !env_dir.trim().is_empty() {
                    return PathBuf::from(env_dir.trim());
                }
            }
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".streamdock")
                .join("cache")
        });
        let token = options.token.unwrap_or_else(|| {
            std::env::var("DS_SECRET")
                .or_else(|_| std::env::var("DS_TOKEN"))
                .unwrap_or_default()
        });
        let web_dir = options.web_dir.or_else(|| {
            std::env::var("DS_WEB_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        });

        let state = ServerState::init(cache_dir, token, web_dir).await?;
        let listener = TcpListener::bind((host.as_str(), port))
            .await
            .map_err(|e| CoreError::msg(format!("绑定端口 {host}:{port} 失败: {e}")))?;
        let addr = listener.local_addr()?;

        let router = build_router(state.clone());
        let shutdown = state.shutdown_cancel.clone();
        let handle = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                shutdown.cancelled().await;
            });
            if let Err(err) = server.await {
                tracing::error!("HTTP 服务异常退出: {err}");
            }
        });

        info!("StreamDock v{VERSION}: http://localhost:{}", addr.port());
        println!("StreamDock v{VERSION}: http://localhost:{}", addr.port());

        Ok(Self {
            state,
            addr,
            task: Mutex::new(Some(handle)),
        })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    pub async fn close(&self) {
        self.state.close().await;
        if let Some(handle) = self.task.lock().await.take() {
            let _ = handle.await;
        }
    }
}
