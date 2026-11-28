use crate::contract::{ApiResponse, HeaderMap, VideoInfo};
use crate::error::{CoreError, CoreResult};
use crate::format_options::percent_decode_lossy;
use crate::http::normalize_download_url;
use crate::video_parser::request::{PageRequest, ParserContext, locate, read_page};
use regex::Regex;
use std::collections::VecDeque;
use std::sync::LazyLock;
use url::Url;

const DOUYIN_MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 16_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1";

fn find_douyin_item(root: &serde_json::Value) -> Option<&serde_json::Value> {
    let mut queue = VecDeque::new();
    queue.push_back(root);
    let mut visited = 0usize;
    while let Some(node) = queue.pop_front() {
        visited += 1;
        if visited > 3000 {
            break;
        }
        match node {
            serde_json::Value::Object(map) => {
                if let Some(video) = map.get("video").and_then(|v| v.as_object()) {
                    if video.contains_key("play_addr") || video.contains_key("playAddr") {
                        return Some(node);
                    }
                }
                for val in map.values() {
                    if val.is_object() || val.is_array() {
                        queue.push_back(val);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for val in arr {
                    if val.is_object() || val.is_array() {
                        queue.push_back(val);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

pub async fn parse_douyin(
    url: &str,
    headers: &HeaderMap,
    ctx: &ParserContext,
) -> CoreResult<ApiResponse<VideoInfo>> {
    let mut outgoing = HeaderMap::new();
    outgoing.insert("user-agent".to_string(), DOUYIN_MOBILE_UA.to_string());
    for (k, v) in headers {
        outgoing.insert(k.clone(), v.clone());
    }

    let res: CoreResult<VideoInfo> = async {
        let location = locate(
            ctx,
            url,
            &PageRequest {
                headers: outgoing.clone(),
                ..Default::default()
            },
        )
        .await?;

        let parsed = Url::parse(&location)?;
        static ID_RX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"/(?:video|note|slides)/(\d+)").unwrap());
        let id = ID_RX
            .captures(parsed.path())
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .or_else(|| {
                parsed
                    .query_pairs()
                    .find(|(k, _)| k == "modal_id")
                    .map(|(_, v)| v.into_owned())
            })
            .unwrap_or_default();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(CoreError::msg("无法解析抖音视频 ID，请检查分享链接"));
        }

        let share_url = format!("https://www.iesdouyin.com/share/video/{id}");
        let page = read_page(
            ctx,
            &share_url,
            &PageRequest {
                headers: outgoing.clone(),
                ..Default::default()
            },
        )
        .await?;

        static ROUTER_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?is)window\._ROUTER_DATA\s*=\s*([\s\S]*?)</script>").unwrap()
        });
        static RENDER_RX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?is)<script\b[^>]*id=["']RENDER_DATA["'][^>]*>([\s\S]*?)</script>"#)
                .unwrap()
        });

        let data: serde_json::Value = if let Some(caps) = ROUTER_RX.captures(&page.text) {
            let raw = caps[1].trim().trim_end_matches(';').trim();
            serde_json::from_str(raw)?
        } else if let Some(caps) = RENDER_RX.captures(&page.text) {
            let decoded = percent_decode_lossy(caps[1].trim());
            serde_json::from_str(&decoded)?
        } else {
            return Err(CoreError::msg(
                "抖音页面未返回视频数据，可能需要有效 Cookie，或平台页面结构已变化",
            ));
        };

        let item = find_douyin_item(&data).ok_or_else(|| {
            CoreError::msg(
                "抖音页面未返回可用视频，可能是图文、内容限制、需要 Cookie 或平台接口变化",
            )
        })?;
        let video = &item["video"];
        let address = if video["play_addr"].is_object() {
            &video["play_addr"]
        } else {
            &video["playAddr"]
        };

        let uri = address["uri"].as_str().unwrap_or("");
        let mut media = if !uri.is_empty()
            && !uri.to_ascii_lowercase().starts_with("http:")
            && !uri.to_ascii_lowercase().starts_with("https:")
        {
            let encoded_uri: String =
                url::form_urlencoded::byte_serialize(uri.as_bytes()).collect();
            format!(
                "https://www.iesdouyin.com/aweme/v1/play/?video_id={encoded_uri}&ratio=1080p&line=0"
            )
        } else {
            address["url_list"][0]
                .as_str()
                .or_else(|| address["urlList"][0].as_str())
                .unwrap_or(uri)
                .to_string()
        };

        if media.is_empty() {
            return Err(CoreError::msg("抖音未返回可下载的视频地址"));
        }
        media = normalize_download_url(&media.replace("/playwm/", "/play/"))?;

        let mut cdn_headers = HeaderMap::new();
        if let Some(ua) = outgoing.get("user-agent") {
            cdn_headers.insert("user-agent".to_string(), ua.clone());
        }
        cdn_headers.insert("referer".to_string(), location.clone());

        let final_media = locate(
            ctx,
            &media,
            &PageRequest {
                headers: cdn_headers,
                ..Default::default()
            },
        )
        .await?;

        Ok(VideoInfo {
            url: final_media,
            title: item["desc"].as_str().unwrap_or("").to_string(),
            platform: Some("douyin".to_string()),
            referer: Some(location),
            author: item["author"]["nickname"].as_str().map(str::to_string),
            uid: item["author"]["unique_id"].as_str().map(str::to_string),
            avatar: item["author"]["avatar_medium"]["url_list"][0]
                .as_str()
                .map(str::to_string),
            cover: video["cover"]["url_list"][0].as_str().map(str::to_string),
            like: item["statistics"]["digg_count"].as_i64(),
            time: item["create_time"].as_i64(),
            music: None,
        })
    }
    .await;

    match res {
        Ok(info) => Ok(ApiResponse::ok(info)),
        Err(e) if ctx.cancel.is_cancelled() || e.is_aborted() => Err(e),
        Err(e) => Ok(ApiResponse::err(1, format!("抖音解析失败：{e}"))),
    }
}
