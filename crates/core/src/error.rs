use std::fmt;
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub url: String,
    pub challenge: bool,
    pub expired: bool,
    pub message: String,
}

impl HttpError {
    pub fn new(status: u16, url: &str, challenge: bool) -> Self {
        let parsed = Url::parse(url).ok();
        let origin_path = parsed
            .as_ref()
            .map(|u| format!("{}{}", u.origin().ascii_serialization(), u.path()))
            .unwrap_or_else(|| url.split('?').next().unwrap_or(url).to_string());
        let access_error = matches!(status, 401 | 403 | 404);
        let expired =
            !challenge && access_error && parsed.as_ref().is_some_and(has_expired_signature);
        let hint = if challenge {
            "站点返回了 Cloudflare 浏览器验证页，请使用来源方提供的下载链接"
        } else if expired {
            "链接签名已过期，请从来源重新获取有效的完整链接"
        } else if access_error {
            "请检查链接有效期、来源页 Referer 或 Cookie"
        } else {
            ""
        };
        let message = if hint.is_empty() {
            format!("HTTP {status}: {origin_path}")
        } else {
            format!("HTTP {status}: {origin_path}（{hint}）")
        };
        Self {
            status,
            url: url.to_string(),
            challenge,
            expired,
            message,
        }
    }

    pub fn retryable(&self) -> bool {
        !self.challenge && (self.status >= 500 || self.status == 429)
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpError {}

pub fn has_expired_signature(url: &Url) -> bool {
    let mut has_t = false;
    let mut e_val: Option<String> = None;
    let mut expires_val: Option<String> = None;
    let mut signed = false;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "token" | "sig" | "sign" | "signature" => signed = true,
            "t" => has_t = true,
            "e" => e_val = Some(v.into_owned()),
            "expires" => expires_val = Some(v.into_owned()),
            _ => {}
        }
    }
    if has_t && e_val.is_some() {
        signed = true;
    }
    let exp = expires_val.or_else(|| if has_t { e_val } else { None });
    let Some(exp_str) = exp else {
        return false;
    };
    if !signed || exp_str.len() != 10 || !exp_str.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(secs) = exp_str.parse::<i64>() else {
        return false;
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    secs * 1000 <= now_ms
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("{0}")]
    Http(#[from] HttpError),
    #[error("{0}")]
    Aborted(String),
    #[error("{0}")]
    Message(String),
    #[error("该地址返回的是 HLS 播放列表")]
    PlaylistFallback,
    #[error("RestartDirectDownload")]
    RestartDownload,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
}

impl CoreError {
    pub fn msg(s: impl Into<String>) -> Self {
        Self::Message(s.into())
    }

    pub fn aborted() -> Self {
        Self::Aborted("The operation was aborted".to_string())
    }

    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted(_))
    }

    pub fn as_http(&self) -> Option<&HttpError> {
        match self {
            Self::Http(err) => Some(err),
            _ => None,
        }
    }

    pub fn is_retryable_http(&self) -> bool {
        match self {
            Self::Http(err) => err.retryable(),
            _ => true,
        }
    }
}

pub type CoreResult<T> = std::result::Result<T, CoreError>;
